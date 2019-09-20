use std::{
    collections::HashMap,
    io::{Error as IoError, ErrorKind, Result as IoResult},
    net::SocketAddr,
};

use log::{error, info, warn};
use tokio::{
    codec::Framed,
    codec::LengthDelimitedCodec,
    net::{TcpListener, TcpStream},
    prelude::*,
};

use futures::{self, channel::mpsc, future::BoxFuture, stream::FuturesUnordered, Stream};

use crate::node::P2PMessage::{Ping, Pong};
use futures::channel::oneshot;

use crate::config::NodeConfig;
use crate::peer::{Peer, PeerSender};
use crate::state::NodeState;
use crate::types::{OriginType, P2PMessage, P2PResult, PeerAddr, PeerId, PeerInfo};
use std::collections::HashSet;
use std::fmt::Debug;
use tokio::runtime::TaskExecutor;

// event flow from peer tasks to p2p main task
#[derive(Clone, Debug)]
pub enum InternalEvent {
    NewPeerFound(PeerInfo),
    NewDataReceived(u32, Vec<u8>),
    PeerDisconnected(PeerInfo),
}

pub enum NodeRequest {
    DialPeer(PeerInfo),
    ProcessMessage(
        (u32, Vec<u8>),
        oneshot::Sender<P2PResult<Option<(u32, Vec<u8>)>>>,
    ),
    CurState(oneshot::Sender<P2PResult<Option<(u32, Vec<u8>)>>>),
}

#[derive(Clone)]
pub struct NodeRequestSender {
    tx: mpsc::Sender<NodeRequest>,
}

impl NodeRequestSender {
    //    pub async fn dial(&mut self, peer_id: PeerId, peer_addr: PeerAddr) -> P2PResult<()> {
    //        let (oneshot_tx, oneshot_rx) = oneshot::channel();
    //        let req = NodeRequest::DialPeer((peer_id, peer_addr), oneshot_tx);
    //        self.tx.send(req).await?;
    //        oneshot_rx.await?
    //    }

    pub async fn send_message(
        &mut self,
        nonce: u32,
        data: Vec<u8>,
    ) -> P2PResult<Option<(u32, Vec<u8>)>> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel();
        let req = NodeRequest::ProcessMessage((nonce, data), oneshot_tx);
        self.tx.send(req).await?;
        info!("request sent, wait resp");
        oneshot_rx.await?
    }

    pub async fn cur_state(&mut self) -> P2PResult<Option<(u32, Vec<u8>)>> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel();
        let req = NodeRequest::CurState(oneshot_tx);
        self.tx.send(req).await?;
        oneshot_rx.await?
    }
}
pub struct P2PNode {
    config: NodeConfig,
    active_peers: HashMap<PeerId, PeerSender>,
    request_rx: mpsc::Receiver<NodeRequest>,
    request_tx: mpsc::Sender<NodeRequest>,
    state: NodeState,
    executor: Option<TaskExecutor>,

    // tx is used in peer tasks
    internal_event_tx: mpsc::Sender<InternalEvent>,
    // rx used in this task
    internal_event_rx: mpsc::Receiver<InternalEvent>,
}

impl P2PNode {
    pub fn new(config: NodeConfig) -> (P2PNode, NodeRequestSender) {
        let (tx, rx) = mpsc::channel(1000);
        let (internal_event_tx, internal_event_rx) = mpsc::channel(5000);
        let node = P2PNode {
            config,
            active_peers: HashMap::default(),
            request_tx: tx.clone(),
            request_rx: rx,
            state: NodeState::new(),
            executor: None,
            internal_event_tx,
            internal_event_rx,
        };
        let sender = NodeRequestSender { tx };
        (node, sender)
    }

    pub async fn start(&mut self, executor: TaskExecutor) {
        self.executor = Some(executor);

        info!("p2p node start with config: {:?}", self.config);

        let sock_addr = self.config.p2p_addr.parse().unwrap();

        let mut listener = match Self::listen(&sock_addr).await {
            Ok(listener) => listener.fuse(),
            Err(e) => {
                error!(target: "node", "fail to listen p2p port, reason: {:?}", e);
                return;
            }
        };

        let mut pending_incoming_conns = FuturesUnordered::default();
        let mut pending_outgoing_conns = FuturesUnordered::default();
        let mut dialing_requests = HashSet::default();

        for peer_info in self.config.bootnodes.iter() {
            let (peer_id, peer_addr) = peer_info;
            match self.request_tx.try_send(NodeRequest::DialPeer((
                peer_id.to_string(),
                peer_addr.to_string(),
            ))) {
                Err(e) => {
                    error!(target: "node", "cannot send dial bootnodes requests to channel, reason: {:?}", e);
                }
                Ok(_) => {}
            }
        }

        loop {
            futures::select! {
                incoming = listener.select_next_some() => {
                    self.handle_inbound(incoming, &mut pending_incoming_conns);
                },
                incoming_peer = pending_incoming_conns.select_next_some() => {
                    self.handle_inbound_peer_result(incoming_peer).await;
                },
                outgoing_peer = pending_outgoing_conns.select_next_some() => {
                    self.handle_outgoing_peer_result(&mut dialing_requests, outgoing_peer).await;
                },
                external_req = self.request_rx.select_next_some() => {
                    self.handle_external_req(external_req, &mut dialing_requests, &mut pending_outgoing_conns).await;
                },
                internal_event = self.internal_event_rx.select_next_some() => {
                    self.handle_internal_event(internal_event).await;
                },
                complete => break,
            }
        }
        info!(target: "node", "p2p node task stopped");
    }

    fn handle_inbound(
        &self,
        incoming: IoResult<TcpStream>,
        pending_incoming_conns: &mut FuturesUnordered<
            BoxFuture<'static, IoResult<(Peer, PeerSender)>>,
        >,
    ) {
        match incoming {
            Ok(incoming) => {
                let fut = Self::accept_conn(self.config.local_peer_id.clone(), incoming);
                pending_incoming_conns.push(fut.boxed());
            }
            Err(e) => {
                warn!("Incoming connection error {}", e);
            }
        }
    }

    async fn handle_inbound_peer_result(&mut self, incoming_peer: IoResult<(Peer, PeerSender)>) {
        match incoming_peer {
            Ok((peer, peer_sender)) => {
                self.add_peer((peer, peer_sender)).await;
                // TODO: broadcast peer
            }
            Err(e) => warn!("Incoming connection handshake error {}", e),
        }
    }

    async fn handle_external_req(
        &mut self,
        req: NodeRequest,
        dialing_requests: &mut HashSet<PeerInfo>,
        pending_outgoing_conns: &mut FuturesUnordered<
            BoxFuture<'static, (PeerInfo, IoResult<(Peer, PeerSender)>)>,
        >,
    ) {
        match req {
            NodeRequest::DialPeer(peer_info) => {
                // 如果有正在链接的 peer，或者 active 的 peer，那就不连接了。
                if dialing_requests.contains(&peer_info)
                    || self.active_peers.contains_key(&peer_info.0)
                {
                    warn!("already dialing peer {:?}", &peer_info);
                } else {
                    let fut = Self::dial(self.config.local_peer_id.clone(), peer_info.clone());
                    let peer_info_clone = peer_info.clone();

                    dialing_requests.insert(peer_info_clone);

                    let fut = fut.map(|r| (peer_info, r));
                    pending_outgoing_conns.push(fut.boxed());
                }
            }
            NodeRequest::ProcessMessage(message, resp_tx) => {
                // TODO: check duplicate message
                let cur_state = self.state.insert(message.0, message.1.clone());
                let resp = Ok(cur_state);
                send_resp(resp_tx, resp);
                // TODO: broadcast data
                self.broadcast(P2PMessage::NewData(message.0, message.1.clone()))
                    .await;
            }
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
    async fn handle_outgoing_peer_result(
        &mut self,
        dialing_requests: &mut HashSet<PeerInfo>,
        result: (PeerInfo, IoResult<(Peer, PeerSender)>),
    ) {
        let peer_info = result.0;
        if !dialing_requests.remove(&peer_info) {
            error!(target: "peer", "cannot find ongoing dial request for {:?}", &peer_info);
        }

        match result.1 {
            Ok((peer, peer_sender)) => {
                self.add_peer((peer, peer_sender)).await;
            }
            Err(e) => {
                warn!("Outgoing connection handshake error {}", e);
            }
        };
    }

    async fn add_peer(&mut self, peer: (Peer, PeerSender)) {
        let (peer, peer_sender) = peer;
        let old_peer_sender = self
            .active_peers
            .insert(peer.peer_info().0.clone(), peer_sender);
        match old_peer_sender {
            Some(mut old_peer_sender) => {
                warn!(
                    "already exists a {:?} conn to {:?}, drop it",
                    old_peer_sender.origin(),
                    old_peer_sender.peer_info().clone()
                );
                old_peer_sender.disconnect().await;
                drop(old_peer_sender);
            }
            None => {}
        }
        self.executor
            .as_ref()
            .unwrap()
            .spawn(peer.start(self.internal_event_tx.clone()));
    }

    ///--------------------------Internal Events Handling------------------------------------------------

    async fn handle_internal_event(&mut self, event: InternalEvent) {
        match event {
            InternalEvent::NewDataReceived(index, data) => {
                self.state.insert(index, data);
                // broadcast to other peers
            }
            InternalEvent::NewPeerFound(peer_info) => {
                if self.active_peers.contains_key(&peer_info.0) {
                    // skip known peer
                } else {
                    match self.request_tx.try_send(NodeRequest::DialPeer(peer_info)) {
                        Err(e) => {
                            error!("fail to send node request, reason: {:?}", e);
                        }
                        Ok(_) => {}
                    }
                }
            }
            InternalEvent::PeerDisconnected(peer_info) => {
                let peer_handle = self.active_peers.remove(&peer_info.0);
                match peer_handle {
                    Some(handle) => {
                        drop(handle);
                    }
                    None => error!("peer already disconnected"),
                }
            }
        }
    }

    // 广播 p2p 消息
    async fn broadcast(&mut self, msg: P2PMessage) {
        let mut broadcast_futs = FuturesUnordered::default();
        for (peer_id, peer) in self.active_peers.iter_mut() {
            let peer_id_clone = peer_id.to_string();
            broadcast_futs.push(peer.send(msg.clone()).map(|r| (peer_id_clone, r)).boxed());
        }

        broadcast_futs
            .for_each_concurrent(None, |(peer_id, r)| {
                async move {
                    match r {
                        Ok(_) => {}
                        Err(e) => {
                            error!(
                                "fail to send p2p message to peer {:?} channel, reason: {:?}",
                                peer_id, e
                            );
                        }
                    }
                }
            })
            .await;
    }

    async fn listen(addr: &SocketAddr) -> IoResult<impl Stream<Item = IoResult<TcpStream>>> {
        let listener: TcpListener = TcpListener::bind(addr).await?;
        let incoming = listener.incoming();
        Ok(incoming)
    }

    // dial a remote peer
    async fn dial(
        local_peer_id: String,
        peer_info: (PeerId, PeerAddr),
    ) -> std::io::Result<(Peer, PeerSender)> {
        let (_peer_id, peer_addr) = peer_info.clone();
        let outbound_stream = TcpStream::connect(&peer_addr).await?;
        let mut framed = Framed::new(outbound_stream, LengthDelimitedCodec::default()).fuse();

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
    async fn accept_conn(
        local_peer_id: String,
        stream: TcpStream,
    ) -> std::io::Result<(Peer, PeerSender)> {
        let remote_addr = stream.peer_addr().unwrap().to_string();
        let mut framed = Framed::new(stream, LengthDelimitedCodec::default()).fuse();

        let req: P2PMessage = super::utils::read_json(&mut framed).await?;
        let remote_peer_id = match req {
            Ping(remote_peer_id) => remote_peer_id,
            _ => return Err(IoError::new(ErrorKind::InvalidInput, "invalid ping")),
        };

        let my_peer_id = local_peer_id;
        let pong = Pong(my_peer_id);
        super::utils::write_json(&mut framed, &pong).await?;

        Ok(Peer::new(
            (remote_peer_id, remote_addr),
            OriginType::InBound,
            framed,
        ))
    }
}

fn send_resp<T: Debug>(resp_tx: oneshot::Sender<T>, data: T) {
    match resp_tx.send(data) {
        Ok(_) => {
            info!("resp success");
        }
        Err(t) => {
            warn!("fail to send resp {:?}, client peer ended", t);
        }
    }
}
