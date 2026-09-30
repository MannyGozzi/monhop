//! Clipboard hub: owns the native, IO and codec threads and fans local changes out to peers.
//!
//! The clipboard is watched only while the switch is on and at least one Share connection is
//! attached. Only changes made on this computer go out, published once into the one outbound slot
//! every link shares, so a mesh of computers never loops. Received images are decoded off the IO
//! runtime one at a time, the newest item winning, and every write goes through the adapter's
//! marker-checked contract, so a local copy made after a transfer started always wins.

use std::{
    collections::BTreeMap,
    io,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak, mpsc},
    task::Waker,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use monhop_protocol::{
    SessionEpoch,
    clipboard::{ClipboardKind, ClipboardTextError},
};
use monhop_transport::{
    crypto::CertificateFingerprint,
    session_clipboard::{
        self, ClipboardLink, ClipboardLinkConfig, ClipboardNote, ClipboardSink, InboundClipboard,
        OutboundClipboard, OutboundClipboardError,
    },
};
use tokio::sync::{oneshot, watch};

use super::content::{EchoGuard, Observation, Outgoing, Verdict, accept_incoming};
use super::image::{self, ImageError, Rgba};
use super::native::{Access, NativeClipboard, NativeError, ReadLimits};
use super::settings::{
    ClipboardSetting, ClipboardView, Direction, LastTransfer, Notice, PeerView, TransferKind,
};

/// Tauri event carrying a `ClipboardView` whenever it changes.
pub const EVENT: &str = "clipboard";
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long stopping waits for a read in progress. A read held past it (a macOS paste-access
/// prompt left open, a program slow to render what it copied) finishes on its own and is dropped.
const READ_WAIT: Duration = Duration::from_secs(1);

/// Receives every changed view; the app forwards it to the window as the `EVENT` event.
pub trait ViewEmitter: Send + Sync + 'static {
    fn view_changed(&self, view: &ClipboardView);
}

impl ViewEmitter for tauri::AppHandle {
    fn view_changed(&self, view: &ClipboardView) {
        let _ = tauri::Emitter::emit(self, EVENT, view.clone());
    }
}

type NativeFactory = Box<dyn Fn() -> Result<Box<dyn NativeClipboard>, NativeError> + Send + Sync>;
type Reencode = Box<dyn Fn(&[u8]) -> Result<(Rgba, Vec<u8>), ImageError> + Send + Sync>;

struct Backend {
    /// Runs on the clipboard thread, which then owns the adapter until it stops.
    native: NativeFactory,
    reencode: Reencode,
    poll: Duration,
    /// macOS asks for paste access on the first read, so that read happens as the switch turns on.
    probe_on_switch: bool,
    read_wait: Duration,
}

impl Backend {
    fn platform() -> Self {
        Self {
            native: Box::new(platform_clipboard),
            reencode: Box::new(image::reencode_png),
            poll: POLL_INTERVAL,
            probe_on_switch: cfg!(target_os = "macos"),
            read_wait: READ_WAIT,
        }
    }
}

#[cfg(windows)]
fn platform_clipboard() -> Result<Box<dyn NativeClipboard>, NativeError> {
    Ok(Box::new(super::windows::WindowsClipboard::new()?))
}

#[cfg(target_os = "macos")]
fn platform_clipboard() -> Result<Box<dyn NativeClipboard>, NativeError> {
    Ok(Box::new(super::macos::MacClipboard::new()))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_clipboard() -> Result<Box<dyn NativeClipboard>, NativeError> {
    Err(NativeError::Failed)
}

/// Clipboard sharing for every attached Share connection. Dropping it stops its threads.
pub struct ClipboardHub {
    shared: Arc<Shared>,
}

/// Held for as long as one Share connection is live; dropping it detaches that computer.
#[must_use = "dropping the attachment detaches the computer from clipboard sharing"]
pub struct ClipboardAttachment {
    shared: Weak<Shared>,
    id: Option<u64>,
    link: Option<ClipboardLink>,
}

impl Drop for ClipboardAttachment {
    fn drop(&mut self) {
        drop(self.link.take());
        if let (Some(shared), Some(id)) = (self.shared.upgrade(), self.id) {
            shared.detach(id);
        }
    }
}

struct Shared {
    state: Mutex<State>,
    /// Signals the codec thread that a job or shutdown is waiting.
    codec_work: Condvar,
    local_enabled: watch::Sender<bool>,
    /// The newest local item. Every link reads this one slot, so each change is prepared once.
    outbound: watch::Sender<Option<Arc<OutboundClipboard>>>,
    io: Option<IoRuntime>,
    emitter: Box<dyn ViewEmitter>,
    /// Held while a view is built and emitted, so the last one emitted is always the newest.
    announced: Mutex<Option<ClipboardView>>,
    backend: Backend,
}

struct State {
    setting: ClipboardSetting,
    /// By attachment: a reconnect overlapping its old connection is two entries until one leaves.
    peers: BTreeMap<u64, Peer>,
    next_attachment: u64,
    access: Access,
    notice: Option<Notice>,
    last: Option<Last>,
    watch: Option<Watch>,
    /// The last clipboard thread told to stop; the next one joins it before touching the clipboard.
    stopping: Option<JoinHandle<()>>,
    /// Moves with every clipboard thread start; work queued under an older one is dropped.
    generation: u64,
    /// The newest local item; an image converted for an older one is never published.
    local_ticket: u64,
    /// The newest received item; an older one still decoding is never written.
    inbound_ticket: u64,
    encode: Option<EncodeJob>,
    decode: Option<DecodeJob>,
    apply: Option<ApplyJob>,
    codec: Option<JoinHandle<()>>,
    shutting_down: bool,
}

impl State {
    fn watching(&self, generation: u64) -> bool {
        self.current_generation() == Some(generation)
    }

    fn current_generation(&self) -> Option<u64> {
        if self.shutting_down {
            return None;
        }
        self.watch.as_ref().map(|watch| watch.generation)
    }

    fn wake_watch(&self) {
        if let Some(waker) = self.watch.as_ref().and_then(|watch| watch.waker.as_ref()) {
            waker.wake_by_ref();
        }
    }

    fn clear_transfer_notice(&mut self) {
        if matches!(self.notice, Some(Notice::TooLarge | Notice::Unsupported)) {
            self.notice = None;
        }
    }
}

struct Peer {
    fingerprint: CertificateFingerprint,
    /// The peer's last reported switch; `None` until it reports.
    enabled: Option<bool>,
}

struct Last {
    direction: Direction,
    kind: TransferKind,
    bytes: u64,
    peer: CertificateFingerprint,
    at: Instant,
}

impl Last {
    fn view(&self) -> LastTransfer {
        LastTransfer {
            direction: self.direction,
            kind: self.kind,
            bytes: self.bytes,
            peer: self.peer.full_hex(),
            age_seconds: self.at.elapsed().as_secs(),
        }
    }
}

struct Watch {
    generation: u64,
    /// Set once the thread opened the clipboard.
    waker: Option<Waker>,
    thread: JoinHandle<()>,
    gate: Arc<ReadGate>,
}

/// Every clipboard read of one watch passes through its gate, so stopping the watch can close it
/// and wait until nothing is reading. Lock order: a gate before `Shared::state`, never the
/// reverse, so a gate is closed only with `state` unlocked.
struct ReadGate {
    reads: Mutex<Reads>,
    finished: Condvar,
}

struct Reads {
    may_read: bool,
    reading: bool,
}

impl ReadGate {
    fn open() -> Arc<Self> {
        Arc::new(Self {
            reads: Mutex::new(Reads {
                may_read: true,
                reading: false,
            }),
            finished: Condvar::new(),
        })
    }

    /// Runs `read` only while the gate is open and `allowed` holds. Both are checked under the
    /// gate, and the read counts as in progress from then on, so `close` never misses one.
    fn read<T>(&self, allowed: impl FnOnce() -> bool, read: impl FnOnce() -> T) -> Option<T> {
        {
            let mut reads = lock(&self.reads);
            if !reads.may_read || !allowed() {
                return None;
            }
            reads.reading = true;
        }
        let _reading = Reading(self);
        Some(read())
    }

    /// No read starts after this returns, and none is in progress unless it outlasted `wait`.
    fn close(&self, wait: Duration) {
        let mut reads = lock(&self.reads);
        reads.may_read = false;
        let (reads, _) = self
            .finished
            .wait_timeout_while(reads, wait, |reads| reads.reading)
            .unwrap_or_else(PoisonError::into_inner);
        if reads.reading {
            log::warn!(
                "clipboard: a clipboard read is still in progress after sharing stopped; what it reads is dropped"
            );
        }
    }
}

/// Ends a read in progress, also when the read unwinds.
struct Reading<'a>(&'a ReadGate);

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        lock(&self.0.reads).reading = false;
        self.0.finished.notify_all();
    }
}

/// A received item on its way to the clipboard.
struct Inbound {
    generation: u64,
    ticket: u64,
    peer: CertificateFingerprint,
    started_at: Instant,
    wire_bytes: usize,
}

struct DecodeJob {
    inbound: Inbound,
    png: Vec<u8>,
}

struct ApplyJob {
    inbound: Inbound,
    content: Incoming,
}

enum Incoming {
    Text(String),
    Png { png: Vec<u8>, rgba: Option<Rgba> },
}

impl Incoming {
    fn kind(&self) -> TransferKind {
        match self {
            Self::Text(_) => TransferKind::Text,
            Self::Png { .. } => TransferKind::Image,
        }
    }
}

struct EncodeJob {
    generation: u64,
    ticket: u64,
    dib: Vec<u8>,
}

enum CodecJob {
    Decode(DecodeJob),
    Encode(EncodeJob),
}

enum Step {
    Stop,
    Idle,
    Apply(ApplyJob),
}

impl ClipboardHub {
    /// Loads the switch from `setting_path` (missing or unreadable means off) and starts the link
    /// and codec threads, which stay idle until a Share connection attaches.
    pub fn start(emitter: impl ViewEmitter, setting_path: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self::launch(
            Box::new(emitter),
            setting_path,
            Backend::platform(),
        ))
    }

    fn launch(
        emitter: Box<dyn ViewEmitter>,
        setting_path: Option<PathBuf>,
        backend: Backend,
    ) -> Self {
        let setting = ClipboardSetting::load(setting_path);
        let enabled = setting.enabled();
        let io = match IoRuntime::start() {
            Ok(io) => Some(io),
            Err(error) => {
                log::warn!(
                    "clipboard: the link thread could not start, sharing is unavailable: {error}"
                );
                None
            }
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                setting,
                peers: BTreeMap::new(),
                next_attachment: 0,
                access: Access::Unknown,
                notice: None,
                last: None,
                watch: None,
                stopping: None,
                generation: 0,
                local_ticket: 0,
                inbound_ticket: 0,
                encode: None,
                decode: None,
                apply: None,
                codec: None,
                shutting_down: false,
            }),
            codec_work: Condvar::new(),
            local_enabled: watch::channel(enabled).0,
            outbound: watch::channel(None).0,
            io,
            emitter,
            announced: Mutex::new(None),
            backend,
        });
        let codec = Arc::clone(&shared);
        match thread::Builder::new()
            .name("monhop-clipboard-codec".into())
            .spawn(move || codec.run_codec())
        {
            Ok(thread) => shared.lock().codec = Some(thread),
            Err(error) => log::warn!("clipboard: the image thread could not start: {error}"),
        }
        log::info!("clipboard: sharing is {}", on_off(enabled));
        Self { shared }
    }

    pub fn view(&self) -> ClipboardView {
        self.shared.view()
    }

    /// Saves the switch (it takes effect for this run even if saving fails). Turning it off stops
    /// the clipboard thread, withdraws the item on offer and refuses transfers in progress; once
    /// it returns no clipboard read starts, and none is in progress unless it outlasted
    /// `READ_WAIT`, the longest this waits.
    pub fn set_enabled(&self, enabled: bool) -> ClipboardView {
        let shared = &self.shared;
        let stopped = {
            let mut state = shared.lock();
            if let Err(error) = state.setting.set_enabled(enabled) {
                log::warn!("clipboard: the preference could not be saved: {error}");
            }
            log::info!("clipboard: sharing turned {}", on_off(enabled));
            state.notice = None;
            shared.local_enabled.send_replace(enabled);
            if enabled {
                shared.watch_if_needed(&mut state, shared.backend.probe_on_switch);
                None
            } else {
                shared.stop_watching(&mut state)
            }
        };
        shared.close(stopped, shared.backend.read_wait);
        shared.announce()
    }

    /// Starts the clipboard link on a negotiated Share connection. Call it only for Share
    /// connections: attaching grants the peer stream credit.
    pub fn attach(
        &self,
        peer: CertificateFingerprint,
        connection: quinn::Connection,
        epoch: SessionEpoch,
    ) -> ClipboardAttachment {
        let shared = &self.shared;
        let (id, link) = {
            let mut state = shared.lock();
            let io = shared.io.as_ref().filter(|_| !state.shutting_down);
            let Some(io) = io else {
                log::warn!(
                    "clipboard: {} not attached, the link thread is not running",
                    short(peer)
                );
                return ClipboardAttachment {
                    shared: Arc::downgrade(shared),
                    id: None,
                    link: None,
                };
            };
            state.next_attachment += 1;
            let id = state.next_attachment;
            state.peers.insert(
                id,
                Peer {
                    fingerprint: peer,
                    enabled: None,
                },
            );
            let link = session_clipboard::attach(
                connection,
                ClipboardLinkConfig {
                    peer,
                    epoch,
                    local_enabled: shared.local_enabled.subscribe(),
                    outbound: shared.outbound.subscribe(),
                    sink: Arc::new(LinkSink {
                        shared: Arc::downgrade(shared),
                        id,
                    }),
                },
                &io.handle,
            );
            log::info!(
                "clipboard: {} attached, {} connected",
                short(peer),
                state.peers.len()
            );
            shared.watch_if_needed(&mut state, false);
            (id, link)
        };
        shared.announce();
        ClipboardAttachment {
            shared: Arc::downgrade(shared),
            id: Some(id),
            link: Some(link),
        }
    }

    /// Asks every thread to stop without waiting; `is_stopped` reports when they have.
    pub fn request_shutdown(&self) {
        self.shared.request_shutdown();
    }

    pub fn is_stopped(&self) -> bool {
        let state = self.shared.lock();
        state.watch.is_none()
            && state.stopping.as_ref().is_none_or(JoinHandle::is_finished)
            && state.codec.as_ref().is_none_or(JoinHandle::is_finished)
            && self.shared.io.as_ref().is_none_or(IoRuntime::is_finished)
    }
}

impl Drop for ClipboardHub {
    fn drop(&mut self) {
        self.shared.request_shutdown();
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn view(&self) -> ClipboardView {
        let state = self.lock();
        let mut peers: Vec<PeerView> = Vec::new();
        for peer in state.peers.values() {
            let fingerprint = peer.fingerprint.full_hex();
            match peers
                .iter_mut()
                .find(|view| view.fingerprint == fingerprint)
            {
                Some(view) => view.peer_enabled = peer.enabled,
                None => peers.push(PeerView {
                    fingerprint,
                    peer_enabled: peer.enabled,
                }),
            }
        }
        ClipboardView {
            enabled: state.setting.enabled(),
            access: state.access,
            notice: state.notice,
            peers,
            last: state.last.as_ref().map(Last::view),
        }
    }

    /// Emits the current view if it differs from the last one emitted. Never call it with the
    /// state locked.
    fn announce(&self) -> ClipboardView {
        let mut announced = lock(&self.announced);
        let view = self.view();
        if announced.as_ref() != Some(&view) {
            self.emitter.view_changed(&view);
            *announced = Some(view.clone());
        }
        view
    }

    fn watch_if_needed(self: &Arc<Self>, state: &mut State, probe: bool) {
        if state.watch.is_some()
            || state.shutting_down
            || !state.setting.enabled()
            || state.peers.is_empty()
        {
            return;
        }
        state.generation += 1;
        let generation = state.generation;
        let previous = state.stopping.take();
        let shared = Arc::clone(self);
        let gate = ReadGate::open();
        let reads = Arc::clone(&gate);
        match thread::Builder::new()
            .name("monhop-clipboard".into())
            .spawn(move || shared.run_watch(generation, previous, probe, reads))
        {
            Ok(thread) => {
                state.watch = Some(Watch {
                    generation,
                    waker: None,
                    thread,
                    gate,
                });
            }
            Err(error) => log::warn!("clipboard: the clipboard thread could not start: {error}"),
        }
    }

    /// Drops all queued work and the item on offer, and tells the clipboard thread to stop.
    /// Returns that thread's read gate, for `close` once `state` is unlocked.
    #[must_use]
    fn stop_watching(&self, state: &mut State) -> Option<Arc<ReadGate>> {
        state.encode = None;
        state.decode = None;
        state.apply = None;
        self.outbound.send_replace(None);
        let watch = state.watch.take()?;
        if let Some(waker) = &watch.waker {
            waker.wake_by_ref();
        }
        state.stopping = Some(watch.thread);
        Some(watch.gate)
    }

    /// Returns once no read of the stopped watch is in progress or can start, waiting at most
    /// `wait`. Never call it with the state locked.
    fn close(&self, stopped: Option<Arc<ReadGate>>, wait: Duration) {
        if let Some(gate) = stopped {
            gate.close(wait);
        }
    }

    fn detach(&self, id: u64) {
        self.detach_within(id, self.backend.read_wait);
    }

    /// Detaches `id`, waiting at most `wait` for a read of a watch this stops.
    fn detach_within(&self, id: u64, wait: Duration) {
        let stopped = {
            let mut state = self.lock();
            let Some(peer) = state.peers.remove(&id) else {
                return;
            };
            log::info!(
                "clipboard: {} detached, {} connected",
                short(peer.fingerprint),
                state.peers.len()
            );
            if state.peers.is_empty() {
                self.stop_watching(&mut state)
            } else {
                None
            }
        };
        self.close(stopped, wait);
        self.announce();
    }

    fn request_shutdown(&self) {
        {
            let mut state = self.lock();
            if state.shutting_down {
                return;
            }
            state.shutting_down = true;
            // Shutdown never waits; a read not yet begun sees `shutting_down` under its gate.
            let _ = self.stop_watching(&mut state);
        }
        self.codec_work.notify_all();
        if let Some(io) = &self.io {
            io.stop();
        }
        log::info!("clipboard: stopping");
    }

    fn run_watch(
        self: Arc<Self>,
        generation: u64,
        previous: Option<JoinHandle<()>>,
        probe: bool,
        gate: Arc<ReadGate>,
    ) {
        if let Some(previous) = previous {
            let _ = previous.join();
        }
        if !self.lock().watching(generation) {
            return;
        }
        let mut native = match (self.backend.native)() {
            Ok(native) => native,
            Err(error) => {
                log::warn!("clipboard: the clipboard cannot be watched: {error}");
                self.watch_failed(generation);
                return;
            }
        };
        let guard = EchoGuard::new(native.change_marker());
        if probe {
            // Raises the paste-access prompt while the user is at the switch; the item it returns
            // is discarded, never shared.
            let _ = gate.read(
                || self.lock().watching(generation),
                || native.read(&ReadLimits::STANDARD),
            );
        }
        if !self.watch_started(generation, native.waker(), native.access()) {
            return;
        }
        log::info!("clipboard: watching local copies");
        Watcher {
            shared: &self,
            gate: &gate,
            generation,
            native,
            guard,
            unread: false,
        }
        .run();
        log::info!("clipboard: stopped watching local copies");
    }

    fn watch_started(&self, generation: u64, waker: Waker, access: Access) -> bool {
        {
            let mut state = self.lock();
            let state = &mut *state;
            if state.shutting_down {
                return false;
            }
            let Some(watch) = state
                .watch
                .as_mut()
                .filter(|watch| watch.generation == generation)
            else {
                return false;
            };
            watch.waker = Some(waker);
            if state.access == access {
                return true;
            }
            state.access = access;
        }
        self.announce();
        true
    }

    fn watch_failed(&self, generation: u64) {
        let mut state = self.lock();
        if state.watching(generation) {
            // Only this thread reads through the gate, and it never opened the clipboard.
            let _ = self.stop_watching(&mut state);
        }
    }

    fn step(&self, generation: u64) -> Step {
        let mut state = self.lock();
        if !state.watching(generation) {
            return Step::Stop;
        }
        state.apply.take().map_or(Step::Idle, Step::Apply)
    }

    /// Content is read only while some attached peer has its switch on.
    fn wants_content(&self, generation: u64) -> bool {
        let state = self.lock();
        state.watching(generation) && state.peers.values().any(|peer| peer.enabled == Some(true))
    }

    fn local_verdict(&self, generation: u64, verdict: Verdict) {
        match verdict {
            Verdict::Share(Outgoing::Text(text)) => {
                self.offer(generation, None, ClipboardKind::Text, text.into_bytes());
            }
            Verdict::Share(Outgoing::Png(png)) => {
                self.offer(generation, None, ClipboardKind::Png, png);
            }
            Verdict::Share(Outgoing::Dib(dib)) => self.queue_encode(generation, dib),
            Verdict::Skipped(skip) => {
                log::debug!("clipboard: local item skipped ({skip:?})");
                self.set_notice(generation, Notice::for_skip(skip));
            }
            Verdict::Received | Verdict::Repeat => {
                log::debug!("clipboard: local change skipped, it already crossed a link");
            }
            Verdict::Retry | Verdict::Empty => {}
        }
    }

    /// Publishes a local item to every link. An encoded image carries its `ticket` and goes out
    /// only if nothing was copied since.
    fn offer(&self, generation: u64, ticket: Option<u64>, kind: ClipboardKind, bytes: Vec<u8>) {
        let len = bytes.len();
        let item = match OutboundClipboard::new(kind, bytes) {
            Ok(item) => Arc::new(item),
            Err(error) => {
                log::debug!(
                    "clipboard: local {} of {len} bytes not shareable: {error}",
                    kind_name(kind)
                );
                self.set_notice(generation, Some(outbound_notice(error)));
                return;
            }
        };
        let mut state = self.lock();
        if !state.watching(generation) {
            return;
        }
        match ticket {
            Some(ticket) if ticket != state.local_ticket => {
                log::debug!("clipboard: local image superseded before it was shared");
                return;
            }
            Some(_) => {}
            None => {
                state.local_ticket += 1;
                state.encode = None;
            }
        }
        let listening = state
            .peers
            .values()
            .filter(|peer| peer.enabled == Some(true))
            .count();
        self.outbound.send_replace(Some(item));
        log::debug!(
            "clipboard: local {} of {len} bytes offered to {listening} computers",
            kind_name(kind)
        );
    }

    fn queue_encode(&self, generation: u64, dib: Vec<u8>) {
        let mut state = self.lock();
        if !state.watching(generation) {
            return;
        }
        state.local_ticket += 1;
        let job = EncodeJob {
            generation,
            ticket: state.local_ticket,
            dib,
        };
        if state.encode.replace(job).is_some() {
            log::debug!("clipboard: local image superseded before it was converted");
        }
        self.codec_work.notify_one();
    }

    fn set_notice(&self, generation: u64, notice: Option<Notice>) {
        let Some(notice) = notice else {
            return;
        };
        {
            let mut state = self.lock();
            if !state.watching(generation) || state.notice == Some(notice) {
                return;
            }
            state.notice = Some(notice);
        }
        self.announce();
    }

    fn note_access(&self, generation: u64, access: Access, denied: bool) {
        {
            let mut state = self.lock();
            if !state.watching(generation) {
                return;
            }
            let notice = match (denied, state.notice) {
                (true, _) => Some(Notice::AccessDenied),
                (false, Some(Notice::AccessDenied)) => None,
                (false, notice) => notice,
            };
            if state.access == access && state.notice == notice {
                return;
            }
            state.access = access;
            state.notice = notice;
        }
        self.announce();
    }

    fn run_codec(self: Arc<Self>) {
        loop {
            let job = {
                let mut state = self.lock();
                loop {
                    if state.shutting_down {
                        return;
                    }
                    if let Some(job) = state.decode.take() {
                        break CodecJob::Decode(job);
                    }
                    if let Some(job) = state.encode.take() {
                        break CodecJob::Encode(job);
                    }
                    state = self
                        .codec_work
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            match job {
                CodecJob::Decode(job) => self.decode(job),
                CodecJob::Encode(job) => self.encode(job),
            }
        }
    }

    /// Validates a peer image by decoding it under the caps and encoding it afresh, so the
    /// platform only ever parses MonHop's own PNG.
    fn decode(&self, job: DecodeJob) {
        let DecodeJob { inbound, png } = job;
        let decoded = (self.backend.reencode)(&png);
        drop(png);
        let (bytes, peer) = (inbound.wire_bytes, short(inbound.peer));
        {
            let mut state = self.lock();
            if !state.watching(inbound.generation) || state.inbound_ticket != inbound.ticket {
                log::debug!(
                    "clipboard: image of {bytes} bytes from {peer} superseded while decoding"
                );
                return;
            }
            match decoded {
                Ok((rgba, png)) => {
                    // Only the Windows adapter builds a second format from the pixels.
                    let rgba = cfg!(windows).then_some(rgba);
                    state.apply = Some(ApplyJob {
                        inbound,
                        content: Incoming::Png { png, rgba },
                    });
                    state.wake_watch();
                    return;
                }
                Err(error) => {
                    log::info!(
                        "clipboard: image of {bytes} bytes from {peer} not applied ({error:?})"
                    );
                    state.notice = Notice::for_skip(error.skip());
                }
            }
        }
        self.announce();
    }

    fn encode(&self, job: EncodeJob) {
        let EncodeJob {
            generation,
            ticket,
            dib,
        } = job;
        let png = image::dib_to_png(&dib);
        drop(dib);
        match png {
            Ok(png) => self.offer(generation, Some(ticket), ClipboardKind::Png, png),
            Err(error) => {
                log::debug!("clipboard: local image not shareable ({error:?})");
                self.set_notice(generation, Notice::for_skip(error.skip()));
            }
        }
    }

    /// Queues a received item, replacing any older one not yet written.
    fn received(&self, id: u64, item: InboundClipboard) {
        let InboundClipboard {
            peer,
            kind,
            bytes,
            started_at,
            ..
        } = item;
        let wire_bytes = bytes.len();
        let text = match kind {
            ClipboardKind::Text => match accept_incoming(bytes) {
                Ok(text) => Ok(text),
                Err(error) => {
                    log::warn!(
                        "clipboard: text of {wire_bytes} bytes from {} refused ({error:?})",
                        short(peer)
                    );
                    return;
                }
            },
            ClipboardKind::Png => Err(bytes),
            ClipboardKind::State => return,
        };
        let mut state = self.lock();
        let Some(generation) = state
            .current_generation()
            .filter(|_| state.peers.contains_key(&id))
        else {
            log::debug!(
                "clipboard: {} of {wire_bytes} bytes from {} dropped, sharing stopped",
                kind_name(kind),
                short(peer)
            );
            return;
        };
        state.inbound_ticket += 1;
        let inbound = Inbound {
            generation,
            ticket: state.inbound_ticket,
            peer,
            started_at,
            wire_bytes,
        };
        let replaced_apply;
        let replaced_decode;
        match text {
            Ok(text) => {
                replaced_decode = state.decode.take().is_some();
                replaced_apply = state
                    .apply
                    .replace(ApplyJob {
                        inbound,
                        content: Incoming::Text(text),
                    })
                    .is_some();
                state.wake_watch();
            }
            Err(png) => {
                replaced_apply = state.apply.take().is_some();
                replaced_decode = state.decode.replace(DecodeJob { inbound, png }).is_some();
                self.codec_work.notify_one();
            }
        }
        if replaced_apply || replaced_decode {
            log::debug!("clipboard: an older received item was superseded before it was applied");
        }
    }

    fn applied(&self, generation: u64, inbound: &Inbound, kind: TransferKind) {
        {
            let mut state = self.lock();
            if !state.watching(generation) {
                return;
            }
            state.last = Some(Last {
                direction: Direction::Received,
                kind,
                bytes: inbound.wire_bytes as u64,
                peer: inbound.peer,
                at: Instant::now(),
            });
            state.clear_transfer_notice();
        }
        self.announce();
    }

    fn peer_state(&self, id: u64, enabled: bool) {
        {
            let mut state = self.lock();
            let Some(peer) = state.peers.get_mut(&id) else {
                return;
            };
            peer.enabled = Some(enabled);
        }
        self.announce();
    }

    fn note(&self, id: u64, peer: CertificateFingerprint, note: ClipboardNote) {
        let ClipboardNote::Sent { kind, bytes } = note else {
            return;
        };
        let Some(kind) = transfer_kind(kind) else {
            return;
        };
        {
            let mut state = self.lock();
            if !state.peers.contains_key(&id) {
                return;
            }
            state.last = Some(Last {
                direction: Direction::Sent,
                kind,
                bytes: u64::from(bytes),
                peer,
                at: Instant::now(),
            });
            state.clear_transfer_notice();
        }
        self.announce();
    }
}

/// The clipboard thread's loop. It alone touches the adapter.
struct Watcher<'a> {
    shared: &'a Shared,
    gate: &'a ReadGate,
    generation: u64,
    native: Box<dyn NativeClipboard>,
    guard: EchoGuard,
    /// A change another program's lock kept this thread from reading; read on the next tick.
    unread: bool,
}

impl Watcher<'_> {
    fn run(&mut self) {
        loop {
            let job = match self.shared.step(self.generation) {
                Step::Stop => return,
                Step::Idle => None,
                Step::Apply(job) => Some(job),
            };
            // A local copy is observed before any write, so it wins over the incoming item.
            self.poll();
            match job {
                Some(job) => self.apply(job),
                None => self.native.wait(self.shared.backend.poll),
            }
        }
    }

    fn poll(&mut self) {
        match self
            .guard
            .observe(self.native.change_marker(), Instant::now())
        {
            Observation::OwnWrite => {
                self.unread = false;
                return;
            }
            Observation::Unchanged if !self.unread => return,
            Observation::Unchanged | Observation::Changed => {}
        }
        let read = self.gate.read(
            || self.shared.wants_content(self.generation),
            || self.native.read(&ReadLimits::STANDARD),
        );
        let Some(read) = read else {
            self.unread = false;
            return;
        };
        let access = self.native.access();
        match read {
            Ok(snapshot) => {
                self.unread = false;
                self.shared.note_access(self.generation, access, false);
                let verdict = self.guard.judge(snapshot);
                self.shared.local_verdict(self.generation, verdict);
            }
            Err(NativeError::Busy) => {
                if !self.unread {
                    log::debug!("clipboard: the clipboard is busy, reading again next tick");
                }
                self.unread = true;
            }
            Err(error) => {
                self.unread = false;
                log::debug!("clipboard: reading the clipboard failed: {error}");
                self.shared
                    .note_access(self.generation, access, access != Access::Allowed);
            }
        }
    }

    fn apply(&mut self, job: ApplyJob) {
        let ApplyJob { inbound, content } = job;
        let kind = content.kind();
        let (name, bytes, peer) = (transfer_name(kind), inbound.wire_bytes, short(inbound.peer));
        let approved = self.native.change_marker();
        if !self.guard.may_apply(inbound.started_at, approved) {
            log::debug!(
                "clipboard: {name} of {bytes} bytes from {peer} not applied, a newer local copy wins"
            );
            return;
        }
        // The read just before can take a while; the switch may have gone off meanwhile.
        if !self.shared.lock().watching(self.generation) {
            return;
        }
        let written = match &content {
            Incoming::Text(text) => {
                let marker = self.guard.begin_apply(self.guard.text_key(text));
                self.native.write_text(text, marker, approved)
            }
            Incoming::Png { png, rgba } => {
                let marker = self.guard.begin_apply(self.guard.png_key(png));
                self.native.write_png(png, rgba.as_ref(), marker, approved)
            }
        };
        drop(content);
        match written {
            Ok(after) => {
                self.guard.applied(after);
                log::debug!("clipboard: {name} of {bytes} bytes from {peer} applied");
                self.shared.applied(self.generation, &inbound, kind);
            }
            Err(NativeError::Changed) => log::debug!(
                "clipboard: {name} of {bytes} bytes from {peer} not applied, a newer local copy wins"
            ),
            Err(error) => {
                log::warn!("clipboard: {name} of {bytes} bytes from {peer} not applied: {error}");
            }
        }
    }
}

/// Routes one link's callbacks to the hub; they run on the link thread and return promptly.
struct LinkSink {
    shared: Weak<Shared>,
    id: u64,
}

impl ClipboardSink for LinkSink {
    fn received(&self, item: InboundClipboard) {
        if let Some(shared) = self.shared.upgrade() {
            shared.received(self.id, item);
        }
    }

    fn peer_state(&self, _: CertificateFingerprint, enabled: bool) {
        if let Some(shared) = self.shared.upgrade() {
            shared.peer_state(self.id, enabled);
        }
    }

    fn note(&self, peer: CertificateFingerprint, note: ClipboardNote) {
        if let Some(shared) = self.shared.upgrade() {
            shared.note(self.id, peer, note);
        }
    }

    /// Detaches at once, before the attachment is dropped. This runs on the link thread, so a
    /// read in flight is left to finish and be discarded rather than waited for.
    fn closed(&self, _: CertificateFingerprint) {
        if let Some(shared) = self.shared.upgrade() {
            shared.detach_within(self.id, Duration::ZERO);
        }
    }
}

/// The `monhop-clipboard-io` thread: a current-thread runtime running every clipboard link, apart
/// from the sharing runtime that carries input.
struct IoRuntime {
    handle: tokio::runtime::Handle,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    thread: JoinHandle<()>,
}

impl IoRuntime {
    fn start() -> io::Result<Self> {
        let (handle_sender, handle_receiver) = mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel::<()>();
        let thread = thread::Builder::new()
            .name("monhop-clipboard-io".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = handle_sender.send(Err(error));
                        return;
                    }
                };
                let _ = handle_sender.send(Ok(runtime.handle().clone()));
                runtime.block_on(async {
                    let _ = stopped.await;
                });
            })?;
        let handle = handle_receiver
            .recv()
            .map_err(|_| io::Error::other("the link thread ended before it started"))??;
        Ok(Self {
            handle,
            stop: Mutex::new(Some(stop)),
            thread,
        })
    }

    /// Ending the runtime drops every link task still on it.
    fn stop(&self) {
        if let Some(stop) = lock(&self.stop).take() {
            let _ = stop.send(());
        }
    }

    fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

fn outbound_notice(error: OutboundClipboardError) -> Notice {
    match error {
        OutboundClipboardError::TooLarge
        | OutboundClipboardError::Text(ClipboardTextError::TooLarge) => Notice::TooLarge,
        _ => Notice::Unsupported,
    }
}

fn transfer_kind(kind: ClipboardKind) -> Option<TransferKind> {
    match kind {
        ClipboardKind::Text => Some(TransferKind::Text),
        ClipboardKind::Png => Some(TransferKind::Image),
        ClipboardKind::State => None,
    }
}

fn kind_name(kind: ClipboardKind) -> &'static str {
    transfer_kind(kind).map_or("state", transfer_name)
}

fn transfer_name(kind: TransferKind) -> &'static str {
    match kind {
        TransferKind::Text => "text",
        TransferKind::Image => "image",
    }
}

/// The first eight hexadecimal characters, as the link's own log lines name a peer.
fn short(peer: CertificateFingerprint) -> String {
    let mut hex = peer.full_hex();
    hex.truncate(8);
    hex
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The hub against an in-memory clipboard, with real links over loopback QUIC connections: the
/// hub's end on its own link thread, each peer's end on the test runtime.
#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    use monhop_protocol::clipboard::MAX_CLIPBOARD_PNG;
    use monhop_transport::crypto::{
        DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer,
    };

    use super::*;
    use crate::clipboard::content::SourceMarker;
    use crate::clipboard::native::{Content, Origin, Skip, Snapshot};

    const WAIT: Duration = Duration::from_secs(10);
    /// Many poll ticks and loopback round trips: what has not happened by then is not happening.
    const QUIET: Duration = Duration::from_millis(300);
    const EPOCH: u64 = 7;

    /// Tauri state and the sharing tasks that hold attachments need both to cross threads.
    const _: fn() = || {
        fn shareable<T: Send + Sync>() {}
        shareable::<ClipboardHub>();
        shareable::<ClipboardAttachment>();
    };

    #[derive(Clone)]
    enum Item {
        Text(String),
        Png(Vec<u8>),
        Dib(Vec<u8>),
        Skipped(Skip),
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Written {
        Text(String),
        Png { png: Vec<u8>, with_pixels: bool },
    }

    #[derive(Default)]
    struct Board {
        marker: u64,
        item: Option<Item>,
        source: Option<SourceMarker>,
        opened: usize,
        closed: usize,
        /// Reads started.
        reads: usize,
        /// Reads started and not yet returned.
        reading: usize,
        waits: usize,
        writes: Vec<Written>,
    }

    /// The test's handle on the in-memory clipboard the hub's adapter sees.
    #[derive(Clone, Default)]
    struct FakeClipboard(Arc<Mutex<Board>>);

    impl FakeClipboard {
        fn copy(&self, item: Item) {
            let mut board = self.board();
            board.marker += 1;
            board.item = Some(item);
            board.source = None;
        }

        fn board(&self) -> MutexGuard<'_, Board> {
            lock(&self.0)
        }

        fn reads(&self) -> usize {
            self.board().reads
        }

        fn writes(&self) -> usize {
            self.board().writes.len()
        }
    }

    /// Blocks every call that passes it while held, counting the calls that arrived.
    #[derive(Default)]
    struct Hold {
        held: Mutex<bool>,
        released: Condvar,
        arrived: AtomicUsize,
    }

    impl Hold {
        fn hold(&self) {
            *lock(&self.held) = true;
        }

        fn release(&self) {
            *lock(&self.held) = false;
            self.released.notify_all();
        }

        fn arrived(&self) -> usize {
            self.arrived.load(Ordering::SeqCst)
        }

        fn pass(&self) {
            self.arrived.fetch_add(1, Ordering::SeqCst);
            let mut held = lock(&self.held);
            while *held {
                held = self
                    .released
                    .wait(held)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
    }

    /// Where the fake adapter can be held: opening the clipboard, and inside each read.
    #[derive(Default)]
    struct Holds {
        open: Hold,
        read: Hold,
    }

    struct FakeNative {
        board: Arc<Mutex<Board>>,
        holds: Arc<Holds>,
        thread: thread::Thread,
    }

    impl FakeNative {
        fn open(board: Arc<Mutex<Board>>, holds: Arc<Holds>) -> Self {
            holds.open.pass();
            lock(&board).opened += 1;
            Self {
                board,
                holds,
                thread: thread::current(),
            }
        }

        fn write(
            &self,
            written: Written,
            item: Item,
            marker: SourceMarker,
            expected: u64,
        ) -> Result<u64, NativeError> {
            let mut board = lock(&self.board);
            if board.marker != expected {
                return Err(NativeError::Changed);
            }
            // Like Windows, one write can move the counter more than once.
            board.marker += 2;
            board.item = Some(item);
            board.source = Some(marker);
            board.writes.push(written);
            Ok(board.marker)
        }
    }

    impl Drop for FakeNative {
        fn drop(&mut self) {
            lock(&self.board).closed += 1;
        }
    }

    struct Unpark(thread::Thread);

    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    impl NativeClipboard for FakeNative {
        fn change_marker(&self) -> u64 {
            lock(&self.board).marker
        }

        fn wait(&mut self, timeout: Duration) {
            lock(&self.board).waits += 1;
            thread::park_timeout(timeout);
        }

        fn waker(&self) -> Waker {
            Waker::from(Arc::new(Unpark(self.thread.clone())))
        }

        fn access(&self) -> Access {
            Access::Allowed
        }

        fn read(&mut self, _: &ReadLimits) -> Result<Snapshot, NativeError> {
            {
                let mut board = lock(&self.board);
                board.reads += 1;
                board.reading += 1;
            }
            self.holds.read.pass();
            let mut board = lock(&self.board);
            board.reading -= 1;
            let origin = board
                .source
                .map_or(Origin::Local, |marker| Origin::MonHop(Some(marker)));
            let content = match (&board.item, origin) {
                (_, Origin::MonHop(_)) | (None, _) => Content::Empty,
                (Some(Item::Text(text)), _) => Content::Text(text.clone()),
                (Some(Item::Png(png)), _) => Content::Png(png.clone()),
                (Some(Item::Dib(dib)), _) => Content::Dib(dib.clone()),
                (Some(Item::Skipped(skip)), _) => Content::Skipped(*skip),
            };
            Ok(Snapshot {
                before: board.marker,
                after: board.marker,
                origin,
                content,
            })
        }

        fn write_text(
            &mut self,
            text: &str,
            marker: SourceMarker,
            expected: u64,
        ) -> Result<u64, NativeError> {
            let written = Written::Text(text.to_owned());
            self.write(written, Item::Text(text.to_owned()), marker, expected)
        }

        fn write_png(
            &mut self,
            png: &[u8],
            rgba: Option<&Rgba>,
            marker: SourceMarker,
            expected: u64,
        ) -> Result<u64, NativeError> {
            let written = Written::Png {
                png: png.to_vec(),
                with_pixels: rgba.is_some(),
            };
            self.write(written, Item::Png(png.to_vec()), marker, expected)
        }
    }

    /// The production re-encode, observable and able to hold every decode until released.
    #[derive(Default)]
    struct Decoder {
        inputs: Mutex<Vec<Vec<u8>>>,
        active: AtomicUsize,
        most_active: AtomicUsize,
        held: Mutex<bool>,
        released: Condvar,
    }

    impl Decoder {
        fn holding() -> Arc<Self> {
            let decoder = Self::default();
            *lock(&decoder.held) = true;
            Arc::new(decoder)
        }

        fn release(&self) {
            *lock(&self.held) = false;
            self.released.notify_all();
        }

        fn reencode(&self, png: &[u8]) -> Result<(Rgba, Vec<u8>), ImageError> {
            lock(&self.inputs).push(png.to_vec());
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_active.fetch_max(active, Ordering::SeqCst);
            let mut held = lock(&self.held);
            while *held {
                held = self
                    .released
                    .wait(held)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            drop(held);
            let decoded = image::reencode_png(png);
            self.active.fetch_sub(1, Ordering::SeqCst);
            decoded
        }

        fn inputs(&self) -> Vec<Vec<u8>> {
            lock(&self.inputs).clone()
        }
    }

    #[derive(Default)]
    struct Views(Mutex<Vec<ClipboardView>>);

    impl ViewEmitter for Arc<Views> {
        fn view_changed(&self, view: &ClipboardView) {
            lock(&self.0).push(view.clone());
        }
    }

    struct Harness {
        hub: ClipboardHub,
        clipboard: FakeClipboard,
        decoder: Arc<Decoder>,
        views: Arc<Views>,
        holds: Arc<Holds>,
    }

    impl Harness {
        /// Stopping waits for a read in progress as long as any test waits for anything.
        fn new(decoder: Arc<Decoder>, probe_on_switch: bool) -> Self {
            Self::with_read_wait(decoder, probe_on_switch, WAIT)
        }

        fn with_read_wait(
            decoder: Arc<Decoder>,
            probe_on_switch: bool,
            read_wait: Duration,
        ) -> Self {
            capture_logs();
            let clipboard = FakeClipboard::default();
            let board = Arc::clone(&clipboard.0);
            let holds = Arc::new(Holds::default());
            let adapter_holds = Arc::clone(&holds);
            let reencode = Arc::clone(&decoder);
            let backend = Backend {
                native: Box::new(move || {
                    let native = FakeNative::open(Arc::clone(&board), Arc::clone(&adapter_holds));
                    Ok(Box::new(native) as Box<dyn NativeClipboard>)
                }),
                reencode: Box::new(move |png| reencode.reencode(png)),
                poll: Duration::from_millis(10),
                probe_on_switch,
                read_wait,
            };
            let views = Arc::new(Views::default());
            let hub = ClipboardHub::launch(Box::new(Arc::clone(&views)), None, backend);
            Self {
                hub,
                clipboard,
                decoder,
                views,
                holds,
            }
        }

        fn off() -> Self {
            Self::new(Arc::new(Decoder::default()), false)
        }

        fn on() -> Self {
            let harness = Self::off();
            assert!(harness.hub.set_enabled(true).enabled);
            harness
        }

        async fn watching(&self) {
            until("the clipboard thread to take its baseline", || {
                self.clipboard.board().waits > 0
            })
            .await;
        }

        /// The peer's switch as the hub's view shows it; `None` while not attached.
        fn reported(&self, peer: &Connected) -> Option<Option<bool>> {
            let fingerprint = peer.peer.full_hex();
            self.hub
                .view()
                .peers
                .into_iter()
                .find(|view| view.fingerprint == fingerprint)
                .map(|view| view.peer_enabled)
        }

        /// Waits until each link knows the other end's switch, so content can move both ways.
        async fn ready(&self, peer: &Connected) {
            let expected = peer.remote.as_ref().map(|remote| *remote.switch.borrow());
            until("the peer's switch to reach the hub", || {
                self.reported(peer) == Some(expected)
            })
            .await;
            if let Some(remote) = &peer.remote {
                let hub_on = self.hub.view().enabled;
                until("the hub's switch to reach the peer", || {
                    remote.recorder.hub_enabled() == Some(hub_on)
                })
                .await;
            }
        }

        fn outbound_is_empty(&self) -> bool {
            self.hub.shared.outbound.borrow().is_none()
        }

        fn reading(&self) -> usize {
            self.clipboard.board().reading
        }

        /// Runs `stop` on another thread while a read is held there, checks that it is still
        /// waiting well after, then releases the read. Returns what `stop` returned and the reads
        /// in progress as it returned.
        fn stop_during_read<T: Send>(&self, stop: impl FnOnce() -> T + Send) -> (T, usize) {
            assert_eq!(self.reading(), 1, "a read is held");
            thread::scope(|scope| {
                let stopping = scope.spawn(|| {
                    let stopped = stop();
                    (stopped, self.reading())
                });
                thread::sleep(QUIET);
                assert!(
                    !stopping.is_finished(),
                    "stopping waits for the read in progress"
                );
                self.holds.read.release();
                stopping.join().expect("stopping does not panic")
            })
        }

        /// Copies after sharing stopped, and checks that nothing reads it.
        async fn reads_nothing_more(&self) {
            let reads = self.clipboard.reads();
            self.clipboard
                .copy(Item::Text("copied after sharing stopped".into()));
            settle().await;
            assert_eq!(
                self.clipboard.reads(),
                reads,
                "a read started after sharing stopped"
            );
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.decoder.release();
            self.holds.open.release();
            self.holds.read.release();
        }
    }

    enum Seen {
        Received(ClipboardKind, Vec<u8>),
        PeerState(bool),
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Seen>>);

    impl ClipboardSink for Recorder {
        fn received(&self, item: InboundClipboard) {
            lock(&self.0).push(Seen::Received(item.kind, item.bytes));
        }

        fn peer_state(&self, _: CertificateFingerprint, enabled: bool) {
            lock(&self.0).push(Seen::PeerState(enabled));
        }

        fn note(&self, _: CertificateFingerprint, _: ClipboardNote) {}

        fn closed(&self, _: CertificateFingerprint) {}
    }

    impl Recorder {
        fn received(&self) -> Vec<(ClipboardKind, Vec<u8>)> {
            lock(&self.0)
                .iter()
                .filter_map(|seen| match seen {
                    Seen::Received(kind, bytes) => Some((*kind, bytes.clone())),
                    Seen::PeerState(_) => None,
                })
                .collect()
        }

        fn hub_enabled(&self) -> Option<bool> {
            lock(&self.0).iter().rev().find_map(|seen| match seen {
                Seen::PeerState(enabled) => Some(*enabled),
                Seen::Received(..) => None,
            })
        }
    }

    /// The other computer's end: a real link with its own switch and outbound slot.
    struct Remote {
        recorder: Arc<Recorder>,
        switch: watch::Sender<bool>,
        outbound: watch::Sender<Option<Arc<OutboundClipboard>>>,
        _link: ClipboardLink,
    }

    impl Remote {
        fn publish(&self, kind: ClipboardKind, bytes: Vec<u8>) {
            let item = OutboundClipboard::new(kind, bytes).expect("a valid outbound item");
            self.outbound.send_replace(Some(Arc::new(item)));
        }
    }

    struct Connected {
        peer: CertificateFingerprint,
        attachment: Option<ClipboardAttachment>,
        remote: Option<Remote>,
        pair: Pair,
    }

    impl Connected {
        fn id(&self) -> u64 {
            let attachment = self.attachment.as_ref().expect("still attached");
            attachment.id.expect("a live attachment")
        }

        fn remote(&self) -> &Remote {
            self.remote.as_ref().expect("the peer runs a link")
        }

        fn inbound(
            &self,
            kind: ClipboardKind,
            bytes: &[u8],
            started_at: Instant,
        ) -> InboundClipboard {
            InboundClipboard {
                peer: self.peer,
                kind,
                bytes: bytes.to_vec(),
                sequence: 1,
                started_at,
            }
        }
    }

    struct Pair {
        hub_side: quinn::Connection,
        remote_side: quinn::Connection,
        hub_id: CertificateFingerprint,
        remote_id: CertificateFingerprint,
        _endpoints: [quinn::Endpoint; 2],
    }

    fn epoch() -> SessionEpoch {
        SessionEpoch::new(EPOCH).expect("nonzero epoch")
    }

    async fn connection_pair() -> Pair {
        let hub_identity = DeviceIdentity::generate().expect("hub identity");
        let remote_identity = DeviceIdentity::generate().expect("peer identity");
        let pin = |identity: &DeviceIdentity| {
            VerifiedPeer::from_certificate_der(
                identity.certificate_der(),
                &identity.fingerprint().full_hex(),
            )
            .expect("generated identity has a full matching pin")
        };
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let listener = quinn::Endpoint::server(
            SecureQuicConfig::server(&hub_identity, &pin(&remote_identity))
                .expect("server TLS configuration"),
            loopback,
        )
        .expect("loopback server endpoint");
        let mut dialer = quinn::Endpoint::client(loopback).expect("loopback client endpoint");
        dialer.set_default_client_config(
            SecureQuicConfig::client(&remote_identity, &pin(&hub_identity))
                .expect("client TLS configuration"),
        );
        let connecting = dialer
            .connect(
                listener.local_addr().expect("server address"),
                LOCAL_TLS_SERVER_NAME,
            )
            .expect("loopback connection");
        let (remote_side, hub_side) = tokio::join!(
            async { connecting.await.expect("client TLS connection") },
            async {
                listener
                    .accept()
                    .await
                    .expect("incoming connection")
                    .await
                    .expect("server TLS connection")
            },
        );
        Pair {
            hub_side,
            remote_side,
            hub_id: hub_identity.fingerprint(),
            remote_id: remote_identity.fingerprint(),
            _endpoints: [dialer, listener],
        }
    }

    /// Attaches a new connection to the hub; `remote_switch` is `None` for a peer running no
    /// clipboard link at all, which therefore never reports a switch.
    async fn connect(hub: &ClipboardHub, remote_switch: Option<bool>) -> Connected {
        let pair = connection_pair().await;
        let attachment = hub.attach(pair.remote_id, pair.hub_side.clone(), epoch());
        let remote = remote_switch.map(|enabled| {
            let recorder = Arc::new(Recorder::default());
            let (switch, local_enabled) = watch::channel(enabled);
            let (outbound, outbound_watch) = watch::channel(None);
            let link = session_clipboard::attach(
                pair.remote_side.clone(),
                ClipboardLinkConfig {
                    peer: pair.hub_id,
                    epoch: epoch(),
                    local_enabled,
                    outbound: outbound_watch,
                    sink: Arc::clone(&recorder) as Arc<dyn ClipboardSink>,
                },
                &tokio::runtime::Handle::current(),
            );
            Remote {
                recorder,
                switch,
                outbound,
                _link: link,
            }
        });
        Connected {
            peer: pair.remote_id,
            attachment: Some(attachment),
            remote,
            pair,
        }
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn settle() {
        tokio::time::sleep(QUIET).await;
    }

    fn one_pixel_png(rgba: [u8; 4]) -> Vec<u8> {
        let image = Rgba::new(1, 1, rgba.to_vec()).expect("one pixel");
        image::encode_png(&image, MAX_CLIPBOARD_PNG as usize).expect("encode")
    }

    /// BITMAPINFOHEADER, one 32-bit BI_RGB pixel.
    fn one_pixel_dib(bgra: [u8; 4]) -> Vec<u8> {
        let mut dib = Vec::new();
        for field in [40_u32, 1, 1] {
            dib.extend_from_slice(&field.to_le_bytes());
        }
        dib.extend_from_slice(&1_u16.to_le_bytes());
        dib.extend_from_slice(&32_u16.to_le_bytes());
        for field in [0_u32, 4, 0, 0, 0, 0] {
            dib.extend_from_slice(&field.to_le_bytes());
        }
        dib.extend_from_slice(&bgra);
        dib
    }

    fn pixels_of(png: &[u8]) -> Vec<u8> {
        image::decode_png(png)
            .expect("a valid PNG")
            .pixels()
            .to_vec()
    }

    struct Capture;

    static CAPTURE: Capture = Capture;
    static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    const MAX_CAPTURED: usize = 200_000;

    impl log::Log for Capture {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.target().starts_with("monhop")
        }

        fn log(&self, record: &log::Record<'_>) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let mut lines = lock(&CAPTURED);
            if lines.len() < MAX_CAPTURED {
                lines.push(format!("{} {}", record.target(), record.args()));
            }
        }

        fn flush(&self) {}
    }

    /// Installs the capture for the whole test binary; every harness calls it so it lands before
    /// any test installs another logger. False if one was installed first.
    fn capture_logs() -> bool {
        static INSTALLED: OnceLock<bool> = OnceLock::new();
        *INSTALLED.get_or_init(|| {
            let installed = log::set_logger(&CAPTURE).is_ok();
            if installed {
                log::set_max_level(log::LevelFilter::Debug);
            }
            installed
        })
    }

    #[tokio::test]
    async fn off_never_starts_the_native_thread_or_reads() {
        let harness = Harness::off();
        let peer = connect(&harness.hub, Some(true)).await;
        // The links still exchange switch states while this side is off.
        harness.ready(&peer).await;
        harness
            .clipboard
            .copy(Item::Text("copied while sharing is off".into()));
        peer.remote().publish(
            ClipboardKind::Text,
            b"sent toward a switched-off hub".to_vec(),
        );
        settle().await;

        let board = harness.clipboard.board();
        assert_eq!((board.opened, board.reads, board.writes.len()), (0, 0, 0));
        drop(board);
        assert!(peer.remote().recorder.received().is_empty());
        assert!(harness.hub.shared.lock().watch.is_none());
        let view = harness.hub.view();
        assert!(!view.enabled);
        assert_eq!(view.peers.len(), 1);
    }

    #[tokio::test]
    async fn on_with_no_peers_reads_nothing() {
        let harness = Harness::on();
        harness
            .clipboard
            .copy(Item::Text("copied with nobody connected".into()));
        settle().await;
        assert_eq!(harness.clipboard.board().opened, 0);
        assert_eq!(harness.clipboard.reads(), 0);

        let mut peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;
        settle().await;
        assert_eq!(
            harness.clipboard.reads(),
            0,
            "what was copied before sharing started stays local"
        );
        assert!(peer.remote().recorder.received().is_empty());

        peer.attachment = None;
        until("the clipboard thread to stop", || {
            harness.clipboard.board().closed == 1
        })
        .await;
        assert!(harness.hub.view().peers.is_empty());
        assert!(harness.outbound_is_empty());
    }

    #[tokio::test]
    async fn a_change_fans_out_once_to_three_peers() {
        let harness = Harness::on();
        let mut peers = Vec::new();
        for _ in 0..3 {
            peers.push(connect(&harness.hub, Some(true)).await);
        }
        harness.watching().await;
        for peer in &peers {
            harness.ready(peer).await;
        }
        assert_eq!(harness.clipboard.board().opened, 1, "one clipboard thread");

        harness.clipboard.copy(Item::Text("fan out\r\n".into()));
        for peer in &peers {
            until("every peer to receive the copy", || {
                !peer.remote().recorder.received().is_empty()
            })
            .await;
        }
        settle().await;
        for peer in &peers {
            assert_eq!(
                peer.remote().recorder.received(),
                [(ClipboardKind::Text, b"fan out\n".to_vec())]
            );
        }
        assert_eq!(harness.clipboard.reads(), 1);
        let last = harness.hub.view().last.expect("a transfer is shown");
        assert_eq!(
            (last.direction, last.kind, last.bytes),
            (Direction::Sent, TransferKind::Text, 8)
        );
    }

    #[tokio::test]
    async fn only_peers_that_reported_on_receive_content() {
        let harness = Harness::on();
        let on = connect(&harness.hub, Some(true)).await;
        let off = connect(&harness.hub, Some(false)).await;
        let silent = connect(&harness.hub, None).await;
        harness.watching().await;
        harness.ready(&on).await;
        harness.ready(&off).await;
        assert_eq!(harness.reported(&silent), Some(None));

        harness
            .clipboard
            .copy(Item::Text("only for switches that are on".into()));
        until("the peer that is on to receive the copy", || {
            !on.remote().recorder.received().is_empty()
        })
        .await;
        settle().await;
        assert_eq!(on.remote().recorder.received().len(), 1);
        assert!(off.remote().recorder.received().is_empty());
        assert!(
            tokio::time::timeout(QUIET, silent.pair.remote_side.accept_uni())
                .await
                .is_err(),
            "nothing reaches a peer that never reported its switch"
        );
        let last = harness.hub.view().last.expect("a transfer is shown");
        assert_eq!(last.peer, on.peer.full_hex());
    }

    #[tokio::test]
    async fn received_content_is_never_re_sent() {
        let harness = Harness::on();
        let from = connect(&harness.hub, Some(true)).await;
        let other = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&from).await;
        harness.ready(&other).await;

        from.remote()
            .publish(ClipboardKind::Text, b"from a peer".to_vec());
        until("the received text to be written", || {
            harness.clipboard.writes() == 1
        })
        .await;
        settle().await;
        assert_eq!(
            harness.clipboard.board().writes,
            [Written::Text("from a peer".into())]
        );
        assert!(from.remote().recorder.received().is_empty());
        assert!(other.remote().recorder.received().is_empty());
        assert!(harness.outbound_is_empty());
        let last = harness.hub.view().last.expect("a transfer is shown");
        assert_eq!(
            (last.direction, last.kind, last.peer),
            (
                Direction::Received,
                TransferKind::Text,
                from.peer.full_hex()
            )
        );

        // A clipboard manager copies it again without MonHop's source marker.
        harness.clipboard.copy(Item::Text("from a peer".into()));
        until("the re-copy to be read", || harness.clipboard.reads() == 1).await;
        settle().await;
        assert!(from.remote().recorder.received().is_empty());
        assert!(other.remote().recorder.received().is_empty());

        harness.clipboard.copy(Item::Text("copied here".into()));
        for peer in [&from, &other] {
            until("a local copy to reach every peer", || {
                !peer.remote().recorder.received().is_empty()
            })
            .await;
            assert_eq!(
                peer.remote().recorder.received(),
                [(ClipboardKind::Text, b"copied here".to_vec())]
            );
        }
    }

    #[tokio::test]
    async fn own_writes_are_skipped() {
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;

        peer.remote()
            .publish(ClipboardKind::Text, b"written by the hub".to_vec());
        until("the text to be written", || harness.clipboard.writes() == 1).await;
        peer.remote()
            .publish(ClipboardKind::Png, one_pixel_png([9, 8, 7, 255]));
        until("the image to be written", || {
            harness.clipboard.writes() == 2
        })
        .await;
        settle().await;

        let board = harness.clipboard.board();
        assert_eq!(board.reads, 0, "the hub never reads back its own writes");
        let Written::Png { png, with_pixels } = &board.writes[1] else {
            panic!("an image was written");
        };
        assert_eq!(pixels_of(png), [9, 8, 7, 255]);
        assert_eq!(*with_pixels, cfg!(windows));
        drop(board);
        assert!(peer.remote().recorder.received().is_empty());
        assert!(harness.outbound_is_empty());
    }

    #[tokio::test]
    async fn concealed_and_file_items_are_skipped() {
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;

        harness.clipboard.copy(Item::Skipped(Skip::Concealed));
        until("the concealed item to be judged", || {
            harness.clipboard.reads() == 1
        })
        .await;
        harness.clipboard.copy(Item::Skipped(Skip::Files));
        until("the file copy to be judged", || {
            harness.clipboard.reads() == 2
        })
        .await;
        settle().await;
        assert!(peer.remote().recorder.received().is_empty());
        assert!(harness.outbound_is_empty());
        assert_eq!(
            harness.hub.view().notice,
            None,
            "skipped by design, silently"
        );

        harness.clipboard.copy(Item::Skipped(Skip::TooLarge));
        until("an oversized item to be noticed", || {
            harness.hub.view().notice == Some(Notice::TooLarge)
        })
        .await;

        harness
            .clipboard
            .copy(Item::Text("after the skipped items".into()));
        until("the next copy to go out", || {
            !peer.remote().recorder.received().is_empty()
        })
        .await;
        assert_eq!(
            peer.remote().recorder.received(),
            [(ClipboardKind::Text, b"after the skipped items".to_vec())]
        );
    }

    #[tokio::test]
    async fn a_copied_bitmap_goes_out_as_png() {
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;

        harness
            .clipboard
            .copy(Item::Dib(one_pixel_dib([3, 2, 1, 255])));
        until("the converted image to arrive", || {
            !peer.remote().recorder.received().is_empty()
        })
        .await;
        let received = peer.remote().recorder.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].0, ClipboardKind::Png);
        assert_eq!(pixels_of(&received[0].1), [1, 2, 3, 255]);
    }

    #[tokio::test]
    async fn turning_off_stops_the_thread_and_withdraws_outbound() {
        let harness = Harness::new(Decoder::holding(), false);
        assert!(harness.hub.set_enabled(true).enabled);
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;

        harness
            .clipboard
            .copy(Item::Text("offered before the switch".into()));
        until("the copy to go out", || {
            !peer.remote().recorder.received().is_empty()
        })
        .await;
        assert!(!harness.outbound_is_empty());
        peer.remote()
            .publish(ClipboardKind::Png, one_pixel_png([1, 1, 1, 255]));
        until("the incoming image to start decoding", || {
            harness.decoder.inputs().len() == 1
        })
        .await;

        let view = harness.hub.set_enabled(false);
        assert!(!view.enabled);
        assert!(
            harness.outbound_is_empty(),
            "the item on offer is withdrawn"
        );
        {
            let state = harness.hub.shared.lock();
            assert!(state.watch.is_none());
            assert!(state.apply.is_none() && state.decode.is_none() && state.encode.is_none());
        }
        until("the clipboard thread to stop", || {
            harness.clipboard.board().closed == 1
        })
        .await;
        until("the peer to see the switch off", || {
            peer.remote().recorder.hub_enabled() == Some(false)
        })
        .await;
        harness.decoder.release();

        let reads = harness.clipboard.reads();
        harness
            .clipboard
            .copy(Item::Text("copied after the switch".into()));
        settle().await;
        assert_eq!(harness.clipboard.reads(), reads);
        assert_eq!(
            harness.clipboard.writes(),
            0,
            "the image decoded meanwhile is dropped"
        );
        assert_eq!(peer.remote().recorder.received().len(), 1);
        assert_eq!(harness.clipboard.board().opened, 1);
    }

    #[tokio::test]
    async fn switching_off_waits_for_an_in_flight_read_and_none_starts_after() {
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;
        harness.holds.read.hold();
        harness
            .clipboard
            .copy(Item::Text("read as the switch goes off".into()));
        until("the read to start", || harness.reading() == 1).await;

        let (view, reading) = harness.stop_during_read(|| harness.hub.set_enabled(false));
        assert!(!view.enabled);
        assert_eq!(
            reading, 0,
            "no read is in progress once switching off returns"
        );
        harness.reads_nothing_more().await;
        assert_eq!(harness.clipboard.reads(), 1);
        until("the clipboard thread to stop", || {
            harness.clipboard.board().closed == 1
        })
        .await;
        assert!(
            peer.remote().recorder.received().is_empty(),
            "what the late read returned stays here"
        );
        assert!(harness.outbound_is_empty());
    }

    #[tokio::test]
    async fn the_last_detach_waits_for_an_in_flight_read_and_none_starts_after() {
        let harness = Harness::on();
        let mut first = connect(&harness.hub, Some(true)).await;
        let mut last = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&first).await;
        harness.ready(&last).await;
        harness.holds.read.hold();
        harness
            .clipboard
            .copy(Item::Text("read as the last computer leaves".into()));
        until("the read to start", || harness.reading() == 1).await;

        // Another computer is still attached, so this detach neither stops nor waits.
        first.attachment = None;
        assert_eq!(harness.reading(), 1);
        assert!(harness.hub.shared.lock().watch.is_some());

        let attachment = last.attachment.take().expect("still attached");
        let ((), reading) = harness.stop_during_read(move || drop(attachment));
        assert_eq!(
            reading, 0,
            "no read is in progress once the last detach returns"
        );
        harness.reads_nothing_more().await;
        assert_eq!(harness.clipboard.reads(), 1);
        until("the clipboard thread to stop", || {
            harness.clipboard.board().closed == 1
        })
        .await;
        assert!(harness.hub.view().peers.is_empty());
        for peer in [&first, &last] {
            assert!(peer.remote().recorder.received().is_empty());
        }
    }

    #[tokio::test]
    async fn a_closed_connection_detaches_before_its_attachment_drops() {
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;
        peer.pair.remote_side.close(quinn::VarInt::from_u32(0), b"");
        until("the closed connection to detach", || {
            harness.hub.view().peers.is_empty()
        })
        .await;
        assert!(peer.attachment.is_some());
        until("the clipboard thread to stop", || {
            harness.clipboard.board().closed == 1
        })
        .await;
        harness.reads_nothing_more().await;
        // Switching on again while the attachment is still held starts no watch and no probe.
        assert!(!harness.hub.set_enabled(false).enabled);
        assert!(harness.hub.set_enabled(true).enabled);
        settle().await;
        assert!(harness.hub.shared.lock().watch.is_none());
        harness.reads_nothing_more().await;
    }

    #[tokio::test]
    async fn a_probe_read_never_starts_after_the_switch_turns_off() {
        let harness = Harness::new(Arc::new(Decoder::default()), true);
        let peer = connect(&harness.hub, Some(true)).await;
        harness.ready(&peer).await;

        // Off while the clipboard thread is still opening the clipboard: no probe follows.
        harness.holds.open.hold();
        assert!(harness.hub.set_enabled(true).enabled);
        until("the clipboard thread to open the clipboard", || {
            harness.holds.open.arrived() == 1
        })
        .await;
        assert!(!harness.hub.set_enabled(false).enabled);
        harness.holds.open.release();
        until("the clipboard thread to stop", || {
            harness.clipboard.board().closed == 1
        })
        .await;
        assert_eq!(harness.clipboard.board().opened, 1);
        assert_eq!(harness.clipboard.reads(), 0, "no probe after the switch");

        // Off during the probe: switching off waits for it, and nothing is read after.
        harness.holds.read.hold();
        assert!(harness.hub.set_enabled(true).enabled);
        until("the probe to start", || harness.reading() == 1).await;
        let (view, reading) = harness.stop_during_read(|| harness.hub.set_enabled(false));
        assert!(!view.enabled);
        assert_eq!(
            reading, 0,
            "no probe is in progress once switching off returns"
        );
        harness.reads_nothing_more().await;
        assert_eq!(harness.clipboard.reads(), 1);
        until("the clipboard thread to stop", || {
            harness.clipboard.board().closed == 2
        })
        .await;
        assert!(peer.remote().recorder.received().is_empty());
    }

    #[tokio::test]
    async fn switching_off_stops_waiting_for_a_stuck_read_and_none_starts_after() {
        const GIVE_UP: Duration = Duration::from_millis(100);
        let harness = Harness::with_read_wait(Arc::new(Decoder::default()), false, GIVE_UP);
        assert!(harness.hub.set_enabled(true).enabled);
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;
        harness.holds.read.hold();
        harness
            .clipboard
            .copy(Item::Text("read by a clipboard that hangs".into()));
        until("the read to start", || harness.reading() == 1).await;

        let started = Instant::now();
        assert!(!harness.hub.set_enabled(false).enabled);
        let waited = started.elapsed();
        assert!(waited >= GIVE_UP && waited < WAIT, "waited {waited:?}");
        assert_eq!(harness.reading(), 1, "the stuck read is left to finish");

        harness.holds.read.release();
        until("the stuck read to finish", || harness.reading() == 0).await;
        harness.reads_nothing_more().await;
        assert_eq!(harness.clipboard.reads(), 1);
        until("the clipboard thread to stop", || {
            harness.clipboard.board().closed == 1
        })
        .await;
        assert!(peer.remote().recorder.received().is_empty());
    }

    #[tokio::test]
    async fn a_local_copy_after_an_incoming_start_wins() {
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;

        let started_before_the_copy = Instant::now();
        harness.clipboard.copy(Item::Text("copied here".into()));
        until("the local copy to be read", || {
            harness.clipboard.reads() == 1
        })
        .await;
        let stale = peer.inbound(
            ClipboardKind::Text,
            b"started before the copy",
            started_before_the_copy,
        );
        harness.hub.shared.received(peer.id(), stale);
        settle().await;
        assert_eq!(harness.clipboard.writes(), 0);
        assert!(matches!(
            &harness.clipboard.board().item,
            Some(Item::Text(text)) if text == "copied here"
        ));

        let fresh = peer.inbound(
            ClipboardKind::Text,
            b"started after the copy",
            Instant::now(),
        );
        harness.hub.shared.received(peer.id(), fresh);
        until("the later transfer to be written", || {
            harness.clipboard.writes() == 1
        })
        .await;
        assert_eq!(
            harness.clipboard.board().writes,
            [Written::Text("started after the copy".into())]
        );
    }

    #[tokio::test]
    async fn an_image_is_decoded_once_and_superseded_items_are_dropped() {
        let harness = Harness::new(Decoder::holding(), false);
        assert!(harness.hub.set_enabled(true).enabled);
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;
        let images = [1, 2, 3].map(|red| one_pixel_png([red, 0, 0, 255]));
        let deliver = |image: &[u8]| {
            let item = peer.inbound(ClipboardKind::Png, image, Instant::now());
            harness.hub.shared.received(peer.id(), item);
        };

        deliver(&images[0]);
        until("the first image to start decoding", || {
            harness.decoder.inputs().len() == 1
        })
        .await;
        deliver(&images[1]);
        deliver(&images[2]);
        settle().await;
        assert_eq!(harness.decoder.inputs().len(), 1, "one decode at a time");

        harness.decoder.release();
        until("the newest image to be written", || {
            harness.clipboard.writes() == 1
        })
        .await;
        settle().await;
        assert_eq!(
            harness.decoder.inputs(),
            [images[0].clone(), images[2].clone()],
            "the superseded image is never decoded"
        );
        assert_eq!(harness.decoder.most_active.load(Ordering::SeqCst), 1);
        let board = harness.clipboard.board();
        assert_eq!(board.writes.len(), 1, "the stale decode is not written");
        let Written::Png { png, .. } = &board.writes[0] else {
            panic!("an image was written");
        };
        assert_eq!(pixels_of(png), [3, 0, 0, 255]);
    }

    #[tokio::test]
    async fn the_switch_turning_on_reads_once_where_the_platform_asks_for_access() {
        let harness = Harness::new(Arc::new(Decoder::default()), true);
        let peer = connect(&harness.hub, Some(true)).await;
        harness.ready(&peer).await;
        harness
            .clipboard
            .copy(Item::Text("copied before the switch".into()));
        settle().await;
        assert_eq!(harness.clipboard.reads(), 0);

        assert!(harness.hub.set_enabled(true).enabled);
        harness.watching().await;
        assert_eq!(harness.clipboard.reads(), 1);
        settle().await;
        assert_eq!(harness.clipboard.reads(), 1);
        assert!(peer.remote().recorder.received().is_empty());
        assert!(harness.outbound_is_empty());
    }

    #[tokio::test]
    async fn logs_contain_no_payload() {
        assert!(
            capture_logs(),
            "the test capture must be the process logger to see what is logged"
        );
        const OUTGOING: &str = "outgoing-secret-4f1c7d";
        const INCOMING: &str = "incoming-secret-9b2e5a";
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;

        harness.clipboard.copy(Item::Text(OUTGOING.into()));
        until("the copy to go out", || {
            !peer.remote().recorder.received().is_empty()
        })
        .await;
        peer.remote()
            .publish(ClipboardKind::Text, INCOMING.as_bytes().to_vec());
        until("the text to be written", || harness.clipboard.writes() == 1).await;
        let image = one_pixel_png([0x5A, 0xA5, 0x3C, 255]);
        peer.remote().publish(ClipboardKind::Png, image.clone());
        until("the image to be written", || {
            harness.clipboard.writes() == 2
        })
        .await;
        drop(peer);
        drop(harness);

        let lines = lock(&CAPTURED).clone();
        let logged = |fragment: &str| lines.iter().any(|line| line.contains(fragment));
        assert!(logged("clipboard: local text of 22 bytes offered"));
        assert!(logged("clipboard: text of 22 bytes from"));
        assert!(logged(&format!(
            "clipboard: image of {} bytes from",
            image.len()
        )));
        let signature = format!("{:?}", &image[..8]);
        for secret in [OUTGOING, INCOMING, &signature[1..signature.len() - 1]] {
            assert!(!logged(secret), "a log line carries {secret:?}");
        }
    }

    #[tokio::test]
    async fn shutdown_stops_every_thread_while_watching() {
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;
        assert!(!harness.hub.is_stopped());

        harness.hub.request_shutdown();
        until("every clipboard thread to stop", || {
            harness.hub.is_stopped()
        })
        .await;
        assert_eq!(harness.clipboard.board().closed, 1);
        assert!(harness.outbound_is_empty());
        harness.hub.set_enabled(true);
        assert!(
            harness.hub.shared.lock().watch.is_none(),
            "nothing restarts"
        );
    }

    #[tokio::test]
    async fn a_started_hub_is_off_and_idle_until_attached_and_inert_after_shutdown() {
        let views = Arc::new(Views::default());
        let hub = ClipboardHub::start(Arc::clone(&views), None);
        let view = hub.view();
        assert!(!view.enabled && view.peers.is_empty() && view.last.is_none());
        assert_eq!(view.access, Access::Unknown);
        assert!(
            !hub.is_stopped(),
            "the link and image threads wait for work"
        );

        hub.request_shutdown();
        until("every clipboard thread to stop", || hub.is_stopped()).await;
        let pair = connection_pair().await;
        let attachment = hub.attach(pair.remote_id, pair.hub_side.clone(), epoch());
        assert!(attachment.id.is_none() && attachment.link.is_none());
        assert!(hub.view().peers.is_empty());
        assert!(
            tokio::time::timeout(QUIET, pair.remote_side.open_uni())
                .await
                .is_err(),
            "an inert attachment grants the peer no stream credit"
        );
    }

    #[tokio::test]
    async fn views_carry_kinds_and_sizes_but_never_content() {
        const SECRET: &str = "view-secret-7e21";
        let harness = Harness::on();
        let peer = connect(&harness.hub, Some(true)).await;
        harness.watching().await;
        harness.ready(&peer).await;
        peer.remote()
            .publish(ClipboardKind::Text, SECRET.as_bytes().to_vec());
        until("the text to be written", || harness.clipboard.writes() == 1).await;
        until("the view to show the transfer", || {
            harness.hub.view().last.is_some()
        })
        .await;

        let views = lock(&harness.views.0).clone();
        assert!(
            views
                .iter()
                .any(|view| view.last.as_ref().is_some_and(|last| {
                    last.direction == Direction::Received && last.bytes == SECRET.len() as u64
                }))
        );
        for view in &views {
            let json = serde_json::to_string(view).expect("the view serializes");
            assert!(!json.contains(SECRET), "{json}");
        }
    }
}
