pub type PeerId = String;
pub type PeerAddr = String;

pub type PeerInfo = (PeerId, PeerAddr);

#[derive(Debug, Copy, Clone)]
pub enum OriginType {
    InBound,
    OutBound,
}
