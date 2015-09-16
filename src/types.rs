// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

/// MethodCall denotes a specific request to be carried out by routing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MethodCall {
    /// request to have `location` to handle put for the `content`
    Put {
        location: ::routing::Authority,
        content: ::routing::data::Data,
    },
    /// request to retrieve data with specified type and location from network
                                                                                            #[allow(dead_code)]
    Get {
        location: ::routing::Authority,
        data_request: ::routing::data::DataRequest,
    },
    // /// request to post
    // Post { destination: ::routing::NameType, content: Data },
    // /// Request delete
    // Delete { name: ::routing::NameType, data : Data },
    /// request to refresh
    // Refresh {
    //     type_tag: u64,
    //     our_authority: ::routing::Authority,
    //     payload: Vec<u8>,
    // },
    /// reply
    Reply {
        data: ::routing::data::Data,
    },
    /// response error indicating failed in putting data
    FailedPut {
        location: ::routing::Authority,
        data: ::routing::data::Data,
    },
    /// response error indicating clearing sacrificial data
    ClearSacrificial {
        location: ::routing::Authority,
        name: ::routing::NameType,
        size: u32,
    },
    /// response error indicating not enough allowance
                                                                                            #[allow(dead_code)]
    LowBalance{
        location: ::routing::Authority,
        data: ::routing::data::Data,
        balance: u32,
    },
    /// response error indicating invalid request
    InvalidRequest {
        data: ::routing::data::Data,
    },
    Deprecated,
}

/// This trait is required for any type (normally an account) which is refreshed on a churn event.
pub trait Refreshable : ::rustc_serialize::Encodable + ::rustc_serialize::Decodable {
    /// The serialised contents
    fn serialised_contents(&self) -> Vec<u8> {
        ::routing::utils::encode(&self).unwrap_or(vec![])
    }

    /// Merge multiple refreshable objects into one
    fn merge(from_group: ::routing::NameType, responses: Vec<Self>) -> Option<Self>;
}

impl Refreshable for ::routing::structured_data::StructuredData {
    fn merge(from_group: ::routing::NameType,
             responses: Vec<::routing::structured_data::StructuredData>)
                    -> Option<::routing::structured_data::StructuredData> {
        let mut sds = Vec::<(::routing::structured_data::StructuredData, u64)>::new();
        for response in responses {
            if response.name() == from_group {
                let push_in_vec = match sds.iter_mut().find(|a| a.0 == response) {
                    Some(find_res) => {
                        find_res.1 += 1;
                        false
                    }
                    None => true,
                };
                if push_in_vec {
                    sds.push((response.clone(), 1));
                }
            }
        }
        sds.sort_by(|a, b| b.1.cmp(&a.1));
        let (sd, count) = sds[0].clone();
        if count >= (::routing::types::GROUP_SIZE as u64 + 1) / 2 {
            return Some(sd);
        }
        None
    }
}
