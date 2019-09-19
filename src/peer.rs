use super::types::{OriginType, PeerInfo};
use bytes::{Bytes, BytesMut};
use futures::{Sink, Stream};
use std::io::Error as IoError;
use std::result::Result as StdResult;
use tokio::codec::{Framed, LengthDelimitedCodec};
use tokio::net::TcpStream;

pub struct ConnectionPeer<S> {
    peer_info: PeerInfo,
    origin: OriginType,
    conn: S,
}

impl<S> ConnectionPeer<S>
where
    S: Stream<Item = StdResult<BytesMut, IoError>> + Sink<Bytes, Error = IoError> + Unpin,
{
    pub fn new(peer_info: PeerInfo, origin: OriginType, conn: S) -> ConnectionPeer<S> {
        ConnectionPeer {
            peer_info,
            origin,
            conn,
        }
    }

    pub fn peer_info(&self) -> &PeerInfo {
        &self.peer_info
    }
    pub fn origin(&self) -> OriginType {
        self.origin
    }
}

pub type Peer = ConnectionPeer<Framed<TcpStream, LengthDelimitedCodec>>;
