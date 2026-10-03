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
    /// Host display → encoder input: ScreenCaptureKit's delivery plus any wait for the encoder.
    pub capture: Ema,
    pub encode: Ema,
    pub network: Ema,
    pub decode: Ema,
    /// Last tile of an update decoded → the update on screen (the display's presentation time).
    pub present: Ema,
    /// Captured on the host → on screen here.
    pub total: Ema,
    pub frames_decoded: u64,
    /// Updates that reached the screen.
    pub frames_shown: u64,
    pub keyframe_requests: u64,
    pub frames_lost: u64,
    /// Viewer input → injected on the host (from the host's input acks).
    pub input: Ema,
    /// Of that: viewer input → received by the host.
    pub input_network: Ema,
    /// Of that: received → posted on the host (decoding, `CGEventPost`).
    pub input_inject: Ema,
    pub inputs_sent: u64,
    window: RateWindow,
    /// Send times of recent input writes: (last sequence number in the write, our clock µs).
    input_sent: VecDeque<(u64, u64)>,
}

/// A snapshot for the UI. Times are in milliseconds.
#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StatsView {
    pub fps: f64,
    pub mbps: f64,
    /// Host capture → on screen here.
    pub total_ms: Option<f64>,
    pub capture_ms: Option<f64>,
    pub encode_ms: Option<f64>,
    pub network_ms: Option<f64>,
    pub decode_ms: Option<f64>,
    /// Decoded → on screen.
    pub display_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
    pub frames_decoded: u64,
    /// Updates that reached the screen.
    pub frames_shown: u64,
    pub frames_lost: u64,
    pub keyframe_requests: u64,
    /// Viewer input → injected on the host, while controlling.
    pub input_ms: Option<f64>,
    pub input_network_ms: Option<f64>,
    pub input_inject_ms: Option<f64>,
    pub inputs_sent: u64,
}

impl Stats {
    const INPUT_LOG: usize = 512;

    /// Input messages up to `last_seq` (counted from 1) were written at `sent_us`.
    pub fn on_input_sent(&mut self, last_seq: u64, sent_us: u64) {
        self.inputs_sent = last_seq;
        if self.input_sent.len() == Self::INPUT_LOG {
            self.input_sent.pop_front();
        }
        self.input_sent.push_back((last_seq, sent_us));
    }

    /// The host received input up to `seq` at `received_us` and injected it at `injected_us`
    /// (host clock).
    pub fn on_input_ack(&mut self, seq: u64, received_us: u64, injected_us: u64) {
        self.input_inject.add(injected_us.saturating_sub(received_us) as f64);
        let Some(&(_, sent_us)) = self.input_sent.iter().find(|(last, _)| *last >= seq) else { return };
        if let (Some(received), Some(injected)) = (self.clock.to_local(received_us), self.clock.to_local(injected_us)) {
            self.input_network.add(received.saturating_sub(sent_us) as f64);
            self.input.add(injected.saturating_sub(sent_us) as f64);
        }
    }

    pub fn view(&mut self) -> StatsView {
        StatsView {
            fps: self.fps(),
            mbps: self.mbps(),
            total_ms: self.total.ms(),
            capture_ms: self.capture.ms(),
            encode_ms: self.encode.ms(),
            network_ms: self.network.ms(),
            decode_ms: self.decode.ms(),
            display_ms: self.present.ms(),
            rtt_ms: self.clock.rtt_us().map(|us| us as f64 / 1000.0),
            frames_decoded: self.frames_decoded,
            frames_shown: self.frames_shown,
            frames_lost: self.frames_lost,
            keyframe_requests: self.keyframe_requests,
            input_ms: self.input.ms(),
            input_network_ms: self.input_network.ms(),
            input_inject_ms: self.input_inject.ms(),
            inputs_sent: self.inputs_sent,
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
        // Not `sum()`: an empty f64 sum is -0.0, which shows as "-0.0 Mbit/s".
        self.events.iter().fold(0.0, |sum, (_, b)| sum + *b as f64) * 8.0 / 1e6
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
