//! Audio: this Mac's microphone, captured for another Mac, and playing what another Mac's
//! microphone hears into "LanKVM Microphone", the input the LanKVM Microphone driver
//! (macos/AudioDriver) adds, so apps here can use it.
//!
//! Both go through the HAL output unit (AUHAL) as 32-bit float mono. The driver takes what is
//! played into its hidden "feed" device and plays it out of the microphone apps see.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use anyhow::{Context, Result, bail};
use objc2_core_foundation::{CFRetained, CFString};

/// The UID of the input apps pick, "LanKVM Microphone".
pub const MICROPHONE_UID: &str = "dev.lankvm.microphone";
/// The UID of the hidden output LanKVM plays into; the driver plays it out of the microphone.
pub const FEED_UID: &str = "dev.lankvm.microphone.feed";
/// The driver's one sample rate.
pub const FEED_SAMPLE_RATE: f64 = 48_000.0;
/// Frames per IO cycle LanKVM asks for, at either end: about 5 ms at 48 kHz.
const IO_FRAMES: u32 = 256;
/// Most frames a capture delivers at once (AUHAL's default slice limit is 4096).
const MAX_CAPTURE_FRAMES: usize = 8192;

type AudioObjectId = u32;
type OsStatus = i32;
type AudioUnit = *mut c_void;

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const SYSTEM_OBJECT: AudioObjectId = 1;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const ELEMENT_MAIN: u32 = 0;
const HARDWARE_DEFAULT_INPUT: u32 = fourcc(b"dIn ");
const HARDWARE_TRANSLATE_UID: u32 = fourcc(b"uidd");
const DEVICE_UID: u32 = fourcc(b"uid ");
const OBJECT_NAME: u32 = fourcc(b"lnam");
const DEVICE_IS_ALIVE: u32 = fourcc(b"livn");
const DEVICE_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");

const UNIT_TYPE_OUTPUT: u32 = fourcc(b"auou");
const UNIT_SUBTYPE_HAL: u32 = fourcc(b"ahal");
const MANUFACTURER_APPLE: u32 = fourcc(b"appl");
const UNIT_SCOPE_GLOBAL: u32 = 0;
const UNIT_SCOPE_INPUT: u32 = 1;
const UNIT_SCOPE_OUTPUT: u32 = 2;
const UNIT_STREAM_FORMAT: u32 = 8;
const UNIT_SET_RENDER_CALLBACK: u32 = 23;
const OUTPUT_CURRENT_DEVICE: u32 = 2000;
const OUTPUT_ENABLE_IO: u32 = 2003;
const OUTPUT_SET_INPUT_CALLBACK: u32 = 2005;
/// AUHAL's elements: 0 plays to the device, 1 records from it.
const OUTPUT_ELEMENT: u32 = 0;
const INPUT_ELEMENT: u32 = 1;

const FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
const FORMAT_FLAG_FLOAT: u32 = 1;
const FORMAT_FLAG_PACKED: u32 = 8;

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
#[derive(Default)]
struct StreamDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

impl StreamDescription {
    fn float_mono(sample_rate: f64) -> Self {
        Self {
            sample_rate,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: FORMAT_FLAG_FLOAT | FORMAT_FLAG_PACKED,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels_per_frame: 1,
            bits_per_channel: 32,
            reserved: 0,
        }
    }
}

#[repr(C)]
struct ComponentDescription {
    kind: u32,
    sub_type: u32,
    manufacturer: u32,
    flags: u32,
    flags_mask: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

type RenderCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    flags: *mut u32,
    time: *const c_void,
    bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OsStatus;

#[repr(C)]
struct RenderCallbackStruct {
    callback: RenderCallback,
    refcon: *mut c_void,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: AudioObjectId,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(after: *mut c_void, description: *const ComponentDescription) -> *mut c_void;
    fn AudioComponentInstanceNew(component: *mut c_void, out: *mut AudioUnit) -> OsStatus;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> OsStatus;
    fn AudioUnitSetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *const c_void, size: u32) -> OsStatus;
    fn AudioUnitGetProperty(unit: AudioUnit, id: u32, scope: u32, element: u32, data: *mut c_void, size: *mut u32) -> OsStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OsStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OsStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OsStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OsStatus;
    fn AudioUnitRender(
        unit: AudioUnit,
        flags: *mut u32,
        time: *const c_void,
        bus: u32,
        frames: u32,
        data: *mut AudioBufferList,
    ) -> OsStatus;
}

fn check(status: OsStatus, what: &str) -> Result<()> {
    if status != 0 {
        let code = status as u32;
        let text: String = code.to_be_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect();
        bail!("{what}: OSStatus {status} ('{text}')");
    }
    Ok(())
}

fn global(selector: u32) -> PropertyAddress {
    PropertyAddress { selector, scope: SCOPE_GLOBAL, element: ELEMENT_MAIN }
}

fn get_u32(object: AudioObjectId, selector: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status =
        unsafe { AudioObjectGetPropertyData(object, &global(selector), 0, ptr::null(), &mut size, (&raw mut value).cast()) };
    (status == 0).then_some(value)
}

fn get_string(object: AudioObjectId, selector: u32) -> Option<String> {
    let mut value: *const CFString = ptr::null();
    let mut size = size_of::<*const CFString>() as u32;
    let status =
        unsafe { AudioObjectGetPropertyData(object, &global(selector), 0, ptr::null(), &mut size, (&raw mut value).cast()) };
    let value = NonNull::new(value.cast_mut()).filter(|_| status == 0)?;
    // The caller owns what the HAL returns.
    Some(unsafe { CFRetained::from_raw(value) }.to_string())
}

/// The device with `uid`, hidden ones included (the feed is), if there is one.
pub fn device_with_uid(uid: &str) -> Option<AudioObjectId> {
    let uid = CFString::from_str(uid);
    let qualifier: *const CFString = &*uid;
    let mut device: AudioObjectId = 0;
    let mut size = size_of::<AudioObjectId>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &global(HARDWARE_TRANSLATE_UID),
            size_of::<*const CFString>() as u32,
            (&raw const qualifier).cast(),
            &mut size,
            (&raw mut device).cast(),
        )
    };
    (status == 0 && device != 0).then_some(device)
}

/// Whether the LanKVM Microphone driver is installed and running (coreaudiod loaded it): its feed
/// exists.
pub fn microphone_driver_ready() -> bool {
    device_with_uid(FEED_UID).is_some_and(|device| get_u32(device, DEVICE_IS_ALIVE) != Some(0))
}

/// This Mac's default input (the microphone apps use unless told otherwise), if any.
pub fn default_input_device() -> Option<AudioObjectId> {
    get_u32(SYSTEM_OBJECT, HARDWARE_DEFAULT_INPUT).filter(|&device| device != 0)
}

pub fn device_uid(device: AudioObjectId) -> Option<String> {
    get_string(device, DEVICE_UID)
}

pub fn device_name(device: AudioObjectId) -> Option<String> {
    get_string(device, OBJECT_NAME)
}

/// A HAL output unit, disposed of when dropped (stopped first if it runs).
struct Unit(AudioUnit);

// SAFETY: an AudioUnit may be configured, started and stopped from any thread, one at a time.
unsafe impl Send for Unit {}

impl Unit {
    fn new() -> Result<Self> {
        let description = ComponentDescription {
            kind: UNIT_TYPE_OUTPUT,
            sub_type: UNIT_SUBTYPE_HAL,
            manufacturer: MANUFACTURER_APPLE,
            flags: 0,
            flags_mask: 0,
        };
        let component = unsafe { AudioComponentFindNext(ptr::null_mut(), &description) };
        if component.is_null() {
            bail!("no HAL output unit");
        }
        let mut unit: AudioUnit = ptr::null_mut();
        check(unsafe { AudioComponentInstanceNew(component, &mut unit) }, "make a HAL output unit")?;
        Ok(Self(unit))
    }

    fn set<T>(&self, id: u32, scope: u32, element: u32, value: &T, what: &str) -> Result<()> {
        let status = unsafe { AudioUnitSetProperty(self.0, id, scope, element, (value as *const T).cast(), size_of::<T>() as u32) };
        check(status, what)
    }

    fn get<T: Default>(&self, id: u32, scope: u32, element: u32, what: &str) -> Result<T> {
        let mut value = T::default();
        let mut size = size_of::<T>() as u32;
        check(unsafe { AudioUnitGetProperty(self.0, id, scope, element, (&raw mut value).cast(), &mut size) }, what)?;
        Ok(value)
    }

    fn start(&self) -> Result<()> {
        check(unsafe { AudioUnitInitialize(self.0) }, "initialize the audio unit")?;
        check(unsafe { AudioOutputUnitStart(self.0) }, "start the audio unit")
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        // Stopping waits for a callback in progress: none runs after this.
        unsafe {
            AudioOutputUnitStop(self.0);
            AudioUnitUninitialize(self.0);
            AudioComponentInstanceDispose(self.0);
        }
    }
}

/// What a capture's callback needs: its unit (to render the input into the buffer) and where the
/// audio goes.
struct CaptureCtx {
    unit: AudioUnit,
    buffer: Vec<f32>,
    deliver: Box<dyn FnMut(&[f32]) + Send>,
}

/// This Mac's microphone (an input device), delivering mono audio at the device's rate until
/// dropped.
pub struct Capture {
    /// Taken (stopped and disposed of) before `ctx` is freed: no callback runs once it's gone.
    unit: Option<Unit>,
    ctx: *mut CaptureCtx,
    device: AudioObjectId,
    sample_rate: f64,
}

// SAFETY: the context is only touched by the unit's callback, which has stopped when `ctx` is freed.
unsafe impl Send for Capture {}

impl Capture {
    /// Starts capturing `device`. `deliver` gets each slice of audio, mono, at
    /// [`sample_rate`](Self::sample_rate), on CoreAudio's IO thread: it must return quickly.
    /// Without the Microphone permission macOS delivers silence.
    pub fn start(device: AudioObjectId, deliver: Box<dyn FnMut(&[f32]) + Send>) -> Result<Self> {
        let unit = Unit::new()?;
        unit.set(OUTPUT_ENABLE_IO, UNIT_SCOPE_INPUT, INPUT_ELEMENT, &1u32, "enable input")?;
        unit.set(OUTPUT_ENABLE_IO, UNIT_SCOPE_OUTPUT, OUTPUT_ELEMENT, &0u32, "disable output")?;
        unit.set(OUTPUT_CURRENT_DEVICE, UNIT_SCOPE_GLOBAL, OUTPUT_ELEMENT, &device, "choose the microphone")?;
        // AUHAL converts formats and channels on input, not the sample rate: take the device's.
        let hardware: StreamDescription = unit.get(UNIT_STREAM_FORMAT, UNIT_SCOPE_INPUT, INPUT_ELEMENT, "the microphone's format")?;
        if !(hardware.sample_rate >= 8_000.0 && hardware.sample_rate <= 384_000.0) {
            bail!("the microphone runs at {} Hz", hardware.sample_rate);
        }
        let format = StreamDescription::float_mono(hardware.sample_rate);
        unit.set(UNIT_STREAM_FORMAT, UNIT_SCOPE_OUTPUT, INPUT_ELEMENT, &format, "set the capture format")?;
        // Best effort: smaller slices, sooner.
        let _ = unit.set(DEVICE_BUFFER_FRAME_SIZE, UNIT_SCOPE_GLOBAL, OUTPUT_ELEMENT, &IO_FRAMES, "IO buffer size");
        let ctx = Box::into_raw(Box::new(CaptureCtx { unit: unit.0, buffer: vec![0.0; MAX_CAPTURE_FRAMES], deliver }));
        let callback = RenderCallbackStruct { callback: capture_callback, refcon: ctx.cast() };
        let capture = Self { unit: Some(unit), ctx, device, sample_rate: hardware.sample_rate };
        let unit = capture.unit.as_ref().expect("just made");
        unit.set(OUTPUT_SET_INPUT_CALLBACK, UNIT_SCOPE_GLOBAL, OUTPUT_ELEMENT, &callback, "set the capture callback")?;
        unit.start()?;
        Ok(capture)
    }

    pub fn device(&self) -> AudioObjectId {
        self.device
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Stop the unit before freeing what its callback uses.
        drop(self.unit.take());
        drop(unsafe { Box::from_raw(self.ctx) });
    }
}

unsafe extern "C" fn capture_callback(
    refcon: *mut c_void,
    flags: *mut u32,
    time: *const c_void,
    bus: u32,
    frames: u32,
    _data: *mut AudioBufferList,
) -> OsStatus {
    let ctx = unsafe { &mut *refcon.cast::<CaptureCtx>() };
    let frames = (frames as usize).min(ctx.buffer.len());
    let mut list = AudioBufferList {
        count: 1,
        buffers: [AudioBuffer { channels: 1, byte_size: (frames * 4) as u32, data: ctx.buffer.as_mut_ptr().cast() }],
    };
    let status = unsafe { AudioUnitRender(ctx.unit, flags, time, bus, frames as u32, &mut list) };
    if status == 0 {
        let got = (list.buffers[0].byte_size as usize / 4).min(frames);
        (ctx.deliver)(&ctx.buffer[..got]);
    }
    status
}

/// Audio played into an output device (the LanKVM Microphone feed) at 48 kHz mono, pulled from
/// `fill` until dropped.
pub struct Playback {
    /// As for `Capture`.
    unit: Option<Unit>,
    ctx: *mut Box<dyn FnMut(&mut [f32]) + Send>,
}

// SAFETY: as for `Capture`.
unsafe impl Send for Playback {}

impl Playback {
    /// Plays into the device with `uid`. `fill` fills each buffer the device asks for, on
    /// CoreAudio's IO thread: it must return quickly.
    pub fn start(uid: &str, fill: Box<dyn FnMut(&mut [f32]) + Send>) -> Result<Self> {
        let device = device_with_uid(uid).with_context(|| format!("no audio device {uid}"))?;
        let unit = Unit::new()?;
        unit.set(OUTPUT_CURRENT_DEVICE, UNIT_SCOPE_GLOBAL, OUTPUT_ELEMENT, &device, "choose the output")?;
        let format = StreamDescription::float_mono(FEED_SAMPLE_RATE);
        unit.set(UNIT_STREAM_FORMAT, UNIT_SCOPE_INPUT, OUTPUT_ELEMENT, &format, "set the playback format")?;
        let _ = unit.set(DEVICE_BUFFER_FRAME_SIZE, UNIT_SCOPE_GLOBAL, OUTPUT_ELEMENT, &IO_FRAMES, "IO buffer size");
        let ctx = Box::into_raw(Box::new(fill));
        let callback = RenderCallbackStruct { callback: playback_callback, refcon: ctx.cast() };
        let playback = Self { unit: Some(unit), ctx };
        let unit = playback.unit.as_ref().expect("just made");
        unit.set(UNIT_SET_RENDER_CALLBACK, UNIT_SCOPE_INPUT, OUTPUT_ELEMENT, &callback, "set the playback callback")?;
        unit.start()?;
        Ok(playback)
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        drop(self.unit.take());
        drop(unsafe { Box::from_raw(self.ctx) });
    }
}

unsafe extern "C" fn playback_callback(
    refcon: *mut c_void,
    _flags: *mut u32,
    _time: *const c_void,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OsStatus {
    let fill = unsafe { &mut *refcon.cast::<Box<dyn FnMut(&mut [f32]) + Send>>() };
    let Some(list) = (unsafe { data.as_mut() }) else { return 0 };
    if list.count == 0 || list.buffers[0].data.is_null() {
        return 0;
    }
    let buffer = &mut list.buffers[0];
    let len = (buffer.byte_size as usize / 4).min(frames as usize);
    let out = unsafe { std::slice::from_raw_parts_mut(buffer.data.cast::<f32>(), len) };
    fill(out);
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");

    #[test]
    fn unknown_devices_are_none() {
        assert_eq!(device_with_uid("dev.lankvm.no-such-device"), None);
        assert!(Playback::start("dev.lankvm.no-such-device", Box::new(|_| {})).is_err());
    }

    #[test]
    fn default_devices_have_uids() {
        // A Mac without any input (a Mac mini with nothing plugged in) has no default input.
        if let Some(device) = default_input_device() {
            assert!(device_uid(device).is_some_and(|uid| !uid.is_empty()));
            assert!(device_name(device).is_some());
        }
    }

    /// Plays silence into this Mac's speakers for a moment: the callback has to be asked for audio.
    #[test]
    fn playback_pulls_audio() {
        let Some(output) = get_u32(SYSTEM_OBJECT, DEFAULT_OUTPUT).filter(|&d| d != 0) else { return };
        let uid = device_uid(output).expect("the default output has a UID");
        let pulled = Arc::new(AtomicUsize::new(0));
        let counter = pulled.clone();
        let playback = Playback::start(
            &uid,
            Box::new(move |out| {
                out.fill(0.0);
                counter.fetch_add(out.len(), Ordering::Relaxed);
            }),
        )
        .expect("play into the default output");
        let deadline = Instant::now() + Duration::from_secs(2);
        while pulled.load(Ordering::Relaxed) < 4800 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(playback);
        assert!(pulled.load(Ordering::Relaxed) >= 4800, "pulled {} frames", pulled.load(Ordering::Relaxed));
    }

    /// Records `seconds` from `device`, at 48 kHz (what the devices here run at; checked).
    fn record(device: AudioObjectId, seconds: f64) -> Vec<f32> {
        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = heard.clone();
        let capture = Capture::start(device, Box::new(move |samples| sink.lock().unwrap().extend_from_slice(samples)))
            .expect("open the input");
        assert_eq!(capture.sample_rate(), FEED_SAMPLE_RATE, "this test expects a 48 kHz input");
        std::thread::sleep(Duration::from_secs_f64(seconds));
        drop(capture);
        Arc::try_unwrap(heard).unwrap().into_inner().unwrap()
    }

    /// This Mac's microphone: audio arrives (silence without the Microphone permission). Needs the
    /// permission for whatever runs the test (macOS may ask):
    /// `cargo test -p platform-mac --lib audio -- --ignored capture`
    #[test]
    #[ignore = "uses the microphone"]
    fn capture_hears_the_default_input() {
        let device = default_input_device().expect("a microphone");
        let heard = record(device, 1.0);
        assert!(heard.len() > 40_000, "{} samples in a second", heard.len());
        let peak = heard.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        println!("{}: {} samples, peak {peak}", device_name(device).unwrap_or_default(), heard.len());
    }

    /// With LanKVM Microphone installed: a tone played into the feed comes out of the microphone.
    /// Needs the Microphone permission too:
    /// `cargo test -p platform-mac --lib audio -- --ignored driver`
    #[test]
    #[ignore = "needs LanKVM Microphone installed, and the microphone permission"]
    fn the_driver_plays_the_feed_out_of_the_microphone() {
        assert!(microphone_driver_ready(), "LanKVM Microphone isn't installed (./scripts/install-audio-driver.sh)");
        let microphone = device_with_uid(MICROPHONE_UID).expect("the microphone");
        let mut phase = 0.0f64;
        let playback = Playback::start(
            FEED_UID,
            Box::new(move |out| {
                for sample in out {
                    *sample = (phase.sin() * 0.5) as f32;
                    phase = (phase + std::f64::consts::TAU * 1000.0 / FEED_SAMPLE_RATE) % std::f64::consts::TAU;
                }
            }),
        )
        .expect("play into the feed");
        let heard = record(microphone, 1.0);
        drop(playback);
        let tone: Vec<f32> = heard.into_iter().skip_while(|&s| s == 0.0).collect();
        assert!(tone.len() > 24_000, "only {} samples of audio came out", tone.len());
        let crossings = tone.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let hz = crossings as f64 * FEED_SAMPLE_RATE / tone.len() as f64;
        assert!((hz - 1000.0).abs() < 10.0, "came out at {hz} Hz");
        let peak = tone.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((0.45..=0.55).contains(&peak), "peak {peak}");
        // And nothing once the feed stops.
        assert!(record(microphone, 0.3).iter().all(|&s| s == 0.0), "the microphone still plays");
    }
}
