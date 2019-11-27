// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod auth_keys;
mod balance;

use self::{
    auth_keys::AuthKeysDb,
    balance::{Balance, BalancesDb},
};
use crate::{
    action::{Action, ConsensusAction},
    chunk_store::{error::Error as ChunkStoreError, LoginPacketChunkStore},
    routing::Node,
    rpc::Rpc,
    utils::{self, AuthorisationKind},
    vault::Init,
    Config, Result,
};
use bytes::Bytes;
use lazy_static::lazy_static;
use log::{error, info, trace, warn};
use safe_nd::{
    AData, ADataAddress, AppPermissions, AppPublicId, Coins, ConnectionInfo, Error as NdError,
    HandshakeRequest, HandshakeResponse, IData, IDataAddress, IDataKind, LoginPacket, MData,
    Message, MessageId, NodePublicId, Notification, PublicId, PublicKey, Request, Response,
    Result as NdResult, Signature, Transaction, TransactionId, XorName,
};
use serde::Serialize;
use std::{
    cell::{Cell, RefCell},
    collections::{hash_map::Entry, HashMap},
    fmt::{self, Display, Formatter},
    net::SocketAddr,
    rc::Rc,
};
use unwrap::unwrap;

lazy_static! {
    /// The cost to Put a chunk to the network.
    pub static ref COST_OF_PUT: Coins = unwrap!(Coins::from_nano(1));
}

#[derive(Clone, Debug)]
struct ClientInfo {
    public_id: PublicId,
}

pub(crate) struct ClientHandler {
    id: NodePublicId,
    auth_keys: AuthKeysDb,
    balances: BalancesDb,
    clients: HashMap<SocketAddr, ClientInfo>,
    pending_msg_ids: HashMap<MessageId, SocketAddr>,
    // Map of new client connections to the challenge value we sent them.
    client_candidates: HashMap<SocketAddr, (Vec<u8>, PublicId)>,
    login_packets: LoginPacketChunkStore,
    routing_node: Rc<RefCell<Node>>,
}

impl ClientHandler {
    pub fn new(
        id: NodePublicId,
        config: &Config,
        total_used_space: &Rc<Cell<u64>>,
        init_mode: Init,
        routing_node: Rc<RefCell<Node>>,
    ) -> Result<Self> {
        let root_dir = config.root_dir()?;
        let root_dir = root_dir.as_path();
        let auth_keys = AuthKeysDb::new(root_dir, init_mode)?;
        let balances = BalancesDb::new(root_dir, init_mode)?;
        let login_packets = LoginPacketChunkStore::new(
            root_dir,
            config.max_capacity(),
            Rc::clone(&total_used_space),
            init_mode,
        )?;
        let client_handler = Self {
            id,
            auth_keys,
            balances,
            clients: Default::default(),
            pending_msg_ids: Default::default(),
            client_candidates: Default::default(),
            login_packets,
            routing_node,
        };
        Ok(client_handler)
    }

    pub fn handle_new_connection(&mut self, peer_addr: SocketAddr) {
        // If we already know the peer, drop the connection attempt.
        if self.clients.contains_key(&peer_addr) || self.client_candidates.contains_key(&peer_addr)
        {
            return;
        }

        info!("{}: Connected to new client on {}", self, peer_addr);
    }

    pub fn handle_connection_failure(&mut self, peer_addr: SocketAddr) {
        if let Some(client) = self.clients.remove(&peer_addr) {
            info!(
                "{}: Disconnected from {:?} on {}",
                self, client.public_id, peer_addr
            );
        } else {
            let _ = self.client_candidates.remove(&peer_addr);
            info!(
                "{}: Disconnected from client candidate on {}",
                self, peer_addr
            );
        }
    }

    pub fn handle_consensused_action(&mut self, action: ConsensusAction) -> Option<Action> {
        use ConsensusAction::*;
        trace!("{}: Consensused {:?}", self, action,);
        match action {
            PayAndForward {
                request,
                client_public_id,
                message_id,
                cost,
            } => {
                let owner = utils::owner(&client_public_id)?;
                self.pay(
                    &client_public_id,
                    owner.public_key(),
                    &request,
                    message_id,
                    cost,
                )?;

                Some(Action::ForwardClientRequest(Rpc::Request {
                    requester: client_public_id,
                    request,
                    message_id,
                }))
            }
            Forward {
                request,
                client_public_id,
                message_id,
            } => Some(Action::ForwardClientRequest(Rpc::Request {
                requester: client_public_id,
                request,
                message_id,
            })),
            PayAndProxy {
                request,
                client_public_id,
                message_id,
                cost,
            } => {
                let owner = utils::owner(&client_public_id)?;
                self.pay(
                    &client_public_id,
                    owner.public_key(),
                    &request,
                    message_id,
                    cost,
                )?;

                Some(Action::ProxyClientRequest(Rpc::Request {
                    requester: client_public_id,
                    request,
                    message_id,
                }))
            }
        }
    }

    pub fn handle_client_message(&mut self, peer_addr: SocketAddr, bytes: Bytes) -> Option<Action> {
        if let Some(client) = self.clients.get(&peer_addr).cloned() {
            match bincode::deserialize(&bytes) {
                Ok(Message::Request {
                    request,
                    message_id,
                    signature,
                }) => {
                    if let Entry::Vacant(ve) = self.pending_msg_ids.entry(message_id) {
                        let _ = ve.insert(peer_addr);
                        return self.handle_client_request(&client, request, message_id, signature);
                    } else {
                        info!("Pending MessageId reused - ignoring client message.");
                    }
                }
                Ok(Message::Response { response, .. }) => {
                    info!(
                        "{}: {} invalidly sent {:?}",
                        self, client.public_id, response
                    );
                }
                Ok(Message::Notification { notification, .. }) => {
                    info!(
                        "{}: {} invalidly sent {:?}",
                        self, client.public_id, notification
                    );
                }
                Err(err) => {
                    info!(
                        "{}: Unable to deserialise message from {}: {}",
                        self, client.public_id, err
                    );
                }
            }
        } else {
            match bincode::deserialize(&bytes) {
                Ok(HandshakeRequest::Bootstrap(client_id)) => {
                    self.handle_bootstrap_request(peer_addr, client_id);
                }
                Ok(HandshakeRequest::Join(client_id)) => {
                    self.handle_join_request(peer_addr, client_id);
                }
                Ok(HandshakeRequest::ChallengeResult(signature)) => {
                    // Handle challenge
                    self.handle_challenge(peer_addr, signature);
                }
                Err(err) => {
                    info!(
                        "{}: Unable to deserialise handshake request from {}: {}",
                        self, peer_addr, err
                    );
                }
            }
        }
        None
    }

    #[allow(clippy::cognitive_complexity)]
    fn handle_client_request(
        &mut self,
        client: &ClientInfo,
        request: Request,
        message_id: MessageId,
        signature: Option<Signature>,
    ) -> Option<Action> {
        use Request::*;
        trace!(
            "{}: Received ({:?} {:?}) from {}",
            self,
            request,
            message_id,
            client.public_id
        );

        self.verify_signature(&client.public_id, &request, message_id, signature)?;
        self.authorise_app(&client.public_id, &request, message_id)?;
        self.verify_consistent_address(&request, message_id)?;

        match request {
            //
            // ===== Immutable Data =====
            //
            PutIData(chunk) => self.handle_put_idata(client, chunk, message_id),
            GetIData(address) => {
                // TODO: We don't check for the existence of a valid signature for published data,
                // since it's free for anyone to get.  However, as a means of spam prevention, we
                // could change this so that signatures are required, and the signatures would need
                // to match a pattern which becomes increasingly difficult as the client's
                // behaviour is deemed to become more "spammy". (e.g. the get requests include a
                // `seed: [u8; 32]`, and the client needs to form a sig matching a required pattern
                // by brute-force attempts with varying seeds)
                self.handle_get_idata(client, address, message_id)
            }
            DeleteUnpubIData(address) => {
                self.handle_delete_unpub_idata(client, address, message_id)
            }
            //
            // ===== Mutable Data =====
            //
            PutMData(chunk) => self.handle_put_mdata(client, chunk, message_id),
            MutateMDataEntries { .. }
            | SetMDataUserPermissions { .. }
            | DelMDataUserPermissions { .. } => {
                self.handle_mutate_mdata(request, client, message_id)
            }
            DeleteMData(..) => self.handle_delete_mdata(request, client, message_id),
            GetMData(..)
            | GetMDataVersion(..)
            | GetMDataShell(..)
            | GetMDataValue { .. }
            | ListMDataPermissions(..)
            | ListMDataUserPermissions { .. }
            | ListMDataEntries(..)
            | ListMDataKeys(..)
            | ListMDataValues(..) => self.handle_get_mdata(request, client, message_id),
            //
            // ===== Append Only Data =====
            //
            PutAData(chunk) => self.handle_put_adata(client, chunk, message_id),
            GetAData(_)
            | GetADataShell { .. }
            | GetADataRange { .. }
            | GetADataIndices(_)
            | GetADataLastEntry(_)
            | GetADataOwners { .. }
            | GetADataPermissions { .. }
            | GetPubADataUserPermissions { .. }
            | GetUnpubADataUserPermissions { .. }
            | GetADataValue { .. } => self.handle_get_adata(client, request, message_id),
            DeleteAData(address) => self.handle_delete_adata(client, address, message_id),
            AddPubADataPermissions { .. }
            | AddUnpubADataPermissions { .. }
            | SetADataOwner { .. }
            | AppendSeq { .. }
            | AppendUnseq(..) => self.handle_mutate_adata(client, request, message_id),
            //
            // ===== Coins =====
            //
            TransferCoins {
                destination,
                amount,
                transaction_id,
            } => self.handle_transfer_coins_client_req(
                &client.public_id,
                destination,
                amount,
                transaction_id,
                message_id,
            ),
            GetBalance => {
                let balance = self
                    .balance(client.public_id.name())
                    .ok_or(NdError::NoSuchBalance);
                let response = Response::GetBalance(balance);
                self.send_response_to_client(message_id, response);
                None
            }
            CreateBalance {
                new_balance_owner,
                amount,
                transaction_id,
            } => self.handle_create_balance_client_req(
                &client.public_id,
                new_balance_owner,
                amount,
                transaction_id,
                message_id,
            ),
            //
            // ===== Login packets =====
            //
            CreateLoginPacket(login_packet) => self.handle_create_login_packet_client_req(
                &client.public_id,
                login_packet,
                message_id,
            ),
            CreateLoginPacketFor {
                new_owner,
                amount,
                transaction_id,
                new_login_packet,
            } => self.handle_chained_create_login_packet_client_req(
                &client.public_id,
                new_owner,
                amount,
                transaction_id,
                new_login_packet,
                message_id,
            ),
            UpdateLoginPacket(updated_login_packet) => self.handle_update_login_packet_req(
                client.public_id.clone(),
                updated_login_packet,
                message_id,
            ),
            GetLoginPacket(ref address) => {
                self.handle_get_login_packet_req(&client.public_id, address, message_id)
            }
            //
            // ===== Client (Owner) to ClientHandlers =====
            //
            ListAuthKeysAndVersion => self.handle_list_auth_keys_and_version(client, message_id),
            InsAuthKey {
                key,
                version,
                permissions,
            } => self.handle_ins_auth_key_client_req(client, key, version, permissions, message_id),
            DelAuthKey { key, version } => {
                self.handle_del_auth_key_client_req(client, key, version, message_id)
            }
        }
    }

    fn is_valid_client_signature(
        &self,
        client_id: &PublicId,
        request: &Request,
        message_id: &MessageId,
        signature: &Signature,
    ) -> bool {
        let pub_key = match utils::own_key(client_id) {
            Some(pk) => pk,
            None => {
                error!("{}: Logic error.  This should be unreachable.", self);
                return false;
            }
        };
        match pub_key.verify(signature, utils::serialise(&(request, message_id))) {
            Ok(_) => true,
            Err(error) => {
                warn!(
                    "{}: ({:?}/{:?}) from {} is invalid: {}",
                    self, request, message_id, client_id, error
                );
                false
            }
        }
    }

    fn handle_get_mdata(
        &mut self,
        request: Request,
        client: &ClientInfo,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ForwardClientRequest(Rpc::Request {
            requester: client.public_id.clone(),
            request,
            message_id,
        }))
    }

    fn handle_mutate_mdata(
        &mut self,
        request: Request,
        client: &ClientInfo,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ConsensusVote(ConsensusAction::PayAndForward {
            request,
            client_public_id: client.public_id.clone(),
            message_id,
            cost: *COST_OF_PUT,
        }))
    }

    fn handle_delete_mdata(
        &mut self,
        request: Request,
        client: &ClientInfo,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ConsensusVote(ConsensusAction::Forward {
            request,
            client_public_id: client.public_id.clone(),
            message_id,
        }))
    }

    fn handle_put_mdata(
        &mut self,
        client: &ClientInfo,
        chunk: MData,
        message_id: MessageId,
    ) -> Option<Action> {
        let owner = utils::owner(&client.public_id)?;

        // Assert that the owner's public key has been added to the chunk, to avoid Apps
        // putting chunks which can't be retrieved by their Client owners.
        if chunk.owner() != *owner.public_key() {
            trace!(
                "{}: {} attempted PutMData with invalid owners field.",
                self,
                client.public_id
            );
            self.send_response_to_client(
                message_id,
                Response::Mutation(Err(NdError::InvalidOwners)),
            );
            return None;
        }

        let request = Request::PutMData(chunk);

        Some(Action::ConsensusVote(ConsensusAction::PayAndForward {
            request,
            client_public_id: client.public_id.clone(),
            message_id,
            cost: *COST_OF_PUT,
        }))
    }

    fn handle_put_idata(
        &mut self,
        client: &ClientInfo,
        chunk: IData,
        message_id: MessageId,
    ) -> Option<Action> {
        let owner = utils::owner(&client.public_id)?;

        // Assert that if the request was for UnpubIData, that the owner's public key has
        // been added to the chunk, to avoid Apps putting chunks which can't be retrieved
        // by their Client owners.
        if let IData::Unpub(ref unpub_chunk) = &chunk {
            if unpub_chunk.owner() != owner.public_key() {
                trace!(
                    "{}: {} attempted Put UnpubIData with invalid owners field.",
                    self,
                    client.public_id
                );
                self.send_response_to_client(
                    message_id,
                    Response::Mutation(Err(NdError::InvalidOwners)),
                );
                return None;
            }
        }

        let request = Request::PutIData(chunk);
        Some(Action::ConsensusVote(ConsensusAction::PayAndForward {
            request,
            client_public_id: client.public_id.clone(),
            message_id,
            cost: *COST_OF_PUT,
        }))
    }

    fn handle_get_idata(
        &mut self,
        client: &ClientInfo,
        address: IDataAddress,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ForwardClientRequest(Rpc::Request {
            requester: client.public_id.clone(),
            request: Request::GetIData(address),
            message_id,
        }))
    }

    fn handle_delete_unpub_idata(
        &mut self,
        client: &ClientInfo,
        address: IDataAddress,
        message_id: MessageId,
    ) -> Option<Action> {
        if address.kind() == IDataKind::Pub {
            self.send_response_to_client(
                message_id,
                Response::Mutation(Err(NdError::InvalidOperation)),
            );
            return None;
        }
        Some(Action::ConsensusVote(ConsensusAction::Forward {
            request: Request::DeleteUnpubIData(address),
            client_public_id: client.public_id.clone(),
            message_id,
        }))
    }

    fn handle_get_adata(
        &mut self,
        client: &ClientInfo,
        request: Request,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ForwardClientRequest(Rpc::Request {
            requester: client.public_id.clone(),
            request,
            message_id,
        }))
    }

    fn handle_put_adata(
        &mut self,
        client: &ClientInfo,
        chunk: AData,
        message_id: MessageId,
    ) -> Option<Action> {
        let owner = utils::owner(&client.public_id)?;
        // TODO - Should we replace this with a adata.check_permission call in data_handler.
        // That would be more consistent, but on the other hand a check here stops spam earlier.
        if chunk.check_is_last_owner(*owner.public_key()).is_err() {
            trace!(
                "{}: {} attempted Put AppendOnlyData with invalid owners.",
                self,
                client.public_id
            );
            self.send_response_to_client(
                message_id,
                Response::Mutation(Err(NdError::InvalidOwners)),
            );
            return None;
        }

        let request = Request::PutAData(chunk);
        Some(Action::ConsensusVote(ConsensusAction::PayAndForward {
            request,
            client_public_id: client.public_id.clone(),
            message_id,
            cost: *COST_OF_PUT,
        }))
    }

    fn handle_delete_adata(
        &mut self,
        client: &ClientInfo,
        address: ADataAddress,
        message_id: MessageId,
    ) -> Option<Action> {
        if address.is_pub() {
            self.send_response_to_client(
                message_id,
                Response::Mutation(Err(NdError::InvalidOperation)),
            );
            return None;
        }

        Some(Action::ConsensusVote(ConsensusAction::Forward {
            request: Request::DeleteAData(address),
            client_public_id: client.public_id.clone(),
            message_id,
        }))
    }

    fn handle_mutate_adata(
        &mut self,
        client: &ClientInfo,
        request: Request,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ConsensusVote(ConsensusAction::PayAndForward {
            request,
            client_public_id: client.public_id.clone(),
            message_id,
            cost: *COST_OF_PUT,
        }))
    }

    /// Handles a received join request from a client.
    fn handle_join_request(&mut self, peer_addr: SocketAddr, client_id: PublicId) {
        if !self
            .routing_node
            .borrow()
            .matches_our_prefix(client_id.name())
        {
            debug!(
                "Client {} ({}) wants to join us but we are not its client handler",
                client_id, peer_addr
            );
            self.routing_node
                .borrow_mut()
                .disconnect_from_client(peer_addr);
        }
        let challenge = utils::random_vec(8);
        self.send(
            peer_addr,
            &HandshakeResponse::Challenge(PublicId::Node(self.id.clone()), challenge.clone()),
        );
        let _ = self
            .client_candidates
            .insert(peer_addr, (challenge, client_id));
    }

    /// Handles a received challenge response.
    ///
    /// Checks that the response contains a valid signature of the challenge we previously sent.
    /// If a client requests the section info, we also send it.
    fn handle_challenge(&mut self, peer_addr: SocketAddr, signature: Signature) {
        if let Some((challenge, public_id)) = self.client_candidates.remove(&peer_addr) {
            let public_key = match utils::own_key(&public_id) {
                Some(pk) => pk,
                None => {
                    info!(
                        "{}: Client on {} identifies as a node: {}",
                        self, peer_addr, public_id
                    );
                    if let Err(err) = self
                        .routing_node
                        .borrow_mut()
                        .disconnect_from_client(peer_addr)
                    {
                        warn!("{}: Could not disconnect client: {:?}", self, err);
                    }
                    return;
                }
            };
            match public_key.verify(&signature, challenge) {
                Ok(()) => {
                    info!("{}: Accepted {} on {}.", self, public_id, peer_addr,);
                    let _ = self.clients.insert(peer_addr, ClientInfo { public_id });
                }
                Err(err) => {
                    info!(
                        "{}: Challenge failed for {} on {}: {}",
                        self, public_id, peer_addr, err
                    );
                    if let Err(err) = self
                        .routing_node
                        .borrow_mut()
                        .disconnect_from_client(peer_addr)
                    {
                        warn!("{}: Could not disconnect client: {:?}", self, err);
                    }
                }
            }
        } else {
            info!(
                "{}: {} supplied challenge response without us providing it.",
                self, peer_addr
            );
            if let Err(err) = self
                .routing_node
                .borrow_mut()
                .disconnect_from_client(peer_addr)
            {
                warn!("{}: Could not disconnect client: {:?}", self, err);
            }
        }
    }

    pub fn handle_vault_rpc(&mut self, src: XorName, rpc: Rpc) -> Option<Action> {
        match rpc {
            Rpc::Request {
                request,
                requester,
                message_id,
            } => self.handle_vault_request(src, requester, request, message_id),
            Rpc::Response {
                response,
                requester,
                message_id,
                refund,
            } => self.handle_response(src, requester, response, message_id, refund),
        }
    }

    fn handle_vault_request(
        &mut self,
        src: XorName,
        requester: PublicId,
        request: Request,
        message_id: MessageId,
    ) -> Option<Action> {
        use Request::*;
        trace!(
            "{}: Received ({:?} {:?}) from src {} (client {:?})",
            self,
            request,
            message_id,
            src,
            requester
        );
        match request {
            CreateLoginPacket(ref login_packet) => {
                self.handle_create_login_packet_vault_req(requester, login_packet, message_id)
            }
            CreateLoginPacketFor {
                new_owner,
                amount,
                transaction_id,
                new_login_packet,
            } => self.handle_chained_create_login_packet_vault_req(
                src,
                requester,
                new_owner,
                amount,
                transaction_id,
                new_login_packet,
                message_id,
            ),
            CreateBalance {
                new_balance_owner,
                amount,
                transaction_id,
            } => self.handle_create_balance_vault_req(
                requester,
                new_balance_owner,
                amount,
                transaction_id,
                message_id,
            ),
            TransferCoins {
                destination,
                amount,
                transaction_id,
            } => self.handle_transfer_coins_vault_req(
                requester,
                destination,
                amount,
                transaction_id,
                message_id,
            ),
            UpdateLoginPacket(updated_login_packet) => self.handle_update_login_packet_vault_req(
                requester,
                updated_login_packet,
                message_id,
            ),
            InsAuthKey {
                key,
                version,
                permissions,
            } => self.handle_ins_auth_key_vault(requester, key, version, permissions, message_id),
            DelAuthKey { key, version } => {
                self.handle_del_auth_key_vault(requester, key, version, message_id)
            }
            PutIData(_)
            | GetIData(_)
            | DeleteUnpubIData(_)
            | PutMData(_)
            | GetMData(_)
            | GetMDataValue { .. }
            | DeleteMData(_)
            | GetMDataShell(_)
            | GetMDataVersion(_)
            | ListMDataEntries(_)
            | ListMDataKeys(_)
            | ListMDataValues(_)
            | SetMDataUserPermissions { .. }
            | DelMDataUserPermissions { .. }
            | ListMDataPermissions(_)
            | ListMDataUserPermissions { .. }
            | MutateMDataEntries { .. }
            | PutAData(_)
            | GetAData(_)
            | GetADataShell { .. }
            | GetADataValue { .. }
            | DeleteAData(_)
            | GetADataRange { .. }
            | GetADataIndices(_)
            | GetADataLastEntry(_)
            | GetADataPermissions { .. }
            | GetPubADataUserPermissions { .. }
            | GetUnpubADataUserPermissions { .. }
            | GetADataOwners { .. }
            | AddPubADataPermissions { .. }
            | AddUnpubADataPermissions { .. }
            | SetADataOwner { .. }
            | AppendSeq { .. }
            | AppendUnseq(_)
            | GetBalance
            | ListAuthKeysAndVersion
            | GetLoginPacket(..) => {
                error!(
                    "{}: Should not receive {:?} as a client handler.",
                    self, request
                );
                None
            }
        }
    }

    /// Handle response from the data handlers.
    fn handle_response(
        &mut self,
        data_handlers: XorName,
        requester: PublicId,
        response: Response,
        message_id: MessageId,
        refund: Option<Coins>,
    ) -> Option<Action> {
        use Response::*;
        trace!(
            "{}: Received ({:?} {:?}) to {} from {}",
            self,
            response,
            message_id,
            requester,
            data_handlers
        );

        if let Some(refund_amount) = refund {
            if let Err(error) = self.deposit(requester.name(), refund_amount) {
                error!(
                    "{}: Failed to refund {} coins for {:?}: {:?}",
                    self, refund_amount, requester, error,
                )
            };
        }

        match response {
            // Transfer the response from data handlers to clients
            GetIData(..)
            | GetAData(..)
            | GetADataShell(..)
            | GetADataRange(..)
            | GetADataIndices(..)
            | GetADataLastEntry(..)
            | GetADataOwners(..)
            | GetPubADataUserPermissions(..)
            | GetUnpubADataUserPermissions(..)
            | GetADataPermissions(..)
            | GetADataValue(..)
            | GetMData(..)
            | GetMDataShell(..)
            | GetMDataVersion(..)
            | ListMDataEntries(..)
            | ListMDataKeys(..)
            | ListMDataValues(..)
            | ListMDataUserPermissions(..)
            | ListMDataPermissions(..)
            | GetMDataValue(..)
            | Mutation(..)
            | Transaction(..) => {
                self.send_response_to_client(message_id, response);
                None
            }
            //
            // ===== Invalid =====
            //
            GetLoginPacket(_) | GetBalance(_) | ListAuthKeysAndVersion(_) => {
                error!(
                    "{}: Should not receive {:?} as a client handler.",
                    self, response
                );
                None
            }
        }
    }

    fn handle_create_balance_client_req(
        &mut self,
        requester: &PublicId,
        owner_key: PublicKey,
        amount: Coins,
        transaction_id: TransactionId,
        message_id: MessageId,
    ) -> Option<Action> {
        let request = Request::CreateBalance {
            new_balance_owner: owner_key,
            amount,
            transaction_id,
        };
        // For phases 1 & 2 we allow owners to create their own balance freely.
        let own_request = utils::own_key(requester)
            .map(|key| key == &owner_key)
            .unwrap_or(false);
        if own_request {
            return Some(Action::ConsensusVote(ConsensusAction::Forward {
                request: request.clone(),
                client_public_id: requester.clone(),
                message_id,
            }));
        }

        let total_amount = amount.checked_add(*COST_OF_PUT)?;
        // When ClientA(owner/app with permissions) creates a balance for ClientB
        Some(Action::ConsensusVote(ConsensusAction::PayAndForward {
            request,
            client_public_id: requester.clone(),
            message_id,
            cost: total_amount,
        }))
    }

    fn handle_create_balance_vault_req(
        &mut self,
        requester: PublicId,
        owner_key: PublicKey,
        amount: Coins,
        transaction_id: TransactionId,
        message_id: MessageId,
    ) -> Option<Action> {
        let (result, refund) = match self.create_balance(&requester, owner_key, amount) {
            Ok(()) => {
                let destination = XorName::from(owner_key);
                let transaction = Transaction {
                    id: transaction_id,
                    amount,
                };
                self.notify_destination_owners(&destination, transaction);
                (Ok(transaction), None)
            }
            Err(error) => {
                // Refund amount (Including the cost of creating a balance)
                let amount = amount.checked_add(*COST_OF_PUT)?;
                (Err(error), Some(amount))
            }
        };

        Some(Action::RespondToClientHandlers {
            sender: *self.id.name(),
            rpc: Rpc::Response {
                response: Response::Transaction(result),
                requester,
                message_id,
                refund,
            },
        })
    }

    fn handle_transfer_coins_client_req(
        &mut self,
        requester: &PublicId,
        destination: XorName,
        amount: Coins,
        transaction_id: TransactionId,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ConsensusVote(ConsensusAction::PayAndForward {
            request: Request::TransferCoins {
                destination,
                amount,
                transaction_id,
            },
            client_public_id: requester.clone(),
            message_id,
            cost: amount,
        }))
    }

    fn handle_transfer_coins_vault_req(
        &mut self,
        requester: PublicId,
        destination: XorName,
        amount: Coins,
        transaction_id: TransactionId,
        message_id: MessageId,
    ) -> Option<Action> {
        let (result, refund) = match self.deposit(&destination, amount) {
            Ok(()) => {
                let transaction = Transaction {
                    id: transaction_id,
                    amount,
                };

                self.notify_destination_owners(&destination, transaction);
                (Ok(transaction), None)
            }
            Err(error) => (Err(error), Some(amount)),
        };

        Some(Action::RespondToClientHandlers {
            sender: *self.id.name(),
            rpc: Rpc::Response {
                response: Response::Transaction(result),
                requester,
                message_id,
                refund,
            },
        })
    }

    fn handle_update_login_packet_vault_req(
        &mut self,
        requester: PublicId,
        updated_login_packet: LoginPacket,
        message_id: MessageId,
    ) -> Option<Action> {
        let result = self
            .login_packet(
                utils::own_key(&requester)?,
                updated_login_packet.destination(),
            )
            .and_then(|_existing_login_packet| {
                if !updated_login_packet.size_is_valid() {
                    return Err(NdError::ExceededSize);
                }
                self.login_packets
                    .put(&updated_login_packet)
                    .map_err(|err| err.to_string().into())
            });
        Some(Action::RespondToClientHandlers {
            sender: *self.id.name(),
            rpc: Rpc::Response {
                response: Response::Mutation(result),
                requester,
                message_id,
                // Updating the login packet is free
                refund: None,
            },
        })
    }

    fn notify_destination_owners(&mut self, destination: &XorName, transaction: Transaction) {
        for client_id in self.lookup_client_and_its_apps(destination) {
            self.send_notification_to_client(client_id, Notification(transaction));
        }
    }

    fn create_balance(
        &mut self,
        requester: &PublicId,
        owner_key: PublicKey,
        amount: Coins,
    ) -> Result<(), NdError> {
        let own_request = utils::own_key(requester)
            .map(|key| key == &owner_key)
            .unwrap_or(false);
        if !own_request && self.balances.exists(&owner_key) {
            info!(
                "{}: Failed to create balance for {:?}: already exists.",
                self, owner_key
            );

            Err(NdError::BalanceExists)
        } else {
            let balance = Balance { coins: amount };
            self.put_balance(&owner_key, &balance)?;
            Ok(())
        }
    }

    fn send<T: Serialize>(&mut self, recipient: SocketAddr, msg: &T) {
        let msg = utils::serialise(msg);
        let msg = Bytes::from(msg);

        if let Err(e) = self
            .routing_node
            .borrow_mut()
            .send_message_to_client(recipient, msg, 0)
        {
            warn!(
                "{}: Could not send message to client {}: {:?}",
                self, recipient, e
            );
        }
    }

    fn handle_bootstrap_request(&mut self, peer_addr: SocketAddr, client_id: PublicId) {
        if !self
            .routing_node
            .borrow()
            .match_our_prefix(client_id.name())
        {
            let closest_known_elders = match self
                .routing_node
                .borrow()
                .closest_known_elders_to(client_id.name())
            {
                Ok(elders_iter) => elders_iter.map(|p2p_node| {
                        let routing::ConnectionInfo {
                            peer_addr,
                            peer_cert_der,
                        } = p2p_node.connection_info().clone();
                        (
                            XorName(p2p_node.name().0),
                            ConnectionInfo {
                                peer_addr,
                                peer_cert_der,
                            },
                        )
                    })
                    .collect::<Vec<_>>()
                Err(e) => {
                    info!("Could not handle bootstrap request: {:?}", e);
                    return;
                }
            };

            if let Some(elders) = closest_known_elders {
                self.send(peer_addr, &HandshakeResponse::Join(elders));
            } else {
                warn!("{}: No closest known elders in any section we know of", self);
            }
        } else {
            let elders = self
                .routing_node
                .borrow_mut()
                .our_elders_info()
                .map(|iter| {
                    iter.map(|p2p_node| {
                        let routing::ConnectionInfo {
                            peer_addr,
                            peer_cert_der,
                        } = p2p_node.connection_info().clone();
                        (
                            XorName(p2p_node.name().0),
                            ConnectionInfo {
                                peer_addr,
                                peer_cert_der,
                            },
                        )
                    })
                    .collect::<Vec<_>>()
                });

            if let Some(elders) = elders {
                self.send(peer_addr, &HandshakeResponse::Join(elders));
            } else {
                warn!("{}: No other elders in our section found", self);
            }
        }
    }

    fn send_notification_to_client(&mut self, client_id: PublicId, notification: Notification) {
        let peer_addrs = self.lookup_client_peer_addrs(&client_id);

        if peer_addrs.is_empty() {
            info!(
                "{}: can't notify {} as none of the instances of the client is connected.",
                self, client_id
            );
            return;
        };

        for peer_addr in peer_addrs {
            self.send(
                peer_addr,
                &Message::Notification {
                    notification: notification.clone(),
                },
            )
        }
    }

    fn send_response_to_client(&mut self, message_id: MessageId, response: Response) {
        let peer_addr = match self.pending_msg_ids.remove(&message_id) {
            Some(peer_addr) => peer_addr,
            None => {
                info!("Expired message-id. Unable to find the client to respond to.");
                return;
            }
        };

        self.send(
            peer_addr,
            &Message::Response {
                response,
                message_id,
            },
        )
    }

    fn lookup_client_peer_addrs(&self, id: &PublicId) -> Vec<SocketAddr> {
        self.clients
            .iter()
            .filter_map(|(peer_addr, client)| {
                if &client.public_id == id {
                    Some(*peer_addr)
                } else {
                    None
                }
            })
            .collect()
    }

    fn lookup_client_and_its_apps(&self, name: &XorName) -> Vec<PublicId> {
        self.clients
            .values()
            .filter_map(|client| {
                if client.public_id.name() == name {
                    Some(client.public_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    }

    fn balance<K: balance::Key>(&self, key: &K) -> Option<Coins> {
        self.balances.get(key).map(|balance| balance.coins)
    }

    fn withdraw<K: balance::Key>(&mut self, key: &K, amount: Coins) -> Result<(), NdError> {
        if amount.as_nano() == 0 {
            return Err(NdError::InvalidOperation);
        }
        let (public_key, mut balance) = self
            .balances
            .get_key_value(key)
            .ok_or(NdError::NoSuchBalance)?;
        balance.coins = balance
            .coins
            .checked_sub(amount)
            .ok_or(NdError::InsufficientBalance)?;
        self.put_balance(&public_key, &balance)
    }

    fn deposit<K: balance::Key>(&mut self, key: &K, amount: Coins) -> Result<(), NdError> {
        let (public_key, mut balance) = self
            .balances
            .get_key_value(key)
            .ok_or_else(|| NdError::NoSuchBalance)?;
        balance.coins = balance
            .coins
            .checked_add(amount)
            .ok_or(NdError::ExcessiveValue)?;

        self.put_balance(&public_key, &balance)
    }

    fn put_balance(&mut self, public_key: &PublicKey, balance: &Balance) -> Result<(), NdError> {
        trace!(
            "{}: Setting balance to {} for {}",
            self,
            balance,
            public_key
        );
        self.balances.put(public_key, balance).map_err(|error| {
            error!(
                "{}: Failed to update balance of {}: {}",
                self, public_key, error
            );

            NdError::from("Failed to update balance")
        })
    }

    // Pays cost of a request.
    fn pay(
        &mut self,
        requester_id: &PublicId,
        requester_key: &PublicKey,
        request: &Request,
        message_id: MessageId,
        cost: Coins,
    ) -> Option<()> {
        trace!("{}: {} is paying {} coins", self, requester_id, cost);
        match self.withdraw(requester_key, cost) {
            Ok(()) => Some(()),
            Err(error) => {
                trace!("{}: Unable to withdraw {} coins: {}", self, cost, error);
                self.send_response_to_client(message_id, request.error_response(error));
                None
            }
        }
    }

    fn handle_create_login_packet_client_req(
        &mut self,
        client_id: &PublicId,
        login_packet: LoginPacket,
        message_id: MessageId,
    ) -> Option<Action> {
        if !login_packet.size_is_valid() {
            self.send_response_to_client(
                message_id,
                Response::Mutation(Err(NdError::ExceededSize)),
            );
            return None;
        }

        let request = Request::CreateLoginPacket(login_packet);

        Some(Action::ConsensusVote(ConsensusAction::PayAndForward {
            request,
            client_public_id: client_id.clone(),
            message_id,
            cost: *COST_OF_PUT,
        }))
    }

    fn handle_create_login_packet_vault_req(
        &mut self,
        requester: PublicId,
        login_packet: &LoginPacket,
        message_id: MessageId,
    ) -> Option<Action> {
        let result = if self.login_packets.has(login_packet.destination()) {
            Err(NdError::LoginPacketExists)
        } else {
            self.login_packets
                .put(login_packet)
                .map_err(|error| error.to_string().into())
        };
        let refund = utils::get_refund_for_put(&result);
        Some(Action::RespondToClientHandlers {
            sender: *login_packet.destination(),
            rpc: Rpc::Response {
                response: Response::Mutation(result),
                requester,
                message_id,
                refund,
            },
        })
    }

    /// Step one of the process - the payer is effectively doing a `CreateBalance` request to
    /// new_owner, and bundling the new_owner's `CreateLoginPacket` along with it.
    fn handle_chained_create_login_packet_client_req(
        &mut self,
        payer: &PublicId,
        new_owner: PublicKey,
        amount: Coins,
        transaction_id: TransactionId,
        login_packet: LoginPacket,
        message_id: MessageId,
    ) -> Option<Action> {
        if !login_packet.size_is_valid() {
            self.send_response_to_client(
                message_id,
                Response::Transaction(Err(NdError::ExceededSize)),
            );
            return None;
        }
        // The requester bears the cost of storing the login packet
        let new_amount = amount.checked_add(*COST_OF_PUT)?;
        Some(Action::ConsensusVote(ConsensusAction::PayAndProxy {
            request: Request::CreateLoginPacketFor {
                new_owner,
                amount,
                transaction_id,
                new_login_packet: login_packet,
            },
            client_public_id: payer.clone(),
            message_id,
            cost: new_amount,
        }))
    }

    /// Step two or three of the process - the payer is effectively doing a `CreateBalance` request
    /// to new_owner, and bundling the new_owner's `CreateLoginPacket` along with it.
    #[allow(clippy::too_many_arguments)]
    fn handle_chained_create_login_packet_vault_req(
        &mut self,
        src: XorName,
        payer: PublicId,
        new_owner: PublicKey,
        amount: Coins,
        transaction_id: TransactionId,
        login_packet: LoginPacket,
        message_id: MessageId,
    ) -> Option<Action> {
        if &src == payer.name() {
            // Step two - create balance and forward login_packet.
            if let Err(error) = self.create_balance(&payer, new_owner, amount) {
                // Refund amount (Including the cost of creating the balance)
                let refund = Some(amount.checked_add(*COST_OF_PUT)?);

                Some(Action::RespondToClientHandlers {
                    sender: XorName::from(new_owner),
                    rpc: Rpc::Response {
                        response: Response::Transaction(Err(error)),
                        requester: payer,
                        message_id,
                        // TODO: Does CreateLoginPacketFor charge
                        // for creation of balance *and* login packet ?
                        refund,
                    },
                })
            } else {
                Some(Action::ForwardClientRequest(Rpc::Request {
                    request: Request::CreateLoginPacketFor {
                        new_owner,
                        amount,
                        transaction_id,
                        new_login_packet: login_packet,
                    },
                    requester: payer.clone(),
                    message_id,
                }))
            }
        } else {
            // Step three - store login_packet.
            let result = if self.login_packets.has(login_packet.destination()) {
                Err(NdError::LoginPacketExists)
            } else {
                self.login_packets
                    .put(&login_packet)
                    .map(|_| Transaction {
                        id: transaction_id,
                        amount,
                    })
                    .map_err(|error| error.to_string().into())
            };
            let refund = utils::get_refund_for_put(&result);
            Some(Action::RespondToClientHandlers {
                sender: *login_packet.destination(),
                rpc: Rpc::Response {
                    response: Response::Transaction(result),
                    requester: payer,
                    message_id,
                    refund,
                },
            })
        }
    }

    fn handle_update_login_packet_req(
        &mut self,
        client_id: PublicId,
        updated_login_packet: LoginPacket,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ConsensusVote(ConsensusAction::Forward {
            request: Request::UpdateLoginPacket(updated_login_packet),
            client_public_id: client_id,
            message_id,
        }))
    }

    fn handle_get_login_packet_req(
        &mut self,
        client_id: &PublicId,
        address: &XorName,
        message_id: MessageId,
    ) -> Option<Action> {
        let result = self
            .login_packet(utils::own_key(client_id)?, address)
            .map(LoginPacket::into_data_and_signature);
        self.send_response_to_client(message_id, Response::GetLoginPacket(result));
        None
    }

    fn login_packet(
        &self,
        requester_pub_key: &PublicKey,
        packet_name: &XorName,
    ) -> NdResult<LoginPacket> {
        self.login_packets
            .get(packet_name)
            .map_err(|e| match e {
                ChunkStoreError::NoSuchChunk => NdError::NoSuchLoginPacket,
                error => error.to_string().into(),
            })
            .and_then(|login_packet| {
                if login_packet.authorised_getter() == requester_pub_key {
                    Ok(login_packet)
                } else {
                    Err(NdError::AccessDenied)
                }
            })
    }

    fn handle_list_auth_keys_and_version(
        &mut self,
        client: &ClientInfo,
        message_id: MessageId,
    ) -> Option<Action> {
        let result = Ok(self
            .auth_keys
            .list_auth_keys_and_version(utils::client(&client.public_id)?));

        self.send_response_to_client(message_id, Response::ListAuthKeysAndVersion(result));
        None
    }

    fn handle_ins_auth_key_client_req(
        &self,
        client: &ClientInfo,
        key: PublicKey,
        new_version: u64,
        permissions: AppPermissions,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ConsensusVote(ConsensusAction::Forward {
            request: Request::InsAuthKey {
                key,
                version: new_version,
                permissions,
            },
            client_public_id: client.public_id.clone(),
            message_id,
        }))
    }

    fn handle_ins_auth_key_vault(
        &mut self,
        client: PublicId,
        key: PublicKey,
        new_version: u64,
        permissions: AppPermissions,
        message_id: MessageId,
    ) -> Option<Action> {
        let result =
            self.auth_keys
                .ins_auth_key(utils::client(&client)?, key, new_version, permissions);
        Some(Action::RespondToClientHandlers {
            sender: *self.id.name(),
            rpc: Rpc::Response {
                response: Response::Mutation(result),
                requester: client,
                message_id,
                // Free operation
                refund: None,
            },
        })
    }

    fn handle_del_auth_key_client_req(
        &mut self,
        client: &ClientInfo,
        key: PublicKey,
        new_version: u64,
        message_id: MessageId,
    ) -> Option<Action> {
        Some(Action::ConsensusVote(ConsensusAction::Forward {
            request: Request::DelAuthKey {
                key,
                version: new_version,
            },
            client_public_id: client.public_id.clone(),
            message_id,
        }))
    }

    fn handle_del_auth_key_vault(
        &mut self,
        client: PublicId,
        key: PublicKey,
        new_version: u64,
        message_id: MessageId,
    ) -> Option<Action> {
        let result = self
            .auth_keys
            .del_auth_key(utils::client(&client)?, key, new_version);
        Some(Action::RespondToClientHandlers {
            sender: *self.id.name(),
            rpc: Rpc::Response {
                response: Response::Mutation(result),
                requester: client,
                message_id,
                // Free operation
                refund: None,
            },
        })
    }

    // Verify that valid signature is provided if the request requires it.
    fn verify_signature(
        &mut self,
        public_id: &PublicId,
        request: &Request,
        message_id: MessageId,
        signature: Option<Signature>,
    ) -> Option<()> {
        let signature_required = match utils::authorisation_kind(request) {
            AuthorisationKind::GetUnpub
            | AuthorisationKind::GetBalance
            | AuthorisationKind::TransferCoins
            | AuthorisationKind::Mut
            | AuthorisationKind::MutAndTransferCoins
            | AuthorisationKind::ManageAppKeys => true,
            AuthorisationKind::GetPub => false,
        };

        if !signature_required {
            return Some(());
        }

        let valid = if let Some(signature) = signature {
            self.is_valid_client_signature(public_id, request, &message_id, &signature)
        } else {
            warn!(
                "{}: ({:?}/{:?}) from {} is unsigned",
                self, request, message_id, public_id
            );
            false
        };

        if valid {
            Some(())
        } else {
            self.send_response_to_client(
                message_id,
                request.error_response(NdError::InvalidSignature),
            );
            None
        }
    }

    // If the client is app, check if it is authorised to perform the given request.
    fn authorise_app(
        &mut self,
        public_id: &PublicId,
        request: &Request,
        message_id: MessageId,
    ) -> Option<()> {
        let app_id = match public_id {
            PublicId::App(app_id) => app_id,
            _ => return Some(()),
        };

        let result = match utils::authorisation_kind(request) {
            AuthorisationKind::GetPub => Ok(()),
            AuthorisationKind::GetUnpub => self.check_app_permissions(app_id, |_| true),
            AuthorisationKind::GetBalance => {
                self.check_app_permissions(app_id, |perms| perms.get_balance)
            }
            AuthorisationKind::Mut => {
                self.check_app_permissions(app_id, |perms| perms.perform_mutations)
            }
            AuthorisationKind::TransferCoins => {
                self.check_app_permissions(app_id, |perms| perms.transfer_coins)
            }
            AuthorisationKind::MutAndTransferCoins => self.check_app_permissions(app_id, |perms| {
                perms.transfer_coins && perms.perform_mutations
            }),
            AuthorisationKind::ManageAppKeys => Err(NdError::AccessDenied),
        };

        if let Err(error) = result {
            self.send_response_to_client(message_id, request.error_response(error));
            None
        } else {
            Some(())
        }
    }

    fn check_app_permissions(
        &self,
        app_id: &AppPublicId,
        check: impl FnOnce(AppPermissions) -> bool,
    ) -> Result<(), NdError> {
        if self
            .auth_keys
            .app_permissions(app_id)
            .map(check)
            .unwrap_or(false)
        {
            Ok(())
        } else {
            Err(NdError::AccessDenied)
        }
    }

    fn verify_consistent_address(
        &mut self,
        request: &Request,
        message_id: MessageId,
    ) -> Option<()> {
        use Request::*;
        let consistent = match request {
            AppendSeq { ref append, .. } => append.address.is_seq(),
            AppendUnseq(ref append) => !&append.address.is_seq(),
            // TODO: any other requests for which this can happen?
            _ => true,
        };
        if !consistent {
            self.send_response_to_client(
                message_id,
                Response::Mutation(Err(NdError::InvalidOperation)),
            );
            None
        } else {
            Some(())
        }
    }
}

impl Display for ClientHandler {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "{}", self.id.name())
    }
}
