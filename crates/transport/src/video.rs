//! Splits serialized video frames into datagrams and puts them back together.
//!
//! There is no retransmission: a frame that misses a packet is dropped, and the client asks the
//! host for a keyframe (later milestones add FEC and reference-frame recovery).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use bytes::Bytes;
use protocol::VideoPacketHeader;

/// Frames still being assembled at once; older partial frames are abandoned beyond this.
const MAX_PARTIAL_FRAMES: usize = 8;

#[derive(Debug, Default)]
pub struct Packetizer {
    next_frame_id: u32,
}

impl Packetizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Splits `frame` into datagrams no larger than `max_datagram` bytes.
    pub fn packetize(&mut self, frame: &[u8], max_datagram: usize) -> Result<Vec<Bytes>> {
        let payload_max = max_datagram.saturating_sub(VideoPacketHeader::LEN);
        if payload_max < 64 {
            bail!("datagram size {max_datagram} too small");
        }
        let total_len = u32::try_from(frame.len())?;
        let count = frame.len().div_ceil(payload_max).max(1);
        let count = u16::try_from(count).map_err(|_| anyhow::anyhow!("frame too large: {} bytes", frame.len()))?;
        let chunk = VideoPacketHeader::chunk_len(total_len, count);

        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            let start = (index as usize * chunk).min(frame.len());
            let end = (start + chunk).min(frame.len());
            let mut buf = Vec::with_capacity(VideoPacketHeader::LEN + end - start);
            VideoPacketHeader { frame_id, index, count, total_len }.write(&mut buf);
            buf.extend_from_slice(&frame[start..end]);
            out.push(Bytes::from(buf));
        }
        Ok(out)
    }
}

/// A completely received frame.
#[derive(Debug, PartialEq, Eq)]
pub struct Assembled {
    pub frame_id: u32,
    pub data: Vec<u8>,
    /// Frames between the previous delivered frame and this one that never completed.
    pub skipped: u32,
}

struct Partial {
    buf: Vec<u8>,
    received: Vec<bool>,
    remaining: u16,
    total_len: u32,
    count: u16,
    first_seen: Instant,
}

#[derive(Default)]
pub struct Reassembler {
    partial: BTreeMap<u32, Partial>,
    last_delivered: Option<u32>,
}

/// Wrapping-aware "a comes after b".
fn is_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one datagram; returns a frame once all of its packets have arrived.
    pub fn push(&mut self, datagram: &[u8]) -> Option<Assembled> {
        let (h, payload) = VideoPacketHeader::parse(datagram)?;
        if let Some(last) = self.last_delivered
            && !is_newer(h.frame_id, last)
        {
            return None; // late packet for a frame already delivered or abandoned
        }

        let partial = self.partial.entry(h.frame_id).or_insert_with(|| Partial {
            buf: vec![0; h.total_len as usize],
            received: vec![false; h.count as usize],
            remaining: h.count,
            total_len: h.total_len,
            count: h.count,
            first_seen: Instant::now(),
        });
        if partial.total_len != h.total_len || partial.count != h.count {
            return None;
        }
        let chunk = VideoPacketHeader::chunk_len(h.total_len, h.count);
        let start = h.index as usize * chunk;
        let end = (start + chunk).min(h.total_len as usize);
        if start > end || payload.len() != end - start || partial.received[h.index as usize] {
            return None;
        }
        partial.buf[start..end].copy_from_slice(payload);
        partial.received[h.index as usize] = true;
        partial.remaining -= 1;

        if partial.remaining > 0 {
            if self.partial.len() > MAX_PARTIAL_FRAMES {
                self.partial.pop_first();
            }
            return None;
        }

        let done = self.partial.remove(&h.frame_id)?;
        let skipped = match self.last_delivered {
            Some(last) => h.frame_id.wrapping_sub(last).wrapping_sub(1),
            None => 0,
        };
        // Anything older than this frame can no longer be shown.
        self.partial.retain(|&id, _| is_newer(id, h.frame_id));
        self.last_delivered = Some(h.frame_id);
        Some(Assembled { frame_id: h.frame_id, data: done.buf, skipped })
    }

    /// True if some frame has been waiting for missing packets longer than `age`. On a static
    /// screen no newer frame arrives to reveal the loss, so the client checks this on a timer.
    pub fn has_stale_partial(&self, age: Duration) -> bool {
        self.partial.values().any(|p| p.first_seen.elapsed() > age)
    }

    /// Forgets incomplete frames (e.g. after requesting a keyframe).
    pub fn clear_partial(&mut self) {
        self.partial.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn round_trip_in_order() {
        let mut p = Packetizer::new();
        let mut r = Reassembler::new();
        for (i, len) in [1, 1188, 1189, 50_000, 300_123].into_iter().enumerate() {
            let data = frame(len, i as u8);
            let packets = p.packetize(&data, 1200).unwrap();
            assert!(packets.iter().all(|d| d.len() <= 1200));
            let mut out = None;
            for d in &packets {
                assert!(out.is_none(), "frame completed early");
                out = r.push(d);
            }
            let out = out.expect("frame assembled");
            assert_eq!(out.data, data);
            assert_eq!(out.skipped, 0);
        }
    }

    #[test]
    fn reordered_and_duplicated_packets() {
        let mut p = Packetizer::new();
        let mut r = Reassembler::new();
        let data = frame(20_000, 7);
        let mut packets = p.packetize(&data, 1200).unwrap();
        packets.reverse();
        let dup = packets[3].clone();
        packets.insert(5, dup);
        let results: Vec<_> = packets.iter().filter_map(|d| r.push(d)).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].data, data);
    }

    #[test]
    fn lost_packet_drops_frame_and_reports_skip() {
        let mut p = Packetizer::new();
        let mut r = Reassembler::new();
        let f0 = p.packetize(&frame(5_000, 0), 1200).unwrap();
        let f1 = p.packetize(&frame(5_000, 1), 1200).unwrap();
        let f2 = p.packetize(&frame(5_000, 2), 1200).unwrap();

        assert!(f0.iter().filter_map(|d| r.push(d)).next().is_some());
        // Frame 1 loses its second packet.
        for (i, d) in f1.iter().enumerate() {
            if i != 1 {
                assert!(r.push(d).is_none());
            }
        }
        let got = f2.iter().filter_map(|d| r.push(d)).next().unwrap();
        assert_eq!(got.frame_id, 2);
        assert_eq!(got.skipped, 1);
        // The missing packet of frame 1 arriving late is ignored.
        assert!(r.push(&f1[1]).is_none());
    }

    #[test]
    fn stale_partial_detected() {
        let mut p = Packetizer::new();
        let mut r = Reassembler::new();
        let f = p.packetize(&frame(5_000, 0), 1200).unwrap();
        r.push(&f[0]);
        assert!(!r.has_stale_partial(Duration::from_secs(60)));
        assert!(r.has_stale_partial(Duration::ZERO));
        r.clear_partial();
        assert!(!r.has_stale_partial(Duration::ZERO));
    }

    #[test]
    fn rejects_garbage() {
        let mut r = Reassembler::new();
        assert!(r.push(&[1, 2, 3]).is_none());
        let mut bad = Vec::new();
        VideoPacketHeader { frame_id: 0, index: 0, count: 2, total_len: 100 }.write(&mut bad);
        bad.extend_from_slice(&[0; 10]); // wrong chunk length
        assert!(r.push(&bad).is_none());
    }

    #[test]
    fn wrapping_frame_ids() {
        assert!(is_newer(0, u32::MAX));
        assert!(!is_newer(u32::MAX, 0));
        assert!(is_newer(5, 4));
    }
}
