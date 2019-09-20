use super::types::PeerInfo;
use crate::types::GenericError;
use failure::Fail;
use futures::channel::oneshot;
use futures_channel::mpsc::SendError;

#[derive(Debug, Fail)]
pub enum P2PError {
    #[fail(display = "IO error: {}", _0)]
    IoError(#[fail(cause)] ::std::io::Error),
    #[fail(display = "Sending end of oneshot dropped")]
    OneshotSenderDropped,
    #[fail(display = "already dialing outbound: {:?}", _0)]
    AlreadyDialingOutbound(PeerInfo),
    #[fail(display = "channel error: {:?}", _0)]
    ChannelError(GenericError),
}

impl From<::std::io::Error> for P2PError {
    fn from(error: ::std::io::Error) -> Self {
        P2PError::IoError(error)
    }
}

impl From<oneshot::Canceled> for P2PError {
    fn from(_: oneshot::Canceled) -> Self {
        P2PError::OneshotSenderDropped
    }
}

impl From<SendError> for P2PError {
    fn from(err: SendError) -> Self {
        P2PError::ChannelError(Box::new(err))
    }
}
