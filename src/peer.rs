use super::types::{OriginType, PeerInfo};
use crate::node::InternalEvent;
use crate::types::{P2PMessage, P2PResult, PeerId};
use bytes::{Bytes, BytesMut};
use core::ops::{Deref, DerefMut};
use futures::channel::mpsc;
use futures::stream::Fuse;
use futures::{Sink, SinkExt, Stream, StreamExt};
use futures_core::FusedStream;
use log::{debug, error, info, warn};
use std::io::Error as IoError;
use std::result::Result as StdResult;
use std::time::Instant;
use tokio::codec::{Framed, LengthDelimitedCodec};
use tokio::net::TcpStream;
use std::fmt::{Formatter, Error};

/// request sent to peer
pub enum PeerRequest {
    DiscoverPeer(u32),
    Disconnect,
    Message(P2PMessage),
}

/// event sent to main loop
#[derive(Clone, Debug)]
pub enum PeerEvent {
    NewPeerFound(PeerInfo),
    NewDataReceived(PeerId, u32, Vec<u8>), // (from, index, data)
    Heartbeated(Option<u32>),
    SyncBlockRequest(u32, u32),
    SyncBlockResult(Vec<(u32, Vec<u8>)>),

    DiscoverPeerRequest(Option<u64>, u32),
    DiscoverPeerResult(PeerInfo),
    PeerDisconnected,
}

pub struct PeerSender {
    peer_info: PeerInfo,
    origin: OriginType,
    msg_tx: mpsc::Sender<PeerRequest>,
    is_shutting_down: bool,
    last_see: Instant,
}

impl PeerSender {
    pub async fn send(&mut self, msg: P2PMessage) -> P2PResult<()> {
        self.msg_tx.send(PeerRequest::Message(msg)).await?;
        Ok(())
    }

    pub fn try_send(&mut self, msg: P2PMessage) -> bool {
        match self.msg_tx.try_send(PeerRequest::Message(msg)) {
            Ok(_) => true,
            Err(e) => {
                error!(
                    "fail to send p2p message to peer({:?}) task, reason : {:?}",
                    self.peer_info(),
                    e
                );
                false
            }
        }
    }

    pub fn discover_peer(&mut self, max_size: u32) {
        if let Err(e) = self.msg_tx.try_send(PeerRequest::DiscoverPeer(max_size)) {
            error!(
                "fail to give order to peer({:?}) task, reason : {:?}",
                self.peer_info(),
                e
            );
        }
    }

    pub async fn disconnect(&mut self) {
        if self.msg_tx.send(PeerRequest::Disconnect).await.is_err() {
            error!("Peer {:?} has already been shutdown.", self.peer_info);
        }
        self.is_shutting_down = true;
    }

    pub fn peer_info(&self) -> &PeerInfo {
        &self.peer_info
    }
    pub fn origin(&self) -> OriginType {
        self.origin
    }

    pub fn set_last_see(&mut self, instant: Instant) {
        self.last_see = instant;
    }
}

impl std::fmt::Debug for PeerSender {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        write!(f, "{:?} {:?}, closed: {:?}", self.origin, self.peer_info, self.is_shutting_down)
    }
}

pub struct ConnectionPeer<S> {
    peer_info: PeerInfo,
    origin: OriginType,
    msg_rx: mpsc::Receiver<PeerRequest>,
    internal_msg_tx: Option<mpsc::Sender<InternalEvent>>,
    conn: S,
    closed: bool,
    peer_discovery_last_time: Option<u64>,
}

impl<S> ConnectionPeer<S>
where
    S: FusedStream<Item = StdResult<BytesMut, IoError>> + Sink<Bytes, Error = IoError> + Unpin,
{
    pub fn new(
        peer_info: PeerInfo,
        origin: OriginType,
        conn: S,
    ) -> (ConnectionPeer<S>, PeerSender) {
        let (tx, rx) = mpsc::channel(100);
        let peer_client = PeerSender {
            peer_info: peer_info.clone(),
            origin: origin.clone(),
            msg_tx: tx,
            is_shutting_down: false,
            last_see: Instant::now(),
        };
        let peer = ConnectionPeer {
            peer_info,
            origin,
            conn,
            msg_rx: rx,
            closed: false,
            internal_msg_tx: None,
            peer_discovery_last_time: None,
        };
        (peer, peer_client)
    }

    pub async fn start(mut self, internal_event_tx: mpsc::Sender<InternalEvent>) {
        self.internal_msg_tx = Some(internal_event_tx);
        info!(
            "new {:?} peer task started, peer_info: {:?}",
            self.origin, self.peer_info
        );
        loop {
            futures::select! {
                external_req = self.msg_rx.select_next_some() => {
                    self.handle_external_req(external_req).await;
                }
                bytes = self.conn.next() => {
                    self.handle_data(bytes).await;
                }
                complete => break,
            }

            if self.closed {
                break;
            }
        }

        debug!("{:?} peer {:?} task stopped", self.origin, self.peer_info);
    }

    async fn handle_external_req(&mut self, req: PeerRequest) {
        match req {
            PeerRequest::Message(msg) => {
                match super::utils::write_json(&mut self.conn, &msg).await {
                    Ok(_) => {}
                    Err(e) => {
                        error!(
                            "cannot send p2p message to peer {:?}, reason: {:?}, close conn now",
                            &self.peer_info, e
                        );
                        self.close_conn().await;
                    }
                }
            }
            PeerRequest::DiscoverPeer(max_size) => {
                let last_discovery = self.peer_discovery_last_time;
                let p2p_msg = P2PMessage::DiscoverPeer(last_discovery, max_size);
                if let Err(e) = super::utils::write_json(&mut self.conn, &p2p_msg).await {
                    error!(
                        "cannot send p2p message to peer {:?}, reason: {:?}, close conn now",
                        &self.peer_info, e
                    );
                    self.close_conn().await;
                }
            }
            PeerRequest::Disconnect => {
                self.close_conn().await;
            }
        }
    }

    async fn handle_data(&mut self, data: Option<<S as Stream>::Item>) {
        match data {
            None => {
                self.close_conn().await;
            }
            Some(Ok(bytes)) => {
                match serde_json::from_slice(&bytes.freeze()) {
                    Ok(msg) => {
                        //                        self.internal_msg_tx.send();
                        match msg {
                            P2PMessage::NewData(index, data) => {
                                self.send_peer_event(PeerEvent::NewDataReceived(
                                    self.peer_info.0.clone(),
                                    index,
                                    data,
                                ))
                                .await;
                            }
                            P2PMessage::NewPeer(peer_info) => {
                                self.send_peer_event(PeerEvent::NewPeerFound(peer_info))
                                    .await;
                            }
                            P2PMessage::Heartbeat(lastest_index) => {
                                self.send_peer_event(PeerEvent::Heartbeated(lastest_index))
                                    .await;
                            }
                            P2PMessage::ScanData(start_index, max_length) => {
                                self.send_peer_event(PeerEvent::SyncBlockRequest(
                                    start_index,
                                    max_length,
                                ))
                                .await;
                            }
                            P2PMessage::ScanDataResult(blocks) => {
                                self.send_peer_event(PeerEvent::SyncBlockResult(blocks))
                                    .await;
                            }
                            P2PMessage::DiscoverPeer(start_ts, max_size) => {
                                self.send_peer_event(PeerEvent::DiscoverPeerRequest(
                                    start_ts, max_size,
                                ))
                                .await;
                            }
                            P2PMessage::DisCoverPeerResult(peer_info) => {
                                let unix_ts =
                                    std::time::UNIX_EPOCH.elapsed().ok().unwrap().as_millis()
                                        as u64;
                                self.peer_discovery_last_time = Some(unix_ts);
                                self.send_peer_event(PeerEvent::DiscoverPeerResult(peer_info))
                                    .await;
                            }
                            _ => {}
                        }
                    }
                    Err(e) => {
                        warn!("fail to decode data to p2p message, reason: {:?}", e);
                        self.close_conn().await;
                    }
                }
            }
            Some(Err(e)) => {
                warn!("read peer data err, reason: {:?}", e);
                self.close_conn().await;
            }
        }
    }

    async fn send_internal_event(&mut self, event: InternalEvent) {
        match self
            .internal_msg_tx
            .as_mut()
            .unwrap()
            .send(event.clone())
            .await
        {
            Ok(_) => {}
            Err(_e) => {
                error!(
                    "{:?} fail to send internal event {:?}",
                    &self.peer_info, event
                );
            }
        }
    }

    async fn send_peer_event(&mut self, event: PeerEvent) {
        let peer_id = self.peer_info.0.clone();
        self.send_internal_event(InternalEvent::PeerEvent((peer_id, event)))
            .await;
    }

    async fn close_conn(&mut self) {
        match self.conn.close().await {
            Ok(_) => {
                info!(
                    "close conn to {:?} {:?} successfully",
                    self.origin, self.peer_info
                );
            }
            Err(e) => {
                warn!(
                    "cannot close conn to  {:?} {:?} gracefully, reason: {:?}",
                    self.origin, self.peer_info, e
                );
            }
        }
        self.closed = true;
        // TODO: send event to main loop
        self.send_peer_event(PeerEvent::PeerDisconnected).await;
    }

    pub fn peer_info(&self) -> &PeerInfo {
        &self.peer_info
    }
    pub fn origin(&self) -> OriginType {
        self.origin
    }
}

impl<S> Deref for ConnectionPeer<S> {
    type Target = S;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl<S> DerefMut for ConnectionPeer<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

impl <S> std::fmt::Debug for ConnectionPeer<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        write!(f, "{:?} {:?}, closed: {:?}", self.origin, self.peer_info, self.closed)
    }
}

pub type Peer = ConnectionPeer<Fuse<Framed<TcpStream, LengthDelimitedCodec>>>;
