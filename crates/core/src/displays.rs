//! Virtual displays this Mac makes for its viewers (see `platform_mac::virtual_display`): a viewer
//! asks for one the size of its own screen and watches it instead of this Mac's screen.
//!
//! The rules:
//! - Each viewer device has at most one, shared by its sessions (two windows, a reconnect). The
//!   latest request decides its size; the device's other sessions follow, never push back.
//! - At most one virtual display is "main" or "only" at a time: a device that takes that role
//!   from another leaves the other's display extended next to this Mac's.
//! - A display goes when its last session stops watching it: at once, or [`LINGER`] later if the
//!   connection was lost, so a viewer that comes back finds its windows where they were.
//!
//! One thread owns every display and handles requests one at a time, in order: creating,
//! resizing, arranging and removing a display each wait on WindowServer for up to a second or so,
//! must not overlap, and must not run on the async runtime. Sessions join when they start and
//! leave when they end, so a request can never outlive its session and leave a display behind.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
use platform_mac::system::{DisplaysAwake, device_name};
use platform_mac::virtual_display::{self, Mode, VirtualDisplay};
use protocol::{Arrangement, DisplayReason, VirtualDisplaySpec};
use serde::Serialize;
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use transport::identity::Fingerprint;

use crate::host::{HostCtx, SessionEvt};

/// How long a virtual display outlives a lost connection.
pub(crate) const LINGER: Duration = Duration::from_secs(60);
/// Virtual displays at once, for all viewers: each costs memory and GPU time for the whole Mac.
pub(crate) const MAX_DISPLAYS: usize = 2;
/// Least time between two changes to the displays, so a viewer flipping back and forth can't keep
/// WindowServer (and every other viewer's stream) busy. Requests wait for their turn.
const MIN_CHANGE_INTERVAL: Duration = Duration::from_millis(1000);
/// How often the worker checks that macOS left each display in its mode.
const MODE_CHECK: Duration = Duration::from_secs(2);
/// Display changes come in bursts (one per display, before and after): act once it's quiet.
const RECONFIGURATION_SETTLE: Duration = Duration::from_millis(400);
/// Least time between two repairs of an arrangement someone else undid, so LanKVM never fights
/// the Mac's user (or macOS) in a loop.
const MIN_REPAIR_INTERVAL: Duration = Duration::from_secs(5);

/// What the UI shows about one virtual display.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VirtualDisplayView {
    pub display_id: u32,
    /// The viewer device it was made for: its name, and its device id (names can repeat).
    pub owner: String,
    pub owner_id: String,
    pub width: u32,
    pub height: u32,
    pub hidpi: bool,
    /// "extend", "main" or "only".
    pub arrangement: &'static str,
    /// Whether a viewer is watching it now (otherwise it waits for its viewer to come back).
    pub in_use: bool,
}

/// A virtual display a session watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Acquired {
    pub display_id: u32,
    /// As it is now (another session of the device may have chosen it).
    pub spec: VirtualDisplaySpec,
    /// Where this stands among the worker's news (see [`DisplayNotice`]).
    pub seq: u64,
}

/// What the worker tells a session about its display. `seq` orders the news and the answers to
/// the session's own requests (they travel apart): news older than what the session already
/// follows is stale.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DisplayNotice {
    /// The display it watches changed: another session of its device resized it, or another device
    /// took the main role. Stream it as `spec` now.
    Changed { display_id: u32, spec: VirtualDisplaySpec, message: String, seq: u64 },
    /// Its display is gone: this Mac's user removed it ([`DisplayReason::REMOVED_BY_HOST`]), or
    /// macOS did ([`DisplayReason::DISPLAY_GONE`]). Sent once this Mac's own display is the main
    /// one again.
    Removed { display_id: u32, reason: DisplayReason, message: String, seq: u64 },
    /// Displays were added, removed or rearranged. A session watching the main display checks
    /// that its display still shows anything.
    Layout,
}

enum Cmd {
    /// A session starts. If its device has a virtual display, the session watches it from the start
    /// (a reconnect finds it again).
    Join { session: u64, owner: Fingerprint, name: String, events: tokio_mpsc::UnboundedSender<SessionEvt>, reply: oneshot::Sender<Option<Acquired>> },
    /// The session watches its device's display, made to match `spec`. `epoch` is
    /// [`Displays::epoch`] when the session decided it may: refused if this Mac's user removed
    /// displays since.
    Acquire { session: u64, spec: VirtualDisplaySpec, epoch: u64, reply: oneshot::Sender<Result<Acquired, Refusal>> },
    /// The session goes back to the main display; its device's display goes if nobody else watches it.
    Release { session: u64, done: oneshot::Sender<()> },
    /// The session ended; `linger` if its connection was lost.
    Leave { session: u64, linger: bool },
    /// This Mac's user removes a display (`None`: all of them), or a device was forgotten.
    Remove { display_id: Option<u32>, owner: Option<Fingerprint>, message: String },
    /// The Mac's displays changed (by LanKVM or not): check that the main one is still as made.
    Reconfigured,
    /// LanKVM is quitting: macOS removes the displays as the process ends, so do nothing more.
    Shutdown,
}

/// Why a session doesn't get a display, for its viewer.
pub(crate) type Refusal = (DisplayReason, String);

/// The handle sessions and the UI use.
pub(crate) struct Displays {
    tx: Mutex<mpsc::Sender<Cmd>>,
    summary: Arc<Mutex<Vec<VirtualDisplayView>>>,
    /// Counts removals by this Mac's user (and policy changes): a request decided on before one
    /// mustn't make a display again after it.
    epoch: Arc<AtomicU64>,
}

impl Displays {
    /// `host` is told when the displays change. `host_fp` makes serial numbers unique to this copy
    /// of LanKVM.
    pub(crate) fn new(host: Weak<HostCtx>, host_fp: Fingerprint, screens: Box<dyn Screens>) -> Self {
        let (tx, rx) = mpsc::channel();
        let summary = Arc::new(Mutex::new(Vec::new()));
        let notify: Box<dyn Fn() + Send> = Box::new(move || {
            if let Some(host) = host.upgrade() {
                host.displays_changed();
            }
        });
        let epoch = Arc::new(AtomicU64::new(0));
        let worker = Worker::new(screens, host_fp, summary.clone(), epoch.clone(), notify);
        if let Err(e) = std::thread::Builder::new().name("lankvm-displays".into()).spawn(move || worker.run(rx)) {
            tracing::warn!("virtual displays unavailable: {e}");
        }
        Self { tx: Mutex::new(tx), summary, epoch }
    }

    /// See [`Cmd::Acquire`].
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.lock().unwrap().send(cmd);
    }

    pub(crate) async fn join(&self, session: u64, owner: Fingerprint, name: &str, events: tokio_mpsc::UnboundedSender<SessionEvt>) -> Option<Acquired> {
        let (reply, answer) = oneshot::channel();
        self.send(Cmd::Join { session, owner, name: display_name(name), events, reply });
        answer.await.ok().flatten()
    }

    pub(crate) async fn acquire(&self, session: u64, spec: VirtualDisplaySpec, epoch: u64) -> Result<Acquired, Refusal> {
        let (reply, answer) = oneshot::channel();
        self.send(Cmd::Acquire { session, spec, epoch, reply });
        answer.await.unwrap_or_else(|_| Err((DisplayReason::FAILED, "LanKVM's virtual displays stopped working.".into())))
    }

    /// Returns once the display is gone (or kept for the device's other sessions) and macOS has
    /// settled, so the main display is a real one again.
    pub(crate) async fn release(&self, session: u64) {
        let (done, settled) = oneshot::channel();
        self.send(Cmd::Release { session, done });
        let _ = settled.await;
    }

    pub(crate) fn leave(&self, session: u64, linger: bool) {
        self.send(Cmd::Leave { session, linger });
    }

    /// Never waits: the main thread calls it.
    pub(crate) fn remove(&self, display_id: Option<u32>, message: String) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.send(Cmd::Remove { display_id, owner: None, message });
    }

    pub(crate) fn forget(&self, owner: Fingerprint, message: String) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.send(Cmd::Remove { display_id: None, owner: Some(owner), message });
    }

    pub(crate) fn shutdown(&self) {
        self.send(Cmd::Shutdown);
    }

    /// The Mac's displays changed. Called on the main thread: never waits.
    pub(crate) fn reconfigured(&self) {
        self.send(Cmd::Reconfigured);
    }

    /// Never waits on a reconfiguration in progress.
    pub(crate) fn summary(&self) -> Vec<VirtualDisplayView> {
        self.summary.lock().unwrap().clone()
    }
}

/// The displays themselves: macOS's, or a fake for tests.
pub(crate) trait Screens: Send {
    fn supported(&self) -> bool;
    /// Creates a display; returns its id.
    fn create(&mut self, name: &str, serial: u32, mode: Mode) -> Result<u32>;
    fn set_mode(&mut self, id: u32, mode: Mode) -> Result<()>;
    /// Puts back the mode if macOS changed it; true if it had to.
    fn ensure_mode(&mut self, id: u32) -> Result<bool>;
    fn arrange(&mut self, id: u32, arrangement: virtual_display::Arrangement, previous_main: Option<u32>) -> Result<()>;
    /// Removes the display and waits until macOS has let go of it.
    fn remove(&mut self, id: u32);
    fn main_display(&self) -> u32;
    fn is_lankvm(&self, id: u32) -> bool;
    /// Whether the display still exists.
    fn is_online(&self, id: u32) -> bool;
    /// Whether the display is arranged so (see `virtual_display::arranged`).
    fn arranged(&self, id: u32, arrangement: virtual_display::Arrangement) -> bool;
    /// Lets the displays go without telling macOS (the process is about to end, which does).
    fn abandon(&mut self);
}

/// macOS's virtual displays.
#[derive(Default)]
pub(crate) struct RealScreens {
    displays: HashMap<u32, VirtualDisplay>,
}

impl Screens for RealScreens {
    fn supported(&self) -> bool {
        virtual_display::supported()
    }

    fn create(&mut self, name: &str, serial: u32, mode: Mode) -> Result<u32> {
        let display = VirtualDisplay::create(name, serial, mode)?;
        let id = display.id();
        self.displays.insert(id, display);
        Ok(id)
    }

    fn set_mode(&mut self, id: u32, mode: Mode) -> Result<()> {
        self.displays.get_mut(&id).ok_or_else(|| anyhow::anyhow!("display {id} is gone"))?.set_mode(mode)
    }

    fn ensure_mode(&mut self, id: u32) -> Result<bool> {
        self.displays.get(&id).map_or(Ok(false), |d| d.ensure_mode())
    }

    fn arrange(&mut self, id: u32, arrangement: virtual_display::Arrangement, previous_main: Option<u32>) -> Result<()> {
        virtual_display::arrange(id, arrangement, previous_main)
    }

    fn remove(&mut self, id: u32) {
        drop(self.displays.remove(&id));
        // macOS takes a few hundred milliseconds to drop it and pick a real main display again;
        // whoever looks for the main display next must find that one.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && (virtual_display::online_displays().contains(&id) || !virtual_display::is_active(virtual_display::main_display()))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn main_display(&self) -> u32 {
        virtual_display::main_display()
    }

    fn is_lankvm(&self, id: u32) -> bool {
        virtual_display::is_lankvm(id)
    }

    fn is_online(&self, id: u32) -> bool {
        virtual_display::online_displays().contains(&id)
    }

    fn arranged(&self, id: u32, arrangement: virtual_display::Arrangement) -> bool {
        virtual_display::arranged(id, arrangement)
    }

    fn abandon(&mut self) {
        for (_, display) in self.displays.drain() {
            std::mem::forget(display);
        }
    }
}

struct Entry {
    owner: Fingerprint,
    owner_name: String,
    id: u32,
    spec: VirtualDisplaySpec,
    /// When it goes if nobody watches it by then.
    remove_at: Option<Instant>,
}

struct Session {
    owner: Fingerprint,
    name: String,
    events: tokio_mpsc::UnboundedSender<SessionEvt>,
    /// Watches its device's display.
    watching: bool,
}

struct Worker {
    screens: Box<dyn Screens>,
    host_fp: Fingerprint,
    entries: Vec<Entry>,
    sessions: HashMap<u64, Session>,
    /// The device whose display is main or only now.
    primary: Option<Fingerprint>,
    /// The real display that was main before a virtual one took over, to give it back to.
    previous_main: Option<u32>,
    last_change: Option<Instant>,
    /// Counts the news sent and answers given (see [`DisplayNotice`]).
    seq: u64,
    epoch: Arc<AtomicU64>,
    /// When to look at the displays after a change (see [`RECONFIGURATION_SETTLE`]).
    reconfigured_at: Option<Instant>,
    last_repair: Option<Instant>,
    /// Held while any virtual display exists.
    awake: Option<DisplaysAwake>,
    summary: Arc<Mutex<Vec<VirtualDisplayView>>>,
    notify: Box<dyn Fn() + Send>,
}

impl Worker {
    fn new(
        screens: Box<dyn Screens>,
        host_fp: Fingerprint,
        summary: Arc<Mutex<Vec<VirtualDisplayView>>>,
        epoch: Arc<AtomicU64>,
        notify: Box<dyn Fn() + Send>,
    ) -> Self {
        Self {
            screens,
            host_fp,
            entries: Vec::new(),
            sessions: HashMap::new(),
            primary: None,
            previous_main: None,
            last_change: None,
            seq: 0,
            epoch,
            reconfigured_at: None,
            last_repair: None,
            awake: None,
            summary,
            notify,
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        loop {
            let wait = self.next_deadline().map_or(MODE_CHECK, |at| at.saturating_duration_since(Instant::now()).min(MODE_CHECK));
            match rx.recv_timeout(wait) {
                Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => self.tick(Instant::now()),
            }
        }
        // Quitting: macOS removes the displays and undoes the arrangement as the process ends.
        self.screens.abandon();
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.entries.iter().filter_map(|e| e.remove_at).chain(self.reconfigured_at).min()
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Join { session, owner, name, events, reply } => {
                let seq = self.next_seq();
                let watching = self.entries.iter_mut().find(|e| e.owner == owner).map(|e| {
                    e.remove_at = None;
                    Acquired { display_id: e.id, spec: e.spec, seq }
                });
                if let Some(a) = &watching {
                    tracing::info!(display = a.display_id, viewer = %name, "viewer is back for its virtual display");
                }
                self.sessions.insert(session, Session { owner, name, events, watching: watching.is_some() });
                let _ = reply.send(watching);
                if watching.is_some() {
                    self.publish();
                }
            }
            Cmd::Acquire { session, spec, epoch, reply } => {
                let result = if epoch != self.epoch.load(Ordering::Acquire) {
                    let message = format!("{}'s user just took its screen back, so it didn't make the display.", device_name());
                    Err((DisplayReason::REMOVED_BY_HOST, message))
                } else {
                    self.acquire(session, spec)
                };
                let _ = reply.send(result);
            }
            Cmd::Release { session, done } => {
                if let Some(s) = self.sessions.get_mut(&session).filter(|s| s.watching) {
                    s.watching = false;
                    let owner = s.owner;
                    self.drop_if_unwatched(owner, None);
                }
                let _ = done.send(());
            }
            Cmd::Leave { session, linger } => {
                if let Some(s) = self.sessions.remove(&session).filter(|s| s.watching) {
                    self.drop_if_unwatched(s.owner, linger.then(|| Instant::now() + LINGER));
                }
            }
            Cmd::Remove { display_id, owner, message } => {
                let doomed: Vec<Fingerprint> = self
                    .entries
                    .iter()
                    .filter(|e| display_id.is_none_or(|id| e.id == id) && owner.is_none_or(|o| e.owner == o))
                    .map(|e| e.owner)
                    .collect();
                for owner in doomed {
                    self.remove(owner, Some((DisplayReason::REMOVED_BY_HOST, message.clone())));
                }
            }
            Cmd::Reconfigured => {
                if self.reconfigured_at.is_none() {
                    self.reconfigured_at = Some(Instant::now() + RECONFIGURATION_SETTLE);
                }
            }
            Cmd::Shutdown => {}
        }
    }

    /// Removes displays whose time is up or that macOS removed, puts back modes macOS changed, and
    /// looks at the displays once a burst of changes is over.
    fn tick(&mut self, now: Instant) {
        while let Some(owner) = self.entries.iter().find(|e| e.remove_at.is_some_and(|at| at <= now)).map(|e| e.owner) {
            tracing::info!("nobody came back for the virtual display; removing it");
            self.remove(owner, None);
        }
        while let Some(owner) = self.entries.iter().find(|e| !self.screens.is_online(e.id)).map(|e| e.owner) {
            tracing::warn!("macOS removed a virtual display");
            let message = format!("{}'s virtual display went away.", device_name());
            self.remove(owner, Some((DisplayReason::DISPLAY_GONE, message)));
        }
        if self.reconfigured_at.is_some_and(|at| at <= now) {
            self.reconfigured_at = None;
            self.repair();
        }
        for i in 0..self.entries.len() {
            let id = self.entries[i].id;
            if let Err(e) = self.screens.ensure_mode(id) {
                tracing::warn!(display = id, "virtual display mode: {e:#}");
            }
        }
    }

    /// After displays changed: if someone else (the lid, a cable, Displays settings) undid the main
    /// display's arrangement, makes it again, now and then; and has every session check its display.
    fn repair(&mut self) {
        if let Some(i) = self.primary.and_then(|p| self.entries.iter().position(|e| e.owner == p)) {
            let (id, arrangement) = (self.entries[i].id, self.entries[i].spec.arrangement);
            if !self.screens.arranged(id, platform(arrangement)) && self.last_repair.is_none_or(|t| t.elapsed() >= MIN_REPAIR_INTERVAL) {
                tracing::info!(display = id, "the virtual display's arrangement changed; arranging it again");
                self.last_repair = Some(Instant::now());
                self.wait_turn();
                if let Err(e) = self.screens.arrange(id, platform(arrangement), self.previous_main) {
                    tracing::warn!(display = id, "arrange again: {e:#}");
                }
                self.last_change = Some(Instant::now());
            }
        }
        for s in self.sessions.values() {
            let _ = s.events.send(SessionEvt::Display(DisplayNotice::Layout));
        }
    }

    fn acquire(&mut self, session: u64, spec: VirtualDisplaySpec) -> Result<Acquired, Refusal> {
        let host = device_name();
        let Some(s) = self.sessions.get(&session) else {
            return Err((DisplayReason::FAILED, "The session ended.".into()));
        };
        let (owner, name) = (s.owner, s.name.clone());
        if !self.screens.supported() {
            return Err((DisplayReason::UNSUPPORTED, format!("{host}'s version of macOS can't make virtual displays.")));
        }
        let existing = self.entries.iter().position(|e| e.owner == owner);
        if existing.is_none() && self.entries.len() >= MAX_DISPLAYS {
            let others: Vec<&str> = self.entries.iter().map(|e| e.owner_name.as_str()).collect();
            let message = format!("{host} already shows {MAX_DISPLAYS} virtual displays (for {}).", others.join(" and "));
            return Err((DisplayReason::TOO_MANY, message));
        }
        if let Some(i) = existing
            && self.entries[i].spec == spec
        {
            // Nothing to change (a viewer re-applying its choice, or another of its windows).
            return Ok(self.watch(session, i));
        }
        self.wait_turn();
        let mode = Mode { width: spec.width, height: spec.height, hidpi: spec.hidpi, refresh_hz: spec.refresh_hz };
        let fail = |e: anyhow::Error| {
            tracing::warn!("virtual display: {e:#}");
            (DisplayReason::FAILED, format!("{host} couldn't make the virtual display ({e:#})."))
        };
        let i = match existing {
            Some(i) => {
                let entry = &self.entries[i];
                if (entry.spec.width, entry.spec.height, entry.spec.hidpi, entry.spec.refresh_hz) != (spec.width, spec.height, spec.hidpi, spec.refresh_hz) {
                    self.screens.set_mode(entry.id, mode).map_err(fail)?;
                }
                i
            }
            None => {
                let id = self.screens.create(&format!("LanKVM ({name})"), serial_for(&self.host_fp, &owner), mode).map_err(fail)?;
                tracing::info!(display = id, viewer = %name, ?spec, "virtual display created");
                if self.awake.is_none() {
                    self.awake = DisplaysAwake::new("LanKVM virtual display");
                }
                self.entries.push(Entry { owner, owner_name: name.clone(), id, spec: VirtualDisplaySpec { arrangement: Arrangement::EXTEND, ..spec }, remove_at: None });
                self.entries.len() - 1
            }
        };
        // The new size counts even if the arrangement fails below.
        let sized = VirtualDisplaySpec { arrangement: self.entries[i].spec.arrangement, ..spec };
        self.entries[i].spec = sized;
        if let Err(e) = self.arrange(i, spec.arrangement) {
            let message = fail(e);
            if existing.is_none() {
                // Just made: don't leave it behind half arranged.
                self.remove(owner, None);
            } else {
                self.followers_changed(owner, session, String::new());
                self.changed();
            }
            return Err(message);
        }
        self.followers_changed(owner, session, String::new());
        let acquired = self.watch(session, i);
        self.changed();
        Ok(acquired)
    }

    /// Counts the session among the display's watchers.
    fn watch(&mut self, session: u64, i: usize) -> Acquired {
        let seq = self.next_seq();
        let entry = &mut self.entries[i];
        entry.remove_at = None;
        let acquired = Acquired { display_id: entry.id, spec: entry.spec, seq };
        if let Some(s) = self.sessions.get_mut(&session) {
            s.watching = true;
        }
        self.publish();
        acquired
    }

    /// Gives the display at `i` its arrangement. Taking the main role from another device's display
    /// leaves that one extended, and tells its sessions, once the new arrangement worked (if it
    /// didn't, the other display gets its role back).
    fn arrange(&mut self, i: usize, arrangement: Arrangement) -> Result<()> {
        let owner = self.entries[i].owner;
        let mut demoted = None;
        if arrangement != Arrangement::EXTEND {
            if let Some(other) = self.primary.filter(|p| *p != owner)
                && let Some(j) = self.entries.iter().position(|e| e.owner == other)
            {
                self.screens.arrange(self.entries[j].id, virtual_display::Arrangement::Extend, self.previous_main)?;
                demoted = Some(j);
            }
            let main = self.screens.main_display();
            if !self.screens.is_lankvm(main) {
                self.previous_main = Some(main);
            }
        }
        let id = self.entries[i].id;
        self.last_change = Some(Instant::now());
        if let Err(e) = self.screens.arrange(id, platform(arrangement), self.previous_main) {
            if let Some(j) = demoted {
                let (other, role) = (self.entries[j].id, self.entries[j].spec.arrangement);
                if let Err(e) = self.screens.arrange(other, platform(role), self.previous_main) {
                    tracing::warn!(display = other, "give the main role back: {e:#}");
                }
            }
            return Err(e);
        }
        self.entries[i].spec.arrangement = arrangement;
        if let Some(j) = demoted {
            self.entries[j].spec.arrangement = Arrangement::EXTEND;
            let other = self.entries[j].owner;
            let message = format!("{}'s display is the main display on {} now.", self.entries[i].owner_name, device_name());
            self.followers_changed(other, 0, message);
        }
        if arrangement != Arrangement::EXTEND {
            self.primary = Some(owner);
        } else if self.primary == Some(owner) {
            self.primary = None;
        }
        Ok(())
    }

    /// Tells `owner`'s sessions watching its display (except `except`) how it is now.
    fn followers_changed(&mut self, owner: Fingerprint, except: u64, message: String) {
        let seq = self.next_seq();
        let Some(entry) = self.entries.iter().find(|e| e.owner == owner) else { return };
        for (_, s) in self.sessions.iter().filter(|(id, s)| **id != except && s.owner == owner && s.watching) {
            let notice = DisplayNotice::Changed { display_id: entry.id, spec: entry.spec, message: message.clone(), seq };
            let _ = s.events.send(SessionEvt::Display(notice));
        }
    }

    /// The device's display goes now (`at` None) or then, unless one of its sessions watches it.
    fn drop_if_unwatched(&mut self, owner: Fingerprint, at: Option<Instant>) {
        if self.sessions.values().any(|s| s.owner == owner && s.watching) {
            return;
        }
        match at {
            Some(at) => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.owner == owner) {
                    tracing::info!(display = e.id, "keeping the virtual display for {} s in case its viewer comes back", LINGER.as_secs());
                    e.remove_at = Some(at);
                }
                self.publish();
            }
            None => self.remove(owner, None),
        }
    }

    /// Removes `owner`'s display, giving this Mac's own displays back their roles first: removing
    /// the display the others mirror is better not left to chance. Sessions still watching it are
    /// told `why` once it's gone and a real display is main again (so the main display they turn
    /// to is that one).
    fn remove(&mut self, owner: Fingerprint, why: Option<(DisplayReason, String)>) {
        let Some(i) = self.entries.iter().position(|e| e.owner == owner) else { return };
        let entry = self.entries.remove(i);
        let mut evicted = Vec::new();
        for s in self.sessions.values_mut().filter(|s| s.owner == owner && s.watching) {
            s.watching = false;
            evicted.push(s.events.clone());
        }
        if self.primary == Some(owner) {
            self.wait_turn();
            if let Err(e) = self.screens.arrange(entry.id, virtual_display::Arrangement::Extend, self.previous_main) {
                tracing::warn!(display = entry.id, "give the main display back: {e:#}");
            }
            self.primary = None;
        }
        self.screens.remove(entry.id);
        tracing::info!(display = entry.id, viewer = %entry.owner_name, "virtual display removed");
        self.last_change = Some(Instant::now());
        if self.entries.is_empty() {
            self.awake = None;
            self.previous_main = None;
        }
        let seq = self.next_seq();
        let (reason, message) = why.unwrap_or_else(|| (DisplayReason::DISPLAY_GONE, format!("{}'s virtual display went away.", device_name())));
        for events in evicted {
            let notice = DisplayNotice::Removed { display_id: entry.id, reason, message: message.clone(), seq };
            let _ = events.send(SessionEvt::Display(notice));
        }
        self.changed();
    }

    /// Waits until enough time has passed since the last change (see [`MIN_CHANGE_INTERVAL`]).
    fn wait_turn(&self) {
        if let Some(last) = self.last_change {
            let next = last + MIN_CHANGE_INTERVAL;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            }
        }
    }

    /// The displays changed: publish, and tell every session (those watching the main display
    /// check theirs is still shown) and the UI.
    fn changed(&self) {
        self.publish();
        for s in self.sessions.values() {
            let _ = s.events.send(SessionEvt::Display(DisplayNotice::Layout));
        }
        (self.notify)();
    }

    fn publish(&self) {
        let summary: Vec<VirtualDisplayView> = self
            .entries
            .iter()
            .map(|e| VirtualDisplayView {
                display_id: e.id,
                owner: e.owner_name.clone(),
                owner_id: transport::identity::short_hex(&e.owner),
                width: e.spec.width,
                height: e.spec.height,
                hidpi: e.spec.hidpi,
                arrangement: arrangement_name(e.spec.arrangement),
                in_use: self.sessions.values().any(|s| s.owner == e.owner && s.watching),
            })
            .collect();
        let mut current = self.summary.lock().unwrap();
        if *current != summary {
            *current = summary;
            drop(current);
            (self.notify)();
        }
    }
}

fn platform(arrangement: Arrangement) -> virtual_display::Arrangement {
    match arrangement {
        Arrangement::MAIN => virtual_display::Arrangement::Main,
        Arrangement::ONLY => virtual_display::Arrangement::Only,
        _ => virtual_display::Arrangement::Extend,
    }
}

pub(crate) fn arrangement_name(arrangement: Arrangement) -> &'static str {
    match arrangement {
        Arrangement::MAIN => "main",
        Arrangement::ONLY => "only",
        _ => "extend",
    }
}

/// A display serial number for `viewer`'s display on this Mac: the same each time, so macOS
/// remembers its arrangement, and different for each pair of devices, because macOS refuses a
/// second display with a serial number in use.
fn serial_for(host: &Fingerprint, viewer: &Fingerprint) -> u32 {
    // FNV-1a over both identities, in order.
    let hash = host.iter().chain(viewer).fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(*b)).wrapping_mul(0x0100_0193));
    hash.max(1)
}

/// The viewer's name as macOS shows it in Displays settings: printable and short, whatever the
/// viewer sent.
fn display_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).take(48).collect();
    let clean = clean.trim();
    if clean.is_empty() { "Mac".into() } else { clean.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Displays in memory: records what was done to them.
    #[derive(Default)]
    struct Fake {
        next_id: u32,
        modes: HashMap<u32, Mode>,
        arranged: Arc<Mutex<Vec<(u32, virtual_display::Arrangement)>>>,
        main: u32,
        log: Arc<Mutex<Vec<String>>>,
        fail_arrange: bool,
    }

    impl Screens for Fake {
        fn supported(&self) -> bool {
            true
        }
        fn create(&mut self, _name: &str, _serial: u32, mode: Mode) -> Result<u32> {
            self.next_id += 1;
            let id = 100 + self.next_id;
            self.modes.insert(id, mode);
            self.log.lock().unwrap().push(format!("create {id} {}x{}", mode.width, mode.height));
            Ok(id)
        }
        fn set_mode(&mut self, id: u32, mode: Mode) -> Result<()> {
            self.modes.insert(id, mode);
            self.log.lock().unwrap().push(format!("mode {id} {}x{}", mode.width, mode.height));
            Ok(())
        }
        fn ensure_mode(&mut self, _id: u32) -> Result<bool> {
            Ok(false)
        }
        fn arrange(&mut self, id: u32, arrangement: virtual_display::Arrangement, previous_main: Option<u32>) -> Result<()> {
            if self.fail_arrange {
                anyhow::bail!("refused");
            }
            self.arranged.lock().unwrap().push((id, arrangement));
            match arrangement {
                virtual_display::Arrangement::Extend if self.main == id => self.main = previous_main.unwrap_or(1),
                virtual_display::Arrangement::Main | virtual_display::Arrangement::Only => self.main = id,
                _ => {}
            }
            Ok(())
        }
        fn remove(&mut self, id: u32) {
            self.modes.remove(&id);
            if self.main == id {
                self.main = 1;
            }
            self.log.lock().unwrap().push(format!("remove {id}"));
        }
        fn main_display(&self) -> u32 {
            self.main
        }
        fn is_lankvm(&self, id: u32) -> bool {
            id > 100
        }
        fn is_online(&self, id: u32) -> bool {
            self.modes.contains_key(&id)
        }
        fn arranged(&self, _id: u32, _arrangement: virtual_display::Arrangement) -> bool {
            true
        }
        fn abandon(&mut self) {}
    }

    fn spec(width: u32, arrangement: Arrangement) -> VirtualDisplaySpec {
        VirtualDisplaySpec { width, height: 2560, hidpi: true, refresh_hz: 60, arrangement }
    }

    struct Harness {
        worker: Worker,
        log: Arc<Mutex<Vec<String>>>,
        events: HashMap<u64, tokio_mpsc::UnboundedReceiver<SessionEvt>>,
    }

    impl Harness {
        fn new() -> Self {
            let fake = Fake { main: 1, ..Default::default() };
            let log = fake.log.clone();
            let mut worker = Worker::new(Box::new(fake), [9; 32], Arc::new(Mutex::new(Vec::new())), Arc::default(), Box::new(|| {}));
            // Tests don't wait between changes.
            worker.last_change = None;
            Self { worker, log, events: HashMap::new() }
        }

        fn join(&mut self, session: u64, owner: u8) -> Option<Acquired> {
            let (tx, rx) = tokio_mpsc::unbounded_channel();
            self.events.insert(session, rx);
            let (reply, mut answer) = oneshot::channel();
            self.worker.handle(Cmd::Join { session, owner: [owner; 32], name: format!("Mac {owner}"), events: tx, reply });
            answer.try_recv().unwrap()
        }

        fn acquire(&mut self, session: u64, spec: VirtualDisplaySpec) -> Result<Acquired, Refusal> {
            self.worker.last_change = None;
            let (reply, mut answer) = oneshot::channel();
            let epoch = self.worker.epoch.load(Ordering::Acquire);
            self.worker.handle(Cmd::Acquire { session, spec, epoch, reply });
            answer.try_recv().unwrap()
        }

        fn release(&mut self, session: u64) {
            self.worker.last_change = None;
            let (done, _) = oneshot::channel();
            self.worker.handle(Cmd::Release { session, done });
        }

        /// Notices for `session` other than layout changes.
        fn notices(&mut self, session: u64) -> Vec<DisplayNotice> {
            let rx = self.events.get_mut(&session).unwrap();
            let mut out = Vec::new();
            while let Ok(SessionEvt::Display(n)) = rx.try_recv() {
                if n != DisplayNotice::Layout {
                    out.push(n);
                }
            }
            out
        }

        fn log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
    }

    #[test]
    fn a_device_gets_one_display_shared_by_its_sessions() {
        let mut h = Harness::new();
        assert_eq!(h.join(1, 7), None);
        let a = h.acquire(1, spec(6144, Arrangement::ONLY)).unwrap();
        assert_eq!(h.log(), ["create 101 6144x2560"]);
        assert_eq!(h.worker.screens.main_display(), a.display_id);
        let same = |x: Acquired| (x.display_id, x.spec);
        // A second window of the same device finds it at once.
        assert_eq!(h.join(2, 7).map(same), Some(same(a)));
        // Asking again for the same thing changes nothing.
        assert_eq!(h.acquire(2, spec(6144, Arrangement::ONLY)).map(same), Ok(same(a)));
        assert!(h.log().is_empty());
        // A new size applies in place, and the other window follows it.
        let b = h.acquire(2, spec(3840, Arrangement::ONLY)).unwrap();
        assert_eq!(b.display_id, a.display_id);
        assert_eq!(h.log(), ["mode 101 3840x2560"]);
        let notices = h.notices(1);
        assert!(
            matches!(&notices[..], [DisplayNotice::Changed { display_id: 101, spec: s, message, .. }] if *s == spec(3840, Arrangement::ONLY) && message.is_empty()),
            "{notices:?}"
        );
        assert!(h.notices(2).is_empty(), "the session that asked doesn't hear about its own change");
        // It goes when nobody watches it any more.
        h.release(1);
        assert!(h.log().is_empty());
        h.release(2);
        assert_eq!(h.log(), ["remove 101"]);
        assert_eq!(h.worker.screens.main_display(), 1, "the real display is main again");
    }

    #[test]
    fn a_lost_connection_keeps_the_display_for_a_while() {
        let mut h = Harness::new();
        h.join(1, 7);
        let a = h.acquire(1, spec(6144, Arrangement::MAIN)).unwrap();
        h.log();
        h.worker.handle(Cmd::Leave { session: 1, linger: true });
        assert!(h.log().is_empty());
        // The viewer comes back in time: same display.
        assert_eq!(h.join(2, 7).map(|x| (x.display_id, x.spec)), Some((a.display_id, a.spec)));
        h.worker.tick(Instant::now() + LINGER * 2);
        assert!(h.log().is_empty());
        // It leaves for good: after the linger time the display goes.
        h.worker.handle(Cmd::Leave { session: 2, linger: true });
        h.worker.tick(Instant::now() + LINGER / 2);
        assert!(h.log().is_empty());
        h.worker.tick(Instant::now() + LINGER * 2);
        assert_eq!(h.log(), ["remove 101"]);
        // Closing the connection removes it at once.
        h.join(3, 7);
        h.acquire(3, spec(6144, Arrangement::EXTEND)).unwrap();
        h.worker.handle(Cmd::Leave { session: 3, linger: false });
        assert_eq!(h.log(), ["create 102 6144x2560", "remove 102"]);
    }

    #[test]
    fn a_request_after_its_session_ended_is_refused() {
        let mut h = Harness::new();
        h.join(1, 7);
        h.worker.handle(Cmd::Leave { session: 1, linger: false });
        assert!(h.acquire(1, spec(6144, Arrangement::ONLY)).is_err());
        assert!(h.log().is_empty(), "nothing made for a session that's gone");
    }

    #[test]
    fn one_device_at_a_time_is_main() {
        let mut h = Harness::new();
        h.join(1, 7);
        h.join(2, 8);
        let a = h.acquire(1, spec(6144, Arrangement::ONLY)).unwrap();
        let b = h.acquire(2, spec(5120, Arrangement::MAIN)).unwrap();
        assert_eq!(h.worker.screens.main_display(), b.display_id);
        // The first device's display stays, extended, and its session hears so.
        let notices = h.notices(1);
        assert!(
            matches!(&notices[..], [DisplayNotice::Changed { display_id, spec: s, message, .. }] if *display_id == a.display_id && s.arrangement == Arrangement::EXTEND && message.contains("Mac 8")),
            "{notices:?}"
        );
        // Only two at once.
        h.join(3, 9);
        let (reason, err) = h.acquire(3, spec(6144, Arrangement::EXTEND)).unwrap_err();
        assert_eq!(reason, DisplayReason::TOO_MANY);
        assert!(err.contains("Mac 7 and Mac 8"), "{err}");
        // The main one leaving gives the real display the main role back.
        h.release(2);
        assert_eq!(h.worker.screens.main_display(), 1);
        assert_eq!(h.worker.primary, None);
    }

    #[test]
    fn removing_on_the_host_tells_the_viewer() {
        let mut h = Harness::new();
        h.join(1, 7);
        let a = h.acquire(1, spec(6144, Arrangement::ONLY)).unwrap();
        h.log();
        h.worker.handle(Cmd::Remove { display_id: Some(a.display_id), owner: None, message: "Removed on Studio.".into() });
        assert_eq!(h.log(), ["remove 101"]);
        let notices = h.notices(1);
        assert!(
            matches!(&notices[..], [DisplayNotice::Removed { display_id, reason: DisplayReason::REMOVED_BY_HOST, message, .. }] if *display_id == a.display_id && message == "Removed on Studio."),
            "{notices:?}"
        );
        // Told once this Mac's own display is main again.
        assert_eq!(h.worker.screens.main_display(), 1);
        // Its session no longer counts as watching: leaving later does nothing more.
        h.worker.handle(Cmd::Leave { session: 1, linger: false });
        assert!(h.log().is_empty());
    }

    #[test]
    fn a_request_decided_before_the_host_took_its_screen_back_is_refused() {
        let mut h = Harness::new();
        h.join(1, 7);
        let epoch = h.worker.epoch.load(Ordering::Acquire);
        // This Mac's user removes the displays (Displays::remove bumps the epoch first).
        h.worker.epoch.fetch_add(1, Ordering::AcqRel);
        h.worker.handle(Cmd::Remove { display_id: None, owner: None, message: "Stopped.".into() });
        let (reply, mut answer) = oneshot::channel();
        h.worker.handle(Cmd::Acquire { session: 1, spec: spec(6144, Arrangement::ONLY), epoch, reply });
        let (reason, _) = answer.try_recv().unwrap().unwrap_err();
        assert_eq!(reason, DisplayReason::REMOVED_BY_HOST);
        assert!(h.log().is_empty(), "nothing made");
    }

    #[test]
    fn a_takeover_that_fails_gives_the_main_role_back() {
        let mut h = Harness::new();
        h.join(1, 7);
        h.join(2, 8);
        let a = h.acquire(1, spec(6144, Arrangement::ONLY)).unwrap();
        h.notices(1);
        // Arranging the second device's display fails (its creation worked).
        let fake = Fake { main: a.display_id, fail_arrange: false, ..Default::default() };
        let _ = fake;
        h.worker.primary = Some([7; 32]);
        struct Failing(Box<dyn Screens>, u32);
        impl Screens for Failing {
            fn supported(&self) -> bool {
                true
            }
            fn create(&mut self, name: &str, serial: u32, mode: Mode) -> Result<u32> {
                self.0.create(name, serial, mode)
            }
            fn set_mode(&mut self, id: u32, mode: Mode) -> Result<()> {
                self.0.set_mode(id, mode)
            }
            fn ensure_mode(&mut self, id: u32) -> Result<bool> {
                self.0.ensure_mode(id)
            }
            fn arrange(&mut self, id: u32, arrangement: virtual_display::Arrangement, previous_main: Option<u32>) -> Result<()> {
                if id != self.1 && arrangement != virtual_display::Arrangement::Extend {
                    anyhow::bail!("refused");
                }
                self.0.arrange(id, arrangement, previous_main)
            }
            fn remove(&mut self, id: u32) {
                self.0.remove(id)
            }
            fn main_display(&self) -> u32 {
                self.0.main_display()
            }
            fn is_lankvm(&self, id: u32) -> bool {
                self.0.is_lankvm(id)
            }
            fn is_online(&self, id: u32) -> bool {
                self.0.is_online(id)
            }
            fn arranged(&self, id: u32, arrangement: virtual_display::Arrangement) -> bool {
                self.0.arranged(id, arrangement)
            }
            fn abandon(&mut self) {}
        }
        let inner = std::mem::replace(&mut h.worker.screens, Box::new(Fake::default()));
        h.worker.screens = Box::new(Failing(inner, a.display_id));
        assert!(h.acquire(2, spec(5120, Arrangement::MAIN)).is_err());
        // The first device keeps its role, its arrangement and its main display, and hears nothing.
        assert_eq!(h.worker.primary, Some([7; 32]));
        assert_eq!(h.worker.entries[0].spec.arrangement, Arrangement::ONLY);
        assert_eq!(h.worker.screens.main_display(), a.display_id);
        assert!(h.notices(1).is_empty());
    }

    #[test]
    fn a_failed_arrangement_leaves_no_new_display_behind() {
        let mut h = Harness::new();
        h.join(1, 7);
        let mut fake = Fake { main: 1, fail_arrange: true, ..Default::default() };
        let log = fake.log.clone();
        fake.next_id = 0;
        h.worker.screens = Box::new(fake);
        let (reason, err) = h.acquire(1, spec(6144, Arrangement::ONLY)).unwrap_err();
        assert_eq!(reason, DisplayReason::FAILED);
        assert!(err.contains("refused"), "{err}");
        assert_eq!(*log.lock().unwrap(), ["create 101 6144x2560", "remove 101"]);
        assert!(h.worker.entries.is_empty());
    }

    #[test]
    fn serials_differ_per_pair_and_are_never_zero() {
        let a: Fingerprint = [1; 32];
        let b: Fingerprint = [2; 32];
        assert_eq!(serial_for(&a, &b), serial_for(&a, &b));
        assert_ne!(serial_for(&a, &b), serial_for(&b, &a));
        assert_ne!(serial_for(&a, &b), serial_for(&a, &a));
        let zero: Fingerprint = [0; 32];
        assert_ne!(serial_for(&zero, &zero), 0);
    }

    #[test]
    fn display_names_are_tamed() {
        assert_eq!(display_name("Alex's MacBook Pro"), "Alex's MacBook Pro");
        assert_eq!(display_name("a\nb\u{7}c"), "abc");
        assert_eq!(display_name(&"x".repeat(500)).len(), 48);
        assert_eq!(display_name(" \t "), "Mac");
    }
}
