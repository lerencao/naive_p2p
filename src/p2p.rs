use std::{
    collections::HashMap,
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
    future::{BoxFuture},
    Stream,StreamExt,
    channel::mpsc,
    stream::FuturesUnordered,
};

use serde::{Serialize, Deserialize};
use crate::p2p::P2PMessage::{Pong, Ping};
use futures::channel::oneshot;
use crate::error::P2PError;
use crate::state::NodeState;
use failure::_core::fmt::Debug;
use crate::types:: {
    PeerId, PeerAddr, PeerInfo,
    OriginType,
};
use crate::peer::Peer;

#[derive(Clone, Default)]
pub struct NodeConfig {
    pub port: u16,
    pub addr: String,
    pub local_peer_id: PeerId,
    pub max_inbounds: u32,
    pub max_outbounds: u32,
}


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
    ProcessMessage((u32, Vec<u8>), oneshot::Sender<P2PResult<Option<(u32, Vec<u8>)>>>),
    CurState(oneshot::Sender<P2PResult<Option<(u32, Vec<u8>)>>>),
}

#[derive(Clone)]
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

    pub async fn send_message(&mut self, nonce: u32, data: Vec<u8>) -> P2PResult<Option<(u32, Vec<u8>)>> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel();
        let req = NodeRequest::ProcessMessage((nonce, data), oneshot_tx);
        self.tx.send(req).await.unwrap();
        info!("request sent, wait resp");
        oneshot_rx.await?
    }

    pub async fn cur_state(&mut self) -> P2PResult<Option<(u32, Vec<u8>)>> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel();
        let req = NodeRequest::CurState(oneshot_tx);
        self.tx.send(req).await.unwrap();
        oneshot_rx.await?
    }
}
pub struct P2PNode {
    config: NodeConfig,
    bootnodes: Vec<(PeerId, PeerAddr)>,
    active_peers: HashMap<PeerId, Peer>,
    request_rx: mpsc::Receiver<NodeRequest>,
    state: NodeState,
}


impl P2PNode {
    pub fn new(config: NodeConfig, bootnodes: Vec<(PeerId, PeerAddr)>) -> (P2PNode, NodeRequestSender) {
        let (tx, rx) = mpsc::channel(1000);
        let node = P2PNode {
            config,
            bootnodes,
            active_peers: HashMap::default(),
            request_rx: rx,
            state: NodeState::new()
        };
        let sender = NodeRequestSender {tx};
        (node, sender)
    }

    pub async fn start(&mut self) -> Result<()> {
        let addr = format!("{}:{}", self.config.addr, self.config.port);
        let sock_addr = addr.parse().unwrap();
        let mut listener = Self::listen(&sock_addr).await?.fuse();
        let mut pending_incoming_conns = FuturesUnordered::default();
        let mut pending_outgoing_conns = FuturesUnordered::default();
        let mut dialing_requests = HashMap::default();

//        for peer_info in self.bootnodes.iter() {
//            let fut = Self::dial(self.config.local_peer_id.clone(), (*peer_info).clone());
//            pending_outgoing_conns.push(fut);
//        }
        loop {
            futures::select! {
                incoming = listener.select_next_some() => {
                    self.handle_inbound(incoming, &mut pending_incoming_conns);
                },
                incoming_peer = pending_incoming_conns.select_next_some() => {
                    self.handle_inbound_peer_result(incoming_peer);
                },
                outgoing_peer = pending_outgoing_conns.select_next_some() => {
                    self.handle_outgoing_peer_result(&mut dialing_requests, outgoing_peer);
                },
                external_req = self.request_rx.select_next_some() => {
                    self.handle_external_req(external_req, &mut dialing_requests, &mut pending_outgoing_conns);
                },
                complete => break,
                default => {
                    info!("not ready");
                }
            }
        }
        error!("connection handler task ended");

        Ok(())
    }

    fn handle_inbound(
        &self,
        incoming: IoResult<TcpStream>,
        pending_incoming_conns: &mut FuturesUnordered<BoxFuture<'static, IoResult<Peer>>>,
    ) {
        match incoming {
            Ok(incoming) => {
                let fut = Self::accept_conn(self.config.local_peer_id.clone(), incoming);
                pending_incoming_conns.push(fut.boxed());
            },
            Err(e) => {
                warn!("Incoming connection error {}", e);
            },
        }
    }

    fn handle_inbound_peer_result(&mut self, incoming_peer: IoResult<Peer>) {
        match incoming_peer {
            Ok(peer) => {
                let old_peer = self.active_peers.insert(peer.peer_info().0.clone(), peer);
                match old_peer {
                    Some(peer) => {
                        warn!("already exists a {:?} conn to {:?}, drop it", peer.origin(), peer.peer_info().clone());
                        drop(peer);
                    },
                    None => {}
                }
            },
            Err(e) => warn!("Incoming connection handshake error {}", e),
        }
    }

    fn handle_external_req(
        &mut self,
        req: NodeRequest,
        dial_requests: &mut HashMap<PeerInfo, oneshot::Sender<P2PResult<()>>>,
        pending_outgoing_conns: &mut FuturesUnordered<BoxFuture<'static, (PeerInfo, IoResult<Peer>)>>
    ) {
        match req {
            NodeRequest::DialPeer(peer_info, resp_tx) => {
                // 如果有正在链接的 peer，或者 active 的 peer，那就不连接了。
                if dial_requests.contains_key(&peer_info) || self.active_peers.contains_key(&peer_info.0) {
                    let resp = Err(P2PError::AlreadyDialingOutbound(peer_info));
                    match resp_tx.send(resp) {
                        Err(_) => {
                            warn!("receiver dropped");
                        },
                        _ => {}
                    };
                } else {
                    let fut = Self::dial(self.config.local_peer_id.clone(), peer_info.clone());
                    let peer_info_clone = peer_info.clone();

                    dial_requests.insert(peer_info_clone, resp_tx);

                    let fut = fut.map(|r| (peer_info, r));
                    pending_outgoing_conns.push(fut.boxed());
                }
            },
            NodeRequest::ProcessMessage(message, resp_tx) => {
                // TODO: check duplicate message
                let cur_state = self.state.insert(message.0, message.1);
                let resp = Ok(cur_state);
                send_resp(resp_tx, resp);
            },
            NodeRequest::CurState(resp_tx) => {
                let cur_state = match self.state.cur_state() {
                    Some(s) => Some((*s.0, s.1.to_vec())),
                    None => None,
                };
                let resp = Ok(cur_state);
                send_resp(resp_tx, resp);
            }
        }
    }

    /// 处理 outbound 链接结果
    fn handle_outgoing_peer_result(
        &mut self,
        dial_requests: &mut HashMap<PeerInfo, oneshot::Sender<P2PResult<()>>>,
        result: (PeerInfo, IoResult<Peer>)
    ) {
        let peer_info = result.0;
        let resp_sender = match dial_requests.remove(&peer_info) {
            Some(sender) => {
                sender
            },
            None => {
                error!(target: "peer", "cannot find corresponding response channel for {:?}", &peer_info);
                return;
            }
        };

        let resp = match result.1 {
            Ok(peer) => {
                let old_peer = self.active_peers.insert(peer.peer_info().0.clone(), peer);
                if old_peer.is_some() {
                    warn!("drop exists peer, info: {:?}", old_peer.unwrap().peer_info());
                }
                Ok(())
            },
            Err(e) => {
                warn!("Outgoing connection handshake error {}", e);
                let e = P2PError::from(e);
                Err(e)
            }
        };
        send_resp(resp_sender, resp);
    }

    async fn listen(addr: &SocketAddr) -> IoResult<impl Stream<Item = IoResult<TcpStream>>> {
        let listener: TcpListener = TcpListener::bind(addr).await.unwrap();
        let incoming = listener.incoming();
        Ok(incoming)
    }

    // dial a remote peer
    async fn dial(local_peer_id: String, peer_info: (PeerId, PeerAddr)) -> std::io::Result<Peer> {
        let (_peer_id, peer_addr) = peer_info.clone();
        let outbound_stream = TcpStream::connect(&peer_addr).await?;
        let mut framed = Framed::new(outbound_stream, LengthDelimitedCodec::default());

        let my_peer_id = local_peer_id;
        let ping = Ping(my_peer_id);
        let _ = super::utils::write_json(&mut framed, &ping).await?;

        let resp = super::utils::read_json(&mut framed).await?;
        match resp {
            Pong(_remote_peer_id) => Ok(Peer::new(peer_info, OriginType::OutBound, framed)),
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
}

fn send_resp<T: Debug>(resp_tx: oneshot::Sender<T>, data: T) {
    match resp_tx.send(data) {
        Ok(_) => {
            info!("resp success");
        },
        Err(t) => {
            warn!("fail to send resp {:?}, client peer ended", t);
        }
    }
}