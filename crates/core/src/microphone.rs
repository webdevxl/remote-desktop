//! Sharing a viewer's microphone with the Mac it views: apps on that Mac (the host) pick
//! "LanKVM Microphone" and hear the viewer's user, in a call say.
//!
//! - On the viewer, one [`Microphone`] captures this Mac's default input while any session sends
//!   it, at the input's own rate, converts it to 48 kHz and cuts it into 10 ms packets
//!   ([`MicPacket`]) that go out as datagrams: a lost one stays lost, as late audio is no use.
//! - On the host, one [`Speaker`] plays into the LanKVM Microphone driver's feed while any session
//!   sends audio, mixing every viewer that does. Each viewer's audio waits in a [`Jitter`] buffer
//!   just long enough to ride out the network's jitter: it grows after a gap, shrinks while none
//!   comes, and gains or loses a sample now and then to follow the two Macs' clocks.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use platform_mac::audio::{self, Capture, FEED_UID, MICROPHONE_UID, Playback};
use protocol::{MIC_PACKET_SAMPLES, MIC_SAMPLE_RATE, MicPacket, MicrophoneReason};
use quinn::Connection;

/// Where a core's microphone comes from, and where the microphones of the Macs viewing it go.
#[derive(Clone, Debug)]
pub struct AudioBackend {
    pub source: MicSource,
    pub sink: MicSink,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MicSource {
    /// This Mac's default input.
    System,
    /// A tone of this many hertz, made at 44.1 kHz so it goes through the rate conversion: for
    /// tests, the probe, and trying the microphone on one Mac (`LANKVM_MICROPHONE=tone[:hz]`).
    Tone(f32),
    /// None: this Mac shares no microphone (`LANKVM_MICROPHONE=off`).
    Off,
}

#[derive(Clone)]
pub enum MicSink {
    /// The LanKVM Microphone driver, when it's installed.
    System,
    /// Tests: what would be played, kept in memory.
    Record(Recording),
    /// None: Macs viewing this one can't share their microphones with it (`LANKVM_MICROPHONE=off`).
    Off,
}

impl fmt::Debug for MicSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => f.write_str("System"),
            Self::Record(recording) => write!(f, "Record({} samples)", recording.len()),
            Self::Off => f.write_str("Off"),
        }
    }
}

/// What a test sink played, at 48 kHz, in real time (silence included).
#[derive(Clone, Default)]
pub struct Recording(Arc<Mutex<Vec<f32>>>);

impl Recording {
    /// Everything played so far, which is then forgotten.
    pub fn take(&self) -> Vec<f32> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl AudioBackend {
    pub const SYSTEM: Self = Self { source: MicSource::System, sink: MicSink::System };
    pub const OFF: Self = Self { source: MicSource::Off, sink: MicSink::Off };

    pub fn from_env() -> Self {
        Self::parse(std::env::var("LANKVM_MICROPHONE").ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("off") => Self::OFF,
            Some(tone) if tone == "tone" || tone.starts_with("tone:") => {
                let hz = tone.strip_prefix("tone:").and_then(|hz| hz.parse().ok()).filter(|hz: &f32| (20.0..=20_000.0).contains(hz));
                Self { source: MicSource::Tone(hz.unwrap_or(440.0)), sink: MicSink::System }
            }
            _ => Self::SYSTEM,
        }
    }

    pub fn is_system(&self) -> bool {
        self.source == MicSource::System && matches!(self.sink, MicSink::System)
    }
}

/// The sample rate audio goes out at.
const RATE: f64 = MIC_SAMPLE_RATE as f64;
/// How often the viewer looks whether this Mac's default input changed (it then follows).
const DEVICE_CHECK: Duration = Duration::from_secs(1);
/// More audio than this (about 160 ms) waiting to be sent means the way to the host is congested:
/// what is captured meanwhile is dropped, so the host hears now rather than a growing delay.
const MAX_SEND_BACKLOG: usize = 16 * 1024;
/// The test tone's rate (most USB microphones run at 48 kHz, many others at 44.1).
const TONE_RATE: f64 = 44_100.0;

/// This Mac's microphone, shared with the hosts that take it (see [`Microphone::join`]).
pub(crate) struct Microphone {
    source: MicSource,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    links: Vec<Arc<Link>>,
    worker: Option<Worker>,
}

/// One session sending the microphone to its host.
struct Link {
    conn: Connection,
    /// Numbers its packets, from 0.
    seq: AtomicU32,
    /// The datagram send buffer's free space with nothing waiting: a viewer sends no other datagrams.
    empty_space: usize,
}

impl Link {
    fn send(&self, samples: &[i16; MIC_PACKET_SAMPLES]) {
        // Numbered even when dropped: the host sees the gap and keeps the timing.
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let backlog = self.empty_space.saturating_sub(self.conn.datagram_send_buffer_space());
        if backlog > MAX_SEND_BACKLOG {
            return;
        }
        // Fails only once the connection closes.
        let _ = self.conn.send_datagram(MicPacket::write(seq, samples).into());
    }
}

/// A session's share of the microphone: its host gets the audio while this lives.
pub(crate) struct MicGuard {
    mic: Arc<Microphone>,
    link: Arc<Link>,
}

impl Drop for MicGuard {
    fn drop(&mut self) {
        let mut inner = self.mic.inner.lock().unwrap();
        inner.links.retain(|link| !Arc::ptr_eq(link, &self.link));
        if inner.links.is_empty() {
            // Its thread lets go of the microphone (which takes a moment) on its own.
            inner.worker = None;
        }
    }
}

impl Microphone {
    pub(crate) fn new(source: MicSource) -> Option<Arc<Self>> {
        (source != MicSource::Off).then(|| Arc::new(Self { source, inner: Mutex::default() }))
    }

    /// Sends this Mac's microphone to `conn`'s host until the guard drops; the first session to
    /// do so opens the microphone. Blocks while it opens. Fails if it can't be opened.
    pub(crate) fn join(self: &Arc<Self>, conn: Connection) -> Result<MicGuard, (MicrophoneReason, String)> {
        let link = Arc::new(Link { empty_space: conn.datagram_send_buffer_space(), conn, seq: AtomicU32::new(0) });
        let mut inner = self.inner.lock().unwrap();
        if inner.worker.is_none() {
            inner.worker = Some(Worker::start(&self.source, Arc::downgrade(self))?);
        }
        inner.links.push(link.clone());
        Ok(MicGuard { mic: self.clone(), link })
    }

    fn links(&self) -> Vec<Arc<Link>> {
        self.inner.lock().unwrap().links.clone()
    }
}

/// The thread that sends what the microphone hears, while it lives.
struct Worker {
    stop: Arc<AtomicBool>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Worker {
    fn start(source: &MicSource, mic: Weak<Microphone>) -> Result<Self, (MicrophoneReason, String)> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let spawned = match source {
            MicSource::System => {
                let (tx, rx) = std_mpsc::channel::<Vec<f32>>();
                let capture = open_default_input(tx.clone())?;
                std::thread::Builder::new()
                    .name("lankvm-microphone".into())
                    .spawn(move || send_captured(capture, tx, rx, mic, thread_stop))
            }
            &MicSource::Tone(hz) => std::thread::Builder::new()
                .name("lankvm-microphone".into())
                .spawn(move || send_tone(hz, mic, thread_stop)),
            MicSource::Off => return Err((MicrophoneReason::NO_INPUT, "This copy of LanKVM shares no microphone.".into())),
        };
        spawned.map_err(|e| (MicrophoneReason::CAPTURE_FAILED, format!("Couldn't start sending this Mac's microphone: {e}")))?;
        Ok(Self { stop })
    }
}

/// Opens this Mac's default input; what it hears goes to `tx`.
fn open_default_input(tx: std_mpsc::Sender<Vec<f32>>) -> Result<Capture, (MicrophoneReason, String)> {
    let device = audio::default_input_device().ok_or((MicrophoneReason::NO_INPUT, "This Mac has no microphone.".to_string()))?;
    // Never "LanKVM Microphone" itself: it plays what other Macs send this one, which would go
    // back to them.
    if audio::device_uid(device).as_deref() == Some(MICROPHONE_UID) {
        return Err((
            MicrophoneReason::LOOPBACK,
            "This Mac's microphone is LanKVM Microphone, which plays what other Macs send here. Choose another one in System Settings → Sound → Input.".into(),
        ));
    }
    let name = audio::device_name(device).unwrap_or_default();
    let capture = Capture::start(
        device,
        Box::new(move |samples| {
            let _ = tx.send(samples.to_vec());
        }),
    )
    .map_err(|e| (MicrophoneReason::CAPTURE_FAILED, format!("Couldn't open this Mac's microphone ({name}): {e:#}")))?;
    tracing::info!(device = %name, rate = capture.sample_rate(), "microphone open");
    Ok(capture)
}

/// Sends what the microphone hears until told to stop, following this Mac's default input when it
/// changes (a headset plugged in, say).
fn send_captured(
    capture: Capture,
    tx: std_mpsc::Sender<Vec<f32>>,
    rx: std_mpsc::Receiver<Vec<f32>>,
    mic: Weak<Microphone>,
    stop: Arc<AtomicBool>,
) {
    let mut resampler = Resampler::new(capture.sample_rate());
    let mut capture = Some(capture);
    let mut packer = Packer::default();
    let mut checked = Instant::now();
    let mut resampled = Vec::new();
    while !stop.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(samples) => {
                let Some(mic) = mic.upgrade() else { break };
                resampled.clear();
                resampler.process(&samples, &mut resampled);
                let links = mic.links();
                packer.push(&resampled, |packet| links.iter().for_each(|link| link.send(packet)));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if checked.elapsed() >= DEVICE_CHECK {
            checked = Instant::now();
            let now = audio::default_input_device();
            if capture.as_ref().map(Capture::device) != now {
                // The old one first: a device can't always be opened twice.
                drop(capture.take());
                match open_default_input(tx.clone()) {
                    Ok(next) => {
                        resampler = Resampler::new(next.sample_rate());
                        capture = Some(next);
                    }
                    // Tried again at the next check; nothing goes out meanwhile.
                    Err((_, why)) => tracing::warn!("microphone: {why}"),
                }
            }
        }
    }
    // Stopping the capture waits for its callback; nothing else waits for this thread.
    drop(capture);
    tracing::info!("microphone closed");
}

fn send_tone(hz: f32, mic: Weak<Microphone>, stop: Arc<AtomicBool>) {
    let chunk = (TONE_RATE / 100.0) as usize;
    let mut resampler = Resampler::new(TONE_RATE);
    let mut packer = Packer::default();
    let mut phase = 0.0f64;
    let step = std::f64::consts::TAU * f64::from(hz) / TONE_RATE;
    let mut samples = vec![0.0f32; chunk];
    let mut resampled = Vec::new();
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let Some(mic) = mic.upgrade() else { break };
        for sample in &mut samples {
            *sample = (phase.sin() * 0.5) as f32;
            phase = (phase + step) % std::f64::consts::TAU;
        }
        resampled.clear();
        resampler.process(&samples, &mut resampled);
        let links = mic.links();
        drop(mic);
        packer.push(&resampled, |packet| links.iter().for_each(|link| link.send(packet)));
        next += Duration::from_millis(10);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

/// Converts audio of any rate to 48 kHz by linear interpolation: plenty for a voice, and no delay.
pub(crate) struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Where the next output sample falls, in input samples after `last` (0: on `last`).
    at: f64,
    /// The previous call's last sample.
    last: f32,
}

impl Resampler {
    pub(crate) fn new(input_rate: f64) -> Self {
        Self { step: input_rate / RATE, at: 1.0, last: 0.0 }
    }

    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let Some(&end) = input.last() else { return };
        let len = input.len() as f64;
        // Sample `i` of `last` followed by `input`.
        let sample = |i: usize| if i == 0 { self.last } else { input[i - 1] };
        while self.at <= len {
            let i = self.at as usize;
            let frac = (self.at - i as f64) as f32;
            let value = if frac == 0.0 { sample(i) } else { sample(i) + (sample(i + 1) - sample(i)) * frac };
            out.push(value);
            self.at += self.step;
        }
        self.at -= len;
        self.last = end;
    }
}

/// Cuts 48 kHz audio into packets of [`MIC_PACKET_SAMPLES`] 16-bit samples.
#[derive(Default)]
struct Packer {
    packet: Vec<i16>,
}

impl Packer {
    fn push(&mut self, samples: &[f32], mut send: impl FnMut(&[i16; MIC_PACKET_SAMPLES])) {
        for &sample in samples {
            self.packet.push((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16);
            if self.packet.len() == MIC_PACKET_SAMPLES {
                send(self.packet.as_slice().try_into().expect("a whole packet"));
                self.packet.clear();
            }
        }
    }
}

/// Samples at 48 kHz.
const MS: usize = MIC_SAMPLE_RATE as usize / 1000;
/// How long a viewer's audio waits before it's played, at least: on the local network, and over
/// the internet. A gap (the buffer running dry) adds [`GROW`], up to [`MAX_TARGET`]; every
/// [`CALM`] without one takes [`SHRINK`] off again.
const MIN_TARGET_LAN: usize = 20 * MS;
const MIN_TARGET_INTERNET: usize = 40 * MS;
const MAX_TARGET: usize = 200 * MS;
const GROW: usize = 20 * MS;
const SHRINK: usize = 5 * MS;
const CALM: usize = 10 * MIC_SAMPLE_RATE as usize;
/// Far enough from the target (smoothed), the buffer gains or loses one sample per packet's worth
/// played (0.2% faster or slower), so it follows the two clocks without a sound.
const SLACK: usize = 10 * MS;
/// Beyond the target by this much (the viewer's audio came in a burst after a stall), the oldest
/// is dropped down to the target at once.
const MAX_EXCESS: usize = 150 * MS;
/// Packets lost in a row that are replaced by silence, keeping the timing; after a longer gap the
/// audio just carries on.
const MAX_GAP: u32 = 10;

/// One viewer's audio, between the network and the device.
pub(crate) struct Jitter {
    queue: VecDeque<f32>,
    /// The packet expected next.
    next_seq: Option<u32>,
    /// Playing, or filling up to the target first (at the start, and after running dry).
    playing: bool,
    target: usize,
    min_target: usize,
    /// The queue's length as the device takes audio, smoothed over about half a second.
    level: f64,
    since_slide: usize,
    since_dry: usize,
}

impl Jitter {
    pub(crate) fn new(internet: bool) -> Self {
        let min_target = if internet { MIN_TARGET_INTERNET } else { MIN_TARGET_LAN };
        Self {
            queue: VecDeque::new(),
            next_seq: None,
            playing: false,
            target: min_target,
            min_target,
            level: 0.0,
            since_slide: 0,
            since_dry: 0,
        }
    }

    pub(crate) fn push(&mut self, seq: u32, samples: &[i16]) {
        if let Some(next) = self.next_seq {
            let ahead = seq.wrapping_sub(next);
            if ahead > u32::MAX / 2 {
                // Late (its place was filled with silence already) or a repeat.
                return;
            }
            if (1..=MAX_GAP).contains(&ahead) {
                self.queue.extend(std::iter::repeat_n(0.0, ahead as usize * MIC_PACKET_SAMPLES));
            }
        }
        self.next_seq = Some(seq.wrapping_add(1));
        self.queue.extend(samples.iter().map(|&s| f32::from(s) / 32768.0));
        if self.queue.len() > self.target + MAX_EXCESS {
            let excess = self.queue.len() - self.target;
            self.queue.drain(..excess);
            self.level = self.queue.len() as f64;
        }
    }

    /// Adds the next `out.len()` samples to `out` (mixing with other viewers').
    pub(crate) fn mix_into(&mut self, out: &mut [f32]) {
        let n = out.len();
        if !self.playing {
            if self.queue.len() < self.target {
                return;
            }
            self.playing = true;
            self.level = self.queue.len() as f64;
        }
        if self.queue.len() < n {
            // Ran dry: what's left, then silence until there's enough again, with more margin.
            for (o, s) in out.iter_mut().zip(self.queue.drain(..)) {
                *o += s;
            }
            self.playing = false;
            self.target = (self.target + GROW).min(MAX_TARGET);
            self.since_dry = 0;
            return;
        }
        for (o, s) in out.iter_mut().zip(self.queue.drain(..n)) {
            *o += s;
        }
        let alpha = (n as f64 / (0.5 * RATE)).min(1.0);
        self.level += (self.queue.len() as f64 - self.level) * alpha;
        self.since_dry += n;
        if self.since_dry >= CALM {
            self.since_dry = 0;
            self.target = self.target.saturating_sub(SHRINK).max(self.min_target);
        }
        self.since_slide += n;
        if self.since_slide >= MIC_PACKET_SAMPLES {
            self.since_slide = 0;
            if self.level > (self.target + SLACK) as f64 {
                self.queue.pop_front();
                self.level -= 1.0;
            } else if self.level < self.target.saturating_sub(SLACK) as f64
                && let Some(&first) = self.queue.front()
            {
                self.queue.push_front(first);
                self.level += 1.0;
            }
        }
    }

    #[cfg(test)]
    fn queued(&self) -> usize {
        self.queue.len()
    }
}

/// Microphone packets a session takes per second at most, and in a burst (after a stall): more
/// are dropped.
const PACKETS_PER_SEC: f64 = 100.0;
const PACKET_BURST: f64 = 50.0;

/// Limits how many microphone packets a viewer gets played.
pub(crate) struct PacketBudget {
    tokens: f64,
    at: Instant,
}

impl PacketBudget {
    pub(crate) fn new(now: Instant) -> Self {
        Self { tokens: PACKET_BURST, at: now }
    }

    pub(crate) fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + elapsed * PACKETS_PER_SEC * 1.2).min(PACKET_BURST);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

/// Playback that took no audio for this long has stopped (coreaudiod restarted, say): it's
/// started again.
const STALLED: Duration = Duration::from_secs(2);
/// Frames the test sink takes at a time.
const RECORD_FRAMES: usize = 240;

/// This Mac's "LanKVM Microphone": plays the microphones of the viewers that share theirs.
pub(crate) struct Speaker {
    sink: MicSink,
    /// Each playing session's audio, by session.
    mix: Arc<Mutex<HashMap<u64, Jitter>>>,
    /// When the device last took audio (ms since `epoch`).
    last_pull: Arc<AtomicU64>,
    epoch: Instant,
    /// Playing, while any session plays.
    output: Mutex<Option<Output>>,
}

/// Playback; stops when dropped (which waits for the device).
enum Output {
    Device(#[allow(dead_code)] Playback),
    Recorder(Arc<AtomicBool>),
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Self::Recorder(stop) = self {
            stop.store(true, Ordering::Release);
        }
    }
}

impl Speaker {
    pub(crate) fn new(sink: MicSink) -> Arc<Self> {
        Arc::new(Self {
            sink,
            mix: Arc::default(),
            last_pull: Arc::default(),
            epoch: Instant::now(),
            output: Mutex::new(None),
        })
    }

    /// Whether viewers' microphones can be played here: the driver is installed and loaded.
    pub(crate) fn ready(&self) -> bool {
        match self.sink {
            MicSink::System => audio::microphone_driver_ready(),
            MicSink::Record(_) => true,
            MicSink::Off => false,
        }
    }

    /// Starts playing a session's microphone (what [`Speaker::push`] gets for it). Blocks while
    /// the first one starts the device.
    pub(crate) fn open(&self, session: u64, internet: bool) -> Result<()> {
        self.mix.lock().unwrap().insert(session, Jitter::new(internet));
        let mut output = self.output.lock().unwrap();
        if output.is_none() {
            match self.start_output() {
                Ok(started) => *output = Some(started),
                Err(e) => {
                    self.mix.lock().unwrap().remove(&session);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Stops playing a session's microphone; the device stops with the last one.
    pub(crate) fn close(&self, session: u64) {
        let mut mix = self.mix.lock().unwrap();
        if mix.remove(&session).is_none() || !mix.is_empty() {
            return;
        }
        drop(mix);
        if let Some(output) = self.output.lock().unwrap().take() {
            // Stopping waits for the device: not on the caller's (async) thread.
            std::thread::spawn(move || drop(output));
        }
    }

    pub(crate) fn push(&self, session: u64, packet: &MicPacket) {
        if let Some(jitter) = self.mix.lock().unwrap().get_mut(&session) {
            jitter.push(packet.seq, &packet.samples);
        }
    }

    /// Starts the device again if it stopped taking audio. Blocks while it does.
    pub(crate) fn check(&self) {
        let mut output = self.output.lock().unwrap();
        let idle = (self.epoch.elapsed().as_millis() as u64).saturating_sub(self.last_pull.load(Ordering::Acquire));
        let stalled = idle > STALLED.as_millis() as u64;
        if output.is_none() || !stalled {
            return;
        }
        tracing::warn!("LanKVM Microphone stopped taking audio; starting it again");
        drop(output.take());
        match self.start_output() {
            Ok(started) => *output = Some(started),
            Err(e) => tracing::warn!("LanKVM Microphone: {e:#}"),
        }
    }

    fn start_output(&self) -> Result<Output> {
        self.last_pull.store(self.epoch.elapsed().as_millis() as u64, Ordering::Release);
        let (mix, last_pull, epoch) = (self.mix.clone(), self.last_pull.clone(), self.epoch);
        let fill = move |out: &mut [f32]| {
            out.fill(0.0);
            last_pull.store(epoch.elapsed().as_millis() as u64, Ordering::Release);
            for jitter in mix.lock().unwrap().values_mut() {
                jitter.mix_into(out);
            }
            for sample in out {
                *sample = sample.clamp(-1.0, 1.0);
            }
        };
        match &self.sink {
            MicSink::System => {
                let playback = Playback::start(FEED_UID, Box::new(fill))?;
                tracing::info!("playing viewers' microphones into LanKVM Microphone");
                Ok(Output::Device(playback))
            }
            MicSink::Record(recording) => Ok(Output::Recorder(record(recording.clone(), fill))),
            MicSink::Off => bail!("this copy of LanKVM plays no microphones"),
        }
    }
}

/// The test sink: takes audio in real time, as the device would, into `recording`.
fn record(recording: Recording, mut fill: impl FnMut(&mut [f32]) + Send + 'static) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    std::thread::spawn(move || {
        let period = Duration::from_secs_f64(RECORD_FRAMES as f64 / RATE);
        let mut buffer = [0.0f32; RECORD_FRAMES];
        let mut next = Instant::now();
        while !thread_stop.load(Ordering::Acquire) {
            fill(&mut buffer);
            recording.0.lock().unwrap().extend_from_slice(&buffer);
            next += period;
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    stop
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: f64, hz: f64, len: usize, from: usize) -> Vec<f32> {
        (from..from + len).map(|i| (std::f64::consts::TAU * hz * i as f64 / rate).sin() as f32 * 0.5).collect()
    }

    /// The strongest frequency in `samples` (48 kHz), by counting rising zero crossings.
    fn frequency(samples: &[f32]) -> f64 {
        let crossings = samples.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        crossings as f64 * RATE / samples.len() as f64
    }

    #[test]
    fn backends_from_the_environment() {
        assert!(AudioBackend::parse(None).is_system());
        assert!(AudioBackend::parse(Some("")).is_system());
        let off = AudioBackend::parse(Some("off"));
        assert_eq!(off.source, MicSource::Off);
        assert!(matches!(off.sink, MicSink::Off));
        let tone = AudioBackend::parse(Some("tone"));
        assert_eq!(tone.source, MicSource::Tone(440.0));
        assert!(matches!(tone.sink, MicSink::System), "a tone still plays into the driver");
        assert_eq!(AudioBackend::parse(Some("tone:1000")).source, MicSource::Tone(1000.0));
        assert_eq!(AudioBackend::parse(Some("tone:5")).source, MicSource::Tone(440.0), "out of range");
    }

    #[test]
    fn resampling_keeps_pitch_and_length() {
        for rate in [44_100.0, 48_000.0, 96_000.0, 16_000.0] {
            let mut resampler = Resampler::new(rate);
            let mut out = Vec::new();
            let mut fed = 0;
            // Uneven slices, as a device delivers them.
            for len in [441, 512, 100, 7, 1024].iter().cycle().take(200) {
                resampler.process(&tone(rate, 1000.0, *len, fed), &mut out);
                fed += len;
            }
            // What falls after the last input sample waits for the next slice: up to one input
            // sample's worth.
            let expected = fed as f64 * RATE / rate;
            let pending = RATE / rate + 1.0;
            assert!((out.len() as f64 - expected).abs() <= pending, "{rate} Hz: {} samples, expected {expected}", out.len());
            let hz = frequency(&out);
            assert!((hz - 1000.0).abs() < 5.0, "{rate} Hz: a 1 kHz tone came out at {hz} Hz");
        }
    }

    #[test]
    fn at_48k_resampling_is_a_copy() {
        let input = tone(RATE, 440.0, 1000, 0);
        let mut out = Vec::new();
        let mut resampler = Resampler::new(RATE);
        resampler.process(&input[..300], &mut out);
        resampler.process(&input[300..], &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn packets_are_whole_and_in_order() {
        let mut packer = Packer::default();
        let mut packets = Vec::new();
        let input: Vec<f32> = (0..1500).map(|i| i as f32 / 2000.0).collect();
        packer.push(&input[..700], |p| packets.push(*p));
        packer.push(&input[700..], |p| packets.push(*p));
        assert_eq!(packets.len(), 3, "1500 samples: three whole packets, 60 waiting");
        assert_eq!(packets[1][0], (480.0f32 / 2000.0 * 32767.0).round() as i16);
        let mut loud = Vec::new();
        Packer::default().push(&[2.0; MIC_PACKET_SAMPLES], |p| loud.push(*p));
        assert_eq!(loud[0][0], 32767, "clipped, not wrapped");
    }

    fn packet(value: i16) -> Vec<i16> {
        vec![value; MIC_PACKET_SAMPLES]
    }

    fn pull(jitter: &mut Jitter, n: usize) -> Vec<f32> {
        let mut out = vec![0.0; n];
        jitter.mix_into(&mut out);
        out
    }

    #[test]
    fn jitter_waits_for_its_target_then_plays_in_order() {
        let mut jitter = Jitter::new(false);
        jitter.push(0, &packet(1000));
        assert!(pull(&mut jitter, 256).iter().all(|&s| s == 0.0), "not before the target");
        jitter.push(1, &packet(2000));
        // Late and repeated packets are dropped.
        jitter.push(0, &packet(9999));
        let out = pull(&mut jitter, 960);
        assert!(out[..480].iter().all(|&s| s == 1000.0 / 32768.0));
        assert!(out[480..].iter().all(|&s| s == 2000.0 / 32768.0));
    }

    #[test]
    fn lost_packets_become_silence_of_their_length() {
        let mut jitter = Jitter::new(false);
        jitter.push(0, &packet(1000));
        jitter.push(3, &packet(1000));
        assert_eq!(jitter.queued(), 4 * MIC_PACKET_SAMPLES, "two lost: their time is kept");
        let out = pull(&mut jitter, 4 * MIC_PACKET_SAMPLES);
        assert!(out[480..1440].iter().all(|&s| s == 0.0));
        // After a long gap the audio just carries on.
        jitter.push(100, &packet(1000));
        assert_eq!(jitter.queued(), MIC_PACKET_SAMPLES);
    }

    #[test]
    fn running_dry_waits_for_more_margin() {
        let mut jitter = Jitter::new(false);
        jitter.push(0, &packet(1000));
        jitter.push(1, &packet(1000));
        let before = jitter.target;
        pull(&mut jitter, 960);
        pull(&mut jitter, 256);
        assert_eq!(jitter.target, before + GROW, "ran dry");
        jitter.push(2, &packet(1000));
        jitter.push(3, &packet(1000));
        assert!(pull(&mut jitter, 256).iter().all(|&s| s == 0.0), "waits for the new target");
    }

    #[test]
    fn a_burst_after_a_stall_is_cut_to_the_target() {
        let mut jitter = Jitter::new(false);
        for seq in 0..100 {
            jitter.push(seq, &packet(1000));
        }
        assert!(jitter.queued() <= jitter.target + MAX_EXCESS);
    }

    /// The viewer's clock runs 0.1% fast against the host's: the buffer stays near its target
    /// instead of growing without end (or running dry, for a slow one).
    #[test]
    fn jitter_follows_clock_drift() {
        for drift in [1.001, 0.999] {
            let mut jitter = Jitter::new(false);
            let mut seq = 0;
            let mut sent = 0.0f64;
            let mut played = 0usize;
            // Two minutes, 256 frames at a time.
            while played < 120 * MIC_SAMPLE_RATE as usize {
                pull(&mut jitter, 256);
                played += 256;
                while sent < played as f64 * drift + jitter.target as f64 {
                    jitter.push(seq, &packet(1000));
                    seq += 1;
                    sent += MIC_PACKET_SAMPLES as f64;
                }
            }
            let target = jitter.target;
            assert!(
                jitter.queued().abs_diff(target) <= SLACK + 2 * MIC_PACKET_SAMPLES,
                "drift {drift}: {} queued, target {target}",
                jitter.queued()
            );
            assert_eq!(target, MIN_TARGET_LAN, "drift {drift}: never ran dry");
        }
    }

    #[test]
    fn packet_budget_allows_a_burst_then_the_rate() {
        let start = Instant::now();
        let mut budget = PacketBudget::new(start);
        let burst = (0..200).filter(|_| budget.take(start)).count();
        assert_eq!(burst, PACKET_BURST as usize);
        let later = start + Duration::from_secs(1);
        let refilled = (0..200).filter(|_| budget.take(later)).count();
        assert_eq!(refilled, PACKET_BURST as usize, "a second refills the burst (capped)");
        let soon = later + Duration::from_millis(100);
        assert_eq!((0..200).filter(|_| budget.take(soon)).count(), 12, "100 ms: 12 packets at 120/s");
    }

    #[test]
    fn the_speaker_mixes_open_sessions_only() {
        let recording = Recording::default();
        let speaker = Speaker::new(MicSink::Record(recording.clone()));
        assert!(speaker.ready());
        speaker.open(1, false).unwrap();
        speaker.open(2, false).unwrap();
        for seq in 0..50 {
            speaker.push(1, &MicPacket { seq, samples: vec![1000; MIC_PACKET_SAMPLES] });
            speaker.push(2, &MicPacket { seq, samples: vec![2000; MIC_PACKET_SAMPLES] });
            speaker.push(3, &MicPacket { seq, samples: vec![9000; MIC_PACKET_SAMPLES] });
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !recording.0.lock().unwrap().iter().any(|&s| s != 0.0) {
            assert!(Instant::now() < deadline, "nothing played");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        speaker.close(1);
        speaker.close(2);
        let played = recording.take();
        let mixed = 3000.0 / 32768.0;
        assert!(played.iter().any(|&s| (s - mixed).abs() < 1e-6), "both sessions, mixed");
        assert!(played.iter().all(|&s| s == 0.0 || (s - mixed).abs() < 1e-6), "and nothing of session 3");
        assert!(speaker.output.lock().unwrap().is_none(), "stopped with the last session");
        assert!(!Speaker::new(MicSink::Off).ready());
    }
}
