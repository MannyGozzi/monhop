//! Explicit sharing actions. Constructing or polling this controller performs no native I/O.

use crate::arrangement_library::{ArrangementLibrary, ArrangementView};
use crate::sharing_preferences::{
    self, ControlMap, DisplayGeometry, PreferenceError, SavedSetupView, SetupFile,
    SharingPreferences, both_directions, fingerprint_key,
};
use monhop_core::{
    DisplayId, Edge, EdgeLink, NativeSessionClaim, NormalizedSpan, Platform, Point,
    RevocationSignal,
};
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
const CONNECTING_FOR_SHARING: &str = "Connecting to the other computer for sharing.";
const LAYOUT_APPLIED_SHARING_ON: &str = "Layout applied on both computers. Sharing is on.";
const APPLYING_LAYOUT: &str = "Applying the layout on both computers.";
const CONFIRMING_LAYOUT: &str = "Confirming the layout on both computers.";
const CONNECTED_ARRANGE: &str = "Connected. Arrange the displays, then apply.";
const CONNECTED_LAYOUT_STALE: &str =
    "Connected. The displays changed since the layout was applied. Arrange them again, then apply.";
const CONNECTED_PEER_ARRANGING: &str = "Connected. The other computer is arranging displays. Sharing resumes when it applies the layout.";
const CONNECTED_CHANGING_CONTROL: &str = "Connected. Updating who can control which computer.";
const PEER_ARRANGING: &str = "The other computer is arranging displays. Joining it for arranging.";
const LAYOUT_MISFIT: &str =
    "The displays changed since the layout was applied. Connecting to arrange them.";
const DISPLAYS_CHANGED: &str = "This computer's displays changed. Connecting to update the layout.";
const PEER_DISPLAYS_CHANGED: &str =
    "The other computer's displays changed. Connecting to update the layout.";
const DISPLAYS_UNSETTLED: &str = "Waiting for this computer's displays to settle.";
const RECORDS_DISAGREE: &str =
    "The two computers hold different layouts. Connecting to agree on one.";
const UPDATING_LAYOUT: &str = "The displays changed. Updating the layout on both computers.";
const UPDATING_CONTROL: &str = "Updating who can control which computer on both computers.";
const CHANGING_CONTROL: &str =
    "Updating who can control which computer. Sharing resumes in a moment.";
const PEER_CONTROL_CHANGED: &str =
    "The other computer changed who can control which computer. Connecting to sync.";
const LAST_DIRECTION: &str =
    "At least one computer must be able to control the other. Use Pause to stop sharing.";
const CONTROL_SUPERSEDED: &str = "The other computer changed who can control which computer at the same time. Check the switches.";
const CONTROL_REFUSED: &str =
    "The other computer could not apply the control change. Try the switch again.";
const SHARING_DROPPED: &str = "Sharing dropped. Reconnecting.";
const PEER_STARTING_SESSION: &str = "The other computer is starting sharing. Connecting again.";
const PEER_LEFT: &str = "The other computer left. Reconnecting.";
const PEER_PAUSED: &str = "The other computer paused sharing. Reconnecting when it resumes.";
pub(crate) const ARRANGEMENTS_UNREADABLE: &str =
    "The saved arrangements could not be read. Your applied layout is unchanged.";
/// The arrangement library sits beside the setup file, so every writer of one finds the other.
pub(crate) const ARRANGEMENTS_FILE: &str = "arrangements.json";
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
/// Retrying cannot change the identity the other computer presents; pairing again does.
const PEER_IDENTITY_BACKOFF: Duration = Duration::from_secs(60);
/// A display list that fails to read mid-change is read again this often, quietly, for this long
/// before it counts as a failure.
const UNSETTLED_POLL: Duration = Duration::from_millis(250);
const UNSETTLED_WINDOW: Duration = Duration::from_secs(5);
/// The decider proposes only once both computers' displays have held still this long; the
/// session they outgrew has already ended.
const DISPLAYS_SETTLE: Duration = Duration::from_secs(1);
/// A proposal refused for a passing reason is made again after this long, at most this many
/// times for one set of displays before they count as answered with "nothing fits".
const PROPOSAL_RETRY_AFTER: Duration = Duration::from_secs(1);
const MAX_PROPOSAL_RETRIES: u8 = 3;
/// Identical failed dial attempts are logged once, then summarised this often.
const ATTEMPT_LOG_INTERVAL: Duration = Duration::from_secs(60);
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
    /// Who may control whom, seen from this computer; null without an active record.
    pub(crate) control: Option<ControlView>,
    last_failure: String,
    /// The session is up but the other computer stopped answering; input stays local until it does.
    held: bool,
}

/// The two switches on Home. While a change made here is syncing, they already show it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlView {
    pub(crate) local_to_peer: bool,
    pub(crate) peer_to_local: bool,
    pub(crate) syncing: bool,
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
            control: None,
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
    /// This computer decides and is sending the other one a layout that fits. Clears at the
    /// commit unless that layout left something out.
    Updating,
    /// The other computer decides and picks the layout; this one only accepts it. Always clears
    /// at the commit: the deciding computer is the one that reports a loss.
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
    /// Every crossing, in both directions.
    pub(crate) links: Vec<LinkRequest>,
    /// Where each display sits on the shared canvas; the links above are derived from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) arrangement: Option<ArrangementRequest>,
    /// Who may control whom. The window never chooses it: this app fills it from the record.
    #[serde(default)]
    pub(crate) control: ControlMap,
}

/// Each computer keeps its own display layout and moves as one block.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArrangementRequest {
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

/// Positions are canvas data for both computers' editors and the block offset; hidden displays
/// stay known but nothing routes onto them, and each computer keeps at least one display in use.
pub(crate) fn validate_arrangement(
    layout: &LayoutRequest,
    displays: &[ArrangedDisplay<'_>],
) -> Result<(), String> {
    let Some(arrangement) = &layout.arrangement else {
        return Ok(());
    };
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
    // Links are parsed leniently later, so hidden ids are compared as numbers.
    let same = |value: &str, id: DisplayId| parse_display(value).is_ok_and(|parsed| parsed == id);
    for hidden in &arrangement.hidden {
        let Some(hidden_id) = known(hidden).and_then(|_| parse_display(hidden).ok()) else {
            return Err("The arrangement hides a display that is not connected.".into());
        };
        if used.contains(&hidden.as_str()) {
            return Err("A hidden display cannot also be placed.".into());
        }
        if layout
            .links
            .iter()
            .any(|link| same(&link.from_display, hidden_id) || same(&link.to_display, hidden_id))
        {
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
    Ok(())
}

/// Everything the setup-link runner needs. Bundled so tests can swap the runner for a fake.
pub(crate) struct LinkRun {
    interface_id: String,
    peer: CertificateFingerprint,
    cancel: RevocationSignal,
    persist: LinkPersist,
    events: UnboundedSender<LinkEvent>,
    commands: UnboundedReceiver<LinkCommand>,
}

pub(crate) type LinkFuture = Pin<Box<dyn Future<Output = Result<(), SetupFailure>>>>;
pub(crate) type LinkRunner = fn(LinkRun) -> LinkFuture;

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

/// The active record's control map and the two computers it names, mirrored so the view needs
/// no disk read.
#[derive(Clone)]
struct ActiveControl {
    local: String,
    peer: String,
    control: ControlMap,
}

impl ActiveControl {
    fn of(record: &SharingPreferences) -> Self {
        Self {
            local: fingerprint_key(record.local_fingerprint()),
            peer: fingerprint_key(record.peer_fingerprint()),
            control: record.control().clone(),
        }
    }
}

#[derive(Default)]
struct State {
    view: SharingView,
    worker_generation: u64,
    authorization_revision: u64,
    shutdown: bool,
    native_cleanup_pending: bool,
    inspection: Option<InspectedPeer>,
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
    /// Validated by the link but not yet written, with whether its sender left something out; a
    /// Stop before commit drops it.
    staged: Option<(SharingPreferences, bool)>,
    /// The computer the running worker is for.
    peer: Option<CertificateFingerprint>,
    /// Mirrors the setup file so the view can show the active computer without a live worker.
    active: Option<String>,
    active_control: Option<ActiveControl>,
    /// Switch flips made here that both computers have not committed yet, over `active_control`.
    pending_control: Option<ControlMap>,
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
    /// Passing refusals of a proposal for one set of displays, and when the last one came.
    proposal_retry: Option<(DisplayGeometry, u8, Instant)>,
    /// When the link's displays last changed; the decider waits for them to hold still.
    displays_changed_at: Option<Instant>,
    /// When this computer's displays first failed to read on a dial or link; a good read clears it.
    displays_unreadable_since: Option<Instant>,
    /// Consumed once by the app, which then brings its window forward.
    notice_window_pending: bool,
    /// Why the supervisor keeps a standing link up instead of a session; None once a layout is
    /// applied or the computer is chosen again.
    link_reason: Option<LinkReason>,
    /// Outlives the automatic reconnect, so Home still shows why the previous session ended.
    last_failure: Option<String>,
    last_failure_at: Option<Instant>,
    /// When a worker last ended with a real error, and the least it waits before an automatic
    /// retry. A close, a misfit or a step change is not a failure and never sets it.
    worker_failed_at: Option<(Instant, Duration)>,
    /// The least wait the failing worker asks for; taken when it ends.
    failure_floor: Duration,
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
        Self::with_link_runner(transport_link, LINK_IDLE_WINDOW)
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

/// What a standing link waits for before the port goes back to a session. Only the decider's
/// commit, or a commit the other computer sent, ends such a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkReason {
    /// This computer's record no longer fits the displays, or the two records disagree.
    LayoutMisfit,
    /// This computer saved a layout the other could not; only a fresh Apply clears it.
    LayoutUnconfirmed,
    /// A session dial met the other computer on its setup link. Only the choice between a session
    /// and a link is affected, so this is cleared the moment the link connects.
    PeerHoldsLink,
    /// The other computer ended its session to change who controls whom; the link is up only to
    /// receive that record.
    ControlChanged,
}

/// Which computer's displays ended a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayChange {
    Local,
    Peer,
}

impl DisplayChange {
    const fn message(self) -> &'static str {
        match self {
            Self::Local => DISPLAYS_CHANGED,
            Self::Peer => PEER_DISPLAYS_CHANGED,
        }
    }

    const fn detected_by(self) -> &'static str {
        match self {
            Self::Local => "this computer",
            Self::Peer => "the other computer",
        }
    }
}

/// Collapses identical failed dial attempts: the first is logged, then one summary a minute.
#[derive(Default)]
struct AttemptLog {
    last: Option<(SetupFailure, Instant)>,
    repeats: u32,
}

impl AttemptLog {
    fn note(
        &mut self,
        error: SetupFailure,
        attempt: u32,
        waited: Duration,
        now: Instant,
    ) -> Option<String> {
        if let Some((last, at)) = self.last
            && last == error
            && now.duration_since(at) < ATTEMPT_LOG_INTERVAL
        {
            self.repeats = self.repeats.saturating_add(1);
            return None;
        }
        let repeats = std::mem::take(&mut self.repeats);
        self.last = Some((error, now));
        let again = if repeats == 0 {
            String::new()
        } else {
            format!(", after {repeats} more like the last line")
        };
        Some(format!(
            "attempt {attempt} did not connect ({error:?}), waited {:.0} s{again}",
            waited.as_secs_f64()
        ))
    }
}

/// True while this computer's displays have failed to read for less than the unsettled window:
/// a read that fails mid-change is retried quietly instead of counting as a failure.
pub(crate) fn still_unsettled(since: &mut Option<Instant>, now: Instant) -> bool {
    let first = *since.get_or_insert(now);
    now.duration_since(first) < UNSETTLED_WINDOW
}

/// `control` with the flips in `pending` on top.
fn overlaid(control: &ControlMap, pending: Option<&ControlMap>) -> ControlMap {
    let mut control = control.clone();
    if let Some(pending) = pending {
        control.extend(pending.iter().map(|(key, allowed)| (key.clone(), *allowed)));
    }
    control
}

const fn local_platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else {
        "windows"
    }
}

impl SharingController {
    pub(crate) fn with_link_runner(link_runner: LinkRunner, idle_window: Duration) -> Self {
        Self {
            operation: Mutex::default(),
            state: Arc::default(),
            worker: Mutex::default(),
            setup_file: Arc::default(),
            link_runner,
            idle_window,
        }
    }

    /// Window shutdown must wait for retained native release workers, including prior sessions.
    pub fn shutdown_ready(&self) -> bool {
        !NativeSessionClaim::is_claimed()
            && lock(&self.worker)
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
    }
    pub fn request_shutdown(&self) {
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        state.shutdown = true;
        begin_close(
            &mut state,
            "Stopping sharing and releasing held input.",
            false,
        );
        if !state.view.busy {
            state.progress = None;
            state.cancel = None;
            state.native_cancel = None;
            set_idle_message(&mut state, true);
        }
    }
    pub fn invalidate_if_idle(&self) -> Result<(), String> {
        let _operation = lock(&self.operation);
        if NativeSessionClaim::is_claimed() {
            return Err("Wait for local input cleanup before connecting again.".into());
        }
        let mut worker = lock(&self.worker);
        if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Err("Wait for the current sharing action to finish.".into());
        }
        if let Some(previous) = worker.take() {
            let _ = previous.join();
        }
        if NativeSessionClaim::is_claimed() {
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
        let native_cleanup_pending = NativeSessionClaim::is_claimed();
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
    pub fn stop_with(&self, idle_message: &str) -> SharingView {
        self.stop_with_reason(idle_message, false)
    }

    /// Same as [`Self::stop_with`], marked so the peer's close reads as a control-change resync
    /// instead of an ordinary stop.
    fn stop_with_reason(&self, idle_message: &str, control_change: bool) -> SharingView {
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        if state.view.busy || state.link.is_some() {
            state.close_message = Some(idle_message.to_owned());
        }
        begin_close(
            &mut state,
            "Stopping the connection and releasing held input.",
            control_change,
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

    /// True only with no link worker and no link phase: the supervisor opens one only then.
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
            state.pending_control = None;
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

    /// Flips one direction for the active computer. The change stays pending here until both
    /// computers commit it over the link; the view already shows it, marked as syncing.
    pub fn set_control(
        &self,
        path: &Path,
        fingerprint: &str,
        direction: &str,
        allowed: bool,
    ) -> Result<SharingView, String> {
        let local_to_peer = match direction {
            "localToPeer" => true,
            "peerToLocal" => false,
            _ => {
                return Err(
                    "Choose which computer's keyboard and mouse this switch is for.".into(),
                );
            }
        };
        let peer = sharing_preferences::parse_fingerprint(fingerprint)?;
        let file =
            SetupFile::load(path).map_err(|_| "The saved setup could not be read.".to_owned())?;
        self.adopt_saved(&file);
        {
            let mut state = lock(&self.state);
            if state.shutdown {
                return Err("MonHop is shutting down.".into());
            }
            let active = state
                .active_control
                .clone()
                .filter(|active| active.peer == fingerprint_key(&peer.full_hex()))
                .ok_or("Arrange the displays with this computer first.")?;
            let key = if local_to_peer {
                active.local.clone()
            } else {
                active.peer.clone()
            };
            let mut pending = state.pending_control.clone().unwrap_or_default();
            pending.insert(key, allowed);
            if !overlaid(&active.control, Some(&pending))
                .values()
                .any(|allowed| *allowed)
            {
                return Err(LAST_DIRECTION.into());
            }
            pending.retain(|key, allowed| active.control.get(key) != Some(allowed));
            state.pending_control = (!pending.is_empty()).then_some(pending);
        }
        Ok(self.status())
    }

    /// True while a switch flip made here waits for both computers to commit it.
    pub fn control_pending(&self) -> bool {
        lock(&self.state).pending_control.is_some()
    }

    /// Ends a running session deliberately so the link can carry a pending control change; true
    /// when it did.
    pub fn end_session_for_control(&self) -> bool {
        {
            let state = lock(&self.state);
            if state.pending_control.is_none()
                || state.link.is_some()
                || !state.view.busy
                || state.close_message.is_some()
                || state.shutdown
            {
                return false;
            }
        }
        self.stop_with_reason(CHANGING_CONTROL, true);
        true
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

    /// True while a proposal for these displays was refused for a passing reason too recently.
    pub fn retry_wait_for(&self, inspection: &InspectedPeer) -> bool {
        let geometry = DisplayGeometry::of(inspection);
        lock(&self.state)
            .proposal_retry
            .as_ref()
            .is_some_and(|(refused, _, at)| {
                refused.same(&geometry) && at.elapsed() < PROPOSAL_RETRY_AFTER
            })
    }

    /// True once the link's displays have held still long enough for the decider to propose.
    pub fn displays_settled(&self) -> bool {
        lock(&self.state)
            .displays_changed_at
            .is_some_and(|at| at.elapsed() >= DISPLAYS_SETTLE)
    }

    /// A proposal that could not leave: the next pass tries again, as for a passing refusal.
    pub fn note_proposal_failed(&self, inspection: &InspectedPeer) {
        note_passing_refusal(&mut lock(&self.state), DisplayGeometry::of(inspection));
    }

    /// True while a failed worker's backoff still runs, so the supervisor does not relaunch the
    /// same failure every pass. Any user choice clears it.
    pub fn within_failure_backoff(&self, backoff: Duration) -> bool {
        lock(&self.state)
            .worker_failed_at
            .is_some_and(|(failed, floor)| failed.elapsed() < backoff.max(floor))
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

    /// True once for each raised "nothing fits" banner: the app then brings its window forward.
    pub fn take_window_forward(&self) -> bool {
        std::mem::take(&mut lock(&self.state).notice_window_pending)
    }

    /// The link's displays while the decision pass may act: never while either user arranges or an
    /// exchange is in flight or committed here, since the sender's close then ends the link.
    pub fn inspection_for_switch(&self) -> Option<InspectedPeer> {
        let state = lock(&self.state);
        if state.link.is_none()
            || state.editing
            || state.peer_arranging
            || state.link_reason == Some(LinkReason::LayoutUnconfirmed)
            || state.view.phase != "connected"
            || state.close_message.is_some()
            || !matches!(state.view.sync.state, "idle" | "rejected")
        {
            return None;
        }
        state.inspection.clone()
    }

    /// This computer's displays no longer fit its record, so the next connection is a link on
    /// which the decider picks the record both computers hold.
    pub fn note_local_misfit(&self) {
        lock(&self.state)
            .link_reason
            .get_or_insert(LinkReason::LayoutMisfit);
    }

    /// True while a standing link must stay up instead of a session.
    pub fn holds_link(&self) -> bool {
        let state = lock(&self.state);
        state.link_reason.is_some() || state.pending_control.is_some()
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

    /// Proposes the user's layout over the standing link. The outcome arrives as a link event.
    pub fn apply_setup(
        &self,
        revision: &str,
        mut layout: LayoutRequest,
    ) -> Result<SharingView, String> {
        let _operation = lock(&self.operation);
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
        let inspection = state
            .inspection
            .clone()
            .ok_or("Connect both computers first.")?;
        layout.control = control_for(&state, &inspection);
        validated_layout(&inspection, &layout)?;
        let bytes = sharing_preferences::shared_setup_bytes(&inspection, layout, false)
            .map_err(|error| error.to_string())?;
        commands
            .send(LinkCommand::Propose { bytes })
            .map_err(|_| "The connection ended. Connect again.".to_owned())?;
        touch_link(&mut state);
        state.view.sync = SyncView {
            state: "sending",
            message: APPLYING_LAYOUT.into(),
        };
        state.view.message = APPLYING_LAYOUT.into();
        Ok(view_of(&state))
    }

    /// The decider's Apply for a display change. `left_out` (a crossing or display dropped) decides
    /// the banner the commit leaves and whether either computer remembers the layout.
    pub fn propose_layout(&self, next: &SharingPreferences, left_out: bool) -> Result<(), String> {
        self.send_proposal(next, Some(left_out))
    }

    /// Proposes a record that already fits both computers' displays: the decider's own, or either
    /// computer's carrying a control change. Nothing changed on screen, so no banner.
    pub fn propose_record(&self, next: &SharingPreferences) -> Result<(), String> {
        self.send_proposal(next, None)
    }

    /// Same staging and commit as `apply_setup`, with no window revision to check. The proposal
    /// is recorded, and any banner raised, under the same lock that sends it.
    fn send_proposal(
        &self,
        next: &SharingPreferences,
        display_change: Option<bool>,
    ) -> Result<(), String> {
        let _operation = lock(&self.operation);
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
        let inspection = state
            .inspection
            .clone()
            .ok_or("Connect both computers first.")?;
        let mut layout = next.layout().clone();
        layout.control = overlaid(next.control(), state.pending_control.as_ref());
        if sharing_preferences::validate_control(
            &layout.control,
            &inspection.local_fingerprint.full_hex(),
            &inspection.peer_fingerprint.full_hex(),
        )
        .is_err()
        {
            layout.control = next.control().clone();
        }
        validated_layout(&inspection, &layout)?;
        let left_out = display_change.unwrap_or(false);
        let bytes = sharing_preferences::shared_setup_bytes(&inspection, layout, left_out)
            .map_err(|error| error.to_string())?;
        commands
            .send(LinkCommand::Propose { bytes })
            .map_err(|_| "The connection ended. Connect again.".to_owned())?;
        let geometry = DisplayGeometry::of(&inspection);
        let message = match display_change {
            Some(left_out) => {
                raise_notice(&mut state, DisplayNotice::Updating, geometry.clone());
                // Set after the raise, which clears it: this send is the authority even when it
                // reuses a banner already up for these displays.
                state.notice_left_out = left_out;
                UPDATING_LAYOUT
            }
            None if state.pending_control.is_some() => UPDATING_CONTROL,
            None => CONFIRMING_LAYOUT,
        };
        state.pending_proposal = Some(geometry);
        state.view.sync = SyncView {
            state: "sending",
            message: message.into(),
        };
        state.view.message = message.into();
        Ok(())
    }

    /// Mirrors the setup file into the view so Home shows the active computer and its switches
    /// without a live link.
    pub fn adopt_saved(&self, file: &SetupFile) {
        let mut state = lock(&self.state);
        state.active = file.active().map(str::to_owned);
        state.active_control = file.active_computer().map(ActiveControl::of);
        reconcile_pending_control(&mut state);
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
        let control = saved.wire_control().map_err(|error| error.to_string())?;
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
                let mut attempts = AttemptLog::default();
                let mut endpoint =
                    session_setup::StandingShareEndpoint::new(&interface_id, peer, control);
                log::info!(
                    "sharing: connecting on {interface_id} with record #{} ({control:?})",
                    saved.digest()
                );
                loop {
                    if cancel.is_revoked() {
                        return Ok(());
                    }
                    attempt = attempt.saturating_add(1);
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
                        // The two records disagree on who may control whom. Dialing again repeats
                        // it: the link is where the decider proposes one record for both.
                        Err(SetupFailure::ChangedSinceInspection) => {
                            note_record_disagreement(&mut lock(&state));
                            return Ok(());
                        }
                        Err(SetupFailure::PeerIdentityChanged) => {
                            log::warn!(
                                "sharing: attempt {attempt}: the other computer presented an identity other than the paired one; stopping"
                            );
                            lock(&state).failure_floor = PEER_IDENTITY_BACKOFF;
                            return Err(setup_message(SetupFailure::PeerIdentityChanged));
                        }
                        Err(SetupFailure::Displays)
                            if still_unsettled(
                                &mut lock(&state).displays_unreadable_since,
                                Instant::now(),
                            ) =>
                        {
                            lock(&state).view.message = DISPLAYS_UNSETTLED.to_owned();
                            sleep_unless_cancelled(&cancel, UNSETTLED_POLL).await;
                            continue;
                        }
                        // The switch is the user's standing intent: a peer that is off or
                        // still installing must be able to join later without a new click.
                        // Attempt outcomes go to the status line; last_failure keeps the reason
                        // the previous session ended.
                        Err(error) => {
                            if let Some(line) = attempts.note(
                                error,
                                attempt,
                                window_started.elapsed(),
                                Instant::now(),
                            ) {
                                log::info!("sharing: {line}");
                            }
                            lock(&state).view.message =
                                share_wait_message(error, window_started.elapsed());
                            sleep_unless_cancelled(&cancel, RECONNECT_INTERVAL).await;
                            continue;
                        }
                    };
                    lock(&state).displays_unreadable_since = None;
                    attempts = AttemptLog::default();
                    if saved.layout_for_inspection(&paired.inspection).as_ref() != Ok(&layout) {
                        note_layout_misfit(&mut lock(&state));
                        return Ok(());
                    }
                    log::info!("sharing: attempt {attempt} connected, starting the session");
                    let topology = validated_layout(&paired.inspection, &layout)?;
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
                    let result = session::run_session(
                        paired.session,
                        topology,
                        worker_generation,
                        paired.revocation.clone(),
                        progress,
                    )
                    .await;
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
                    // Pause, a switch flip or quitting set these before the session ended.
                    let deliberate = {
                        let guard = lock(&state);
                        guard.shutdown
                            || guard.close_message.is_some()
                            || guard.worker_generation != worker_generation
                    };
                    // A display change is a step, not a failure. Classified before the arms below
                    // so it keeps no failure reason, and acted on after native release so the
                    // worker still leaves input clean.
                    let skip_transition_checks =
                        deliberate || matches!(result, Err(SessionFailure::NativeCleanup));
                    let outgrown = if skip_transition_checks {
                        None
                    } else {
                        displays_changed_end(&result, peer_close, || {
                            current_local_displays(&saved)
                                .ok()
                                .map(|current| saved.matches_local_displays(&current))
                        })
                    };
                    // The peer's control flip is checked once the displays already ruled
                    // themselves out: the two close reasons never both apply.
                    let peer_control_change =
                        outgrown.is_none() && !skip_transition_checks && peer_control_change_end(peer_close);
                    if let Some(change) = outgrown {
                        log::info!(
                            "sharing: the session ended as a display change detected by {}",
                            change.detected_by()
                        );
                    }
                    if !deliberate {
                        lock(&state).view.message = match outgrown {
                            Some(change) => change.message(),
                            None if peer_control_change => PEER_CONTROL_CHANGED,
                            None => SHARING_DROPPED,
                        }
                        .to_owned();
                    }
                    match result {
                        _ if deliberate => return Ok(()),
                        Err(SessionFailure::NativeCleanup) => {
                            return Err(native_cleanup_message().into());
                        }
                        // The displays explain this end; it is not a drop and keeps no reason.
                        _ if outgrown.is_some() => {}
                        // The other computer's control flip explains this end; not a drop either.
                        _ if peer_control_change => {}
                        // A network revocation or the emergency stop: the next dial starts fresh.
                        Err(SessionFailure::Revoked) if cancel.is_revoked() => return Ok(()),
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
                    if !await_native_release().await {
                        return Err(native_cleanup_message().into());
                    }
                    endpoint.reclaim(paired.lease).await;
                    // The saved record can no longer fit, so every dial from here would only wait
                    // for the other computer to reach the same conclusion.
                    if let Some(change) = outgrown {
                        note_displays_changed(&mut lock(&state), change.message());
                        return Ok(());
                    }
                    if peer_control_change {
                        note_control_changed(&mut lock(&state));
                        return Ok(());
                    }
                    if cancel.is_revoked() {
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

    /// A setup built from the live inspection, valid for both computers, carrying the control map
    /// this computer holds for the pair.
    fn connected_setup(
        &self,
        revision: &str,
        mut layout: LayoutRequest,
    ) -> Result<SharingPreferences, String> {
        let state = lock(&self.state);
        if state.shutdown
            || state.view.busy
            || state.view.phase != "connected"
            || state.view.revision != revision
        {
            return Err("Connect both computers again before saving this layout.".into());
        }
        let inspected = state
            .inspection
            .clone()
            .ok_or("Connect both computers first.")?;
        layout.control = control_for(&state, &inspected);
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
        if NativeSessionClaim::is_claimed() {
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
        state.failure_floor = Duration::ZERO;
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
                    state.displays_changed_at = None;
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
                    let floor = std::mem::take(&mut state.failure_floor);
                    state.native_cleanup_pending = NativeSessionClaim::is_claimed();
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
                            state.worker_failed_at = Some((Instant::now(), floor));
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

/// Shared teardown for Stop, quit and a control-record flip: stop the session so its close can
/// leave, ask a live link to close cleanly, and let the cancellation watch revoke the socket
/// after its grace. `control_change` marks the stop so the peer's close reads as a resync.
fn begin_close(state: &mut State, stopping_message: &str, control_change: bool) {
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
        if control_change {
            native.request_stop_for_control_change();
        } else {
            native.request_stop();
        }
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
                    // Displays read mid-change: the supervisor reopens the link on its next pass.
                    Err(SetupFailure::Displays) => {
                        let mut state = lock(&state);
                        if state.worker_generation == generation
                            && still_unsettled(&mut state.displays_unreadable_since, Instant::now())
                        {
                            state
                                .close_message
                                .get_or_insert_with(|| DISPLAYS_UNSETTLED.to_owned());
                            return Ok(());
                        }
                        Err(setup_message(SetupFailure::Displays))
                    }
                    Err(SetupFailure::PeerIdentityChanged) => {
                        log::warn!("link: the other computer presented an identity other than the paired one");
                        lock(&state).failure_floor = PEER_IDENTITY_BACKOFF;
                        Err(setup_message(SetupFailure::PeerIdentityChanged))
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
fn open_link_for(state: &mut State, reason: LinkReason, message: &str) {
    state.link_reason = Some(reason);
    state.close_message = Some(message.to_owned());
}

fn open_link_for_layout(state: &mut State, message: &str) {
    open_link_for(state, LinkReason::LayoutMisfit, message);
}

/// A dialed session whose displays no longer fit the saved layout. That is the ordinary answer to
/// a display change, not a failure: the worker ends cleanly, so the supervisor opens the link on
/// its next pass with no backoff and the decider sends a layout that fits.
fn note_layout_misfit(state: &mut State) {
    log::info!(
        "sharing: the displays no longer fit the saved layout; opening the link to arrange them"
    );
    open_link_for_layout(state, LAYOUT_MISFIT);
}

/// The two computers' saved records disagree on who may control whom. Answered on the link like
/// a misfit, never by dialing again: the decider proposes one record and both then hold it.
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

/// A running session the other computer ended to carry a control-record change. Same answer as a
/// display change: the worker ends cleanly and the link opens next to receive the new record.
fn note_control_changed(state: &mut State) {
    log::info!(
        "sharing: the other computer changed who can control which computer; opening the link to sync"
    );
    open_link_for(state, LinkReason::ControlChanged, PEER_CONTROL_CHANGED);
    // Already on screen from the end that classified it; held here so the close cannot drop it.
    state.view.message = PEER_CONTROL_CHANGED.to_owned();
}

/// A display change, not a failure: LocalDisplaysChanged, any end once this computer's displays
/// positively differ from the record, or the peer's change. One read; None proves nothing.
fn displays_changed_end(
    result: &Result<(), SessionFailure>,
    peer_close: LinkClose,
    local_displays_match: impl FnOnce() -> Option<bool>,
) -> Option<DisplayChange> {
    if matches!(result, Err(SessionFailure::LocalDisplaysChanged))
        || local_displays_match() == Some(false)
    {
        return Some(DisplayChange::Local);
    }
    (peer_close == LinkClose::PeerDisplaysChanged).then_some(DisplayChange::Peer)
}

/// The other computer ended its session to change who controls whom. Never true for the computer
/// that flipped it: its own stop is deliberate and returns before this is read.
fn peer_control_change_end(peer_close: LinkClose) -> bool {
    peer_close == LinkClose::PeerControlChanged
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
    // Only "nothing fits" asks the user for anything, so only it brings the window forward.
    if kind == DisplayNotice::Waiting {
        state.notice_window_pending = true;
    }
    true
}

/// "Nothing fits" for these displays: the supervisor proposes nothing more for them until they
/// change or a layout is applied. A banner the user dismissed for them stays down.
fn answer_waiting(state: &mut State, geometry: DisplayGeometry) {
    let dismissed = state.display_notice.is_none()
        && state
            .notice_geometry
            .as_ref()
            .is_some_and(|raised| raised.same(&geometry));
    state.notice_geometry = Some(geometry);
    state.notice_answer = Some(DisplayNotice::Waiting);
    state.notice_left_out = false;
    if !dismissed {
        state.display_notice = Some(DisplayNotice::Waiting);
        state.notice_window_pending = true;
    }
    // A control change riding on these proposals cannot land either; the switches show the record.
    if state.pending_control.take().is_some() {
        state.view.message = CONTROL_REFUSED.into();
    }
}

/// A refusal that says nothing about the layout itself: the displays count as unanswered, so the
/// next pass proposes again after a short wait, until the retries for them run out.
fn note_passing_refusal(state: &mut State, geometry: DisplayGeometry) {
    let count = match &state.proposal_retry {
        Some((refused, count, _)) if refused.same(&geometry) => count.saturating_add(1),
        _ => 1,
    };
    state.proposal_retry = Some((geometry.clone(), count, Instant::now()));
    if count >= MAX_PROPOSAL_RETRIES {
        answer_waiting(state, geometry);
    } else if state
        .notice_geometry
        .as_ref()
        .is_some_and(|raised| raised.same(&geometry))
    {
        state.notice_geometry = None;
        state.notice_answer = None;
    }
}

/// Drops flips the record already holds, or every flip when together they would leave neither
/// computer in control (only a change made on the other computer can); true in that case.
fn reconcile_pending_control(state: &mut State) -> bool {
    let Some(pending) = state.pending_control.as_mut() else {
        return false;
    };
    let Some(active) = &state.active_control else {
        state.pending_control = None;
        return false;
    };
    pending.retain(|key, allowed| active.control.get(key) != Some(allowed));
    let superseded = pending.keys().any(|key| !active.control.contains_key(key))
        || !overlaid(&active.control, Some(pending))
            .values()
            .any(|allowed| *allowed);
    if superseded || pending.is_empty() {
        state.pending_control = None;
    }
    superseded
}

/// The control map a layout made here carries: the active record's with any pending flips, or
/// both directions for a pair that has no record yet. The window never chooses it.
fn control_for(state: &State, inspection: &InspectedPeer) -> ControlMap {
    let local = fingerprint_key(&inspection.local_fingerprint.full_hex());
    let peer = fingerprint_key(&inspection.peer_fingerprint.full_hex());
    state
        .active_control
        .as_ref()
        .filter(|active| active.local == local && active.peer == peer)
        .map(|active| overlaid(&active.control, state.pending_control.as_ref()))
        .filter(|control| sharing_preferences::validate_control(control, &local, &peer).is_ok())
        .unwrap_or_else(|| both_directions(&local, &peer))
}

fn control_view(state: &State) -> Option<ControlView> {
    let active = state.active_control.as_ref()?;
    let control = overlaid(&active.control, state.pending_control.as_ref());
    Some(ControlView {
        local_to_peer: control.get(&active.local) == Some(&true),
        peer_to_local: control.get(&active.peer) == Some(&true),
        syncing: state.pending_control.is_some(),
    })
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
fn remember_applied(setup_path: &Path, setup: &SharingPreferences) {
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
    while NativeSessionClaim::is_claimed() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
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
            state.displays_unreadable_since = None;
            // Both computers are on the link now, so the reason that chose a link over a session
            // is spent and the ordinary decision path runs again.
            if state.link_reason == Some(LinkReason::PeerHoldsLink) {
                state.link_reason = None;
            }
            state.view.message = connected_message(&state).into();
        }
        LinkEvent::PeerArranging { arranging } => {
            state.peer_arranging = arranging;
            if !state.editing && state.view.phase == "connected" {
                state.view.message = if arranging {
                    CONNECTED_PEER_ARRANGING.to_owned()
                } else {
                    connected_message(&state).to_owned()
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
            Ok((record, _)) => {
                state.view.local_displays = display_views(&inspection.local_displays);
                state.view.peer_displays = display_views(&inspection.peer_displays);
                state.view.synchronized_layout = Some(record.layout().clone());
                state.inspection = Some(inspection);
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
                state.proposal_retry = None;
                // Both computers' switches follow the commit, whichever computer flipped them.
                if state.active.as_deref()
                    == Some(fingerprint_key(record.peer_fingerprint()).as_str())
                {
                    state.active_control = Some(ActiveControl::of(&record));
                    if reconcile_pending_control(&mut state) {
                        state.view.message = CONTROL_SUPERSEDED.into();
                    }
                }
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

/// Geometry may have changed with every connect, so the previous authorization is spent and the
/// decider waits for the displays to hold still again.
fn adopt_inspection(state: &mut State, inspection: InspectedPeer) {
    state.displays_changed_at = Some(Instant::now());
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
    // Only a supervisor proposal is retried or answered; a user's Apply is theirs to repeat. The
    // other computer finding the layout unusable is the one refusal that says "nothing fits".
    let flip_pending = state.pending_control.is_some();
    if let Some(geometry) = state.pending_proposal.take().filter(|_| sending) {
        if reason == LinkRejectReason::Invalid {
            answer_waiting(state, geometry);
        } else {
            note_passing_refusal(state, geometry);
        }
    }
    let disagreed =
        !sending && reason == LinkRejectReason::SaveFailed && state.view.sync.state == "applied";
    let message = if disagreed {
        ONLY_THIS_COMPUTER_SAVED
    } else if flip_pending && state.pending_control.is_none() {
        CONTROL_REFUSED
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

fn connected_message(state: &State) -> &'static str {
    if state.pending_control.is_some() {
        return CONNECTED_CHANGING_CONTROL;
    }
    match state.link_reason {
        None | Some(LinkReason::PeerHoldsLink) => CONNECTED_ARRANGE,
        Some(LinkReason::LayoutMisfit) => CONNECTED_LAYOUT_STALE,
        Some(LinkReason::LayoutUnconfirmed) => ONLY_THIS_COMPUTER_SAVED,
        Some(LinkReason::ControlChanged) => CONNECTED_CHANGING_CONTROL,
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
                let (staged, left_out) = state.staged.take().ok_or(LinkRejectReason::SaveFailed)?;
                applied = Some((staged.clone(), left_out));
                Ok(staged)
            })?;
            if let Some((applied, left_out)) = applied {
                resolve_notice_on_commit(&mut lock(&commit_state));
                // A layout that had to leave something out is a stopgap, not an arrangement.
                if !left_out {
                    remember_applied(&commit_path, &applied);
                }
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
    let staged = sharing_preferences::shared_setup_for_inspection(fresh, bytes).map_err(
        |error| match error {
            PreferenceError::InspectionChanged => LinkRejectReason::InspectionChanged,
            PreferenceError::Invalid => LinkRejectReason::Invalid,
        },
    )?;
    {
        let state = lock(shared);
        if link_retired(&state, generation, epoch) {
            return Err(LinkRejectReason::Cancelled);
        }
        if state
            .inspection
            .as_ref()
            .is_some_and(|current| !current.matches(fresh))
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
    state.staged = Some(staged);
    Ok(())
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

/// Platform and control fields are derived here so no call site can publish a stale pairing.
fn view_of(state: &State) -> SharingView {
    let mut view = state.view.clone();
    view.link.since = state
        .link_since
        .map(|since| human_duration(since.elapsed()))
        .unwrap_or_default();
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
    view.peer_fingerprint = state.peer.map(|peer| fingerprint_key(&peer.full_hex()));
    view.active = state.active.clone();
    view.editing = state.editing;
    view.display_notice = state.display_notice;
    view.control = control_view(state);
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
        LinkClose::PeerDisplaysChanged => "the other computer's displays changed",
        LinkClose::PeerControlChanged => {
            "the other computer changed who can control which computer"
        }
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
    state.native_cleanup_pending = NativeSessionClaim::is_claimed();
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
            .map_err(|_| "Choose a valid edge span.")?,
        parse_display(&link.to_display)?,
        parse_edge(&link.to_edge)?,
        NormalizedSpan::new(link.to_span[0], link.to_span[1])
            .map_err(|_| "Choose a valid edge span.")?,
        link.hysteresis,
    )
    .map_err(|_| "This crossing has invalid display geometry.".into())
}
/// Symmetric: a crossing, each in both directions, none overlapping on an edge or on either
/// computer's own seams, and both outbound topologies build. Returns this computer's.
pub(crate) fn validated_layout(
    inspected: &InspectedPeer,
    layout: &LayoutRequest,
) -> Result<monhop_core::Topology, String> {
    if layout.links.len() > 64 {
        return Err("The layout supports at most 64 directed edges.".into());
    }
    let links = layout
        .links
        .iter()
        .map(parse_link)
        .collect::<Result<Vec<_>, _>>()?;
    if links.is_empty() {
        return Err("Move the displays together until they touch.".into());
    }
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
    sharing_preferences::validate_control(
        &layout.control,
        &inspected.local_fingerprint.full_hex(),
        &inspected.peer_fingerprint.full_hex(),
    )
    .map_err(|_| "Choose which computer can control the other.".to_owned())?;
    if links
        .iter()
        .any(|link| !links.iter().any(|other| reverses(link, other)))
    {
        return Err("Every crossing must work in both directions.".into());
    }
    if links.iter().enumerate().any(|(index, link)| {
        links[index + 1..].iter().any(|other| {
            other.from_display == link.from_display
                && other.from_edge == link.from_edge
                && other.from_span.start() < link.from_span.end()
                && link.from_span.start() < other.from_span.end()
        })
    }) {
        return Err("Crossings on the same display edge must not overlap.".into());
    }
    let hidden: Vec<DisplayId> = layout
        .arrangement
        .iter()
        .flat_map(|arrangement| arrangement.hidden.iter())
        .map(|id| parse_display(id))
        .collect::<Result<_, _>>()?;
    let offset = peer_offset(inspected, layout);
    let own_seam = |_| {
        "A crossing sits on an edge where one computer's own displays meet. Use a free edge."
            .to_owned()
    };
    let topology = inspected
        .outbound_topology(links.clone(), &hidden, offset)
        .map_err(own_seam)?;
    mirrored(inspected)
        .outbound_topology(links, &hidden, Point::new(-offset.x, -offset.y))
        .map_err(own_seam)?;
    Ok(topology)
}

/// `other` crosses the same seam as `link`, the other way.
fn reverses(link: &EdgeLink, other: &EdgeLink) -> bool {
    other.from_display == link.to_display
        && other.from_edge == link.to_edge
        && other.from_span == link.to_span
        && other.to_display == link.from_display
        && other.to_edge == link.from_edge
        && other.to_span == link.from_span
}

/// The same pair as the other computer inspects it.
fn mirrored(inspected: &InspectedPeer) -> InspectedPeer {
    InspectedPeer {
        local_device: inspected.peer_device,
        peer_device: inspected.local_device,
        local_fingerprint: inspected.peer_fingerprint,
        peer_fingerprint: inspected.local_fingerprint,
        local_platform: inspected.peer_platform,
        peer_platform: inspected.local_platform,
        local_displays: inspected.peer_displays.clone(),
        peer_displays: inspected.local_displays.clone(),
        interface_id: inspected.interface_id.clone(),
    }
}

/// The other block's offset from this one's. Crossings are explicit, so it only translates
/// coordinates; one the session cannot reproduce exactly on both computers becomes zero.
fn peer_offset(inspected: &InspectedPeer, layout: &LayoutRequest) -> Point {
    let local = sharing_preferences::snapshots(&inspected.local_displays);
    let peer = sharing_preferences::snapshots(&inspected.peer_displays);
    let Some((Some(here), Some(there))) = layout.arrangement.as_ref().map(|arrangement| {
        (
            sharing_preferences::block_translation(arrangement, &local),
            sharing_preferences::block_translation(arrangement, &peer),
        )
    }) else {
        return Point::default();
    };
    let offset = Point::new((there[0] - here[0]).round(), (there[1] - here[1]).round());
    // The session checks each display's translation exactly, on this computer and the other.
    let exact = |topology: &session_setup::DisplayTopology, sign: f64| {
        topology.displays().iter().all(|display| {
            let Point { x, y } = display.logical_origin;
            (x + sign * offset.x) - x == sign * offset.x
                && (y + sign * offset.y) - y == sign * offset.y
        })
    };
    if offset.is_finite()
        && exact(&inspected.peer_displays, 1.0)
        && exact(&inspected.local_displays, -1.0)
    {
        offset
    } else {
        Point::default()
    }
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
    match error {
        SetupFailure::Identity => "The protected identity could not be read. Check pairing.",
        SetupFailure::PairingRequired => "Pair both computers before setting up sharing.",
        SetupFailure::NetworkSelection => "Select a connected physical Wi-Fi or Ethernet interface.",
        SetupFailure::NetworkRoute => {
            "The peer must use the selected direct physical network. Check for a VPN route or changed address."
        }
        SetupFailure::PortBusy => {
            "The MonHop port on this computer is still closing. Try again in a moment."
        }
        SetupFailure::Connection => {
            "The connection did not finish. Check that MonHop is open on the other computer."
        }
        SetupFailure::PeerIdentityChanged => {
            "The other computer is using a different identity than the one paired. Pair the computers again."
        }
        SetupFailure::Handshake => {
            "The computers did not agree on identity or version. Check that both run the same build."
        }
        SetupFailure::Cancelled => "The connection was cancelled or its physical network changed.",
        SetupFailure::Displays => "Current display information could not be read.",
        SetupFailure::ChangedSinceInspection => {
            "The identity, network, displays or control settings changed. Connect again."
        }
        SetupFailure::Layout => "The display layout is invalid.",
        SetupFailure::PurposeMismatch => {
            "The other computer is sharing input while this one arranges displays. Choose Change layout there too, or apply here."
        }
        SetupFailure::VersionMismatch => {
            "The other computer runs a different MonHop version. Install the same build on both computers."
        }
    }
    .into()
}
fn session_message(error: SessionFailure) -> String {
    match error {
        SessionFailure::NativeCleanup => native_cleanup_message(),
        SessionFailure::LocalDisplaysChanged => {
            "This computer's displays changed. Sharing reconnects with an updated layout."
        }
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
            "Input arrived faster than MonHop allows. Move more slowly; sharing reconnects on its own."
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
            "The computers disagreed during input setup. Check both have the latest build; sharing reconnects on its own."
        }
        Failure::DisplayUnavailable => {
            "Display information could not be read. Check the display arrangement; sharing reconnects on its own."
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
        Failure::DesktopUnavailable => {
            "Windows is not on an available normal desktop. Close any lock or security screen normally; sharing reconnects on its own."
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
            "Native input capture could not start. Check input permissions; sharing reconnects on its own."
        }
    };
    format!("{explanation} [Capture: {reason:?}]")
}

#[cfg(test)]
pub(crate) mod tests {
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

    pub(crate) struct LinkFixture {
        pub(crate) events: UnboundedSender<LinkEvent>,
        pub(crate) proposals: std::sync::mpsc::Receiver<Vec<u8>>,
        /// Every arranging state the controller told the link to publish, in order.
        arranging: std::sync::mpsc::Receiver<bool>,
        /// What the real link calls to stage and commit an agreed layout on this computer.
        pub(crate) persist: LinkPersist,
    }

    pub(crate) fn fake_link(run: LinkRun) -> LinkFuture {
        Box::pin(async move {
            let LinkRun {
                cancel,
                events,
                mut commands,
                persist,
                ..
            } = run;
            let (proposed, proposals) = std::sync::mpsc::channel();
            let (arranged, arranging) = std::sync::mpsc::channel();
            if let Some(sender) = lock(&LINK_FIXTURES).as_ref() {
                let _ = sender.send(LinkFixture {
                    events: events.clone(),
                    proposals,
                    arranging,
                    persist,
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
        SharingController::with_link_runner(runner, idle_window)
    }

    fn link_controller(idle_window: Duration) -> (SharingController, LinkFixture) {
        let controller = controller_with(fake_link, idle_window);
        let (directory, path) = sync_test_path();
        std::mem::forget(directory);
        let fixture = open_fake_link(&controller, path, fixture_peer());
        (controller, fixture)
    }

    /// Opens `controller`'s link on the fake runner and returns that link's fixture.
    pub(crate) fn open_fake_link(
        controller: &SharingController,
        path: std::path::PathBuf,
        peer: CertificateFingerprint,
    ) -> LinkFixture {
        let (sender, fixtures) = std::sync::mpsc::channel();
        *lock(&LINK_FIXTURES) = Some(sender);
        controller
            .connect_link(path, "en0:4:192.168.1.4".into(), peer)
            .expect("the link must start");
        fixtures
            .recv_timeout(Duration::from_secs(2))
            .expect("the fake link runner must start")
    }

    pub(crate) fn sync_state(view: &SharingView) -> &'static str {
        view.sync.state
    }

    /// Backdates the link's last display change so the decider may propose at once.
    pub(crate) fn settle_displays(controller: &SharingController) {
        lock(&controller.state).displays_changed_at =
            Some(Instant::now() - DISPLAYS_SETTLE - Duration::from_millis(1));
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

    pub(crate) fn wait_for(
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
        let bytes = sharing_preferences::shared_setup_bytes(&inspection, layout, false).unwrap();
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

    pub(crate) fn join_finished_worker(controller: &SharingController) {
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
        lock(&controller.state).inspection = Some(inspected);
        let before = controller.save_setup(&path, "4", layout.clone()).unwrap();
        assert!(serde_json::to_value(before).unwrap()["layout"].is_object());
        let saved_bytes = std::fs::read(&path).unwrap();
        let file = SetupFile::load(&path).unwrap();
        assert_eq!(file.active(), Some("b".repeat(64).as_str()));
        assert_eq!(
            file.active_computer().map(|saved| saved.layout()),
            Some(&layout)
        );
        controller.stop_with(NOT_CONNECTED);
        assert!(controller.save_setup(&path, "4", layout.clone()).is_err());
        assert!(controller.apply_setup("4", layout).is_err());
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
        lock(&controller.state).inspection = Some(inspected);
        assert!(
            controller
                .save_arrangement(&path, "3", "Desk", layout.clone())
                .is_err()
        );
        let listed = controller
            .save_arrangement(&path, "4", " Desk ", layout.clone())
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].name.as_str(), listed[0].crossings), ("Desk", 1));
        assert_eq!(listed[0].layout.as_ref(), Some(&layout));
        layout.arrangement = Some(ArrangementRequest {
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
        assert_eq!(replaced[0].layout.as_ref(), Some(&layout));
        controller
            .save_arrangement(&path, "4", "Couch", layout.clone())
            .unwrap();
        let listed = controller.arrangements(&path).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(
            listed
                .iter()
                .all(|entry| entry.layout.is_some() && entry.fits)
        );
        // The saved library never carries an enabled switch.
        let file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(file["arrangements"][0]["setup"]["sharingEnabled"].is_null());
        controller.stop_with(NOT_CONNECTED);
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
            // Only "nothing fits" asks the user for anything, so only it brings the window up.
            assert_eq!(
                controller.take_window_forward(),
                kind == DisplayNotice::Waiting
            );
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
        // The decider had to drop a crossing or a display to keep sharing going.
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
            pending_proposal: Some(DisplayGeometry::of(&inspected)),
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
        // same way, and they name the same layout and the same control map.
        let (staged, left_out) =
            sharing_preferences::shared_setup_for_inspection(&inspection, &proposed).unwrap();
        assert_eq!(staged.layout(), next.layout());
        assert!(!left_out);
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
        controller.stop_with(NOT_CONNECTED);
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
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
    }

    #[test]
    fn a_proposal_lost_with_its_link_is_made_again_while_a_commit_settles_it() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, inspection) = connected_link(Duration::from_secs(600));
        let next = crate::sharing_preferences::tests::preferences();
        let (controller, fixture, inspection) = (controller, _fixture, inspection);
        controller.propose_layout(&next, false).unwrap();
        assert!(controller.proposal_pending_for(&inspection));
        // The link drops before either computer commits; the real link refuses the exchange
        // first, then reports the peer gone.
        fixture
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::Cancelled,
                sending: true,
            })
            .unwrap();
        fixture
            .events
            .send(LinkEvent::Disconnected {
                reason: session_link::LinkDisconnect::PeerClosed,
            })
            .unwrap();
        join_finished_worker(&controller);
        // Nothing is in flight any more, and neither a banner nor the refusal stands in for an
        // answer: the supervisor proposes again for these same displays once the short wait ends.
        assert!(!controller.proposal_pending_for(&inspection));
        assert!(!controller.waiting_notice_for(&inspection));
        assert!(controller.retry_wait_for(&inspection));
        lock(&controller.state).proposal_retry.as_mut().unwrap().2 -= PROPOSAL_RETRY_AFTER;
        assert!(!controller.retry_wait_for(&inspection));

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
        controller.stop_with(NOT_CONNECTED);
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
    fn local_displays_changed_is_a_clean_transition() {
        // The transport's own verdict needs no read, whoever closed first.
        for close in [LinkClose::Local, LinkClose::PeerFailed, LinkClose::Open] {
            assert_eq!(
                displays_changed_end(&Err(SessionFailure::LocalDisplaysChanged), close, || {
                    panic!("the verdict is already known")
                }),
                Some(DisplayChange::Local)
            );
        }
        // The other computer's displays changed: a transition here too, never a failure line.
        assert_eq!(
            displays_changed_end(
                &Err(SessionFailure::Wire),
                LinkClose::PeerDisplaysChanged,
                || { Some(true) }
            ),
            Some(DisplayChange::Peer)
        );
        assert_eq!(
            displays_changed_end(&Ok(()), LinkClose::PeerDisplaysChanged, || None),
            Some(DisplayChange::Peer)
        );
        // Any end once this computer's displays positively differ from the record.
        assert_eq!(
            displays_changed_end(&Err(SessionFailure::Wire), LinkClose::PeerFailed, || {
                Some(false)
            }),
            Some(DisplayChange::Local)
        );
        assert_eq!(
            displays_changed_end(&Ok(()), LinkClose::PeerEnded, || Some(false)),
            Some(DisplayChange::Local)
        );
        // Displays that could not be read say nothing either way, so the failure stands.
        assert!(
            displays_changed_end(&Err(SessionFailure::Wire), LinkClose::PeerFailed, || None)
                .is_none()
        );
        // Real failures with displays that still match keep their error arm and its backoff.
        for failure in [
            SessionFailure::QueueFull,
            SessionFailure::InvalidLayout,
            SessionFailure::SourceController(SourceFailure::Topology),
            SessionFailure::DestinationActor(
                monhop_transport::session_actor::ActorFailure::Native,
                3,
            ),
        ] {
            assert!(
                displays_changed_end(&Err(failure), LinkClose::Local, || Some(true)).is_none(),
                "{failure:?}"
            );
        }
        assert!(
            displays_changed_end(&Err(SessionFailure::Wire), LinkClose::PeerFailed, || {
                Some(true)
            })
            .is_none()
        );
        assert_eq!(DisplayChange::Local.message(), DISPLAYS_CHANGED);
        assert_eq!(DisplayChange::Peer.message(), PEER_DISPLAYS_CHANGED);
    }

    #[test]
    fn a_peer_control_change_is_recognized_only_by_its_own_close_reason() {
        assert!(peer_control_change_end(LinkClose::PeerControlChanged));
        for close in [
            LinkClose::Open,
            LinkClose::PeerEnded,
            LinkClose::PeerFailed,
            LinkClose::PeerRevoked,
            LinkClose::PeerDisplaysChanged,
            LinkClose::PeerClosed,
            LinkClose::IdleTimeout,
            LinkClose::Local,
            LinkClose::Transport,
        ] {
            assert!(!peer_control_change_end(close), "{close:?}");
        }
    }

    #[test]
    fn a_revoked_or_native_end_with_changed_local_displays_is_a_display_change() {
        // macOS revokes the session when its displays reconfigure, and this computer may end
        // first with no word from the other one: the mismatch alone decides.
        for failure in [SessionFailure::Revoked, SessionFailure::Native] {
            assert_eq!(
                displays_changed_end(&Err(failure), LinkClose::Open, || Some(false)),
                Some(DisplayChange::Local),
                "{failure:?}"
            );
            assert!(
                displays_changed_end(&Err(failure), LinkClose::Open, || Some(true)).is_none(),
                "{failure:?}"
            );
        }
    }

    #[test]
    fn a_session_the_displays_outgrew_ends_the_worker_for_the_link_without_a_backoff() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        // What a session records when either computer's displays change under it.
        note_displays_changed(&mut lock(&controller.state), DISPLAYS_CHANGED);
        assert!(controller.holds_link());
        close_link(&mut lock(&controller.state), DISPLAYS_CHANGED);
        join_finished_worker(&controller);
        let view = controller.status();
        // Not an error and not a drop, so the supervisor opens the link on its next pass
        // instead of waiting out ten seconds.
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, DISPLAYS_CHANGED);
        assert!(view.last_failure.is_empty());
        assert!(lock(&controller.state).worker_failed_at.is_none());
        assert!(!controller.within_failure_backoff(Duration::from_secs(10)));
        assert!(controller.holds_link());
        assert!(controller.link_is_off());
    }

    #[test]
    fn a_session_the_other_computer_ended_for_control_reopens_the_link_without_a_backoff() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        // What a session records when the other computer ends it to change control instead.
        note_control_changed(&mut lock(&controller.state));
        assert!(controller.holds_link());
        close_link(&mut lock(&controller.state), PEER_CONTROL_CHANGED);
        join_finished_worker(&controller);
        let view = controller.status();
        // Not an error and not a drop, so the supervisor opens the link on its next pass
        // instead of waiting out ten seconds.
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, PEER_CONTROL_CHANGED);
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
        lock(&controller.state).inspection = Some(inspected.clone());
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
        controller.stop_with(NOT_CONNECTED);
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
        controller.stop_with(NOT_CONNECTED);
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
            control: ControlMap::new(),
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
        for positions in [
            vec![position("1", -5.5, 0.0), position("2", 1920.0, 0.25)],
            vec![position("2", 1920.0, 0.0)],
            vec![],
        ] {
            let ok = ArrangementRequest {
                positions,
                hidden: vec![],
            };
            assert!(validate_arrangement(&arranged_layout(ok), &displays).is_ok());
        }
        let mut without_arrangement = arranged_layout(ArrangementRequest {
            positions: vec![],
            hidden: vec![],
        });
        without_arrangement.arrangement = None;
        assert!(validate_arrangement(&without_arrangement, &displays).is_ok());
        for positions in [
            vec![position("1", 0.0, 0.0), position("1", 1.0, 1.0)],
            vec![position("9", 0.0, 0.0)],
            vec![position("1", f64::NAN, 0.0)],
            vec![position("1", 0.0, 20_000_001.0)],
        ] {
            let bad = ArrangementRequest {
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
        // A hidden display that is also placed, and unknown or duplicate hidden ids.
        assert!(validate_arrangement(&free(all_but(&["9"]), vec!["4"]), &displays).is_err());
        assert!(validate_arrangement(&free(all_but(&["4"]), vec!["9"]), &displays).is_err());
        assert!(validate_arrangement(&free(all_but(&["4"]), vec!["4", "4"]), &displays).is_err());
        // A hidden display is never a link end.
        assert!(validate_arrangement(&free(all_but(&["2"]), vec!["2"]), &displays).is_err());
        let mut linked = free(all_but(&["4"]), vec!["4"]);
        linked.links[0].to_display = "4".into();
        assert!(validate_arrangement(&linked, &displays).is_err());
        // A differently spelled id still names the hidden display.
        let mut spelled = free(all_but(&["4"]), vec!["4"]);
        spelled.links[0].to_display = "04".into();
        assert!(validate_arrangement(&spelled, &displays).is_err());
        assert!(validate_arrangement(&free(all_but(&["4"]), vec!["04"]), &displays).is_err());
    }

    #[test]
    fn a_hidden_copy_beside_a_crossing_keeps_the_crossing_usable() {
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
        // The Mac's copy (3) sits right of its display 1; the Windows copy (4) right of 2.
        let inspected = InspectedPeer {
            local_device: DeviceId([1; 16]),
            peer_device: DeviceId([2; 16]),
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
            links: vec![
                link("1", "right", "2", "left"),
                link("2", "left", "1", "right"),
            ],
            arrangement: Some(ArrangementRequest {
                positions: vec![],
                hidden: vec!["3".into()],
            }),
            control: both_directions(&"a".repeat(64), &"b".repeat(64)),
        };
        let topology = validated_layout(&inspected, &layout).unwrap();
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

    /// Two 100x100 displays on this computer, 2 under 3, and the other computer's display 1.
    fn stacked_pair(stack_is_local: bool) -> InspectedPeer {
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
        let stack = DisplayTopology::new(vec![
            describe(2, 0.0, 100.0, true),
            describe(3, 0.0, 0.0, false),
        ])
        .unwrap();
        let single = DisplayTopology::new(vec![describe(1, 0.0, 0.0, true)]).unwrap();
        let (local_displays, peer_displays) = if stack_is_local {
            (stack, single)
        } else {
            (single, stack)
        };
        InspectedPeer {
            local_device: DeviceId([1; 16]),
            peer_device: DeviceId([2; 16]),
            local_fingerprint: fingerprint("a"),
            peer_fingerprint: fingerprint("b"),
            local_platform: Platform::MacOs,
            peer_platform: Platform::Windows,
            local_displays,
            peer_displays,
            interface_id: "en0".into(),
        }
    }

    fn full_link(from: &str, from_edge: &str, to: &str, to_edge: &str) -> LinkRequest {
        LinkRequest {
            from_display: from.into(),
            from_edge: from_edge.into(),
            from_span: [0.0, 1.0],
            to_display: to.into(),
            to_edge: to_edge.into(),
            to_span: [0.0, 1.0],
            hysteresis: 1.0,
        }
    }

    fn both_ways(links: Vec<LinkRequest>, hidden: &[&str]) -> LayoutRequest {
        LayoutRequest {
            links,
            arrangement: Some(ArrangementRequest {
                positions: vec![],
                hidden: hidden.iter().map(|id| (*id).to_owned()).collect(),
            }),
            control: both_directions(&"a".repeat(64), &"b".repeat(64)),
        }
    }

    #[test]
    fn crossing_on_either_native_seam_is_rejected() {
        // The crossing uses display 2's top edge, where that computer's OS already joins 2 to 3.
        let links = vec![
            full_link("1", "bottom", "2", "top"),
            full_link("2", "top", "1", "bottom"),
        ];
        for stack_is_local in [true, false] {
            let inspected = stacked_pair(stack_is_local);
            let error = validated_layout(&inspected, &both_ways(links.clone(), &[])).unwrap_err();
            assert!(error.contains("own displays meet"), "{error}");
            // The other computer validates the same record the same way.
            assert!(
                validated_layout(&mirrored(&inspected), &both_ways(links.clone(), &[])).is_err()
            );
            // Marked not in use, 3 frees the seam: still known, never the pointer's display.
            let topology = validated_layout(&inspected, &both_ways(links.clone(), &["3"])).unwrap();
            assert!(
                validated_layout(&mirrored(&inspected), &both_ways(links.clone(), &["3"])).is_ok()
            );
            if stack_is_local {
                assert!(!topology.display(DisplayId(3)).unwrap().in_use);
                assert!(topology.display(DisplayId(2)).unwrap().in_use);
            }
        }
        // A free edge is fine from both sides.
        let free_edge = vec![
            full_link("1", "right", "2", "left"),
            full_link("2", "left", "1", "right"),
        ];
        for stack_is_local in [true, false] {
            let inspected = stacked_pair(stack_is_local);
            assert!(validated_layout(&inspected, &both_ways(free_edge.clone(), &[])).is_ok());
            assert!(
                validated_layout(&mirrored(&inspected), &both_ways(free_edge.clone(), &[])).is_ok()
            );
        }
    }

    #[test]
    fn crossing_missing_reverse_is_rejected() {
        let inspected = stacked_pair(true);
        let there = full_link("1", "right", "2", "left");
        let back = full_link("2", "left", "1", "right");
        for one_way in [vec![there.clone()], vec![back.clone()]] {
            let error = validated_layout(&inspected, &both_ways(one_way, &[])).unwrap_err();
            assert_eq!(error, "Every crossing must work in both directions.");
        }
        // The return must cross the same stretch of the same seam.
        let mut shifted = back.clone();
        shifted.to_span = [0.0, 0.5];
        assert!(
            validated_layout(&inspected, &both_ways(vec![there.clone(), shifted], &[])).is_err()
        );
        let mut elsewhere = back.clone();
        elsewhere.from_edge = "right".into();
        assert!(
            validated_layout(&inspected, &both_ways(vec![there.clone(), elsewhere], &[])).is_err()
        );
        assert!(
            validated_layout(
                &inspected,
                &both_ways(vec![there.clone(), back.clone()], &[])
            )
            .is_ok()
        );
        // Two crossings may not claim overlapping stretches of one edge.
        let mut half = full_link("1", "right", "2", "left");
        half.from_span = [0.5, 1.0];
        half.to_span = [0.5, 1.0];
        let mut half_back = full_link("2", "left", "1", "right");
        half_back.from_span = [0.5, 1.0];
        half_back.to_span = [0.5, 1.0];
        let error = validated_layout(
            &inspected,
            &both_ways(vec![there, back, half, half_back], &[]),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Crossings on the same display edge must not overlap."
        );
        // No crossing at all is not a layout.
        assert!(validated_layout(&inspected, &both_ways(vec![], &[])).is_err());
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
                controller.stop_with(NOT_CONNECTED);
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
        controller.stop_with(NOT_CONNECTED);
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
            assert!(view.control.is_none());
        }
        assert_eq!(controller.stop_with(NOT_CONNECTED).phase, "off");
        assert!(lock(&controller.worker).is_none());
        assert!(lock(&controller.state).cancel.is_none());
        assert!(controller.shutdown_ready());
    }
    #[test]
    fn applying_requires_a_link_and_strict_display_identifiers() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        assert!(
            controller
                .apply_setup(
                    "0",
                    LayoutRequest {
                        links: vec![],
                        arrangement: None,
                        control: ControlMap::new(),
                    }
                )
                .is_err()
        );
        for invalid in ["", "-1", "1e3", "1.0", "18446744073709551616"] {
            assert!(parse_display(invalid).is_err());
        }
        assert_eq!(parse_display("18446744073709551615").unwrap().0, u64::MAX);
        assert!(lock(&controller.worker).is_none());
    }

    #[test]
    fn stop_invalidates_a_revision_before_a_worker_starts() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        connected_revision(&controller, 7);
        controller.stop_with(NOT_CONNECTED);
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
        assert_eq!(controller.stop_with(NOT_CONNECTED).phase, "stopping");
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
            monhop_core::NativeSessionClaim::claim().expect("test retains native cleanup");
        assert_eq!(controller.stop_with(NOT_CONNECTED).phase, "stopping");
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
    fn shutdown_latches_and_rejects_later_connection_and_apply() {
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
        assert!(
            controller
                .apply_setup(
                    "0",
                    LayoutRequest {
                        links: vec![],
                        arrangement: None,
                        control: ControlMap::new(),
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn retained_native_cleanup_blocks_invalidation_and_new_connections() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        let ownership =
            monhop_core::NativeSessionClaim::claim().expect("test owns native input cleanup");
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

        begin_close(&mut lock(&controller.state), "fixture stop", false);
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
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
    }

    #[test]
    fn apply_setup_requires_a_connected_link_and_the_current_revision() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let idle = SharingController::default();
        let saved = crate::sharing_preferences::tests::preferences();
        let layout = saved.layout().clone();
        assert!(idle.apply_setup("0", layout.clone()).is_err());

        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        let revision = controller.status().revision;
        assert!(controller.apply_setup("0", layout.clone()).is_err());
        let mut one_way = layout.clone();
        one_way.links.pop();
        assert_eq!(
            error(controller.apply_setup(&revision, one_way)),
            "Every crossing must work in both directions."
        );
        let sending = controller.apply_setup(&revision, layout.clone()).unwrap();
        assert_eq!(sending.sync.state, "sending");
        let proposed = fixture
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the link must receive one proposal");
        assert!(!proposed.is_empty());
        assert_eq!(
            error(controller.apply_setup(&revision, layout)),
            "Wait for the current layout to finish applying."
        );
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
    }

    #[test]
    fn a_rejected_layout_keeps_the_link_and_the_switches() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        let saved = crate::sharing_preferences::tests::preferences();
        controller.adopt_saved(&crate::sharing_preferences::tests::file_with(saved.clone()));
        let before = controller.status().control;
        let sending = controller
            .apply_setup(&controller.status().revision, saved.layout().clone())
            .unwrap();
        assert_eq!(sending.sync.state, "sending");
        fixture
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::Busy,
                sending: true,
            })
            .unwrap();
        let rejected = wait_for(&controller, |view| view.sync.state == "rejected");
        assert_eq!(
            rejected.sync.message,
            "The other computer applied a layout first. Review it, then apply again if you want to change it."
        );
        assert_eq!(rejected.phase, "connected");
        assert_eq!(rejected.control, before);
        controller.stop_with(NOT_CONNECTED);
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
        assert!(controller.inspection_for_switch().is_none());
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
        controller.stop_with(NOT_CONNECTED);
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
    fn ending_arranging_closes_the_link() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        controller.begin_editing();
        let ended = controller.end_editing();
        assert_eq!(ended.phase, "stopping");
        assert!(!ended.editing);
        join_finished_worker(&controller);
        assert_eq!(controller.status().message, EDITING_ENDED);
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
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
    }

    #[test]
    fn a_misfit_outlives_its_link_so_the_next_one_opens_for_the_same_reason() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        // A misfit outlives the link: the next link is opened for the same reason.
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        lock(&controller.state).link_reason = Some(LinkReason::LayoutMisfit);
        controller.stop_with(NOT_CONNECTED);
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
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        drop(directory);
    }

    #[test]
    fn stop_during_connecting_reports_not_connected_after_the_worker_exits() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture) = link_controller(Duration::from_secs(600));
        let stopping = controller.stop_with(NOT_CONNECTED);
        assert_eq!(stopping.phase, "stopping");
        assert!(stopping.busy);
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, NOT_CONNECTED);
        assert!(!view.busy);
        assert!(controller.shutdown_ready());
    }

    fn control(local: bool, peer: bool) -> ControlMap {
        [("a".repeat(64), local), ("b".repeat(64), peer)]
            .into_iter()
            .collect()
    }

    /// Saves the fixture pair's record, carrying `control`, as the active one.
    fn active_file(path: &Path, control: ControlMap) -> SharingPreferences {
        let record = crate::sharing_preferences::tests::preferences()
            .with_control(control)
            .expect("a valid control map");
        crate::sharing_preferences::tests::file_with(record.clone())
            .save(path)
            .unwrap();
        record
    }

    fn switches(local_to_peer: bool, peer_to_local: bool, syncing: bool) -> Option<ControlView> {
        Some(ControlView {
            local_to_peer,
            peer_to_local,
            syncing,
        })
    }

    #[test]
    fn the_view_names_both_switches_and_no_input_side() {
        let (directory, path) = sync_test_path();
        let controller = SharingController::default();
        let view = serde_json::to_value(controller.status()).unwrap();
        assert!(view["control"].is_null());
        for gone in ["sourceSide", "sourcePlatform", "sharingRole"] {
            assert!(view.get(gone).is_none(), "{gone}");
        }
        active_file(&path, control(true, false));
        controller.adopt_saved(&SetupFile::load(&path).unwrap());
        let view = serde_json::to_value(controller.status()).unwrap();
        assert_eq!(
            view["control"],
            serde_json::json!({ "localToPeer": true, "peerToLocal": false, "syncing": false })
        );
        drop(directory);
    }

    #[test]
    fn set_control_refuses_last_direction() {
        let (directory, path) = sync_test_path();
        let controller = SharingController::default();
        let peer = "b".repeat(64);
        // Nothing to flip before the pair has a record.
        assert!(
            controller
                .set_control(&path, &peer, "localToPeer", false)
                .is_err()
        );
        active_file(&path, control(true, true));
        assert!(
            controller
                .set_control(&path, &peer, "sideways", false)
                .is_err()
        );
        assert!(
            controller
                .set_control(&path, &"c".repeat(64), "localToPeer", false)
                .is_err()
        );
        let one = controller
            .set_control(&path, &peer, "localToPeer", false)
            .unwrap();
        assert_eq!(one.control, switches(false, true, true));
        assert_eq!(
            error(controller.set_control(&path, &peer, "peerToLocal", false)),
            LAST_DIRECTION
        );
        assert_eq!(controller.status().control, one.control);
        // Flipping back is no change at all, so nothing is left to sync.
        let back = controller
            .set_control(&path, &peer, "localToPeer", true)
            .unwrap();
        assert_eq!(back.control, switches(true, true, false));
        assert!(!controller.control_pending());
        // A record that allows only one direction refuses turning that one off.
        active_file(&path, control(false, true));
        assert_eq!(
            error(controller.set_control(&path, &peer.to_uppercase(), "peerToLocal", false)),
            LAST_DIRECTION
        );
        assert!(!controller.control_pending());
        drop(directory);
    }

    #[test]
    fn new_layout_defaults_to_both() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let mut layout = crate::sharing_preferences::tests::preferences()
            .layout()
            .clone();
        // The window never chooses the switches: whatever it sends is replaced.
        layout.control = control(false, false);
        let proposed = |controller: &SharingController, fixture: &LinkFixture| {
            controller
                .apply_setup(&controller.status().revision, layout.clone())
                .unwrap();
            let bytes = fixture
                .proposals
                .recv_timeout(Duration::from_secs(1))
                .expect("one proposal");
            let inspection = lock(&controller.state).inspection.clone().unwrap();
            sharing_preferences::shared_setup_for_inspection(&inspection, &bytes)
                .unwrap()
                .0
                .control()
                .clone()
        };
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        assert_eq!(proposed(&controller, &fixture), control(true, true));
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);

        // With a record for the pair, a new layout carries its switches and any pending flip.
        let (directory, path) = sync_test_path();
        let (controller, fixture, _) = connected_link(Duration::from_secs(600));
        active_file(&path, control(true, false));
        controller.adopt_saved(&SetupFile::load(&path).unwrap());
        assert_eq!(proposed(&controller, &fixture), control(true, false));
        fixture
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::Busy,
                sending: true,
            })
            .unwrap();
        wait_for(&controller, |view| view.sync.state == "rejected");
        controller
            .set_control(&path, &"b".repeat(64), "peerToLocal", true)
            .unwrap();
        assert_eq!(proposed(&controller, &fixture), control(true, true));
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        drop(directory);
    }

    #[test]
    fn set_control_while_sharing_produces_one_proposal_and_reconnect() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let saved = active_file(&path, control(true, true));
        let inspection = crate::sharing_preferences::tests::inspection(&saved);
        let controller = controller_with(fake_link, Duration::from_secs(600));
        controller.adopt_saved(&SetupFile::load(&path).unwrap());
        // A running session: busy, no link, and it ends only when asked to.
        controller
            .launch(
                "starting",
                "fixture",
                None,
                WorkerKind::Session,
                Some(fixture_peer()),
                |_, _, _, cancel, _| async move {
                    while !cancel.is_revoked() {
                        tokio::task::yield_now().await;
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert!(!controller.end_session_for_control());
        let flipped = controller
            .set_control(&path, &"b".repeat(64), "peerToLocal", false)
            .unwrap();
        assert_eq!(flipped.control, switches(true, false, true));
        assert!(controller.end_session_for_control());
        // The close is already under way; a second pass does not stop it again.
        assert!(!controller.end_session_for_control());
        join_finished_worker(&controller);
        let ended = controller.status();
        assert_eq!(ended.phase, "off");
        assert_eq!(ended.message, CHANGING_CONTROL);
        assert!(ended.last_failure.is_empty());
        assert!(!controller.within_failure_backoff(Duration::from_secs(10)));
        // The flip keeps a link up instead of a session until both computers commit it.
        assert!(controller.holds_link());
        let fixture = open_fake_link(&controller, path.clone(), fixture_peer());
        fixture
            .events
            .send(LinkEvent::Connected {
                inspection: inspection.clone(),
            })
            .unwrap();
        let connected = wait_for(&controller, |view| view.phase == "connected");
        assert_eq!(connected.message, CONNECTED_CHANGING_CONTROL);
        assert!(!controller.end_session_for_control());
        controller.propose_record(&saved).unwrap();
        assert_eq!(controller.status().message, UPDATING_CONTROL);
        let bytes = fixture
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the flip travels as one proposal");
        let (agreed, _) =
            sharing_preferences::shared_setup_for_inspection(&inspection, &bytes).unwrap();
        assert_eq!(agreed.control(), &control(true, false));
        assert!(controller.propose_record(&saved).is_err());
        (fixture.persist.stage)(&inspection, &bytes).unwrap();
        (fixture.persist.commit)(&inspection, &bytes).unwrap();
        fixture
            .events
            .send(LinkEvent::SyncCompleted {
                inspection: inspection.clone(),
                bytes,
                sending: true,
            })
            .unwrap();
        let closed = wait_for(&controller, |view| view.phase == "off");
        assert_eq!(closed.message, LAYOUT_APPLIED_SHARING_ON);
        assert_eq!(closed.control, switches(true, false, false));
        // Nothing holds a link any more, so the next supervisor pass starts the session.
        assert!(!controller.holds_link());
        assert!(controller.link_is_off());
        join_finished_worker(&controller);
        let committed = SetupFile::load(&path).unwrap();
        let committed = committed.active_computer().unwrap();
        assert_eq!(committed.control(), &control(true, false));
        assert_eq!(
            committed.wire_control(),
            Ok(monhop_protocol::ControlPermissions {
                lower_controls_higher: true,
                higher_controls_lower: false,
            })
        );
        assert!(fixture.proposals.try_recv().is_err());
        drop(directory);
    }

    #[test]
    fn a_passing_refusal_is_retried_three_times_then_waits() {
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let geometry = || DisplayGeometry::of(&inspected);
        let waiting = |state: &State| state.notice_answer == Some(DisplayNotice::Waiting);
        for reason in [
            LinkRejectReason::Busy,
            LinkRejectReason::Cancelled,
            LinkRejectReason::InspectionChanged,
            LinkRejectReason::SaveFailed,
        ] {
            let mut state = State {
                pending_control: Some(control(false, true)),
                ..State::default()
            };
            for attempt in 1..=MAX_PROPOSAL_RETRIES {
                state.pending_proposal = Some(geometry());
                reject_sync(&mut state, reason, true);
                assert!(state.pending_proposal.is_none());
                let (_, count, _) = state.proposal_retry.as_ref().unwrap();
                assert_eq!(*count, attempt);
                assert_eq!(
                    waiting(&state),
                    attempt == MAX_PROPOSAL_RETRIES,
                    "{reason:?}"
                );
            }
            // The last refusal answers these displays; a flip riding on them is dropped.
            assert_eq!(state.display_notice, Some(DisplayNotice::Waiting));
            assert!(state.notice_window_pending);
            assert!(state.pending_control.is_none());
            assert_eq!(state.view.message, CONTROL_REFUSED);
        }
        // The other computer finding the layout unusable is "nothing fits" at once.
        let mut invalid = State {
            pending_proposal: Some(geometry()),
            ..State::default()
        };
        reject_sync(&mut invalid, LinkRejectReason::Invalid, true);
        assert!(waiting(&invalid));
        // Displays that changed start their own count.
        let mut changed = saved.clone();
        changed.set_local_displays_for_test(&["1", "3"]);
        let changed = crate::sharing_preferences::tests::inspection(&changed);
        let mut state = State::default();
        for _ in 1..MAX_PROPOSAL_RETRIES {
            note_passing_refusal(&mut state, geometry());
        }
        note_passing_refusal(&mut state, DisplayGeometry::of(&changed));
        assert!(!waiting(&state));
        // A user's own Apply is theirs to repeat: its refusal leaves no retry behind.
        let mut user = State::default();
        reject_sync(&mut user, LinkRejectReason::Busy, true);
        assert!(user.proposal_retry.is_none());
    }

    #[test]
    fn a_proposal_voided_by_a_display_change_is_made_again() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, inspection) = connected_link(Duration::from_secs(600));
        settle_displays(&controller);
        assert!(controller.displays_settled());
        let next = crate::sharing_preferences::tests::preferences();
        controller.propose_layout(&next, false).unwrap();
        fixture
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::InspectionChanged,
                sending: true,
            })
            .unwrap();
        let mut moved = next.clone();
        moved.move_local_display_for_test("1", [0.0, 240.0]);
        let moved = crate::sharing_preferences::tests::inspection(&moved);
        fixture
            .events
            .send(LinkEvent::TopologyChanged {
                inspection: moved.clone(),
            })
            .unwrap();
        wait_for(&controller, |view| {
            view.local_displays[0].origin == [0.0, 240.0]
        });
        // Never stored as "nothing fits", for the old displays or the new ones.
        assert!(!controller.waiting_notice_for(&inspection));
        assert!(!controller.waiting_notice_for(&moved));
        assert!(!controller.proposal_pending_for(&moved));
        assert!(!controller.retry_wait_for(&moved));
        // The decider waits for the new displays to hold still, then proposes for them.
        assert!(!controller.displays_settled());
        assert!(
            controller
                .inspection_for_switch()
                .is_some_and(|current| current.matches(&moved))
        );
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
    }

    #[test]
    fn the_decider_waits_for_the_displays_to_hold_still() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        assert!(!controller.displays_settled());
        settle_displays(&controller);
        assert!(controller.displays_settled());
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        // Nothing carries the settle past its link.
        assert!(!controller.displays_settled());
    }

    #[test]
    fn a_layout_that_left_something_out_is_never_remembered() {
        for left_out in [true, false] {
            let controller = SharingController::default();
            let (directory, path) = sync_test_path();
            let saved = crate::sharing_preferences::tests::preferences();
            let fresh = crate::sharing_preferences::tests::inspection(&saved);
            let bytes =
                sharing_preferences::shared_setup_bytes(&fresh, saved.layout().clone(), left_out)
                    .unwrap();
            let persist = link_persist(
                Arc::clone(&controller.state),
                0,
                path.clone(),
                Arc::clone(&controller.setup_file),
            );
            (persist.stage)(&fresh, &bytes).unwrap();
            (persist.commit)(&fresh, &bytes).unwrap();
            // Both computers still hold the stopgap, so sharing continues on it.
            assert!(SetupFile::load(&path).unwrap().active_computer().is_some());
            let library =
                ArrangementLibrary::load(&path.with_file_name(ARRANGEMENTS_FILE)).unwrap();
            assert_eq!(library.automatic_fit(&fresh).is_some(), !left_out);
            drop(directory);
        }
    }

    /// A link that reads this computer's displays mid-change.
    fn unreadable_displays_link(run: LinkRun) -> LinkFuture {
        Box::pin(async move {
            drop(run);
            Err(SetupFailure::Displays)
        })
    }

    #[test]
    fn unreadable_displays_are_unsettled_for_a_while_then_a_failure() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let start = Instant::now();
        let mut since = None;
        assert!(still_unsettled(&mut since, start));
        assert!(still_unsettled(
            &mut since,
            start + UNSETTLED_WINDOW - Duration::from_millis(1)
        ));
        assert!(!still_unsettled(&mut since, start + UNSETTLED_WINDOW));

        let controller = controller_with(unreadable_displays_link, Duration::from_secs(600));
        let (directory, path) = sync_test_path();
        controller
            .connect_link(path.clone(), "en0:4:192.168.1.4".into(), fixture_peer())
            .unwrap();
        join_finished_worker(&controller);
        // Quiet: no error, no drop line, no backoff, and the next pass reopens the link.
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, DISPLAYS_UNSETTLED);
        assert!(view.last_failure.is_empty());
        assert!(!controller.within_failure_backoff(Duration::from_secs(10)));
        assert!(controller.link_is_off());
        // Unreadable for the whole window is a failure after all.
        lock(&controller.state).displays_unreadable_since = Some(Instant::now() - UNSETTLED_WINDOW);
        controller
            .connect_link(path, "en0:4:192.168.1.4".into(), fixture_peer())
            .unwrap();
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "error");
        assert_eq!(view.message, setup_message(SetupFailure::Displays));
        assert!(controller.within_failure_backoff(Duration::from_secs(10)));
        drop(directory);
    }

    /// A link whose peer answers with a certificate other than the paired one.
    fn identity_changed_link(run: LinkRun) -> LinkFuture {
        Box::pin(async move {
            drop(run);
            Err(SetupFailure::PeerIdentityChanged)
        })
    }

    #[test]
    fn a_changed_peer_identity_stops_the_worker_and_waits_a_minute() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = controller_with(identity_changed_link, Duration::from_secs(600));
        let (directory, path) = sync_test_path();
        controller
            .connect_link(path.clone(), "en0:4:192.168.1.4".into(), fixture_peer())
            .unwrap();
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "error");
        assert_eq!(
            view.message,
            "The other computer is using a different identity than the one paired. Pair the computers again."
        );
        let (_, floor) = lock(&controller.state).worker_failed_at.unwrap();
        assert_eq!(floor, PEER_IDENTITY_BACKOFF);
        // The ordinary 10 s backoff is long spent before a minute is.
        assert!(controller.within_failure_backoff(Duration::from_secs(10)));
        lock(&controller.state).worker_failed_at =
            Some((Instant::now() - Duration::from_secs(30), floor));
        assert!(controller.within_failure_backoff(Duration::from_secs(10)));
        // A user's choice ends the wait at once.
        controller.set_active(&path, Some(fixture_peer())).unwrap();
        assert!(!controller.within_failure_backoff(Duration::from_secs(10)));
        drop(directory);
    }

    #[test]
    fn identical_failed_attempts_are_logged_once_a_minute() {
        let start = Instant::now();
        let mut log = AttemptLog::default();
        let first = log
            .note(SetupFailure::Connection, 1, Duration::ZERO, start)
            .unwrap();
        assert!(first.starts_with("attempt 1 did not connect (Connection)"));
        for attempt in 2..=30 {
            let at = start + Duration::from_secs(2 * u64::from(attempt - 1));
            assert!(
                log.note(SetupFailure::Connection, attempt, Duration::ZERO, at)
                    .is_none()
            );
        }
        let summary = log
            .note(
                SetupFailure::Connection,
                31,
                Duration::from_secs(62),
                start + ATTEMPT_LOG_INTERVAL,
            )
            .unwrap();
        assert!(summary.contains("attempt 31"));
        assert!(summary.ends_with("after 29 more like the last line"));
        // A different reason is news and is logged at once.
        assert!(
            log.note(
                SetupFailure::Handshake,
                32,
                Duration::ZERO,
                start + ATTEMPT_LOG_INTERVAL
            )
            .is_some()
        );
    }
}
