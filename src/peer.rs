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
use tokio::codec::{Framed, LengthDelimitedCodec};
use tokio::net::TcpStream;
use std::time::Instant;
use futures_channel::mpsc::TrySendError;

pub enum PeerRequest {
    // cast
    Disconnect,
    Message(P2PMessage),
}

#[derive(Clone, Debug)]
pub enum PeerEvent {
    NewPeerFound(PeerInfo),
    NewDataReceived(PeerId, u32, Vec<u8>), // (from, index, data)
    Heartbeated(Option<u32>),
    SyncBlockRequest(u32, u32),
    SyncBlockResult(Vec<(u32, Vec<u8>)>),

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
                error!("fail to send p2p message to peer({:?}) task, reason : {:?}", self.peer_info(), e);
                false
            }
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

pub struct ConnectionPeer<S> {
    peer_info: PeerInfo,
    origin: OriginType,
    msg_rx: mpsc::Receiver<PeerRequest>,
    internal_msg_tx: Option<mpsc::Sender<InternalEvent>>,
    conn: S,
    closed: bool,
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
            last_see: Instant::now()
        };
        let peer = ConnectionPeer {
            peer_info,
            origin,
            conn,
            msg_rx: rx,
            closed: false,
            internal_msg_tx: None,
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
                                self.send_peer_event(
                                    PeerEvent::NewDataReceived(self.peer_info.0.clone(), index, data)
                                ).await;
                            }
                            P2PMessage::NewPeer(peer_info) => {
                                self.send_peer_event(PeerEvent::NewPeerFound(peer_info))
                                    .await;
                            }
                            P2PMessage::Heartbeat(lastest_index) => {
                                self.send_peer_event(PeerEvent::Heartbeated(lastest_index)).await;
                            }
                            P2PMessage::ScanData(start_index, max_length) => {
                                self.send_peer_event(PeerEvent::SyncBlockRequest(start_index, max_length)).await;
                            }
                            P2PMessage::ScanDataResult(blocks) => {
                                self.send_peer_event(PeerEvent::SyncBlockResult(blocks)).await;
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
                error!("{:?} fail to send internal event {:?}", &self.peer_info, event);
            }
        }
    }

    async fn send_peer_event(&mut self, event: PeerEvent) {
        let peer_id = self.peer_info.0.clone();
        self.send_internal_event(InternalEvent::PeerEvent((peer_id, event))).await;
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
        self.send_peer_event(PeerEvent::PeerDisconnected)
            .await;
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

pub type Peer = ConnectionPeer<Fuse<Framed<TcpStream, LengthDelimitedCodec>>>;
