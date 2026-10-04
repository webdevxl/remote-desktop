//! Over the internet, the video's bitrate follows what the connection carries.
//!
//! On the local network a stream gets a generous fixed bitrate. An internet path is shared, far
//! slower, and changes from one minute to the next, so there a [`RateControl`] keeps a ceiling
//! for the stream's bitrate, from a [`Sample`] of QUIC's own counters every [`TICK`]:
//!
//! - It **starts** at [`START_BPS`], or the stream's LAN bitrate if that is lower.
//! - It **decreases** to 85% of what the path delivered over the last two ticks (what QUIC sent,
//!   less what it found lost), and by at least 15%, never below [`FLOOR_BPS`], when the stream is
//!   busy (it used at least 70% of the ceiling, or video waits in QUIC's datagram buffer beyond
//!   the backlog limit, [`backlog_limit`]) and:
//!   - more than 2% of the packets sent were lost (judged over at least a second and 50 packets),
//!     or more than 10% over the last two ticks;
//!   - QUIC saw a loss (a congestion event) while video waits beyond the backlog limit. QUIC
//!     counts every loss as one, so a loss alone, which may be noise on a path with room to
//!     spare, doesn't count;
//!   - a queue along the path overflowed: packets were lost while the round trip is above its
//!     least over the last 10 s by more than max(10 ms, a quarter of that least). Noise loses
//!     packets with the queues empty;
//!   - the round trip is above its least over the last 10 s by more than max(20 ms, a quarter of
//!     that least) for two ticks in a row: a queue is building up somewhere along the path, and
//!     every packet waits in it. (QUIC's own minimum is the connection's lifetime's: a path whose
//!     round trip went up for good, another route, would look congested from then on.)
//!   - or video has waited beyond the backlog limit for 200 ms: the path doesn't carry what the
//!     encoders make.
//!
//!   A stream sending well under the ceiling with nothing waiting can't be what loses or queues
//!   up packets along the path (that is noise, or someone else's traffic), and what it sends
//!   says nothing about the path: the ceiling stays. Except when the stream sent at least 20% of
//!   the ceiling and a queue overflowed or over 2% was lost: as QUIC doesn't back off (see the
//!   pace below), a stream needing less than its ceiling still sends more than a slower path
//!   carries, and its bursts (a keyframe, a scroll) can overflow a short queue; it decreases as
//!   above. A decrease at most halves the ceiling: what went out over a stall (a Wi-Fi roam) says
//!   nothing about the path either. After a decrease the ceiling holds for 1 s, or, when over a
//!   tenth of what went out was lost, for two round trips (at least 300 ms): long enough for the
//!   smaller frames to show in what is lost, short enough not to lose that much for long. Such
//!   loss also ends a longer hold that far after the decrease: the path got slower since.
//! - It **increases** by 8% every 200 ms, up to the stream's LAN bitrate, only when the stream
//!   used at least 70% of the ceiling (otherwise the screen didn't need more, and nothing was
//!   learnt about the path) and none of the signals above shows in the latest tick: no inflated
//!   round trip, no loss over 2%, nothing waiting beyond the limit. (A congestion event alone
//!   doesn't stop it: on a path that loses a packet now and then, most ticks have one.) Near
//!   where loss or a queue last brought it down (from 90% of that), only by 2%, for 10 s after:
//!   the path's limit is likely there, and going over it is a queue or loss again.
//!
//! The host also holds back the next update while the backlog is over its limit, so video that
//! can't go out yet (QUIC's flow control, a burst over its window) costs frame rate instead of
//! queueing stale pictures.
//!
//! QUIC itself doesn't back off on loss: loss shrinks the bitrate here instead of a congestion
//! window there. The connection sends at a **pace** ([`pace_for`], see
//! `transport::cc::WanController`): how fast a frame leaves, not how much video there is. It is
//! high, [`PACE_GAIN`] times the ceiling and at least [`PACE_FLOOR_BPS`], so a frame leaves about
//! as fast as the path takes it. Pacing it slower than that makes it late for nothing; pacing it
//! just over a slow bottleneck (rather than at once) only stretches the time its queue overflows,
//! onto the frames behind it. (Measured with `tests/wan_latency.rs`: QUIC's Cubic, whose window
//! also paced each frame over most of a round trip and shrank with every random loss, added
//! 100-400 ms to frames on long paths losing 0.5% of their packets; this adds next to nothing.)

use std::time::Duration;

use quinn::ConnectionStats;

/// How often the connection is sampled.
pub const TICK: Duration = Duration::from_millis(100);
/// The ceiling a connection starts with.
pub const START_BPS: u32 = 12_000_000;
/// The ceiling never goes below this: a picture still worth looking at.
pub const FLOOR_BPS: u32 = 1_500_000;
/// Through a LanKVM server's relay the ceiling stays under this: the server passes up to
/// 40 Mbit/s a session (both ways together) and drops what goes over.
pub const RELAYED_MAX_BPS: u32 = 32_000_000;
/// The connection sends this many times the ceiling ...
const PACE_GAIN: f64 = 8.0;
/// ... and at least this fast (bit/s): about as fast as a path with room takes a frame.
const PACE_FLOOR_BPS: f64 = 150_000_000.0;
/// More lost than this fraction of the packets sent calls for a decrease ...
const MAX_LOSS: f64 = 0.02;
/// ... judged over at least this long and this many packets (keep-alives alone send 50 a
/// second): over fewer, a couple of losses for no reason would look like more than 2%.
const LOSS_WINDOW: Duration = Duration::from_secs(1);
const LOSS_MIN_PACKETS: u64 = 50;
/// The round trip counts as inflated above its least over the last `RTT_WINDOW` seconds plus the
/// larger of `RTT_SLACK` and a quarter of that least, for `RTT_TICKS` ticks in a row.
const RTT_WINDOW: usize = 10;
const RTT_SLACK: Duration = Duration::from_millis(20);
const RTT_TICKS: u32 = 2;
/// A loss counts as a queue overflowing when the round trip is above its least by more than the
/// larger of this and a quarter of that least.
const QUEUE_SLACK: Duration = Duration::from_millis(10);
/// Video waiting beyond the backlog limit this long calls for a decrease.
const BACKLOG_PATIENCE: Duration = Duration::from_millis(200);
/// A decrease goes to this fraction of what the path delivered (and of the ceiling, at most) ...
const DECREASE_TO: f64 = 0.85;
/// ... keeping at least this fraction of the ceiling ...
const DECREASE_KEEPS: f64 = 0.5;
/// ... and then the ceiling holds this long ...
const HOLD: Duration = Duration::from_secs(1);
/// ... or two round trips and at least this long, when over `HEAVY_LOSS` of the packets sent over
/// the last two ticks were lost.
const SHORT_HOLD: Duration = Duration::from_millis(300);
const HEAVY_LOSS: f64 = 0.1;
/// After the connection moved to another path, losses don't count for this many ticks: they are
/// the packets still on the old one.
const PATH_CHANGE_DEAF_TICKS: u32 = 3;
/// An increase multiplies the ceiling by this, at most every `STEP_INTERVAL`.
const STEP: f64 = 1.08;
const STEP_INTERVAL: Duration = Duration::from_millis(200);
/// Near the ceiling that loss or a queue last brought down (from this fraction of it), an increase
/// multiplies by `CAREFUL_STEP` instead, for `LIMIT_MEMORY` after.
const NEAR_LIMIT: f64 = 0.9;
const CAREFUL_STEP: f64 = 1.02;
const LIMIT_MEMORY: Duration = Duration::from_secs(10);
/// Sending less than this fraction of the ceiling, the stream is limited by what the screen
/// shows, not by the ceiling.
const APP_LIMITED_BELOW: f64 = 0.7;
/// Sending at least this fraction of the ceiling, the stream's bursts can be what overflows a
/// queue, though it is limited by what the screen shows.
const ACTIVE_ABOVE: f64 = 0.2;
/// The backlog limit: about this much video at the ceiling, and at least `MIN_BACKLOG`.
const BACKLOG_TIME: Duration = Duration::from_millis(40);
const MIN_BACKLOG: usize = 64 * 1024;
/// Keyframes asked for go out at most once a round trip, within these bounds.
const MIN_KEYFRAME_GAP: Duration = Duration::from_millis(50);
const MAX_KEYFRAME_GAP: Duration = Duration::from_millis(500);

/// Bytes of video that may wait in QUIC's datagram buffer at `ceiling_bps` before the host holds
/// back the next update: about 40 ms of video, and at least a keyframe of a tile or two.
pub fn backlog_limit(ceiling_bps: u32) -> usize {
    let bytes = f64::from(ceiling_bps) / 8.0 * BACKLOG_TIME.as_secs_f64();
    (bytes as usize).max(MIN_BACKLOG)
}

/// How fast the connection sends at ceiling `ceiling_bps` (bit/s): see the module's description.
pub fn pace_for(ceiling_bps: u32) -> u64 {
    (PACE_GAIN * f64::from(ceiling_bps)).max(PACE_FLOOR_BPS) as u64
}

/// The least time between keyframes served on request, for a round trip of `rtt`: a viewer that
/// asks again before the last ones could have arrived only gets the same tiles twice.
pub fn keyframe_gap(rtt: Duration) -> Duration {
    rtt.clamp(MIN_KEYFRAME_GAP, MAX_KEYFRAME_GAP)
}

/// What a connection did over one tick.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    /// The tick's length.
    pub interval: Duration,
    /// What QUIC sent over the tick (everything: video, control messages, acknowledgements), and
    /// what it found lost and the congestion events it counted.
    pub sent_bytes: u64,
    pub sent_packets: u64,
    pub lost_bytes: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    /// The smoothed round trip, now.
    pub rtt: Duration,
    /// Bytes of datagrams waiting in QUIC's send buffer, now.
    pub backlog: usize,
}

impl Sample {
    /// From two snapshots of a connection's statistics `interval` apart, and the backlog now.
    pub fn between(before: &ConnectionStats, after: &ConnectionStats, interval: Duration, backlog: usize) -> Self {
        Self {
            interval,
            sent_bytes: after.udp_tx.bytes.saturating_sub(before.udp_tx.bytes),
            sent_packets: after.path.sent_packets.saturating_sub(before.path.sent_packets),
            lost_bytes: after.path.lost_bytes.saturating_sub(before.path.lost_bytes),
            lost_packets: after.path.lost_packets.saturating_sub(before.path.lost_packets),
            congestion_events: after.path.congestion_events.saturating_sub(before.path.congestion_events),
            rtt: after.path.rtt,
            backlog,
        }
    }
}

/// A stream's bitrate ceiling over the internet (see the module's description).
#[derive(Debug)]
pub struct RateControl {
    /// The stream's LAN bitrate: never more than that.
    cap: u32,
    ceiling: u32,
    /// Time left before the ceiling may move again, after a decrease, and time since that.
    hold: Duration,
    since_decrease: Duration,
    /// Time since the last increase (or since the hold ended).
    since_step: Duration,
    /// The last two ticks: the send rate, what the path delivered, the loss.
    recent: [Recent; 2],
    /// Ticks whose losses don't count, after the connection moved to another path.
    deaf: u32,
    /// Packets sent and lost since loss was last judged, and over how long.
    sent: u64,
    lost: u64,
    loss_time: Duration,
    /// The least round trip lately, ticks in a row with the round trip inflated, and how long the
    /// backlog has been over its limit.
    least_rtt: RecentMin,
    rtt_high: u32,
    backlog_over: Duration,
    /// The ceiling that loss or a queue last brought down, and how long ago.
    limit: Option<f64>,
    since_limit: Duration,
}

/// What a tick sent and lost, and its length.
#[derive(Clone, Copy, Debug, Default)]
struct Recent {
    sent_bytes: u64,
    lost_bytes: u64,
    sent_packets: u64,
    lost_packets: u64,
    interval: Duration,
}

/// The least of a value over about the last [`RTT_WINDOW`] seconds, kept by the second.
#[derive(Debug)]
struct RecentMin {
    seconds: [Duration; RTT_WINDOW],
    /// The second being filled, and how much of it is.
    at: usize,
    filled: Duration,
}

impl RecentMin {
    fn new() -> Self {
        Self { seconds: [Duration::MAX; RTT_WINDOW], at: 0, filled: Duration::ZERO }
    }

    /// Takes `value`, `interval` after the one before; returns the least lately.
    fn add(&mut self, value: Duration, interval: Duration) -> Duration {
        self.filled += interval;
        if self.filled >= Duration::from_secs(1) {
            self.filled = Duration::ZERO;
            self.at = (self.at + 1) % RTT_WINDOW;
            self.seconds[self.at] = Duration::MAX;
        }
        self.seconds[self.at] = self.seconds[self.at].min(value);
        self.seconds.iter().copied().min().unwrap_or(value)
    }
}

impl RateControl {
    /// For a stream whose LAN bitrate is `cap_bps`.
    pub fn new(cap_bps: u32) -> Self {
        let cap = cap_bps.max(FLOOR_BPS);
        Self {
            cap,
            ceiling: START_BPS.min(cap),
            hold: Duration::ZERO,
            since_decrease: Duration::MAX,
            since_step: Duration::ZERO,
            recent: [Recent::default(); 2],
            deaf: 0,
            sent: 0,
            lost: 0,
            loss_time: Duration::ZERO,
            least_rtt: RecentMin::new(),
            rtt_high: 0,
            backlog_over: Duration::ZERO,
            limit: None,
            since_limit: Duration::ZERO,
        }
    }

    /// The bitrate ceiling (bit/s).
    pub fn ceiling(&self) -> u32 {
        self.ceiling
    }

    /// Another stream started (another display or size), with LAN bitrate `cap_bps`. Returns the
    /// new ceiling if it had to come down to it.
    pub fn set_cap(&mut self, cap_bps: u32) -> Option<u32> {
        self.cap = cap_bps.max(FLOOR_BPS);
        self.set(f64::from(self.ceiling))
    }

    /// What the connection sent lately (bit/s): over the last two ticks.
    pub fn send_rate(&self) -> f64 {
        self.recent_rate(|r| r.sent_bytes)
    }

    /// What the path delivered lately (bit/s): what was sent over the last two ticks, less what
    /// was found lost.
    pub fn delivered_rate(&self) -> f64 {
        self.recent_rate(|r| r.sent_bytes.saturating_sub(r.lost_bytes))
    }

    fn recent_rate(&self, bytes: impl Fn(&Recent) -> u64) -> f64 {
        let time: Duration = self.recent.iter().map(|r| r.interval).sum();
        let bytes: u64 = self.recent.iter().map(bytes).sum();
        if time.is_zero() { 0.0 } else { bytes as f64 * 8.0 / time.as_secs_f64() }
    }

    /// The connection moved to another path (off a relay, say): what it lost now is mostly what
    /// was still on the old one, and the round trip starts anew.
    pub fn path_changed(&mut self) {
        (self.sent, self.lost, self.loss_time) = (0, 0, Duration::ZERO);
        self.recent = [Recent::default(); 2];
        self.least_rtt = RecentMin::new();
        self.rtt_high = 0;
        self.backlog_over = Duration::ZERO;
        self.limit = None;
        self.deaf = PATH_CHANGE_DEAF_TICKS;
    }

    /// Takes one tick's sample; returns the new ceiling if it changed.
    pub fn on_sample(&mut self, s: &Sample) -> Option<u32> {
        let mut s = *s;
        if self.deaf > 0 {
            self.deaf -= 1;
            (s.lost_bytes, s.lost_packets, s.congestion_events) = (0, 0, 0);
        }
        let tick = Recent {
            sent_bytes: s.sent_bytes,
            lost_bytes: s.lost_bytes,
            sent_packets: s.sent_packets,
            lost_packets: s.lost_packets,
            interval: s.interval,
        };
        self.recent = [self.recent[1], tick];
        let rate = self.send_rate();
        let ceiling = f64::from(self.ceiling);
        let (sent, lost) = self.recent.iter().fold((0, 0), |(sent, lost), r| (sent + r.sent_packets, lost + r.lost_packets));
        let heavy_loss = lost as f64 > HEAVY_LOSS * sent.max(1) as f64;
        // Down to what the path delivered, and by at least 15%: QUIC sends what it is given.
        let decreased = (DECREASE_TO * rate.min(self.delivered_rate())).max(DECREASE_KEEPS * ceiling).min(DECREASE_TO * ceiling);

        // What the sample says about the path.
        self.sent += s.sent_packets;
        self.lost += s.lost_packets;
        self.loss_time += s.interval;
        let mut lossy = false;
        if self.loss_time >= LOSS_WINDOW && self.sent >= LOSS_MIN_PACKETS {
            lossy = self.lost as f64 > MAX_LOSS * self.sent as f64;
            (self.sent, self.lost, self.loss_time) = (0, 0, Duration::ZERO);
        }
        let backlogged = s.backlog > backlog_limit(self.ceiling);
        self.backlog_over = if backlogged { self.backlog_over + s.interval } else { Duration::ZERO };
        let least = self.least_rtt.add(s.rtt, s.interval);
        let inflated = s.rtt > least + RTT_SLACK.max(least / 4);
        self.rtt_high = if inflated { self.rtt_high + 1 } else { 0 };
        let overflowed = s.lost_packets > 0 && s.rtt > least + QUEUE_SLACK.max(least / 4);
        let congested = (s.congestion_events > 0 && backlogged) || overflowed;
        let app_limited = rate < APP_LIMITED_BELOW * ceiling;

        self.since_limit += s.interval;
        if self.since_limit >= LIMIT_MEMORY {
            self.limit = None;
        }
        self.hold = self.hold.saturating_sub(s.interval);
        self.since_decrease = self.since_decrease.saturating_add(s.interval);
        // Heavy loss ends a hold early: the path got slower since (someone else's download, a
        // Wi-Fi rate drop), and it is lost for as long as the hold lasts.
        let short_hold = (2 * s.rtt).max(SHORT_HOLD);
        let held = !self.hold.is_zero();
        if held && !(heavy_loss && self.since_decrease >= short_hold) {
            return None;
        }
        let busy = backlogged || !app_limited;
        let path_limit = lossy || heavy_loss || congested || self.rtt_high >= RTT_TICKS;
        if path_limit && (busy || rate >= ACTIVE_ABOVE * ceiling) {
            // Not a stall: what went out then says nothing about where the path's limit is.
            self.limit = Some(ceiling);
            self.since_limit = Duration::ZERO;
        }
        // A stream well under its ceiling on average still sends more than a slower path carries,
        // and its bursts (a keyframe, a scroll) can overflow a short queue. (Not a stream sending
        // next to nothing: keep-alives lost now and then are noise.)
        let active = rate >= ACTIVE_ABOVE * ceiling && (overflowed || lossy || heavy_loss);
        if (busy && (path_limit || self.backlog_over >= BACKLOG_PATIENCE)) || active {
            self.hold = if heavy_loss { short_hold } else { HOLD };
            self.since_decrease = Duration::ZERO;
            self.since_step = Duration::ZERO;
            self.rtt_high = 0;
            self.backlog_over = Duration::ZERO;
            return self.set(decreased);
        }
        // A hold cut short is for decreasing only.
        if held {
            return None;
        }
        self.since_step += s.interval;
        if self.since_step < STEP_INTERVAL {
            return None;
        }
        self.since_step = Duration::ZERO;
        if app_limited || lossy || heavy_loss || inflated || overflowed || backlogged {
            return None;
        }
        let near_limit = self.limit.is_some_and(|limit| ceiling * STEP > NEAR_LIMIT * limit);
        self.set(ceiling * if near_limit { CAREFUL_STEP } else { STEP })
    }

    /// Sets the ceiling to `bps`, within the floor and the cap; returns it if it changed.
    fn set(&mut self, bps: f64) -> Option<u32> {
        let bps = (bps as u32).clamp(FLOOR_BPS, self.cap);
        if bps == self.ceiling {
            return None;
        }
        self.ceiling = bps;
        Some(bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);
    const CAP: u32 = 80_000_000;

    /// One tick sending at `mbps`, on a path with a 31 ms round trip and nothing else going on.
    fn tick(mbps: f64) -> Sample {
        Sample {
            interval: TICK,
            sent_bytes: (mbps * 1e6 / 8.0 * TICK.as_secs_f64()) as u64,
            sent_packets: 100,
            lost_bytes: 0,
            lost_packets: 0,
            congestion_events: 0,
            rtt: 31 * MS,
            backlog: 0,
        }
    }

    fn near(bps: u32, expected: f64) -> bool {
        (f64::from(bps) - expected).abs() <= 1.0
    }

    /// Feeds `n` ticks made by `sample` from the current ceiling; returns the ceilings after each.
    fn run(control: &mut RateControl, n: usize, sample: impl Fn(f64) -> Sample) -> Vec<u32> {
        (0..n)
            .map(|_| {
                let s = sample(f64::from(control.ceiling()) / 1e6);
                control.on_sample(&s);
                control.ceiling()
            })
            .collect()
    }

    fn rising(ceilings: &[u32]) -> bool {
        ceilings.windows(2).all(|w| w[1] >= w[0])
    }

    #[test]
    fn starts_at_twelve_megabits_or_the_lan_bitrate() {
        assert_eq!(RateControl::new(CAP).ceiling(), START_BPS);
        assert_eq!(RateControl::new(9_000_000).ceiling(), 9_000_000);
    }

    #[test]
    fn rises_while_the_stream_uses_the_ceiling() {
        let mut control = RateControl::new(CAP);
        // Every 200 ms, 8% more.
        let ceilings = run(&mut control, 4, tick);
        assert_eq!(ceilings[0], START_BPS);
        assert_eq!(ceilings[1], (f64::from(START_BPS) * STEP) as u32);
        assert_eq!(ceilings[2], ceilings[1]);
        assert_eq!(ceilings[3], (f64::from(ceilings[1]) * STEP) as u32);
        // Up to the stream's LAN bitrate, and no further.
        let ceilings = run(&mut control, 100, tick);
        assert_eq!(*ceilings.last().unwrap(), CAP);
    }

    #[test]
    fn does_not_rise_while_the_screen_needs_less() {
        let mut control = RateControl::new(CAP);
        // Two thirds of the ceiling: what the screen needs, nothing learnt about the path.
        let ceilings = run(&mut control, 30, |mbps| tick(mbps * 0.66));
        assert!(ceilings.iter().all(|&c| c == START_BPS), "{ceilings:?}");
    }

    #[test]
    fn falls_on_loss() {
        let mut control = RateControl::new(CAP);
        // 1 lost in 100: noise.
        let ceilings = run(&mut control, 10, |mbps| Sample { lost_packets: 1, ..tick(mbps) });
        assert!(rising(&ceilings) && ceilings[9] > START_BPS, "{ceilings:?}");
        // 3 in 100 over a second: to 85% of the send rate.
        let lossy = |mbps| Sample { lost_packets: 3, ..tick(mbps) };
        let ceilings = run(&mut control, 9, lossy);
        assert!(rising(&ceilings), "judged over a second: {ceilings:?}");
        let high = control.ceiling();
        control.on_sample(&lossy(f64::from(high) / 1e6));
        assert!(control.ceiling() < high && near(control.ceiling(), DECREASE_TO * control.send_rate()));
    }

    #[test]
    fn loss_is_judged_over_a_second() {
        let mut control = RateControl::new(CAP);
        // 10 lost in a tick, none in the 9 after: 1% over the second, not 10%.
        assert_eq!(control.on_sample(&Sample { lost_packets: 10, ..tick(12.0) }), None);
        let ceilings = run(&mut control, 9, tick);
        assert!(rising(&ceilings) && control.ceiling() > START_BPS, "{ceilings:?}");
    }

    #[test]
    fn random_loss_does_not_hold_the_ceiling_down() {
        let mut control = RateControl::new(CAP);
        // 1 packet in 100 lost for no reason: QUIC counts a congestion event nearly every tick.
        let noisy = |mbps| Sample { lost_packets: 1, congestion_events: 1, ..tick(mbps) };
        assert_eq!(*run(&mut control, 100, noisy).last().unwrap(), CAP);
    }

    #[test]
    fn falls_at_once_on_loss_with_a_queue() {
        let mut control = RateControl::new(CAP);
        run(&mut control, 4, tick);
        let high = control.ceiling();
        // One loss, 12 ms over the least (31 ms): a queue that overflowed, not noise.
        let overflow = Sample { lost_packets: 1, rtt: 43 * MS, ..tick(f64::from(high) / 1e6) };
        assert!(control.on_sample(&overflow).is_some_and(|c| c < high));
        // A loss with the round trip at its least, or a round trip up as much without a loss:
        // neither.
        let mut control = RateControl::new(CAP);
        let noise = |mbps| Sample { lost_packets: 1, rtt: 31 * MS, ..tick(mbps) };
        assert!(rising(&run(&mut control, 4, noise)));
        let before = control.ceiling();
        let up = Sample { rtt: 43 * MS, ..tick(f64::from(before) / 1e6) };
        assert!(control.on_sample(&up).is_none_or(|c| c >= before));
    }

    #[test]
    fn heavy_loss_cuts_a_hold_short() {
        let mut control = RateControl::new(CAP);
        run(&mut control, 20, tick);
        // A decrease (a queue), then the path slows down to a quarter: 30 of 100 lost a tick.
        let queue = |mbps| Sample { rtt: 60 * MS, ..tick(mbps) };
        let ceilings = run(&mut control, 3, queue);
        let after = ceilings[2];
        assert!(after < ceilings[0], "{ceilings:?}");
        let heavy = |mbps| Sample { lost_packets: 30, ..tick(mbps) };
        let ceilings = run(&mut control, 6, heavy);
        // Down again 300 ms after the first decrease, not a second after.
        assert!(ceilings[1] < after, "{ceilings:?}");
    }

    #[test]
    fn rises_carefully_near_where_the_path_gave_out() {
        let mut control = RateControl::new(CAP);
        run(&mut control, 20, tick);
        // Loss, judged over a second: down from where the ceiling got to.
        let lossy = |mbps| Sample { lost_packets: 3, ..tick(mbps) };
        let ceilings = run(&mut control, 10, lossy);
        assert!(ceilings[9] < ceilings[8], "{ceilings:?}");
        // From within 10% of where it gave out (it came down by 15%), 2% at a time, past it too.
        let limit = ceilings[8];
        let ceilings = run(&mut control, 40, tick);
        let steps: Vec<f64> = ceilings.windows(2).filter(|w| w[1] != w[0]).map(|w| f64::from(w[1]) / f64::from(w[0])).collect();
        assert!(steps.len() >= 10 && steps.iter().all(|&s| s < 1.03), "{ceilings:?}");
        assert!(*ceilings.last().unwrap() > limit);
        // Forgotten after 10 s: 8% again.
        run(&mut control, 100, |mbps| tick(mbps * 0.5));
        let before = control.ceiling();
        let ceilings = run(&mut control, 2, tick);
        assert_eq!(ceilings[1], (f64::from(before) * STEP) as u32);
    }

    #[test]
    fn falls_on_a_congestion_event_only_with_video_waiting() {
        let mut control = RateControl::new(CAP);
        // A loss QUIC counted, with nothing waiting: the path has room, and the ceiling rises.
        let event = |mbps| Sample { congestion_events: 1, ..tick(mbps) };
        let ceilings = run(&mut control, 4, event);
        assert!(ceilings[3] > START_BPS, "{ceilings:?}");
        // With video waiting beyond the limit: at once.
        let high = control.ceiling();
        let waiting = Sample { backlog: backlog_limit(high) + 1, ..event(f64::from(high) / 1e6) };
        assert!(control.on_sample(&waiting).is_some_and(|c| c < high));
    }

    #[test]
    fn falls_when_the_round_trip_stays_inflated() {
        let mut control = RateControl::new(CAP);
        let inflated = |mbps| Sample { rtt: 60 * MS, ..tick(mbps) };
        // One tick may be a hiccup: it only stops the rise.
        assert_eq!(control.on_sample(&tick(12.0)), None);
        assert_eq!(control.on_sample(&inflated(12.0)), None);
        assert_eq!(control.on_sample(&tick(12.0)), None);
        // Two in a row are a queue building up.
        assert_eq!(control.on_sample(&inflated(12.0)), None);
        assert!(control.on_sample(&inflated(12.0)).is_some_and(|c| near(c, DECREASE_TO * 12e6)));
        // Above 20 ms over the least, or a quarter of the least when that is more.
        let mut control = RateControl::new(CAP);
        run(&mut control, 2, |mbps| Sample { rtt: 100 * MS, ..tick(mbps) });
        let far = |mbps| Sample { rtt: 124 * MS, ..tick(mbps) };
        assert!(*run(&mut control, 4, far).last().unwrap() > START_BPS, "24 ms over 100 ms is normal");
        let mut control = RateControl::new(CAP);
        run(&mut control, 2, |mbps| Sample { rtt: 100 * MS, ..tick(mbps) });
        let farther = |mbps| Sample { rtt: 130 * MS, ..tick(mbps) };
        assert!(run(&mut control, 4, farther).iter().any(|&c| c < START_BPS), "30 ms over 100 ms is a queue");
    }

    #[test]
    fn a_round_trip_that_went_up_for_good_becomes_the_normal() {
        let mut control = RateControl::new(CAP);
        run(&mut control, 60, tick);
        assert_eq!(control.ceiling(), CAP);
        // Another route, 30 ms longer for good: taken for a queue at first ...
        let longer = |mbps| Sample { rtt: 61 * MS, ..tick(mbps) };
        let ceilings = run(&mut control, 300, longer);
        assert!(ceilings[..100].iter().any(|&c| c < CAP));
        // ... until the shorter round trips are forgotten: then back up to the LAN bitrate.
        assert_eq!(*ceilings.last().unwrap(), CAP, "{ceilings:?}");
    }

    #[test]
    fn falls_when_video_keeps_waiting() {
        let mut control = RateControl::new(CAP);
        let waiting = |mbps| Sample { backlog: backlog_limit(START_BPS) + 1, ..tick(mbps) };
        assert_eq!(control.on_sample(&waiting(12.0)), None, "a burst");
        assert!(control.on_sample(&waiting(12.0)).is_some(), "200 ms");
        // The path carries what went out: 85% of it ...
        let mut control = RateControl::new(CAP);
        let slow = |_| Sample { backlog: 1 << 20, ..tick(8.0) };
        assert!(near(run(&mut control, 2, slow)[1], DECREASE_TO * 8e6));
        // ... but at most half the ceiling at once.
        let mut control = RateControl::new(CAP);
        let slower = |_| Sample { backlog: 1 << 20, ..tick(2.0) };
        assert_eq!(run(&mut control, 2, slower)[1], START_BPS / 2);
    }

    #[test]
    fn a_stall_costs_half_the_ceiling_for_a_few_seconds() {
        let mut control = RateControl::new(CAP);
        run(&mut control, 20, tick);
        let before = control.ceiling();
        // 300 ms with nothing going out and video waiting (a Wi-Fi roam): what went out says
        // nothing about the path.
        let stall = |_| Sample { sent_bytes: 0, sent_packets: 0, backlog: 1 << 20, ..tick(0.0) };
        assert_eq!(run(&mut control, 3, stall)[2], before / 2);
        // The path is back: in 3 s, so is the ceiling.
        let ceilings = run(&mut control, 30, tick);
        assert!(ceilings[29] >= before, "{ceilings:?}");
    }

    #[test]
    fn an_idle_stream_keeps_its_ceiling() {
        let mut control = RateControl::new(CAP);
        // A still screen for 5 minutes: keep-alives and acknowledgements, 1 in 24 of them lost,
        // on a path whose round trip jumps now and then (someone else's traffic).
        for i in 0..3000 {
            let rtt = if (i / 20) % 2 == 0 { 31 * MS } else { 90 * MS };
            let s = Sample { sent_bytes: 500, sent_packets: 8, lost_packets: u64::from(i % 3 == 0), rtt, ..tick(0.0) };
            assert_eq!(control.on_sample(&s), None, "tick {i}");
        }
    }

    #[test]
    fn holds_after_a_decrease() {
        let mut control = RateControl::new(CAP);
        let lossy = |mbps| Sample { lost_packets: 10, ..tick(mbps) };
        run(&mut control, 9, lossy);
        let after = control.on_sample(&lossy(f64::from(control.ceiling()) / 1e6)).unwrap();
        // For a second, neither another decrease nor a rise.
        let ceilings = run(&mut control, 9, lossy);
        assert!(ceilings.iter().all(|&c| c == after), "{ceilings:?}");
        // Then the loss still there: down again.
        assert!(control.on_sample(&lossy(f64::from(after) / 1e6)).is_some_and(|c| c < after));
        // Gone: rising again after the hold.
        let low = control.ceiling();
        let ceilings = run(&mut control, 12, tick);
        assert!(ceilings[..10].iter().all(|&c| c == low) && ceilings[10] > low, "{ceilings:?}");
    }

    #[test]
    fn stays_between_the_floor_and_the_lan_bitrate() {
        let mut control = RateControl::new(CAP);
        let collapsed = |_| Sample { backlog: 1 << 20, ..tick(0.1) };
        let ceilings = run(&mut control, 60, collapsed);
        assert_eq!(*ceilings.last().unwrap(), FLOOR_BPS);
        // A smaller stream comes next: the ceiling goes down to its LAN bitrate at once.
        let mut control = RateControl::new(CAP);
        run(&mut control, 100, tick);
        assert_eq!(control.set_cap(20_000_000), Some(20_000_000));
        assert_eq!(control.set_cap(30_000_000), None, "and rises from there as before");
    }

    #[test]
    fn the_pace_is_high() {
        assert_eq!(pace_for(START_BPS), 150_000_000);
        assert_eq!(pace_for(40_000_000), 320_000_000);
    }

    #[test]
    fn backlog_limit_and_keyframe_gap() {
        assert_eq!(backlog_limit(FLOOR_BPS), 64 * 1024);
        assert_eq!(backlog_limit(50_000_000), 250_000);
        assert_eq!(keyframe_gap(Duration::from_millis(5)), Duration::from_millis(50));
        assert_eq!(keyframe_gap(Duration::from_millis(120)), Duration::from_millis(120));
        assert_eq!(keyframe_gap(Duration::from_secs(2)), Duration::from_millis(500));
    }
}
