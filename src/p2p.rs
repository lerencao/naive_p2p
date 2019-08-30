use std::{
    sync::Arc,
    collections::HashMap,
    result::Result as StdResult,
    io::{Error as IoError, ErrorKind, Result as IoResult},
    net::{SocketAddr}
};
use log::{
    info, warn, error
};
use tokio::{
    prelude::*,
    net::{
        TcpListener,TcpStream
    },
    codec::Framed,
    codec::LengthDelimitedCodec
};

use futures::{
    self,
    Stream,StreamExt,
    channel::mpsc,
    stream::FuturesUnordered,
};
use bytes::{Bytes, BytesMut};
use serde::{Serialize, Deserialize};
use crate::p2p::P2PMessage::{Pong, Ping};
use futures::channel::oneshot;
use crate::error::P2PError;

#[derive(Clone, Default)]
pub struct P2PConfig {
    pub port: u16,
    pub addr: String,
    pub local_peer_id: PeerId,
    pub max_inbounds: u32,
    pub max_outbounds: u32,
}

type PeerId = String;
type PeerAddr = String;

type PeerInfo = (PeerId, PeerAddr);

struct ConnectionPeer<S> {
    peer_info: PeerInfo,
    origin: OriginType,
    conn: S
}

enum OriginType {
    InBound,
    OutBound
}

impl<S> ConnectionPeer<S>
    where S: Stream<Item = StdResult<BytesMut, IoError>> + Sink<Bytes, Error = IoError> + Unpin {
    pub fn new(peer_info: PeerInfo, origin: OriginType, conn: S) -> ConnectionPeer<S> {
        ConnectionPeer {
            peer_info,
            origin,
            conn
        }
    }
}



type Peer = ConnectionPeer<Framed<TcpStream, LengthDelimitedCodec>>;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

type P2PResult<T> = std::result::Result<T, P2PError>;

pub enum P2pEvent {
    NewPeer,
}

#[derive(Serialize, Deserialize)]
pub enum P2PMessage {
    Ping(PeerId),
    Pong(PeerId)
}

pub enum NodeRequest {
    DialPeer((PeerId, PeerAddr), oneshot::Sender<P2PResult<()>>),

}

pub struct NodeRequestSender {
    tx: mpsc::Sender<NodeRequest>,
}

impl NodeRequestSender {
    pub async fn dial(&mut self, peer_id: PeerId, peer_addr: PeerAddr) -> P2PResult<()> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel();
        let req = NodeRequest::DialPeer((peer_id, peer_addr), oneshot_tx);
        self.tx.send(req).await.unwrap();
        oneshot_rx.await?
    }
}
pub struct P2PNode {
    config: P2PConfig,
    bootnodes: Vec<(PeerId, PeerAddr)>,
    active_peers: HashMap<PeerId, Peer>,
    request_rx: mpsc::Receiver<NodeRequest>,
}


impl P2PNode {
    pub fn new(config: P2PConfig, bootnodes: Vec<(PeerId, PeerAddr)>)-> (P2PNode, NodeRequestSender) {
        let (tx, rx) = mpsc::channel(1000);
        let node = P2PNode {
            config,
            bootnodes,
            active_peers: HashMap::default(),
            request_rx: rx
        };
        let sender = NodeRequestSender {tx};
        (node, sender)
    }

    pub async fn start(&mut self) -> Result<()> {
        let addr = format!("{}:{}", self.config.addr, self.config.port);
        let sock_addr = addr.parse().unwrap();
//        let (tx, rx) = mpsc::channel(1000);
//        tokio::spawn(Self::listen(sock_addr));
        let mut listener = Self::listen(&sock_addr).await?.fuse();
        let mut pending_incoming_conns = FuturesUnordered::default();
        let mut pending_outgoing_conns = FuturesUnordered::default();
        let mut dialing_requests = HashMap::default();

        for peer_info in self.bootnodes.iter() {
            let fut = Self::dial(self.config.local_peer_id.clone(), (*peer_info).clone());
            pending_outgoing_conns.push(fut);
        }
        loop {
            futures::select! {
                incoming = listener.select_next_some() => {
                    match incoming {
                        Ok(incoming) => {
                            let fut = Self::accept_conn(self.config.local_peer_id.clone(), incoming);
                            pending_incoming_conns.push(fut);
                        },
                        Err(e) => {
                            warn!("Incoming connection error {}", e);
                        },
                    }
                },
                incoming_peer = pending_incoming_conns.select_next_some() => {
                     match incoming_peer {
                         Ok(peer) => {
                             self.handle_incoming_peer(peer);
                         },
                         Err(e) => warn!("Incoming connection handshake error {}", e),
                     }
                },
                outgoing_peer = pending_outgoing_conns.select_next_some() => {
                    self.handle_outgoing_peer_result(&mut dialing_requests, outgoing_peer);
                },
                external_req = self.request_rx.select_next_some() => {
                    match external_req {
                        NodeRequest::DialPeer(peer_info, resp_tx) => {
                            let fut = Self::dial(self.config.local_peer_id.clone(), peer_info.clone());
                            dialing_requests.insert(peer_info.clone(), resp_tx);
                            pending_outgoing_conns.push(fut);
                        },
                        _ => {}
                    }
                },
                complete => break,
            }
        }
        error!("connection handler task ended");

        Ok(())
    }

    async fn listen(addr: &SocketAddr) -> IoResult<impl Stream<Item = IoResult<TcpStream>>> {
        let listener: TcpListener = TcpListener::bind(addr).await.unwrap();
        let incoming = listener.incoming();
        Ok(incoming)
    }

    // dial a remote peer
    async fn dial(local_peer_id: String, peer_info: (PeerId, PeerAddr)) -> std::io::Result<Peer> {
        let (peer_id, peer_addr) = peer_info;
        let outbound_stream = TcpStream::connect(&peer_addr).await?;
        let mut framed = Framed::new(outbound_stream, LengthDelimitedCodec::default());

        let my_peer_id = local_peer_id;
        let ping = Ping(my_peer_id);
        let _ = super::utils::write_json(&mut framed, &ping).await?;

        let resp = super::utils::read_json(&mut framed).await?;
        match resp {
            Pong(remote_peer_id) => Ok(Peer::new(peer_info.clone(), OriginType::OutBound, framed)),
            _ => Err(IoError::new(ErrorKind::InvalidInput, "invalid pong")),
        }
    }

    // do some handshake work
    async fn accept_conn(local_peer_id: String, stream: TcpStream) -> std::io::Result<Peer> {
        let remote_addr = stream.peer_addr().unwrap().to_string();
        let mut framed = Framed::new(stream, LengthDelimitedCodec::default());

        let req: P2PMessage = super::utils::read_json(&mut framed).await?;
        let remote_peer_id = match req {
            Ping(remote_peer_id) => remote_peer_id,
            _ => return Err(IoError::new(ErrorKind::InvalidInput, "invalid ping"))
        };

        let my_peer_id = local_peer_id;
        let pong = Pong(my_peer_id);
        super::utils::write_json(&mut framed, &pong).await?;

        Ok(Peer::new((remote_peer_id, remote_addr), OriginType::InBound, framed))
    }

    fn handle_incoming_peer(&mut self, peer: Peer) {
        self.active_peers.insert(peer.remote_peer_id.clone(), peer);
    }

    fn handle_outgoing_peer(&mut self, peer: Peer) {
        self.active_peers.insert(peer.remote_peer_id.clone(), peer);
    }


    fn handle_outgoing_peer_result(
        &mut self,
        dial_requests: &mut HashMap<PeerInfo, oneshot::Sender<P2PResult<()>>>,
        result: (PeerInfo, IoResult<Peer>)
    ) {
        let peer_info = result.0;
        match result.1 {
            Ok(peer) => {
                self.active_peers.insert(peer.remote_peer_id.clone(), peer);
                match dial_requests.remove(&peer_info) {
                    Some(sender) => sender.send(Ok(())),
                    None => error!(target: "peer", "cannot find corresponding response channel for {}", &peer_info),
                };
            },
            Err(e) => {
                warn!("Outgoing connection handshake error {}", e);
                match dial_requests.remove(&peer_info) {
                    Some(sender) => {
                        let e = P2PError::from(e);
                        sender.send(Err(e));
                    },
                    None => error!(target: "peer", "cannot find corresponding response channel for {}", &peer_info),
                };
            }
        }
    }
}

