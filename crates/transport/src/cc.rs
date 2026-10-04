//! Congestion controllers for video, whose bitrate is controlled at the encoder instead.
//!
//! quinn's default (Cubic) starts small and backs off on loss, which only adds latency to video:
//! a keyframe burst sits in the datagram queue (and gets silently dropped) while the window grows,
//! and quinn paces every packet at 1.25 × window / round trip, so a small window spreads a frame
//! over most of a round trip. Video's packets are never sent again, and every loss Cubic sees
//! shrinks its window by 30%: random loss on a long path (Wi-Fi at either end, the internet in
//! between) keeps it at a few Mbit/s, under what the stream needs, and frames queue.
//!
//! - On a trusted LAN ([`LanController`]) the window is simply large and fixed.
//! - Over the internet ([`WanController`]) the window follows a pacing rate the host sets from its
//!   bitrate ceiling ([`Pace`]): frames go out a few times faster than the stream's average, never
//!   at a crawl, and the rate controller in the core backs off on loss and queueing by lowering
//!   the encoders' bitrate, which is what makes the stream fit the path.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;

/// 32 MiB in flight is far more than a LAN can hold, i.e. effectively "never block".
const WINDOW: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct LanController;

impl Controller for LanController {
    fn on_congestion_event(&mut self, _now: Instant, _sent: Instant, _persistent: bool, _lost: u64) {}

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        WINDOW
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        WINDOW
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Debug, Default)]
pub struct LanControllerFactory;

impl ControllerFactory for LanControllerFactory {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(LanController)
    }
}

/// quinn paces packets at this many times the window per round trip.
const PACER_GAIN: f64 = 1.25;
/// The pace a connection starts with, until the host sets one (bit/s).
const DEFAULT_PACE_BPS: u64 = 40_000_000;
/// The window is never smaller than this many packets ...
const MIN_WINDOW_PACKETS: u64 = 64;
/// ... nor larger than this. (The datagram send buffer holds 4 MiB.)
const MAX_WINDOW: u64 = 16 * 1024 * 1024;
/// The round trip the window assumes before the first sample (quinn's initial guess).
const INITIAL_RTT: Duration = Duration::from_millis(333);

/// How fast an internet connection sends (bit/s): set by the host as its video's bitrate ceiling
/// moves, read by the connection's [`WanController`]. One per connection.
#[derive(Clone, Debug)]
pub struct Pace(Arc<AtomicU64>);

impl Pace {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(DEFAULT_PACE_BPS)))
    }

    pub fn set(&self, bps: u64) {
        self.0.store(bps.max(1), Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

impl Default for Pace {
    fn default() -> Self {
        Self::new()
    }
}

/// A controller for video over the internet: its window is what makes quinn pace packets out at
/// [`Pace`] (the window over the round trip, times quinn's 1.25), with room for some
/// [`MIN_WINDOW_PACKETS`] on a short path. A burst longer than the window goes out at 80% of the
/// pace: what is in flight is acknowledged a round trip later. (The host's pace is many times its
/// bitrate.) Loss doesn't shrink it: the host's rate controller answers loss and queueing by
/// lowering the bitrate, and with it the pace.
#[derive(Debug, Clone)]
pub struct WanController {
    pace: Pace,
    rtt: Duration,
    mtu: u16,
}

impl WanController {
    fn window_for(&self, rtt: Duration) -> u64 {
        let bytes = self.pace.get() as f64 / 8.0 * rtt.as_secs_f64() / PACER_GAIN;
        (bytes as u64).clamp(MIN_WINDOW_PACKETS * u64::from(self.mtu), MAX_WINDOW)
    }
}

impl Controller for WanController {
    fn on_ack(&mut self, _now: Instant, _sent: Instant, _bytes: u64, _app_limited: bool, rtt: &RttEstimator) {
        self.rtt = rtt.get();
    }

    fn on_congestion_event(&mut self, _now: Instant, _sent: Instant, _persistent: bool, _lost: u64) {}

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = new_mtu;
    }

    fn window(&self) -> u64 {
        self.window_for(self.rtt)
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.window_for(INITIAL_RTT)
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Builds a connection's [`WanController`], paced by `pace`. quinn builds a new one when the
/// connection moves to another path; it keeps the pace.
#[derive(Debug, Default)]
pub struct WanControllerFactory {
    pub pace: Pace,
}

impl ControllerFactory for WanControllerFactory {
    fn build(self: Arc<Self>, _now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(WanController { pace: self.pace.clone(), rtt: INITIAL_RTT, mtu: current_mtu })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(pace_bps: u64, rtt_ms: u64) -> WanController {
        let pace = Pace::new();
        pace.set(pace_bps);
        WanController { pace, rtt: Duration::from_millis(rtt_ms), mtu: 1200 }
    }

    #[test]
    fn the_window_makes_quinn_pace_at_the_pace() {
        // quinn sends 1.25 windows per round trip.
        for (bps, rtt) in [(40_000_000, 150), (36_000_000, 40), (100_000_000, 75)] {
            let c = controller(bps, rtt);
            let paced = 1.25 * c.window() as f64 * 8.0 / (rtt as f64 / 1e3);
            assert!((paced / bps as f64 - 1.0).abs() < 0.01, "{bps} bit/s at {rtt} ms paces at {paced}");
        }
    }

    #[test]
    fn the_window_has_bounds() {
        // A short path still gets a burst's worth: on a LAN-like round trip pacing all but stops.
        assert_eq!(controller(40_000_000, 1).window(), 64 * 1200);
        assert_eq!(controller(10_000_000_000, 1000).window(), MAX_WINDOW);
    }

    #[test]
    fn loss_doesnt_shrink_the_window_but_the_pace_does() {
        let mut c = controller(40_000_000, 100);
        let before = c.window();
        let now = Instant::now();
        c.on_congestion_event(now, now, true, 100_000);
        assert_eq!(c.window(), before);
        c.pace.set(20_000_000);
        assert_eq!(c.window(), before / 2);
    }
}
