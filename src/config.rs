use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::types::{PeerAddr, PeerId};
use std::fmt::Debug;

#[derive(Clone, Deserialize, Serialize, Debug)]
#[serde(default)]
pub struct NodeConfig {
    pub p2p_addr: String,
    pub http_addr: String,
    pub local_peer_id: PeerId,
    pub bootnodes: HashMap<PeerId, PeerAddr>,
    //    pub max_inbounds: u32,
    //    pub max_outbounds: u32,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            p2p_addr: "localhost:18080".to_string(),
            http_addr: "localhost:8080".to_string(),
            local_peer_id: "0".to_string(),
            bootnodes: HashMap::default(),
        }
    }
}
