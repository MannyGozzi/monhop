//! Clipboard transfers on their own unidirectional streams, never the input control stream.
//!
//! [`attach`] is the only place a Share connection grants unidirectional stream credit, and
//! dropping the returned [`ClipboardLink`] withdraws it. Each transfer is one stream at a priority
//! below the control stream: the fixed header, the payload, then FIN. Every peer byte is untrusted;
//! a broken rule stops that one stream, never the connection, and nothing here touches input.

use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    future::{Future, pending, poll_fn},
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};

use bytes::Bytes;
use monhop_protocol::{
    SessionEpoch,
    clipboard::{
        CLIPBOARD_HEADER_LEN, ClipboardHeader, ClipboardHeaderError, ClipboardKind,
        ClipboardPngError, ClipboardTextError, MAX_CLIPBOARD_PNG, decode_header, encode_header,
        png_dimensions, validate_text,
    },
};
use quinn::{ReadError, ReadExactError, RecvStream, SendStream, VarInt, WriteError};
use tokio::{sync::watch, task::JoinHandle, time::Instant};

use crate::crypto::{CLIPBOARD_UNI_STREAMS, CertificateFingerprint};

/// Below the control stream's default 0, so buffered input always leaves first.
const CLIPBOARD_PRIORITY: i32 = -1;
const MAX_STREAMS_PER_WINDOW: usize = 16;
const MAX_VIOLATIONS: u8 = 4;

// Application codes on RESET_STREAM and STOP_SENDING. Informative only: nothing acts on a code.
const CODE_CANCELLED: u32 = 0;
const CODE_REFUSED: u32 = 1;
const CODE_SWITCHED_OFF: u32 = 2;
const CODE_STALE: u32 = 3;
const CODE_TIMED_OUT: u32 = 4;
const CODE_DISABLED: u32 = 5;

#[derive(Clone, Copy, Debug)]
struct Limits {
    /// From accept to a complete header; a State stream gets the same again for its byte and FIN.
    header: Duration,
    text_body: Duration,
    png_body: Duration,
    inbound_window: Duration,
    /// Longer than `inbound_window`, so path jitter can never make a paced sender look too fast.
    outbound_window: Duration,
}

impl Limits {
    const PRODUCTION: Self = Self {
        header: Duration::from_secs(2),
        text_body: Duration::from_secs(10),
        png_body: Duration::from_secs(120),
        inbound_window: Duration::from_secs(10),
        outbound_window: Duration::from_secs(11),
    };

    const fn body(&self, kind: ClipboardKind) -> Duration {
        match kind {
            ClipboardKind::State => self.header,
            ClipboardKind::Text => self.text_body,
            ClipboardKind::Png => self.png_body,
        }
    }
}

/// What one Share connection's link needs; the hub builds one per attached peer.
pub struct ClipboardLinkConfig {
    pub peer: CertificateFingerprint,
    /// The connection's negotiated initial epoch; every header in both directions carries it.
    pub epoch: SessionEpoch,
    /// This computer's clipboard-sharing switch.
    pub local_enabled: watch::Receiver<bool>,
    /// The newest locally copied item. Each change is one transfer and the latest wins; `None`
    /// withdraws a transfer still in flight. Whatever it holds when the link attaches stays local.
    pub outbound: watch::Receiver<Option<Arc<OutboundClipboard>>>,
    pub sink: Arc<dyn ClipboardSink>,
}

/// Called on the clipboard runtime. Every method must return promptly: the link waits on it.
pub trait ClipboardSink: Send + Sync {
    /// A complete transfer. Text passed `validate_text`; a PNG passed only the IHDR pre-check and
    /// must still be fully decoded before anything else parses it.
    fn received(&self, item: InboundClipboard);
    /// The peer's switch, reported when it changes; the first report is the peer's attach State.
    fn peer_state(&self, peer: CertificateFingerprint, enabled: bool);
    fn note(&self, peer: CertificateFingerprint, note: ClipboardNote);
}

/// A locally copied item, validated so a peer never counts it as a violation. Publish it in an
/// `Arc` to fan out: every link shares the same payload bytes.
#[derive(Clone)]
pub struct OutboundClipboard {
    kind: ClipboardKind,
    payload: Bytes,
}

impl OutboundClipboard {
    pub fn new(kind: ClipboardKind, payload: Vec<u8>) -> Result<Self, OutboundClipboardError> {
        match kind {
            ClipboardKind::State => return Err(OutboundClipboardError::NotContent),
            ClipboardKind::Text => validate_text(&payload).map_err(OutboundClipboardError::Text)?,
            ClipboardKind::Png => {
                if payload.len() > MAX_CLIPBOARD_PNG as usize {
                    return Err(OutboundClipboardError::TooLarge);
                }
                png_dimensions(&payload).map_err(OutboundClipboardError::Png)?;
            }
        }
        Ok(Self {
            kind,
            payload: Bytes::from(payload),
        })
    }

    pub const fn kind(&self) -> ClipboardKind {
        self.kind
    }

    /// Validation caps every kind far below `u32::MAX`.
    pub fn byte_len(&self) -> u32 {
        self.payload.len() as u32
    }
}

impl fmt::Debug for OutboundClipboard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutboundClipboard")
            .field("kind", &self.kind)
            .field("len", &self.payload.len())
            .field("payload", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboundClipboardError {
    /// State is the link's own stream kind; only Text and Png are clipboard content.
    NotContent,
    Text(ClipboardTextError),
    /// Above the PNG byte cap.
    TooLarge,
    Png(ClipboardPngError),
}

impl fmt::Display for OutboundClipboardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotContent => "only text and images are clipboard content",
            Self::Text(_) => "clipboard text is not valid for sharing",
            Self::TooLarge => "clipboard image is above the sharing limit",
            Self::Png(_) => "clipboard image is not a valid PNG for sharing",
        })
    }
}

impl Error for OutboundClipboardError {}

pub struct InboundClipboard {
    pub peer: CertificateFingerprint,
    pub kind: ClipboardKind,
    pub bytes: Vec<u8>,
    pub sequence: u64,
    /// When the stream was accepted: a local copy observed after this wins over the transfer.
    pub started_at: std::time::Instant,
}

impl fmt::Debug for InboundClipboard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InboundClipboard")
            .field("peer", &self.peer.short_hex())
            .field("kind", &self.kind)
            .field("len", &self.bytes.len())
            .field("bytes", &"[redacted]")
            .field("sequence", &self.sequence)
            .field("started_at", &self.started_at)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardNote {
    /// The peer's transport acknowledged every byte of an outbound transfer.
    Sent { kind: ClipboardKind, bytes: u32 },
    /// An outbound transfer ended before the peer had all of it.
    NotSent {
        kind: ClipboardKind,
        bytes: u32,
        reason: NotSent,
    },
    /// An inbound stream ended without reaching the sink; `kind` is `None` before a valid header.
    Refused {
        kind: Option<ClipboardKind>,
        reason: Refusal,
    },
    /// Violations reached the limit: the peer can open no more clipboard streams on this
    /// connection and nothing more is sent to it. Reported once.
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotSent {
    /// A newer local item replaced it, or the item was withdrawn.
    Superseded,
    /// This computer's switch or the peer's is off, or the peer's reports are no longer read.
    SwitchedOff,
    PeerStopped,
    LinkFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The stream broke the wire rules; this is what counts toward disabling the link.
    Violation(Violation),
    SwitchedOff,
    /// Its sequence was not newer than one already taken.
    Stale,
    /// A newer transfer replaced it mid-read.
    Superseded,
    /// The body missed its deadline, which a slow path can cause, so it is not a violation.
    TimedOut,
    PeerReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Violation {
    Header(ClipboardHeaderError),
    /// The fixed header is written right after open and never waits on flow control.
    HeaderTimedOut,
    /// FIN before the declared length.
    Truncated,
    /// More bytes where FIN belongs.
    TrailingBytes,
    /// A State byte other than 0 or 1.
    InvalidState,
    InvalidText(ClipboardTextError),
    InvalidPng(ClipboardPngError),
    RateLimited,
}

/// Owns one connection's clipboard tasks. Dropping it aborts them and withdraws the stream
/// credit; credit the peer already holds cannot be revoked, only never renewed.
#[must_use = "dropping the link stops clipboard transfers on its connection"]
pub struct ClipboardLink {
    connection: quinn::Connection,
    sender: JoinHandle<()>,
    receiver: JoinHandle<()>,
}

impl Drop for ClipboardLink {
    fn drop(&mut self) {
        self.connection
            .set_max_concurrent_uni_streams(VarInt::from_u32(0));
        self.sender.abort();
        self.receiver.abort();
    }
}

/// Grants the peer two concurrent unidirectional streams and starts the link on `handle`, whose
/// runtime must have the time driver. Call it only on a negotiated Share connection: every other
/// connection keeps zero credit, so a stream there closes it with STREAM_LIMIT_ERROR.
pub fn attach(
    connection: quinn::Connection,
    config: ClipboardLinkConfig,
    handle: &tokio::runtime::Handle,
) -> ClipboardLink {
    attach_with(connection, config, handle, Limits::PRODUCTION)
}

fn attach_with(
    connection: quinn::Connection,
    config: ClipboardLinkConfig,
    handle: &tokio::runtime::Handle,
    limits: Limits,
) -> ClipboardLink {
    let ClipboardLinkConfig {
        peer,
        epoch,
        local_enabled,
        outbound,
        sink,
    } = config;
    let (peer_enabled, peer_watch) = watch::channel(false);
    connection.set_max_concurrent_uni_streams(VarInt::from_u32(CLIPBOARD_UNI_STREAMS));
    let receiver = handle.spawn(
        Receiver {
            connection: connection.clone(),
            epoch: epoch.get(),
            peer,
            sink: Arc::clone(&sink),
            limits,
            local: local_enabled.clone(),
            local_on: false,
            local_live: true,
            peer_enabled,
            reported_peer: None,
            accepted: VecDeque::new(),
            streams: Vec::new(),
            content: None,
            last_state: 0,
            last_content: 0,
            violations: 0,
            disabled: false,
        }
        .run(),
    );
    let sender = handle.spawn(
        Sender {
            connection: connection.clone(),
            epoch: epoch.get(),
            peer,
            sink,
            limits,
            sequence: 0,
            opened: VecDeque::new(),
            content: None,
        }
        .run(local_enabled, outbound, peer_watch),
    );
    ClipboardLink {
        connection,
        sender,
        receiver,
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Resolves with `future`'s output; never resolves for `None`.
async fn next<T>(future: Option<&mut BoxFuture<T>>) -> T {
    match future {
        Some(future) => future.as_mut().await,
        None => pending().await,
    }
}

/// Resolves with the first finished future and its index; never resolves while `futures` is empty.
fn next_ready<T>(futures: &mut [BoxFuture<T>]) -> impl Future<Output = (usize, T)> + '_ {
    poll_fn(move |context| {
        for (index, future) in futures.iter_mut().enumerate() {
            if let Poll::Ready(output) = future.as_mut().poll(context) {
                return Poll::Ready((index, output));
            }
        }
        Poll::Pending
    })
}

/// Quinn finishes a dropped send stream, which would hand the peer a truncated payload with a
/// FIN. This resets instead unless the transfer completed.
struct Outgoing {
    stream: SendStream,
    armed: bool,
}

impl Outgoing {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Outgoing {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.stream.reset(VarInt::from_u32(CODE_CANCELLED));
        }
    }
}

struct Transfer {
    item: Arc<OutboundClipboard>,
    future: BoxFuture<Result<(), NotSent>>,
}

struct Sender {
    connection: quinn::Connection,
    epoch: u64,
    peer: CertificateFingerprint,
    sink: Arc<dyn ClipboardSink>,
    limits: Limits,
    sequence: u64,
    opened: VecDeque<Instant>,
    content: Option<Transfer>,
}

impl Sender {
    /// Opens one stream at a time, so each open is timed exactly for the pacing budget. At most
    /// one State and one content stream are in flight; a newer content item resets the older.
    async fn run(
        mut self,
        mut local: watch::Receiver<bool>,
        mut outbound: watch::Receiver<Option<Arc<OutboundClipboard>>>,
        mut peer: watch::Receiver<bool>,
    ) {
        outbound.mark_unchanged();
        let mut local_on = *local.borrow_and_update();
        let mut peer_on = *peer.borrow_and_update();
        let (mut local_live, mut outbound_live, mut peer_live) = (true, true, true);
        let mut announced = None;
        let mut want_state = Some(local_on);
        let mut want_content: Option<Arc<OutboundClipboard>> = None;
        let mut opening: Option<BoxFuture<Option<Outgoing>>> = None;
        let mut state: Option<BoxFuture<()>> = None;
        loop {
            let wanted = want_state.is_some() || want_content.is_some();
            if !wanted {
                opening = None;
            }
            let mut wake = None;
            if wanted && opening.is_none() {
                match self.open_budget() {
                    None => opening = Some(Box::pin(open(self.connection.clone()))),
                    Some(at) => wake = Some(at),
                }
            }
            tokio::select! {
                biased;
                _ = self.connection.closed() => break,
                changed = local.changed(), if local_live => {
                    local_live = changed.is_ok();
                    local_on = local_live && *local.borrow_and_update();
                    want_state = (announced != Some(local_on)).then_some(local_on);
                    if !local_on {
                        want_content = None;
                        self.cancel(NotSent::SwitchedOff);
                    }
                }
                changed = peer.changed(), if peer_live => {
                    peer_live = changed.is_ok();
                    peer_on = peer_live && *peer.borrow_and_update();
                    if !peer_on {
                        want_content = None;
                        self.cancel(NotSent::SwitchedOff);
                    }
                }
                changed = outbound.changed(), if outbound_live => {
                    outbound_live = changed.is_ok();
                    let item = if outbound_live {
                        outbound.borrow_and_update().clone()
                    } else {
                        None
                    };
                    match item {
                        Some(item) if local_on && peer_on => {
                            let current = self.content.as_ref().map(|transfer| &transfer.item);
                            if [current, want_content.as_ref()]
                                .into_iter()
                                .flatten()
                                .any(|queued| Arc::ptr_eq(queued, &item))
                            {
                                continue;
                            }
                            self.cancel(NotSent::Superseded);
                            want_content = Some(item);
                        }
                        Some(_) => {}
                        None => {
                            want_content = None;
                            self.cancel(NotSent::Superseded);
                        }
                    }
                }
                stream = next(opening.as_mut()) => {
                    opening = None;
                    self.opened.push_back(Instant::now());
                    let Some(stream) = stream else { continue };
                    if let Some(enabled) = want_state.take() {
                        if let Some(header) = self.header(ClipboardKind::State, 1) {
                            announced = Some(enabled);
                            state = Some(Box::pin(send_state(stream, header, enabled)));
                        }
                    } else if let Some(item) = want_content.take()
                        && let Some(header) = self.header(item.kind, item.byte_len())
                    {
                        self.cancel(NotSent::Superseded);
                        let future = Box::pin(send_content(stream, header, item.payload.clone()));
                        self.content = Some(Transfer { item, future });
                    }
                }
                () = next(state.as_mut()) => state = None,
                outcome = next(self.content.as_mut().map(|transfer| &mut transfer.future)) => {
                    if let Some(transfer) = self.content.take() {
                        self.report(&transfer.item, outcome);
                    }
                }
                () = tokio::time::sleep_until(wake.unwrap_or_else(Instant::now)),
                    if wake.is_some() => {}
            }
        }
        self.cancel(NotSent::LinkFailed);
    }

    /// `None` when another stream may open now, else when the oldest open leaves the window.
    fn open_budget(&mut self) -> Option<Instant> {
        let now = Instant::now();
        let window = self.limits.outbound_window;
        while self
            .opened
            .front()
            .is_some_and(|&at| now.duration_since(at) >= window)
        {
            self.opened.pop_front();
        }
        (self.opened.len() >= MAX_STREAMS_PER_WINDOW)
            .then(|| self.opened.front().map_or(now, |&at| at + window))
    }

    fn header(&mut self, kind: ClipboardKind, payload_len: u32) -> Option<Bytes> {
        self.sequence = self.sequence.checked_add(1)?;
        let header = encode_header(&ClipboardHeader {
            kind,
            epoch: self.epoch,
            sequence: self.sequence,
            payload_len,
        });
        Some(Bytes::copy_from_slice(&header))
    }

    /// Dropping the transfer resets its stream.
    fn cancel(&mut self, reason: NotSent) {
        if let Some(transfer) = self.content.take() {
            self.report(&transfer.item, Err(reason));
        }
    }

    fn report(&self, item: &OutboundClipboard, outcome: Result<(), NotSent>) {
        let (kind, bytes, peer) = (item.kind, item.byte_len(), self.peer.short_hex());
        let note = match outcome {
            Ok(()) => {
                log::debug!("clipboard {kind:?} of {bytes} bytes sent to {peer}");
                ClipboardNote::Sent { kind, bytes }
            }
            Err(reason) => {
                log::debug!("clipboard {kind:?} of {bytes} bytes to {peer} not sent: {reason:?}");
                ClipboardNote::NotSent {
                    kind,
                    bytes,
                    reason,
                }
            }
        };
        self.sink.note(self.peer, note);
    }
}

async fn open(connection: quinn::Connection) -> Option<Outgoing> {
    let stream = Outgoing {
        stream: connection.open_uni().await.ok()?,
        armed: true,
    };
    stream.stream.set_priority(CLIPBOARD_PRIORITY).ok()?;
    Some(stream)
}

async fn send_state(mut stream: Outgoing, header: Bytes, enabled: bool) {
    let mut chunks = [header, Bytes::copy_from_slice(&[u8::from(enabled)])];
    if stream.stream.write_all_chunks(&mut chunks).await.is_ok() && stream.stream.finish().is_ok() {
        stream.disarm();
    }
}

async fn send_content(mut stream: Outgoing, header: Bytes, payload: Bytes) -> Result<(), NotSent> {
    let mut chunks = [header, payload];
    stream
        .stream
        .write_all_chunks(&mut chunks)
        .await
        .map_err(|error| match error {
            WriteError::Stopped(_) => NotSent::PeerStopped,
            _ => NotSent::LinkFailed,
        })?;
    stream.stream.finish().map_err(|_| NotSent::LinkFailed)?;
    match stream.stream.stopped().await {
        Ok(None) => {
            stream.disarm();
            Ok(())
        }
        Ok(Some(_)) => Err(NotSent::PeerStopped),
        Err(_) => Err(NotSent::LinkFailed),
    }
}

#[derive(Clone, Copy, Debug)]
enum HeaderFailure {
    TimedOut,
    Truncated,
    Reset,
    Lost,
}

#[derive(Clone, Copy, Debug)]
enum BodyFailure {
    Violation(Violation),
    TimedOut,
    Reset,
    Lost,
}

enum Event {
    Header {
        stream: RecvStream,
        started_at: std::time::Instant,
        header: Result<[u8; CLIPBOARD_HEADER_LEN], HeaderFailure>,
    },
    State {
        sequence: u64,
        body: Result<Vec<u8>, BodyFailure>,
    },
}

struct ContentRead {
    kind: ClipboardKind,
    sequence: u64,
    started_at: std::time::Instant,
    future: BoxFuture<Result<Vec<u8>, BodyFailure>>,
}

struct Receiver {
    connection: quinn::Connection,
    epoch: u64,
    peer: CertificateFingerprint,
    sink: Arc<dyn ClipboardSink>,
    limits: Limits,
    local: watch::Receiver<bool>,
    local_on: bool,
    local_live: bool,
    peer_enabled: watch::Sender<bool>,
    reported_peer: Option<bool>,
    accepted: VecDeque<Instant>,
    /// Streams reading a header and State streams reading their byte; quinn's credit bounds them.
    streams: Vec<BoxFuture<Event>>,
    /// The one inbound content buffer; a newer admitted transfer drops it, stopping its stream.
    content: Option<ContentRead>,
    last_state: u64,
    last_content: u64,
    violations: u8,
    disabled: bool,
}

impl Receiver {
    async fn run(mut self) {
        self.local_on = *self.local.borrow_and_update();
        loop {
            tokio::select! {
                biased;
                incoming = self.connection.accept_uni() => match incoming {
                    Ok(stream) => self.admit(stream),
                    Err(_) => break,
                },
                changed = self.local.changed(), if self.local_live => {
                    self.local_live = changed.is_ok();
                    self.local_on = self.local_live && *self.local.borrow_and_update();
                    if !self.local_on {
                        self.drop_content(Refusal::SwitchedOff);
                    }
                }
                (index, event) = next_ready(&mut self.streams) => {
                    drop(self.streams.swap_remove(index));
                    match event {
                        Event::Header { stream, started_at, header } => {
                            self.on_header(stream, started_at, header);
                        }
                        Event::State { sequence, body } => self.on_state(sequence, body),
                    }
                }
                body = next(self.content.as_mut().map(|read| &mut read.future)) => {
                    if let Some(read) = self.content.take() {
                        self.on_content(read, body);
                    }
                }
            }
        }
    }

    fn admit(&mut self, mut stream: RecvStream) {
        if self.disabled {
            stop(&mut stream, CODE_DISABLED);
            return;
        }
        let now = Instant::now();
        let window = self.limits.inbound_window;
        while self
            .accepted
            .front()
            .is_some_and(|&at| now.duration_since(at) >= window)
        {
            self.accepted.pop_front();
        }
        if self.accepted.len() >= MAX_STREAMS_PER_WINDOW {
            stop(&mut stream, CODE_REFUSED);
            self.violation(None, Violation::RateLimited);
            return;
        }
        self.accepted.push_back(now);
        let started_at = std::time::Instant::now();
        let limit = self.limits.header;
        self.streams.push(Box::pin(async move {
            let header = read_header(&mut stream, limit).await;
            Event::Header {
                stream,
                started_at,
                header,
            }
        }));
    }

    fn on_header(
        &mut self,
        mut stream: RecvStream,
        started_at: std::time::Instant,
        header: Result<[u8; CLIPBOARD_HEADER_LEN], HeaderFailure>,
    ) {
        let header = match header {
            Ok(bytes) => decode_header(&bytes, self.epoch).map_err(Violation::Header),
            Err(HeaderFailure::TimedOut) => Err(Violation::HeaderTimedOut),
            Err(HeaderFailure::Truncated) => Err(Violation::Truncated),
            Err(HeaderFailure::Reset) => return self.refused(None, Refusal::PeerReset),
            Err(HeaderFailure::Lost) => return,
        };
        let header = match header {
            Ok(header) => header,
            Err(violation) => {
                stop(&mut stream, CODE_REFUSED);
                return self.violation(None, violation);
            }
        };
        let (kind, sequence) = (header.kind, header.sequence);
        if kind == ClipboardKind::State {
            if sequence <= self.last_state {
                stop(&mut stream, CODE_STALE);
                return self.refused(Some(kind), Refusal::Stale);
            }
            let deadline = Instant::now() + self.limits.body(kind);
            self.streams.push(Box::pin(async move {
                let body = read_body(stream, header.payload_len, deadline, kind).await;
                Event::State { sequence, body }
            }));
            return;
        }
        if !self.local_on {
            stop(&mut stream, CODE_SWITCHED_OFF);
            return self.refused(Some(kind), Refusal::SwitchedOff);
        }
        if sequence <= self.last_content {
            stop(&mut stream, CODE_STALE);
            return self.refused(Some(kind), Refusal::Stale);
        }
        self.last_content = sequence;
        self.drop_content(Refusal::Superseded);
        let deadline = Instant::now() + self.limits.body(kind);
        self.content = Some(ContentRead {
            kind,
            sequence,
            started_at,
            future: Box::pin(read_body(stream, header.payload_len, deadline, kind)),
        });
    }

    fn on_state(&mut self, sequence: u64, body: Result<Vec<u8>, BodyFailure>) {
        let kind = Some(ClipboardKind::State);
        let enabled = match body.as_deref() {
            Ok([0]) => false,
            Ok([1]) => true,
            Ok(_) => return self.violation(kind, Violation::InvalidState),
            Err(&failure) => return self.body_failed(kind, failure),
        };
        if sequence <= self.last_state {
            return self.refused(kind, Refusal::Stale);
        }
        self.last_state = sequence;
        self.peer_enabled.send_replace(enabled);
        if self.reported_peer != Some(enabled) {
            self.reported_peer = Some(enabled);
            log::debug!(
                "clipboard switch of {} is {}",
                self.peer.short_hex(),
                if enabled { "on" } else { "off" }
            );
            self.sink.peer_state(self.peer, enabled);
        }
    }

    fn on_content(&mut self, read: ContentRead, body: Result<Vec<u8>, BodyFailure>) {
        let kind = Some(read.kind);
        let bytes = match body {
            Ok(bytes) => bytes,
            Err(failure) => return self.body_failed(kind, failure),
        };
        if read.kind == ClipboardKind::Text
            && let Err(error) = validate_text(&bytes)
        {
            return self.violation(kind, Violation::InvalidText(error));
        }
        if !self.local_on {
            return self.refused(kind, Refusal::SwitchedOff);
        }
        log::debug!(
            "clipboard {:?} of {} bytes received from {}",
            read.kind,
            bytes.len(),
            self.peer.short_hex()
        );
        self.sink.received(InboundClipboard {
            peer: self.peer,
            kind: read.kind,
            bytes,
            sequence: read.sequence,
            started_at: read.started_at,
        });
    }

    fn body_failed(&mut self, kind: Option<ClipboardKind>, failure: BodyFailure) {
        match failure {
            BodyFailure::Violation(violation) => self.violation(kind, violation),
            BodyFailure::TimedOut => self.refused(kind, Refusal::TimedOut),
            BodyFailure::Reset => self.refused(kind, Refusal::PeerReset),
            BodyFailure::Lost => {}
        }
    }

    /// Dropping the read stops its stream.
    fn drop_content(&mut self, reason: Refusal) {
        if let Some(read) = self.content.take() {
            self.refused(Some(read.kind), reason);
        }
    }

    fn refused(&self, kind: Option<ClipboardKind>, reason: Refusal) {
        log::debug!(
            "clipboard {kind:?} from {} refused: {reason:?}",
            self.peer.short_hex()
        );
        self.sink
            .note(self.peer, ClipboardNote::Refused { kind, reason });
    }

    fn violation(&mut self, kind: Option<ClipboardKind>, violation: Violation) {
        let peer = self.peer.short_hex();
        log::warn!("clipboard {kind:?} from {peer} broke the stream rules: {violation:?}");
        self.refused(kind, Refusal::Violation(violation));
        self.violations = self.violations.saturating_add(1);
        if self.disabled || self.violations < MAX_VIOLATIONS {
            return;
        }
        self.disabled = true;
        self.connection
            .set_max_concurrent_uni_streams(VarInt::from_u32(0));
        self.streams.clear();
        self.content = None;
        // Its switch reports are no longer read, so nothing more goes to it.
        self.peer_enabled.send_replace(false);
        log::warn!(
            "clipboard from {peer} disabled on this connection after {MAX_VIOLATIONS} violations"
        );
        self.sink.note(self.peer, ClipboardNote::Disabled);
    }
}

fn stop(stream: &mut RecvStream, code: u32) {
    let _ = stream.stop(VarInt::from_u32(code));
}

async fn read_header(
    stream: &mut RecvStream,
    limit: Duration,
) -> Result<[u8; CLIPBOARD_HEADER_LEN], HeaderFailure> {
    let mut header = [0; CLIPBOARD_HEADER_LEN];
    match tokio::time::timeout(limit, stream.read_exact(&mut header)).await {
        Ok(Ok(())) => Ok(header),
        Ok(Err(ReadExactError::FinishedEarly(_))) => Err(HeaderFailure::Truncated),
        Ok(Err(ReadExactError::ReadError(ReadError::Reset(_)))) => Err(HeaderFailure::Reset),
        Ok(Err(ReadExactError::ReadError(_))) => Err(HeaderFailure::Lost),
        Err(_) => Err(HeaderFailure::TimedOut),
    }
}

/// Reads exactly `len` bytes then FIN before `deadline`, stopping the stream on any failure.
async fn read_body(
    mut stream: RecvStream,
    len: u32,
    deadline: Instant,
    kind: ClipboardKind,
) -> Result<Vec<u8>, BodyFailure> {
    let mut body = Vec::new();
    let failure = match tokio::time::timeout_at(
        deadline,
        read_exactly(&mut stream, &mut body, len as usize, kind),
    )
    .await
    {
        Ok(Ok(())) => return Ok(body),
        Ok(Err(failure)) => failure,
        Err(_) => BodyFailure::TimedOut,
    };
    match failure {
        BodyFailure::Violation(_) => stop(&mut stream, CODE_REFUSED),
        BodyFailure::TimedOut => stop(&mut stream, CODE_TIMED_OUT),
        BodyFailure::Reset | BodyFailure::Lost => {}
    }
    Err(failure)
}

async fn read_exactly(
    stream: &mut RecvStream,
    body: &mut Vec<u8>,
    len: usize,
    kind: ClipboardKind,
) -> Result<(), BodyFailure> {
    let mut ihdr_checked = kind != ClipboardKind::Png;
    while body.len() < len {
        let chunk = match stream.read_chunk(len - body.len(), true).await {
            Ok(Some(chunk)) => chunk.bytes,
            Ok(None) => return Err(BodyFailure::Violation(Violation::Truncated)),
            Err(error) => return Err(read_failure(error)),
        };
        grow_within(body, chunk.len(), len);
        body.extend_from_slice(&chunk);
        if !ihdr_checked {
            match png_dimensions(body) {
                Err(ClipboardPngError::Truncated) => {}
                Err(error) => return Err(BodyFailure::Violation(Violation::InvalidPng(error))),
                Ok(_) => ihdr_checked = true,
            }
        }
    }
    match stream.read_chunk(1, true).await {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(BodyFailure::Violation(Violation::TrailingBytes)),
        Err(error) => Err(read_failure(error)),
    }
}

fn read_failure(error: ReadError) -> BodyFailure {
    match error {
        ReadError::Reset(_) => BodyFailure::Reset,
        _ => BodyFailure::Lost,
    }
}

/// Doubles capacity as bytes arrive, like `Vec`, but never reserves past the declared length.
fn grow_within(buffer: &mut Vec<u8>, additional: usize, limit: usize) {
    let needed = buffer.len() + additional;
    if needed > buffer.capacity() {
        let target = needed.max(buffer.capacity().saturating_mul(2)).min(limit);
        buffer.reserve_exact(target - buffer.len());
    }
}

/// Deadlines and pacing on real loopback connections with scaled-down limits: tokio's paused
/// clock needs its `test-util` feature, which this crate does not enable.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer};
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::sync::mpsc;

    const EPOCH: u64 = 7;
    const WAIT: Duration = Duration::from_secs(10);

    enum Seen {
        Received(InboundClipboard),
        PeerState(bool),
        Note(ClipboardNote),
    }

    struct Channel(mpsc::UnboundedSender<Seen>);

    impl ClipboardSink for Channel {
        fn received(&self, item: InboundClipboard) {
            let _ = self.0.send(Seen::Received(item));
        }

        fn peer_state(&self, _: CertificateFingerprint, enabled: bool) {
            let _ = self.0.send(Seen::PeerState(enabled));
        }

        fn note(&self, _: CertificateFingerprint, note: ClipboardNote) {
            let _ = self.0.send(Seen::Note(note));
        }
    }

    struct Attached {
        seen: mpsc::UnboundedReceiver<Seen>,
        outbound: watch::Sender<Option<Arc<OutboundClipboard>>>,
        _switch: watch::Sender<bool>,
        _link: ClipboardLink,
    }

    impl Attached {
        async fn next<T>(&mut self, mut pick: impl FnMut(Seen) -> Option<T>) -> T {
            tokio::time::timeout(WAIT, async {
                loop {
                    let seen = self.seen.recv().await.expect("the link is still attached");
                    if let Some(found) = pick(seen) {
                        return found;
                    }
                }
            })
            .await
            .expect("the expected clipboard event arrived")
        }

        async fn note(&mut self) -> ClipboardNote {
            self.next(|seen| match seen {
                Seen::Note(note) => Some(note),
                _ => None,
            })
            .await
        }
    }

    fn attach_scaled(
        connection: &quinn::Connection,
        peer: CertificateFingerprint,
        limits: Limits,
    ) -> Attached {
        let (sink, seen) = mpsc::unbounded_channel();
        let (switch, local_enabled) = watch::channel(true);
        let (outbound, outbound_watch) = watch::channel(None);
        let link = attach_with(
            connection.clone(),
            ClipboardLinkConfig {
                peer,
                epoch: SessionEpoch::new(EPOCH).expect("nonzero epoch"),
                local_enabled,
                outbound: outbound_watch,
                sink: Arc::new(Channel(sink)),
            },
            &tokio::runtime::Handle::current(),
            limits,
        );
        Attached {
            seen,
            outbound,
            _switch: switch,
            _link: link,
        }
    }

    struct Pair {
        client: quinn::Connection,
        server: quinn::Connection,
        client_id: CertificateFingerprint,
        server_id: CertificateFingerprint,
        _endpoints: [quinn::Endpoint; 2],
    }

    async fn connection_pair() -> Pair {
        let client_identity = DeviceIdentity::generate().expect("client identity");
        let server_identity = DeviceIdentity::generate().expect("server identity");
        let pin = |identity: &DeviceIdentity| {
            VerifiedPeer::from_certificate_der(
                identity.certificate_der(),
                &identity.fingerprint().full_hex(),
            )
            .expect("generated identity has a full matching pin")
        };
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let listener = quinn::Endpoint::server(
            SecureQuicConfig::server(&server_identity, &pin(&client_identity))
                .expect("server TLS configuration"),
            loopback,
        )
        .expect("loopback server endpoint");
        let mut dialer = quinn::Endpoint::client(loopback).expect("loopback client endpoint");
        dialer.set_default_client_config(
            SecureQuicConfig::client(&client_identity, &pin(&server_identity))
                .expect("client TLS configuration"),
        );
        let connecting = dialer
            .connect(
                listener.local_addr().expect("server address"),
                LOCAL_TLS_SERVER_NAME,
            )
            .expect("loopback connection");
        let (client, server) = tokio::join!(
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
            client,
            server,
            client_id: client_identity.fingerprint(),
            server_id: server_identity.fingerprint(),
            _endpoints: [dialer, listener],
        }
    }

    fn header(kind: ClipboardKind, sequence: u64, payload_len: u32) -> Vec<u8> {
        encode_header(&ClipboardHeader {
            kind,
            epoch: EPOCH,
            sequence,
            payload_len,
        })
        .to_vec()
    }

    fn png(len: usize) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&16_u32.to_be_bytes());
        bytes.extend_from_slice(&16_u32.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        bytes.resize(len, 0x5A);
        bytes
    }

    async fn open_with(connection: &quinn::Connection, bytes: &[u8]) -> SendStream {
        let mut stream = tokio::time::timeout(WAIT, connection.open_uni())
            .await
            .expect("the link grants credit")
            .expect("open a stream");
        stream.write_all(bytes).await.expect("write the stream");
        stream
    }

    #[tokio::test]
    async fn slow_headers_and_bodies_miss_their_deadlines() {
        let limits = Limits {
            header: Duration::from_millis(150),
            text_body: Duration::from_millis(300),
            png_body: Duration::from_millis(900),
            ..Limits::PRODUCTION
        };
        let pair = connection_pair().await;
        let mut link = attach_scaled(&pair.server, pair.client_id, limits);

        let started = Instant::now();
        let partial = open_with(&pair.client, &header(ClipboardKind::Text, 1, 5)[..10]).await;
        assert_eq!(
            link.note().await,
            ClipboardNote::Refused {
                kind: None,
                reason: Refusal::Violation(Violation::HeaderTimedOut),
            }
        );
        assert!(started.elapsed() >= limits.header);
        assert!(matches!(partial.stopped().await, Ok(Some(_))));
        // A stopped stream keeps its credit until the sender resets it, as dropping it does.
        drop(partial);

        let started = Instant::now();
        let slow_text = open_with(
            &pair.client,
            &[header(ClipboardKind::Text, 2, 10), b"12345".to_vec()].concat(),
        )
        .await;
        assert_eq!(
            link.note().await,
            ClipboardNote::Refused {
                kind: Some(ClipboardKind::Text),
                reason: Refusal::TimedOut,
            },
            "a slow body is refused but is not a violation"
        );
        assert!(started.elapsed() >= limits.text_body);
        assert!(matches!(slow_text.stopped().await, Ok(Some(_))));
        drop(slow_text);

        let image = png(200);
        let mut slow_png = open_with(
            &pair.client,
            &[header(ClipboardKind::Png, 3, 200), image[..100].to_vec()].concat(),
        )
        .await;
        tokio::time::sleep(limits.text_body + Duration::from_millis(200)).await;
        slow_png
            .write_all(&image[100..])
            .await
            .expect("finish the image");
        slow_png.finish().expect("finish the stream");
        let received = link
            .next(|seen| match seen {
                Seen::Received(item) => Some(item),
                Seen::Note(note) => panic!("the image had its longer deadline: {note:?}"),
                Seen::PeerState(_) => None,
            })
            .await;
        assert_eq!(
            (received.kind, received.bytes),
            (ClipboardKind::Png, image.clone())
        );

        let started = Instant::now();
        let _stalled_png = open_with(
            &pair.client,
            &[header(ClipboardKind::Png, 4, 200), image[..100].to_vec()].concat(),
        )
        .await;
        assert_eq!(
            link.note().await,
            ClipboardNote::Refused {
                kind: Some(ClipboardKind::Png),
                reason: Refusal::TimedOut,
            }
        );
        assert!(started.elapsed() >= limits.png_body);
    }

    #[tokio::test]
    async fn a_paced_sender_never_trips_the_receivers_rate_limit() {
        let limits = Limits {
            inbound_window: Duration::from_millis(300),
            outbound_window: Duration::from_millis(500),
            ..Limits::PRODUCTION
        };
        let pair = connection_pair().await;
        let started = Instant::now();
        let mut sender = attach_scaled(&pair.server, pair.client_id, limits);
        let mut receiver = attach_scaled(&pair.client, pair.server_id, limits);
        let peer_on = |seen| matches!(seen, Seen::PeerState(true)).then_some(());
        sender.next(peer_on).await;
        receiver.next(peer_on).await;

        // The attach State plus 24 transfers is more than one window's worth of streams.
        for index in 0..24 {
            let text = format!("item {index}").into_bytes();
            let item = OutboundClipboard::new(ClipboardKind::Text, text.clone()).expect("text");
            sender.outbound.send_replace(Some(Arc::new(item)));
            let received = receiver
                .next(|seen| match seen {
                    Seen::Received(item) => Some(item),
                    Seen::Note(note) => panic!("the receiver refused a paced stream: {note:?}"),
                    Seen::PeerState(_) => None,
                })
                .await;
            assert_eq!(received.bytes, text);
        }
        assert!(
            started.elapsed() >= limits.outbound_window,
            "the sender waited for its budget instead of opening every stream at once"
        );
    }

    #[test]
    fn a_body_buffer_never_reserves_past_its_declared_length() {
        let declared = 100_000;
        let mut buffer = Vec::new();
        while buffer.len() < declared {
            let chunk = (declared - buffer.len()).min(1_200);
            grow_within(&mut buffer, chunk, declared);
            buffer.resize(buffer.len() + chunk, 0);
            assert!(buffer.capacity() <= declared);
            assert!(buffer.capacity() >= buffer.len());
        }
        assert_eq!(buffer.capacity(), declared);
    }
}
