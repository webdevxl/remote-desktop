//! Clock sync and live latency statistics shown in the viewer overlay.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Serialize;

/// Estimates the offset between the host clock and ours from ping/pong round trips, using the
/// samples with the smallest RTT (least queuing noise).
#[derive(Default)]
pub struct ClockSync {
    samples: VecDeque<(u64, i64)>,
}

impl ClockSync {
    const WINDOW: usize = 16;

    /// `sent`/`received` are our clock, `host` is the host clock at reply time (all µs).
    pub fn add(&mut self, sent: u64, received: u64, host: u64) {
        let rtt = received.saturating_sub(sent);
        let midpoint = sent + rtt / 2;
        let offset = host as i64 - midpoint as i64;
        if self.samples.len() == Self::WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back((rtt, offset));
    }

    fn best(&self) -> Option<(u64, i64)> {
        self.samples.iter().copied().min_by_key(|(rtt, _)| *rtt)
    }

    pub fn rtt_us(&self) -> Option<u64> {
        self.best().map(|(rtt, _)| rtt)
    }

    /// Converts a host timestamp to our clock.
    pub fn to_local(&self, host_us: u64) -> Option<u64> {
        self.best().map(|(_, offset)| (host_us as i64 - offset).max(0) as u64)
    }
}

/// Exponential moving average for jittery per-frame measurements.
#[derive(Default, Clone, Copy)]
pub struct Ema(Option<f64>);

impl Ema {
    pub fn add(&mut self, sample_us: f64) {
        self.0 = Some(match self.0 {
            Some(v) => v * 0.9 + sample_us * 0.1,
            None => sample_us,
        });
    }

    pub fn ms(&self) -> Option<f64> {
        self.0.map(|us| us / 1000.0)
    }
}

/// Per-frame timing carried from reception to presentation (all on our clock, µs).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTiming {
    pub capture_local_us: Option<u64>,
    pub received_us: u64,
    pub decoded_us: u64,
}

#[derive(Default)]
pub struct Stats {
    pub clock: ClockSync,
    pub encode: Ema,
    pub network: Ema,
    pub decode: Ema,
    pub present: Ema,
    pub total: Ema,
    pub frames_decoded: u64,
    pub frames_shown: u64,
    pub keyframe_requests: u64,
    pub frames_lost: u64,
    window: RateWindow,
}

/// A snapshot for the UI. Times are in milliseconds.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StatsView {
    pub fps: f64,
    pub mbps: f64,
    pub total_ms: Option<f64>,
    pub encode_ms: Option<f64>,
    pub network_ms: Option<f64>,
    pub decode_ms: Option<f64>,
    pub display_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub frames_decoded: u64,
    pub frames_shown: u64,
    pub frames_lost: u64,
    pub keyframe_requests: u64,
}

impl Stats {
    pub fn view(&mut self) -> StatsView {
        StatsView {
            fps: self.fps(),
            mbps: self.mbps(),
            total_ms: self.total.ms(),
            encode_ms: self.encode.ms(),
            network_ms: self.network.ms(),
            decode_ms: self.decode.ms(),
            display_ms: self.present.ms(),
            rtt_ms: self.clock.rtt_us().map(|us| us as f64 / 1000.0),
            frames_decoded: self.frames_decoded,
            frames_shown: self.frames_shown,
            frames_lost: self.frames_lost,
            keyframe_requests: self.keyframe_requests,
        }
    }

    pub fn on_frame_received(&mut self, bytes: usize) {
        self.window.add(bytes);
    }

    pub fn fps(&mut self) -> f64 {
        self.window.fps()
    }

    pub fn mbps(&mut self) -> f64 {
        self.window.mbps()
    }
}

/// Frames and bytes received over the last second.
#[derive(Default)]
struct RateWindow {
    events: VecDeque<(Instant, usize)>,
}

impl RateWindow {
    const SPAN: Duration = Duration::from_secs(1);

    fn add(&mut self, bytes: usize) {
        self.events.push_back((Instant::now(), bytes));
        self.trim();
    }

    fn trim(&mut self) {
        while self.events.front().is_some_and(|(t, _)| t.elapsed() > Self::SPAN) {
            self.events.pop_front();
        }
    }

    fn fps(&mut self) -> f64 {
        self.trim();
        self.events.len() as f64
    }

    fn mbps(&mut self) -> f64 {
        self.trim();
        self.events.iter().map(|(_, b)| *b as f64).sum::<f64>() * 8.0 / 1e6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_sync_prefers_low_rtt() {
        let mut c = ClockSync::default();
        // Host clock is 1_000_000 µs ahead. One noisy sample with a long one-way delay.
        c.add(100, 300, 1_000_200); // rtt 200, true midpoint
        c.add(1_000, 3_000, 1_001_100); // rtt 2000, skewed
        assert_eq!(c.rtt_us(), Some(200));
        assert_eq!(c.to_local(1_000_200), Some(200));
    }
}
