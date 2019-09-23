use std::{
    collections::HashMap,
    io::{Error as IoError, ErrorKind, Result as IoResult},
    net::SocketAddr,
};

use log::{debug, error, info, warn};
use tokio::{
    codec::Framed,
    codec::LengthDelimitedCodec,
    net::{TcpListener, TcpStream},
    prelude::*,
};

use futures::{self, channel::mpsc, future::BoxFuture, stream::FuturesUnordered, Stream};

use crate::node::P2PMessage::Pong;
use futures::channel::oneshot;

use crate::config::NodeConfig;
use crate::peer::{Peer, PeerEvent, PeerSender};
use crate::state::NodeState;
use crate::types::{OriginType, P2PMessage, P2PResult, PeerId, PeerInfo};
use futures_util::rand_reexport::thread_rng;
use rand::Rng;
use std::collections::HashSet;
use std::fmt::Debug;
use std::time::Instant;
use tokio::runtime::TaskExecutor;

// event flow from peer tasks to p2p main task
#[derive(Clone, Debug)]
pub enum InternalEvent {
    DialPeer(PeerInfo),
    SyncBlock(PeerId, u32), // known lastest index of peer id
    PeerEvent((PeerId, PeerEvent)),
}
const SYNC_MAX_BLOCK: u32 = 10;
const DISCOVER_PEER_MAX_SIZE: u32 = 5;

pub enum NodeRequest {
    SubmitBlock(
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
    pub async fn submit_block(
        &mut self,
        nonce: u32,
        data: Vec<u8>,
    ) -> P2PResult<Option<(u32, Vec<u8>)>> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel();
        let req = NodeRequest::SubmitBlock((nonce, data), oneshot_tx);
        self.tx.send(req).await?;
        oneshot_rx.await?
    }

    pub async fn cur_state(&mut self) -> P2PResult<Option<(u32, Vec<u8>)>> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel();
        let req = NodeRequest::CurState(oneshot_tx);
        self.tx.send(req).await?;
        oneshot_rx.await?
    }
}
/// TODO: separate the functionality of state manager and peer_manager
pub struct P2PNode {
    config: NodeConfig,
    active_peers: HashMap<PeerId, PeerSender>,
    dialing_requests: HashSet<PeerInfo>,
    pending_outgoing_conns:
        FuturesUnordered<BoxFuture<'static, (PeerInfo, IoResult<(Peer, PeerSender)>)>>,
    request_rx: mpsc::Receiver<NodeRequest>,
    state: NodeState,
    executor: Option<TaskExecutor>,

    // tx is used in peer tasks
    internal_event_tx: mpsc::Sender<InternalEvent>,
    // rx used in this task
    internal_event_rx: mpsc::Receiver<InternalEvent>,

    // when new peer created, use this channel to notify main task
    new_peer_notify_tx: mpsc::Sender<(Peer, PeerSender)>,
    new_peer_notify_rx: mpsc::Receiver<(Peer, PeerSender)>
}

impl P2PNode {
    pub fn new(config: NodeConfig) -> (P2PNode, NodeRequestSender) {
        let (tx, rx) = mpsc::channel(1000);
        let (internal_event_tx, internal_event_rx) = mpsc::channel(5000);
        let (new_peer_notify_tx, new_peer_notify_rx) = mpsc::channel(100);
        let node = P2PNode {
            config,
            active_peers: HashMap::default(),
            dialing_requests: HashSet::default(),
            pending_outgoing_conns: FuturesUnordered::default(),
            request_rx: rx,
            state: NodeState::new(),
            executor: None,
            internal_event_tx,
            internal_event_rx,

            new_peer_notify_tx,
            new_peer_notify_rx,
        };
        let sender = NodeRequestSender { tx };
        (node, sender)
    }

    pub async fn start(mut self, executor: TaskExecutor) {
        self.executor = Some(executor);

        info!("p2p node start with config: {:?}", self.config);

        let sock_addr = self.config.p2p_addr.parse().unwrap();

        let mut listener = match Self::listen(&sock_addr).await {
            Ok(listener) => listener.fuse(),
            Err(e) => {
                error!("fail to listen p2p port, reason: {:?}", e);
                return;
            }
        };

        for peer_info in self.config.bootnodes.iter() {
            let (peer_id, peer_addr) = peer_info;
            match self.internal_event_tx.try_send(InternalEvent::DialPeer((
                peer_id.to_string(),
                peer_addr.to_string(),
            ))) {
                Err(e) => {
                    error!(
                        "cannot send dial bootnodes requests to channel, reason: {:?}",
                        e
                    );
                }
                Ok(_) => {}
            }
        }

        let mut heartbeat_timer =
            tokio_timer::Interval::new_interval(std::time::Duration::SECOND * 2);
        let mut peer_discovery_timer =
            tokio_timer::Interval::new_interval(std::time::Duration::SECOND * 5);

        loop {
            futures::select! {
                incoming = listener.select_next_some() => {
                    self.handle_inbound(incoming);
                },
                incoming_peer = self.new_peer_notify_rx.select_next_some() => {
                    self.handle_inbound_peer(incoming_peer).await;
                },
                outgoing_peer = self.pending_outgoing_conns.select_next_some() => {
                    self.handle_outgoing_peer_result(outgoing_peer).await;
                },
                external_req = self.request_rx.select_next_some() => {
                    self.handle_external_req(external_req).await;
                },
                internal_event = self.internal_event_rx.select_next_some() => {
                    self.handle_internal_event(internal_event).await;
                },
                heartbeat_time = heartbeat_timer.select_next_some() => {
                    self.handle_heartbeat_timer(heartbeat_time);
                },
                discover_peer = peer_discovery_timer.select_next_some() => {
                    self.handle_peer_discovery_timer();
                },
                complete => break,
            }
        }
        info!("p2p node task stopped");
    }

    fn handle_inbound(
        &mut self,
        incoming: IoResult<TcpStream>,
    ) {
        match incoming {
            Ok(incoming) => {
                let local_peer_info = (
                    self.config.local_peer_id.clone(),
                    self.config.p2p_addr.clone(),
                );
                let mut peer_created_notify_tx = self.new_peer_notify_tx.clone();
                let handshake = async move {
                    let result = Self::accept_conn(local_peer_info, incoming).await;
                    match result {
                        Ok((peer, peer_sender)) => {
                            if let Err(e) = peer_created_notify_tx.send((peer, peer_sender)).await {
                                error!("fail to notify main loop, reason: {:?}", e);
                            }
                        },
                        Err(e) => {
                            warn!("Incoming connection handshake error {:?}", e);
                        }
                    }
                };
                self.executor.as_ref().unwrap().spawn(handshake);
            }
            Err(e) => {
                warn!("Incoming connection error {}", e);
            }
        }
    }

    async fn handle_inbound_peer(&mut self, incoming_peer: (Peer, PeerSender)) {

        let (peer, peer_sender) = incoming_peer;
        let peer_info = (*peer_sender.peer_info()).clone();
        info!("inbound peer {:?} connect success", &peer_info);
        let p2p_msg = P2PMessage::NewPeer((*peer_sender.peer_info()).clone());
        // 只广播 inbound peer
        self.broadcast(p2p_msg, Some(peer_sender.peer_info().0.clone()));
        self.add_peer((peer, peer_sender)).await;
    }

    async fn handle_external_req(&mut self, req: NodeRequest) {
        match req {
            NodeRequest::SubmitBlock(message, resp_tx) => {
                if self.state.contains(&message.0) {
                    let cur_state = self.state.cur_state().map(|d| (*d.0, d.1.to_vec()));
                    send_resp(resp_tx, Ok(cur_state));
                } else {
                    let cur_state = self.state.insert(message.0, message.1.clone());
                    let resp = Ok(cur_state);
                    send_resp(resp_tx, resp);
                    self.broadcast(P2PMessage::NewData(message.0, message.1.clone()), None);
                }
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
        result: (PeerInfo, IoResult<(Peer, PeerSender)>),
    ) {
        let peer_info = result.0;
        if !self.dialing_requests.remove(&peer_info) {
            error!("cannot find ongoing dial request for {:?}", &peer_info);
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
            InternalEvent::PeerEvent((peer_id, peer_event)) => {
                self.handle_peer_event(peer_id, peer_event).await;
            }
            InternalEvent::DialPeer(peer_info) => {
                // TODO: 如果 active peer 太多，需要做一些限制，具体策略？
                // 如果有正在链接的 peer，或者 active 的 peer，那就不连接了。
                if self.dialing_requests.contains(&peer_info)
                    || self.active_peers.contains_key(&peer_info.0)
                {
                    warn!("already dialing peer {:?}", &peer_info);
                } else {
                    let local_peer_info = (
                        self.config.local_peer_id.clone(),
                        self.config.p2p_addr.clone(),
                    );
                    let fut = Self::dial(local_peer_info, peer_info.clone());
                    let peer_info_clone = peer_info.clone();

                    self.dialing_requests.insert(peer_info_clone);

                    let fut = fut.map(|r| (peer_info, r));
                    self.pending_outgoing_conns.push(fut.boxed());
                }
            }
            InternalEvent::SyncBlock(peer_id, latest_index) => {
                if let Some(peer_handle) = self.active_peers.get_mut(&peer_id) {
                    let cur_state = self.state.cur_state();
                    let missing_index = match cur_state {
                        None => 0,
                        Some((index, _)) => *index + 1,
                    };
                    if missing_index <= latest_index {
                        info!(
                            "start sync block from {:?}, start_index: {:?}",
                            &peer_id, missing_index
                        );
                        if !peer_handle
                            .try_send(P2PMessage::ScanData(missing_index, SYNC_MAX_BLOCK))
                        {
                            warn!("fail to send sync-block request to peer {:?}", &peer_id);
                        }
                    }
                }
            }
        }
    }

    async fn handle_peer_event(&mut self, peer_id: PeerId, peer_event: PeerEvent) {
        match peer_event {
            PeerEvent::Heartbeated(latest_index) => {
                debug!("receive heartbeat({:?}) from {:?}", latest_index, peer_id);
                if let Some(peer_handle) = self.active_peers.get_mut(&peer_id) {
                    peer_handle.set_last_see(Instant::now());
                    if let Some(latest_index) = latest_index {
                        if self.should_sync_block(latest_index) {
                            let sync_block =
                                InternalEvent::SyncBlock(peer_id.clone(), latest_index);
                            if let Err(e) = self.internal_event_tx.try_send(sync_block) {
                                error!("fail to reqeust sync block,reason: {:?}", e);
                            }
                        }
                    }
                }
            }
            PeerEvent::NewPeerFound(peer_info) => {
                self.handle_found_peer(peer_info);
            }
            PeerEvent::NewDataReceived(from_peer, index, data) => {
                if !self.state.contains(&index) {
                    info!(
                        "found new data(index: {:?}) broadcasted from peer {:?}",
                        index, &from_peer
                    );
                    self.state.insert(index, data.clone());
                    // broadcast to other peers
                    self.broadcast(P2PMessage::NewData(index, data), Some(from_peer));
                } else {
                    info!(
                        "receive duplicate data(index: {:?}) broadcasted from peer {:?}",
                        index, &from_peer
                    );
                }
            }
            PeerEvent::SyncBlockRequest(start_index, max_size) => {
                if let Some(peer_handle) = self.active_peers.get_mut(&peer_id) {
                    let blocks = self.state.scan_block(start_index, max_size);
                    let sync_block_result = P2PMessage::ScanDataResult(blocks);
                    if !peer_handle.try_send(sync_block_result) {
                        warn!("fail to send sync-block result to peer {:?}", &peer_id);
                    }
                }
            }
            PeerEvent::SyncBlockResult(blocks) => {
                let len = blocks.len();
                let maybe_largest_index = blocks.last().map(|(i, _)| *i);
                for (index, data) in blocks.into_iter() {
                    self.state.insert(index, data);
                }
                // 如果少于 max_block，说明没有数据要同步了。
                if len >= SYNC_MAX_BLOCK as usize {
                    let largest_index = maybe_largest_index.unwrap();
                    let sync_block = InternalEvent::SyncBlock(peer_id, largest_index + 1);
                    if let Err(e) = self.internal_event_tx.try_send(sync_block) {
                        warn!("fail to give order of sync block, reason:{:?}", e)
                    }
                }
            }
            PeerEvent::DiscoverPeerRequest(_start_ts, _u32) => {
                let active_peer_ids = self
                    .active_peers
                    .keys()
                    .filter(|k| *k != &peer_id)
                    .collect::<Vec<_>>();
                if !active_peer_ids.is_empty() {
                    let peer_info = {
                        let rand_index = thread_rng().gen_range(0, active_peer_ids.len());
                        let peer_info = self
                            .active_peers
                            .get(active_peer_ids[rand_index])
                            .unwrap()
                            .peer_info();
                        (*peer_info).clone()
                    };
                    if let Some(peer_handle) = self.active_peers.get_mut(&peer_id) {
                        if peer_id != peer_info.0 {
                            let discover_peer_result = P2PMessage::DisCoverPeerResult(peer_info);
                            if !peer_handle.try_send(discover_peer_result) {
                                warn!("fail to send discover-peer result to peer {:?}", &peer_id);
                            }
                        }
                    }
                }
            }
            PeerEvent::DiscoverPeerResult(peer_info) => {
                self.handle_found_peer(peer_info);
            }
            PeerEvent::PeerDisconnected => {
                info!("remove disconnected peer: {:?}", &peer_id);
                let peer_handle = self.active_peers.remove(&peer_id);
                match peer_handle {
                    Some(handle) => {
                        drop(handle);
                    }
                    None => error!("peer already disconnected"),
                }
            }
        }
    }

    /// --------------------------Heartbeat timer --------------------------------
    fn handle_heartbeat_timer(&mut self, _time: Instant) {
        let lastest_index = self.state.lastest_block_index();
        self.broadcast(P2PMessage::Heartbeat(lastest_index), None);
    }
    fn handle_peer_discovery_timer(&mut self) {
        if self.active_peers.is_empty() {
            return;
        }
        let active_peer_ids = self.active_peers.keys().collect::<Vec<&PeerId>>();
        let random_index = rand::thread_rng().gen_range(0, active_peer_ids.len());
        let random_select_peer_id = (*active_peer_ids[random_index]).clone();
        let peer_handle = self.active_peers.get_mut(&random_select_peer_id).unwrap();

        peer_handle.discover_peer(DISCOVER_PEER_MAX_SIZE);
    }

    // 广播 p2p 消息
    fn broadcast(&mut self, msg: P2PMessage, except: Option<PeerId>) {
        for (peer_id, peer) in self.active_peers.iter_mut() {
            if except.contains(peer_id) {
                continue;
            }
            let _ = peer.try_send(msg.clone());
        }
    }

    async fn listen(addr: &SocketAddr) -> IoResult<impl Stream<Item = IoResult<TcpStream>>> {
        let listener: TcpListener = TcpListener::bind(addr).await?;
        let incoming = listener.incoming();
        Ok(incoming)
    }

    // dial a remote peer
    async fn dial(
        local_peer_info: PeerInfo,
        peer_info: PeerInfo,
    ) -> std::io::Result<(Peer, PeerSender)> {
        let (_peer_id, peer_addr) = peer_info.clone();
        let outbound_stream = TcpStream::connect(&peer_addr).await?;
        let mut framed = Framed::new(outbound_stream, LengthDelimitedCodec::default()).fuse();

        let ping = P2PMessage::Ping(local_peer_info.clone());
        let _ = super::utils::write_json(&mut framed, &ping).await?;

        let resp = super::utils::read_json(&mut framed).await?;
        match resp {
            Pong(remote_peer_info) => {
                if remote_peer_info == peer_info {
                    Ok(Peer::new(peer_info, OriginType::OutBound, framed))
                } else {
                    Err(IoError::new(ErrorKind::InvalidData, "peer_info mismatched"))
                }
            }
            _ => Err(IoError::new(ErrorKind::InvalidInput, "invalid pong")),
        }
    }

    // do some handshake work
    async fn accept_conn(
        local_peer_info: PeerInfo,
        stream: TcpStream,
    ) -> std::io::Result<(Peer, PeerSender)> {
        let mut framed = Framed::new(stream, LengthDelimitedCodec::default()).fuse();

        let req: P2PMessage = super::utils::read_json(&mut framed).await?;
        let remote_peer_info = match req {
            P2PMessage::Ping(remote_peer_info) => remote_peer_info,
            _ => return Err(IoError::new(ErrorKind::InvalidInput, "invalid ping")),
        };

        let pong = P2PMessage::Pong(local_peer_info);
        super::utils::write_json(&mut framed, &pong).await?;

        Ok(Peer::new(remote_peer_info, OriginType::InBound, framed))
    }

    /// ----------- helper functions -----------------


    fn handle_found_peer(&mut self, peer_info: PeerInfo) {
        if !self.active_peers.contains_key(&peer_info.0) {
            info!("found new peer {:?}", &peer_info);
            if let Err(e) = self
                .internal_event_tx
                .try_send(InternalEvent::DialPeer(peer_info.clone()))
            {
                error!("fail to request dial-peer, reason: {:?}", e);
            }
        }
    }


    fn should_sync_block(&self, newest_index: u32) -> bool {
        let cur_state = self.state.cur_state();
        let missing_index = match cur_state {
            None => 0,
            Some((index, _)) => *index + 1,
        };
        missing_index <= newest_index
    }
}

fn send_resp<T: Debug>(resp_tx: oneshot::Sender<T>, data: T) {
    match resp_tx.send(data) {
        Ok(_) => {}
        Err(t) => {
            warn!("fail to send resp {:?}, client peer ended", t);
        }
    }
}
