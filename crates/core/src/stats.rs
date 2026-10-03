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

/// The samples of the last [`Samples::SPAN`], for percentiles: the overlay's moving averages
/// hide the tail (a Wi-Fi stall, a full-screen change) that percentiles show.
#[derive(Default, Clone)]
pub struct Samples {
    samples: VecDeque<(Instant, f64)>,
}

impl Samples {
    const SPAN: Duration = Duration::from_secs(2);
    /// At most this many kept (about 2 s at 1000 samples/s).
    const MAX: usize = 2048;

    pub fn add(&mut self, sample_us: f64) {
        self.add_at(Instant::now(), sample_us);
    }

    fn add_at(&mut self, now: Instant, sample_us: f64) {
        if self.samples.len() == Self::MAX {
            self.samples.pop_front();
        }
        self.samples.push_back((now, sample_us));
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > Self::SPAN) {
            self.samples.pop_front();
        }
    }

    /// The `q` quantile (0-1) of the recent samples, in milliseconds.
    pub fn quantile_ms(&mut self, q: f64) -> Option<f64> {
        self.trim(Instant::now());
        if self.samples.is_empty() {
            return None;
        }
        let mut v: Vec<f64> = self.samples.iter().map(|(_, s)| *s).collect();
        v.sort_by(f64::total_cmp);
        let i = ((v.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
        Some(v[i] / 1000.0)
    }

    /// Samples kept now.
    pub fn len(&mut self) -> usize {
        self.trim(Instant::now());
        self.samples.len()
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
    /// Percentiles of the same stages, per update where the averages are per tile frame:
    /// captured → on screen, decoded (an update's last tile) → on screen, and per tile frame
    /// encoded → received.
    pub total_samples: Samples,
    pub display_samples: Samples,
    pub network_samples: Samples,
    /// The display stage split up, per shown update: decoded → the render thread at it, → the
    /// command buffer committed, → the GPU done, → on screen (the compositor and the display).
    pub display_wake: Samples,
    pub display_cpu: Samples,
    pub display_gpu: Samples,
    pub display_compositor: Samples,
    /// Updates (not tiles) presented on screen over the last second.
    shown: RateWindow,
    /// Presents with nothing new, which keep the display and GPU awake after an update.
    pub warm_presents: u64,
    /// Presents that never reached the screen (presentedTime 0).
    pub dropped_presents: u64,
    /// Gaps of at least [`Stats::STALL`] in what the host sends (it sends something at least every
    /// 20 ms): the network, usually the Wi-Fi radio, went silent. Count, and the gaps' lengths.
    pub stalls: u64,
    pub stall_samples: Samples,
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
    /// Updates presented on screen over the last second ("fps" counts updates received).
    pub shown_fps: f64,
    /// Percentiles (p50, p95) over the last 2 s; see [`Stats::total_samples`].
    pub total_p50_ms: Option<f64>,
    pub total_p95_ms: Option<f64>,
    pub display_p50_ms: Option<f64>,
    pub display_p95_ms: Option<f64>,
    pub network_p95_ms: Option<f64>,
    /// The display stage's parts, p50 (see [`Stats::display_wake`]).
    pub display_wake_ms: Option<f64>,
    pub display_cpu_ms: Option<f64>,
    pub display_gpu_ms: Option<f64>,
    pub display_compositor_ms: Option<f64>,
    pub warm_presents: u64,
    pub dropped_presents: u64,
    /// Network stalls so far, and the longest of the last 2 s.
    pub stalls: u64,
    pub max_stall_ms: Option<f64>,
}

impl Stats {
    const INPUT_LOG: usize = 512;
    /// A gap in what the host sends at least this long is a stall (see [`Stats::stalls`]).
    pub const STALL: Duration = Duration::from_millis(30);

    /// An update reached the screen.
    pub fn on_shown(&mut self) {
        self.frames_shown += 1;
        self.shown.add(0);
    }

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
            shown_fps: self.shown.fps(),
            total_p50_ms: self.total_samples.quantile_ms(0.5),
            total_p95_ms: self.total_samples.quantile_ms(0.95),
            display_p50_ms: self.display_samples.quantile_ms(0.5),
            display_p95_ms: self.display_samples.quantile_ms(0.95),
            network_p95_ms: self.network_samples.quantile_ms(0.95),
            display_wake_ms: self.display_wake.quantile_ms(0.5),
            display_cpu_ms: self.display_cpu.quantile_ms(0.5),
            display_gpu_ms: self.display_gpu.quantile_ms(0.5),
            display_compositor_ms: self.display_compositor.quantile_ms(0.5),
            warm_presents: self.warm_presents,
            dropped_presents: self.dropped_presents,
            stalls: self.stalls,
            max_stall_ms: self.stall_samples.quantile_ms(1.0),
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
    fn samples_give_percentiles_of_the_recent_ones() {
        let mut s = Samples::default();
        assert_eq!(s.quantile_ms(0.5), None);
        let t0 = Instant::now();
        // Half 8.33 ms, half 16.67 ms; then old ones age out.
        for i in 0..100 {
            s.add_at(t0, if i % 2 == 0 { 8_330.0 } else { 16_670.0 });
        }
        assert_eq!(s.quantile_ms(0.0), Some(8.33));
        assert_eq!(s.quantile_ms(0.95), Some(16.67));
        assert_eq!(s.quantile_ms(1.0), Some(16.67));
        let later = t0 + Samples::SPAN + Duration::from_millis(1);
        s.add_at(later, 1_000.0);
        assert_eq!(s.samples.len(), 1);
    }

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
