//! Length-prefixed postcard messages on QUIC streams.

use anyhow::{Context, Result, bail};
use quinn::{RecvStream, SendStream};
use serde::{Serialize, de::DeserializeOwned};

pub async fn write_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let bytes = protocol::encode_framed(msg)?;
    send.write_all(&bytes).await.context("write control message")?;
    Ok(())
}

/// Reads one message. Returns `Ok(None)` when the peer finished the stream cleanly.
pub async fn read_msg<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e).context("read control message length"),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > protocol::MAX_CONTROL_MSG_LEN {
        bail!("control message too large: {len} bytes");
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.context("read control message body")?;
    Ok(Some(protocol::decode(&body)?))
}
