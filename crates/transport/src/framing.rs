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

/// Splits a stream of length-prefixed messages that arrives in arbitrary pieces, so a reader can
/// take everything already received in one go (and, say, merge a backlog of mouse moves).
pub struct FrameBuffer {
    buf: Vec<u8>,
    start: usize,
    max_len: usize,
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::with_limit(protocol::MAX_CONTROL_MSG_LEN)
    }
}

impl FrameBuffer {
    /// Messages longer than `max_len` are an error (a corrupt or hostile stream).
    pub fn with_limit(max_len: usize) -> Self {
        Self { buf: Vec::new(), start: 0, max_len }
    }

    pub fn extend(&mut self, data: &[u8]) {
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// The next complete message, or `Ok(None)` until more data arrives.
    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let pending = &self.buf[self.start..];
        let Some(len) = pending.get(..4) else { return Ok(None) };
        let len = u32::from_le_bytes(len.try_into().expect("4 bytes")) as usize;
        if len > self.max_len {
            bail!("message too large: {len} bytes");
        }
        let Some(body) = pending.get(4..4 + len) else { return Ok(None) };
        let msg = protocol::decode(body)?;
        self.start += 4 + len;
        if self.start > 64 * 1024 && self.start * 2 > self.buf.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::InputMsg;

    #[test]
    fn splits_messages_across_arbitrary_pieces() {
        let msgs: Vec<InputMsg> = (0..500u16)
            .map(|i| match i % 3 {
                0 => InputMsg::MouseMove { x: i, y: i * 2 },
                1 => InputMsg::Key { code: i % 128, down: i % 2 == 0, repeat: false },
                _ => InputMsg::ReleaseAll,
            })
            .collect();
        let bytes: Vec<u8> = msgs.iter().flat_map(|m| protocol::encode_framed(m).unwrap()).collect();
        for piece in [1, 3, 7, 64, 4096] {
            let mut fb = FrameBuffer::default();
            let mut got = Vec::new();
            for chunk in bytes.chunks(piece) {
                fb.extend(chunk);
                while let Some(m) = fb.next::<InputMsg>().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, msgs, "piece size {piece}");
        }
    }

    #[test]
    fn rejects_garbage_lengths() {
        let mut fb = FrameBuffer::default();
        fb.extend(&u32::MAX.to_le_bytes());
        assert!(fb.next::<InputMsg>().is_err());
        let mut fb = FrameBuffer::with_limit(protocol::MAX_INPUT_MSG_LEN);
        fb.extend(&1000u32.to_le_bytes());
        assert!(fb.next::<InputMsg>().is_err(), "over the input limit");
    }
}
