//! The shared clipboard: while a Mac controls another, what is copied on either one can be pasted
//! on the other.
//!
//! One [`Clipboard`] per core watches this Mac's clipboard and keeps it the same as the Macs it
//! shares it with. Each session that shares it is a link: the viewer controls the host, and the
//! viewer's user lets it share (see [`SessionClipboard`]). A change here goes to every link; a
//! change that comes over a link is written here and passed on to the other links, so a chain of
//! Macs (A controls B, whose window controls C) has one clipboard.
//!
//! - Every copy has an id that travels with it: it isn't sent back where it came from, and a ring
//!   of Macs stops at the first one that has it.
//! - As a link starts, both Macs offer what they have, and the later copy wins on both. After
//!   that a change always wins, except when both Macs changed their clipboards at once (see
//!   `protocol::ClipboardHeader::prior`).
//! - Only the change count is polled, which costs next to nothing. The contents are read only
//!   when they changed while a link shares them (or as one starts), never otherwise.
//! - Each transfer goes on a stream of its own, so a big image never holds up control messages
//!   or input, and a newer copy cancels the one still on its way.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use platform_mac::clipboard::{Pasteboard, png_from_tiff};
use protocol::{
    CLIPBOARD_TYPES, ClipboardHeader, ClipboardItem, MAX_CLIPBOARD_BYTES, MAX_CLIPBOARD_BYTES_INTERNET, MAX_CLIPBOARD_HEADER_LEN,
    STREAM_CLIPBOARD,
};
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::watch;

/// How often the clipboard is looked at while a link shares it. A copy reaches the other Mac
/// within this (plus the trip); the viewer also looks at once when its window gains or loses the
/// keyboard, just before the user pastes on the other side.
const POLL: Duration = Duration::from_millis(250);
/// And while none does: only the change count, to know when the contents changed, for the
/// later-copy-wins rule as a link starts.
const IDLE_POLL: Duration = Duration::from_secs(1);
/// A change is read this long after the change count said so at the earliest: an app empties the
/// clipboard (which changes the count) before it puts its data there, one type after another.
const SETTLE: Duration = Duration::from_millis(40);
/// A transfer that arrives before this side shares too (each Mac learns that control started on
/// its own, a moment apart) waits this long for it, then is dropped.
const LINK_GRACE: Duration = Duration::from_secs(2);
/// Time a stream gets to say what it carries, and a clipboard transfer to send its header.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Transfers read at once per session; more are refused (the sender cancels a transfer when a
/// newer copy replaces it, so two cover a transfer winding down and the next).
const MAX_READERS: usize = 2;
/// The image type the clipboard often has besides PNG, or instead of it.
const TIFF: &str = "public.tiff";
const PNG: &str = "public.png";

/// Which clipboard a core shares.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipboardBackend {
    /// This Mac's clipboard, the one everybody copies to and pastes from.
    System,
    /// A pasteboard of LanKVM's own, by name (`LANKVM_CLIPBOARD=<name>`): for tests, and for a
    /// second copy of LanKVM on the same Mac, which can then share it with the first.
    Named(String),
    /// None (`LANKVM_CLIPBOARD=off`).
    Off,
}

impl ClipboardBackend {
    pub fn from_env() -> Self {
        Self::parse(std::env::var("LANKVM_CLIPBOARD").ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") => Self::System,
            Some("off") => Self::Off,
            Some(name) => Self::Named(name.to_string()),
        }
    }
}

/// What a clipboard holds that Macs share: (type, data) in the clipboard's order.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Content {
    pub(crate) items: Vec<(String, Vec<u8>)>,
}

impl Content {
    fn len(&self) -> u64 {
        self.items.iter().map(|(_, data)| data.len() as u64).sum()
    }
}

/// A clipboard: this Mac's, one of LanKVM's own, or one in memory (tests).
pub(crate) trait Board: Send {
    fn change_count(&mut self) -> i64;
    /// The types Macs share (see `protocol::CLIPBOARD_TYPES`).
    fn read(&mut self) -> Content;
    /// Replaces the contents (nothing: empties it). Returns the change count that makes.
    fn write(&mut self, content: &Content) -> i64;
}

struct SystemBoard(Pasteboard);

impl Board for SystemBoard {
    fn change_count(&mut self) -> i64 {
        self.0.change_count()
    }

    fn read(&mut self) -> Content {
        let mut kinds = CLIPBOARD_TYPES.to_vec();
        kinds.push(TIFF);
        let mut items = self.0.read(&kinds);
        // An image goes as PNG only: a TIFF is often uncompressed, many times its size.
        if let Some(i) = items.iter().position(|(kind, _)| kind == TIFF) {
            let (_, tiff) = items.remove(i);
            if !items.iter().any(|(kind, _)| kind == PNG)
                && let Some(png) = png_from_tiff(&tiff)
            {
                items.insert(i, (PNG.to_string(), png));
            }
        }
        Content { items }
    }

    fn write(&mut self, content: &Content) -> i64 {
        self.0.write(&content.items)
    }
}

/// One copy, as it goes from Mac to Mac.
#[derive(Debug, Clone)]
struct Clip {
    id: u64,
    /// When it was copied (wall clock, µs since 1970), on the Mac where it was.
    copied_us: u64,
    /// Not 0: a clipboard this many bytes big, too big to share. `content` is then empty.
    too_large: u64,
    content: Arc<Content>,
}

impl Clip {
    /// Which of two copies is the later one.
    fn key(&self) -> (u64, u64) {
        (self.copied_us, self.id)
    }

    fn is_empty(&self) -> bool {
        self.content.items.is_empty()
    }
}

/// What a link sends next.
#[derive(Debug, Clone)]
struct Outgoing {
    clip: Clip,
    /// The copy this Mac had before (see `protocol::ClipboardHeader::prior`).
    prior: u64,
    offer: bool,
}

/// Something a session may tell its user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Note {
    /// This Mac's clipboard (bytes) is too big to share over the link: the other Mac's was
    /// emptied instead.
    TooLargeToSend(u64),
    /// The other Mac's clipboard (bytes) was too big to share: this Mac's was emptied.
    TooLargeToReceive(u64),
}

pub(crate) type Notes = Arc<dyn Fn(Note) + Send + Sync>;

enum Cmd {
    Link { peer: u64, cap: u64, out: watch::Sender<Option<Outgoing>>, notes: Notes },
    Unlink(u64),
    Received { peer: u64, clip: Clip, prior: u64, offer: bool },
    /// Look now (the user may be about to paste on the other Mac).
    Check,
}

/// This Mac's side of the shared clipboard: a thread that owns the clipboard, and what sessions
/// use to share it.
pub(crate) struct Clipboard {
    cmds: std_mpsc::Sender<Cmd>,
    /// It is this Mac's own clipboard, not one of LanKVM's by name.
    system: bool,
    /// The later-copy key of what the clipboard has (see [`Clip::key`]), for checking an offer
    /// before reading it.
    newest: Arc<Mutex<(u64, u64)>>,
    next_peer: AtomicU64,
}

impl Clipboard {
    /// None if the core shares no clipboard.
    pub(crate) fn start(backend: &ClipboardBackend) -> Option<Arc<Self>> {
        match backend {
            ClipboardBackend::Off => None,
            ClipboardBackend::System => Some(Self::with_board(Box::new(SystemBoard(Pasteboard::general())), true)),
            ClipboardBackend::Named(name) => Some(Self::with_board(Box::new(SystemBoard(Pasteboard::named(name))), false)),
        }
    }

    fn with_board(board: Box<dyn Board>, system: bool) -> Arc<Self> {
        let newest = Arc::new(Mutex::new((0, 0)));
        let (cmds, rx) = std_mpsc::channel();
        let hub = Hub::new(board, newest.clone());
        // Ends once the core and every link let go of it.
        std::thread::Builder::new().name("clipboard".into()).spawn(move || hub.run(rx)).expect("spawn the clipboard thread");
        Arc::new(Self { cmds, system, newest, next_peer: AtomicU64::new(1) })
    }

    /// Looks at the clipboard now rather than at the next poll.
    pub(crate) fn check(&self) {
        let _ = self.cmds.send(Cmd::Check);
    }

    fn link(&self, peer: u64, conn: Connection, cap: u64, sent: Arc<AtomicU64>, notes: Notes) -> Link {
        let (out, rx) = watch::channel(None);
        let task = tokio::spawn(send_transfers(conn, rx, sent));
        let _ = self.cmds.send(Cmd::Link { peer, cap, out, notes });
        Link { cmds: self.cmds.clone(), peer, task }
    }

    fn received(&self, peer: u64, clip: Clip, prior: u64, offer: bool) {
        let _ = self.cmds.send(Cmd::Received { peer, clip, prior, offer });
    }

    fn newest(&self) -> (u64, u64) {
        *self.newest.lock().unwrap()
    }
}

/// A session's link while it shares the clipboard: the task sending its transfers. Dropping it
/// stops sharing.
struct Link {
    cmds: std_mpsc::Sender<Cmd>,
    peer: u64,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.cmds.send(Cmd::Unlink(self.peer));
        self.task.abort();
    }
}

/// A link as the clipboard thread sees it.
struct LinkState {
    peer: u64,
    /// Largest clipboard it shares.
    cap: u64,
    out: watch::Sender<Option<Outgoing>>,
    notes: Notes,
    /// The copy it has: the last sent there or received from there.
    has: Option<u64>,
}

/// What the clipboard has, as far as this Mac knows.
struct Current {
    id: u64,
    copied_us: u64,
    too_large: u64,
    /// None: not read yet, or let go of while nothing shares it (read again when needed).
    content: Option<Arc<Content>>,
}

/// The clipboard thread's state.
struct Hub {
    board: Box<dyn Board>,
    /// The change count the clipboard had when last looked at (or written).
    count: i64,
    /// When the contents changed: when this Mac noticed, or, for a copy from another Mac, when it
    /// was copied there. Before the first change it noticed, when it started.
    changed_us: u64,
    /// When this Mac noticed the change, to read it once it settled (see [`SETTLE`]).
    noticed: Option<Instant>,
    settle: Duration,
    /// None: changed since, not looked at yet.
    current: Option<Current>,
    /// What it had before that change: if the contents turn out the same (an app wrote them
    /// again, or Universal Clipboard brought what another Mac had already sent), it's still that
    /// copy.
    replaced: Option<Current>,
    /// The id of the newest copy, kept while `current` is None, and of the one before it.
    last_id: u64,
    prior: u64,
    links: Vec<LinkState>,
    newest: Arc<Mutex<(u64, u64)>>,
}

impl Hub {
    fn new(mut board: Box<dyn Board>, newest: Arc<Mutex<(u64, u64)>>) -> Self {
        let count = board.change_count();
        let hub = Self {
            board,
            count,
            changed_us: now_us(),
            noticed: None,
            settle: SETTLE,
            current: None,
            replaced: None,
            last_id: 0,
            prior: 0,
            links: Vec::new(),
            newest,
        };
        hub.publish();
        hub
    }

    fn run(mut self, cmds: std_mpsc::Receiver<Cmd>) {
        let mut polled = Instant::now();
        loop {
            let every = if self.links.is_empty() { IDLE_POLL } else { POLL };
            match cmds.recv_timeout(every.saturating_sub(polled.elapsed())) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if polled.elapsed() >= every {
                self.poll();
                polled = Instant::now();
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Link { peer, cap, out, notes } => self.link(LinkState { peer, cap, out, notes, has: None }),
            Cmd::Unlink(peer) => {
                self.links.retain(|l| l.peer != peer);
                // Nothing shares it now: don't hold on to what it has (read again when needed).
                if self.links.is_empty()
                    && let Some(current) = &mut self.current
                {
                    current.content = None;
                }
            }
            Cmd::Received { peer, clip, prior, offer } => self.received(peer, clip, prior, offer),
            Cmd::Check => self.poll(),
        }
    }

    /// Whether the clipboard changed since it was last looked at.
    fn notice(&mut self) -> bool {
        let count = self.board.change_count();
        if count == self.count {
            return false;
        }
        self.count = count;
        self.changed_us = now_us();
        self.noticed = Some(Instant::now());
        self.replaced = self.current.take().or(self.replaced.take());
        self.publish();
        true
    }

    /// A change goes to every link.
    fn poll(&mut self) {
        if self.notice() && !self.links.is_empty() {
            let clip = self.current();
            if self.links.iter().all(|l| l.has == Some(clip.id)) {
                return;
            }
            tracing::debug!(bytes = clip.content.len(), types = clip.content.items.len(), "copied on this Mac");
            self.distribute(&clip, None);
        }
    }

    /// What the clipboard has, read if it hasn't been.
    fn current(&mut self) -> Clip {
        if self.current.as_ref().is_none_or(|c| c.content.is_none()) {
            if let Some(wait) = self.noticed.map(|at| self.settle.saturating_sub(at.elapsed())) {
                std::thread::sleep(wait);
            }
            let content = Arc::new(self.board.read());
            match &mut self.current {
                Some(current) => current.content = Some(content),
                None => match self.replaced.take() {
                    Some(before) if before.content.as_deref() == Some(&*content) => self.current = Some(before),
                    _ => {
                        let id = new_id();
                        self.prior = self.last_id;
                        self.last_id = id;
                        self.current = Some(Current { id, copied_us: self.changed_us, too_large: 0, content: Some(content) });
                    }
                },
            }
        }
        let current = self.current.as_ref().expect("read above");
        let content = current.content.clone().expect("read above");
        let clip = Clip { id: current.id, copied_us: current.copied_us, too_large: current.too_large, content };
        self.publish();
        clip
    }

    /// A new link gets this Mac's offer: what it has, if it's anything to share.
    fn link(&mut self, mut link: LinkState) {
        self.links.retain(|l| l.peer != link.peer);
        // A change not noticed yet goes to the other links first.
        self.poll();
        let clip = self.current();
        if !clip.is_empty() && clip.too_large == 0 && clip.content.len() <= link.cap {
            link.has = Some(clip.id);
            link.out.send_replace(Some(Outgoing { clip, prior: self.prior, offer: true }));
        }
        self.links.push(link);
    }

    fn received(&mut self, peer: u64, clip: Clip, prior: u64, offer: bool) {
        let Some(link) = self.links.iter_mut().find(|l| l.peer == peer) else { return };
        link.has = Some(clip.id);
        // A change here not noticed yet is newer than anything the other Mac had: it goes there
        // now, and the copy that came is weighed against it as one made at the same time.
        self.poll();
        let current = self.current();
        if clip.id == current.id {
            return;
        }
        let later = clip.key() > current.key();
        let take = if offer { later && !clip.is_empty() } else { prior == current.id || later };
        if take {
            self.apply(clip, peer);
        } else {
            tracing::debug!(offer, "kept this Mac's clipboard: it has the later copy");
        }
    }

    /// Writes a copy from another Mac here and passes it on to the other links.
    fn apply(&mut self, clip: Clip, from: u64) {
        let started = Instant::now();
        self.count = self.board.write(&clip.content);
        tracing::debug!(
            bytes = clip.content.len(),
            types = clip.content.items.len(),
            too_large = clip.too_large,
            ms = started.elapsed().as_secs_f64() * 1000.0,
            "clipboard from another Mac"
        );
        self.changed_us = clip.copied_us;
        self.prior = self.last_id;
        self.last_id = clip.id;
        self.replaced = None;
        self.current =
            Some(Current { id: clip.id, copied_us: clip.copied_us, too_large: clip.too_large, content: Some(clip.content.clone()) });
        self.publish();
        if clip.too_large > 0
            && let Some(link) = self.links.iter().find(|l| l.peer == from)
        {
            (link.notes)(Note::TooLargeToReceive(clip.too_large));
        }
        self.distribute(&clip, Some(from));
    }

    /// Sends `clip` to every link that doesn't have it, but `except` (where it came from).
    fn distribute(&mut self, clip: &Clip, except: Option<u64>) {
        let prior = self.prior;
        for link in self.links.iter_mut().filter(|l| Some(l.peer) != except && l.has != Some(clip.id)) {
            link.has = Some(clip.id);
            let len = clip.content.len();
            let clip = if len > link.cap {
                tracing::info!(bytes = len, cap = link.cap, "clipboard too large to share");
                (link.notes)(Note::TooLargeToSend(len));
                Clip { too_large: len, content: Arc::default(), ..clip.clone() }
            } else {
                clip.clone()
            };
            link.out.send_replace(Some(Outgoing { clip, prior, offer: false }));
        }
    }

    fn publish(&self) {
        let key = self.current.as_ref().map_or((self.changed_us, 0), |c| (c.copied_us, c.id));
        *self.newest.lock().unwrap() = key;
    }
}

fn now_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

/// A random id for a copy (never 0, which stands for none).
fn new_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    RandomState::new().hash_one((COUNTER.fetch_add(1, Ordering::Relaxed), now_us())).max(1)
}

/// One session's side of the shared clipboard: whether it shares now, and its transfers. Host
/// and viewer sessions alike; dropping it stops sharing.
pub(crate) struct SessionClipboard {
    hub: Arc<Clipboard>,
    /// The session, for the clipboard thread.
    peer: u64,
    conn: Connection,
    /// Largest clipboard this session sends.
    cap: u64,
    /// The other Mac is this one, using this same clipboard: there is nothing to share (and each
    /// change would come back as a new one).
    same_clipboard: bool,
    notes: Notes,
    link: Option<Link>,
    sharing: watch::Sender<bool>,
    /// Transfers sent on this connection, and the newest taken from it.
    sent: Arc<AtomicU64>,
    taken: Arc<AtomicU64>,
    readers: Arc<AtomicUsize>,
}

impl SessionClipboard {
    pub(crate) fn new(
        hub: &Arc<Clipboard>,
        conn: Connection,
        internet: bool,
        same_mac: bool,
        notes: impl Fn(Note) + Send + Sync + 'static,
    ) -> Self {
        Self {
            hub: hub.clone(),
            peer: hub.next_peer.fetch_add(1, Ordering::Relaxed),
            conn,
            cap: if internet { MAX_CLIPBOARD_BYTES_INTERNET } else { MAX_CLIPBOARD_BYTES },
            same_clipboard: same_mac && hub.system,
            notes: Arc::new(notes),
            link: None,
            sharing: watch::channel(false).0,
            sent: Arc::default(),
            taken: Arc::default(),
            readers: Arc::default(),
        }
    }

    /// Shares the clipboard with the other Mac (`on`), or stops.
    pub(crate) fn set(&mut self, on: bool) {
        let on = on && !self.same_clipboard;
        if on == self.link.is_some() {
            return;
        }
        tracing::info!(on, "sharing the clipboard");
        self.link = on.then(|| self.hub.link(self.peer, self.conn.clone(), self.cap, self.sent.clone(), self.notes.clone()));
        self.sharing.send_replace(on);
    }

    /// Looks at the clipboard now.
    pub(crate) fn check(&self) {
        if self.link.is_some() {
            self.hub.check();
        }
    }

    /// A clipboard transfer the other Mac opened, its kind byte read: taken while this session
    /// shares the clipboard.
    pub(crate) fn incoming(&self, recv: RecvStream) {
        self.read(recv, false);
    }

    /// A stream the other Mac opened, which should be a clipboard transfer.
    pub(crate) fn incoming_unread(&self, recv: RecvStream) {
        self.read(recv, true);
    }

    fn read(&self, mut recv: RecvStream, kind_unread: bool) {
        if self.readers.fetch_add(1, Ordering::AcqRel) >= MAX_READERS {
            self.readers.fetch_sub(1, Ordering::AcqRel);
            let _ = recv.stop(0u32.into());
            return;
        }
        let slot = ReaderSlot(self.readers.clone());
        let (hub, peer, sharing, taken) = (self.hub.clone(), self.peer, self.sharing.subscribe(), self.taken.clone());
        tokio::spawn(async move {
            let _slot = slot;
            if kind_unread && stream_kind(&mut recv).await != Some(STREAM_CLIPBOARD) {
                let _ = recv.stop(0u32.into());
                return;
            }
            if let Err(e) = receive(recv, &hub, peer, sharing, &taken).await {
                tracing::debug!("clipboard transfer: {e:#}");
            }
        });
    }
}

struct ReaderSlot(Arc<AtomicUsize>);

impl Drop for ReaderSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The first byte of a unidirectional stream: what it carries (`protocol::STREAM_*`). None if it
/// doesn't say in time.
pub(crate) async fn stream_kind(recv: &mut RecvStream) -> Option<u8> {
    let mut kind = [0u8; 1];
    tokio::time::timeout(HEADER_TIMEOUT, recv.read_exact(&mut kind)).await.ok()?.ok()?;
    Some(kind[0])
}

/// Sends a link's transfers, each on a stream of its own, as the clipboard thread hands them
/// over. A newer one cancels the one still on its way.
async fn send_transfers(conn: Connection, mut rx: watch::Receiver<Option<Outgoing>>, sent: Arc<AtomicU64>) {
    let mut next = rx.borrow_and_update().clone();
    loop {
        let Some(out) = next.take() else {
            if rx.changed().await.is_err() {
                return;
            }
            next = rx.borrow_and_update().clone();
            continue;
        };
        let Ok(mut send) = conn.open_uni().await else { return };
        // Behind the control and input streams (video datagrams go first anyway).
        let _ = send.set_priority(-1);
        let seq = sent.fetch_add(1, Ordering::AcqRel) + 1;
        tokio::select! {
            written = write_transfer(&mut send, seq, &out) => match written {
                Ok(()) => drop(send.finish()),
                Err(e) => tracing::debug!("clipboard transfer: {e:#}"),
            },
            changed = rx.changed() => {
                let _ = send.reset(0u32.into());
                if changed.is_err() {
                    return;
                }
                next = rx.borrow_and_update().clone();
            }
        }
    }
}

async fn write_transfer(send: &mut SendStream, seq: u64, out: &Outgoing) -> Result<()> {
    let items = &out.clip.content.items;
    let header = ClipboardHeader {
        seq,
        id: out.clip.id,
        copied_us: out.clip.copied_us,
        prior: out.prior,
        offer: out.offer,
        too_large: out.clip.too_large,
        items: items.iter().map(|(kind, data)| ClipboardItem { kind: kind.clone(), len: data.len() as u64 }).collect(),
    };
    let mut head = vec![STREAM_CLIPBOARD];
    head.extend(protocol::encode_framed(&header)?);
    send.write_all(&head).await?;
    for (_, data) in items {
        send.write_all(data).await?;
    }
    Ok(())
}

/// Reads one transfer and hands it to the clipboard thread, if this session shares the clipboard.
async fn receive(mut recv: RecvStream, hub: &Clipboard, peer: u64, mut sharing: watch::Receiver<bool>, taken: &AtomicU64) -> Result<()> {
    let header = tokio::time::timeout(HEADER_TIMEOUT, read_header(&mut recv)).await.context("no clipboard header in time")??;
    header.check().map_err(anyhow::Error::msg)?;
    if !matches!(tokio::time::timeout(LINK_GRACE, sharing.wait_for(|on| *on)).await, Ok(Ok(_))) {
        bail!("not sharing the clipboard");
    }
    if header.offer && (header.copied_us, header.id) <= hub.newest() {
        let _ = recv.stop(0u32.into());
        tracing::debug!("this Mac has the later copy: not reading the other's");
        return Ok(());
    }
    let mut items: Vec<(String, Vec<u8>)> = Vec::new();
    for item in &header.items {
        let mut data = vec![0; item.len as usize];
        recv.read_exact(&mut data).await.context("read clipboard data")?;
        if CLIPBOARD_TYPES.contains(&item.kind.as_str()) && !items.iter().any(|(kind, _)| *kind == item.kind) {
            items.push((item.kind.clone(), data));
        }
    }
    if recv.read(&mut [0u8; 1]).await.context("read clipboard transfer")?.is_some() {
        bail!("more clipboard data than the header said");
    }
    // A newer transfer was taken already, or sharing stopped meanwhile.
    if taken.fetch_max(header.seq, Ordering::AcqRel) >= header.seq || !*sharing.borrow() {
        return Ok(());
    }
    let clip = Clip { id: header.id, copied_us: header.copied_us, too_large: header.too_large, content: Arc::new(Content { items }) };
    hub.received(peer, clip, header.prior, header.offer);
    Ok(())
}

async fn read_header(recv: &mut RecvStream) -> Result<ClipboardHeader> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.context("read clipboard header length")?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_CLIPBOARD_HEADER_LEN {
        bail!("clipboard header too long: {len} bytes");
    }
    let mut body = vec![0; len];
    recv.read_exact(&mut body).await.context("read clipboard header")?;
    Ok(protocol::decode(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clipboard in memory that counts its reads.
    #[derive(Clone, Default)]
    struct MemoryBoard(Arc<Mutex<(i64, Content, usize)>>);

    impl MemoryBoard {
        fn copy(&self, text: &str) {
            let mut b = self.0.lock().unwrap();
            b.0 += 1;
            b.1 = text_content(text);
        }

        fn copy_bytes(&self, len: usize) {
            let mut b = self.0.lock().unwrap();
            b.0 += 1;
            b.1 = Content { items: vec![(PNG.to_string(), vec![7; len])] };
        }

        fn content(&self) -> Content {
            self.0.lock().unwrap().1.clone()
        }

        fn reads(&self) -> usize {
            self.0.lock().unwrap().2
        }
    }

    impl Board for MemoryBoard {
        fn change_count(&mut self) -> i64 {
            self.0.lock().unwrap().0
        }

        fn read(&mut self) -> Content {
            let mut b = self.0.lock().unwrap();
            b.2 += 1;
            b.1.clone()
        }

        fn write(&mut self, content: &Content) -> i64 {
            let mut b = self.0.lock().unwrap();
            b.0 += 1;
            b.1 = content.clone();
            b.0
        }
    }

    fn text_content(text: &str) -> Content {
        Content { items: vec![("public.utf8-plain-text".to_string(), text.as_bytes().to_vec())] }
    }

    /// A hub driven by hand (no thread), with its board.
    fn hub() -> (Hub, MemoryBoard) {
        let board = MemoryBoard::default();
        let mut hub = Hub::new(Box::new(board.clone()), Arc::default());
        // Memory boards change in one step.
        hub.settle = Duration::ZERO;
        (hub, board)
    }

    struct TestLink {
        rx: watch::Receiver<Option<Outgoing>>,
        notes: Arc<Mutex<Vec<Note>>>,
    }

    impl TestLink {
        /// What the link was given to send since last asked, if anything.
        fn sent(&mut self) -> Option<Outgoing> {
            if !self.rx.has_changed().unwrap() {
                return None;
            }
            self.rx.borrow_and_update().clone()
        }

        fn notes(&self) -> Vec<Note> {
            std::mem::take(&mut *self.notes.lock().unwrap())
        }
    }

    fn link(hub: &mut Hub, peer: u64, cap: u64) -> TestLink {
        let (out, rx) = watch::channel(None);
        let notes = Arc::new(Mutex::new(Vec::new()));
        let recorded = notes.clone();
        hub.handle(Cmd::Link { peer, cap, out, notes: Arc::new(move |n| recorded.lock().unwrap().push(n)) });
        TestLink { rx, notes }
    }

    fn text(out: &Outgoing) -> String {
        let items = &out.clip.content.items;
        items.iter().find(|(k, _)| k == "public.utf8-plain-text").map(|(_, d)| String::from_utf8(d.clone()).unwrap()).unwrap_or_default()
    }

    /// Hands what `from`'s link to `to` sends over to `to`, as `to`'s link `peer` would.
    fn deliver(out: Outgoing, to: &mut Hub, peer: u64) {
        to.handle(Cmd::Received { peer, clip: out.clip, prior: out.prior, offer: out.offer });
    }

    #[test]
    fn a_copy_goes_to_every_link_once() {
        let (mut hub, board) = hub();
        let (mut a, mut b) = (link(&mut hub, 1, MAX_CLIPBOARD_BYTES), link(&mut hub, 2, MAX_CLIPBOARD_BYTES));
        assert!(a.sent().is_none() && b.sent().is_none(), "an empty clipboard isn't offered");
        board.copy("hello");
        hub.poll();
        let (to_a, to_b) = (a.sent().unwrap(), b.sent().unwrap());
        assert_eq!((text(&to_a), to_a.offer), ("hello".to_string(), false));
        assert_eq!(to_a.clip.id, to_b.clip.id);
        hub.poll();
        assert!(a.sent().is_none(), "nothing changed");
        board.copy("again");
        hub.handle(Cmd::Check);
        assert_eq!(text(&a.sent().unwrap()), "again");
        assert_eq!(text(&b.sent().unwrap()), "again");
    }

    #[test]
    fn the_same_contents_written_again_are_the_same_copy() {
        let (mut hub, board) = hub();
        let mut a = link(&mut hub, 1, MAX_CLIPBOARD_BYTES);
        board.copy("once");
        hub.poll();
        let first = a.sent().unwrap();
        // An app writes it again, or Universal Clipboard brings what this Mac had already.
        board.copy("once");
        hub.poll();
        assert!(a.sent().is_none());
        board.copy("twice");
        hub.poll();
        let second = a.sent().unwrap();
        assert_ne!(second.clip.id, first.clip.id);
        assert_eq!(second.prior, first.clip.id);
    }

    #[test]
    fn contents_are_read_only_while_shared() {
        let (mut hub, board) = hub();
        board.copy("private");
        hub.poll();
        board.copy("still private");
        hub.poll();
        assert_eq!(board.reads(), 0, "only the change count, while nothing shares it");
        let mut a = link(&mut hub, 1, MAX_CLIPBOARD_BYTES);
        assert_eq!(board.reads(), 1);
        assert_eq!(text(&a.sent().unwrap()), "still private");
        hub.handle(Cmd::Unlink(1));
        hub.poll();
        let mut again = link(&mut hub, 1, MAX_CLIPBOARD_BYTES);
        let offer = again.sent().unwrap();
        assert_eq!(text(&offer), "still private");
        assert_eq!(board.reads(), 2, "let go of while nothing shared it, read again");
    }

    #[test]
    fn a_copy_from_another_mac_is_written_and_passed_on_but_never_back() {
        let (mut other, other_board) = hub();
        let mut to_us = link(&mut other, 1, MAX_CLIPBOARD_BYTES);
        let (mut hub, board) = hub();
        let mut from = link(&mut hub, 1, MAX_CLIPBOARD_BYTES);
        let mut next = link(&mut hub, 2, MAX_CLIPBOARD_BYTES);
        other_board.copy("from afar");
        other.poll();
        deliver(to_us.sent().unwrap(), &mut hub, 1);
        assert_eq!(board.content(), text_content("from afar"));
        assert_eq!(text(&next.sent().unwrap()), "from afar", "passed on down the chain");
        assert!(from.sent().is_none(), "not sent back");
        hub.poll();
        assert!(from.sent().is_none() && next.sent().is_none(), "writing it isn't a change of this Mac's");
    }

    #[test]
    fn a_ring_of_macs_stops() {
        let (mut hub, board) = hub();
        let mut out = link(&mut hub, 1, MAX_CLIPBOARD_BYTES);
        let _back = link(&mut hub, 2, MAX_CLIPBOARD_BYTES);
        board.copy("round");
        hub.poll();
        let copy = out.sent().unwrap();
        // It went around the ring and comes back from the other side.
        let count = board.0.lock().unwrap().0;
        deliver(copy, &mut hub, 2);
        assert_eq!(board.0.lock().unwrap().0, count, "not written again");
        assert!(out.sent().is_none(), "and not sent again");
    }

    /// Two Macs start sharing: each offers what it has to the other.
    fn start_sharing(a: &mut Hub, b: &mut Hub) -> (TestLink, TestLink) {
        let mut a_to_b = link(a, 1, MAX_CLIPBOARD_BYTES);
        let mut b_to_a = link(b, 1, MAX_CLIPBOARD_BYTES);
        let (from_a, from_b) = (a_to_b.sent(), b_to_a.sent());
        if let Some(offer) = from_a {
            assert!(offer.offer);
            deliver(offer, b, 1);
        }
        if let Some(offer) = from_b {
            deliver(offer, a, 1);
        }
        (a_to_b, b_to_a)
    }

    #[test]
    fn as_sharing_starts_the_later_copy_wins_on_both() {
        let (mut a, a_board) = hub();
        let (mut b, b_board) = hub();
        a_board.copy("earlier");
        a.poll();
        std::thread::sleep(Duration::from_millis(2));
        b_board.copy("later");
        b.poll();
        let (mut a_to_b, mut b_to_a) = start_sharing(&mut a, &mut b);
        assert_eq!(a_board.content(), text_content("later"));
        assert_eq!(b_board.content(), text_content("later"));
        // Neither sends anything more: each knows what the other has.
        a.poll();
        b.poll();
        assert!(a_to_b.sent().is_none() && b_to_a.sent().is_none());
    }

    #[test]
    fn an_empty_clipboard_never_wins_as_sharing_starts() {
        let (mut a, a_board) = hub();
        let (mut b, b_board) = hub();
        a_board.copy("keep me");
        a.poll();
        std::thread::sleep(Duration::from_millis(2));
        // A file was copied there: nothing the Macs share, but the later copy.
        b_board.0.lock().unwrap().0 += 1;
        b.poll();
        start_sharing(&mut a, &mut b);
        assert_eq!(a_board.content(), text_content("keep me"), "not emptied");
        assert_eq!(b_board.content(), Content::default(), "and the later copy isn't replaced either");
    }

    #[test]
    fn changes_after_that_always_win() {
        let (mut a, a_board) = hub();
        let (mut b, b_board) = hub();
        a_board.copy("first");
        a.poll();
        let (mut a_to_b, mut b_to_a) = start_sharing(&mut a, &mut b);
        assert_eq!(b_board.content(), text_content("first"));
        // B's clock is behind: its copies look older, yet each change is the newest.
        b_board.copy("b's");
        b.poll();
        let mut out = b_to_a.sent().unwrap();
        out.clip.copied_us = 1;
        deliver(out, &mut a, 1);
        assert_eq!(a_board.content(), text_content("b's"));
        a_board.copy("a's");
        a.poll();
        deliver(a_to_b.sent().unwrap(), &mut b, 1);
        assert_eq!(b_board.content(), text_content("a's"));
    }

    #[test]
    fn changes_at_the_same_time_end_the_same_on_both() {
        for b_later in [false, true] {
            let (mut a, a_board) = hub();
            let (mut b, b_board) = hub();
            a_board.copy("shared");
            a.poll();
            let (mut a_to_b, mut b_to_a) = start_sharing(&mut a, &mut b);
            // Both copy before either hears of the other's, one a moment after the other.
            let mut copy = |b_now: bool| {
                if b_now {
                    b_board.copy("b's");
                    b.poll();
                } else {
                    a_board.copy("a's");
                    a.poll();
                }
            };
            copy(!b_later);
            std::thread::sleep(Duration::from_millis(2));
            copy(b_later);
            let (from_a, from_b) = (a_to_b.sent().unwrap(), b_to_a.sent().unwrap());
            deliver(from_b, &mut a, 1);
            deliver(from_a, &mut b, 1);
            let winner = if b_later { "b's" } else { "a's" };
            assert_eq!(a_board.content(), text_content(winner), "b later: {b_later}");
            assert_eq!(b_board.content(), text_content(winner), "b later: {b_later}");
        }
    }

    #[test]
    fn a_copy_that_arrives_as_this_mac_changes_is_weighed_against_the_change() {
        let (mut a, a_board) = hub();
        let (mut b, b_board) = hub();
        a_board.copy("shared");
        a.poll();
        let (mut a_to_b, mut b_to_a) = start_sharing(&mut a, &mut b);
        // B copied, A copied too but hasn't looked yet when B's arrives.
        b_board.copy("b's");
        b.poll();
        let from_b = b_to_a.sent().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        a_board.copy("a's");
        deliver(from_b, &mut a, 1);
        assert_eq!(a_board.content(), text_content("a's"), "A's own change is the later one");
        // A sent its change on the spot, and B takes it.
        deliver(a_to_b.sent().unwrap(), &mut b, 1);
        assert_eq!(b_board.content(), text_content("a's"));
    }

    #[test]
    fn a_clipboard_too_large_empties_the_other_one() {
        let (mut a, a_board) = hub();
        let (mut b, b_board) = hub();
        a_board.copy("stale");
        a.poll();
        let mut small = link(&mut a, 1, 10);
        let receiving = link(&mut b, 1, 10);
        deliver(small.sent().unwrap(), &mut b, 1);
        assert_eq!(b_board.content(), text_content("stale"));
        let mut roomy = link(&mut a, 2, 100);
        roomy.sent();

        a_board.copy_bytes(20);
        a.poll();
        let skipped = small.sent().unwrap();
        assert_eq!((skipped.clip.too_large, skipped.clip.is_empty()), (20, true));
        assert_eq!(small.notes(), [Note::TooLargeToSend(20)]);
        assert_eq!(roomy.sent().unwrap().clip.content.len(), 20, "fits the other link");
        assert!(roomy.notes().is_empty());
        deliver(skipped, &mut b, 1);
        assert_eq!(b_board.content(), Content::default(), "emptied, rather than pasting something older");
        assert_eq!(receiving.notes(), [Note::TooLargeToReceive(20)]);
    }

    #[test]
    fn a_clipboard_too_large_is_not_offered() {
        let (mut a, a_board) = hub();
        a_board.copy_bytes(20);
        a.poll();
        let mut small = link(&mut a, 1, 10);
        assert!(small.sent().is_none());
        assert!(small.notes().is_empty(), "nothing to say about a clipboard the user may have forgotten");
    }

    #[test]
    fn transfers_from_an_unknown_link_are_ignored() {
        let (mut hub, board) = hub();
        board.copy("mine");
        hub.poll();
        let clip = Clip { id: 5, copied_us: u64::MAX, too_large: 0, content: Arc::new(text_content("theirs")) };
        hub.handle(Cmd::Received { peer: 9, clip, prior: 0, offer: false });
        assert_eq!(board.content(), text_content("mine"));
    }

    #[test]
    fn backend_from_the_environment() {
        assert_eq!(ClipboardBackend::parse(None), ClipboardBackend::System);
        assert_eq!(ClipboardBackend::parse(Some(" ")), ClipboardBackend::System);
        assert_eq!(ClipboardBackend::parse(Some("off")), ClipboardBackend::Off);
        assert_eq!(ClipboardBackend::parse(Some("lankvm-2")), ClipboardBackend::Named("lankvm-2".into()));
    }

    #[test]
    fn copies_get_ids_of_their_own() {
        let ids: std::collections::HashSet<u64> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 1000);
        assert!(!ids.contains(&0), "0 stands for none");
    }
}
