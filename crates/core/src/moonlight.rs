//! Moonlight as LanKVM's other streaming engine, on the viewer side.
//!
//! Moonlight (GPL-3.0) is installed separately and runs as a separate process; LanKVM never links
//! or bundles it. While a session streams with the host's Sunshine, a runner task here pairs
//! Moonlight with that Sunshine (once: Moonlight shows a PIN, LanKVM hands it to the host over its
//! own paired connection) and keeps a Moonlight window streaming it. LanKVM's session stays up for
//! displays and control; its own video stops meanwhile.
//!
//! Every Moonlight process LanKVM starts is killed when the runner lets go of it, and the app's
//! shutdown waits for (or kills) any still running: none is ever left behind.

use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::{ClientMsg, SunshineInfo};
use tokio::sync::mpsc;

use crate::client::{SessionEvent, SessionEvents};

/// `Moonlight list` (are we paired, and can we reach it?) gets this long.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// After the host accepted the PIN, until `Moonlight list` says paired.
const PAIRED_WAIT: Duration = Duration::from_secs(15);
/// The host waits 30 s for Moonlight's request, then up to 40 s for the handshake.
const PAIR_ANSWER_WAIT: Duration = Duration::from_secs(80);
/// Once the host ended the stream, Moonlight gets this long to quit by itself (writing its
/// stats) before it is killed.
const EXIT_WAIT: Duration = Duration::from_secs(3);
/// Without a log to tell, a Moonlight that runs this long counts as streaming.
const STREAMING_AFTER: Duration = Duration::from_secs(2);
/// How often the runner looks at Moonlight.
const WATCH_INTERVAL: Duration = Duration::from_millis(100);
/// A new generation this soon after Moonlight ended (or failed) opens it again: the host's
/// Sunshine starting again (another display, another input setting) is what ended it. The host
/// ends the stream first (Moonlight quits within a second), stops Sunshine (up to ~6 s), starts
/// it again (up to 15 s until it answers) and revokes stale pairings before it says so.
const RESTART_GRACE: Duration = Duration::from_secs(30);

/// Where Moonlight may be: the app (in /Applications or ~/Applications), Homebrew's link to it.
fn candidates() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/Applications/Moonlight.app/Contents/MacOS/Moonlight")];
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join("Applications/Moonlight.app/Contents/MacOS/Moonlight"));
    }
    paths.push(PathBuf::from("/opt/homebrew/bin/moonlight"));
    paths
}

/// Moonlight's binary: `$LANKVM_MOONLIGHT_BIN` (only it, when set), or the first of the usual
/// places that has one. None: not installed.
pub fn binary() -> Option<PathBuf> {
    if let Some(bin) = std::env::var_os("LANKVM_MOONLIGHT_BIN").filter(|b| !b.is_empty()) {
        let bin = PathBuf::from(bin);
        return bin.is_file().then_some(bin);
    }
    candidates().into_iter().find(|p| p.is_file())
}

/// The message when this Mac has no Moonlight.
pub(crate) fn not_installed() -> String {
    "Moonlight isn't installed on this Mac. Run LanKVM's installer (./install.sh) or `brew install --cask moonlight`.".into()
}

/// How Moonlight names the host: its address and Sunshine's HTTP port ("192.168.1.20:48989",
/// "[fd00::2]:48989").
pub(crate) fn host_string(ip: IpAddr, port: u16) -> String {
    SocketAddr::new(ip.to_canonical(), port).to_string()
}

/// Moonlight's window: a window, or full screen (system keys like ⌘Tab then go to the host).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayMode {
    Windowed,
    Fullscreen,
}

/// How Moonlight 6.1 draws: its Metal renderer (the default), or AVSampleBufferDisplayLayer
/// (`VT_FORCE_METAL=0`), which avoids the Metal renderer's trouble on macOS 26.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renderer {
    Default,
    AvSampleBuffer,
}

/// How to run Moonlight for a session.
#[derive(Clone, Debug, PartialEq)]
pub struct MoonlightOptions {
    pub display_mode: DisplayMode,
    /// None: Moonlight's own setting.
    pub vsync: Option<bool>,
    pub renderer: Renderer,
    /// None: like LanKVM's own stream (see [`default_bitrate_kbps`]).
    pub bitrate_kbps: Option<u32>,
    /// Appended to Moonlight's arguments.
    pub extra_args: Vec<String>,
}

impl MoonlightOptions {
    /// The defaults, in a window or full screen, with the test overrides from the environment:
    /// `LANKVM_MOONLIGHT_RENDERER=metal|avsample`, `LANKVM_MOONLIGHT_VSYNC=0|1`,
    /// `LANKVM_MOONLIGHT_BITRATE=<kbps>`, `LANKVM_MOONLIGHT_ARGS="..."` (appended).
    pub fn new(fullscreen: bool) -> Self {
        Self::with_overrides(fullscreen, |name| std::env::var(name).ok())
    }

    fn with_overrides(fullscreen: bool, var: impl Fn(&str) -> Option<String>) -> Self {
        let mut options = Self {
            display_mode: if fullscreen { DisplayMode::Fullscreen } else { DisplayMode::Windowed },
            vsync: None,
            renderer: Renderer::Default,
            bitrate_kbps: None,
            extra_args: Vec::new(),
        };
        match var("LANKVM_MOONLIGHT_RENDERER").as_deref().map(str::trim) {
            Some("avsample") => options.renderer = Renderer::AvSampleBuffer,
            Some("metal") | Some("") | None => {}
            Some(other) => tracing::warn!("ignoring LANKVM_MOONLIGHT_RENDERER={other:?}: expected metal or avsample"),
        }
        match var("LANKVM_MOONLIGHT_VSYNC").as_deref().map(str::trim) {
            Some("1") => options.vsync = Some(true),
            Some("0") => options.vsync = Some(false),
            Some("") | None => {}
            Some(other) => tracing::warn!("ignoring LANKVM_MOONLIGHT_VSYNC={other:?}: expected 0 or 1"),
        }
        if let Some(v) = var("LANKVM_MOONLIGHT_BITRATE").filter(|v| !v.trim().is_empty()) {
            match v.trim().parse::<u32>() {
                Ok(kbps) if kbps > 0 => options.bitrate_kbps = Some(kbps),
                _ => tracing::warn!("ignoring LANKVM_MOONLIGHT_BITRATE={v:?}: expected kbit/s"),
            }
        }
        if let Some(args) = var("LANKVM_MOONLIGHT_ARGS") {
            options.extra_args = args.split_whitespace().map(String::from).collect();
        }
        options
    }
}

/// Like LanKVM's own stream: about 0.12 bits per pixel per frame at up to 60 fps (the budget
/// doesn't double at 120), 8 to 150 Mbit/s.
pub(crate) fn default_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    let bps = f64::from(width) * f64::from(height) * f64::from(fps.min(60)) * 0.12;
    (bps.clamp(8e6, 150e6) / 1000.0) as u32
}

/// `Moonlight stream`'s arguments for Sunshine's `info` at `host`. On the local network (`lan`)
/// packets are as big as LAN streaming allows (which also keeps Moonlight in its LAN mode).
pub(crate) fn stream_args(host: &str, info: &SunshineInfo, options: &MoonlightOptions, lan: bool) -> Vec<String> {
    let fullscreen = options.display_mode == DisplayMode::Fullscreen;
    let bitrate = options.bitrate_kbps.unwrap_or_else(|| default_bitrate_kbps(info.width, info.height, info.fps));
    let mut args: Vec<String> = vec![
        "stream".into(),
        host.into(),
        info.app.clone(),
        "--resolution".into(),
        format!("{}x{}", info.width, info.height),
        "--fps".into(),
        info.fps.to_string(),
        "--bitrate".into(),
        bitrate.to_string(),
        "--video-codec".into(),
        "HEVC".into(),
        "--video-decoder".into(),
        "hardware".into(),
        "--display-mode".into(),
        if fullscreen { "fullscreen" } else { "windowed" }.into(),
        // The pointer stays this Mac's: Moonlight 6.1 otherwise detaches it from the mouse.
        "--absolute-mouse".into(),
        "--capture-system-keys".into(),
        if fullscreen { "fullscreen" } else { "never" }.into(),
        // The host's Sunshine keeps its desktop "app" running: opening Moonlight again resumes it.
        "--no-quit-after".into(),
    ];
    if lan {
        args.extend(["--packet-size".into(), "1392".into()]);
    }
    args.extend(
        ["--no-performance-overlay", "--no-background-gamepad", "--no-multi-controller", "--keep-awake"].into_iter().map(String::from),
    );
    match options.vsync {
        Some(true) => args.push("--vsync".into()),
        Some(false) => args.push("--no-vsync".into()),
        None => {}
    }
    args.extend(options.extra_args.iter().cloned());
    args
}

/// Moonlight's log, from what it prints first: "Redirecting log output to /tmp/Moonlight-….log".
pub(crate) fn log_path_in(line: &str) -> Option<String> {
    let path = line.split_once("Redirecting log output to ")?.1.trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// Whether Moonlight's log shows the stream up: its last connection stage (input) done.
pub(crate) fn log_shows_streaming(log: &str) -> bool {
    log.find("Starting input stream").is_some_and(|at| log[at..].contains("done"))
}

/// Process ids of the Moonlight processes alive (not yet reaped), for [`shutdown`].
static LIVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// A Moonlight process. Killed (and reaped) when dropped.
pub(crate) struct Process {
    child: Child,
    pid: u32,
    /// Moonlight's log file, once it said where.
    log: Arc<Mutex<Option<String>>>,
    reaped: Option<ExitStatus>,
}

impl Process {
    fn spawn(bin: &Path, args: &[String], renderer: Renderer) -> std::io::Result<Self> {
        let mut command = Command::new(bin);
        command.args(args).current_dir("/").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        if renderer == Renderer::AvSampleBuffer {
            command.env("VT_FORCE_METAL", "0");
        }
        // Registered before it can exit, so shutdown never misses one.
        let mut live = LIVE.lock().unwrap();
        let mut child = command.spawn()?;
        let pid = child.id();
        live.push(pid);
        drop(live);
        let log = Arc::new(Mutex::new(None));
        if let Some(stderr) = child.stderr.take() {
            let log = log.clone();
            let _ = std::thread::Builder::new().name("lankvm-moonlight-stderr".into()).spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    match log_path_in(&line) {
                        Some(path) => *log.lock().unwrap() = Some(path),
                        None if !line.trim().is_empty() => tracing::debug!(pid, "moonlight: {line}"),
                        None => {}
                    }
                }
            });
        }
        Ok(Self { child, pid, log, reaped: None })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn log(&self) -> Option<String> {
        self.log.lock().unwrap().clone()
    }

    /// Its exit status, once it exited.
    fn exited(&mut self) -> Option<ExitStatus> {
        if self.reaped.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.reaped = Some(status);
            LIVE.lock().unwrap().retain(|p| *p != self.pid);
        }
        self.reaped
    }

    /// Kills it (if it still runs) and waits for it: its exit status.
    fn kill(&mut self) -> Option<ExitStatus> {
        if self.exited().is_none() {
            let _ = self.child.kill();
            if let Ok(status) = self.child.wait() {
                self.reaped = Some(status);
            }
            LIVE.lock().unwrap().retain(|p| *p != self.pid);
        }
        self.reaped
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Before the app quits: gives the Moonlight processes still running `timeout` to quit (their
/// runners stop them), then kills the rest.
pub(crate) fn shutdown(timeout: Duration) {
    let until = Instant::now() + timeout;
    while !LIVE.lock().unwrap().is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    // Under the lock: a process listed is not reaped yet, so its pid is still its own.
    let live = LIVE.lock().unwrap();
    for &pid in live.iter() {
        tracing::warn!(pid, "killing a Moonlight still running at quit");
        if let Ok(pid) = i32::try_from(pid) {
            // SAFETY: plain syscall on our own child.
            unsafe { kill(pid, 9) };
        }
    }
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// Whether Moonlight is paired with the Sunshine at `host` and reaches it (`Moonlight list`
/// exits 0). Blocks up to [`LIST_TIMEOUT`].
fn paired(bin: &Path, host: &str) -> bool {
    let args = ["list".to_string(), host.to_string()];
    let Ok(mut list) = Process::spawn(bin, &args, Renderer::Default) else { return false };
    let until = Instant::now() + LIST_TIMEOUT;
    loop {
        if let Some(status) = list.exited() {
            tracing::debug!(%status, host, "moonlight list");
            return status.success();
        }
        if Instant::now() >= until {
            tracing::warn!(host, "moonlight list didn't finish");
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A random 4-digit PIN for Moonlight to pair with.
fn random_pin() -> String {
    let mut bytes = [0u8; 4];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    let n = if read.is_ok() { u32::from_le_bytes(bytes) } else { std::process::id() ^ Instant::now().elapsed().subsec_nanos() };
    format!("{:04}", n % 10_000)
}

/// What a session tells its runner.
#[derive(Debug)]
pub(crate) enum Cmd {
    /// The host's Sunshine as it is now: a new generation means the old stream is gone.
    Info(SunshineInfo),
    /// The host's answer to the PIN.
    Paired { ok: bool, message: String },
    /// Open Moonlight again (after it ended).
    Open,
    /// The engine is LanKVM's again, or the session is over: the host ends the stream, and
    /// Moonlight quits by itself (or is killed).
    Stop,
}

/// Talks to a session's runner. Dropping it stops the runner.
pub(crate) struct Runner {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Runner {
    pub(crate) fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

/// Everything a runner needs besides Sunshine's info and the options.
#[derive(Clone)]
pub(crate) struct Env {
    /// The host's address (Moonlight connects there, at Sunshine's port).
    pub ip: IpAddr,
    /// Connected on the local network.
    pub lan: bool,
    pub host_name: String,
    /// The session's control messages (for the PIN).
    pub ctl: mpsc::UnboundedSender<ClientMsg>,
    pub events: SessionEvents,
}

/// Starts a session's runner on the current tokio runtime: it pairs Moonlight if needed and
/// opens it on Sunshine's stream.
pub(crate) fn start(env: Env, info: SunshineInfo, options: MoonlightOptions) -> Runner {
    let (tx, rx) = mpsc::unbounded_channel();
    match binary() {
        Some(bin) => {
            tokio::spawn(RunnerTask { bin, env, info, options }.run(rx));
        }
        None => (env.events)(SessionEvent::Moonlight { state: "failed", message: not_installed(), pid: None, log: None }),
    }
    Runner { tx }
}

struct RunnerTask {
    bin: PathBuf,
    env: Env,
    info: SunshineInfo,
    options: MoonlightOptions,
}

/// How starting Moonlight went.
enum Launch {
    Started(Process),
    Failed(String),
    Restart,
    Stop,
}

/// How a Moonlight that ran ended.
enum Watched {
    /// It quit (or was closed): the session stays on Sunshine, idle.
    Ended,
    /// A new generation: start again.
    Restart,
    Stop,
}

impl RunnerTask {
    fn emit(&self, state: &'static str, message: String, process: Option<&Process>) {
        let (pid, log) = process.map_or((None, None), |p| (Some(p.pid()), p.log()));
        tracing::info!(state, pid, log, "moonlight: {message}");
        (self.env.events)(SessionEvent::Moonlight { state, message, pid, log });
    }

    fn host(&self) -> String {
        host_string(self.env.ip, self.info.port)
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut open = true;
        // Since when Moonlight is idle (it ended or failed).
        let mut idle_since: Option<Instant> = None;
        loop {
            if open {
                open = false;
                idle_since = None;
                match self.launch(&mut rx).await {
                    Launch::Started(process) => match self.watch(process, &mut rx).await {
                        Watched::Ended => idle_since = Some(Instant::now()),
                        Watched::Restart => open = true,
                        Watched::Stop => return,
                    },
                    Launch::Failed(message) => {
                        self.emit("failed", message, None);
                        idle_since = Some(Instant::now());
                    }
                    Launch::Restart => open = true,
                    Launch::Stop => return,
                }
                continue;
            }
            // Idle: until asked to open it again, or Sunshine started again.
            match rx.recv().await {
                None | Some(Cmd::Stop) => return,
                Some(Cmd::Open) => open = true,
                Some(Cmd::Info(info)) => {
                    let restarted = info.generation != self.info.generation;
                    self.info = info;
                    // The host ends the stream before it stops Sunshine, so Moonlight usually quits
                    // seconds before the new generation comes: that's the restart, not the user.
                    if restarted && idle_since.is_some_and(|t| t.elapsed() < RESTART_GRACE) {
                        let message = format!("Sunshine on {} started again: opening Moonlight again.", self.env.host_name);
                        self.emit("starting", message, None);
                        open = true;
                    }
                }
                Some(Cmd::Paired { .. }) => {}
            }
        }
    }

    /// Pairs Moonlight if it isn't, then starts it.
    async fn launch(&mut self, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Launch {
        let host = self.host();
        let (bin, h) = (self.bin.clone(), host.clone());
        let mut check = tokio::task::spawn_blocking(move || paired(&bin, &h));
        let is_paired = loop {
            tokio::select! {
                done = &mut check => break done.unwrap_or(false),
                cmd = rx.recv() => match self.on_cmd(cmd) {
                    Some(stop @ (Launch::Stop | Launch::Restart)) => return stop,
                    _ => {}
                },
            }
        };
        if !is_paired {
            match self.pair(&host, rx).await {
                Ok(()) => {}
                Err(launch) => return launch,
            }
        }
        let args = stream_args(&host, &self.info, &self.options, self.env.lan);
        tracing::info!(bin = %self.bin.display(), ?args, renderer = ?self.options.renderer, "starting Moonlight");
        match Process::spawn(&self.bin, &args, self.options.renderer) {
            Ok(process) => {
                let message = format!("Moonlight is connecting to {}.", self.env.host_name);
                self.emit("starting", message, Some(&process));
                Launch::Started(process)
            }
            Err(e) => Launch::Failed(format!("Moonlight couldn't start ({e}).")),
        }
    }

    /// A command that came while not watching a Moonlight: what it means for the launch.
    fn on_cmd(&mut self, cmd: Option<Cmd>) -> Option<Launch> {
        match cmd {
            None | Some(Cmd::Stop) => Some(Launch::Stop),
            Some(Cmd::Info(info)) if info.generation != self.info.generation => {
                self.info = info;
                Some(Launch::Restart)
            }
            Some(Cmd::Info(info)) => {
                self.info = info;
                None
            }
            Some(Cmd::Open) | Some(Cmd::Paired { .. }) => None,
        }
    }

    /// Pairs Moonlight with the host's Sunshine: Moonlight shows a PIN of LanKVM's choosing, and
    /// the host hands it to Sunshine.
    async fn pair(&mut self, host: &str, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Result<(), Launch> {
        let name = self.env.host_name.clone();
        let pin = random_pin();
        let args = ["pair".to_string(), host.to_string(), "--pin".to_string(), pin.clone()];
        let mut pairing = Process::spawn(&self.bin, &args, Renderer::Default).map_err(|e| Launch::Failed(format!("Moonlight couldn't start ({e}).")))?;
        self.emit("pairing", format!("Pairing Moonlight with Sunshine on {name}."), Some(&pairing));
        if self.env.ctl.send(ClientMsg::SunshinePair { pin }).is_err() {
            return Err(Launch::Stop);
        }
        let until = tokio::time::Instant::now() + PAIR_ANSWER_WAIT;
        let mut tick = tokio::time::interval(WATCH_INTERVAL);
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Paired { ok: true, .. }) => break,
                    Some(Cmd::Paired { ok: false, message }) => {
                        return Err(Launch::Failed(if message.is_empty() { format!("Sunshine on {name} didn't pair with Moonlight.") } else { message }));
                    }
                    other => if let Some(launch) = self.on_cmd(other) { return Err(launch) },
                },
                _ = tick.tick() => {
                    if pairing.exited().is_some() {
                        return Err(Launch::Failed("Moonlight stopped pairing.".into()));
                    }
                    if tokio::time::Instant::now() >= until {
                        return Err(Launch::Failed(format!("{name} didn't answer about pairing Moonlight.")));
                    }
                }
            }
        }
        // Moonlight saves the pairing in the background: until `list` works, it may not have.
        let deadline = Instant::now() + PAIRED_WAIT;
        loop {
            let (bin, h) = (self.bin.clone(), host.to_string());
            let mut check = tokio::task::spawn_blocking(move || paired(&bin, &h));
            let done = loop {
                tokio::select! {
                    done = &mut check => break done.unwrap_or(false),
                    cmd = rx.recv() => if let Some(launch) = self.on_cmd(cmd) { return Err(launch) },
                }
            };
            if done {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Launch::Failed(format!("Moonlight paired, but can't reach Sunshine on {name}.")));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // It shows "pairing completed" until dismissed: nobody needs to.
        pairing.kill();
        tracing::info!(host, "Moonlight paired");
        Ok(())
    }

    /// Watches a Moonlight until it ends, a new generation replaces it, or the runner stops.
    async fn watch(&mut self, mut process: Process, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> Watched {
        let started = Instant::now();
        let mut streaming = false;
        let mut tick = tokio::time::interval(WATCH_INTERVAL);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Some(status) = process.exited() {
                        self.emit("ended", ended_message(Some(status), false), Some(&process));
                        return Watched::Ended;
                    }
                    if !streaming {
                        streaming = match process.log() {
                            Some(log) => std::fs::read_to_string(log).is_ok_and(|text| log_shows_streaming(&text)),
                            None => started.elapsed() >= STREAMING_AFTER,
                        };
                        if streaming {
                            self.emit("streaming", format!("Moonlight is streaming {}.", self.env.host_name), Some(&process));
                        }
                    }
                }
                cmd = rx.recv() => match cmd {
                    None | Some(Cmd::Stop) => {
                        let status = quit_or_kill(&mut process).await;
                        self.emit("ended", ended_message(status.0, status.1), Some(&process));
                        return Watched::Stop;
                    }
                    Some(Cmd::Info(info)) if info.generation != self.info.generation => {
                        // Sunshine started again (another display): the old stream is gone.
                        self.info = info;
                        let status = process.kill();
                        self.emit("ended", ended_message(status, true), Some(&process));
                        return Watched::Restart;
                    }
                    Some(Cmd::Info(info)) => self.info = info,
                    Some(Cmd::Open) | Some(Cmd::Paired { .. }) => {}
                }
            }
        }
    }
}

/// Waits up to [`EXIT_WAIT`] for Moonlight to quit (the host ends the stream), then kills it:
/// its status, and whether it was killed.
async fn quit_or_kill(process: &mut Process) -> (Option<ExitStatus>, bool) {
    let until = Instant::now() + EXIT_WAIT;
    while Instant::now() < until {
        if let Some(status) = process.exited() {
            return (Some(status), false);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tracing::info!(pid = process.pid(), "Moonlight didn't quit by itself; killing it");
    (process.kill(), true)
}

fn ended_message(status: Option<ExitStatus>, killed: bool) -> String {
    match (status.and_then(|s| s.code()), killed) {
        (_, true) => "Moonlight was closed.".into(),
        (Some(code), false) => format!("Moonlight ended (exit code {code})."),
        (None, false) => "Moonlight ended.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> SunshineInfo {
        SunshineInfo { port: 48989, generation: 1, app: "Desktop".into(), width: 2560, height: 1600, fps: 120 }
    }

    #[test]
    fn hosts_are_address_and_port() {
        assert_eq!(host_string("192.168.1.20".parse().unwrap(), 48989), "192.168.1.20:48989");
        assert_eq!(host_string("::ffff:127.0.0.1".parse().unwrap(), 48810), "127.0.0.1:48810");
        assert_eq!(host_string("fd00::2".parse().unwrap(), 48989), "[fd00::2]:48989");
    }

    #[test]
    fn stream_arguments_ask_for_the_display_as_it_is() {
        let options = MoonlightOptions::with_overrides(false, |_| None);
        let args = stream_args("127.0.0.1:48810", &info(), &options, true);
        let joined = args.join(" ");
        assert!(joined.starts_with("stream 127.0.0.1:48810 Desktop --resolution 2560x1600 --fps 120 --bitrate 29491 "), "{joined}");
        for want in [
            "--video-codec HEVC",
            "--video-decoder hardware",
            "--display-mode windowed",
            "--absolute-mouse",
            "--capture-system-keys never",
            "--no-quit-after",
            "--packet-size 1392",
            "--no-performance-overlay",
            "--no-background-gamepad",
            "--no-multi-controller",
            "--keep-awake",
        ] {
            assert!(joined.contains(want), "{want} missing from {joined}");
        }
        assert!(!joined.contains("vsync"), "Moonlight's own setting unless asked");
        let full = MoonlightOptions::with_overrides(true, |_| None);
        let joined = stream_args("[fd00::2]:48989", &info(), &full, false).join(" ");
        assert!(joined.contains("--display-mode fullscreen") && joined.contains("--capture-system-keys fullscreen"), "{joined}");
        assert!(!joined.contains("--packet-size"), "only on the local network");
    }

    #[test]
    fn the_environment_overrides_the_defaults() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
        };
        let options = MoonlightOptions::with_overrides(
            false,
            env(&[
                ("LANKVM_MOONLIGHT_RENDERER", "avsample"),
                ("LANKVM_MOONLIGHT_VSYNC", "0"),
                ("LANKVM_MOONLIGHT_BITRATE", "80000"),
                ("LANKVM_MOONLIGHT_ARGS", " --frame-pacing  --hdr "),
            ]),
        );
        assert_eq!(options.renderer, Renderer::AvSampleBuffer);
        assert_eq!(options.vsync, Some(false));
        assert_eq!(options.bitrate_kbps, Some(80000));
        let args = stream_args("h:1", &info(), &options, true);
        assert_eq!(&args[7..9], ["--bitrate", "80000"]);
        assert_eq!(&args[args.len() - 3..], ["--no-vsync", "--frame-pacing", "--hdr"]);
        let vsync = MoonlightOptions::with_overrides(false, env(&[("LANKVM_MOONLIGHT_VSYNC", "1"), ("LANKVM_MOONLIGHT_RENDERER", "metal")]));
        assert_eq!((vsync.vsync, vsync.renderer), (Some(true), Renderer::Default));
        let junk = MoonlightOptions::with_overrides(false, env(&[("LANKVM_MOONLIGHT_VSYNC", "x"), ("LANKVM_MOONLIGHT_BITRATE", "fast")]));
        assert_eq!((junk.vsync, junk.bitrate_kbps), (None, None));
    }

    #[test]
    fn bitrate_is_like_lankvms() {
        assert_eq!(default_bitrate_kbps(2560, 1600, 120), 29491, "60 fps worth at 120");
        assert_eq!(default_bitrate_kbps(640, 480, 30), 8000, "at least 8 Mbit/s");
        assert_eq!(default_bitrate_kbps(8192, 4320, 120), 150_000, "at most 150 Mbit/s");
    }

    #[test]
    fn moonlights_log_tells_where_and_when() {
        assert_eq!(log_path_in("Redirecting log output to /tmp/Moonlight-1791073083.log").as_deref(), Some("/tmp/Moonlight-1791073083.log"));
        assert_eq!(log_path_in("something else"), None);
        let starting = "00:00:01 - SDL Info (0): Starting video stream...\n00:00:01 - SDL Info (0): done\n00:00:01 - SDL Info (0): Starting input stream...\n";
        assert!(!log_shows_streaming(starting));
        assert!(log_shows_streaming(&format!("{starting}00:00:01 - SDL Info (0): done\n")));
        assert!(!log_shows_streaming("00:00:00 - Qt Info: No existing credentials found"));
    }

    #[test]
    fn pins_are_four_digits() {
        for _ in 0..20 {
            let pin = random_pin();
            assert!(pin.len() == 4 && pin.bytes().all(|b| b.is_ascii_digit()), "{pin}");
        }
    }

    /// Sunshine starting again for another display ends the stream before the host says so:
    /// Moonlight has quit by the time the new generation comes, and opens again then (once per
    /// generation: the same one again changes nothing).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_generation_after_moonlight_quit_opens_it_again() {
        use std::os::unix::fs::PermissionsExt;
        // Paired, and streams for a moment (until the host "ends the stream").
        let bin = std::env::temp_dir().join(format!("lankvm-fake-moonlight-{}", std::process::id()));
        std::fs::write(&bin, "#!/bin/sh\ncase \"$1\" in\nlist) exit 0 ;;\nstream) sleep 0.3; exit 0 ;;\nesac\nexit 1\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (ctl, _ctl_rx) = mpsc::unbounded_channel();
        let env = Env {
            ip: "127.0.0.1".parse().unwrap(),
            lan: true,
            host_name: "Studio".into(),
            ctl,
            events: Arc::new(move |event| {
                if let SessionEvent::Moonlight { state, pid, .. } = event {
                    let _ = events_tx.send((state, pid));
                }
            }),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let options = MoonlightOptions::with_overrides(false, |_| None);
        let task = tokio::spawn(RunnerTask { bin: bin.clone(), env, info: info(), options }.run(rx));
        let mut next = async || tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("no Moonlight news").unwrap();
        assert!(matches!(next().await, ("starting", Some(_))));
        assert!(matches!(next().await, ("ended", Some(_))));
        tx.send(Cmd::Info(SunshineInfo { generation: 2, ..info() })).unwrap();
        assert_eq!(next().await, ("starting", None), "said at once, before `list`");
        assert!(matches!(next().await, ("starting", Some(_))));
        assert!(matches!(next().await, ("ended", Some(_))));
        tx.send(Cmd::Info(SunshineInfo { generation: 2, ..info() })).unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(500), events.recv()).await.is_err(), "same generation: stays closed");
        tx.send(Cmd::Stop).unwrap();
        task.await.unwrap();
        let _ = std::fs::remove_file(bin);
    }

    #[test]
    fn exit_messages() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(ended_message(Some(ExitStatus::from_raw(0)), false), "Moonlight ended (exit code 0).");
        assert_eq!(ended_message(Some(ExitStatus::from_raw(9)), true), "Moonlight was closed.");
        assert_eq!(ended_message(Some(ExitStatus::from_raw(9)), false), "Moonlight ended.");
    }
}
