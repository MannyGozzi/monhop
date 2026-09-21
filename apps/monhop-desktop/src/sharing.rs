//! Explicit sharing actions. Constructing or polling this controller performs no native I/O.

use crate::arrangement_library::{ArrangementLibrary, ArrangementView};
use crate::sharing_preferences::{
    self, DisplayGeometry, PreferenceError, SavedSetupView, SetupFile, SharingPreferences,
    fingerprint_key,
};
use monhop_core::{DisplayId, Edge, EdgeLink, NormalizedSpan, Platform, RevocationSignal};
use monhop_transport::{
    crypto::CertificateFingerprint,
    session::{
        self, LinkClose, NativeCaptureStartupFailure, SessionFailure, SessionStartupFailure,
    },
    session_link::{self, LinkCommand, LinkEvent, LinkPersist, LinkRejectReason},
    session_native,
    session_setup::{self, InspectedPeer, SetupFailure},
    session_source::SourceFailure,
};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Arranging over the link ends after this long without a user action; a standing link has no
/// idle window.
const LINK_IDLE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// The link loop wakes at least this often so a change to the editing flag takes effect.
const LINK_IDLE_POLL: Duration = Duration::from_secs(1);
/// A requested close that the transport never acknowledges is forced with the revocation signal.
const LINK_CLOSE_GRACE: Duration = Duration::from_secs(5);
const NOT_CONNECTED: &str = "Not connected. Input is local.";
pub(crate) const PAUSED: &str = "Paused. Input is local.";
const SWITCHING_COMPUTER: &str = "Switching computers.";
const EDITING_ENDED: &str = "Arranging ended. Reconnecting.";
const LAYOUT_FITS_AGAIN: &str = "The displays fit the saved layout again. Starting sharing.";
const CONNECTING_FOR_SHARING: &str = "Connecting to the other computer for sharing.";
const LAYOUT_APPLIED_SHARING_ON: &str = "Layout applied on both computers. Sharing is on.";
const CONNECTED_ARRANGE: &str =
    "Connected. Choose the input computer, arrange the displays, then apply.";
const CONNECTED_LAYOUT_STALE: &str =
    "Connected. The displays changed since the layout was applied. Arrange them again, then apply.";
const CONNECTED_PEER_ARRANGING: &str = "Connected. The other computer is arranging displays. Sharing resumes when it applies the layout.";
const PEER_ARRANGING: &str = "The other computer is arranging displays. Joining it for arranging.";
const LAYOUT_MISFIT: &str =
    "The displays changed since the layout was applied. Connecting to arrange them.";
/// A session this computer's own displays outgrew, seen from the computer receiving input.
const DESTINATION_DISPLAYS_CHANGED: &str =
    "This computer's displays changed. Connecting to update the layout.";
/// The same end seen from the computer with the keyboard, which is the one that decides next.
const SOURCE_DISPLAYS_CHANGED: &str = "The displays changed. Connecting to update the layout.";
const RECORDS_DISAGREE: &str =
    "The two computers hold different layouts. Connecting to agree on one.";
const UPDATING_LAYOUT: &str = "The displays changed. Updating the layout on both computers.";
const PEER_STARTING_SESSION: &str = "The other computer is starting sharing. Connecting again.";
const PEER_LEFT: &str = "The other computer left. Reconnecting.";
const PEER_PAUSED: &str = "The other computer paused sharing. Reconnecting when it resumes.";
pub(crate) const ARRANGEMENTS_UNREADABLE: &str =
    "The saved arrangements could not be read. Your applied layout is unchanged.";
/// The arrangement library sits beside the setup file, so every writer of one finds the other.
pub(crate) const ARRANGEMENTS_FILE: &str = "arrangements.json";
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
/// How long a finished session's QUIC close may take to reach the other computer before the
/// endpoint is dropped.
const CLOSE_FLUSH: Duration = Duration::from_millis(100);
/// A session that ran at least this long reconnects at once; a shorter one is flapping and
/// waits one reconnect interval so two computers cannot spin on each other.
const STABLE_SESSION: Duration = Duration::from_secs(5);
const IDLE_CLOSED: &str = "Arranging ended after 15 minutes without changes. Reconnecting.";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayView {
    id: String,
    name: String,
    origin: [f64; 2],
    size: [f64; 2],
    scale: f32,
    primary: bool,
    monitor: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharingView {
    pub(crate) phase: &'static str,
    revision: String,
    /// Which computer supplies input. A four-way pairing makes the platform alone ambiguous.
    source_side: Option<&'static str>,
    source_platform: Option<&'static str>,
    local_platform: &'static str,
    pub(crate) peer_platform: Option<&'static str>,
    local_displays: Vec<DisplayView>,
    peer_displays: Vec<DisplayView>,
    pub(crate) message: String,
    pub(crate) busy: bool,
    pub(crate) sharing_active: bool,
    pub(crate) diagnostics: DiagnosticsView,
    link: LinkView,
    sync: SyncView,
    synchronized_layout: Option<LayoutRequest>,
    /// The computer the live link or session is with, lowercase; None once the worker ends.
    pub(crate) peer_fingerprint: Option<String>,
    /// The computer MonHop keeps connected, as saved in the setup file.
    pub(crate) active: Option<String>,
    /// True while the user arranges displays over the link.
    pub(crate) editing: bool,
    /// Set while the displays changed and Home should say so; null otherwise.
    pub(crate) display_notice: Option<DisplayNotice>,
    sharing_role: Option<&'static str>,
    last_failure: String,
    /// The session is up but the other computer stopped answering; input stays local until it does.
    held: bool,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkView {
    attempt: u32,
    since: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncView {
    state: &'static str,
    message: String,
}

impl Default for SyncView {
    fn default() -> Self {
        Self {
            state: "idle",
            message: String::new(),
        }
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticsView {
    pub(crate) sent_events: String,
    pub(crate) received_events: String,
    round_trip_ms: f64,
    active_display: Option<String>,
    active_is_local: Option<bool>,
}
impl Default for DiagnosticsView {
    fn default() -> Self {
        Self {
            sent_events: "0".into(),
            received_events: "0".into(),
            round_trip_ms: 0.0,
            active_display: None,
            active_is_local: None,
        }
    }
}
impl Default for SharingView {
    fn default() -> Self {
        Self {
            phase: "off",
            revision: "0".into(),
            source_side: None,
            source_platform: None,
            local_platform: local_platform_name(),
            peer_platform: None,
            local_displays: Vec::new(),
            peer_displays: Vec::new(),
            message: NOT_CONNECTED.into(),
            busy: false,
            sharing_active: false,
            diagnostics: DiagnosticsView::default(),
            link: LinkView::default(),
            sync: SyncView::default(),
            synchronized_layout: None,
            peer_fingerprint: None,
            active: None,
            editing: false,
            display_notice: None,
            sharing_role: None,
            last_failure: String::new(),
            held: false,
        }
    }
}

/// Why Home shows its display banner. Raised at most once per set of displays, and only while
/// the user has something to decide or something was lost: a display change MonHop answered in
/// full leaves no banner at all, because there is nothing for the user to do about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisplayNotice {
    /// Sharing goes on, but the rebuilt layout had to leave a crossing or a display out. Raised
    /// only at the commit that ends an `Updating`, never for a layout that fit exactly.
    Continued,
    /// Nothing fits; sharing waits until the displays are arranged.
    Waiting,
    /// This computer holds the keyboard and is sending the other one a layout that fits. Clears
    /// at the commit unless that layout left something out.
    Updating,
    /// The other computer holds the keyboard and picks the layout; this one only accepts it.
    /// Always clears at the commit: the deciding computer is the one that reports a loss.
    PeerDeciding,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LinkRequest {
    pub(crate) from_display: String,
    pub(crate) from_edge: String,
    pub(crate) from_span: [f64; 2],
    pub(crate) to_display: String,
    pub(crate) to_edge: String,
    pub(crate) to_span: [f64; 2],
    pub(crate) hysteresis: f64,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LayoutRequest {
    pub(crate) source_display: String,
    pub(crate) links: Vec<LinkRequest>,
    /// Where each display sits on the shared canvas; the links above are derived from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) arrangement: Option<ArrangementRequest>,
}

/// "grouped" keeps each computer's own display layout and moves it as one block;
/// "free" places every display on its own.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArrangementRequest {
    pub(crate) mode: String,
    pub(crate) positions: Vec<DisplayPosition>,
    /// Displays marked not in use, which the picture leaves out. Always written, so a layout
    /// that shows every display restores that way.
    #[serde(default)]
    pub(crate) hidden: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisplayPosition {
    pub(crate) display: String,
    pub(crate) x: f64,
    pub(crate) y: f64,
}

pub(crate) const MAX_ARRANGED_DISPLAYS: usize = 32;
pub(crate) const MAX_ARRANGEMENT_COORDINATE: f64 = 20_000_000.0;

/// One connected display as the arrangement validator sees it.
pub(crate) struct ArrangedDisplay<'a> {
    pub(crate) id: &'a str,
    pub(crate) local: bool,
}

/// Positions are canvas data for both computers' editors; the transport never reads them.
/// A hidden display is one marked not in use: it stays known, but nothing routes the pointer
/// onto it, and each computer keeps at least one display in use.
pub(crate) fn validate_arrangement(
    layout: &LayoutRequest,
    displays: &[ArrangedDisplay<'_>],
) -> Result<(), String> {
    let Some(arrangement) = &layout.arrangement else {
        return Ok(());
    };
    if !matches!(arrangement.mode.as_str(), "grouped" | "free") {
        return Err("Choose how the displays are arranged.".into());
    }
    if arrangement.positions.len() > MAX_ARRANGED_DISPLAYS
        || arrangement.hidden.len() > MAX_ARRANGED_DISPLAYS
    {
        return Err("The arrangement places too many displays.".into());
    }
    let known = |id: &str| displays.iter().find(|display| display.id == id);
    let mut used: Vec<&str> = Vec::with_capacity(arrangement.positions.len());
    for position in &arrangement.positions {
        if parse_display(&position.display).is_err()
            || known(&position.display).is_none()
            || used.contains(&position.display.as_str())
        {
            return Err("The arrangement names a display that is not connected.".into());
        }
        if !position.x.is_finite()
            || !position.y.is_finite()
            || position.x.abs() > MAX_ARRANGEMENT_COORDINATE
            || position.y.abs() > MAX_ARRANGEMENT_COORDINATE
        {
            return Err("The arrangement places a display out of range.".into());
        }
        used.push(&position.display);
    }
    // Links and the starting display are parsed leniently later, so hidden ids are compared as numbers.
    let same = |value: &str, id: DisplayId| parse_display(value).is_ok_and(|parsed| parsed == id);
    for hidden in &arrangement.hidden {
        let Some(hidden_id) = known(hidden).and_then(|_| parse_display(hidden).ok()) else {
            return Err("The arrangement hides a display that is not connected.".into());
        };
        if used.contains(&hidden.as_str()) {
            return Err("A hidden display cannot also be placed.".into());
        }
        let routed = same(&layout.source_display, hidden_id)
            || layout.links.iter().any(|link| {
                same(&link.from_display, hidden_id) || same(&link.to_display, hidden_id)
            });
        if routed {
            return Err("The pointer cannot cross onto a display that is not in use.".into());
        }
        used.push(hidden);
    }
    for local in [true, false] {
        let own: Vec<&str> = displays
            .iter()
            .filter(|d| d.local == local)
            .map(|d| d.id)
            .collect();
        if !own.is_empty()
            && own
                .iter()
                .all(|id| arrangement.hidden.iter().any(|h| h == id))
        {
            return Err("Each computer needs at least one display in use.".into());
        }
    }
    if arrangement.mode == "free" && used.len() != displays.len() {
        return Err("Place every display before applying a free arrangement.".into());
    }
    Ok(())
}

/// Everything the setup-link runner needs. Bundled so tests can swap the runner for a fake.
struct LinkRun {
    interface_id: String,
    peer: CertificateFingerprint,
    cancel: RevocationSignal,
    persist: LinkPersist,
    events: UnboundedSender<LinkEvent>,
    commands: UnboundedReceiver<LinkCommand>,
}

type LinkFuture = Pin<Box<dyn Future<Output = Result<(), SetupFailure>>>>;
type LinkRunner = fn(LinkRun) -> LinkFuture;

fn transport_link(run: LinkRun) -> LinkFuture {
    Box::pin(async move {
        let LinkRun {
            interface_id,
            peer,
            cancel,
            persist,
            events,
            commands,
        } = run;
        session_link::run_setup_link(&interface_id, peer, &cancel, persist, events, commands).await
    })
}

#[derive(Default)]
struct State {
    view: SharingView,
    worker_generation: u64,
    authorization_revision: u64,
    shutdown: bool,
    native_cleanup_pending: bool,
    inspection: Option<InspectedPeer>,
    source_side: Option<SourceSide>,
    cancel: Option<RevocationSignal>,
    native_cancel: Option<RevocationSignal>,
    progress: Option<session::SessionProgress>,
    link: Option<UnboundedSender<LinkCommand>>,
    link_since: Option<Instant>,
    link_touched: Option<Instant>,
    /// Set by any deliberate close so the finished worker never reports an error.
    close_message: Option<String>,
    /// Claimed by each link start and retired by every Stop, so no staged write outlives its link.
    link_epoch: u64,
    /// Validated by the link but not yet written; a Stop before commit drops it.
    staged: Option<SharingPreferences>,
    /// The computer the running worker is for.
    peer: Option<CertificateFingerprint>,
    /// Mirrors the setup file so the view can show the active computer without a live worker.
    active: Option<String>,
    editing: bool,
    /// The other computer's arranging state as the link reports it, never inferred; false
    /// whenever no link worker is alive.
    peer_arranging: bool,
    display_notice: Option<DisplayNotice>,
    /// The displays the banner was last raised for, so one change raises it once.
    notice_geometry: Option<DisplayGeometry>,
    /// The answer given for `notice_geometry`. A dismissal hides the banner but not the answer,
    /// so dismissing "nothing fits" does not set the supervisor searching again every pass.
    notice_answer: Option<DisplayNotice>,
    /// Whether the layout this computer proposed had to leave a crossing or a display out. Only
    /// that turns the commit into "sharing continued"; a layout that fit exactly says nothing.
    notice_left_out: bool,
    /// The displays of a proposal still in flight. Cleared by its outcome or by the link ending,
    /// so a proposal lost with the link is made again instead of waiting forever.
    pending_proposal: Option<DisplayGeometry>,
    /// Consumed once by the app, which then brings its window forward.
    notice_window_pending: bool,
    /// Why the supervisor keeps a standing link up instead of a session; None once a layout is
    /// applied or the computer is chosen again.
    link_reason: Option<LinkReason>,
    sharing_role: Option<&'static str>,
    /// Outlives the automatic reconnect, so Home still shows why the previous session ended.
    last_failure: Option<String>,
    last_failure_at: Option<Instant>,
    /// When a worker last ended with a real error, so the supervisor waits out its backoff
    /// instead of relaunching the same failure every pass. A close, a misfit or a step change
    /// is not a failure and never sets it.
    worker_failed_at: Option<Instant>,
}

pub struct SharingController {
    operation: Mutex<()>,
    state: Arc<Mutex<State>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    /// Shared with each link's commit, so no two writers of the setup file interleave.
    setup_file: Arc<SetupFileLock>,
    link_runner: LinkRunner,
    idle_window: Duration,
}

impl Default for SharingController {
    fn default() -> Self {
        Self {
            operation: Mutex::default(),
            state: Arc::default(),
            worker: Mutex::default(),
            setup_file: Arc::default(),
            link_runner: transport_link,
            idle_window: LINK_IDLE_WINDOW,
        }
    }
}

/// Every change to the setup file is one load-modify-save under this lock: a user action and a
/// link commit that race each other both land, in some order, on top of the other's write.
#[derive(Default)]
struct SetupFileLock(Mutex<()>);

#[derive(Debug, PartialEq, Eq)]
enum SetupFileError {
    Read,
    Write,
}

impl SetupFileLock {
    fn update(
        &self,
        path: &Path,
        edit: impl FnOnce(&mut SetupFile),
    ) -> Result<SetupFile, SetupFileError> {
        let _guard = lock(&self.0);
        update_setup(path, edit)
    }

    fn write_setup(&self, path: &Path, setup: SharingPreferences) -> Result<(), SetupFileError> {
        let _guard = lock(&self.0);
        write_setup(path, setup)
    }

    /// The link's commit. `take_staged` runs under the file lock, so a Use or Pause either
    /// retired the staged layout before this or waits and then writes over the committed one.
    fn commit(
        &self,
        path: &Path,
        take_staged: impl FnOnce() -> Result<SharingPreferences, LinkRejectReason>,
    ) -> Result<(), LinkRejectReason> {
        let _guard = lock(&self.0);
        let staged = take_staged()?;
        write_setup(path, staged).map_err(|_| LinkRejectReason::SaveFailed)
    }
}

fn update_setup(
    path: &Path,
    edit: impl FnOnce(&mut SetupFile),
) -> Result<SetupFile, SetupFileError> {
    let mut file = SetupFile::load(path).map_err(|_| SetupFileError::Read)?;
    edit(&mut file);
    file.save(path).map_err(|_| SetupFileError::Write)?;
    Ok(file)
}

/// Stores one computer's record and makes that computer active.
fn write_setup(path: &Path, setup: SharingPreferences) -> Result<(), SetupFileError> {
    let peer = setup.peer().map_err(|_| SetupFileError::Write)?;
    update_setup(path, |file| {
        file.set_active(Some(&peer));
        file.insert(setup);
    })
    .map(drop)
}

#[derive(Clone)]
enum WorkerKind {
    Session,
    Link(UnboundedSender<LinkCommand>),
}

/// What a standing link waits for before the port goes back to a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkReason {
    /// The saved layout no longer fits both computers' displays; the link yields once it does.
    LayoutMisfit,
    /// This computer saved a layout the other could not; only a fresh Apply clears it.
    LayoutUnconfirmed,
    /// A session dial met the other computer on its setup link. Only the choice between a session
    /// and a link is affected: once both links are up the decision path runs as usual, so this is
    /// cleared the moment the link connects.
    PeerHoldsLink,
}

/// Which computer of the pair supplies input. Two Macs or two PCs make the platform ambiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceSide {
    Local,
    Peer,
}

fn parse_side(value: &str) -> Result<SourceSide, String> {
    match value {
        "local" => Ok(SourceSide::Local),
        "peer" => Ok(SourceSide::Peer),
        _ => Err("Choose which computer has the keyboard and mouse.".into()),
    }
}

const fn side_name(side: SourceSide) -> &'static str {
    match side {
        SourceSide::Local => "local",
        SourceSide::Peer => "peer",
    }
}

const fn transport_side(side: SourceSide) -> session_setup::SourceSide {
    match side {
        SourceSide::Local => session_setup::SourceSide::Local,
        SourceSide::Peer => session_setup::SourceSide::Peer,
    }
}

const fn local_platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else {
        "windows"
    }
}

impl SharingController {
    /// Window shutdown must wait for retained native release workers, including prior sessions.
    pub fn shutdown_ready(&self) -> bool {
        !monhop_core::NativeInputOwnership::is_claimed()
            && lock(&self.worker)
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
    }
    pub fn request_shutdown(&self) {
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        state.shutdown = true;
        begin_close(&mut state, "Stopping sharing and releasing held input.");
        if !state.view.busy {
            state.progress = None;
            state.cancel = None;
            state.native_cancel = None;
            set_idle_message(&mut state, true);
        }
    }
    pub fn invalidate_if_idle(&self) -> Result<(), String> {
        let _operation = lock(&self.operation);
        if monhop_core::NativeInputOwnership::is_claimed() {
            return Err("Wait for local input cleanup before connecting again.".into());
        }
        let mut worker = lock(&self.worker);
        if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Err("Wait for the current sharing action to finish.".into());
        }
        if let Some(previous) = worker.take() {
            let _ = previous.join();
        }
        if monhop_core::NativeInputOwnership::is_claimed() {
            return Err("Wait for local input cleanup before connecting again.".into());
        }
        let mut state = lock(&self.state);
        if state.shutdown {
            return Err("MonHop is shutting down and cannot connect.".into());
        }
        if state.view.busy {
            return Err("Wait for the current sharing action to finish.".into());
        }
        invalidate_authorization(&mut state);
        state.view.phase = "off";
        state.view.sharing_active = false;
        state.view.message = NOT_CONNECTED.into();
        state.native_cleanup_pending = false;
        state.cancel = None;
        state.native_cancel = None;
        state.progress = None;
        state.link = None;
        state.link_since = None;
        state.link_touched = None;
        Ok(())
    }
    pub fn status(&self) -> SharingView {
        let native_cleanup_pending = monhop_core::NativeInputOwnership::is_claimed();
        let mut state = lock(&self.state);
        if !state.view.busy && state.native_cleanup_pending && !native_cleanup_pending {
            state.native_cleanup_pending = false;
            let shutdown = state.shutdown;
            set_idle_message(&mut state, shutdown);
        }
        let mut view = view_of(&state);
        let mut held = false;
        if let Some(progress) = state.progress.as_ref() {
            let stats = progress.diagnostics();
            held = stats.held;
            view.diagnostics = DiagnosticsView {
                sent_events: stats.sent_events.to_string(),
                received_events: stats.received_events.to_string(),
                round_trip_ms: stats.round_trip_micros as f64 / 1000.0,
                active_display: if view.busy {
                    stats.active_display.map(|display| display.0.to_string())
                } else {
                    None
                },
                active_is_local: if view.busy {
                    stats.active_is_local
                } else {
                    None
                },
            };
        }
        if view.phase == "starting"
            && state
                .progress
                .as_ref()
                .is_some_and(session::SessionProgress::is_started)
        {
            view.phase = "sharing";
            view.sharing_active = true;
            view.message =
                "Sharing enabled. Hold both Control keys and Escape for two seconds to stop."
                    .into();
        }
        view.held = held && view.phase == "sharing";
        if !view.busy && native_cleanup_pending {
            view.phase = "error";
            view.message = native_cleanup_message().into();
        }
        view
    }
    /// Ends the live link or session; `idle_message` is what the view says once input is local.
    pub fn stop(&self) -> SharingView {
        self.stop_with(NOT_CONNECTED)
    }

    pub fn stop_with(&self, idle_message: &str) -> SharingView {
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        if state.view.busy || state.link.is_some() {
            state.close_message = Some(idle_message.to_owned());
        }
        begin_close(
            &mut state,
            "Stopping the connection and releasing held input.",
        );
        if !state.view.busy {
            state.cancel = None;
            state.native_cancel = None;
            state.progress = None;
            set_idle_message(&mut state, false);
            if !state.native_cleanup_pending {
                state.view.message = idle_message.to_owned();
            }
        }
        view_of(&state)
    }

    /// The drop line as Home shows it, for an explicit copy; None while no drop is recorded.
    pub fn last_drop_text(&self) -> Option<String> {
        let state = lock(&self.state);
        let text = drop_note(
            state.last_failure.as_deref(),
            state.last_failure_at.map(|at| at.elapsed()),
        );
        (!text.is_empty()).then_some(text)
    }

    /// True only with no standing link: the test window owns input on its own connection.
    pub fn link_is_off(&self) -> bool {
        let state = lock(&self.state);
        state.link.is_none()
            && !matches!(
                state.view.phase,
                "connecting" | "connected" | "reconnecting" | "stopping"
            )
    }

    /// Whether a link or session worker is alive, and for which computer.
    pub fn live_peer(&self) -> Option<CertificateFingerprint> {
        let alive = lock(&self.worker)
            .as_ref()
            .is_some_and(|worker| !worker.is_finished());
        alive.then(|| lock(&self.state).peer).flatten()
    }

    pub fn editing(&self) -> bool {
        lock(&self.state).editing
    }

    /// The link's current displays for the computer it is with; None without a live link.
    pub fn live_inspection(&self) -> Option<(String, InspectedPeer)> {
        let state = lock(&self.state);
        state
            .peer
            .zip(state.inspection.clone())
            .map(|(peer, inspection)| (fingerprint_key(&peer.full_hex()), inspection))
    }

    pub fn revision(&self) -> String {
        lock(&self.state).view.revision.clone()
    }

    /// Records the active computer and, when a worker is up with another computer, ends it so
    /// the supervisor connects to the chosen one. None pauses sharing.
    pub fn set_active(
        &self,
        path: &Path,
        fingerprint: Option<CertificateFingerprint>,
    ) -> Result<SharingView, String> {
        self.update_setup_file(path, |file| file.set_active(fingerprint.as_ref()))?;
        {
            let mut state = lock(&self.state);
            publish_arranging(&state, false);
            state.editing = false;
            state.link_reason = None;
            state.last_failure = None;
            state.last_failure_at = None;
            state.worker_failed_at = None;
        }
        let live = self.live_peer();
        if live.is_some() && live != fingerprint {
            return Ok(self.stop_with(if fingerprint.is_some() {
                SWITCHING_COMPUTER
            } else {
                PAUSED
            }));
        }
        if fingerprint.is_none() {
            let mut state = lock(&self.state);
            if !state.view.busy {
                state.view.phase = "off";
                state.view.message = PAUSED.to_owned();
            }
        }
        Ok(self.status())
    }

    /// Applies one change to the setup file and mirrors the result into the view. Every writer
    /// of that file goes through here or the link's commit, never around them.
    pub fn update_setup_file(
        &self,
        path: &Path,
        edit: impl FnOnce(&mut SetupFile),
    ) -> Result<SetupFile, String> {
        let file = self.setup_file.update(path, edit).map_err(|error| {
            match error {
                SetupFileError::Read => "The saved setup could not be read.",
                SetupFileError::Write => "The saved setup could not be updated.",
            }
            .to_owned()
        })?;
        self.adopt_saved(&file);
        Ok(file)
    }

    /// Arranging starts the idle window on the current link; the link itself is opened by the
    /// caller when none is up. The other computer is told, so it never proposes over the user.
    pub fn begin_editing(&self) -> SharingView {
        let mut state = lock(&self.state);
        state.editing = true;
        publish_arranging(&state, true);
        touch_link(&mut state);
        view_of(&state)
    }

    /// Ends arranging and closes the link so the supervisor reconnects for sharing.
    pub fn end_editing(&self) -> SharingView {
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        state.editing = false;
        publish_arranging(&state, false);
        state.worker_failed_at = None;
        if state.link.is_some() && state.close_message.is_none() {
            state.close_message = Some(EDITING_ENDED.to_owned());
            close_link(&mut state, EDITING_ENDED);
        }
        view_of(&state)
    }

    /// Home's display banner, raised once per set of displays. A dismissal keeps that memory,
    /// so the same change never raises it twice.
    pub fn raise_display_notice(&self, kind: DisplayNotice, inspection: &InspectedPeer) -> bool {
        let geometry = DisplayGeometry::of(inspection);
        raise_notice(&mut lock(&self.state), kind, geometry)
    }

    /// True while a proposal for exactly these displays is still out. A banner is not this
    /// answer: it survives a lost link, and a proposal that went down with one must be made again.
    pub fn proposal_pending_for(&self, inspection: &InspectedPeer) -> bool {
        let geometry = DisplayGeometry::of(inspection);
        lock(&self.state)
            .pending_proposal
            .as_ref()
            .is_some_and(|pending| pending.same(&geometry))
    }

    /// True once these displays were answered with "nothing fits": there is nothing left to
    /// propose until they change again or a layout is applied.
    pub fn waiting_notice_for(&self, inspection: &InspectedPeer) -> bool {
        let geometry = DisplayGeometry::of(inspection);
        let state = lock(&self.state);
        state.notice_answer == Some(DisplayNotice::Waiting)
            && state
                .notice_geometry
                .as_ref()
                .is_some_and(|raised| raised.same(&geometry))
    }

    /// True while a failed worker's backoff still runs, so the supervisor does not relaunch the
    /// same failure every pass. Any user choice clears it.
    pub fn within_failure_backoff(&self, backoff: Duration) -> bool {
        lock(&self.state)
            .worker_failed_at
            .is_some_and(|failed| failed.elapsed() < backoff)
    }

    pub fn clear_failure_backoff(&self) {
        lock(&self.state).worker_failed_at = None;
    }

    pub fn dismiss_display_notice(&self) -> SharingView {
        let mut state = lock(&self.state);
        state.display_notice = None;
        view_of(&state)
    }

    pub fn clear_display_notice(&self) {
        clear_notice(&mut lock(&self.state));
    }

    /// True once for each raised banner: the app then brings its window forward.
    pub fn take_window_forward(&self) -> bool {
        std::mem::take(&mut lock(&self.state).notice_window_pending)
    }

    /// The live link's displays while a switch may act, under the same guards a yield uses.
    /// A user arranging on either computer holds it off: an automatic proposal must never eject
    /// someone mid-arrangement.
    pub fn inspection_for_switch(&self) -> Option<InspectedPeer> {
        let state = lock(&self.state);
        if state.link.is_none()
            || state.editing
            || state.peer_arranging
            || state.link_reason == Some(LinkReason::LayoutUnconfirmed)
            || state.view.phase != "connected"
            || state.close_message.is_some()
        {
            return None;
        }
        state.inspection.clone()
    }

    /// Makes a remembered or adapted record the active one; the banner it answers is cleared.
    pub fn write_active_setup(&self, path: &Path, setup: SharingPreferences) -> Result<(), String> {
        self.setup_file
            .write_setup(path, setup)
            .map_err(|_| "The saved setup could not be updated.".to_owned())?;
        self.clear_display_notice();
        Ok(())
    }

    /// A standing link whose displays fit the saved layout again yields the port to a session,
    /// unless it is held for a fresh Apply or either computer's user is arranging.
    pub fn yield_link_when_layout_fits(&self, saved: &SharingPreferences) -> bool {
        let mut state = lock(&self.state);
        let fits = state.link.is_some()
            && !state.editing
            && !state.peer_arranging
            && state.link_reason != Some(LinkReason::LayoutUnconfirmed)
            && state.view.phase == "connected"
            && state.close_message.is_none()
            && state
                .inspection
                .as_ref()
                .is_some_and(|inspection| saved.fits_displays(inspection));
        if fits {
            state.link_reason = None;
            state.close_message = Some(LAYOUT_FITS_AGAIN.to_owned());
            close_link(&mut state, LAYOUT_FITS_AGAIN);
        }
        fits
    }

    /// True while a standing link must stay up instead of a session.
    pub fn holds_link(&self) -> bool {
        lock(&self.state).link_reason.is_some()
    }

    /// Opens the setup link to `peer`; whether it is a standing link or one for arranging is
    /// decided by the editing flag, never by the link itself.
    pub fn connect_link(
        &self,
        path: PathBuf,
        interface_id: String,
        peer: CertificateFingerprint,
    ) -> Result<SharingView, String> {
        if interface_id.is_empty() || interface_id.len() > 512 {
            return Err("Choose a current physical network.".into());
        }
        let (events, incoming) = tokio::sync::mpsc::unbounded_channel();
        let (commands, outgoing) = tokio::sync::mpsc::unbounded_channel();
        let closer = commands.clone();
        let runner = self.link_runner;
        let idle_window = self.idle_window;
        let setup_file = Arc::clone(&self.setup_file);
        self.launch(
            "connecting",
            "Connecting to the other computer.",
            None,
            WorkerKind::Link(commands),
            Some(peer),
            move |state, generation, _authorization, cancel, _progress| {
                let persist = link_persist(Arc::clone(&state), generation, path, setup_file);
                let link = runner(LinkRun {
                    interface_id,
                    peer,
                    cancel: cancel.clone(),
                    persist,
                    events,
                    commands: outgoing,
                });
                pump_link(
                    state,
                    generation,
                    cancel,
                    closer,
                    incoming,
                    link,
                    idle_window,
                )
            },
        )
    }

    /// Refreshes the idle window after a user action the UI performed without a controller call.
    pub fn touch(&self) {
        let mut state = lock(&self.state);
        touch_link(&mut state);
    }

    /// Proposes a layout over the standing link. The outcome arrives as a link event.
    pub fn apply_setup(
        &self,
        revision: &str,
        source: &str,
        layout: LayoutRequest,
    ) -> Result<SharingView, String> {
        let _operation = lock(&self.operation);
        let side = parse_side(source)?;
        let mut state = lock(&self.state);
        if state.shutdown
            || state.view.busy
            || state.view.phase != "connected"
            || state.view.revision != revision
        {
            return Err(
                "Connect the computers and check the current displays before applying a layout."
                    .into(),
            );
        }
        if matches!(state.view.sync.state, "sending" | "receiving") {
            return Err("Wait for the current layout to finish applying.".into());
        }
        let commands = state
            .link
            .clone()
            .ok_or("Connect the computers before applying a layout.")?;
        // The stored inspection keeps its reviewed source until the peer accepts this proposal.
        let mut proposed = state
            .inspection
            .clone()
            .ok_or("Connect both computers first.")?;
        proposed.source = proposed.device_for(transport_side(side));
        validated_layout(&proposed, &layout)?;
        let bytes = sharing_preferences::shared_setup_bytes(&proposed, layout)
            .map_err(|error| error.to_string())?;
        commands
            .send(LinkCommand::Propose { bytes })
            .map_err(|_| "The connection ended. Connect again.".to_owned())?;
        touch_link(&mut state);
        state.view.sync = SyncView {
            state: "sending",
            message: "Applying the layout on both computers.".into(),
        };
        state.view.message = "Applying the layout on both computers.".into();
        Ok(view_of(&state))
    }

    /// The supervisor's own Apply: the computer with the keyboard sends the layout it chose for
    /// the displays connected now. Same staging and commit as `apply_setup`, with no revision
    /// from the window to check, because no window took part in choosing it.
    ///
    /// The proposal is recorded and the banner raised under the same lock that sends it, so the
    /// supervisor can never see a sent proposal it has no record of, or a record of one that
    /// never left.
    ///
    /// `left_out` says whether this layout had to drop a crossing or a display; it decides what
    /// the commit leaves on screen, so it travels with the send rather than being asked for later.
    pub fn propose_layout(&self, next: &SharingPreferences, left_out: bool) -> Result<(), String> {
        let _operation = lock(&self.operation);
        let side = parse_side(next.source_side())?;
        let mut state = lock(&self.state);
        if state.shutdown || state.view.busy || state.view.phase != "connected" {
            return Err("Connect the computers before applying a layout.".into());
        }
        if matches!(state.view.sync.state, "sending" | "receiving") {
            return Err("Wait for the current layout to finish applying.".into());
        }
        let commands = state
            .link
            .clone()
            .ok_or("Connect the computers before applying a layout.")?;
        let mut proposed = state
            .inspection
            .clone()
            .ok_or("Connect both computers first.")?;
        proposed.source = proposed.device_for(transport_side(side));
        let layout = next.layout().clone();
        validated_layout(&proposed, &layout)?;
        let bytes = sharing_preferences::shared_setup_bytes(&proposed, layout)
            .map_err(|error| error.to_string())?;
        commands
            .send(LinkCommand::Propose { bytes })
            .map_err(|_| "The connection ended. Connect again.".to_owned())?;
        let geometry = DisplayGeometry::of(&proposed);
        raise_notice(&mut state, DisplayNotice::Updating, geometry.clone());
        // Set after the raise, which clears it: this send is the authority even when it reuses a
        // banner already up for these displays.
        state.notice_left_out = left_out;
        state.pending_proposal = Some(geometry);
        state.view.sync = SyncView {
            state: "sending",
            message: UPDATING_LAYOUT.into(),
        };
        state.view.message = UPDATING_LAYOUT.into();
        Ok(())
    }

    /// The test window opens with the link closed, so the active computer's applied setup on
    /// disk is the basis.
    pub fn validate_trial_setup(
        &self,
        path: &Path,
        revision: &str,
        layout: &LayoutRequest,
    ) -> Result<SharingPreferences, String> {
        {
            let state = lock(&self.state);
            if state.shutdown || state.view.busy || state.view.revision != revision {
                return Err("Finish the current sharing action before testing.".into());
            }
        }
        let saved = active_setup(path)?;
        if saved.layout() != layout {
            return Err("Apply this layout on both computers before testing.".into());
        }
        if !local_displays_fit(&saved)? {
            return Err(
                "The displays changed since this layout was applied. Connect and apply again."
                    .into(),
            );
        }
        Ok(saved)
    }

    pub fn select_source(&self, revision: &str, source: &str) -> Result<SharingView, String> {
        let _operation = lock(&self.operation);
        let side = parse_side(source)?;
        let mut state = lock(&self.state);
        if state.shutdown
            || state.view.busy
            || state.view.phase != "connected"
            || state.view.revision != revision
        {
            return Err("Connect the computers again before changing the input computer.".into());
        }
        let inspection = state
            .inspection
            .as_mut()
            .ok_or("Connect both computers first.")?;
        inspection.source = inspection.device_for(transport_side(side));
        state.source_side = Some(side);
        advance_authorization(&mut state);
        if state.shutdown {
            return Err("Restart MonHop before changing the input computer.".into());
        }
        state.view.synchronized_layout = None;
        state.view.sync = SyncView::default();
        state.view.message =
            "Input computer selected. Arrange the displays, then apply the layout.".into();
        touch_link(&mut state);
        Ok(view_of(&state))
    }

    pub fn enable_trial(
        &self,
        path: &Path,
        revision: String,
        layout: LayoutRequest,
        trial: monhop_transport::session_trial::TrialAuthorization,
    ) -> Result<SharingView, String> {
        let saved = self.validate_trial_setup(path, &revision, &layout)?;
        let interface_id = saved.interface_id().to_owned();
        let peer = saved.peer()?;
        // The applied file, not a live choice, decides which computer supplies input in a test.
        let side = transport_side(parse_side(saved.source_side())?);
        self.launch(
            "starting",
            "Starting the controlled test. Nothing is shared until both computers start.",
            Some(revision),
            WorkerKind::Session,
            Some(peer),
            move |state, worker_generation, authorization_revision, cancel, progress| async move {
                let paired = session_setup::connect_trial_after_local_action(
                    &interface_id,
                    side,
                    &trial.revocation(),
                    peer,
                )
                .await
                .map_err(setup_message)?;
                // Identity and exact geometry must still match what was applied to disk.
                if saved.layout_for_inspection(&paired.inspection).as_ref() != Ok(&layout) {
                    return Err(setup_message(SetupFailure::ChangedSinceInspection));
                }
                let (initial_display, topology) = validated_layout(&paired.inspection, &layout)?;
                register_native_cancellation(
                    &mut lock(&state),
                    worker_generation,
                    authorization_revision,
                    &cancel,
                    paired.revocation.clone(),
                )?;
                let result = if paired.session.local_is_source() {
                    session::run_trial_source(
                        paired.session,
                        topology,
                        initial_display,
                        worker_generation,
                        paired.revocation.clone(),
                        progress,
                        trial,
                    )
                    .await
                } else {
                    session::run_trial_destination(
                        paired.session,
                        paired.revocation.clone(),
                        progress,
                        trial,
                    )
                    .await
                };
                match result {
                    Err(SessionFailure::Revoked) if cancel.is_revoked() => Ok(()),
                    other => other.map_err(session_message),
                }
            },
        )
    }

    #[cfg(test)]
    pub fn enable(&self, revision: String, layout: LayoutRequest) -> Result<SharingView, String> {
        let (inspected, side) = {
            let state = lock(&self.state);
            if state.shutdown {
                return Err("MonHop is shutting down and cannot enable sharing.".into());
            }
            if state.view.busy || state.view.phase != "connected" || state.view.revision != revision
            {
                return Err("Connect the computers again before enabling sharing.".into());
            }
            let inspected = state
                .inspection
                .clone()
                .ok_or("Connect both computers first.")?;
            let side = state
                .source_side
                .ok_or("Choose which computer has the keyboard and mouse.")?;
            (inspected, side)
        };
        let (initial_display, topology) = validated_layout(&inspected, &layout)?;
        let peer = inspected.peer_fingerprint;
        self.launch(
            "starting",
            "Starting input sharing.",
            Some(revision),
            WorkerKind::Session,
            Some(peer),
            move |state, worker_generation, authorization_revision, cancel, progress| async move {
                let paired = session_setup::connect_after_local_action(
                    &inspected.interface_id,
                    transport_side(side),
                    &cancel,
                    peer,
                )
                .await
                .map_err(setup_message)?;
                if !inspected.matches(&paired.inspection) {
                    return Err(setup_message(SetupFailure::ChangedSinceInspection));
                }
                register_native_cancellation(
                    &mut lock(&state),
                    worker_generation,
                    authorization_revision,
                    &cancel,
                    paired.revocation.clone(),
                )?;
                let result = if paired.session.local_is_source() {
                    session::run_source(
                        paired.session,
                        topology,
                        initial_display,
                        worker_generation,
                        paired.revocation.clone(),
                        progress,
                    )
                    .await
                } else {
                    session::run_destination(paired.session, paired.revocation.clone(), progress)
                        .await
                };
                flush_session_close(&paired.lease).await;
                match result {
                    Err(SessionFailure::Revoked) if cancel.is_revoked() => Ok(()),
                    other => other.map_err(session_message),
                }
            },
        )
    }

    /// Mirrors the setup file into the view so Home shows the active computer and its role
    /// without a live link.
    pub fn adopt_saved(&self, file: &SetupFile) {
        let mut state = lock(&self.state);
        state.active = file.active().map(str::to_owned);
        state.sharing_role = file.active_computer().map(|saved| {
            if saved.source_side() == "local" {
                "sends"
            } else {
                "receives"
            }
        });
    }

    /// A start the supervisor could not make is shown once, not retried every tick.
    pub fn note_supervisor_failure(&self, message: String) {
        let mut state = lock(&self.state);
        if !state.view.busy {
            state.view.phase = "error";
            state.view.message = message;
        }
    }

    /// Keeps one authenticated sharing session alive with the active computer: connect, run,
    /// reconnect. The pairing itself is checked by the handshake.
    pub fn start_sharing(&self, saved: SharingPreferences) -> Result<SharingView, String> {
        let interface_id = saved.interface_id().to_owned();
        let peer = saved.peer()?;
        let side = transport_side(parse_side(saved.source_side())?);
        let layout = saved.layout().clone();
        self.launch(
            "starting",
            CONNECTING_FOR_SHARING,
            None,
            WorkerKind::Session,
            Some(peer),
            move |state, worker_generation, authorization_revision, cancel, _progress| async move {
                let mut attempt: u32 = 0;
                let mut window_started = Instant::now();
                let mut endpoint =
                    session_setup::StandingShareEndpoint::new(&interface_id, side, peer);
                loop {
                    if cancel.is_revoked() {
                        return Ok(());
                    }
                    attempt = attempt.saturating_add(1);
                    log::info!("sharing: attempt {attempt} on {interface_id}, source side {side:?}");
                    let progress = session::SessionProgress::default();
                    {
                        let mut guard = lock(&state);
                        if guard.worker_generation != worker_generation {
                            return Ok(());
                        }
                        guard.view.link.attempt = attempt;
                        guard.view.phase = "starting";
                        guard.view.sharing_active = false;
                        guard.view.message = CONNECTING_FOR_SHARING.to_owned();
                        guard.progress = Some(progress.clone());
                    }
                    let paired = match endpoint.connect(&cancel).await {
                        Ok(paired) => paired,
                        Err(_) if cancel.is_revoked() => return Ok(()),
                        // The other computer already has its setup link open; no session starts
                        // until both are on the same step, so this computer joins it there.
                        Err(SetupFailure::PurposeMismatch) => {
                            log::info!(
                                "sharing: attempt {attempt} met the other computer on its setup link; opening this computer's link"
                            );
                            let mut guard = lock(&state);
                            guard.link_reason = Some(LinkReason::PeerHoldsLink);
                            guard.close_message = Some(PEER_ARRANGING.to_owned());
                            return Ok(());
                        }
                        // Both computers dialed a session and their records name different
                        // keyboard sides. Dialing again repeats it: the link is where the
                        // keyboard side proposes one record for both.
                        Err(SetupFailure::ChangedSinceInspection) => {
                            note_record_disagreement(&mut lock(&state));
                            return Ok(());
                        }
                        // The switch is the user's standing intent: a peer that is off or
                        // still installing must be able to join later without a new click.
                        // Attempt outcomes go to the status line; last_failure keeps the reason
                        // the previous session ended.
                        Err(error) => {
                            log::debug!(
                                "sharing: attempt {attempt} did not connect ({error:?}), waited {:.0} s",
                                window_started.elapsed().as_secs_f64()
                            );
                            lock(&state).view.message =
                                share_wait_message(error, window_started.elapsed());
                            sleep_unless_cancelled(&cancel, RECONNECT_INTERVAL).await;
                            continue;
                        }
                    };
                    if saved.layout_for_inspection(&paired.inspection).as_ref() != Ok(&layout) {
                        note_layout_misfit(&mut lock(&state));
                        return Ok(());
                    }
                    let local_is_source = paired.session.local_is_source();
                    log::info!(
                        "sharing: attempt {attempt} connected, starting the {} session",
                        if local_is_source { "source" } else { "destination" }
                    );
                    let (initial_display, topology) =
                        validated_layout(&paired.inspection, &layout)?;
                    register_native_cancellation(
                        &mut lock(&state),
                        worker_generation,
                        authorization_revision,
                        &cancel,
                        paired.revocation.clone(),
                    )?;
                    // A session has no link to report displays, so the computer cards read the
                    // live ones from here; the record it runs with is exactly this geometry.
                    lock(&state).inspection = Some(paired.inspection.clone());
                    let session_started = Instant::now();
                    let result = if local_is_source {
                        session::run_source(
                            paired.session,
                            topology,
                            initial_display,
                            worker_generation,
                            paired.revocation.clone(),
                            progress,
                        )
                        .await
                    } else {
                        session::run_destination(
                            paired.session,
                            paired.revocation.clone(),
                            progress,
                        )
                        .await
                    };
                    flush_session_close(&paired.lease).await;
                    let ended = {
                        let mut guard = lock(&state);
                        guard.native_cancel = None;
                        guard.inspection = None;
                        guard.view.sharing_active = false;
                        // Keep the ended session's counters visible; a cleared progress stops
                        // status() from reporting the reconnect wait as sharing.
                        let ended = guard.progress.take().map(|progress| progress.diagnostics());
                        if let Some(stats) = ended {
                            guard.view.diagnostics = DiagnosticsView {
                                sent_events: stats.sent_events.to_string(),
                                received_events: stats.received_events.to_string(),
                                round_trip_ms: stats.round_trip_micros as f64 / 1000.0,
                                active_display: None,
                                active_is_local: None,
                            };
                        }
                        guard.view.phase = "starting";
                        ended
                    };
                    let load = ended
                        .as_ref()
                        .map(|stats| format!("{}{}", receiver_load_note(stats), link_note(stats)))
                        .unwrap_or_default();
                    log::warn!(
                        "sharing: attempt {attempt} session over after {:.1} s: {result:?}{load}",
                        session_started.elapsed().as_secs_f64()
                    );
                    let peer_close = ended
                        .as_ref()
                        .and_then(|stats| stats.link)
                        .map(|link| link.close)
                        .unwrap_or_default();
                    // A display change is a step, not a failure. Classified before the arms below
                    // so it keeps no failure reason, and acted on after native release so the
                    // worker still leaves input clean.
                    let outgrown = match result {
                        Err(SessionFailure::Revoked | SessionFailure::NativeCleanup) => None,
                        _ => displays_changed_end(result, peer_close, local_is_source, || {
                            current_local_displays(&saved)
                                .ok()
                                .map(|current| saved.matches_local_displays(&current))
                        }),
                    };
                    // The status line is written once the end is classified, so a display change
                    // says so at once instead of showing "Sharing dropped" for the up-to-two
                    // seconds native release takes. A real drop still says it did.
                    lock(&state).view.message =
                        outgrown.unwrap_or("Sharing dropped. Reconnecting.").to_owned();
                    match result {
                        Err(SessionFailure::Revoked) if cancel.is_revoked() => return Ok(()),
                        Err(SessionFailure::NativeCleanup) => {
                            return Err(native_cleanup_message().into());
                        }
                        // The displays explain this end; it is not a drop and keeps no reason.
                        _ if outgrown.is_some() => {}
                        // The other computer's user ended it: nothing dropped, so no drop line.
                        Ok(()) => lock(&state).view.message = PEER_PAUSED.to_owned(),
                        Err(_) if peer_close == LinkClose::PeerEnded => {
                            lock(&state).view.message = PEER_PAUSED.to_owned();
                        }
                        Err(error) => {
                            let reason = if peer_close == LinkClose::PeerFailed {
                                format!(
                                    "The other computer stopped sharing because of a failure. Its Home says why. [Session: {error:?}]"
                                )
                            } else {
                                session_message(error)
                            };
                            let mut guard = lock(&state);
                            guard.last_failure = Some(format!("Attempt {attempt}: {reason}{load}"));
                            guard.last_failure_at = Some(Instant::now());
                        }
                    }
                    if cancel.is_revoked() {
                        return Ok(());
                    }
                    if !await_native_release().await {
                        return Err(native_cleanup_message().into());
                    }
                    endpoint.reclaim(paired.lease).await;
                    // The saved record can no longer fit, so every dial from here would only wait
                    // for the other computer to reach the same conclusion.
                    if let Some(message) = outgrown {
                        note_displays_changed(&mut lock(&state), message);
                        return Ok(());
                    }
                    window_started = Instant::now();
                    if session_started.elapsed() < STABLE_SESSION {
                        sleep_unless_cancelled(&cancel, RECONNECT_INTERVAL).await;
                    }
                }
            },
        )
    }

    /// Named arrangements for the connected pair; an unconnected app has no pair to list for.
    pub fn arrangements(&self, path: &Path) -> Result<Vec<ArrangementView>, String> {
        let inspection = lock(&self.state).inspection.clone();
        let library = ArrangementLibrary::load(path).map_err(|_| ARRANGEMENTS_UNREADABLE)?;
        Ok(inspection
            .map(|inspection| library.views(&inspection))
            .unwrap_or_default())
    }

    /// Saves a complete, valid layout under a name; the same name replaces the earlier one.
    pub fn save_arrangement(
        &self,
        path: &Path,
        revision: &str,
        name: &str,
        layout: LayoutRequest,
    ) -> Result<Vec<ArrangementView>, String> {
        let setup = self.connected_setup(revision, layout)?;
        let inspected = self.inspection_for_library()?;
        let mut library = ArrangementLibrary::load(path).map_err(|_| ARRANGEMENTS_UNREADABLE)?;
        library
            .upsert(name, setup)
            .map_err(|error| error.message().to_owned())?;
        save_library(path, &library)?;
        Ok(library.views(&inspected))
    }

    fn inspection_for_library(&self) -> Result<InspectedPeer, String> {
        lock(&self.state)
            .inspection
            .clone()
            .ok_or_else(|| "Connect both computers first.".to_owned())
    }

    /// A setup built from the live inspection and the reviewed source, valid for both computers.
    fn connected_setup(
        &self,
        revision: &str,
        layout: LayoutRequest,
    ) -> Result<SharingPreferences, String> {
        let state = lock(&self.state);
        if state.shutdown
            || state.view.busy
            || state.view.phase != "connected"
            || state.view.revision != revision
        {
            return Err("Connect both computers again before saving this layout.".into());
        }
        let side = state
            .source_side
            .ok_or("Choose which computer has the keyboard and mouse.")?;
        let mut inspected = state
            .inspection
            .clone()
            .ok_or("Connect both computers first.")?;
        inspected.source = inspected.device_for(transport_side(side));
        validated_layout(&inspected, &layout)?;
        SharingPreferences::from_inspection(&inspected, layout).map_err(|error| error.to_string())
    }

    pub fn save_setup(
        &self,
        path: &Path,
        revision: &str,
        layout: LayoutRequest,
    ) -> Result<SavedSetupView, String> {
        let preferences = self.connected_setup(revision, layout)?;
        // Disk data is inert. Stop may invalidate the inspection while this write completes.
        self.setup_file
            .write_setup(path, preferences.clone())
            .map_err(|_| {
                "The setup could not be saved. Your previous setup is unchanged.".to_owned()
            })?;
        self.clear_display_notice();
        remember_applied(path, &preferences);
        crate::autostart::setup_applied(path);
        let state = lock(&self.state);
        Ok(SavedSetupView::from_saved(
            Some(&preferences),
            state.inspection.as_ref(),
            &state.view.revision,
        ))
    }

    fn launch<F, Fut>(
        &self,
        phase: &'static str,
        message: &str,
        expected_revision: Option<String>,
        kind: WorkerKind,
        peer: Option<CertificateFingerprint>,
        run: F,
    ) -> Result<SharingView, String>
    where
        F: FnOnce(Arc<Mutex<State>>, u64, u64, RevocationSignal, session::SessionProgress) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + 'static,
    {
        let _operation = lock(&self.operation);
        let is_link = matches!(kind, WorkerKind::Link(_));
        if is_link && lock(&self.state).link.is_some() {
            return Err("Already connected. Stop the connection before connecting again.".into());
        }
        let mut worker = lock(&self.worker);
        if monhop_core::NativeInputOwnership::is_claimed() {
            return Err(
                "Wait for local input cleanup before starting another sharing action.".into(),
            );
        }
        if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Err("Wait for the current sharing action to finish.".into());
        }
        if let Some(previous) = worker.take() {
            let _ = previous.join();
        }
        let mut state = lock(&self.state);
        if state.shutdown {
            return Err("MonHop is shutting down and cannot start sharing.".into());
        }
        if state.view.busy {
            return Err("Wait for the current sharing action to finish.".into());
        }
        if expected_revision.is_some_and(|expected| expected != state.view.revision) {
            return Err("The setup changed. Connect the computers again.".into());
        }
        if is_link {
            invalidate_authorization(&mut state);
        } else {
            advance_authorization(&mut state);
        }
        if state.shutdown {
            return Err("Restart MonHop before starting another session.".into());
        }
        let worker_generation = state
            .worker_generation
            .checked_add(1)
            .ok_or("Restart MonHop before starting another session.")?;
        state.worker_generation = worker_generation;
        let authorization_revision = state.authorization_revision;
        state.view.revision = authorization_revision.to_string();
        state.view.phase = phase;
        state.view.busy = true;
        state.view.sharing_active = false;
        state.view.message = message.to_owned();
        state.view.link = LinkView::default();
        if is_link {
            state.view.sync = SyncView::default();
        }
        state.close_message = None;
        state.peer = peer;
        if let WorkerKind::Link(commands) = kind {
            let now = Instant::now();
            // A link reopened while the user is already arranging must say so from its first
            // connection, or the other computer would read the silence as idle.
            let _ = commands.send(LinkCommand::Arranging(state.editing));
            state.link = Some(commands);
            state.link_since = Some(now);
            state.link_touched = Some(now);
        }
        let cancel = RevocationSignal::default();
        state.cancel = Some(cancel.clone());
        state.native_cancel = None;
        let progress = session::SessionProgress::default();
        state.progress = Some(progress.clone());
        let shared = self.state.clone();
        *worker = Some(
            std::thread::Builder::new()
                .name("monhop-sharing".into())
                .spawn(move || {
                    monhop_transport::session_threads::mark_time_sensitive();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|_| "The sharing worker could not start.".to_owned())?;
                        runtime.block_on(run(
                            shared.clone(),
                            worker_generation,
                            authorization_revision,
                            cancel.clone(),
                            progress,
                        ))
                    }))
                    .unwrap_or_else(|_| {
                        Err("The sharing worker stopped unexpectedly. Input sharing is off.".into())
                    });
                    let mut state = lock(&shared);
                    if state.worker_generation != worker_generation {
                        log::debug!("worker {worker_generation} ended after being superseded");
                        return;
                    }
                    let superseded =
                        !is_link && state.authorization_revision != authorization_revision;
                    match &result {
                        Ok(()) => log::info!(
                            "worker {worker_generation} ({}) ended{}",
                            if is_link { "setup link" } else { "sharing" },
                            state
                                .close_message
                                .as_deref()
                                .map(|message| format!(": {message}"))
                                .unwrap_or_default()
                        ),
                        Err(error) => log::warn!(
                            "worker {worker_generation} ({}) ended with an error: {error}",
                            if is_link { "setup link" } else { "sharing" }
                        ),
                    }
                    invalidate_authorization(&mut state);
                    state.view.busy = false;
                    state.view.sharing_active = false;
                    state.view.link = LinkView::default();
                    if is_link && state.view.sync.state != "applied" {
                        state.view.sync = SyncView::default();
                    }
                    state.cancel = None;
                    state.native_cancel = None;
                    state.link = None;
                    state.link_since = None;
                    state.link_touched = None;
                    state.staged = None;
                    state.peer = None;
                    // Nothing carries the peer's arranging state or a proposal past its link:
                    // the next link reports the one and re-sends the other.
                    state.peer_arranging = false;
                    state.pending_proposal = None;
                    // "Updating" and "the other computer is choosing" both describe an exchange
                    // this link was carrying, so neither may outlive it; a banner that did would
                    // sit there promising a layout nothing is still working on.
                    if matches!(
                        state.display_notice,
                        Some(DisplayNotice::Updating | DisplayNotice::PeerDeciding)
                    ) {
                        clear_notice(&mut state);
                    }
                    if let Some(progress) = state.progress.as_ref() {
                        let stats = progress.diagnostics();
                        state.view.diagnostics = DiagnosticsView {
                            sent_events: stats.sent_events.to_string(),
                            received_events: stats.received_events.to_string(),
                            round_trip_ms: stats.round_trip_micros as f64 / 1000.0,
                            active_display: None,
                            active_is_local: None,
                        };
                    }
                    state.progress = None;
                    state.native_cleanup_pending = monhop_core::NativeInputOwnership::is_claimed();
                    if state.native_cleanup_pending {
                        state.view.phase = "error";
                        state.view.message = native_cleanup_message().into();
                        return;
                    }
                    if state.shutdown {
                        state.view.phase = "off";
                        state.view.message = "MonHop is shutting down. Input is local.".into();
                        return;
                    }
                    // A close the user, the supervisor, or the idle window asked for is never a failure.
                    if let Some(message) = state.close_message.take() {
                        state.view.phase = "off";
                        state.view.message = message;
                        return;
                    }
                    match result {
                        Err(error) if !superseded => {
                            state.view.phase = "error";
                            state.view.message = error;
                            // Only a real failure backs the supervisor off; every step change
                            // above returned before reaching here.
                            state.worker_failed_at = Some(Instant::now());
                        }
                        _ => {
                            state.view.phase = "off";
                            state.view.message = NOT_CONNECTED.into();
                        }
                    }
                })
                .map_err(|_| {
                    state.view.busy = false;
                    state.view.phase = "error";
                    state.cancel = None;
                    state.native_cancel = None;
                    state.link = None;
                    state.link_since = None;
                    state.link_touched = None;
                    state.peer = None;
                    "The sharing worker could not start.".to_owned()
                })?,
        );
        Ok(view_of(&state))
    }
}

/// Shared teardown for Stop and quit: stop the session so its close can leave, ask a live link
/// to close cleanly, and let the cancellation watch revoke the socket after its grace.
fn begin_close(state: &mut State, stopping_message: &str) {
    invalidate_authorization(state);
    // The peer learns from the close reason that this was the user's choice, not a drop.
    if let Some(progress) = state.progress.as_ref() {
        progress.end_deliberately();
    }
    // Retire the link's persistence before anything else: a commit racing this close finds no
    // staged file and a spent epoch, so it can never overwrite the applied setup.
    state.link_epoch = state.link_epoch.wrapping_add(1);
    state.staged = None;
    if let Some(native) = state.native_cancel.as_ref() {
        native.request_stop();
    }
    let link_running = state.link.is_some();
    if let Some(link) = state.link.as_ref() {
        state
            .close_message
            .get_or_insert_with(|| NOT_CONNECTED.to_owned());
        let _ = link.send(LinkCommand::Close);
    }
    if let Some(cancel) = state.cancel.as_ref() {
        cancel.revoke();
    }
    state.view.sharing_active = false;
    state.view.sync = SyncView::default();
    if state.view.busy || link_running {
        state.view.busy = true;
        state.view.phase = "stopping";
        state.view.message = stopping_message.to_owned();
    }
}

/// Applies link events to the view and enforces the idle window without blocking the transport.
async fn pump_link(
    state: Arc<Mutex<State>>,
    generation: u64,
    cancel: RevocationSignal,
    commands: UnboundedSender<LinkCommand>,
    mut events: UnboundedReceiver<LinkEvent>,
    link: LinkFuture,
    idle_window: Duration,
) -> Result<(), String> {
    let mut link = std::pin::pin!(link);
    let mut open = true;
    let mut closing: Option<tokio::time::Instant> = None;
    let mut forced = false;
    loop {
        // Without a closing deadline the loop wakes every second: the editing flag decides
        // whether the idle window applies, and it changes without any link event.
        let deadline = closing
            .unwrap_or_else(|| tokio::time::Instant::now() + LINK_IDLE_POLL.min(idle_window));
        tokio::select! {
            biased;
            event = events.recv(), if open => match event {
                Some(event) => {
                    if apply_link_event(&state, generation, event) && closing.is_none() {
                        closing = Some(tokio::time::Instant::now() + LINK_CLOSE_GRACE);
                    }
                }
                None => open = false,
            },
            result = &mut link => {
                while let Ok(event) = events.try_recv() {
                    apply_link_event(&state, generation, event);
                }
                return match result {
                    Ok(()) => Ok(()),
                    Err(SetupFailure::Cancelled) if cancel.is_revoked() => Ok(()),
                    // The other computer is dialing a session on the same port: a step change,
                    // not a failure. It ends its own dial the same way and opens its link, so
                    // the supervisor reopens this one and the two meet there.
                    Err(SetupFailure::PurposeMismatch) => {
                        log::info!("link: the other computer is starting a session; reopening the link");
                        let mut state = lock(&state);
                        if state.worker_generation == generation {
                            state
                                .close_message
                                .get_or_insert_with(|| PEER_STARTING_SESSION.to_owned());
                        }
                        Ok(())
                    }
                    Err(error) => Err(setup_message(error)),
                };
            }
            () = tokio::time::sleep_until(deadline) => {
                if closing.is_some() {
                    if !forced {
                        cancel.revoke();
                        forced = true;
                    }
                    closing = Some(tokio::time::Instant::now() + LINK_CLOSE_GRACE);
                } else if close_when_idle(&state, generation, idle_window) {
                    let _ = commands.send(LinkCommand::Close);
                    closing = Some(tokio::time::Instant::now() + LINK_CLOSE_GRACE);
                }
            }
        }
    }
}

async fn sleep_unless_cancelled(cancel: &RevocationSignal, wait: Duration) {
    let deadline = tokio::time::Instant::now() + wait;
    while !cancel.is_revoked() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Ends this worker cleanly and leaves the supervisor a reason to open the link instead of dialing
/// a session again. Nothing here is a failure, so there is no `last_failure` and no backoff.
fn open_link_for_layout(state: &mut State, message: &str) {
    state.link_reason = Some(LinkReason::LayoutMisfit);
    state.close_message = Some(message.to_owned());
}

/// A dialed session whose displays no longer fit the saved layout. That is the ordinary answer to
/// a display change, not a failure: the worker ends cleanly, so the supervisor opens the link on
/// its next pass with no backoff and the computer with the keyboard sends a layout that fits.
fn note_layout_misfit(state: &mut State) {
    log::info!(
        "sharing: the displays no longer fit the saved layout; opening the link to arrange them"
    );
    open_link_for_layout(state, LAYOUT_MISFIT);
}

/// The two computers' saved records name different keyboard sides. Answered on the link like a
/// misfit, never by dialing again: the keyboard side proposes one record and both then hold it.
fn note_record_disagreement(state: &mut State) {
    log::info!("sharing: the two computers hold different layouts; opening the link to agree");
    open_link_for_layout(state, RECORDS_DISAGREE);
}

/// A running session the displays outgrew. Same answer as a misfit, for the same reason: the
/// record cannot fit again, so every reconnect wait before the link opens is dead time.
fn note_displays_changed(state: &mut State, message: &str) {
    log::info!("sharing: the displays changed during the session; opening the link to update");
    open_link_for_layout(state, message);
    // Already on screen from the end that classified it; held here so the close cannot drop it.
    state.view.message = message.to_owned();
}

/// Whether a session end means the displays changed rather than that sharing failed, and the
/// message to show for it. Re-dialing cannot help any of these: the saved record no longer
/// describes what is connected, so the worker ends and the link opens on the next pass.
///
/// `local_displays_match` is only consulted for a close the other computer began, and is a native
/// read, so it stays a closure and runs at most once. `None` is a read that failed; displays that
/// could not be read are not displays that changed, so that end keeps its failure arm and backoff.
fn displays_changed_end(
    result: Result<(), SessionFailure>,
    peer_close: LinkClose,
    local_is_source: bool,
    local_displays_match: impl FnOnce() -> Option<bool>,
) -> Option<&'static str> {
    use monhop_transport::session_actor::ActorFailure;
    let role = if local_is_source {
        SOURCE_DISPLAYS_CHANGED
    } else {
        DESTINATION_DISPLAYS_CHANGED
    };
    match result {
        // The receiving side's own native step found different displays than the session began on.
        // Only `Native` proves that: the step on any other reason is read from a process-global
        // counter (`session_native::LAST_DESTINATION_STEP`) that is never reset, so a `Startup`
        // failure after any earlier display change would arrive carrying step 3 or 8 and be
        // laundered into an expected transition, skipping the backoff a real failure needs.
        Err(SessionFailure::DestinationActor(ActorFailure::Native, 3 | 8)) if !local_is_source => {
            Some(role)
        }
        // The sending side's periodic geometry check found the same.
        Err(SessionFailure::InvalidLayout) if local_is_source => Some(role),
        // The other computer closed first and this one's displays no longer match the record it
        // ran with: whatever it says, a session on that record cannot start again.
        _ if matches!(
            peer_close,
            LinkClose::PeerEnded | LinkClose::PeerFailed | LinkClose::PeerClosed
        ) && local_displays_match() == Some(false) =>
        {
            Some(role)
        }
        _ => None,
    }
}

/// Raises Home's banner for `geometry` unless one is already raised for exactly those displays.
fn raise_notice(state: &mut State, kind: DisplayNotice, geometry: DisplayGeometry) -> bool {
    if state
        .notice_geometry
        .as_ref()
        .is_some_and(|raised| raised.same(&geometry))
    {
        return false;
    }
    state.notice_geometry = Some(geometry);
    state.notice_answer = Some(kind);
    state.display_notice = Some(kind);
    // Only the send that follows knows whether its layout lost anything; until then it has not.
    state.notice_left_out = false;
    state.notice_window_pending = true;
    true
}

/// Tells the live link this computer's arranging state; without a link there is nobody to tell.
fn publish_arranging(state: &State, arranging: bool) {
    if let Some(link) = &state.link {
        let _ = link.send(LinkCommand::Arranging(arranging));
    }
}

/// An applied or promoted layout answers the change, so the next one raises the banner again.
fn clear_notice(state: &mut State) {
    state.display_notice = None;
    state.notice_geometry = None;
    state.notice_answer = None;
    state.notice_left_out = false;
}

/// A layout committed over the link answers the display change behind it, so the banner comes
/// down. It stays up, as "sharing continued", only where the user lost something: the computer
/// that chose a layout which had to leave a crossing or a display out. A layout that fit exactly,
/// a user's own Apply, and the computer that only accepted the other's choice all leave none. The
/// geometry memory always goes, so the next change is judged afresh.
fn resolve_notice_on_commit(state: &mut State) {
    let continued = state.display_notice == Some(DisplayNotice::Updating) && state.notice_left_out;
    clear_notice(state);
    if continued {
        state.display_notice = Some(DisplayNotice::Continued);
    }
}

/// Remembers an applied record for the displays it was made with; an identical memory is left
/// alone. `setup` is already validated, and a library problem never fails the Apply behind it.
pub(crate) fn remember_applied(setup_path: &Path, setup: &SharingPreferences) {
    let path = setup_path.with_file_name(ARRANGEMENTS_FILE);
    let Ok(mut library) = ArrangementLibrary::load(&path) else {
        log::warn!("arrangements: the applied layout was not remembered; the library is damaged");
        return;
    };
    match library.remember_automatically(setup.clone()) {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            log::warn!(
                "arrangements: the applied layout was not remembered: {}",
                error.message()
            );
            return;
        }
    }
    if let Err(message) = save_library(&path, &library) {
        log::warn!("arrangements: the applied layout was not remembered: {message}");
    }
}

pub(crate) fn save_library(path: &Path, library: &ArrangementLibrary) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| "The setup folder could not be created.".to_owned())?;
    }
    library.save(path).map_err(|_| {
        "The arrangement could not be saved. Your saved arrangements are unchanged.".to_owned()
    })
}

/// Native ownership must be back before the next session claims it; a stuck release fails closed.
async fn await_native_release() -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while monhop_core::NativeInputOwnership::is_claimed() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// The active computer's record on disk, or the reason there is none to test or share with.
fn active_setup(path: &Path) -> Result<SharingPreferences, String> {
    let file = SetupFile::load(path)
        .map_err(|_| "The saved setup could not be read. Apply a layout again.".to_owned())?;
    if file.active().is_none() {
        return Err("Choose a computer on Home first.".into());
    }
    file.active_computer()
        .cloned()
        .ok_or_else(|| "Apply a layout on both computers first.".to_owned())
}

/// Whether this computer still shows the displays the record was made with. Display
/// identifiers carry the owning computer, so the record's own local identity is used.
pub(crate) fn local_displays_fit(saved: &SharingPreferences) -> Result<bool, String> {
    Ok(saved.matches_local_displays(&current_local_displays(saved)?))
}

/// This computer's displays, read as the record's own computer identity names them.
pub(crate) fn current_local_displays(
    saved: &SharingPreferences,
) -> Result<session_setup::DisplayTopology, String> {
    let local_device = CertificateFingerprint::parse_full(saved.local_fingerprint())
        .map(monhop_transport::session_handshake::device_id_from_fingerprint)
        .map_err(|_| "The saved setup is damaged. Apply a layout again.".to_owned())?;
    session_native::current_displays(local_device)
        .map_err(|_| "Current display information could not be read.".to_owned())
}

impl SharingPreferences {
    fn peer(&self) -> Result<CertificateFingerprint, String> {
        CertificateFingerprint::parse_full(self.peer_fingerprint())
            .map_err(|_| "The saved setup is damaged. Apply a layout again.".to_owned())
    }
}

/// Only arranging has an idle window; a standing link stays up until the supervisor ends it.
fn close_when_idle(shared: &Arc<Mutex<State>>, generation: u64, idle_window: Duration) -> bool {
    let mut state = lock(shared);
    // A close the user already asked for keeps its own wording.
    if state.worker_generation != generation
        || !state.editing
        || state.close_message.is_some()
        || !state
            .link_touched
            .is_some_and(|touched| touched.elapsed() >= idle_window)
    {
        return false;
    }
    state.editing = false;
    state.close_message = Some(IDLE_CLOSED.to_owned());
    state.view.sync = SyncView::default();
    close_link(&mut state, IDLE_CLOSED);
    true
}

/// Asks the live link to close with `message` as the view's wording meanwhile. An applied
/// sync outcome stays visible through the close.
fn close_link(state: &mut State, message: &str) {
    state.view.phase = "stopping";
    state.view.busy = true;
    state.view.message = message.to_owned();
    if let Some(link) = &state.link {
        let _ = link.send(LinkCommand::Close);
    }
}

/// Returns true once the link reports it has closed.
fn apply_link_event(shared: &Arc<Mutex<State>>, generation: u64, event: LinkEvent) -> bool {
    let mut state = lock(shared);
    if state.worker_generation != generation || state.shutdown {
        return matches!(event, LinkEvent::Closed);
    }
    match event {
        LinkEvent::Connecting { attempt } => {
            state.view.link.attempt = attempt;
            state.view.busy = true;
            state.view.phase = if state.inspection.is_some() {
                "reconnecting"
            } else {
                "connecting"
            };
            state.view.message = "Connecting to the other computer.".into();
        }
        LinkEvent::Connected { inspection } => {
            adopt_inspection(&mut state, inspection);
            // Both computers are on the link now, so the reason that chose a link over a session
            // is spent and the ordinary decision path runs again.
            if state.link_reason == Some(LinkReason::PeerHoldsLink) {
                state.link_reason = None;
            }
            state.view.message = connected_message(state.link_reason).into();
        }
        LinkEvent::PeerArranging { arranging } => {
            state.peer_arranging = arranging;
            if !state.editing && state.view.phase == "connected" {
                state.view.message = if arranging {
                    CONNECTED_PEER_ARRANGING.to_owned()
                } else {
                    connected_message(state.link_reason).to_owned()
                };
            }
        }
        LinkEvent::TopologyChanged { inspection } => {
            adopt_inspection(&mut state, inspection);
            state.view.synchronized_layout = None;
            state.view.message =
                "The displays changed. Check the arrangement, then apply the layout.".into();
        }
        LinkEvent::SyncStarted { sending } => {
            let (name, message) = if sending {
                ("sending", "Applying the layout on both computers.")
            } else {
                ("receiving", "The other computer is applying a layout.")
            };
            state.view.sync = SyncView {
                state: name,
                message: message.to_owned(),
            };
            state.view.message = message.to_owned();
        }
        LinkEvent::SyncCompleted {
            inspection,
            bytes,
            sending,
        } => match sharing_preferences::shared_setup_for_inspection(&inspection, &bytes) {
            Ok((decoded, _, layout)) => {
                state.source_side = Some(if decoded.source == decoded.local_device {
                    SourceSide::Local
                } else {
                    SourceSide::Peer
                });
                state.view.local_displays = display_views(&decoded.local_displays);
                state.view.peer_displays = display_views(&decoded.peer_displays);
                state.view.synchronized_layout = Some(layout);
                state.inspection = Some(decoded);
                advance_authorization(&mut state);
                state.view.phase = "connected";
                state.view.busy = false;
                state.view.sync = SyncView {
                    state: "applied",
                    message: LAYOUT_APPLIED_SHARING_ON.into(),
                };
                state.view.message = LAYOUT_APPLIED_SHARING_ON.into();
                state.editing = false;
                state.link_reason = None;
                state.pending_proposal = None;
                state.sharing_role = Some(if state.source_side == Some(SourceSide::Local) {
                    "sends"
                } else {
                    "receives"
                });
                // The sender commits last, so only it may close; the receiver waits for that close.
                if sending {
                    close_after_apply(&mut state);
                }
            }
            Err(_) => reject_sync(&mut state, LinkRejectReason::Invalid, sending),
        },
        LinkEvent::SyncRejected { reason, sending } => reject_sync(&mut state, reason, sending),
        LinkEvent::Disconnected { .. } if state.view.sync.state == "applied" => {
            // The setup is done on both computers; the peer leaving frees the port for sharing.
            close_after_apply(&mut state);
        }
        // A standing link that lost its peer ends, so the supervisor can choose a session or a
        // fresh link; only an arranging link reconnects by itself.
        LinkEvent::Disconnected { .. } if !state.editing && state.view.phase == "connected" => {
            state
                .close_message
                .get_or_insert_with(|| PEER_LEFT.to_owned());
            close_link(&mut state, PEER_LEFT);
        }
        LinkEvent::Disconnected { .. } => {
            advance_authorization(&mut state);
            state.view.phase = "reconnecting";
            state.view.busy = true;
            state.view.sync = SyncView::default();
            state.view.message = "Lost the connection to the other computer. Reconnecting.".into();
        }
        LinkEvent::Closed => {
            let message = state
                .close_message
                .get_or_insert_with(|| NOT_CONNECTED.to_owned())
                .clone();
            if state.view.sync.state != "applied" {
                state.view.sync = SyncView::default();
            }
            if state.view.phase != "stopping" {
                state.view.phase = "off";
                state.view.busy = false;
                state.view.message = message;
            }
            return true;
        }
    }
    false
}

/// Geometry may have changed with every connect, so the previous authorization is spent.
fn adopt_inspection(state: &mut State, mut inspection: InspectedPeer) {
    if let Some(side) = state.source_side {
        inspection.source = inspection.device_for(transport_side(side));
    }
    state.view.local_displays = display_views(&inspection.local_displays);
    state.view.peer_displays = display_views(&inspection.peer_displays);
    state.inspection = Some(inspection);
    advance_authorization(state);
    state.view.phase = "connected";
    state.view.busy = false;
    state.view.sync = SyncView::default();
}

/// A refusal that follows this computer's own commit means the pair no longer agrees on disk.
/// Ends the link deliberately once both computers hold the layout; the supervisor then shares.
fn close_after_apply(state: &mut State) {
    state
        .close_message
        .get_or_insert_with(|| LAYOUT_APPLIED_SHARING_ON.to_owned());
    close_link(state, LAYOUT_APPLIED_SHARING_ON);
}

fn reject_sync(state: &mut State, reason: LinkRejectReason, sending: bool) {
    // A supervisor proposal the other computer refused stops promising an update; the geometry
    // memory stays, so the same change is never proposed twice.
    state.pending_proposal = None;
    if state.notice_answer == Some(DisplayNotice::Updating) {
        state.notice_answer = Some(DisplayNotice::Waiting);
        if state.display_notice == Some(DisplayNotice::Updating) {
            state.display_notice = Some(DisplayNotice::Waiting);
        }
    }
    let disagreed =
        !sending && reason == LinkRejectReason::SaveFailed && state.view.sync.state == "applied";
    let message = if disagreed {
        ONLY_THIS_COMPUTER_SAVED
    } else {
        reject_message(reason)
    }
    .to_owned();
    if disagreed {
        // Both computers must hold the same layout before either shares; one saved copy is not
        // enough, so the supervisor keeps a link up until a fresh Apply lands on both.
        state.link_reason = Some(LinkReason::LayoutUnconfirmed);
        if state.close_message.is_some() {
            state.close_message = Some(message.clone());
        }
    }
    state.view.sync = SyncView {
        state: "rejected",
        message: message.clone(),
    };
    state.view.message = message;
}

const fn connected_message(reason: Option<LinkReason>) -> &'static str {
    match reason {
        None => CONNECTED_ARRANGE,
        Some(LinkReason::LayoutMisfit) => CONNECTED_LAYOUT_STALE,
        Some(LinkReason::LayoutUnconfirmed) => ONLY_THIS_COMPUTER_SAVED,
        Some(LinkReason::PeerHoldsLink) => CONNECTED_ARRANGE,
    }
}

const ONLY_THIS_COMPUTER_SAVED: &str =
    "This computer saved the layout, but the other computer could not. Apply again.";

const fn reject_message(reason: LinkRejectReason) -> &'static str {
    match reason {
        LinkRejectReason::Invalid => "The other computer could not use this layout.",
        LinkRejectReason::InspectionChanged => "The displays changed. Arrange again.",
        LinkRejectReason::SaveFailed => "The other computer could not save the layout.",
        LinkRejectReason::Busy => {
            "The other computer applied a layout first. Review it, then apply again if you want to change it."
        }
        LinkRejectReason::Cancelled => "The layout was not applied.",
    }
}

/// Both computers write the same agreed bytes: stage to a temporary file, then rename on commit.
///
/// Claiming an epoch here binds every step to this one link, so a Stop midway leaves the applied
/// setup alone even when the transport asks to commit afterwards.
fn link_persist(
    state: Arc<Mutex<State>>,
    generation: u64,
    path: PathBuf,
    setup_file: Arc<SetupFileLock>,
) -> LinkPersist {
    let epoch = {
        let mut claimed = lock(&state);
        claimed.link_epoch = claimed.link_epoch.wrapping_add(1);
        claimed.link_epoch
    };
    let commit_state = Arc::clone(&state);
    let commit_path = path.clone();
    let discard_state = Arc::clone(&state);
    LinkPersist {
        stage: Arc::new(move |fresh, bytes| {
            stage_shared_setup(&state, generation, epoch, &path, fresh, bytes)
        }),
        commit: Arc::new(move |_, _| {
            // The state lock is released before the write: disk work must never block the UI thread.
            let mut applied = None;
            setup_file.commit(&commit_path, || {
                let mut state = lock(&commit_state);
                if link_retired(&state, generation, epoch) {
                    return Err(LinkRejectReason::Cancelled);
                }
                let staged = state.staged.take().ok_or(LinkRejectReason::SaveFailed)?;
                applied = Some(staged.clone());
                Ok(staged)
            })?;
            if let Some(applied) = applied {
                resolve_notice_on_commit(&mut lock(&commit_state));
                remember_applied(&commit_path, &applied);
                crate::autostart::setup_applied(&commit_path);
            }
            Ok(())
        }),
        discard: Arc::new(move || {
            lock(&discard_state).staged = None;
        }),
    }
}

/// True once shutdown, a Stop, or a newer link spent the epoch this persistence belongs to.
fn link_retired(state: &State, generation: u64, epoch: u64) -> bool {
    state.shutdown || state.worker_generation != generation || state.link_epoch != epoch
}

fn stage_shared_setup(
    shared: &Arc<Mutex<State>>,
    generation: u64,
    epoch: u64,
    path: &Path,
    fresh: &InspectedPeer,
    bytes: &[u8],
) -> Result<(), LinkRejectReason> {
    let (decoded, preferences, _) = sharing_preferences::shared_setup_for_inspection(fresh, bytes)
        .map_err(|error| match error {
            PreferenceError::InspectionChanged => LinkRejectReason::InspectionChanged,
            PreferenceError::Invalid => LinkRejectReason::Invalid,
        })?;
    {
        let state = lock(shared);
        if link_retired(&state, generation, epoch) {
            return Err(LinkRejectReason::Cancelled);
        }
        if state
            .inspection
            .as_ref()
            .is_some_and(|current| !same_computers_and_displays(current, &decoded))
        {
            return Err(LinkRejectReason::InspectionChanged);
        }
    }
    // The file must be readable now: a damaged file would fail the commit after the peer saved.
    SetupFile::load(path).map_err(|_| LinkRejectReason::SaveFailed)?;
    let mut state = lock(shared);
    if link_retired(&state, generation, epoch) {
        return Err(LinkRejectReason::Cancelled);
    }
    state.staged = Some(preferences);
    Ok(())
}

/// The proposal may change which computer supplies input, so the source is deliberately excluded.
fn same_computers_and_displays(current: &InspectedPeer, decoded: &InspectedPeer) -> bool {
    current.local_fingerprint == decoded.local_fingerprint
        && current.peer_fingerprint == decoded.peer_fingerprint
        && current.local_device == decoded.local_device
        && current.peer_device == decoded.peer_device
        && current.local_platform == decoded.local_platform
        && current.peer_platform == decoded.peer_platform
        && current.interface_id == decoded.interface_id
        && current
            .local_displays
            .same_geometry(&decoded.local_displays)
        && current.peer_displays.same_geometry(&decoded.peer_displays)
}

impl Drop for SharingController {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}
/// Lets a finished session's QUIC close reach the other computer before the endpoint is dropped,
/// so it learns the session ended instead of waiting out its health deadline.
/// Waits for the session's QUIC close to leave without retiring the endpoint: the standing
/// share endpoint serves the next attempt on the same socket.
async fn flush_session_close(lease: &session_setup::EndpointLease) {
    match tokio::time::timeout(CLOSE_FLUSH, lease.endpoint().wait_idle()).await {
        Ok(Ok(())) => log::debug!("session close delivered"),
        Ok(Err(_)) => log::debug!("session close cut short by revocation"),
        Err(_) => log::debug!("session close flush ended after {CLOSE_FLUSH:?}"),
    }
}

fn register_native_cancellation(
    state: &mut State,
    worker_generation: u64,
    authorization_revision: u64,
    cancel: &RevocationSignal,
    native: RevocationSignal,
) -> Result<(), String> {
    if state.worker_generation != worker_generation
        || state.authorization_revision != authorization_revision
        || state.shutdown
        || cancel.is_revoked()
        || native.is_revoked()
    {
        native.mark_revoked_without_wake();
        return Err("Sharing cancelled.".into());
    }
    // Stop sees the native latch before construction, without waiting for a relay or peer.
    state.native_cancel = Some(native);
    Ok(())
}

fn touch_link(state: &mut State) {
    if state.link.is_some() {
        state.link_touched = Some(Instant::now());
    }
}

/// Platform and side fields are derived here so no call site can publish a stale pairing.
fn view_of(state: &State) -> SharingView {
    let mut view = state.view.clone();
    view.link.since = state
        .link_since
        .map(|since| human_duration(since.elapsed()))
        .unwrap_or_default();
    view.source_side = state.source_side.map(side_name);
    view.local_platform = state
        .inspection
        .as_ref()
        .map_or_else(local_platform_name, |inspection| {
            platform_name(inspection.local_platform)
        });
    view.peer_platform = state
        .inspection
        .as_ref()
        .map(|inspection| platform_name(inspection.peer_platform));
    view.source_platform = match state.source_side {
        Some(SourceSide::Local) => Some(view.local_platform),
        Some(SourceSide::Peer) => view.peer_platform,
        None => None,
    };
    view.peer_fingerprint = state.peer.map(|peer| fingerprint_key(&peer.full_hex()));
    view.active = state.active.clone();
    view.editing = state.editing;
    view.display_notice = state.display_notice;
    view.sharing_role = state.sharing_role;
    view.last_failure = drop_note(
        state.last_failure.as_deref(),
        state.last_failure_at.map(|at| at.elapsed()),
    );
    view
}

fn drop_note(reason: Option<&str>, since: Option<Duration>) -> String {
    match (reason, since) {
        (Some(reason), Some(since)) => match human_duration(since).as_str() {
            "just now" => format!("Last drop just now. {reason}"),
            elapsed => format!("Last drop {elapsed} ago. {reason}"),
        },
        (Some(reason), None) => reason.to_owned(),
        (None, _) => String::new(),
    }
}

fn human_duration(elapsed: Duration) -> String {
    let minutes = elapsed.as_secs() / 60;
    match (minutes / 60, minutes) {
        (0, 0) => "just now".to_owned(),
        (0, 1) => "1 minute".to_owned(),
        (0, minutes) => format!("{minutes} minutes"),
        (1, _) => "1 hour".to_owned(),
        (hours, _) => format!("{hours} hours"),
    }
}

fn advance_authorization(state: &mut State) {
    match state.authorization_revision.checked_add(1) {
        Some(revision) => {
            state.authorization_revision = revision;
            state.view.revision = revision.to_string();
        }
        None => {
            state.shutdown = true;
            state.view.revision = "invalid".into();
        }
    }
}
fn invalidate_authorization(state: &mut State) {
    advance_authorization(state);
    state.inspection = None;
    state.source_side = None;
    state.view.local_displays.clear();
    state.view.peer_displays.clear();
    state.view.synchronized_layout = None;
}

fn display_views(topology: &session_setup::DisplayTopology) -> Vec<DisplayView> {
    topology
        .displays()
        .iter()
        .map(|display| DisplayView {
            id: display.id.0.to_string(),
            name: display.name.clone(),
            origin: [display.logical_origin.x, display.logical_origin.y],
            size: [display.logical_size.x, display.logical_size.y],
            scale: display.scale_factor,
            primary: display.is_primary,
            monitor: monitor_key(display),
        })
        .collect()
}

/// Two displays with the same key are one physical monitor cabled to both computers.
pub(crate) fn monitor_key(display: &session_setup::DisplayDescription) -> Option<String> {
    display.monitor.map(|monitor| {
        format!(
            "{:04x}-{:04x}-{:08x}",
            monitor.vendor, monitor.product, monitor.serial
        )
    })
}
/// Steps: 1 revoked before start, 2/7 displays unreadable, 3/8 displays changed, 4 injector,
/// 5 revoked, 6 permission or desktop, 9 revoked mid-input, 10-14 move/key/button/scroll/release.
fn destination_actor_message(
    reason: monhop_transport::session_actor::ActorFailure,
    step: u8,
) -> &'static str {
    use monhop_transport::session_actor::ActorFailure;
    match (reason, step) {
        (ActorFailure::Native | ActorFailure::Startup, 3 | 8) => {
            "This computer's displays changed since the session started. Sharing reconnects with the current layout."
        }
        (ActorFailure::Native | ActorFailure::Startup, 4 | 6) => {
            "This computer cannot inject input. Check Accessibility in System Settings, then turn sharing off and on."
        }
        (ActorFailure::Native, 10..=14) => {
            "This computer could not inject an event. Input stays local until the session reconnects."
        }
        (ActorFailure::PeerGone, _) => "The other computer stopped sending.",
        (ActorFailure::QueueFull, _) => "Input arrived faster than this computer could apply it.",
        (ActorFailure::Receiver, 21) => {
            "The other computer did not answer this computer's health checks for half a second."
        }
        (ActorFailure::Receiver, 20 | 22 | 23) => {
            "This computer's health check of the other computer lost track of its replies."
        }
        (ActorFailure::Receiver, 24 | 25) => "The other computer sent input out of order.",
        (ActorFailure::Receiver, 26) => {
            "The other computer sent input faster than this computer accepts."
        }
        (ActorFailure::Receiver, 27) => {
            "The other computer pointed at a display this computer does not have. Change layout and apply again."
        }
        (ActorFailure::Receiver, 28) => {
            "The other computer moved the pointer outside this computer's displays."
        }
        (ActorFailure::Receiver, 29) => {
            "The other computer sent keys or buttons before moving the pointer here."
        }
        (ActorFailure::Receiver, 30) => {
            "The other computer's key or button state did not match what this computer had seen."
        }
        (ActorFailure::Receiver, 31) => {
            "The other computer sent a message this computer did not expect."
        }
        (ActorFailure::Receiver, 32) => {
            "This computer could not inject an event. Input stays local until the session reconnects."
        }
        (ActorFailure::Receiver, 33) => "The other computer ended the session.",
        (ActorFailure::Receiver, _) => {
            "The other computer sent input this computer could not order."
        }
        _ => "Receiving stopped after a connection or input-state failure.",
    }
}

/// Receiving-side load at the end of a session, for the drop line; counts and timings only.
fn receiver_load_note(stats: &monhop_transport::session::SessionDiagnostics) -> String {
    let Some(load) = stats.receiver else {
        return String::new();
    };
    let age =
        |millis: Option<u64>| millis.map_or_else(|| "none".to_owned(), |age| format!("{age} ms"));
    let ping = age(load.ping_age_millis);
    let frame = age(load.frame_age_millis);
    format!(
        " [Receiver: {} frames, burst {}, batches 1:{} 2-3:{} 4-8:{} 9+:{}, slow applies {}, apply {} ms, checks {} ms, reply {} ms, pending ping {ping}, last frame {frame}, gap max {} ms, holds {} (max {} ms), rtt {:.1} ms]",
        load.frames,
        load.batch_max,
        load.batch_buckets[0],
        load.batch_buckets[1],
        load.batch_buckets[2],
        load.batch_buckets[3],
        load.slow_applies,
        load.apply_max_micros as f64 / 1000.0,
        load.env_max_micros as f64 / 1000.0,
        load.reply_age_millis,
        load.gap_max_millis,
        load.holds,
        load.held_max_millis,
        stats.round_trip_micros as f64 / 1000.0,
    )
}

/// Transport facts at the end of a session, for the drop line; counts and timings only.
fn link_note(stats: &monhop_transport::session::SessionDiagnostics) -> String {
    let Some(link) = stats.link else {
        return String::new();
    };
    let close = match link.close {
        LinkClose::Open => "still open",
        LinkClose::PeerEnded => "the other computer ended it",
        LinkClose::PeerFailed => "the other computer's session failed",
        LinkClose::PeerRevoked => "the other computer's network changed",
        LinkClose::PeerClosed => "the other computer closed it",
        LinkClose::IdleTimeout => "idle timeout",
        LinkClose::Local => "closed here",
        LinkClose::Transport => "transport error",
    };
    let revoked = link
        .revoked_at
        .map(|at| format!(", revoked at {}:{}", at.file(), at.line()))
        .unwrap_or_default();
    format!(
        " [Link: {:.1} s, loop pause max {:.1} ms, out {}/{} written, rtt {:.1} ms, lost {}/{} packets, in {} datagrams, congestion {}, holds {} (max {} ms), close: {close}{revoked}]",
        link.session_millis as f64 / 1000.0,
        link.max_tick_gap_micros as f64 / 1000.0,
        link.written_frames,
        link.queued_frames,
        link.rtt_micros as f64 / 1000.0,
        link.lost_packets,
        link.sent_packets,
        link.received_datagrams,
        link.congestion_events,
        link.holds,
        link.held_max_millis,
    )
}

fn native_cleanup_message() -> &'static str {
    "Sharing stopped, but native input cleanup needs attention. Do not start another session until held input is released."
}
fn set_idle_message(state: &mut State, shutdown: bool) {
    state.native_cleanup_pending = monhop_core::NativeInputOwnership::is_claimed();
    if state.native_cleanup_pending {
        state.view.phase = "error";
        state.view.message = native_cleanup_message().into();
    } else {
        state.view.phase = "off";
        state.view.message = if shutdown {
            "MonHop is shutting down. Input is local."
        } else {
            NOT_CONNECTED
        }
        .into();
    }
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn platform_name(value: Platform) -> &'static str {
    match value {
        Platform::Windows => "windows",
        Platform::MacOs => "macos",
    }
}
pub(crate) fn parse_display(value: &str) -> Result<DisplayId, String> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Choose an inspected display.".into());
    }
    value
        .parse::<u64>()
        .map(DisplayId)
        .map_err(|_| "Choose an inspected display.".into())
}
fn parse_edge(value: &str) -> Result<Edge, String> {
    match value {
        "left" => Ok(Edge::Left),
        "right" => Ok(Edge::Right),
        "top" => Ok(Edge::Top),
        "bottom" => Ok(Edge::Bottom),
        _ => Err("Choose a display edge.".into()),
    }
}
pub(crate) fn parse_link(link: &LinkRequest) -> Result<EdgeLink, String> {
    EdgeLink::new(
        parse_display(&link.from_display)?,
        parse_edge(&link.from_edge)?,
        NormalizedSpan::new(link.from_span[0], link.from_span[1])
            .map_err(|_| "Choose a valid source edge span.")?,
        parse_display(&link.to_display)?,
        parse_edge(&link.to_edge)?,
        NormalizedSpan::new(link.to_span[0], link.to_span[1])
            .map_err(|_| "Choose a valid destination edge span.")?,
        link.hysteresis,
    )
    .map_err(|_| "This crossing has invalid display geometry.".into())
}
pub(crate) fn validated_layout(
    inspected: &InspectedPeer,
    layout: &LayoutRequest,
) -> Result<(DisplayId, monhop_core::Topology), String> {
    if layout.links.len() > 64 {
        return Err("The layout supports at most 64 directed edges.".into());
    }
    let initial_display = parse_display(&layout.source_display)?;
    let links = layout
        .links
        .iter()
        .map(parse_link)
        .collect::<Result<Vec<_>, _>>()?;
    let ids: Vec<(String, bool)> = inspected
        .local_displays
        .displays()
        .iter()
        .map(|display| (display.id.0.to_string(), true))
        .chain(
            inspected
                .peer_displays
                .displays()
                .iter()
                .map(|d| (d.id.0.to_string(), false)),
        )
        .collect();
    let arranged: Vec<ArrangedDisplay<'_>> = ids
        .iter()
        .map(|(id, local)| ArrangedDisplay { id, local: *local })
        .collect();
    validate_arrangement(layout, &arranged)?;
    let hidden: Vec<DisplayId> = layout
        .arrangement
        .iter()
        .flat_map(|arrangement| arrangement.hidden.iter())
        .map(|id| parse_display(id))
        .collect::<Result<_, _>>()?;
    // Placed one by one, the picture is where the other computer's displays sit; grouped keeps its OS layout.
    let placed: Vec<(DisplayId, monhop_core::Point)> = layout
        .arrangement
        .iter()
        .filter(|arrangement| arrangement.mode == "free")
        .flat_map(|arrangement| arrangement.positions.iter())
        .map(|position| {
            parse_display(&position.display)
                .map(|id| (id, monhop_core::Point::new(position.x, position.y)))
        })
        .collect::<Result<_, _>>()?;
    let topology = inspected
        .topology(links, &hidden, &placed)
        .map_err(setup_message)?;
    if topology
        .display(initial_display)
        .map_err(|_| "Choose a source display.")?
        .machine
        != inspected.source
    {
        return Err(
            "The starting display must belong to the computer with the keyboard and mouse.".into(),
        );
    }
    if !topology.links().iter().any(|link| {
        topology
            .display(link.from_display)
            .is_ok_and(|d| d.machine == inspected.source)
            && topology
                .display(link.to_display)
                .is_ok_and(|d| d.machine != inspected.source)
    }) {
        return Err("Add a crossing from the keyboard's computer to the other computer.".into());
    }
    validate_return_paths(&topology, inspected.source)?;
    Ok((initial_display, topology))
}

fn validate_return_paths(
    topology: &monhop_core::Topology,
    source: monhop_core::DeviceId,
) -> Result<(), String> {
    let mut reachable: Vec<_> = topology
        .displays()
        .filter(|display| display.machine == source)
        .map(|display| display.id)
        .collect();
    let mut index = 0;
    while let Some(display) = reachable.get(index).copied() {
        for link in topology
            .links()
            .iter()
            .filter(|link| link.from_display == display)
        {
            if !reachable.contains(&link.to_display) {
                reachable.push(link.to_display);
            }
        }
        index += 1;
    }
    for remote in reachable.into_iter().filter(|id| {
        topology
            .display(*id)
            .is_ok_and(|display| display.machine != source)
    }) {
        let mut visited = vec![remote];
        let mut index = 0;
        let mut returns = false;
        while let Some(display) = visited.get(index).copied() {
            if topology
                .display(display)
                .is_ok_and(|display| display.machine == source)
            {
                returns = true;
                break;
            }
            for link in topology
                .links()
                .iter()
                .filter(|link| link.from_display == display)
            {
                if !visited.contains(&link.to_display) {
                    visited.push(link.to_display);
                }
            }
            index += 1;
        }
        if !returns {
            return Err("Add a return crossing from every reachable remote display to the keyboard's computer.".into());
        }
    }
    Ok(())
}
/// Attempt failures while the switch is on describe the peer's state, not a broken setup.
fn share_wait_message(error: SetupFailure, waited: Duration) -> String {
    let minutes = waited.as_secs() / 60;
    let waited = if minutes == 0 {
        "under a minute".to_owned()
    } else if minutes == 1 {
        "1 minute".to_owned()
    } else {
        format!("{minutes} minutes")
    };
    let reason = match error {
        SetupFailure::Connection => "The other computer has not answered yet.".to_owned(),
        SetupFailure::Handshake => {
            "The other computer answered but is in a different step or on a different build. Open MonHop there with the same version.".to_owned()
        }
        SetupFailure::PurposeMismatch => {
            "The other computer is arranging displays. Sharing resumes when it applies the layout.".to_owned()
        }
        SetupFailure::PortBusy => {
            "This computer's MonHop port was still closing from the last session.".to_owned()
        }
        other => setup_message(other),
    };
    format!("Waiting for the other computer ({waited}). {reason}")
}

fn setup_message(error: SetupFailure) -> String {
    match error{SetupFailure::Identity=>"The protected identity could not be read. Check pairing.",SetupFailure::PairingRequired=>"Pair both computers before setting up sharing.",SetupFailure::NetworkSelection=>"Select a connected physical Wi-Fi or Ethernet interface.",SetupFailure::NetworkRoute=>"The peer must use the selected direct physical network. Check for a VPN route or changed address.",SetupFailure::PortBusy=>"The MonHop port on this computer is still closing. Try again in a moment.",SetupFailure::Connection=>"The connection did not finish. Start setup on macOS, then try Windows again.",SetupFailure::Handshake=>"The computers did not agree on identity, version, or which computer supplies input. Check both builds and source choices.",SetupFailure::Cancelled=>"The connection was cancelled or its physical network changed.",SetupFailure::Displays=>"Current display information could not be read.",SetupFailure::ChangedSinceInspection=>"The identity, network, or displays changed. Inspect the computers again.",SetupFailure::Layout=>"The display layout is invalid.",SetupFailure::LayoutSyncConflict=>"Choose Send setup on one computer and Use other computer’s setup on the other. Do not send from both.",SetupFailure::LayoutSyncIncomplete=>"Setup sync was not confirmed on both computers. A local copy may have been saved. Try syncing again. Sharing stays off.",SetupFailure::PurposeMismatch=>"The other computer is sharing input while this one arranges displays. Choose Change layout there too, or apply here.",SetupFailure::VersionMismatch=>"The other computer runs a different MonHop version. Install the same build on both computers."}.into()
}
fn session_message(error: SessionFailure) -> String {
    match error {
        SessionFailure::TrialShortcut => "System shortcuts are not part of this controlled test.",
        SessionFailure::NativeCleanup => native_cleanup_message(),
        SessionFailure::Startup(reason) => return startup_message(reason),
        SessionFailure::NativeCaptureStartup(reason) => return capture_startup_message(reason),
        SessionFailure::NativeStartup => {
            "Input setup could not finish. Turn sharing off and on."
        }
        SessionFailure::Native => "Sharing stopped because native input became unavailable.",
        SessionFailure::Revoked => "Sharing stopped because the selected connection was revoked.",
        SessionFailure::InvalidLayout => "Sharing could not start with this display layout.",
        SessionFailure::DestinationActor(reason, step) => {
            return format!(
                "{} [Session: DestinationActor({reason:?}) step {step}]",
                destination_actor_message(reason, step)
            );
        }
        SessionFailure::Wire => {
            return "The connection to the other computer ended. Its Home says why. [Session: Wire]"
                .to_owned();
        }
        SessionFailure::UnexpectedStream => {
            return "The other computer sent data this session does not use. [Session: UnexpectedStream]"
                .to_owned();
        }
        SessionFailure::SourceController(reason) => {
            return format!(
                "{} [Session: {error:?}]",
                source_controller_message(reason)
            )
        }
        _ => {
            return format!(
                "Sharing stopped after a connection or input-state failure. [Session: {error:?}]"
            )
        }
    }
    .into()
}

fn source_controller_message(reason: SourceFailure) -> &'static str {
    match reason {
        SourceFailure::Topology => {
            "The pointer reached a place the applied layout does not cover. Apply the layout again on both computers, then retry."
        }
        SourceFailure::PeerHealth(_) => {
            "The other computer did not answer this computer's health checks for half a second. Its Home says why."
        }
        SourceFailure::InvalidKeyState | SourceFailure::InvalidCapturedInput => {
            "A key or button state did not match what MonHop tracked. Release every key and button, then retry."
        }
        SourceFailure::RateLimited => {
            "Input arrived faster than the test allows. Move more slowly, then retry."
        }
        _ => "Input routing lost sync with native capture. Turn sharing off and on.",
    }
}

fn startup_message(reason: SessionStartupFailure) -> String {
    use SessionStartupFailure as Failure;
    let explanation = match reason {
        Failure::PeerHealth(_) => {
            "The connection check failed before input started. Sharing reconnects on its own."
        }
        Failure::Deadline => {
            "Both computers did not become ready in time. Its Home says why. Sharing reconnects on its own."
        }
        Failure::NotReady
        | Failure::Sequence
        | Failure::UnexpectedMessage
        | Failure::RateLimited => {
            "The computers disagreed during input setup. Check both have the latest build, then reopen the test."
        }
        Failure::DisplayUnavailable => {
            "Display information could not be read. Close the test and check the display arrangement again."
        }
        Failure::NativeOwnershipUnavailable => {
            "Another local input task is still running or cleaning up. Stop it before retrying."
        }
        Failure::ReceiverUnavailable => {
            "The receiving computer could not prepare native input. Check its Home and permissions."
        }
        Failure::CaptureEnded | Failure::CaptureStopped(_) => {
            "Input capture stopped during setup. Sharing reconnects on its own."
        }
        Failure::CaptureTaskUnavailable
        | Failure::CaptureWorkerUnavailable
        | Failure::RevocationWatchUnavailable => {
            "The input safety worker could not start. Quit MonHop, reopen it, then retry."
        }
    };
    format!("{explanation} [Startup: {reason:?}]")
}

fn capture_startup_message(reason: NativeCaptureStartupFailure) -> String {
    use NativeCaptureStartupFailure as Failure;
    let explanation = match reason {
        Failure::AlreadyActive | Failure::CleanupPending => {
            "Another local input task is still running or cleaning up. Stop it before retrying."
        }
        Failure::TrialWindowNotForeground => {
            "The test window lost focus. Reopen the test and keep it in front on both computers."
        }
        Failure::DesktopUnavailable => {
            "Windows is not on an available normal desktop. Close any lock or security screen normally, then reopen the test."
        }
        Failure::WindowsOperation { .. } => {
            "Windows could not start input capture. Send the operation and error code below for diagnosis."
        }
        Failure::StartupTimeout => {
            "Native input capture did not become ready in time. Sharing reconnects on its own."
        }
        Failure::Stopped(_) => {
            "Native input capture stopped during setup. Sharing reconnects on its own."
        }
        _ => {
            "Native input capture could not start. Close the test and check input permissions before retrying."
        }
    };
    format!("{explanation} [Capture: {reason:?}]")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_drop_note_dates_the_reason_and_survives_without_a_time() {
        let reason = "Attempt 3: The other computer ended the session. [Link: 0.1 s]";
        assert_eq!(
            super::drop_note(Some(reason), Some(std::time::Duration::from_secs(20))),
            format!("Last drop just now. {reason}")
        );
        assert_eq!(
            super::drop_note(Some(reason), Some(std::time::Duration::from_secs(7 * 60))),
            format!("Last drop 7 minutes ago. {reason}")
        );
        assert_eq!(super::drop_note(Some(reason), None), reason);
        assert_eq!(
            super::drop_note(None, Some(std::time::Duration::from_secs(5))),
            ""
        );
    }

    use super::*;
    use monhop_core::DeviceId;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    fn fixture_peer() -> CertificateFingerprint {
        CertificateFingerprint::parse_full(&"B".repeat(64)).unwrap()
    }

    static NEXT_SYNC_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    /// One fake link runner at a time; link tests also hold NATIVE_LIFECYCLE_TEST_LOCK.
    static LINK_FIXTURES: Mutex<Option<std::sync::mpsc::Sender<LinkFixture>>> = Mutex::new(None);

    struct LinkFixture {
        events: UnboundedSender<LinkEvent>,
        proposals: std::sync::mpsc::Receiver<Vec<u8>>,
        /// Every arranging state the controller told the link to publish, in order.
        arranging: std::sync::mpsc::Receiver<bool>,
    }

    fn fake_link(run: LinkRun) -> LinkFuture {
        Box::pin(async move {
            let LinkRun {
                cancel,
                events,
                mut commands,
                ..
            } = run;
            let (proposed, proposals) = std::sync::mpsc::channel();
            let (arranged, arranging) = std::sync::mpsc::channel();
            if let Some(sender) = lock(&LINK_FIXTURES).as_ref() {
                let _ = sender.send(LinkFixture {
                    events: events.clone(),
                    proposals,
                    arranging,
                });
            }
            loop {
                tokio::select! {
                    command = commands.recv() => match command {
                        Some(LinkCommand::Propose { bytes }) => {
                            let _ = proposed.send(bytes);
                        }
                        Some(LinkCommand::Arranging(value)) => {
                            let _ = arranged.send(value);
                        }
                        Some(LinkCommand::Close) | None => {
                            let _ = events.send(LinkEvent::Closed);
                            return Ok(());
                        }
                    },
                    () = tokio::time::sleep(Duration::from_millis(2)) => {
                        if cancel.is_revoked() {
                            return Err(SetupFailure::Cancelled);
                        }
                    }
                }
            }
        })
    }

    /// A link that meets the other computer dialing a session on the shared port: since the
    /// disagreement repeats, the transport gives the verdict up at once instead of retrying.
    fn purpose_mismatch_link(run: LinkRun) -> LinkFuture {
        Box::pin(async move {
            drop(run);
            Err(SetupFailure::PurposeMismatch)
        })
    }

    fn controller_with(runner: LinkRunner, idle_window: Duration) -> SharingController {
        SharingController {
            operation: Mutex::default(),
            state: Arc::default(),
            worker: Mutex::default(),
            setup_file: Arc::default(),
            link_runner: runner,
            idle_window,
        }
    }

    fn link_controller(idle_window: Duration) -> (SharingController, LinkFixture) {
        let (sender, fixtures) = std::sync::mpsc::channel();
        *lock(&LINK_FIXTURES) = Some(sender);
        let controller = controller_with(fake_link, idle_window);
        let (directory, path) = sync_test_path();
        std::mem::forget(directory);
        controller
            .connect_link(path, "en0:4:192.168.1.4".into(), fixture_peer())
            .expect("the first link must start");
        let fixture = fixtures
            .recv_timeout(Duration::from_secs(2))
            .expect("the fake link runner must start");
        (controller, fixture)
    }

    fn connected_link(idle_window: Duration) -> (SharingController, LinkFixture, InspectedPeer) {
        let (controller, fixture) = link_controller(idle_window);
        let inspection = crate::sharing_preferences::tests::inspection(
            &crate::sharing_preferences::tests::preferences(),
        );
        fixture
            .events
            .send(LinkEvent::Connected {
                inspection: inspection.clone(),
            })
            .unwrap();
        wait_for(&controller, |view| view.phase == "connected");
        (controller, fixture, inspection)
    }

    fn wait_for(
        controller: &SharingController,
        ready: impl Fn(&SharingView) -> bool,
    ) -> SharingView {
        for _ in 0..400 {
            let view = controller.status();
            if ready(&view) {
                return view;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let view = controller.status();
        panic!("state was never reached: {} {}", view.phase, view.message);
    }

    fn sync_test_path() -> (TestDirectory, std::path::PathBuf) {
        let sequence = NEXT_SYNC_TEST_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "monhop-sync-persist-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("sharing.json");
        (TestDirectory(directory), path)
    }

    struct TestDirectory(std::path::PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temporary_count(directory: &Path) -> usize {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".monhop-sharing-")
            })
            .count()
    }

    fn sync_payload() -> (InspectedPeer, Vec<u8>) {
        let preferences = crate::sharing_preferences::tests::preferences();
        let inspection = crate::sharing_preferences::tests::inspection(&preferences);
        let layout = preferences.layout_for_inspection(&inspection).unwrap();
        let bytes = sharing_preferences::shared_setup_bytes(&inspection, layout).unwrap();
        (inspection, bytes)
    }

    #[test]
    fn startup_messages_distinguish_connection_readiness_and_native_capture() {
        use monhop_transport::session::StartupPeerHealthFailure;
        let peer = session_message(SessionFailure::Startup(SessionStartupFailure::PeerHealth(
            StartupPeerHealthFailure::DeadlineExpired,
        )));
        assert!(peer.contains("connection check failed before input started"));
        assert!(peer.contains("PeerHealth(DeadlineExpired)"));
        assert!(!peer.contains("permissions"));
        let waiting = session_message(SessionFailure::Startup(SessionStartupFailure::Deadline));
        assert!(waiting.contains("Its Home says why"));
        assert!(!waiting.contains("capture could not start"));
        let capture = session_message(SessionFailure::NativeCaptureStartup(
            NativeCaptureStartupFailure::WindowsOperation {
                operation: "SetWindowsHookExW(keyboard)",
                code: 5,
            },
        ));
        assert!(capture.contains("Windows could not start input capture"));
        assert!(capture.contains("SetWindowsHookExW(keyboard)"));
        assert!(capture.contains("code: 5"));
        assert!(!capture.contains("release held input"));
    }

    #[test]
    fn startup_messages_preserve_cleanup_and_do_not_guess_a_permission_denial() {
        let focus = session_message(SessionFailure::NativeCaptureStartup(
            NativeCaptureStartupFailure::TrialWindowNotForeground,
        ));
        assert!(focus.contains("test window lost focus"));
        assert!(!focus.contains("permissions"));
        let desktop = session_message(SessionFailure::NativeCaptureStartup(
            NativeCaptureStartupFailure::DesktopUnavailable,
        ));
        assert!(desktop.contains("lock or security screen normally"));
        let cleanup = session_message(SessionFailure::NativeCaptureStartup(
            NativeCaptureStartupFailure::CleanupPending,
        ));
        assert!(cleanup.contains("cleaning up. Stop it before retrying"));
        let legacy = session_message(SessionFailure::NativeStartup);
        assert!(!legacy.contains("permission"));
        assert!(!legacy.contains("release held input"));
        let wire = session_message(SessionFailure::Wire);
        assert!(wire.contains("Its Home says why"));
        assert!(wire.ends_with("[Session: Wire]"));
        let stream = session_message(SessionFailure::UnexpectedStream);
        assert!(stream.contains("sent data this session does not use"));
        assert!(stream.ends_with("[Session: UnexpectedStream]"));
        let queue = session_message(SessionFailure::QueueFull);
        assert!(queue.ends_with("[Session: QueueFull]"));
        let topology = session_message(SessionFailure::SourceController(SourceFailure::Topology));
        assert!(topology.contains("applied layout does not cover"));
        assert!(topology.ends_with("[Session: SourceController(Topology)]"));
        let routing = session_message(SessionFailure::SourceController(
            SourceFailure::RouteMismatch,
        ));
        assert!(routing.contains("lost sync"));
        assert!(routing.ends_with("[Session: SourceController(RouteMismatch)]"));
        let layout = session_message(SessionFailure::Startup(
            SessionStartupFailure::DisplayUnavailable,
        ));
        assert!(layout.contains("display arrangement"));
        assert!(!layout.contains("permissions"));
    }

    fn connected_revision(controller: &SharingController, revision: u64) {
        let mut state = lock(&controller.state);
        state.authorization_revision = revision;
        state.view.revision = revision.to_string();
        state.view.phase = "connected";
    }

    fn error(result: Result<SharingView, String>) -> String {
        result.err().expect("action must fail")
    }

    fn join_finished_worker(controller: &SharingController) {
        for _ in 0..400 {
            if lock(&controller.worker)
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let worker = lock(&controller.worker)
            .take()
            .expect("worker must finish after cancellation");
        worker.join().expect("sharing worker must not panic");
    }

    #[test]
    fn stop_and_restart_preserve_saved_setup_without_restoring_authorization() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let controller = SharingController::default();
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let layout = saved.layout_for_inspection(&inspected).unwrap();
        connected_revision(&controller, 4);
        {
            let mut state = lock(&controller.state);
            state.inspection = Some(inspected);
            state.source_side = Some(SourceSide::Peer);
        }
        let before = controller.save_setup(&path, "4", layout.clone()).unwrap();
        assert!(serde_json::to_value(before).unwrap()["layout"].is_object());
        let saved_bytes = std::fs::read(&path).unwrap();
        let file = SetupFile::load(&path).unwrap();
        assert_eq!(file.active(), Some("b".repeat(64).as_str()));
        assert_eq!(
            file.active_computer().map(|saved| saved.layout()),
            Some(&layout)
        );
        controller.stop();
        assert!(controller.save_setup(&path, "4", layout.clone()).is_err());
        assert!(controller.enable("4".into(), layout).is_err());
        assert!(controller.live_inspection().is_none());
        assert_eq!(std::fs::read(&path).unwrap(), saved_bytes);
        drop(controller);
        let restarted = SharingController::default();
        assert!(restarted.live_inspection().is_none());
        assert_eq!(restarted.status().phase, "off");
        assert!(!restarted.status().sharing_active);
        assert!(lock(&restarted.worker).is_none());
        drop(directory);
    }

    #[test]
    // Forgetting moved to the per-computer command, which needs no connection; see lifecycle.
    fn named_arrangements_save_replace_and_list_for_the_connected_pair() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let path = path.with_file_name("arrangements.json");
        let controller = SharingController::default();
        assert!(controller.arrangements(&path).unwrap().is_empty());
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let mut layout = saved.layout_for_inspection(&inspected).unwrap();
        connected_revision(&controller, 4);
        {
            let mut state = lock(&controller.state);
            state.inspection = Some(inspected);
            state.source_side = Some(SourceSide::Peer);
        }
        assert!(
            controller
                .save_arrangement(&path, "3", "Desk", layout.clone())
                .is_err()
        );
        let listed = controller
            .save_arrangement(&path, "4", " Desk ", layout.clone())
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].name.as_str(), listed[0].mode.as_str()),
            ("Desk", "grouped")
        );
        assert_eq!((listed[0].source_side, listed[0].crossings), ("peer", 1));
        assert_eq!(listed[0].layout.as_ref(), Some(&layout));
        layout.arrangement = Some(ArrangementRequest {
            mode: "free".into(),
            positions: vec![
                DisplayPosition {
                    display: "1".into(),
                    x: 0.0,
                    y: 0.0,
                },
                DisplayPosition {
                    display: "2".into(),
                    x: 1920.0,
                    y: 100.0,
                },
            ],
            hidden: vec![],
        });
        let replaced = controller
            .save_arrangement(&path, "4", "Desk", layout.clone())
            .unwrap();
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].mode, "free");
        assert_eq!(replaced[0].layout.as_ref(), Some(&layout));
        controller
            .save_arrangement(&path, "4", "Couch", layout.clone())
            .unwrap();
        assert_eq!(controller.arrangements(&path).unwrap().len(), 2);
        // Choosing the other input computer keeps every arrangement loadable; the UI switches sides on load.
        {
            let mut state = lock(&controller.state);
            let inspection = state.inspection.as_mut().unwrap();
            inspection.source = inspection.local_device;
            state.source_side = Some(SourceSide::Local);
        }
        let switched = controller.arrangements(&path).unwrap();
        assert_eq!(switched.len(), 2);
        assert!(switched.iter().all(|entry| entry.layout.is_some()));
        assert!(switched.iter().all(|entry| entry.fits));
        assert!(switched.iter().all(|entry| entry.source_side == "peer"));
        // The saved library never carries an enabled switch.
        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(file["arrangements"][0]["setup"]["sharingEnabled"].is_null());
        controller.stop();
        assert!(controller.arrangements(&path).unwrap().is_empty());
        drop(directory);
    }

    #[test]
    fn the_display_banner_rises_once_for_each_change_and_a_dismissal_keeps_it_down() {
        let controller = SharingController::default();
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        assert!(controller.status().display_notice.is_none());
        assert!(!controller.take_window_forward());
        assert!(controller.raise_display_notice(DisplayNotice::Waiting, &inspected));
        assert_eq!(
            controller.status().display_notice,
            Some(DisplayNotice::Waiting)
        );
        assert!(controller.take_window_forward());
        assert!(!controller.take_window_forward());
        assert!(!controller.raise_display_notice(DisplayNotice::Continued, &inspected));
        assert!(controller.dismiss_display_notice().display_notice.is_none());
        assert!(!controller.raise_display_notice(DisplayNotice::Waiting, &inspected));
        assert!(controller.status().display_notice.is_none());
        // Hiding the banner does not unanswer the displays: the supervisor still has nothing
        // left to send for them.
        assert!(controller.waiting_notice_for(&inspected));

        let mut changed = saved.clone();
        changed.set_local_displays_for_test(&["1", "3"]);
        let changed = crate::sharing_preferences::tests::inspection(&changed);
        assert!(controller.raise_display_notice(DisplayNotice::Continued, &changed));
        let view = serde_json::to_value(controller.status()).unwrap();
        assert_eq!(
            view["displayNotice"],
            serde_json::json!({ "kind": "continued" })
        );
        controller.dismiss_display_notice();
        let view = serde_json::to_value(controller.status()).unwrap();
        assert!(view["displayNotice"].is_null());
        // An applied or promoted layout answers the change, so the same displays raise it again.
        controller.clear_display_notice();
        assert!(controller.raise_display_notice(DisplayNotice::Continued, &changed));
    }

    #[test]
    fn every_banner_kind_rises_once_for_one_set_of_displays_and_names_itself_to_the_window() {
        let saved = crate::sharing_preferences::tests::preferences();
        let mut changed = saved.clone();
        changed.set_local_displays_for_test(&["1", "3"]);
        for (kind, name) in [
            (DisplayNotice::Continued, "continued"),
            (DisplayNotice::Waiting, "waiting"),
            (DisplayNotice::Updating, "updating"),
            (DisplayNotice::PeerDeciding, "peerDeciding"),
        ] {
            let controller = SharingController::default();
            let inspected = crate::sharing_preferences::tests::inspection(&saved);
            assert!(controller.raise_display_notice(kind, &inspected));
            // The same displays never raise a second banner, whichever kind asks.
            assert!(!controller.raise_display_notice(kind, &inspected));
            assert!(!controller.raise_display_notice(DisplayNotice::Waiting, &inspected));
            assert_eq!(
                serde_json::to_value(controller.status()).unwrap()["displayNotice"],
                serde_json::json!({ "kind": name })
            );
            // A further change is a new question and is answered once more.
            let inspected = crate::sharing_preferences::tests::inspection(&changed);
            assert!(controller.raise_display_notice(kind, &inspected));
        }
    }

    /// The state a raised banner leaves behind, ready for a commit or a refusal to settle.
    fn raised(kind: DisplayNotice, inspected: &InspectedPeer, left_out: bool) -> State {
        State {
            display_notice: Some(kind),
            notice_answer: Some(kind),
            notice_geometry: Some(DisplayGeometry::of(inspected)),
            notice_left_out: left_out,
            ..State::default()
        }
    }

    #[test]
    fn a_commit_leaves_a_line_only_where_the_new_layout_left_something_out() {
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        // A layout that fit exactly, and the computer that only accepted the other's choice:
        // the change was answered in full, so Home has nothing to tell anyone.
        for raised_kind in [DisplayNotice::Updating, DisplayNotice::PeerDeciding] {
            let mut state = raised(raised_kind, &inspected, false);
            resolve_notice_on_commit(&mut state);
            assert!(state.display_notice.is_none());
            // The memory goes with the answered change, so the next one is judged afresh.
            assert!(state.notice_geometry.is_none());
        }
        // The keyboard computer had to drop a crossing or a display to keep sharing going.
        let mut lost = raised(DisplayNotice::Updating, &inspected, true);
        resolve_notice_on_commit(&mut lost);
        assert_eq!(lost.display_notice, Some(DisplayNotice::Continued));
        assert!(lost.notice_geometry.is_none());
        assert!(!lost.notice_left_out);
        // The computer that only accepted that layout still says nothing: one computer reports
        // the loss, and it is the one that chose.
        let mut accepted = raised(DisplayNotice::PeerDeciding, &inspected, true);
        resolve_notice_on_commit(&mut accepted);
        assert!(accepted.display_notice.is_none());
        // A user's own Apply raised no banner, so it leaves none behind either.
        let mut applied = State::default();
        resolve_notice_on_commit(&mut applied);
        assert!(applied.display_notice.is_none());
        // A dismissed banner is not brought back by the commit that answered it.
        let mut dismissed = State {
            display_notice: None,
            ..raised(DisplayNotice::Updating, &inspected, true)
        };
        resolve_notice_on_commit(&mut dismissed);
        assert!(dismissed.display_notice.is_none());
    }

    #[test]
    fn a_refusal_asks_for_arranging_and_leaves_nothing_in_flight() {
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let mut refused = State {
            display_notice: Some(DisplayNotice::Updating),
            notice_answer: Some(DisplayNotice::Updating),
            notice_geometry: Some(DisplayGeometry::of(&inspected)),
            pending_proposal: Some(DisplayGeometry::of(&inspected)),
            ..State::default()
        };
        reject_sync(&mut refused, LinkRejectReason::Invalid, true);
        assert_eq!(refused.display_notice, Some(DisplayNotice::Waiting));
        // The answer stays with the displays it was given for, so the same change is never
        // proposed a second time, and nothing is left in flight.
        assert_eq!(refused.notice_answer, Some(DisplayNotice::Waiting));
        assert!(refused.notice_geometry.is_some());
        assert!(refused.pending_proposal.is_none());
        // A dismissal hides the banner without unanswering the displays behind it.
        let mut dismissed = State {
            notice_answer: Some(DisplayNotice::Updating),
            notice_geometry: Some(DisplayGeometry::of(&inspected)),
            ..State::default()
        };
        reject_sync(&mut dismissed, LinkRejectReason::Invalid, true);
        assert_eq!(dismissed.notice_answer, Some(DisplayNotice::Waiting));
        assert!(dismissed.display_notice.is_none());
        let mut plain = State::default();
        reject_sync(&mut plain, LinkRejectReason::Invalid, true);
        assert!(plain.display_notice.is_none());
    }

    #[test]
    fn the_supervisor_proposes_a_layout_over_the_link_without_a_window_revision() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let idle = SharingController::default();
        let next = crate::sharing_preferences::tests::preferences();
        assert!(idle.propose_layout(&next, false).is_err());

        let (controller, fixture, inspection) = connected_link(Duration::from_secs(600));
        controller.propose_layout(&next, false).unwrap();
        let sending = controller.status();
        assert_eq!(sending.sync.state, "sending");
        assert_eq!(sending.message, UPDATING_LAYOUT);
        let proposed = fixture
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the link must carry the supervisor's proposal");
        // The bytes are the shared setup an Apply sends: the other computer stages them the
        // same way, and they name the same layout and the same computer for the keyboard.
        let (decoded, staged, layout) =
            sharing_preferences::shared_setup_for_inspection(&inspection, &proposed).unwrap();
        assert_eq!(&layout, next.layout());
        assert_eq!(decoded.source, decoded.peer_device);
        assert_eq!(staged.layout(), next.layout());
        assert_eq!(
            controller.propose_layout(&next, false).err(),
            Some("Wait for the current layout to finish applying.".to_owned())
        );
        // Sending records the proposal and raises the banner under the same lock.
        assert!(controller.proposal_pending_for(&inspection));
        assert_eq!(
            controller.status().display_notice,
            Some(DisplayNotice::Updating)
        );
        // A layout that fit exactly leaves nothing behind once both computers hold it.
        resolve_notice_on_commit(&mut lock(&controller.state));
        assert!(controller.status().display_notice.is_none());
        controller.stop();
        join_finished_worker(&controller);
    }

    #[test]
    fn a_sent_layout_that_dropped_something_is_what_home_reports_after_the_commit() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        let next = crate::sharing_preferences::tests::preferences();
        controller.propose_layout(&next, true).unwrap();
        assert_eq!(
            controller.status().display_notice,
            Some(DisplayNotice::Updating)
        );
        resolve_notice_on_commit(&mut lock(&controller.state));
        assert_eq!(
            controller.status().display_notice,
            Some(DisplayNotice::Continued)
        );
        controller.stop();
        join_finished_worker(&controller);
    }

    #[test]
    fn a_proposal_lost_with_its_link_is_made_again_while_a_commit_settles_it() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, inspection) = connected_link(Duration::from_secs(600));
        let next = crate::sharing_preferences::tests::preferences();
        controller.propose_layout(&next, false).unwrap();
        assert!(controller.proposal_pending_for(&inspection));
        // The link drops before either computer commits.
        controller.stop();
        join_finished_worker(&controller);
        // Nothing is in flight any more, and the banner alone must not stand in for one, so the
        // supervisor proposes again for these same displays on the next link.
        assert!(!controller.proposal_pending_for(&inspection));
        assert!(!controller.waiting_notice_for(&inspection));

        let (controller, fixture, inspection) = connected_link(Duration::from_secs(600));
        controller.propose_layout(&next, false).unwrap();
        assert!(controller.proposal_pending_for(&inspection));
        let (applied, bytes) = sync_payload();
        fixture
            .events
            .send(LinkEvent::SyncCompleted {
                inspection: applied,
                bytes,
                sending: true,
            })
            .unwrap();
        wait_for(&controller, |view| view.sync.state == "applied");
        assert!(!controller.proposal_pending_for(&inspection));
        join_finished_worker(&controller);
    }

    /// A link worker that fails outright, the way an unusable network or identity would.
    fn failing_link(run: LinkRun) -> LinkFuture {
        Box::pin(async move {
            drop(run);
            Err(SetupFailure::NetworkRoute)
        })
    }

    #[test]
    fn a_failed_worker_holds_the_supervisor_off_until_the_user_chooses_again() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let backoff = Duration::from_secs(10);
        let controller = controller_with(failing_link, Duration::from_secs(600));
        let (directory, path) = sync_test_path();
        assert!(!controller.within_failure_backoff(backoff));
        controller
            .connect_link(path.clone(), "en0:4:192.168.1.4".into(), fixture_peer())
            .expect("the link must start");
        join_finished_worker(&controller);
        assert_eq!(controller.status().phase, "error");
        assert!(controller.within_failure_backoff(backoff));
        // Choosing the computer again is a fresh user intent, so the wait is over at once.
        controller.set_active(&path, Some(fixture_peer())).unwrap();
        assert!(!controller.within_failure_backoff(backoff));

        // A misfit and a deliberate close are steps, not failures, and never hold anything off.
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        note_layout_misfit(&mut lock(&controller.state));
        close_link(&mut lock(&controller.state), LAYOUT_MISFIT);
        join_finished_worker(&controller);
        assert!(!controller.within_failure_backoff(backoff));
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        controller.stop();
        join_finished_worker(&controller);
        assert!(!controller.within_failure_backoff(backoff));
        drop(directory);
    }

    #[test]
    fn a_layout_that_no_longer_fits_ends_the_worker_without_an_error_or_a_lost_reason() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        // What a dialed session does when the displays no longer fit the saved layout.
        note_layout_misfit(&mut lock(&controller.state));
        assert!(controller.holds_link());
        close_link(&mut lock(&controller.state), LAYOUT_MISFIT);
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, LAYOUT_MISFIT);
        assert!(view.last_failure.is_empty());
        // The reason outlives the worker, so the supervisor opens a link instead of a session.
        assert!(controller.holds_link());
        assert!(controller.link_is_off());
    }

    #[test]
    fn a_session_the_displays_outgrew_is_told_apart_from_one_that_failed() {
        use monhop_transport::session_actor::ActorFailure;
        // The receiving computer's own displays changed under a running session.
        for step in [3, 8] {
            assert_eq!(
                displays_changed_end(
                    Err(SessionFailure::DestinationActor(ActorFailure::Native, step)),
                    LinkClose::Local,
                    false,
                    || Some(true),
                ),
                Some(DESTINATION_DISPLAYS_CHANGED)
            );
        }
        // A startup failure carries whatever step the process last noted, which may be one of
        // those two from an earlier session. It is a real failure and keeps its backoff.
        for step in [3, 8] {
            assert!(
                displays_changed_end(
                    Err(SessionFailure::DestinationActor(
                        ActorFailure::Startup,
                        step
                    )),
                    LinkClose::Local,
                    false,
                    || Some(true),
                )
                .is_none()
            );
        }
        // The sending computer's periodic geometry check found the same.
        assert_eq!(
            displays_changed_end(
                Err(SessionFailure::InvalidLayout),
                LinkClose::Local,
                true,
                || Some(true)
            ),
            Some(SOURCE_DISPLAYS_CHANGED)
        );
        // The other computer closed first and this one's displays no longer match the record.
        assert_eq!(
            displays_changed_end(
                Err(SessionFailure::Wire),
                LinkClose::PeerFailed,
                true,
                || { Some(false) }
            ),
            Some(SOURCE_DISPLAYS_CHANGED)
        );
        assert_eq!(
            displays_changed_end(Ok(()), LinkClose::PeerEnded, false, || Some(false)),
            Some(DESTINATION_DISPLAYS_CHANGED)
        );
        // Displays that could not be read say nothing either way, so the failure stands.
        assert!(
            displays_changed_end(
                Err(SessionFailure::Wire),
                LinkClose::PeerFailed,
                true,
                || None
            )
            .is_none()
        );
        // Real failures keep their error arm and its backoff: injection refused, a queue that
        // overflowed, a health check missed, and a peer close this computer's displays explain
        // nothing about.
        assert!(
            displays_changed_end(
                Err(SessionFailure::DestinationActor(ActorFailure::Native, 4)),
                LinkClose::Local,
                false,
                || Some(true)
            )
            .is_none()
        );
        assert!(
            displays_changed_end(
                Err(SessionFailure::DestinationActor(ActorFailure::QueueFull, 3)),
                LinkClose::Local,
                false,
                || Some(true)
            )
            .is_none()
        );
        assert!(
            displays_changed_end(
                Err(SessionFailure::SourceController(SourceFailure::Topology)),
                LinkClose::Local,
                true,
                || Some(true)
            )
            .is_none()
        );
        assert!(
            displays_changed_end(
                Err(SessionFailure::Wire),
                LinkClose::PeerFailed,
                true,
                || { Some(true) }
            )
            .is_none()
        );
        // The verdict belongs to the side that can see it: a source never reads the other
        // computer's native steps, and a destination never runs the geometry check.
        assert!(
            displays_changed_end(
                Err(SessionFailure::DestinationActor(ActorFailure::Native, 3)),
                LinkClose::Local,
                true,
                || Some(true)
            )
            .is_none()
        );
        assert!(
            displays_changed_end(
                Err(SessionFailure::InvalidLayout),
                LinkClose::Local,
                false,
                || Some(true)
            )
            .is_none()
        );
    }

    #[test]
    fn a_session_the_displays_outgrew_ends_the_worker_for_the_link_without_a_backoff() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        // What the receiving computer records when its displays change under the session.
        note_displays_changed(&mut lock(&controller.state), DESTINATION_DISPLAYS_CHANGED);
        assert!(controller.holds_link());
        close_link(&mut lock(&controller.state), DESTINATION_DISPLAYS_CHANGED);
        join_finished_worker(&controller);
        let view = controller.status();
        // Not an error and not a drop, so the supervisor opens the link on its next pass
        // instead of waiting out ten seconds.
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, DESTINATION_DISPLAYS_CHANGED);
        assert!(view.last_failure.is_empty());
        assert!(lock(&controller.state).worker_failed_at.is_none());
        assert!(!controller.within_failure_backoff(Duration::from_secs(10)));
        assert!(controller.holds_link());
        assert!(controller.link_is_off());
    }

    #[test]
    fn a_link_that_meets_the_other_computer_starting_a_session_ends_without_an_error() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = controller_with(purpose_mismatch_link, Duration::from_secs(600));
        let (directory, path) = sync_test_path();
        lock(&controller.state).link_reason = Some(LinkReason::LayoutMisfit);
        controller
            .connect_link(path, "en0:4:192.168.1.4".into(), fixture_peer())
            .expect("the link must start");
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, PEER_STARTING_SESSION);
        assert!(view.last_failure.is_empty());
        // The supervisor opens another link at once: the other computer joins it from its own
        // side after its session dial ends the same way.
        assert!(controller.link_is_off());
        assert!(controller.holds_link());
        drop(directory);
    }

    #[test]
    fn an_applied_layout_is_remembered_for_its_displays_and_puts_the_banner_down() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let controller = SharingController::default();
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let layout = saved.layout_for_inspection(&inspected).unwrap();
        connected_revision(&controller, 4);
        {
            let mut state = lock(&controller.state);
            state.inspection = Some(inspected.clone());
            state.source_side = Some(SourceSide::Peer);
        }
        assert!(controller.raise_display_notice(DisplayNotice::Waiting, &inspected));
        controller.save_setup(&path, "4", layout).unwrap();
        assert!(controller.status().display_notice.is_none());
        let library = ArrangementLibrary::load(&path.with_file_name(ARRANGEMENTS_FILE)).unwrap();
        let listed = library.views(&inspected);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].automatic);
        assert_eq!(
            serde_json::to_value(&listed[0]).unwrap()["automatic"],
            serde_json::json!(true)
        );
        assert!(library.automatic_fit(&inspected).is_some());
        // Writing a remembered record as the active one answers the banner too.
        assert!(controller.raise_display_notice(DisplayNotice::Waiting, &inspected));
        controller.write_active_setup(&path, saved.clone()).unwrap();
        assert!(controller.status().display_notice.is_none());
        assert_eq!(
            SetupFile::load(&path).unwrap().active_computer(),
            Some(&saved)
        );
        drop(directory);
    }

    #[test]
    fn a_switch_waits_while_either_computer_is_arranging_or_a_layout_is_unconfirmed() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        let saved = crate::sharing_preferences::tests::preferences();
        assert!(controller.inspection_for_switch().is_some());
        lock(&controller.state).editing = true;
        assert!(controller.inspection_for_switch().is_none());
        lock(&controller.state).editing = false;
        // The other computer's user is arranging, as its link said so; nothing is inferred.
        fixture
            .events
            .send(LinkEvent::PeerArranging { arranging: true })
            .unwrap();
        wait_for(&controller, |view| view.message == CONNECTED_PEER_ARRANGING);
        assert!(controller.inspection_for_switch().is_none());
        assert!(!controller.yield_link_when_layout_fits(&saved));
        fixture
            .events
            .send(LinkEvent::PeerArranging { arranging: false })
            .unwrap();
        wait_for(&controller, |view| view.message == CONNECTED_ARRANGE);
        assert!(controller.inspection_for_switch().is_some());
        lock(&controller.state).link_reason = Some(LinkReason::LayoutUnconfirmed);
        assert!(controller.inspection_for_switch().is_none());
        // A stale layout is exactly when a switch may act.
        lock(&controller.state).link_reason = Some(LinkReason::LayoutMisfit);
        assert!(controller.inspection_for_switch().is_some());
        controller.stop();
        join_finished_worker(&controller);
        assert!(controller.inspection_for_switch().is_none());
        // The peer's state never outlives its link.
        assert!(!lock(&controller.state).peer_arranging);
    }

    #[test]
    fn arranging_here_is_published_to_the_other_computer_and_withdrawn_when_it_ends() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        // Every link opens by stating this computer's arranging state rather than staying silent.
        assert_eq!(
            fixture.arranging.recv_timeout(Duration::from_secs(1)),
            Ok(false)
        );
        controller.begin_editing();
        assert_eq!(
            fixture.arranging.recv_timeout(Duration::from_secs(1)),
            Ok(true)
        );
        controller.end_editing();
        assert_eq!(
            fixture.arranging.recv_timeout(Duration::from_secs(1)),
            Ok(false)
        );
        join_finished_worker(&controller);
    }

    #[test]
    fn a_session_dial_that_met_the_other_computers_link_lets_go_once_the_link_is_up() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture) = link_controller(Duration::from_secs(600));
        // What a session dial records when the handshake reports PurposeMismatch.
        lock(&controller.state).link_reason = Some(LinkReason::PeerHoldsLink);
        assert!(controller.holds_link());
        let inspection = crate::sharing_preferences::tests::inspection(
            &crate::sharing_preferences::tests::preferences(),
        );
        fixture
            .events
            .send(LinkEvent::Connected {
                inspection: inspection.clone(),
            })
            .unwrap();
        wait_for(&controller, |view| view.phase == "connected");
        // Both computers are on the link, so the decision path runs again from here.
        assert!(!controller.holds_link());
        assert!(controller.inspection_for_switch().is_some());
        controller.stop();
        join_finished_worker(&controller);
    }

    fn arranged<'a>(displays: &'a [(&'a str, bool)]) -> Vec<ArrangedDisplay<'a>> {
        displays
            .iter()
            .map(|(id, local)| ArrangedDisplay { id, local: *local })
            .collect()
    }

    fn arranged_layout(arrangement: ArrangementRequest) -> LayoutRequest {
        LayoutRequest {
            source_display: "1".into(),
            links: vec![LinkRequest {
                from_display: "1".into(),
                from_edge: "right".into(),
                from_span: [0.0, 1.0],
                to_display: "2".into(),
                to_edge: "left".into(),
                to_span: [0.0, 1.0],
                hysteresis: 1.0,
            }],
            arrangement: Some(arrangement),
        }
    }

    fn position(display: &str, x: f64, y: f64) -> DisplayPosition {
        DisplayPosition {
            display: display.into(),
            x,
            y,
        }
    }

    #[test]
    fn arrangements_are_validated_against_the_connected_displays() {
        let displays = arranged(&[("1", true), ("2", false)]);
        let ok = ArrangementRequest {
            mode: "free".into(),
            positions: vec![position("1", -5.5, 0.0), position("2", 1920.0, 0.25)],
            hidden: vec![],
        };
        assert!(validate_arrangement(&arranged_layout(ok), &displays).is_ok());
        let grouped_partial = ArrangementRequest {
            mode: "grouped".into(),
            positions: vec![position("2", 1920.0, 0.0)],
            hidden: vec![],
        };
        assert!(validate_arrangement(&arranged_layout(grouped_partial), &displays).is_ok());
        let mut without_arrangement = arranged_layout(ArrangementRequest {
            mode: "diagonal".into(),
            positions: vec![],
            hidden: vec![],
        });
        without_arrangement.arrangement = None;
        assert!(validate_arrangement(&without_arrangement, &displays).is_ok());
        for (mode, positions) in [
            ("diagonal", vec![]),
            ("free", vec![position("1", 0.0, 0.0)]),
            (
                "grouped",
                vec![position("1", 0.0, 0.0), position("1", 1.0, 1.0)],
            ),
            ("grouped", vec![position("9", 0.0, 0.0)]),
            ("grouped", vec![position("1", f64::NAN, 0.0)]),
            ("grouped", vec![position("1", 0.0, 20_000_001.0)]),
        ] {
            let bad = ArrangementRequest {
                mode: mode.into(),
                positions,
                hidden: vec![],
            };
            assert!(validate_arrangement(&arranged_layout(bad), &displays).is_err());
        }
    }

    #[test]
    fn a_hidden_display_leaves_its_computer_a_display_and_no_route() {
        // Displays 1 and 3 are local; 2, 4 and 5 belong to the other computer.
        let displays = arranged(&[
            ("1", true),
            ("2", false),
            ("3", true),
            ("4", false),
            ("5", false),
        ]);
        let free = |positions: Vec<DisplayPosition>, hidden: Vec<&str>| {
            arranged_layout(ArrangementRequest {
                mode: "free".into(),
                positions,
                hidden: hidden.into_iter().map(String::from).collect(),
            })
        };
        let all_but = |hidden: &[&str]| {
            ["1", "2", "3", "4", "5"]
                .iter()
                .filter(|id| !hidden.contains(id))
                .enumerate()
                .map(|(index, id)| position(id, index as f64 * 100.0, 0.0))
                .collect::<Vec<_>>()
        };
        // Any display the pointer does not route onto can be marked not in use, alone or together.
        for hidden in [
            vec!["4"],
            vec!["3"],
            vec!["5"],
            vec!["3", "4"],
            vec!["4", "5"],
        ] {
            assert!(validate_arrangement(&free(all_but(&hidden), hidden), &displays).is_ok());
        }
        // A computer keeps at least one display in use.
        assert!(validate_arrangement(&free(all_but(&["3"]), vec!["3", "1"]), &displays).is_err());
        assert!(
            validate_arrangement(&free(all_but(&["4", "5"]), vec!["4", "5", "2"]), &displays)
                .is_err()
        );
        // A hidden display that is also placed, unknown or duplicate hidden ids, and a free picture
        // that leaves a display out.
        assert!(validate_arrangement(&free(all_but(&["9"]), vec!["4"]), &displays).is_err());
        assert!(validate_arrangement(&free(all_but(&["4"]), vec!["9"]), &displays).is_err());
        assert!(validate_arrangement(&free(all_but(&["4"]), vec!["4", "4"]), &displays).is_err());
        let mut short = all_but(&["4"]);
        short.pop();
        assert!(validate_arrangement(&free(short, vec!["4"]), &displays).is_err());
        // A hidden display can be neither the starting display nor a link end.
        assert!(validate_arrangement(&free(all_but(&["2"]), vec!["2"]), &displays).is_err());
        let mut routed = free(all_but(&["4"]), vec!["4"]);
        routed.source_display = "4".into();
        assert!(validate_arrangement(&routed, &displays).is_err());
        let mut linked = free(all_but(&["4"]), vec!["4"]);
        linked.links[0].to_display = "4".into();
        assert!(validate_arrangement(&linked, &displays).is_err());
        // A differently spelled id still names the hidden display.
        let mut spelled = free(all_but(&["4"]), vec!["4"]);
        spelled.links[0].to_display = "04".into();
        assert!(validate_arrangement(&spelled, &displays).is_err());
        spelled.links[0].to_display = "2".into();
        spelled.source_display = "004".into();
        assert!(validate_arrangement(&spelled, &displays).is_err());
        assert!(validate_arrangement(&free(all_but(&["4"]), vec!["04"]), &displays).is_err());
    }

    #[test]
    fn a_hidden_copy_beside_the_starting_display_keeps_the_crossing_usable() {
        use monhop_core::{MonitorIdentity, Point};
        use session_setup::{DisplayDescription, DisplayTopology};
        let shared = MonitorIdentity::new(0x10ac, 0x4123, 0xabcd);
        let describe = |id: u64, x: f64, primary: bool, monitor| DisplayDescription {
            id: DisplayId(id),
            name: format!("d{id}"),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::new(x, 0.0),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: primary,
            monitor,
        };
        let fingerprint = |value: &str| {
            monhop_transport::crypto::CertificateFingerprint::parse_full(&value.repeat(64)).unwrap()
        };
        // The Mac's copy (3) sits right of its starting display (1); the Windows copy (4) right of 2.
        let inspected = InspectedPeer {
            local_device: DeviceId([1; 16]),
            peer_device: DeviceId([2; 16]),
            source: DeviceId([1; 16]),
            local_fingerprint: fingerprint("a"),
            peer_fingerprint: fingerprint("b"),
            local_platform: Platform::MacOs,
            peer_platform: Platform::Windows,
            local_displays: DisplayTopology::new(vec![
                describe(1, 0.0, true, None),
                describe(3, 100.0, false, shared),
            ])
            .unwrap(),
            peer_displays: DisplayTopology::new(vec![
                describe(2, 0.0, true, None),
                describe(4, 100.0, false, shared),
            ])
            .unwrap(),
            interface_id: "en0".into(),
        };
        let link = |from: &str, from_edge: &str, to: &str, to_edge: &str| LinkRequest {
            from_display: from.into(),
            from_edge: from_edge.into(),
            from_span: [0.0, 1.0],
            to_display: to.into(),
            to_edge: to_edge.into(),
            to_span: [0.0, 1.0],
            hysteresis: 1.0,
        };
        let layout = LayoutRequest {
            source_display: "1".into(),
            links: vec![
                link("1", "right", "2", "left"),
                link("2", "left", "1", "right"),
            ],
            arrangement: Some(ArrangementRequest {
                mode: "grouped".into(),
                positions: vec![],
                hidden: vec!["3".into()],
            }),
        };
        let (start, topology) = validated_layout(&inspected, &layout).unwrap();
        assert_eq!(start, DisplayId(1));
        // The hidden copy stays a known display, so a pointer the OS parks there is not an error, but
        // nothing routes onto it; the Windows copy keeps its own adjacency.
        assert!(topology.display(DisplayId(3)).is_ok());
        assert!(
            topology
                .links()
                .iter()
                .all(|l| l.from_display != DisplayId(3) && l.to_display != DisplayId(3))
        );
        assert!(
            topology
                .links()
                .iter()
                .any(|l| l.from_display == DisplayId(2) && l.to_display == DisplayId(4))
        );
        // Without the mark the OS adjacency 1.right -> 3.left overlaps the crossing and the layout fails.
        let plain = LayoutRequest {
            arrangement: None,
            ..layout.clone()
        };
        assert!(validated_layout(&inspected, &plain).is_err());
    }

    #[test]
    fn placed_one_by_one_the_picture_rules_the_other_computers_adjacency() {
        use monhop_core::Point;
        use session_setup::{DisplayDescription, DisplayTopology};
        let describe = |id: u64, x: f64, y: f64, primary: bool| DisplayDescription {
            id: DisplayId(id),
            name: format!("d{id}"),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::new(x, y),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: primary,
            monitor: None,
        };
        let fingerprint = |value: &str| {
            monhop_transport::crypto::CertificateFingerprint::parse_full(&value.repeat(64)).unwrap()
        };
        // The peer supplies input from display 1. The local OS stacks 3 above 2 along 2's whole top edge.
        let inspected = InspectedPeer {
            local_device: DeviceId([1; 16]),
            peer_device: DeviceId([2; 16]),
            source: DeviceId([2; 16]),
            local_fingerprint: fingerprint("a"),
            peer_fingerprint: fingerprint("b"),
            local_platform: Platform::MacOs,
            peer_platform: Platform::Windows,
            local_displays: DisplayTopology::new(vec![
                describe(2, 0.0, 100.0, true),
                describe(3, 0.0, 0.0, false),
            ])
            .unwrap(),
            peer_displays: DisplayTopology::new(vec![describe(1, 0.0, 0.0, true)]).unwrap(),
            interface_id: "en0".into(),
        };
        let link = |from: &str, from_edge: &str, to: &str, to_edge: &str| LinkRequest {
            from_display: from.into(),
            from_edge: from_edge.into(),
            from_span: [0.0, 1.0],
            to_display: to.into(),
            to_edge: to_edge.into(),
            to_span: [0.0, 1.0],
            hysteresis: 1.0,
        };
        // The crossing uses 2's top edge, which the local OS already routes to 3.
        let links = vec![
            link("1", "bottom", "2", "top"),
            link("2", "top", "1", "bottom"),
        ];
        let grouped = LayoutRequest {
            source_display: "1".into(),
            links: links.clone(),
            arrangement: Some(ArrangementRequest {
                mode: "grouped".into(),
                positions: vec![],
                hidden: vec![],
            }),
        };
        assert!(validated_layout(&inspected, &grouped).is_err());
        // Placed one by one with 3 off to the side, the picture is the adjacency and the layout stands.
        let free = LayoutRequest {
            arrangement: Some(ArrangementRequest {
                mode: "free".into(),
                positions: vec![
                    position("1", 0.0, 0.0),
                    position("2", 0.0, 100.0),
                    position("3", 300.0, 0.0),
                ],
                hidden: vec![],
            }),
            ..grouped.clone()
        };
        let (_, topology) = validated_layout(&inspected, &free).unwrap();
        assert!(
            topology
                .links()
                .iter()
                .all(|l| l.from_display != DisplayId(3) && l.to_display != DisplayId(3))
        );
        // Marked not in use, 3 stays in the topology but is never the pointer's display.
        let left_out = LayoutRequest {
            arrangement: Some(ArrangementRequest {
                mode: "free".into(),
                positions: vec![position("1", 0.0, 0.0), position("2", 0.0, 100.0)],
                hidden: vec!["3".into()],
            }),
            ..grouped.clone()
        };
        let (_, topology) = validated_layout(&inspected, &left_out).unwrap();
        assert!(!topology.display(DisplayId(3)).unwrap().in_use);
        assert!(topology.display(DisplayId(2)).unwrap().in_use);
        assert!(topology.display(DisplayId(1)).unwrap().in_use);
        // The input computer's own OS layout is physical: its picture positions change nothing.
        let peer_stack = InspectedPeer {
            source: DeviceId([1; 16]),
            local_displays: DisplayTopology::new(vec![describe(1, 0.0, 0.0, true)]).unwrap(),
            peer_displays: DisplayTopology::new(vec![
                describe(2, 0.0, 100.0, true),
                describe(3, 0.0, 0.0, false),
            ])
            .unwrap(),
            ..inspected
        };
        let mirrored = LayoutRequest {
            source_display: "2".into(),
            links: vec![
                link("2", "top", "1", "bottom"),
                link("1", "bottom", "2", "top"),
            ],
            ..free.clone()
        };
        assert!(validated_layout(&peer_stack, &mirrored).is_err());
    }

    #[test]
    fn every_reachable_remote_display_requires_a_return_path() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        use monhop_core::{Display, LogicalSize, Machine, NativeSize, Point, Topology};
        let source = DeviceId([1; 16]);
        let peer = DeviceId([2; 16]);
        let displays = (1..=4)
            .map(|id| {
                Display::new(
                    DisplayId(id),
                    if id == 1 { source } else { peer },
                    format!("fixture{id}"),
                    NativeSize::new(100, 100),
                    LogicalSize::new(100.0, 100.0),
                    Point::new((id - 1) as f64 * 100.0, 0.0),
                    1.0,
                    None,
                    id == 1,
                )
            })
            .collect::<Vec<_>>();
        let edge = |from, from_edge, to| {
            EdgeLink::new(
                DisplayId(from),
                from_edge,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                DisplayId(to),
                Edge::Left,
                NormalizedSpan::new(0.0, 1.0).unwrap(),
                1.0,
            )
            .unwrap()
        };
        let machines = vec![
            Machine::new(source, Platform::Windows),
            Machine::new(peer, Platform::MacOs),
        ];
        let mut links = vec![
            edge(1, Edge::Right, 2),
            edge(2, Edge::Right, 3),
            edge(2, Edge::Left, 1),
        ];
        let stranded = Topology::new(machines.clone(), displays.clone(), links.clone()).unwrap();
        assert!(validate_return_paths(&stranded, source).is_err());
        links.push(edge(3, Edge::Right, 2));
        let connected = Topology::new(machines, displays, links).unwrap();
        assert!(validate_return_paths(&connected, source).is_ok());
    }
    #[test]
    fn stop_and_shutdown_stop_registered_native_input_before_returning() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        for shutdown in [false, true] {
            let controller = SharingController::default();
            let cancel = RevocationSignal::default();
            let native = RevocationSignal::default();
            register_native_cancellation(
                &mut lock(&controller.state),
                0,
                0,
                &cancel,
                native.clone(),
            )
            .unwrap();
            if shutdown {
                controller.request_shutdown();
            } else {
                controller.stop();
            }
            // The session loops end on the stop request; the socket revoke follows the watch's
            // grace so the QUIC close can leave first.
            assert!(native.is_stopping());
            assert!(!native.is_revoked());
        }
    }

    #[test]
    fn stop_before_native_registration_rejects_and_revokes_the_late_signal() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        let cancel = RevocationSignal::default();
        let native = RevocationSignal::default();
        controller.stop();
        assert!(
            register_native_cancellation(
                &mut lock(&controller.state),
                0,
                0,
                &cancel,
                native.clone()
            )
            .is_err()
        );
        assert!(native.is_revoked());
        assert!(lock(&controller.state).native_cancel.is_none());
    }

    #[test]
    fn startup_polling_and_idle_stop_do_not_create_a_worker_or_session() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        for _ in 0..10 {
            let view = controller.status();
            assert_eq!(view.phase, "off");
            assert!(!view.busy);
            assert!(!view.sharing_active);
            assert_eq!(view.sync.state, "idle");
            assert!(view.source_side.is_none());
        }
        assert_eq!(controller.stop().phase, "off");
        assert!(lock(&controller.worker).is_none());
        assert!(lock(&controller.state).cancel.is_none());
        assert!(controller.shutdown_ready());
    }
    #[test]
    fn enable_requires_a_current_inspection_and_strict_display_identifiers() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        assert!(
            controller
                .enable(
                    "0".into(),
                    LayoutRequest {
                        source_display: "1".into(),
                        links: vec![],
                        arrangement: None,
                    }
                )
                .is_err()
        );
        for invalid in ["", "-1", "1e3", "1.0", "18446744073709551616"] {
            assert!(parse_display(invalid).is_err());
        }
        assert_eq!(parse_display("18446744073709551615").unwrap().0, u64::MAX);
        assert!(parse_side("windows").is_err());
        assert!(lock(&controller.worker).is_none());
    }

    #[test]
    fn stop_invalidates_an_enable_revision_before_a_worker_starts() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        connected_revision(&controller, 7);
        controller.stop();
        assert_eq!(
            error(controller.launch(
                "starting",
                "fixture",
                Some("7".into()),
                WorkerKind::Session,
                None,
                |_, _, _, _, _| async { Ok(()) },
            )),
            "The setup changed. Connect the computers again."
        );
        assert!(lock(&controller.worker).is_none());
        assert!(lock(&controller.state).inspection.is_none());
    }

    #[test]
    fn a_native_revoked_session_consumes_approval_and_cannot_restore_connected() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        connected_revision(&controller, 7);
        let original = lock(&controller.state).view.revision.clone();
        let view = controller
            .launch(
                "starting",
                "fixture",
                Some(original.clone()),
                WorkerKind::Session,
                None,
                |state, _, _, cancel, _| async move {
                    cancel.mark_revoked_without_wake();
                    // A native failure can occur without the user clicking Stop.
                    lock(&state).view.phase = "starting";
                    Ok(())
                },
            )
            .unwrap();
        assert_ne!(view.revision, original);
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert!(!view.sharing_active);
        assert!(lock(&controller.state).inspection.is_none());
        assert!(
            controller
                .launch(
                    "starting",
                    "fixture",
                    Some(original),
                    WorkerKind::Session,
                    None,
                    |_, _, _, _, _| async { panic!("old approval must never launch") },
                )
                .is_err()
        );
    }

    #[test]
    fn failed_input_session_invalidates_inspection_without_hiding_the_failure() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        connected_revision(&controller, 7);
        controller
            .launch(
                "starting",
                "fixture",
                Some("7".into()),
                WorkerKind::Session,
                None,
                |_, _, _, _, _| async { Err("fixture input failure".into()) },
            )
            .unwrap();
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "error");
        assert_eq!(view.message, "fixture input failure");
        assert!(lock(&controller.state).inspection.is_none());
        assert_ne!(view.revision, "7");
    }

    #[test]
    fn session_revision_overflow_latches_shutdown_before_a_worker_starts() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        connected_revision(&controller, u64::MAX);
        assert_eq!(
            error(controller.launch(
                "starting",
                "fixture",
                Some(u64::MAX.to_string()),
                WorkerKind::Session,
                None,
                |_, _, _, _, _| async { Ok(()) },
            )),
            "Restart MonHop before starting another session."
        );
        assert!(lock(&controller.state).shutdown);
        assert!(lock(&controller.worker).is_none());
    }

    #[test]
    fn stopped_worker_completion_never_publishes_connected() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        let (started, entered) = std::sync::mpsc::channel();
        controller
            .launch(
                "starting",
                "fixture",
                None,
                WorkerKind::Session,
                None,
                move |_, _, _, cancel, _| {
                    let _ = started.send(());
                    async move {
                        while !cancel.is_revoked() {
                            tokio::task::yield_now().await;
                        }
                        Ok(())
                    }
                },
            )
            .unwrap();
        entered
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker must start");
        assert_eq!(controller.stop().phase, "stopping");
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert!(!view.busy);
        assert!(lock(&controller.state).inspection.is_none());
    }

    #[test]
    fn stopped_worker_reports_native_cleanup_until_the_retained_owner_releases() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        let (started, entered) = std::sync::mpsc::channel();
        controller
            .launch(
                "starting",
                "fixture",
                None,
                WorkerKind::Session,
                None,
                move |_, _, _, cancel, _| {
                    let _ = started.send(());
                    async move {
                        while !cancel.is_revoked() {
                            tokio::task::yield_now().await;
                        }
                        Ok(())
                    }
                },
            )
            .unwrap();
        entered
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("worker must start");
        let ownership =
            monhop_core::NativeInputOwnership::claim().expect("test retains native cleanup");
        assert_eq!(controller.stop().phase, "stopping");
        join_finished_worker(&controller);
        let pending = controller.status();
        assert_eq!(pending.phase, "error");
        assert_eq!(pending.message, native_cleanup_message());
        assert!(!pending.busy);
        assert!(!controller.shutdown_ready());
        drop(ownership);
        let released = controller.status();
        assert_eq!(released.phase, "off");
        assert_eq!(released.message, NOT_CONNECTED);
        assert!(!released.busy);
        assert!(controller.shutdown_ready());
    }

    #[test]
    fn shutdown_latches_and_rejects_later_connection_and_enable() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        controller.request_shutdown();
        assert!(controller.shutdown_ready());
        assert_eq!(
            error(controller.connect_link(
                "unused".into(),
                "physical-network".into(),
                fixture_peer()
            )),
            "MonHop is shutting down and cannot start sharing."
        );
        assert_eq!(
            error(controller.enable(
                "0".into(),
                LayoutRequest {
                    source_display: "1".into(),
                    links: vec![],
                    arrangement: None,
                },
            )),
            "MonHop is shutting down and cannot enable sharing."
        );
    }

    #[test]
    fn retained_native_cleanup_blocks_invalidation_and_new_connections() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        let ownership =
            monhop_core::NativeInputOwnership::claim().expect("test owns native input cleanup");
        assert!(!controller.shutdown_ready());
        assert_eq!(
            controller.invalidate_if_idle().unwrap_err(),
            "Wait for local input cleanup before connecting again."
        );
        assert_eq!(
            error(controller.connect_link(
                "unused".into(),
                "physical-network".into(),
                fixture_peer()
            )),
            "Wait for local input cleanup before starting another sharing action."
        );
        drop(ownership);
        assert!(controller.invalidate_if_idle().is_ok());
    }

    #[test]
    fn choosing_source_is_local_and_invalidates_old_layout_authorization() {
        let controller = SharingController::default();
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        connected_revision(&controller, 12);
        lock(&controller.state).inspection = Some(inspected.clone());
        let view = controller.select_source("12", "local").unwrap();
        assert_eq!(view.source_side, Some("local"));
        assert_eq!(view.source_platform, Some("macos"));
        assert_eq!(view.peer_platform, Some("windows"));
        assert_eq!(view.revision, "13");
        assert_eq!(
            lock(&controller.state).inspection.as_ref().unwrap().source,
            inspected.local_device
        );
        assert!(lock(&controller.worker).is_none());
        assert!(!view.busy);
        assert!(!view.sharing_active);
        assert!(controller.select_source("12", "peer").is_err());
        controller.stop();
        assert!(controller.select_source("13", "peer").is_err());
    }

    #[test]
    fn staged_setup_is_rejected_after_a_newer_worker_generation() {
        let controller = SharingController::default();
        lock(&controller.state).worker_generation = 2;
        let (directory, path) = sync_test_path();
        let original = b"previous inert setup".to_vec();
        std::fs::write(&path, &original).unwrap();
        let (fresh, bytes) = sync_payload();
        assert_eq!(
            stage_shared_setup(&controller.state, 1, 0, &path, &fresh, &bytes),
            Err(LinkRejectReason::Cancelled)
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(temporary_count(&directory.0), 0);
        assert!(lock(&controller.state).staged.is_none());
    }

    #[test]
    fn staged_setup_is_only_visible_after_commit() {
        let controller = SharingController::default();
        let (directory, path) = sync_test_path();
        let (fresh, bytes) = sync_payload();
        let persist = link_persist(
            Arc::clone(&controller.state),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&fresh, &bytes).unwrap();
        assert!(lock(&controller.state).staged.is_some());
        assert!(!path.exists());
        (persist.commit)(&fresh, &bytes).unwrap();
        assert_eq!(temporary_count(&directory.0), 0);
        let file = SetupFile::load(&path).unwrap();
        assert_eq!(file.active(), Some("b".repeat(64).as_str()));
        assert!(file.active_computer().is_some());

        (persist.stage)(&fresh, &bytes).unwrap();
        assert!(lock(&controller.state).staged.is_some());
        (persist.discard)();
        assert!(lock(&controller.state).staged.is_none());
        assert_eq!(
            (persist.commit)(&fresh, &bytes),
            Err(LinkRejectReason::SaveFailed)
        );
    }

    #[test]
    fn stop_between_staging_and_commit_leaves_the_applied_setup_untouched() {
        let (directory, path) = sync_test_path();
        crate::sharing_preferences::tests::file_with(
            crate::sharing_preferences::tests::preferences_for_peer('C'),
        )
        .save(&path)
        .unwrap();
        let original = std::fs::read(&path).unwrap();
        let controller = SharingController::default();
        let (fresh, bytes) = sync_payload();
        let persist = link_persist(
            Arc::clone(&controller.state),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&fresh, &bytes).unwrap();
        assert!(lock(&controller.state).staged.is_some());

        begin_close(&mut lock(&controller.state), "fixture stop");
        assert!(lock(&controller.state).staged.is_none());
        assert_eq!(temporary_count(&directory.0), 0);
        assert_eq!(
            (persist.commit)(&fresh, &bytes),
            Err(LinkRejectReason::Cancelled)
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        // A stage after the Stop cannot re-arm the commit either.
        assert_eq!(
            (persist.stage)(&fresh, &bytes),
            Err(LinkRejectReason::Cancelled)
        );
        assert_eq!(temporary_count(&directory.0), 0);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn late_trial_event_cannot_rewrite_a_finished_sharing_error() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let sharing = SharingController::default();
        sharing
            .launch(
                "starting",
                "fixture",
                None,
                WorkerKind::Session,
                None,
                |_, _, _, _, _| async { Err("The sharing worker failed after setup.".into()) },
            )
            .unwrap();
        join_finished_worker(&sharing);
        let before = sharing.status();
        assert!(!before.busy);
        assert_eq!(before.phase, "error");
        assert_eq!(before.message, "The sharing worker failed after setup.");

        let trial = crate::trial::TrialController::default();
        trial.test_activate_for_late_event();
        assert!(!trial.test_late_focus_after_worker(&sharing));

        let after = sharing.status();
        assert!(!after.busy);
        assert_eq!(after.phase, "error");
        assert_eq!(after.message, before.message);
    }

    #[test]
    fn connect_link_rejects_a_second_standing_link() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture) = link_controller(Duration::from_secs(600));
        let connecting = controller.status();
        assert_eq!(connecting.phase, "connecting");
        assert!(connecting.busy);
        assert_eq!(
            error(controller.connect_link(
                "unused".into(),
                "en0:4:192.168.1.4".into(),
                fixture_peer()
            )),
            "Already connected. Stop the connection before connecting again."
        );
        drop(fixture);
        controller.stop();
        join_finished_worker(&controller);
    }

    #[test]
    fn apply_setup_requires_a_connected_link_and_the_current_revision() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let idle = SharingController::default();
        let saved = crate::sharing_preferences::tests::preferences();
        let layout = saved.layout().clone();
        assert!(idle.apply_setup("0", "peer", layout.clone()).is_err());

        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        let revision = controller.status().revision;
        assert!(controller.apply_setup("0", "peer", layout.clone()).is_err());
        assert!(
            controller
                .apply_setup(&revision, "both", layout.clone())
                .is_err()
        );
        let sending = controller
            .apply_setup(&revision, "peer", layout.clone())
            .unwrap();
        assert_eq!(sending.sync.state, "sending");
        let proposed = fixture
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the link must receive one proposal");
        assert!(!proposed.is_empty());
        assert_eq!(
            error(controller.apply_setup(&revision, "peer", layout)),
            "Wait for the current layout to finish applying."
        );
        controller.stop();
        join_finished_worker(&controller);
    }

    #[test]
    fn a_rejected_layout_never_changes_the_chosen_input_computer() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, inspection) = connected_link(Duration::from_secs(600));
        let saved = crate::sharing_preferences::tests::preferences();
        let chosen = controller
            .select_source(&controller.status().revision, "peer")
            .unwrap();
        assert_eq!(chosen.source_platform, Some("windows"));
        let mut local_layout = saved.layout().clone();
        local_layout.source_display = inspection.local_displays.displays()[0].id.0.to_string();
        let sending = controller
            .apply_setup(&chosen.revision, "local", local_layout)
            .unwrap();
        assert_eq!(sending.sync.state, "sending");
        assert_eq!(sending.source_side, Some("peer"));
        assert_eq!(sending.source_platform, Some("windows"));

        fixture
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::Busy,
                sending: true,
            })
            .unwrap();
        let rejected = wait_for(&controller, |view| view.sync.state == "rejected");
        assert_eq!(rejected.source_side, Some("peer"));
        assert_eq!(rejected.source_platform, Some("windows"));
        assert_eq!(
            rejected.sync.message,
            "The other computer applied a layout first. Review it, then apply again if you want to change it."
        );
        assert_eq!(rejected.phase, "connected");
        controller.stop();
        join_finished_worker(&controller);
    }

    #[test]
    fn the_sender_closes_the_link_once_both_computers_applied_and_sharing_is_armed() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        let (inspection, bytes) = sync_payload();
        fixture
            .events
            .send(LinkEvent::SyncCompleted {
                inspection,
                bytes,
                sending: true,
            })
            .unwrap();
        let closed = wait_for(&controller, |view| view.phase == "off");
        assert_eq!(closed.message, LAYOUT_APPLIED_SHARING_ON);
        assert!(!closed.editing);
        assert!(!controller.holds_link());
        assert_eq!(closed.sharing_role, Some("receives"));
        assert_eq!(closed.sync.state, "applied");
        assert!(!closed.busy);
        assert!(controller.link_is_off());
        join_finished_worker(&controller);
    }

    #[test]
    fn the_receiver_keeps_the_link_until_the_sender_leaves_after_apply() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        let (inspection, bytes) = sync_payload();
        fixture
            .events
            .send(LinkEvent::SyncCompleted {
                inspection,
                bytes,
                sending: false,
            })
            .unwrap();
        let applied = wait_for(&controller, |view| view.sync.state == "applied");
        assert_eq!(applied.phase, "connected");
        assert!(!applied.editing);
        fixture
            .events
            .send(LinkEvent::Disconnected {
                reason: session_link::LinkDisconnect::PeerClosed,
            })
            .unwrap();
        let closed = wait_for(&controller, |view| view.phase == "off");
        assert_eq!(closed.message, LAYOUT_APPLIED_SHARING_ON);
        assert!(closed.peer_fingerprint.is_none());
        assert!(controller.link_is_off());
        join_finished_worker(&controller);
    }

    #[test]
    fn a_peer_that_cannot_save_after_this_computer_applied_says_only_one_side_saved() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        let (inspection, bytes) = sync_payload();
        fixture
            .events
            .send(LinkEvent::SyncCompleted {
                inspection,
                bytes,
                sending: false,
            })
            .unwrap();
        let applied = wait_for(&controller, |view| view.sync.state == "applied");
        assert_eq!(applied.sync.message, LAYOUT_APPLIED_SHARING_ON);
        assert!(!controller.holds_link());
        assert_eq!(applied.sharing_role, Some("receives"));

        fixture
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::SaveFailed,
                sending: false,
            })
            .unwrap();
        let rejected = wait_for(&controller, |view| view.sync.state == "rejected");
        assert_eq!(rejected.sync.message, ONLY_THIS_COMPUTER_SAVED);
        assert!(controller.holds_link());
        assert_eq!(rejected.message, ONLY_THIS_COMPUTER_SAVED);
        // Fitting displays are not enough: the link waits for an Apply both computers hold.
        assert!(
            !controller
                .yield_link_when_layout_fits(&crate::sharing_preferences::tests::preferences())
        );
        assert_eq!(controller.status().phase, "connected");

        // A refusal this computer never committed against keeps the plain wording.
        fixture
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::SaveFailed,
                sending: true,
            })
            .unwrap();
        wait_for(&controller, |view| {
            view.sync.message == reject_message(LinkRejectReason::SaveFailed)
        });
        controller.stop();
        join_finished_worker(&controller);
    }

    #[test]
    fn an_idle_close_ends_arranging_and_reports_no_error() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_millis(60));
        // A standing link has no idle window.
        std::thread::sleep(Duration::from_millis(200));
        let standing = controller.status();
        assert_eq!(standing.phase, "connected");
        assert_eq!(
            standing.peer_fingerprint.as_deref(),
            Some("b".repeat(64).as_str())
        );
        assert!(!standing.editing);
        assert!(controller.begin_editing().editing);
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, IDLE_CLOSED);
        assert!(!view.editing);
        assert!(!view.busy);
        assert!(view.peer_fingerprint.is_none());
        assert!(lock(&controller.state).link.is_none());
    }

    #[test]
    fn ending_arranging_closes_the_link_and_a_fitting_layout_yields_it_to_sharing() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        controller.begin_editing();
        let ended = controller.end_editing();
        assert_eq!(ended.phase, "stopping");
        assert!(!ended.editing);
        join_finished_worker(&controller);
        assert_eq!(controller.status().message, EDITING_ENDED);

        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        let saved = crate::sharing_preferences::tests::preferences();
        let other = crate::sharing_preferences::tests::preferences_for_peer('C');
        assert!(!controller.yield_link_when_layout_fits(&other));
        assert_eq!(controller.status().phase, "connected");
        lock(&controller.state).link_reason = Some(LinkReason::LayoutMisfit);
        assert!(controller.yield_link_when_layout_fits(&saved));
        assert!(!controller.holds_link());
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, LAYOUT_FITS_AGAIN);
    }

    #[test]
    fn a_standing_link_that_loses_its_peer_ends_while_an_arranging_link_reconnects() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        fixture
            .events
            .send(LinkEvent::Disconnected {
                reason: session_link::LinkDisconnect::PeerClosed,
            })
            .unwrap();
        let ended = wait_for(&controller, |view| view.phase == "off");
        assert_eq!(ended.message, PEER_LEFT);
        assert!(controller.link_is_off());
        join_finished_worker(&controller);

        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        controller.begin_editing();
        fixture
            .events
            .send(LinkEvent::Disconnected {
                reason: session_link::LinkDisconnect::PeerClosed,
            })
            .unwrap();
        let reconnecting = wait_for(&controller, |view| view.phase == "reconnecting");
        assert!(reconnecting.editing);
        assert!(!controller.link_is_off());
        controller.stop();
        join_finished_worker(&controller);
    }

    #[test]
    fn a_misfit_outlives_its_link_so_the_next_one_opens_for_the_same_reason() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        // A misfit outlives the link: the next link is opened for the same reason.
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        lock(&controller.state).link_reason = Some(LinkReason::LayoutMisfit);
        controller.stop();
        join_finished_worker(&controller);
        assert!(controller.holds_link());
    }

    #[test]
    fn pausing_ends_the_link_with_the_paused_wording() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        let (directory, path) = sync_test_path();
        let paused = controller.set_active(&path, None).unwrap();
        assert_eq!(paused.phase, "stopping");
        assert!(paused.active.is_none());
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, PAUSED);
        assert!(SetupFile::load(&path).unwrap().active().is_none());
        // Choosing the computer the link is already with keeps the link.
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        let kept = controller.set_active(&path, Some(fixture_peer())).unwrap();
        assert_eq!(kept.phase, "connected");
        assert_eq!(kept.active.as_deref(), Some("b".repeat(64).as_str()));
        controller.stop();
        join_finished_worker(&controller);
        drop(directory);
    }

    #[test]
    fn stop_during_connecting_reports_not_connected_after_the_worker_exits() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture) = link_controller(Duration::from_secs(600));
        let stopping = controller.stop();
        assert_eq!(stopping.phase, "stopping");
        assert!(stopping.busy);
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, NOT_CONNECTED);
        assert!(!view.busy);
        assert!(controller.shutdown_ready());
    }
}
