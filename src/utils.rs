use serde::Serialize;

use bytes::{Bytes, BytesMut};
use futures::{
    sink::{Sink, SinkExt},
    stream::{Stream, StreamExt},
};

use serde::de::DeserializeOwned;
use std::io::{self, Error as IoError};

pub async fn read_json<S, T>(stream: &mut S) -> Result<T, IoError>
where
    S: Stream<Item = Result<BytesMut, IoError>> + Unpin,
    T: DeserializeOwned,
{
    let data: Bytes = match stream.next().await {
        Some(data) => data.map(|d| d.freeze()),
        None => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
    }?;
    let msg = serde_json::from_slice(&data)?;
    Ok(msg)
}

pub async fn write_json<S, T>(sink: &mut S, data: &T) -> Result<(), IoError>
where
    S: Sink<Bytes, Error = IoError> + Unpin,
    T: Serialize,
{
    let to_send = bytes::Bytes::from(serde_json::to_string(data)?);
    return sink.send(to_send).await;
}
