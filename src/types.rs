use crate::error::P2PError;

use serde::{Deserialize, Serialize};

pub type PeerId = String;
pub type PeerAddr = String;

pub type PeerInfo = (PeerId, PeerAddr);

#[derive(Debug, Copy, Clone)]
pub enum OriginType {
    InBound,
    OutBound,
}

pub type GenericError = Box<dyn std::error::Error + Send + Sync>;
pub type GenericResult<T> = std::result::Result<T, GenericError>;
pub type P2PResult<T> = std::result::Result<T, P2PError>;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum P2PMessage {
    Ping(PeerInfo),
    Pong(PeerInfo),

    // cast
    NewData(u32, Vec<u8>),
    NewPeer(PeerInfo),

    // call
    ScanData(u32, u32),                  // start_index, max_length
    ScanDataResult(Vec<(u32, Vec<u8>)>), // (index, data)

    Heartbeat(Option<u32>), // notify others I'm alive and what's my lastest block index

    DiscoverPeer(Option<u64>, u32), // from_ts_in_milliseconds, max_size
    DisCoverPeerResult(PeerInfo),
}
