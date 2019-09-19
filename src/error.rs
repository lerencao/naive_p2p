use super::types::PeerInfo;
use failure::Fail;
use futures::channel::oneshot;

#[derive(Debug, Fail)]
pub enum P2PError {
    #[fail(display = "IO error: {}", _0)]
    IoError(#[fail(cause)] ::std::io::Error),
    #[fail(display = "Sending end of oneshot dropped")]
    OneshotSenderDropped,
    #[fail(display = "already dialing outbound: {:?}", _0)]
    AlreadyDialingOutbound(PeerInfo),
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
