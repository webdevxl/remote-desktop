//! Sunshine as LanKVM's other streaming engine, on the host side.
//!
//! Sunshine (GPL-3.0) is installed separately and runs as a separate process; LanKVM never links
//! or bundles it. LanKVM starts it itself, as a child process, for two reasons: macOS attributes
//! a child of LanKVM to LanKVM, so Sunshine shares LanKVM's Screen Recording and Accessibility
//! grants (started any other way it would need grants of its own); and LanKVM knows which display
//! a viewer watches, which Sunshine reads only once, when it starts.
//!
//! One Sunshine at most, streaming to one session at a time. It gets a folder of its own in
//! LanKVM's data folder, used as its `HOME`, so the user's own `~/.config/sunshine` (and a
//! Sunshine they run themselves, on its default port) is never touched. Its config is LanKVM's,
//! written whole before every start. Its web API (localhost only) takes a password LanKVM made up
//! once; pairing a viewer's Moonlight goes through it, with the PIN Moonlight shows on the viewer
//! sent over LanKVM's own (paired, encrypted) connection.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use platform_mac::system;
use protocol::SunshineInfo;
use serde::{Deserialize, Serialize};
use transport::identity::Fingerprint;

use crate::{Event, EventSink, Trust};

/// Sunshine's base port unless `LANKVM_SUNSHINE_PORT` says otherwise. Not Sunshine's own default
/// (47989), so a Sunshine the user runs themselves doesn't clash with LanKVM's.
pub(crate) const DEFAULT_PORT: u16 = 48989;
/// The web API's user (the password is in `api-password`).
const API_USER: &str = "lankvm";
/// Names of the Moonlight clients LanKVM paired start with this, so they can be revoked.
const LABEL_PREFIX: &str = "lankvm-";
/// What Moonlight streams: Sunshine's whole-desktop app.
pub(crate) const APP: &str = "Desktop";
/// Sunshine probes its encoders at start (a few seconds) before it answers.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// A Sunshine asked to quit gets this long before it is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long Moonlight on the viewer gets to ask to pair, and how often to look.
const PAIR_WAIT: Duration = Duration::from_secs(30);
const PAIR_POLL: Duration = Duration::from_millis(250);
/// Sunshine's stdout and stderr (a copy of its log) start over beyond this size.
const MAX_OUT_BYTES: u64 = 4 << 20;
/// Where Sunshine may be (Homebrew on Apple Silicon and Intel, the DMG's app).
const BINARIES: [&str; 3] = ["/opt/homebrew/bin/sunshine", "/usr/local/bin/sunshine", "/Applications/Sunshine.app/Contents/MacOS/Sunshine"];

/// Sunshine's binary: `$LANKVM_SUNSHINE_BIN` (only it, when set), or the first of the usual
/// places that has one. None: not installed.
pub fn binary() -> Option<PathBuf> {
    if let Some(bin) = std::env::var_os("LANKVM_SUNSHINE_BIN").filter(|b| !b.is_empty()) {
        let bin = PathBuf::from(bin);
        return is_executable(&bin).then_some(bin);
    }
    BINARIES.iter().map(PathBuf::from).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The message for a viewer when this Mac has no Sunshine.
pub(crate) fn not_installed(host: &str) -> String {
    format!("Sunshine isn't installed on {host}. Run LanKVM's installer there (./install.sh) or `brew install lizardbyte/homebrew/sunshine`.")
}

/// Sunshine's ports are offsets from its base port: TCP base−5 (HTTPS), base (HTTP), base+1 (web
/// UI and API), base+21 (RTSP); UDP base+9..=base+11 (video, control, audio). A base whose ports
/// all fit above the privileged ones is valid.
pub(crate) fn valid_port(base: u16) -> bool {
    (1029..=65514).contains(&base)
}

/// The web API's port (HTTPS, self-signed).
pub(crate) fn api_port(base: u16) -> u16 {
    base + 1
}

/// `$LANKVM_SUNSHINE_PORT`, or [`DEFAULT_PORT`].
fn port_from_env() -> u16 {
    match std::env::var("LANKVM_SUNSHINE_PORT") {
        Ok(v) => match v.trim().parse() {
            Ok(port) if valid_port(port) => port,
            _ => {
                tracing::warn!("ignoring LANKVM_SUNSHINE_PORT={v:?}: expected a port from 1029 to 65514");
                DEFAULT_PORT
            }
        },
        Err(_) => DEFAULT_PORT,
    }
}

/// `$LANKVM_SUNSHINE_LOG_LEVEL` if it is one Sunshine knows, else "info".
fn log_level_from_env() -> String {
    let level = std::env::var("LANKVM_SUNSHINE_LOG_LEVEL").unwrap_or_default();
    let known = ["verbose", "debug", "info", "warning", "error", "fatal", "none", "0", "1", "2", "3", "4", "5", "6"];
    if known.contains(&level.as_str()) { level } else { "info".into() }
}

/// Test knob `LANKVM_SUNSHINE_CONFIG="key = value; key = value"`: more Sunshine settings (e.g.
/// `vt_realtime = disabled` for a bench), as lines after LanKVM's own. Only plain `key = value`
/// pairs of settings LanKVM doesn't write pass; anything else is ignored with a warning.
pub(crate) fn extra_config(spec: &str) -> String {
    let ours = config_text(&Config { port: 0, name: "", display_id: 0, input: false, audio: false, log_level: "", log_path: Path::new("") });
    let mut out = String::new();
    for pair in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').map(|(k, v)| (k.trim(), v.trim())).unwrap_or(("", ""));
        let plain_key = !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        let plain_value = !value.is_empty() && !value.chars().any(|c| c.is_control() || c == '#');
        let taken = ours.lines().any(|l| l.split_once(" = ").is_some_and(|(k, _)| k == key));
        if plain_key && plain_value && !taken {
            out.push_str(&format!("{key} = {value}\n"));
        } else {
            tracing::warn!("ignoring {pair:?} in LANKVM_SUNSHINE_CONFIG");
        }
    }
    out
}

/// The name Sunshine pairs a viewer's Moonlight under: revocable once the viewer is forgotten.
pub(crate) fn label(fp: &Fingerprint) -> String {
    let hex: String = fp[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{LABEL_PREFIX}{hex}")
}

/// Whether `pin` is what Moonlight shows: exactly 4 digits.
pub(crate) fn valid_pin(pin: &str) -> bool {
    pin.len() == 4 && pin.bytes().all(|b| b.is_ascii_digit())
}

/// What Sunshine is started with. Everything else is Sunshine's default.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Config<'a> {
    pub port: u16,
    /// What Moonlight lists the host as.
    pub name: &'a str,
    /// The display to stream (a CGDirectDisplayID).
    pub display_id: u32,
    /// Sunshine may post keyboard and mouse input (see [`crate::host`]).
    pub input: bool,
    pub audio: bool,
    pub log_level: &'a str,
    pub log_path: &'a Path,
}

/// The whole of `sunshine.conf`. Values run to the end of their line (Sunshine keeps a trailing
/// `#...` as part of one), so nothing follows them, and the name loses what could break a line.
pub(crate) fn config_text(c: &Config) -> String {
    let on = |b: bool| if b { "enabled" } else { "disabled" };
    let name: String = c.name.chars().filter(|ch| !ch.is_control() && *ch != '#' && *ch != '[').collect();
    let name = if name.trim().is_empty() { "Mac".to_string() } else { name.trim().to_string() };
    let lines = [
        "# Written by LanKVM before every start of Sunshine: changes here are lost.".to_string(),
        format!("port = {}", c.port),
        format!("sunshine_name = {name}"),
        format!("output_name = {}", c.display_id),
        "encoder = videotoolbox".into(),
        // HEVC Main: Moonlight's H.264 path freezes with this Sunshine (Sunshine #5469).
        "hevc_mode = 2".into(),
        // The web API answers this Mac only (LanKVM pairs through it).
        "origin_web_ui_allowed = pc".into(),
        "upnp = disabled".into(),
        "system_tray = disabled".into(),
        "notify_pre_releases = disabled".into(),
        "controller = disabled".into(),
        format!("keyboard = {}", on(c.input)),
        format!("mouse = {}", on(c.input)),
        format!("stream_audio = {}", on(c.audio)),
        "lan_encryption_mode = 1".into(),
        "wan_encryption_mode = 1".into(),
        "address_family = both".into(),
        format!("min_log_level = {}", c.log_level),
        format!("log_path = {}", c.log_path.display()),
    ];
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// A pairing Moonlight asked Sunshine for, waiting for its PIN.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct PendingPairing {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub address: String,
}

/// `GET /api/pin`'s pending pairings.
pub(crate) fn parse_pairings(text: &str) -> Vec<PendingPairing> {
    #[derive(Deserialize)]
    struct Pins {
        #[serde(default)]
        pairings: Vec<PendingPairing>,
    }
    serde_json::from_str::<Pins>(text).map(|p| p.pairings).unwrap_or_default()
}

/// The pairing that is the viewer's: the one from its address (Sunshine may write it as an
/// IPv4-mapped IPv6 address), else the newest.
pub(crate) fn choose_pairing(pairings: &[PendingPairing], viewer: IpAddr) -> Option<&PendingPairing> {
    let viewer = viewer.to_canonical();
    let ip_of = |address: &str| {
        let address = address.trim();
        address.parse::<IpAddr>().ok().or_else(|| address.parse::<SocketAddr>().ok().map(|a| a.ip())).map(|ip| ip.to_canonical())
    };
    pairings.iter().find(|p| ip_of(&p.address) == Some(viewer)).or(pairings.last())
}

/// A Moonlight client Sunshine has paired with.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct NamedCert {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub uuid: String,
}

/// `GET /api/clients/list`'s paired clients.
pub(crate) fn parse_clients(text: &str) -> Vec<NamedCert> {
    #[derive(Deserialize)]
    struct Clients {
        #[serde(default)]
        named_certs: Vec<NamedCert>,
    }
    serde_json::from_str::<Clients>(text).map(|c| c.named_certs).unwrap_or_default()
}

/// `{"status": true|false}`.
pub(crate) fn parse_status(text: &str) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(text).ok()?.get("status")?.as_bool()
}

/// Clients LanKVM paired (by their names) that aren't for a trusted viewer any more.
pub(crate) fn stale_clients<'a>(clients: &'a [NamedCert], trusted: &[String]) -> Vec<&'a NamedCert> {
    clients.iter().filter(|c| c.name.starts_with(LABEL_PREFIX) && !trusted.contains(&c.name)).collect()
}

/// `text` as a double-quoted value of a curl config file.
fn curl_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// What the host's UI shows about Sunshine.
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SunshineView {
    pub installed: bool,
    pub running: bool,
    /// The viewer it streams to.
    pub viewer: Option<String>,
}

/// What a running Sunshine was started for: another of any of these needs a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    pub session: u64,
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub input: bool,
}

struct Owner {
    session: u64,
    viewer: String,
}

struct Running {
    child: Child,
    /// Ends Sunshine if LanKVM goes away without stopping it (killed, crashed).
    watchdog: Option<Child>,
    key: Key,
    info: SunshineInfo,
}

#[derive(Default)]
struct State {
    owner: Option<Owner>,
    running: Option<Running>,
    /// Counts starts (see [`SunshineInfo::generation`]).
    generation: u32,
}

/// The host's Sunshine: one process at most, for one session at a time.
pub(crate) struct Sunshine {
    /// Sunshine's `HOME`: its config, state, credentials and log.
    dir: PathBuf,
    port: u16,
    trust: Arc<Trust>,
    events: EventSink,
    /// One start or stop at a time (each takes seconds). Taken before `state`, never while
    /// holding it.
    op: Mutex<()>,
    /// Never held for long: the UI reads it.
    state: Mutex<State>,
}

impl Sunshine {
    pub(crate) fn new(dir: PathBuf, trust: Arc<Trust>, events: EventSink) -> Self {
        Self { dir, port: port_from_env(), trust, events, op: Mutex::new(()), state: Mutex::default() }
    }

    pub(crate) fn view(&self) -> SunshineView {
        let installed = binary().is_some();
        let state = self.state.lock().unwrap();
        SunshineView { installed, running: state.running.is_some(), viewer: state.owner.as_ref().map(|o| o.viewer.clone()) }
    }

    /// The viewer Sunshine streams to, if another session than `session` has it.
    pub(crate) fn other_owner(&self, session: u64) -> Option<String> {
        self.state.lock().unwrap().owner.as_ref().filter(|o| o.session != session).map(|o| o.viewer.clone())
    }

    pub(crate) fn owned_by(&self, session: u64) -> bool {
        self.state.lock().unwrap().owner.as_ref().is_some_and(|o| o.session == session)
    }

    /// Whether `session`'s Sunshine still runs, and whether it may post input (None: it has none).
    pub(crate) fn status(&self, session: u64) -> Option<(bool, bool)> {
        let mut state = self.state.lock().unwrap();
        let running = state.running.as_mut().filter(|r| r.key.session == session)?;
        let alive = matches!(running.child.try_wait(), Ok(None));
        Some((alive, running.key.input))
    }

    /// Streams `key.display_id` to `key.session`'s viewer (`viewer` names it): starts Sunshine,
    /// or starts it again if it streams anything else (it reads its display only at start), and
    /// says where Moonlight connects. Fails, in words for the viewer, if Sunshine isn't installed,
    /// streams to another session, or can't start. Blocks for seconds.
    pub(crate) fn show(&self, key: Key, viewer: &str) -> Result<SunshineInfo, String> {
        let host = system::device_name();
        let _op = self.op.lock().unwrap();
        let old = {
            let mut state = self.state.lock().unwrap();
            match &state.owner {
                Some(o) if o.session != key.session => return Err(format!("Sunshine on {host} is streaming to {} already.", o.viewer)),
                _ => state.owner = Some(Owner { session: key.session, viewer: viewer.to_string() }),
            }
            // Already streaming just that (a capture restart, a recheck): nothing to change.
            if let Some(r) = state.running.as_mut().filter(|r| r.key == key)
                && matches!(r.child.try_wait(), Ok(None))
            {
                return Ok(r.info.clone());
            }
            state.running.take()
        };
        if let Some(old) = old {
            tracing::info!(?old.key, "restarting Sunshine");
            self.stop_process(old);
        }
        let started = self.start(&key, &host);
        let mut state = self.state.lock().unwrap();
        let mut running = match started {
            Ok(running) => running,
            Err(message) => {
                if state.owner.as_ref().is_some_and(|o| o.session == key.session) {
                    state.owner = None;
                }
                drop(state);
                (self.events)(Event::HostChanged);
                return Err(message);
            }
        };
        // The session ended (or let go) while it started.
        if !state.owner.as_ref().is_some_and(|o| o.session == key.session) {
            drop(state);
            self.stop_process(running);
            return Err(format!("Sunshine on {host} was stopped while it started."));
        }
        state.generation = state.generation.wrapping_add(1).max(1);
        running.info.generation = state.generation;
        let info = running.info.clone();
        tracing::info!(pid = running.child.id(), ?key, generation = info.generation, port = self.port, "Sunshine streams");
        state.running = Some(running);
        drop(state);
        (self.events)(Event::HostChanged);
        Ok(info)
    }

    /// `session` no longer uses Sunshine: it stops, in the background (never waits).
    pub(crate) fn release(self: &Arc<Self>, session: u64) {
        if !self.disown(session) {
            return;
        }
        let this = self.clone();
        std::thread::Builder::new()
            .name("lankvm-sunshine-stop".into())
            .spawn(move || this.stop_if_unowned())
            .map(drop)
            .unwrap_or_else(|e| tracing::warn!("stop Sunshine: {e}"));
    }

    /// `session` no longer uses Sunshine: it stops before this returns (blocks for a moment).
    pub(crate) fn release_now(&self, session: u64) {
        if self.disown(session) {
            self.stop_if_unowned();
        }
    }

    /// Before the app quits: stops Sunshine, whoever it streams to. Blocks for a moment.
    pub(crate) fn shutdown(&self) {
        self.state.lock().unwrap().owner = None;
        self.stop_if_unowned();
    }

    fn disown(&self, session: u64) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.owner.as_ref().is_some_and(|o| o.session == session) {
            state.owner = None;
            true
        } else {
            false
        }
    }

    /// Stops Sunshine unless a session took it meanwhile (that session's start replaces it).
    fn stop_if_unowned(&self) {
        let _op = self.op.lock().unwrap();
        let running = {
            let mut state = self.state.lock().unwrap();
            if state.owner.is_some() {
                return;
            }
            state.running.take()
        };
        if let Some(running) = running {
            tracing::info!(?running.key, "stopping Sunshine");
            self.stop_process(running);
            (self.events)(Event::HostChanged);
        }
    }

    /// Pairs the Moonlight that asked to (it shows `pin`) under `label`, preferring the request
    /// from `viewer`. Blocks until Sunshine says how it went (up to a minute).
    pub(crate) fn pair(&self, pin: &str, label: &str, viewer: IpAddr) -> Result<(), String> {
        let host = system::device_name();
        if !valid_pin(pin) {
            return Err("Moonlight's pairing code must be 4 digits.".into());
        }
        if self.state.lock().unwrap().running.is_none() {
            return Err(format!("Sunshine isn't running on {host}."));
        }
        let deadline = Instant::now() + PAIR_WAIT;
        let pairing = loop {
            match self.api("GET", "/api/pin", None, Duration::from_secs(3)) {
                Ok((200, text)) => {
                    if let Some(p) = choose_pairing(&parse_pairings(&text), viewer) {
                        break p.clone();
                    }
                }
                Ok((code, text)) => tracing::warn!(code, "Sunshine GET /api/pin: {}", text.trim()),
                Err(e) => tracing::warn!("Sunshine GET /api/pin: {e:#}"),
            }
            if Instant::now() >= deadline {
                return Err(format!("Moonlight didn't ask Sunshine on {host} to pair within {} s.", PAIR_WAIT.as_secs()));
            }
            std::thread::sleep(PAIR_POLL);
        };
        tracing::info!(id = %pairing.id, name = %pairing.name, address = %pairing.address, label, "pairing Moonlight with Sunshine");
        let body = serde_json::json!({ "pairing_id": pairing.id, "pin": pin, "name": label }).to_string();
        // Sunshine answers once the handshake with Moonlight is over (or it gave up on it).
        match self.api("POST", "/api/pin", Some(&body), Duration::from_secs(40)) {
            Ok((200, text)) if parse_status(&text) == Some(true) => Ok(()),
            Ok((code, text)) => {
                tracing::warn!(code, "Sunshine POST /api/pin: {}", text.trim());
                Err(format!("Sunshine on {host} didn't accept Moonlight's pairing (a wrong code, or Moonlight gave up)."))
            }
            Err(e) => {
                tracing::warn!("Sunshine POST /api/pin: {e:#}");
                Err(format!("LanKVM couldn't reach Sunshine on {host} to pair Moonlight."))
            }
        }
    }

    /// The viewer `fp` was forgotten: its Moonlight can't connect to Sunshine any more (now, if
    /// Sunshine runs; else at its next start). Never waits.
    pub(crate) fn revoke(self: &Arc<Self>, fp: Fingerprint) {
        if self.state.lock().unwrap().running.is_none() {
            return;
        }
        let this = self.clone();
        std::thread::spawn(move || {
            let label = label(&fp);
            match this.api("GET", "/api/clients/list", None, Duration::from_secs(3)) {
                Ok((200, text)) => {
                    for client in parse_clients(&text).iter().filter(|c| c.name == label) {
                        this.unpair(client);
                    }
                }
                Ok((code, _)) => tracing::warn!(code, "Sunshine GET /api/clients/list"),
                Err(e) => tracing::warn!("Sunshine GET /api/clients/list: {e:#}"),
            }
        });
    }

    /// Unpairs the Moonlight clients LanKVM paired for viewers it no longer trusts.
    fn revoke_stale(&self) {
        let trusted: Vec<String> = self.trust.viewers.lock().unwrap().entries().iter().map(|(fp, _)| label(fp)).collect();
        match self.api("GET", "/api/clients/list", None, Duration::from_secs(3)) {
            Ok((200, text)) => {
                for client in stale_clients(&parse_clients(&text), &trusted) {
                    self.unpair(client);
                }
            }
            Ok((code, _)) => tracing::warn!(code, "Sunshine GET /api/clients/list"),
            Err(e) => tracing::warn!("Sunshine GET /api/clients/list: {e:#}"),
        }
    }

    fn unpair(&self, client: &NamedCert) {
        let body = serde_json::json!({ "uuid": client.uuid }).to_string();
        match self.api("POST", "/api/clients/unpair", Some(&body), Duration::from_secs(3)) {
            Ok((200, _)) => tracing::info!(name = %client.name, "unpaired a Moonlight client from Sunshine"),
            Ok((code, text)) => tracing::warn!(code, name = %client.name, "Sunshine unpair: {}", text.trim()),
            Err(e) => tracing::warn!(name = %client.name, "Sunshine unpair: {e:#}"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Sunshine's own state (its credentials and paired clients), under its `HOME`.
    fn state_file(&self) -> PathBuf {
        self.dir.join(".config/sunshine/sunshine_state.json")
    }

    /// The web API's password, made up the first time; true if it is new.
    fn api_password(&self) -> std::io::Result<(String, bool)> {
        let path = self.path("api-password");
        if let Ok(pw) = std::fs::read_to_string(&path) {
            let pw = pw.trim().to_string();
            if pw.len() >= 24 && pw.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Ok((pw, false));
            }
        }
        let pw = random_password(32)?;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        file.write_all(pw.as_bytes())?;
        Ok((pw, true))
    }

    /// Calls Sunshine's web API with curl: (HTTP status, body). The password goes to curl on its
    /// stdin (`-K -`), never on a command line where `ps` would show it.
    fn api(&self, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> anyhow::Result<(u16, String)> {
        let (password, _) = self.api_password()?;
        let url = format!("https://127.0.0.1:{}{path}", api_port(self.port));
        let mut config = format!(
            "url = {}\nuser = {}\nrequest = {}\nwrite-out = \"\\n%{{http_code}}\"\n",
            curl_quote(&url),
            curl_quote(&format!("{API_USER}:{password}")),
            curl_quote(method)
        );
        if let Some(body) = body {
            config.push_str("header = \"Content-Type: application/json\"\n");
            config.push_str(&format!("data = {}\n", curl_quote(body)));
        }
        let mut child = Command::new("/usr/bin/curl")
            .args(["-sk", "--max-time", &format!("{:.1}", timeout.as_secs_f64()), "-K", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(config.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        let code: u16 = code.trim().parse().unwrap_or(0);
        if !output.status.success() || code == 0 {
            anyhow::bail!("curl {method} {path}: {} (HTTP {code})", output.status);
        }
        Ok((code, body.to_string()))
    }

    /// Starts Sunshine for `key` and waits until it answers. Errors are for the viewer (naming
    /// `host`); the details go to the log.
    fn start(&self, key: &Key, host: &str) -> Result<Running, String> {
        let Some(bin) = binary() else { return Err(not_installed(host)) };
        let failed = |what: &str, e: &dyn std::fmt::Display| {
            tracing::warn!("Sunshine: {what}: {e}");
            format!("Sunshine on {host} couldn't start ({what}).")
        };
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&self.dir).map_err(|e| failed("its folder", &e))?;
        self.kill_stale();
        let config_path = self.path("sunshine.conf");
        let log_path = self.path("sunshine.log");
        let log_level = log_level_from_env();
        let config = Config {
            port: self.port,
            name: &system::device_name(),
            display_id: key.display_id,
            input: key.input,
            audio: std::env::var("LANKVM_SUNSHINE_AUDIO").is_ok_and(|v| v == "1"),
            log_level: &log_level,
            log_path: &log_path,
        };
        let mut text = config_text(&config);
        text.push_str(&extra_config(&std::env::var("LANKVM_SUNSHINE_CONFIG").unwrap_or_default()));
        std::fs::write(&config_path, text).map_err(|e| failed("its settings", &e))?;
        let out = self.out_file().map_err(|e| failed("its output file", &e))?;
        let (password, new) = self.api_password().map_err(|e| failed("its password", &e))?;
        if new || !self.state_file().exists() {
            self.set_credentials(&bin, &config_path, &password).map_err(|e| failed("its credentials", &e))?;
        }
        let started = Instant::now();
        let child = Command::new(&bin)
            .arg(&config_path)
            .env("HOME", &self.dir)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(out.try_clone().map_err(|e| failed("its output file", &e))?)
            .stderr(out)
            .spawn()
            .map_err(|e| failed("launch", &e))?;
        let pid = child.id();
        let _ = std::fs::write(self.path("sunshine.pid"), pid.to_string());
        let watchdog = spawn_watchdog(pid);
        tracing::info!(pid, bin = %bin.display(), port = self.port, display = key.display_id, input = key.input, "starting Sunshine");
        let mut running = Running {
            child,
            watchdog,
            key: *key,
            info: SunshineInfo { port: self.port, generation: 0, app: APP.into(), width: key.width, height: key.height, fps: key.fps },
        };
        // Up once its HTTP port answers (it probes the encoders first).
        let up = loop {
            if let Ok(Some(status)) = running.child.try_wait() {
                tracing::warn!(%status, "Sunshine exited while starting; its log ends:\n{}", self.log_tail(20));
                self.stop_process(running);
                return Err(format!("Sunshine on {host} quit while starting."));
            }
            if http_answers(self.port) {
                break true;
            }
            if started.elapsed() >= START_TIMEOUT {
                break false;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if !up {
            tracing::warn!("Sunshine didn't answer on port {} within {} s; its log ends:\n{}", self.port, START_TIMEOUT.as_secs(), self.log_tail(20));
            self.stop_process(running);
            return Err(format!("Sunshine on {host} didn't answer within {} s.", START_TIMEOUT.as_secs()));
        }
        if log_lacks_capture(&log) {
            tracing::warn!("Sunshine has no Screen Recording permission");
            self.stop_process(running);
            return Err(format!("Sunshine on {host} can't record the screen: allow Screen Recording for LanKVM there."));
        }
        tracing::info!(
            ms = started.elapsed().as_millis() as u64,
            hevc = log_has_hevc(&log),
            display = log_display(&log).unwrap_or_default(),
            "Sunshine answers"
        );
        if !log_has_hevc(&log) {
            tracing::warn!("Sunshine found no HEVC encoder: Moonlight will fall back to H.264");
        }
        self.revoke_stale();
        Ok(running)
    }

    /// Sunshine's stdout and stderr (a copy of its log), appended to across starts.
    fn out_file(&self) -> std::io::Result<std::fs::File> {
        let path = self.path("sunshine.out");
        let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_OUT_BYTES);
        std::fs::OpenOptions::new().create(true).append(!too_big).write(true).truncate(too_big).open(path)
    }

    /// Writes the web API's credentials into Sunshine's state (it does that and exits; the
    /// config path must come before `--creds`).
    fn set_credentials(&self, bin: &Path, config: &Path, password: &str) -> anyhow::Result<()> {
        let mut child = Command::new(bin)
            .arg(config)
            .args(["--creds", API_USER, password])
            .env("HOME", &self.dir)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(status.success(), "sunshine --creds: {status}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("sunshine --creds didn't finish");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        anyhow::ensure!(self.state_file().exists(), "sunshine --creds wrote no state file");
        tracing::info!("set Sunshine's web API credentials");
        Ok(())
    }

    /// Ends the stream (Moonlight then quits by itself and writes its stats), then Sunshine.
    fn stop_process(&self, mut running: Running) {
        let pid = running.child.id();
        // Only an app that runs (a stream, or one Moonlight left to resume) needs ending.
        let app_running = http_get(self.port, "/serverinfo").is_some_and(|xml| serverinfo_busy(&xml));
        if matches!(running.child.try_wait(), Ok(None)) {
            let log_path = self.path("sunshine.log");
            let terminated = |p: &Path| std::fs::read_to_string(p).unwrap_or_default().matches("Process terminated").count();
            let before = terminated(&log_path);
            let closed = if app_running { Some(self.api("POST", "/api/apps/close", Some("{}"), Duration::from_secs(2))) } else { None };
            match closed {
                None => {}
                Some(closed) => match closed {
                // Sunshine tells Moonlight within a moment (its stream loop runs every 150 ms).
                Ok((200, _)) => {
                    let until = Instant::now() + Duration::from_secs(1);
                    while terminated(&log_path) == before && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Ok((code, text)) => tracing::debug!(code, "Sunshine apps/close: {}", text.trim()),
                Err(e) => tracing::debug!("Sunshine apps/close: {e:#}"),
                },
            }
            signal(pid, SIGTERM);
            let until = Instant::now() + STOP_TIMEOUT;
            while matches!(running.child.try_wait(), Ok(None)) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(25));
            }
            if matches!(running.child.try_wait(), Ok(None)) {
                tracing::warn!(pid, "Sunshine didn't quit; killing it");
                let _ = running.child.kill();
            }
        }
        let status = running.child.wait();
        tracing::info!(pid, ?status, "Sunshine stopped");
        if let Some(mut watchdog) = running.watchdog.take() {
            // It sees Sunshine gone within a second and exits.
            let until = Instant::now() + Duration::from_secs(2);
            while matches!(watchdog.try_wait(), Ok(None)) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = watchdog.kill();
            let _ = watchdog.wait();
        }
        let _ = std::fs::remove_file(self.path("sunshine.pid"));
    }

    /// Kills a Sunshine a LanKVM that crashed left behind (its pid file says which), if that
    /// process is still a Sunshine.
    fn kill_stale(&self) {
        let pid_path = self.path("sunshine.pid");
        let Some(pid) = std::fs::read_to_string(&pid_path).ok().and_then(|s| s.trim().parse::<i32>().ok()) else { return };
        let _ = std::fs::remove_file(&pid_path);
        if pid <= 1 || !executable_of(pid).is_some_and(|path| is_sunshine(&path)) {
            return;
        }
        tracing::warn!(pid, "stopping a Sunshine left behind by an earlier LanKVM");
        signal_pid(pid, SIGTERM);
        let until = Instant::now() + STOP_TIMEOUT;
        while alive(pid) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        if alive(pid) && executable_of(pid).is_some_and(|path| is_sunshine(&path)) {
            signal_pid(pid, SIGKILL);
        }
    }

    /// The last `lines` lines of Sunshine's log, for LanKVM's.
    fn log_tail(&self, lines: usize) -> String {
        let log = std::fs::read_to_string(self.path("sunshine.log")).unwrap_or_default();
        let all: Vec<&str> = log.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }
}

impl Drop for Sunshine {
    fn drop(&mut self) {
        if let Some(running) = self.state.get_mut().unwrap().running.take() {
            self.stop_process(running);
        }
    }
}

/// Sunshine found no Screen Recording permission (it keeps running without, streaming nothing).
pub(crate) fn log_lacks_capture(log: &str) -> bool {
    log.contains("No screen capture permission")
}

/// Sunshine's encoder probe found HEVC (otherwise Moonlight falls back to H.264).
pub(crate) fn log_has_hevc(log: &str) -> bool {
    log.contains("Found HEVC encoder")
}

/// The display Sunshine says it set up: "Configuring selected display (5) to stream".
pub(crate) fn log_display(log: &str) -> Option<String> {
    let line = log.lines().rev().find(|l| l.contains("Configuring selected display ("))?;
    let start = line.find("display (")? + "display (".len();
    let end = line[start..].find(')')? + start;
    Some(line[start..end].to_string())
}

/// Whether Sunshine's HTTP port answers a request (it does once it is up).
fn http_answers(port: u16) -> bool {
    http_get(port, "/serverinfo").is_some()
}

/// A plain HTTP GET of `path` on Sunshine's HTTP port (which answers `/serverinfo` to anyone):
/// the response, if one came.
fn http_get(port: u16, path: &str) -> Option<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = Vec::new();
    // Up to what came before the timeout: serverinfo is short, and the head is what counts.
    let _ = stream.take(64 * 1024).read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response).into_owned();
    response.starts_with("HTTP/1.").then_some(response)
}

/// Whether Sunshine's `/serverinfo` says an app runs (a stream, or one left to resume).
pub(crate) fn serverinfo_busy(xml: &str) -> bool {
    let game = xml.split_once("<currentgame>").and_then(|(_, rest)| rest.split_once("</currentgame>")).map(|(v, _)| v.trim());
    xml.contains("SUNSHINE_SERVER_BUSY") || game.is_some_and(|g| !g.is_empty() && g != "0")
}

/// A password of `len` letters and digits from the system's random source.
fn random_password(len: usize) -> std::io::Result<String> {
    const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut urandom = std::fs::File::open("/dev/urandom")?;
    let mut out = String::with_capacity(len);
    let mut byte = [0u8; 1];
    while out.len() < len {
        urandom.read_exact(&mut byte)?;
        // 248 = 4 × 62: anything above would favour the first letters.
        if byte[0] < 248 {
            out.push(ALPHABET[usize::from(byte[0] % 62)] as char);
        }
    }
    Ok(out)
}

/// The watchdog: a shell loop that ends Sunshine (`$2`) once LanKVM (`$1`) is gone, and exits
/// by itself once Sunshine is. LanKVM quitting normally stops Sunshine itself; this covers it
/// being killed or crashing (a stale Sunshine would otherwise stream on, and hold the port).
const WATCHDOG: &str = r#"p=$1; s=$2
while kill -0 "$p" 2>/dev/null && kill -0 "$s" 2>/dev/null; do sleep 1; done
if kill -0 "$s" 2>/dev/null; then
  kill -TERM "$s" 2>/dev/null; i=0
  while kill -0 "$s" 2>/dev/null && [ $i -lt 30 ]; do sleep 0.1; i=$((i+1)); done
  kill -KILL "$s" 2>/dev/null
fi"#;

fn spawn_watchdog(sunshine: u32) -> Option<Child> {
    Command::new("/bin/sh")
        .args(["-c", WATCHDOG, "lankvm-sunshine-watchdog", &std::process::id().to_string(), &sunshine.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .inspect_err(|e| tracing::warn!("Sunshine watchdog: {e}"))
        .ok()
}

const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, buffersize: u32) -> i32;
}

fn signal(pid: u32, sig: i32) {
    if let Ok(pid) = i32::try_from(pid) {
        signal_pid(pid, sig);
    }
}

fn signal_pid(pid: i32, sig: i32) {
    if pid > 1 {
        // SAFETY: plain syscall; a pid that's gone only makes it fail.
        unsafe { kill(pid, sig) };
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 1 && unsafe { kill(pid, 0) } == 0
}

/// The executable of process `pid`, if it exists.
fn executable_of(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; 4096];
    // SAFETY: the buffer is as long as said; proc_pidpath writes at most that much.
    let len = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buffer).into_owned()))
}

/// Whether `path` is a Sunshine binary (Homebrew's `sunshine`, the app's `Sunshine`).
fn is_sunshine(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case("sunshine"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_written_whole_with_lankvms_settings() {
        let log = PathBuf::from("/Users/a b/Library/Application Support/lankvm/sunshine/sunshine.log");
        let config = Config { port: 48989, name: "Studio #2\n[x]", display_id: 7, input: false, audio: false, log_level: "info", log_path: &log };
        let text = config_text(&config);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with('#'), "a comment line first");
        for want in [
            "port = 48989",
            "sunshine_name = Studio 2x]",
            "output_name = 7",
            "encoder = videotoolbox",
            "hevc_mode = 2",
            "origin_web_ui_allowed = pc",
            "upnp = disabled",
            "system_tray = disabled",
            "notify_pre_releases = disabled",
            "controller = disabled",
            "keyboard = disabled",
            "mouse = disabled",
            "stream_audio = disabled",
            "lan_encryption_mode = 1",
            "wan_encryption_mode = 1",
            "address_family = both",
            "min_log_level = info",
            "log_path = /Users/a b/Library/Application Support/lankvm/sunshine/sunshine.log",
        ] {
            assert!(lines.contains(&want), "{want:?} missing from\n{text}");
        }
        // Nothing may follow a value on its line, and only the first line is a comment.
        assert!(lines[1..].iter().all(|l| !l.contains('#') && l.contains(" = ")), "{text}");
        assert!(!text.contains("minimum_fps_target"), "left to Sunshine: it would repeat frames");
        let with_input = config_text(&Config { input: true, audio: true, ..config });
        for want in ["keyboard = enabled", "mouse = enabled", "stream_audio = enabled"] {
            assert!(with_input.lines().any(|l| l == want), "{want}");
        }
        assert!(config_text(&Config { name: " \n", ..config }).lines().any(|l| l == "sunshine_name = Mac"));
    }

    #[test]
    fn extra_settings_are_plain_pairs_lankvm_doesnt_write() {
        assert_eq!(extra_config(" vt_realtime = disabled ;; fec_percentage=10 "), "vt_realtime = disabled\nfec_percentage = 10\n");
        assert_eq!(extra_config("output_name = 1; mouse = enabled"), "", "LanKVM's own settings stay LanKVM's");
        assert_eq!(extra_config("Bad Key = 1; x = a#b; y =; = z; novalue"), "");
    }

    #[test]
    fn ports_follow_the_base() {
        assert_eq!(api_port(DEFAULT_PORT), 48990);
        assert_eq!(api_port(48810), 48811);
        assert!(valid_port(DEFAULT_PORT) && valid_port(47989) && valid_port(1029) && valid_port(65514));
        assert!(!valid_port(1028), "its HTTPS port (base − 5) would be privileged");
        assert!(!valid_port(65515), "its RTSP port (base + 21) wouldn't fit");
        assert!(!valid_port(0));
    }

    #[test]
    fn pins_are_four_digits() {
        assert!(valid_pin("0042") && valid_pin("9999"));
        for bad in ["", "123", "12345", "12a4", " 123", "١٢٣٤", "12 4"] {
            assert!(!valid_pin(bad), "{bad:?}");
        }
    }

    #[test]
    fn labels_name_the_viewer() {
        let fp: Fingerprint = core::array::from_fn(|i| (i * 17) as u8);
        assert_eq!(label(&fp), "lankvm-0011223344556677");
        assert!(label(&fp).starts_with(LABEL_PREFIX));
    }

    #[test]
    fn pairings_parse_and_the_viewers_is_chosen() {
        let text = r#"{"pairings":[
            {"id":"0123456789abcdef0123456789abcdef","name":"roth","address":"::ffff:192.168.1.7"},
            {"id":"fedcba9876543210fedcba9876543210","name":"roth","address":"192.168.1.9"}]}"#;
        let pairings = parse_pairings(text);
        assert_eq!(pairings.len(), 2);
        assert_eq!(pairings[0].name, "roth");
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(choose_pairing(&pairings, ip("192.168.1.7")).unwrap().id, "0123456789abcdef0123456789abcdef", "mapped address matches");
        assert_eq!(choose_pairing(&pairings, ip("::ffff:192.168.1.9")).unwrap().id, "fedcba9876543210fedcba9876543210");
        assert_eq!(choose_pairing(&pairings, ip("10.0.0.1")).unwrap().id, "fedcba9876543210fedcba9876543210", "else the newest");
        assert!(choose_pairing(&parse_pairings(r#"{"pairings":[]}"#), ip("10.0.0.1")).is_none());
        assert!(parse_pairings("not json").is_empty());
        assert_eq!(parse_pairings(r#"{"pairings":[{"id":"ab"}]}"#)[0].address, "");
    }

    #[test]
    fn statuses_and_clients_parse() {
        assert_eq!(parse_status(r#"{"status":true}"#), Some(true));
        assert_eq!(parse_status(r#"{"status":false}"#), Some(false));
        assert_eq!(parse_status(r#"{"error":"x"}"#), None);
        assert_eq!(parse_status("<html>"), None);
        let clients = parse_clients(
            r#"{"named_certs":[{"name":"lankvm-0011223344556677","uuid":"A"},{"name":"lankvm-ffffffffffffffff","uuid":"B"},{"name":"My PC","uuid":"C"}],"status":true}"#,
        );
        assert_eq!(clients.len(), 3);
        let stale = stale_clients(&clients, &["lankvm-0011223344556677".to_string()]);
        assert_eq!(stale.iter().map(|c| c.uuid.as_str()).collect::<Vec<_>>(), ["B"], "only LanKVM's, and only untrusted ones");
    }

    #[test]
    fn curl_config_values_are_quoted() {
        assert_eq!(curl_quote("abc"), "\"abc\"");
        assert_eq!(curl_quote(r#"{"pin":"1234"}"#), r#""{\"pin\":\"1234\"}""#);
        assert_eq!(curl_quote("a\\b\nc"), "\"a\\\\b\\nc\"");
    }

    #[test]
    fn the_log_says_what_sunshine_found() {
        let log = "[2026-10-03 17:56:42.670]: Error: No screen capture permission!\n\
                   [2026-10-03 17:56:42.681]: Info: Configuring selected display (5) to stream\n\
                   [2026-10-03 17:56:46.262]: Info: Found HEVC encoder: hevc_videotoolbox [videotoolbox]\n";
        assert!(log_lacks_capture(log));
        assert!(log_has_hevc(log));
        assert_eq!(log_display(log).as_deref(), Some("5"));
        assert!(!log_lacks_capture("Info: Found H.264 encoder"));
        assert_eq!(log_display(""), None);
    }

    #[test]
    fn serverinfo_says_whether_an_app_runs() {
        let free = "<root status_code=\"200\"><PairStatus>0</PairStatus><currentgame>0</currentgame><state>SUNSHINE_SERVER_FREE</state></root>";
        let busy = "<root status_code=\"200\"><currentgame>881448767</currentgame><state>SUNSHINE_SERVER_BUSY</state></root>";
        assert!(!serverinfo_busy(free));
        assert!(serverinfo_busy(busy));
        assert!(!serverinfo_busy(""));
    }

    #[test]
    fn passwords_are_long_and_plain() {
        let a = random_password(32).unwrap();
        let b = random_password(32).unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }

    #[test]
    fn only_sunshine_binaries_count_as_sunshine() {
        assert!(is_sunshine(Path::new("/opt/homebrew/Cellar/sunshine/2026.914.233613/bin/sunshine")));
        assert!(is_sunshine(Path::new("/Applications/Sunshine.app/Contents/MacOS/Sunshine")));
        assert!(!is_sunshine(Path::new("/bin/sh")));
        assert!(executable_of(std::process::id() as i32).is_some(), "this process has an executable");
    }
}
