//! Explicit sharing actions. Constructing or polling this controller performs no native I/O.

use crate::arrangement_library::{ArrangementLibrary, ArrangementView};
use crate::clipboard::ClipboardHub;
use crate::group_record::{
    self, GroupMember, GroupRecord, KnownDisplays, RecordSummary, Stamp, shared_group_bytes,
    shared_group_for_link,
};
use crate::key_names;
use crate::sharing_hub::{
    ClipboardSlot, GroupPlan, SetupLinkRequest, ShareEnd, ShareFailure, ShareRequest, ShareUp,
    SharingNetwork,
};
use crate::sharing_preferences::{
    self, ControlMap, DisplayGeometry, PreferenceError, SavedSetupView, SetupFile, fingerprint_key,
    topology_of,
};
#[cfg(test)]
use monhop_core::Topology;
use monhop_core::{
    DisplayId, Edge, EdgeLink, NativeSessionClaim, NormalizedSpan, Platform, Point,
    RevocationSignal,
};
use monhop_protocol::ControlPermissions;
#[cfg(test)]
use monhop_transport::session_link;
use monhop_transport::{
    crypto::CertificateFingerprint,
    session::{
        self, LinkClose, NativeCaptureStartupFailure, SessionFailure, SessionStartupFailure,
    },
    session_link::{LinkCommand, LinkEvent, LinkPersist, LinkRejectReason},
    session_native,
    session_setup::{self, InspectedPeer, SetupFailure},
    session_source::SourceFailure,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
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
pub(crate) const NOT_CONNECTED: &str = "Not connected. Input is local.";
pub(crate) const PAUSED: &str = "Paused. Input is local.";
pub(crate) const SWITCHING_COMPUTER: &str = "Switching computers.";
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
const SHARING_ENABLED: &str =
    "Sharing enabled. Hold both Control keys and Escape for two seconds to stop.";
/// A session made under a record the group has since replaced ends, to start again under it.
const GROUP_CHANGED: &str = "The layout changed. Reconnecting.";
pub(crate) const ARRANGEMENTS_UNREADABLE: &str =
    "The saved arrangements could not be read. Your applied layout is unchanged.";
/// The arrangement library sits beside the setup file, so every writer of one finds the other.
pub(crate) const ARRANGEMENTS_FILE: &str = "group-arrangements.json";
const SETUP_FROM_NEWER: &str =
    "This setup was saved by a newer version of MonHop. Update MonHop to change it.";
const ARRANGEMENTS_FROM_NEWER: &str =
    "These arrangements were saved by a newer version of MonHop. Update MonHop to change them.";
const CONNECT_TO_APPLY: &str =
    "Connect the computers and check the current displays before applying a layout.";
const CONNECT_TO_SAVE: &str = "Connect both computers again before saving this layout.";
const NEVER_SEEN: &str = "A computer that is switched on has never connected, so its displays are unknown. Connect it once, or switch it off.";
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
/// Retrying cannot change the identity the other computer presents; pairing again does.
const PEER_IDENTITY_BACKOFF: Duration = Duration::from_secs(60);
/// A display list that fails to read mid-change is read again this often, quietly, for this long
/// before it counts as a failure.
const UNSETTLED_POLL: Duration = Duration::from_millis(250);
const UNSETTLED_WINDOW: Duration = Duration::from_secs(5);
/// A record is proposed for the link's displays only once they have held still this long; the
/// session they outgrew has already ended.
const DISPLAYS_SETTLE: Duration = Duration::from_secs(1);
/// A proposal refused for a passing reason is made again after this long, at most this many
/// times for one set of displays before they count as answered with "nothing fits".
const PROPOSAL_RETRY_AFTER: Duration = Duration::from_secs(1);
const MAX_PROPOSAL_RETRIES: u8 = 3;
/// Identical failed dial attempts are logged once, then summarised this often.
const ATTEMPT_LOG_INTERVAL: Duration = Duration::from_secs(60);
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
    /// Moves on whenever what the computer cards draw may have changed; the window then reads
    /// the computers and their layouts again.
    pub(crate) setup_revision: String,
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
    /// While sharing, each key or button held since capture started that keeps the pointer from
    /// crossing, by name; empty otherwise. Names identify keys, so they never reach a log.
    blocking_presses: Vec<String>,
    /// Every computer switched on for sharing, lowercase.
    enabled: Vec<String>,
    /// Sharing is paused; the enabled computers stay chosen.
    paused: bool,
    /// Each enabled or connected computer on its own.
    peers: Vec<PeerView>,
    /// The active group's members as its record holds them.
    members: Vec<MemberView>,
}

/// One computer's connection. The top-level fields above sum these up.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerView {
    fingerprint: String,
    platform: Option<&'static str>,
    /// A `SharingView` phase, or "notPaired" when this computer holds no pairing with it.
    phase: &'static str,
    message: String,
    busy: bool,
    sharing_active: bool,
    held: bool,
    link: LinkView,
    sync: SyncView,
    displays: Vec<DisplayView>,
    diagnostics: DiagnosticsView,
    last_failure: String,
    peer_arranging: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemberView {
    fingerprint: String,
    local: bool,
    platform: &'static str,
    /// Live while the member is connected, else as the record holds them.
    displays: Vec<DisplayView>,
    live: bool,
    record_revision: u64,
}

/// The two switches on Home. While a change made here is syncing, they already show it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlView {
    /// This computer's own entry.
    pub(crate) local_to_peer: bool,
    /// Whether any other member may control this computer.
    pub(crate) peer_to_local: bool,
    pub(crate) syncing: bool,
    /// Every member's entry, for a group of three or more; a pair has only the two above.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) members: Vec<ControlMember>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ControlMember {
    pub(crate) fingerprint: String,
    pub(crate) allowed: bool,
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
            setup_revision: "0".into(),
            local_platform: platform_name(local_platform()),
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
            blocking_presses: Vec::new(),
            enabled: Vec::new(),
            paused: false,
            peers: Vec::new(),
            members: Vec::new(),
        }
    }
}

impl SharingView {
    /// How many computers a link is connected with, and the platform of the one when only one is.
    pub(crate) fn connected_peers(&self) -> (usize, Option<&'static str>) {
        let connected: Vec<&PeerView> = self
            .peers
            .iter()
            .filter(|peer| peer.phase == "connected")
            .collect();
        let platform = match connected.as_slice() {
            [only] => only.platform,
            _ => None,
        };
        (connected.len(), platform)
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
    network: Arc<SharingNetwork>,
}

pub(crate) type LinkFuture = Pin<Box<dyn Future<Output = Result<(), SetupFailure>>>>;
pub(crate) type LinkRunner = fn(LinkRun) -> LinkFuture;
/// This computer's displays, read as the identity its records name it by.
pub(crate) type LocalDisplays = fn(&str) -> Result<session_setup::DisplayTopology, String>;

/// Runs the link on the network thread's shared endpoint; the returned future only waits.
fn transport_link(run: LinkRun) -> LinkFuture {
    let LinkRun {
        interface_id,
        peer,
        cancel,
        persist,
        events,
        commands,
        network,
    } = run;
    network.open_link(SetupLinkRequest {
        interface_id,
        peer,
        cancel,
        persist,
        events,
        commands,
    })
}

/// The active record's control map and the computers it names, mirrored so the view needs no
/// disk read.
#[derive(Clone)]
struct ActiveControl {
    local: String,
    /// Every other member, lowercase and sorted.
    members: Vec<String>,
    control: ControlMap,
}

impl ActiveControl {
    /// None unless `record` names `local`.
    fn of(record: &GroupRecord, local: &str) -> Option<Self> {
        let local = fingerprint_key(local);
        record.has_member(&local).then(|| Self {
            members: record
                .member_keys()
                .into_iter()
                .filter(|member| *member != local)
                .collect(),
            local,
            control: record.layout().control.clone(),
        })
    }
}

/// A session with one enabled computer, on the file's network, under the active group's record.
#[derive(Clone)]
pub(crate) struct SessionPlan {
    interface_id: String,
    /// This computer's fingerprint as the record names it.
    local: String,
    peer: CertificateFingerprint,
    record: GroupRecord,
}

impl SessionPlan {
    /// None while no single computer is chosen, it has no record with this computer yet, or no
    /// network is chosen.
    #[cfg(test)]
    pub(crate) fn of(file: &SetupFile) -> Option<Self> {
        Self::for_peer(file, file.active()?)
    }

    /// None while sharing is paused, `peer` is not enabled, the enabled computers have no record
    /// with this computer yet, or no network is chosen.
    pub(crate) fn for_peer(file: &SetupFile, peer: &str) -> Option<Self> {
        let key = fingerprint_key(peer);
        if !file.sharing_chosen() || !file.enabled().contains(&key) {
            return None;
        }
        Some(Self {
            interface_id: file.interface_id()?.to_owned(),
            local: file.local()?.to_owned(),
            peer: CertificateFingerprint::parse_full(peer).ok()?,
            record: file.active_group()?.clone(),
        })
    }

    pub(crate) fn local(&self) -> &str {
        &self.local
    }

    pub(crate) fn record(&self) -> &GroupRecord {
        &self.record
    }

    /// Whether this computer still shows the displays the record holds for it.
    pub(crate) fn fits_local(&self, displays: &session_setup::DisplayTopology) -> bool {
        self.record.fits_local(&self.local, displays)
    }

    /// What both computers negotiate; None when neither may control the other.
    pub(crate) fn control(&self) -> Option<ControlPermissions> {
        group_record::wire_control(
            &self.record.layout().control,
            &self.local,
            &self.peer.full_hex(),
        )
    }

    /// The pointer topology of a session that met the other computer as `inspection` found
    /// both; None when the record does not fit them, whatever network the session runs on.
    #[cfg(test)]
    pub(crate) fn topology(&self, inspection: &InspectedPeer) -> Option<Topology> {
        if inspection.peer_fingerprint != self.peer
            || self.record.members().len() != 2
            || !self.record.fits_link(inspection)
        {
            return None;
        }
        validated_layout(inspection, self.record.layout()).ok()
    }
}

/// What this computer holds for the whole group, whichever computers are connected.
#[derive(Default)]
struct State {
    /// What Home shows while no worker runs: every change to any computer's view lands here too,
    /// so with one computer it is exactly that computer's view.
    view: SharingView,
    /// The last worker generation handed out to any computer, so no two workers share one.
    generation: u64,
    authorization_revision: u64,
    /// Each write of the setup file or the layouts remembered with it, and each change of the
    /// displays a card draws, moves this on; see `SharingView::setup_revision`.
    setup_revision: u64,
    shutdown: bool,
    native_cleanup_pending: bool,
    /// Mirrors the setup file so the view can show the active computer without a live worker.
    active: Option<String>,
    active_control: Option<ActiveControl>,
    /// The highest revision the setup file, a commit, or the other computer's summary showed: a
    /// record made here is stamped past it.
    clock: u64,
    /// This computer's identity, the enabled computers and the active group's record, mirrored
    /// from the setup file for the summary each link carries.
    local: Option<String>,
    enabled: Vec<String>,
    paused: bool,
    held: Option<GroupRecord>,
    /// Each computer the file's records name, as the newest of them holds it: the displays a
    /// layout made here carries for a member that is not connected.
    seen: BTreeMap<String, GroupMember>,
    /// The content digest of the record shares last ran under, and a count that moves on with
    /// every change of it; a session worker made under an older count is stale.
    group_agreement: Option<[u8; 32]>,
    group_epoch: u64,
    /// Switch flips made here that both computers have not committed yet, over `active_control`.
    pending_control: Option<ControlMap>,
    editing: bool,
    display_notice: Option<DisplayNotice>,
    /// When this computer's displays first failed to read on a dial or link; a good read clears it.
    displays_unreadable_since: Option<Instant>,
    /// Consumed once by the app, which then brings its window forward.
    notice_window_pending: bool,
    /// Each other computer by lowercase fingerprint, from the first time anything concerns it.
    peers: BTreeMap<String, PeerState>,
}

/// What one other computer's worker holds, and what outlives that worker for the computer.
#[derive(Default)]
struct PeerState {
    /// This computer's connection alone, made from the group's view when first written.
    view: Option<SharingView>,
    /// The authorization a session worker registers native input under; any stop for this
    /// computer spends it, leaving the group's.
    authorized: Option<u64>,
    /// Set by the hub once this computer's session finished startup.
    share_started: Option<Arc<AtomicBool>>,
    /// The group epoch this computer's session worker runs under.
    share_epoch: Option<u64>,
    /// The last Share connect found no pairing with this computer; any user choice clears it.
    not_paired: bool,
    /// The generation of this computer's latest worker; an older one's writes are dropped.
    worker_generation: u64,
    /// This computer from its worker's launch until that worker ends.
    running: Option<CertificateFingerprint>,
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
    staged: Option<(GroupRecord, bool)>,
    /// The summary this link last told the other computer; None until it told one.
    summary_sent: Option<Vec<u8>>,
    /// The other computer's summary on this connection; None until it arrives.
    peer_summary: Option<RecordSummary>,
    /// The setup file as this link's commit left it, for the completion that follows.
    committed: Option<SetupFile>,
    /// The other computer's arranging state as the link reports it, never inferred; false
    /// whenever no link worker is alive.
    peer_arranging: bool,
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

/// The key `State::peers` files a computer under.
fn peer_key(peer: &CertificateFingerprint) -> String {
    fingerprint_key(&peer.full_hex())
}

/// `key`'s entry, made on first use. A free function so the group fields stay borrowable.
fn peer_entry<'a>(peers: &'a mut BTreeMap<String, PeerState>, key: &str) -> &'a mut PeerState {
    peers.entry(key.to_owned()).or_default()
}

/// The generation of `key`'s latest worker; 0 before its first.
fn generation_of(state: &State, key: &str) -> u64 {
    state
        .peers
        .get(key)
        .map_or(0, |peer| peer.worker_generation)
}

/// The computer whose worker runs. Only one runs at a time, so whatever concerns "the other
/// computer" without naming one resolves it here.
fn the_peer(state: &State) -> Option<(&str, &PeerState)> {
    state
        .peers
        .iter()
        .find(|(_, peer)| peer.running.is_some())
        .map(|(key, peer)| (key.as_str(), peer))
}

/// One computer's entry, or every computer's for None.
fn scoped<'a>(
    peers: &'a mut BTreeMap<String, PeerState>,
    scope: Option<&'a str>,
) -> impl Iterator<Item = &'a mut PeerState> + 'a {
    peers
        .iter_mut()
        .filter(move |(key, _)| scope.is_none_or(|only| only == key.as_str()))
        .map(|(_, peer)| peer)
}

/// The computer with the newest drop; a user's choice clears every computer's at once.
fn latest_failure(state: &State) -> Option<&PeerState> {
    state
        .peers
        .values()
        .filter(|peer| peer.last_failure.is_some())
        .max_by_key(|peer| peer.last_failure_at)
}

/// Applies `edit` to `key`'s own view, made from the shared one on first use, and to the shared
/// view, which thereby stays exactly what a lone computer's connection shows.
fn show(state: &mut State, key: &str, edit: impl Fn(&mut SharingView)) {
    let shared = &state.view;
    let own = state
        .peers
        .entry(key.to_owned())
        .or_default()
        .view
        .get_or_insert_with(|| shared.clone());
    edit(own);
    edit(&mut state.view);
}

/// The shared view and every computer's own.
fn all_views(state: &mut State) -> impl Iterator<Item = &mut SharingView> {
    std::iter::once(&mut state.view).chain(
        state
            .peers
            .values_mut()
            .filter_map(|peer| peer.view.as_mut()),
    )
}

/// `key`'s own view; the shared one until it has its own.
fn own_view<'a>(state: &'a State, key: &str) -> &'a SharingView {
    state
        .peers
        .get(key)
        .and_then(|peer| peer.view.as_ref())
        .unwrap_or(&state.view)
}

/// Whether `key`'s own worker holds its view busy; another computer's never counts.
fn peer_busy(state: &State, key: &str) -> bool {
    state
        .peers
        .get(key)
        .and_then(|peer| peer.view.as_ref())
        .is_some_and(|view| view.busy)
}

/// The authorization `key`'s session worker must still hold to register native input.
fn peer_authorization(state: &State, key: &str) -> u64 {
    state
        .peers
        .get(key)
        .and_then(|peer| peer.authorized)
        .unwrap_or(state.authorization_revision)
}

pub struct SharingController {
    operation: Mutex<()>,
    state: Arc<Mutex<State>>,
    /// At most one worker per computer, keyed like `State::peers`.
    workers: Mutex<BTreeMap<String, JoinHandle<()>>>,
    /// Shared with each link's commit, so no two writers of the setup file interleave.
    setup_file: Arc<SetupFileLock>,
    link_runner: LinkRunner,
    idle_window: Duration,
    /// Runs every Share session in one hub, and every setup link, on one pinned endpoint.
    network: Arc<SharingNetwork>,
    clipboard: ClipboardSlot,
    local_displays: LocalDisplays,
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
    /// A newer MonHop wrote the file, so this one saves nothing over it.
    Newer,
}

impl SetupFileError {
    const fn message(&self, write: &'static str) -> &'static str {
        match self {
            Self::Read => "The saved setup could not be read.",
            Self::Write => write,
            Self::Newer => SETUP_FROM_NEWER,
        }
    }
}

impl SetupFileLock {
    /// `seen` is the highest revision this computer saw, which every write keeps. A refused edit
    /// writes nothing.
    fn try_update(
        &self,
        path: &Path,
        seen: u64,
        edit: impl FnOnce(&mut SetupFile) -> Result<(), PreferenceError>,
    ) -> Result<SetupFile, SetupFileError> {
        let _guard = lock(&self.0);
        try_update_setup(path, |file| {
            file.observe_clock(seen);
            edit(file)
        })
    }

    /// Stores `record`, made here by `local`, as this computer's newest change: its other members
    /// become the enabled group and a pause ends.
    fn write_record(
        &self,
        path: &Path,
        seen: u64,
        local: &str,
        record: &GroupRecord,
    ) -> Result<GroupRecord, SetupFileError> {
        let _guard = lock(&self.0);
        let mut written = None;
        try_update_setup(path, |file| {
            file.observe_clock(seen);
            let record = record.restamped(file.clock().saturating_add(1), local)?;
            file.resume();
            file.adopt(local, record.clone())?;
            written = Some(record);
            Ok(())
        })?;
        written.ok_or(SetupFileError::Write)
    }

    /// The link's commit, a max-merge into the file as it is now: a newer record written since
    /// staging stays. `take_staged` runs under the file lock, so a Use or Pause either retired
    /// the staged layout before this or waits and then writes over the committed one. Returns
    /// the file written and whether the staged record is now its members' record.
    fn commit(
        &self,
        path: &Path,
        local: &str,
        take_staged: impl FnOnce() -> Result<(GroupRecord, u64), LinkRejectReason>,
    ) -> Result<(SetupFile, bool), LinkRejectReason> {
        let _guard = lock(&self.0);
        let (staged, seen) = take_staged()?;
        let mut adopted = false;
        let mut refused = None;
        try_update_setup(path, |file| {
            file.observe_clock(seen);
            // Staging checked against the computers switched on then; the user may have changed
            // them since.
            if !keeps_enabled(&staged, file, local) {
                refused = Some(LinkRejectReason::Invalid);
                return Err(PreferenceError::Invalid);
            }
            adopted = file.adopt(local, staged)?;
            Ok(())
        })
        .map(|file| (file, adopted))
        .map_err(|_| refused.unwrap_or(LinkRejectReason::SaveFailed))
    }
}

/// A refused edit writes nothing.
fn try_update_setup(
    path: &Path,
    edit: impl FnOnce(&mut SetupFile) -> Result<(), PreferenceError>,
) -> Result<SetupFile, SetupFileError> {
    let mut file = SetupFile::load(path).map_err(|_| SetupFileError::Read)?;
    if file.written_by_newer() {
        return Err(SetupFileError::Newer);
    }
    edit(&mut file).map_err(|_| SetupFileError::Write)?;
    file.save(path).map_err(|_| SetupFileError::Write)?;
    Ok(file)
}

#[derive(Clone)]
enum WorkerKind {
    Session,
    Link(UnboundedSender<LinkCommand>),
}

/// What a standing link waits for before the port goes back to a session. Only a commit ends
/// such a link.
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

pub(crate) const fn local_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Windows
    }
}

impl SharingController {
    pub(crate) fn with_link_runner(link_runner: LinkRunner, idle_window: Duration) -> Self {
        let clipboard = ClipboardSlot::default();
        let network = SharingNetwork::transport(Arc::clone(&clipboard));
        Self::assemble(
            link_runner,
            idle_window,
            network,
            clipboard,
            current_local_displays,
        )
    }

    /// A controller on `network`, reading this computer's displays with `local_displays`.
    #[cfg(test)]
    pub(crate) fn with_network(
        link_runner: LinkRunner,
        idle_window: Duration,
        network: SharingNetwork,
        local_displays: LocalDisplays,
    ) -> Self {
        Self::assemble(
            link_runner,
            idle_window,
            network,
            ClipboardSlot::default(),
            local_displays,
        )
    }

    fn assemble(
        link_runner: LinkRunner,
        idle_window: Duration,
        network: SharingNetwork,
        clipboard: ClipboardSlot,
        local_displays: LocalDisplays,
    ) -> Self {
        Self {
            operation: Mutex::default(),
            state: Arc::default(),
            workers: Mutex::default(),
            setup_file: Arc::default(),
            link_runner,
            idle_window,
            network: Arc::new(network),
            clipboard,
            local_displays,
        }
    }

    #[cfg(test)]
    pub(crate) fn network(&self) -> &SharingNetwork {
        &self.network
    }

    /// Share sessions attach to `hub` from now on.
    pub fn use_clipboard(&self, hub: Arc<ClipboardHub>) {
        *lock(&self.clipboard) = Some(hub);
    }

    /// This computer's displays, read as the identity `local` its records name it by.
    pub(crate) fn read_local_displays(
        &self,
        local: &str,
    ) -> Result<session_setup::DisplayTopology, String> {
        (self.local_displays)(local)
    }

    /// Native input is claimed, and no hub holds it for a live session: an earlier release
    /// still needs attention.
    fn cleanup_pending(&self) -> bool {
        NativeSessionClaim::is_claimed() && !self.network.holds_native()
    }

    /// Window shutdown must wait for retained native release workers, including prior sessions,
    /// and for the network to let go of the port.
    pub fn shutdown_ready(&self) -> bool {
        !NativeSessionClaim::is_claimed()
            && lock(&self.workers).values().all(JoinHandle::is_finished)
            && self.network.is_quiet()
    }
    pub fn request_shutdown(&self) {
        let cleanup = self.cleanup_pending();
        {
            let _operation = lock(&self.operation);
            let mut state = lock(&self.state);
            state.shutdown = true;
            begin_close(
                &mut state,
                None,
                "Stopping sharing and releasing held input.",
                false,
            );
            settle_idle(&mut state, None, true, cleanup);
        }
        self.network.request_shutdown();
    }
    pub fn invalidate_if_idle(&self) -> Result<(), String> {
        let _operation = lock(&self.operation);
        if NativeSessionClaim::is_claimed() {
            return Err("Wait for local input cleanup before connecting again.".into());
        }
        let mut workers = lock(&self.workers);
        if workers.values().any(|worker| !worker.is_finished()) {
            return Err("Wait for the current sharing action to finish.".into());
        }
        for previous in std::mem::take(&mut *workers).into_values() {
            let _ = previous.join();
        }
        if NativeSessionClaim::is_claimed() {
            return Err("Wait for local input cleanup before connecting again.".into());
        }
        let mut state = lock(&self.state);
        if state.shutdown {
            return Err("MonHop is shutting down and cannot connect.".into());
        }
        if all_views(&mut state).any(|view| view.busy) || !self.network.is_quiet() {
            return Err("Wait for the current sharing action to finish.".into());
        }
        invalidate_authorization(&mut state, None);
        for view in all_views(&mut state) {
            view.phase = "off";
            view.sharing_active = false;
            view.message = NOT_CONNECTED.into();
        }
        state.native_cleanup_pending = false;
        for peer in state.peers.values_mut() {
            peer.cancel = None;
            peer.native_cancel = None;
            peer.progress = None;
            peer.link = None;
            peer.link_since = None;
            peer.link_touched = None;
        }
        Ok(())
    }
    pub fn status(&self) -> SharingView {
        let cleanup = self.cleanup_pending();
        let mut state = lock(&self.state);
        if state.native_cleanup_pending && !cleanup && !render(&state, None).busy {
            state.native_cleanup_pending = false;
            let shutdown = state.shutdown;
            for view in all_views(&mut state).filter(|view| !view.busy) {
                show_idle(view, shutdown, false);
            }
        }
        let mut view = render(&state, Some(cleanup));
        if !view.busy && cleanup {
            view.phase = "error";
            view.message = native_cleanup_message().into();
        }
        view
    }
    /// Ends every live link or session; `idle_message` is what the view says once input is local.
    pub fn stop_with(&self, idle_message: &str) -> SharingView {
        self.stop_with_reason(None, idle_message, false)
    }

    /// Same as [`Self::stop_with`] for `scope` (every computer for None), marked so the peer's
    /// close reads as a control-change resync instead of an ordinary stop.
    fn stop_with_reason(
        &self,
        scope: Option<&str>,
        idle_message: &str,
        control_change: bool,
    ) -> SharingView {
        let cleanup = self.cleanup_pending();
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        for peer in scoped(&mut state.peers, scope) {
            let busy = peer.view.as_ref().is_some_and(|view| view.busy);
            if peer.running.is_some() && (busy || peer.link.is_some()) {
                peer.close_message = Some(idle_message.to_owned());
            }
        }
        begin_close(
            &mut state,
            scope,
            "Stopping the connection and releasing held input.",
            control_change,
        );
        let idle = idle_message.to_owned();
        settle_idle_with(&mut state, scope, false, cleanup, |view| {
            if !cleanup {
                view.message.clone_from(&idle);
            }
        });
        view_of(&state)
    }

    /// Ends `peer`'s link or session alone; `idle_message` is what its view says afterwards.
    pub fn stop_peer(&self, peer: CertificateFingerprint, idle_message: &str) -> SharingView {
        self.stop_with_reason(Some(&peer_key(&peer)), idle_message, false)
    }

    /// [`Self::stop_peer`] unless its worker already ended or is already stopping; true when this
    /// stopped it.
    pub fn release_peer(&self, peer: CertificateFingerprint, idle_message: &str) -> bool {
        let stopping = lock(&self.state)
            .peers
            .get(&peer_key(&peer))
            .is_none_or(|entry| entry.running.is_none() || entry.close_message.is_some());
        if stopping || !self.worker_alive(peer) {
            return false;
        }
        self.stop_peer(peer, idle_message);
        true
    }

    /// A forgotten computer is no longer admitted on the sharing network.
    pub fn forget_on_network(&self, peer: CertificateFingerprint) {
        self.network.forget(peer);
    }

    /// The newest drop line as Home shows it, for an explicit copy; None while none is recorded.
    pub fn last_drop_text(&self) -> Option<String> {
        let state = lock(&self.state);
        let failure = latest_failure(&state);
        let text = drop_note(
            failure.and_then(|peer| peer.last_failure.as_deref()),
            failure
                .and_then(|peer| peer.last_failure_at)
                .map(|at| at.elapsed()),
        );
        (!text.is_empty()).then_some(text)
    }

    /// True only with no link worker and no link phase: the supervisor opens one only then.
    pub fn link_is_off(&self) -> bool {
        let state = lock(&self.state);
        state.peers.values().all(|peer| {
            peer.link.is_none() && peer.view.as_ref().is_none_or(|view| !linking(view.phase))
        }) && !linking(state.view.phase)
    }

    /// [`Self::link_is_off`] for `peer` alone.
    pub fn link_is_off_for(&self, peer: CertificateFingerprint) -> bool {
        lock(&self.state)
            .peers
            .get(&peer_key(&peer))
            .is_none_or(|entry| {
                entry.link.is_none() && entry.view.as_ref().is_none_or(|view| !linking(view.phase))
            })
    }

    /// Whether a link or session worker is alive, and for which computer.
    pub fn live_peer(&self) -> Option<CertificateFingerprint> {
        self.live_peers().into_iter().next()
    }

    /// Every computer a link or session worker is alive for.
    pub fn live_peers(&self) -> Vec<CertificateFingerprint> {
        let alive: Vec<String> = lock(&self.workers)
            .iter()
            .filter(|(_, worker)| !worker.is_finished())
            .map(|(key, _)| key.clone())
            .collect();
        let state = lock(&self.state);
        alive
            .iter()
            .filter_map(|key| state.peers.get(key).and_then(|peer| peer.running))
            .collect()
    }

    /// Whether `peer`'s link or session worker is alive.
    pub fn worker_alive(&self, peer: CertificateFingerprint) -> bool {
        lock(&self.workers)
            .get(&peer_key(&peer))
            .is_some_and(|worker| !worker.is_finished())
    }

    /// Nothing holds native input except a hub that serves a live session.
    pub fn native_settled(&self) -> bool {
        !self.cleanup_pending()
    }

    pub fn editing(&self) -> bool {
        lock(&self.state).editing
    }

    /// The link's current displays for the computer it is with; None without a live link.
    pub fn live_inspection(&self) -> Option<(String, InspectedPeer)> {
        let state = lock(&self.state);
        let (_, peer) = the_peer(&state)?;
        peer.running
            .zip(peer.inspection.clone())
            .map(|(running, inspection)| (peer_key(&running), inspection))
    }

    /// [`Self::live_inspection`] for every computer a link or session runs with.
    pub fn live_inspections(&self) -> Vec<(String, InspectedPeer)> {
        live_connections(&lock(&self.state))
            .map(|(key, inspection)| (key.to_owned(), inspection.clone()))
            .collect()
    }

    /// What each computer shows as far as this one knows now; see [`known_now`].
    pub(crate) fn known_displays(&self) -> KnownDisplays {
        known_now(&lock(&self.state))
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
        interface_id: Option<&str>,
    ) -> Result<SharingView, String> {
        self.update_setup_file(path, |file| {
            match fingerprint.as_ref() {
                Some(fingerprint) => file.choose(fingerprint),
                None => file.set_active(None),
            }
            // The supervisor dials only over a recorded network, so choosing a computer records the chosen one.
            if let Some(interface_id) =
                interface_id.filter(|id| fingerprint.is_some() && !id.is_empty())
            {
                file.set_interface_id(interface_id);
            }
        })?;
        self.clear_choice();
        let live = self.live_peers();
        let others: Vec<CertificateFingerprint> = live
            .iter()
            .copied()
            .filter(|peer| Some(*peer) != fingerprint)
            .collect();
        let message = if fingerprint.is_some() {
            SWITCHING_COMPUTER
        } else {
            PAUSED
        };
        if !others.is_empty() && others.len() == live.len() {
            return Ok(self.stop_with(message));
        }
        for other in others {
            self.stop_peer(other, message);
        }
        if fingerprint.is_none() {
            let mut state = lock(&self.state);
            for view in all_views(&mut state).filter(|view| !view.busy) {
                view.phase = "off";
                view.message = PAUSED.to_owned();
            }
        }
        Ok(self.status())
    }

    /// Switches one computer in or out of the group, recording the network along with a computer
    /// switched on. A computer switched off lets go at once; the supervisor connects the rest.
    pub fn set_enabled(
        &self,
        path: &Path,
        fingerprint: CertificateFingerprint,
        enabled: bool,
        interface_id: Option<&str>,
    ) -> Result<SharingView, String> {
        let file = self.try_update_setup_file(path, |file| {
            file.set_enabled(&fingerprint, enabled)?;
            // Under the setup-file lock, so a link commit racing this cannot switch it back on.
            if !enabled && let Some(peer) = lock(&self.state).peers.get_mut(&peer_key(&fingerprint))
            {
                retire_persistence(peer);
            }
            if let Some(interface_id) = interface_id.filter(|id| enabled && !id.is_empty()) {
                file.set_interface_id(interface_id);
            }
            Ok(())
        })?;
        self.clear_peer_choice(fingerprint);
        if enabled {
            return Ok(self.status());
        }
        let alone = file.enabled().is_empty();
        let message = if alone {
            NOT_CONNECTED
        } else {
            SWITCHING_COMPUTER
        };
        if self.worker_alive(fingerprint) {
            self.stop_peer(fingerprint, message);
        }
        if alone {
            let mut state = lock(&self.state);
            for view in all_views(&mut state).filter(|view| !view.busy) {
                view.phase = "off";
                view.message = NOT_CONNECTED.to_owned();
            }
        }
        Ok(self.status())
    }

    /// Drops what was going on for the computers chosen so far: arranging, a reason to hold a
    /// link, unsynced switch flips, the last drop and its backoff.
    pub fn clear_choice(&self) {
        let mut state = lock(&self.state);
        publish_arranging(&state, false);
        state.editing = false;
        state.pending_control = None;
        for peer in state.peers.values_mut() {
            forget_choice(peer);
        }
    }

    /// [`Self::clear_choice`] for `peer` alone, as forgetting one computer of several does.
    pub fn clear_peer_choice(&self, peer: CertificateFingerprint) {
        if let Some(entry) = lock(&self.state).peers.get_mut(&peer_key(&peer)) {
            forget_choice(entry);
        }
    }

    /// Applies one change to the setup file and mirrors the result into the view. Every writer
    /// of that file goes through here or the link's commit, never around them.
    pub fn update_setup_file(
        &self,
        path: &Path,
        edit: impl FnOnce(&mut SetupFile),
    ) -> Result<SetupFile, String> {
        self.try_update_setup_file(path, |file| {
            edit(file);
            Ok(())
        })
    }

    /// [`Self::update_setup_file`] for an edit that may refuse: a refusal writes nothing and is
    /// the error returned.
    pub fn try_update_setup_file(
        &self,
        path: &Path,
        edit: impl FnOnce(&mut SetupFile) -> Result<(), &'static str>,
    ) -> Result<SetupFile, String> {
        let seen = lock(&self.state).clock;
        let mut refusal = None;
        let written = self.setup_file.try_update(path, seen, |file| {
            edit(file).map_err(|message| {
                refusal = Some(message);
                PreferenceError::Invalid
            })
        });
        let file = written.map_err(|error| {
            refusal.map_or_else(
                || {
                    error
                        .message("The saved setup could not be updated.")
                        .to_owned()
                },
                str::to_owned,
            )
        })?;
        self.adopt_saved(&file);
        self.advance_setup_revision();
        Ok(file)
    }

    /// For a change the cards draw that no write made: this computer's displays, read without a
    /// link or session reporting them.
    pub fn advance_setup_revision(&self) {
        advance_setup_revision(&mut lock(&self.state));
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
            let member = fingerprint_key(&peer.full_hex());
            let active = state
                .active_control
                .clone()
                .filter(|active| active.members.contains(&member))
                .ok_or("Arrange the displays with this computer first.")?;
            let key = if local_to_peer {
                active.local.clone()
            } else {
                member
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

    /// Ends `peer`'s running session deliberately so the link can carry a pending control
    /// change; true when it did.
    pub fn end_session_for_control(&self, peer: CertificateFingerprint) -> bool {
        let key = peer_key(&peer);
        {
            let state = lock(&self.state);
            let entry = state.peers.get(&key);
            if state.pending_control.is_none()
                || entry.is_some_and(|entry| entry.link.is_some())
                || !peer_busy(&state, &key)
                || entry.is_some_and(|entry| entry.close_message.is_some())
                || state.shutdown
            {
                return false;
            }
        }
        self.stop_with_reason(Some(&key), CHANGING_CONTROL, true);
        true
    }

    /// Arranging starts the idle window on every current link; a link is opened by the caller
    /// when none is up. The other computers are told, so none proposes over the user.
    pub fn begin_editing(&self) -> SharingView {
        let mut state = lock(&self.state);
        state.editing = true;
        publish_arranging(&state, true);
        for peer in state.peers.values_mut() {
            touch_link(peer);
        }
        view_of(&state)
    }

    /// Ends arranging and closes every link so the supervisor reconnects for sharing.
    pub fn end_editing(&self) -> SharingView {
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        state.editing = false;
        publish_arranging(&state, false);
        let mut closing = Vec::new();
        for (key, peer) in &mut state.peers {
            peer.worker_failed_at = None;
            if peer.link.is_some() && peer.close_message.is_none() {
                peer.close_message = Some(EDITING_ENDED.to_owned());
                closing.push(key.clone());
            }
        }
        for key in closing {
            close_link(&mut state, &key, EDITING_ENDED);
        }
        view_of(&state)
    }

    /// Home's display banner, raised once per set of the link's displays. A dismissal keeps that
    /// memory, so the same change never raises it twice.
    pub fn raise_display_notice(&self, kind: DisplayNotice, inspection: &InspectedPeer) -> bool {
        let geometry = DisplayGeometry::of(inspection);
        raise_notice(
            &mut lock(&self.state),
            &peer_key(&inspection.peer_fingerprint),
            kind,
            geometry,
        )
    }

    /// True while a proposal for exactly these displays is still out. A banner is not this
    /// answer: it survives a lost link, and a proposal that went down with one must be made again.
    pub fn proposal_pending_for(&self, inspection: &InspectedPeer) -> bool {
        let geometry = DisplayGeometry::of(inspection);
        lock(&self.state)
            .peers
            .get(&peer_key(&inspection.peer_fingerprint))
            .and_then(|peer| peer.pending_proposal.as_ref())
            .is_some_and(|pending| pending.same(&geometry))
    }

    /// True once these displays were answered with "nothing fits": there is nothing left to
    /// propose until they change again or a layout is applied.
    pub fn waiting_notice_for(&self, inspection: &InspectedPeer) -> bool {
        let geometry = DisplayGeometry::of(inspection);
        let state = lock(&self.state);
        state
            .peers
            .get(&peer_key(&inspection.peer_fingerprint))
            .is_some_and(|peer| {
                peer.notice_answer == Some(DisplayNotice::Waiting)
                    && peer
                        .notice_geometry
                        .as_ref()
                        .is_some_and(|raised| raised.same(&geometry))
            })
    }

    /// True while a proposal for these displays was refused for a passing reason too recently.
    pub fn retry_wait_for(&self, inspection: &InspectedPeer) -> bool {
        let geometry = DisplayGeometry::of(inspection);
        lock(&self.state)
            .peers
            .get(&peer_key(&inspection.peer_fingerprint))
            .and_then(|peer| peer.proposal_retry.as_ref())
            .is_some_and(|(refused, _, at)| {
                refused.same(&geometry) && at.elapsed() < PROPOSAL_RETRY_AFTER
            })
    }

    /// True once the link's displays have held still long enough to propose a record for them.
    #[cfg(test)]
    pub fn displays_settled(&self) -> bool {
        let state = lock(&self.state);
        the_peer(&state)
            .and_then(|(_, peer)| peer.displays_changed_at)
            .is_some_and(|at| at.elapsed() >= DISPLAYS_SETTLE)
    }

    /// [`Self::displays_settled`] for the link `inspection` describes.
    pub fn displays_settled_for(&self, inspection: &InspectedPeer) -> bool {
        lock(&self.state)
            .peers
            .get(&peer_key(&inspection.peer_fingerprint))
            .filter(|peer| peer.running.is_some())
            .and_then(|peer| peer.displays_changed_at)
            .is_some_and(|at| at.elapsed() >= DISPLAYS_SETTLE)
    }

    /// The stamp of the other computer's record as its summary on this connection gives it,
    /// None inside when it holds none. None until a summary naming both link ends arrived.
    pub(crate) fn peer_stamp_for(&self, inspection: &InspectedPeer) -> Option<Option<Stamp>> {
        let state = lock(&self.state);
        let summary = state
            .peers
            .get(&peer_key(&inspection.peer_fingerprint))?
            .peer_summary
            .as_ref()?;
        let names = |fingerprint: &CertificateFingerprint| {
            summary.members().contains(&peer_key(fingerprint))
        };
        (names(&inspection.local_fingerprint) && names(&inspection.peer_fingerprint))
            .then(|| summary.stamp())
    }

    /// A proposal that could not leave: the next pass tries again, as for a passing refusal.
    pub fn note_proposal_failed(&self, inspection: &InspectedPeer) {
        note_passing_refusal(
            &mut lock(&self.state),
            &peer_key(&inspection.peer_fingerprint),
            DisplayGeometry::of(inspection),
        );
    }

    /// True while `peer`'s failed worker's backoff still runs, so the supervisor does not
    /// relaunch the same failure every pass. Any user choice clears it.
    pub fn within_failure_backoff(&self, peer: CertificateFingerprint, backoff: Duration) -> bool {
        lock(&self.state)
            .peers
            .get(&peer_key(&peer))
            .and_then(|peer| peer.worker_failed_at)
            .is_some_and(|(failed, floor)| failed.elapsed() < backoff.max(floor))
    }

    /// A user's action ends every computer's backoff.
    pub fn clear_failure_backoff(&self) {
        for peer in lock(&self.state).peers.values_mut() {
            peer.worker_failed_at = None;
        }
    }

    pub fn dismiss_display_notice(&self) -> SharingView {
        let mut state = lock(&self.state);
        state.display_notice = None;
        view_of(&state)
    }

    pub fn clear_display_notice(&self) {
        clear_notices(&mut lock(&self.state));
    }

    /// True once for each raised "nothing fits" banner: the app then brings its window forward.
    pub fn take_window_forward(&self) -> bool {
        std::mem::take(&mut lock(&self.state).notice_window_pending)
    }

    /// The link's displays while the decision pass may act: never while either user arranges or an
    /// exchange is in flight or committed here, since the sender's close then ends the link.
    #[cfg(test)]
    pub fn inspection_for_switch(&self) -> Option<InspectedPeer> {
        let state = lock(&self.state);
        let (key, _) = the_peer(&state)?;
        switch_inspection(&state, key)
    }

    /// [`Self::inspection_for_switch`] for the link with `peer`.
    pub fn inspection_for_switch_with(
        &self,
        peer: CertificateFingerprint,
    ) -> Option<InspectedPeer> {
        switch_inspection(&lock(&self.state), &peer_key(&peer))
    }

    /// This computer's displays no longer fit its record, so the next connection with `peer` is
    /// a link on which one computer proposes the record both then hold.
    pub fn note_local_misfit(&self, peer: CertificateFingerprint) {
        peer_entry(&mut lock(&self.state).peers, &peer_key(&peer))
            .link_reason
            .get_or_insert(LinkReason::LayoutMisfit);
    }

    /// True while a standing link with `peer` must stay up instead of a session.
    pub fn holds_link(&self, peer: CertificateFingerprint) -> bool {
        let state = lock(&self.state);
        state
            .peers
            .get(&peer_key(&peer))
            .is_some_and(|peer| peer.link_reason.is_some())
            || state.pending_control.is_some()
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
        let network = Arc::clone(&self.network);
        let key = peer_key(&peer);
        self.launch(
            "connecting",
            "Connecting to the other computer.",
            None,
            WorkerKind::Link(commands),
            peer,
            move |state, generation, _authorization, epoch, cancel, _progress| {
                let persist = link_persist(
                    Arc::clone(&state),
                    key.clone(),
                    generation,
                    epoch,
                    path,
                    setup_file,
                );
                let link = runner(LinkRun {
                    interface_id,
                    peer,
                    cancel: cancel.clone(),
                    persist,
                    events,
                    commands: outgoing,
                    network,
                });
                pump_link(
                    state,
                    key,
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
        for peer in lock(&self.state).peers.values_mut() {
            touch_link(peer);
        }
    }

    /// Proposes the user's layout for the whole group, one proposal on each connected link; each
    /// outcome arrives as that link's event. A member without one is carried with the displays it
    /// showed last.
    pub fn apply_setup(
        &self,
        revision: &str,
        layout: LayoutRequest,
    ) -> Result<SharingView, String> {
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        if state.shutdown || state.view.revision != revision {
            return Err(CONNECT_TO_APPLY.into());
        }
        let draft = group_draft(&state, CONNECT_TO_APPLY)?;
        if draft
            .others
            .iter()
            .any(|key| matches!(own_view(&state, key).sync.state, "sending" | "receiving"))
        {
            return Err("Wait for the current layout to finish applying.".into());
        }
        let record = group_layout(&state, &draft, layout)?;
        let mut outgoing = Vec::new();
        for key in &draft.ready {
            let Ok((key, commands, inspection)) = live_link(&state, Some(key)) else {
                continue;
            };
            let bytes = shared_group_bytes(&inspection, &record, false)
                .map_err(|error| error.to_string())?;
            outgoing.push((key, commands, bytes));
        }
        if outgoing.is_empty() {
            return Err(CONNECT_TO_APPLY.into());
        }
        let mut sent = false;
        for (key, commands, bytes) in outgoing {
            if commands.send(LinkCommand::Propose { bytes }).is_err() {
                continue;
            }
            sent = true;
            touch_link(peer_entry(&mut state.peers, &key));
            show(&mut state, &key, |view| {
                view.sync = SyncView {
                    state: "sending",
                    message: APPLYING_LAYOUT.into(),
                };
                view.message = APPLYING_LAYOUT.into();
            });
        }
        if !sent {
            return Err("The connection ended. Connect again.".into());
        }
        Ok(view_of(&state))
    }

    /// A record rebuilt for the link's displays, stamped as a local change. `left_out` (a crossing
    /// or display dropped) decides the banner the commit leaves and whether either computer
    /// remembers the layout.
    #[cfg(test)]
    pub fn propose_layout(&self, next: &GroupRecord, left_out: bool) -> Result<(), String> {
        self.send_proposal(None, next, Some(left_out))
    }

    /// Proposes a record that already fits both computers' displays: this computer's own as it
    /// is, or carrying a control change as a local change. Nothing changed on screen, so no
    /// banner.
    #[cfg(test)]
    pub fn propose_record(&self, next: &GroupRecord) -> Result<(), String> {
        self.send_proposal(None, next, None)
    }

    /// [`Self::propose_layout`] over the link `inspection` describes.
    pub fn propose_layout_to(
        &self,
        inspection: &InspectedPeer,
        next: &GroupRecord,
        left_out: bool,
    ) -> Result<(), String> {
        let key = peer_key(&inspection.peer_fingerprint);
        self.send_proposal(Some(&key), next, Some(left_out))
    }

    /// [`Self::propose_record`] over the link `inspection` describes.
    pub fn propose_record_to(
        &self,
        inspection: &InspectedPeer,
        next: &GroupRecord,
    ) -> Result<(), String> {
        let key = peer_key(&inspection.peer_fingerprint);
        self.send_proposal(Some(&key), next, None)
    }

    /// Same staging and commit as `apply_setup`, with no window revision to check, over `target`'s
    /// link (the one live link for None). The proposal is recorded, and any banner raised, under
    /// the same lock that sends it.
    fn send_proposal(
        &self,
        target: Option<&str>,
        next: &GroupRecord,
        display_change: Option<bool>,
    ) -> Result<(), String> {
        let _operation = lock(&self.operation);
        let mut state = lock(&self.state);
        let shown = target
            .or_else(|| the_link_key(&state))
            .map_or(&state.view, |key| own_view(&state, key));
        if state.shutdown || shown.busy || shown.phase != "connected" {
            return Err("Connect the computers before applying a layout.".into());
        }
        if matches!(shown.sync.state, "sending" | "receiving") {
            return Err("Wait for the current layout to finish applying.".into());
        }
        let (key, commands, inspection) = live_link(&state, target)?;
        let local = inspection.local_fingerprint.full_hex();
        let pair = next.members().len() == 2;
        let held = &next.layout().control;
        let mut control = overlaid(held, state.pending_control.as_ref());
        let usable = if pair {
            sharing_preferences::validate_control(
                &control,
                &local,
                &inspection.peer_fingerprint.full_hex(),
            )
            .is_ok()
        } else {
            next.with_control(control.clone()).is_ok()
        };
        if !usable {
            control.clone_from(held);
        }
        let mut layout = next.layout().clone();
        layout.control.clone_from(&control);
        // A group's record reaches past this link's two computers; its own validation covers it.
        if pair {
            validated_layout(&inspection, &layout)?;
        }
        let next = if display_change.is_some() || control != *held {
            next.with_control(control)
                .and_then(|record| record.restamped(state.clock.saturating_add(1), &local))
                .map_err(|error| error.to_string())?
        } else {
            next.clone()
        };
        let left_out = display_change.unwrap_or(false);
        let bytes =
            shared_group_bytes(&inspection, &next, left_out).map_err(|error| error.to_string())?;
        commands
            .send(LinkCommand::Propose { bytes })
            .map_err(|_| "The connection ended. Connect again.".to_owned())?;
        let geometry = DisplayGeometry::of(&inspection);
        let message = match display_change {
            Some(left_out) => {
                raise_notice(&mut state, &key, DisplayNotice::Updating, geometry.clone());
                // Set after the raise, which clears it: this send is the authority even when it
                // reuses a banner already up for these displays.
                peer_entry(&mut state.peers, &key).notice_left_out = left_out;
                UPDATING_LAYOUT
            }
            None if state.pending_control.is_some() => UPDATING_CONTROL,
            None => CONFIRMING_LAYOUT,
        };
        peer_entry(&mut state.peers, &key).pending_proposal = Some(geometry);
        show(&mut state, &key, |view| {
            view.sync = SyncView {
                state: "sending",
                message: message.into(),
            };
            view.message = message.into();
        });
        Ok(())
    }

    /// Mirrors the setup file into the view so Home shows the active computer and its switches
    /// without a live link, and tells a live link a summary that changed with it.
    pub fn adopt_saved(&self, file: &SetupFile) {
        let mut state = lock(&self.state);
        mirror_file(&mut state, file);
        reconcile_pending_control(&mut state);
    }

    /// Follows the active group's record. A changed record moves the group on: every session made
    /// under the old one ends as a resync, and the network restarts its hub under the new one.
    pub(crate) fn sync_group(&self, file: &SetupFile) {
        let current = file
            .sharing_chosen()
            .then(|| file.local().zip(file.active_group()))
            .flatten();
        let agreement = current.map(|(_, record)| record.content_digest());
        let (stale, plan) = {
            let mut state = lock(&self.state);
            if state.group_agreement == agreement {
                return;
            }
            state.group_agreement = agreement;
            state.group_epoch = state.group_epoch.wrapping_add(1);
            let epoch = state.group_epoch;
            let stale: Vec<String> = state
                .peers
                .iter()
                .filter(|(_, peer)| {
                    peer.running.is_some()
                        && peer.close_message.is_none()
                        && peer.share_epoch.is_some_and(|made| made != epoch)
                })
                .map(|(key, _)| key.clone())
                .collect();
            let plan = current
                .zip(agreement)
                .map(|((local, record), agreement)| GroupPlan {
                    epoch,
                    local: local.to_owned(),
                    record: record.clone(),
                    agreement,
                });
            (stale, plan)
        };
        for key in stale {
            log::info!(
                "sharing: the group's record changed; ending a session made under the old one"
            );
            self.stop_with_reason(Some(&key), GROUP_CHANGED, true);
        }
        if let Some(plan) = plan {
            self.network.set_group(plan);
        }
    }

    /// The group a session under `plan` runs in, moving the group on when its record changed.
    fn group_plan(&self, plan: &SessionPlan) -> GroupPlan {
        let agreement = plan.record.content_digest();
        let mut state = lock(&self.state);
        if state.group_agreement != Some(agreement) {
            state.group_agreement = Some(agreement);
            state.group_epoch = state.group_epoch.wrapping_add(1);
        }
        GroupPlan {
            epoch: state.group_epoch,
            local: plan.local.clone(),
            record: plan.record.clone(),
            agreement,
        }
    }

    /// A start the supervisor could not make is shown once, not retried every tick.
    pub fn note_supervisor_failure(&self, message: String) {
        let mut state = lock(&self.state);
        for view in all_views(&mut state).filter(|view| !view.busy) {
            view.phase = "error";
            view.message.clone_from(&message);
        }
    }

    /// A start the supervisor could not make for `peer` alone: shown on its view, and it waits
    /// out a backoff while the other computers go on.
    pub fn note_peer_failure(&self, peer: CertificateFingerprint, message: String) {
        let mut state = lock(&self.state);
        let key = peer_key(&peer);
        peer_entry(&mut state.peers, &key).worker_failed_at =
            Some((Instant::now(), Duration::ZERO));
        if !peer_busy(&state, &key) {
            show(&mut state, &key, |view| {
                view.phase = "error";
                view.message.clone_from(&message);
            });
        }
    }

    /// Keeps one authenticated sharing session alive with `plan`'s computer: connect, run in the
    /// hub, reconnect. The pairing itself is checked by the handshake.
    pub fn start_sharing(&self, plan: SessionPlan) -> Result<SharingView, String> {
        let control = plan
            .control()
            .ok_or("Choose which computer can control the other.")?;
        let group = self.group_plan(&plan);
        let (interface_id, peer) = (plan.interface_id.clone(), plan.peer);
        let key = peer_key(&peer);
        let network = Arc::clone(&self.network);
        let local_displays = self.local_displays;
        let made_under = peer_entry(&mut lock(&self.state).peers, &key)
            .share_epoch
            .replace(group.epoch);
        let launched = self.launch(
            "starting",
            CONNECTING_FOR_SHARING,
            None,
            WorkerKind::Session,
            peer,
            {
                let key = key.clone();
                move |state, worker_generation, authorization_revision, _epoch, cancel, _progress| async move {
                    let _release = ShareRelease(Arc::clone(&network), peer);
                    let mut attempt: u32 = 0;
                    let mut window_started = Instant::now();
                    let mut attempts = AttemptLog::default();
                    log::info!(
                        "sharing: connecting on {interface_id} with record #{} ({control:?})",
                        group.record.digest()
                    );
                    loop {
                        if cancel.is_revoked() {
                            return Ok(());
                        }
                        attempt = attempt.saturating_add(1);
                        let progress = session::SessionProgress::default();
                        {
                            let mut guard = lock(&state);
                            if generation_of(&guard, &key) != worker_generation {
                                return Ok(());
                            }
                            show(&mut guard, &key, |view| {
                                view.link.attempt = attempt;
                                view.phase = "starting";
                                view.sharing_active = false;
                                view.message = CONNECTING_FOR_SHARING.to_owned();
                            });
                            peer_entry(&mut guard.peers, &key).progress = Some(progress.clone());
                        }
                        let request = ShareRequest {
                            interface_id: interface_id.clone(),
                            plan: group.clone(),
                            peer,
                            cancel: cancel.clone(),
                            progress,
                        };
                        let up = match network.connect_share(request).await {
                            Ok(up) => up,
                            Err(_) if cancel.is_revoked() => return Ok(()),
                            // The group moved on while this connect waited; the next pass starts
                            // a session under the new record.
                            Err(ShareFailure::Stale) => return Ok(()),
                            Err(ShareFailure::Misfit) => {
                                note_layout_misfit(&mut lock(&state), &key);
                                return Ok(());
                            }
                            Err(ShareFailure::NativeCleanup) => {
                                return Err(native_cleanup_message().into());
                            }
                            // Not a layout question: dialing again would only repeat it, so the
                            // supervisor backs off.
                            Err(ShareFailure::Hub(failure)) => {
                                log::warn!(
                                    "sharing: attempt {attempt}: the hub could not start: {failure:?}"
                                );
                                return Err(session_message(failure));
                            }
                            // The other computer already has its setup link open; no session
                            // starts until both are on the same step, so this computer joins it
                            // there.
                            Err(ShareFailure::Setup(SetupFailure::PurposeMismatch)) => {
                                log::info!(
                                    "sharing: attempt {attempt} met the other computer on its setup link; opening this computer's link"
                                );
                                let mut guard = lock(&state);
                                let entry = peer_entry(&mut guard.peers, &key);
                                entry.link_reason = Some(LinkReason::PeerHoldsLink);
                                entry.close_message = Some(PEER_ARRANGING.to_owned());
                                return Ok(());
                            }
                            // The two records disagree on who may control whom. Dialing again
                            // repeats it: the link is where the decider proposes one record for
                            // both.
                            Err(ShareFailure::Setup(SetupFailure::ChangedSinceInspection)) => {
                                note_record_disagreement(&mut lock(&state), &key);
                                return Ok(());
                            }
                            Err(ShareFailure::Setup(SetupFailure::PeerIdentityChanged)) => {
                                log::warn!(
                                    "sharing: attempt {attempt}: the other computer presented an identity other than the paired one; stopping"
                                );
                                peer_entry(&mut lock(&state).peers, &key).failure_floor =
                                    PEER_IDENTITY_BACKOFF;
                                return Err(setup_message(SetupFailure::PeerIdentityChanged));
                            }
                            // A record can name a computer this one never paired with: retrying
                            // cannot change that, pairing it can.
                            Err(ShareFailure::Setup(SetupFailure::PairingRequired)) => {
                                log::warn!(
                                    "sharing: attempt {attempt}: this computer is not paired with the other computer; stopping"
                                );
                                let mut guard = lock(&state);
                                let entry = peer_entry(&mut guard.peers, &key);
                                entry.not_paired = true;
                                entry.failure_floor = PEER_IDENTITY_BACKOFF;
                                return Err(setup_message(SetupFailure::PairingRequired));
                            }
                            Err(ShareFailure::Setup(SetupFailure::Displays))
                                if still_unsettled(
                                    &mut lock(&state).displays_unreadable_since,
                                    Instant::now(),
                                ) =>
                            {
                                show(&mut lock(&state), &key, |view| {
                                    view.message = DISPLAYS_UNSETTLED.to_owned();
                                });
                                sleep_unless_cancelled(&cancel, UNSETTLED_POLL).await;
                                continue;
                            }
                            // The switch is the user's standing intent: a peer that is off or
                            // still installing must be able to join later without a new click.
                            // Attempt outcomes go to the status line; last_failure keeps the
                            // reason the previous session ended.
                            Err(ShareFailure::Setup(error)) => {
                                if let Some(line) = attempts.note(
                                    error,
                                    attempt,
                                    window_started.elapsed(),
                                    Instant::now(),
                                ) {
                                    log::info!("sharing: {line}");
                                }
                                let message = share_wait_message(error, window_started.elapsed());
                                show(&mut lock(&state), &key, |view| {
                                    view.message.clone_from(&message);
                                });
                                sleep_unless_cancelled(&cancel, RECONNECT_INTERVAL).await;
                                continue;
                            }
                        };
                        {
                            let mut guard = lock(&state);
                            guard.displays_unreadable_since = None;
                            peer_entry(&mut guard.peers, &key).not_paired = false;
                        }
                        attempts = AttemptLog::default();
                        log::info!("sharing: attempt {attempt} connected, starting the session");
                        let ShareUp {
                            inspection,
                            cancel: session_cancel,
                            started,
                            ended,
                        } = up;
                        let registered = register_native_cancellation(
                            &mut lock(&state),
                            &key,
                            worker_generation,
                            authorization_revision,
                            &cancel,
                            session_cancel,
                        );
                        if let Err(message) = registered {
                            // The stop that came first ends this session in the hub; its end
                            // still brings the clipboard attachment back to be released here.
                            drop(ended.await);
                            return Err(message);
                        }
                        {
                            // A session has no link to report displays, so the computer cards
                            // read the live ones from here; the record it runs with is exactly
                            // this geometry.
                            let mut guard = lock(&state);
                            let entry = peer_entry(&mut guard.peers, &key);
                            entry.inspection = Some(inspection);
                            entry.share_started = Some(started);
                        }
                        let session_started = Instant::now();
                        // A napped app's timers stretch past the capture lease, so the Mac holds
                        // this until the session has ended.
                        #[cfg(target_os = "macos")]
                        let awake = monhop_platform_macos::SessionActivity::begin();
                        #[cfg(target_os = "macos")]
                        if awake.is_none() {
                            log::warn!(
                                "sharing: macOS refused the no-nap activity for this session"
                            );
                        }
                        let ShareEnd {
                            result,
                            diagnostics: stats,
                            detached,
                        } = ended.await.unwrap_or_else(|_| ShareEnd::lost());
                        #[cfg(target_os = "macos")]
                        drop(awake);
                        {
                            let mut guard = lock(&state);
                            let entry = peer_entry(&mut guard.peers, &key);
                            entry.native_cancel = None;
                            entry.inspection = None;
                            entry.share_started = None;
                            // A cleared progress stops status() from reporting the reconnect
                            // wait as sharing; the ended session's counters stay visible.
                            entry.progress = None;
                            show(&mut guard, &key, |view| {
                                view.sharing_active = false;
                                view.diagnostics = ended_diagnostics(&stats);
                                view.phase = "starting";
                            });
                        }
                        // Detaching the clipboard can wait on a read in progress, so it happens
                        // on this thread, never on the network's.
                        drop(detached);
                        let load = format!("{}{}", receiver_load_note(&stats), link_note(&stats));
                        log::warn!(
                            "sharing: attempt {attempt} session over after {:.1} s: {result:?}{load}",
                            session_started.elapsed().as_secs_f64()
                        );
                        let peer_close = stats.link.map(|link| link.close).unwrap_or_default();
                        // Pause, a switch flip or quitting set these before the session ended.
                        let deliberate = {
                            let guard = lock(&state);
                            guard.shutdown
                                || guard
                                    .peers
                                    .get(&key)
                                    .is_some_and(|entry| entry.close_message.is_some())
                                || generation_of(&guard, &key) != worker_generation
                        };
                        // A display change is a step, not a failure. Classified before the arms
                        // below so it keeps no failure reason, and acted on after native release
                        // so the worker still leaves input clean.
                        let skip_transition_checks =
                            deliberate || matches!(result, Err(SessionFailure::NativeCleanup));
                        let outgrown = if skip_transition_checks {
                            None
                        } else {
                            displays_changed_end(&result, peer_close, || {
                                local_displays(&plan.local)
                                    .ok()
                                    .map(|current| plan.fits_local(&current))
                            })
                        };
                        // The peer's control flip is checked once the displays already ruled
                        // themselves out: the two close reasons never both apply.
                        let peer_control_change = outgrown.is_none()
                            && !skip_transition_checks
                            && peer_control_change_end(peer_close);
                        if let Some(change) = outgrown {
                            log::info!(
                                "sharing: the session ended as a display change detected by {}",
                                change.detected_by()
                            );
                        }
                        if !deliberate {
                            let message = match outgrown {
                                Some(change) => change.message(),
                                None if peer_control_change => PEER_CONTROL_CHANGED,
                                None => SHARING_DROPPED,
                            };
                            show(&mut lock(&state), &key, |view| {
                                view.message = message.to_owned();
                            });
                        }
                        match result {
                            _ if deliberate => return Ok(()),
                            Err(SessionFailure::NativeCleanup) => {
                                return Err(native_cleanup_message().into());
                            }
                            // The displays explain this end; it is not a drop and keeps no reason.
                            _ if outgrown.is_some() => {}
                            // The other computer's control flip explains this end; not a drop
                            // either.
                            _ if peer_control_change => {}
                            // A network revocation or the emergency stop: the next dial starts
                            // fresh.
                            Err(SessionFailure::Revoked) if cancel.is_revoked() => return Ok(()),
                            // The other computer's user ended it: nothing dropped, so no drop
                            // line.
                            Ok(()) => show(&mut lock(&state), &key, |view| {
                                view.message = PEER_PAUSED.to_owned();
                            }),
                            Err(_) if peer_close == LinkClose::PeerEnded => {
                                show(&mut lock(&state), &key, |view| {
                                    view.message = PEER_PAUSED.to_owned();
                                });
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
                                let entry = peer_entry(&mut guard.peers, &key);
                                entry.last_failure =
                                    Some(format!("Attempt {attempt}: {reason}{load}"));
                                entry.last_failure_at = Some(Instant::now());
                            }
                        }
                        if !await_native_settled(&network).await {
                            return Err(native_cleanup_message().into());
                        }
                        // The saved record can no longer fit, so every dial from here would only
                        // wait for the other computer to reach the same conclusion.
                        if let Some(change) = outgrown {
                            note_displays_changed(&mut lock(&state), &key, change.message());
                            return Ok(());
                        }
                        if peer_control_change {
                            note_control_changed(&mut lock(&state), &key);
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
                }
            },
        );
        if launched.is_err() {
            peer_entry(&mut lock(&self.state).peers, &key).share_epoch = made_under;
        }
        launched
    }

    /// The active group's named arrangements, each fitting while every member shows the displays
    /// it was made with; none while the group is unknown.
    pub fn arrangements(&self, path: &Path) -> Result<Vec<ArrangementView>, String> {
        let (members, known) = {
            let state = lock(&self.state);
            (group_keys(&state), known_now(&state))
        };
        let library = ArrangementLibrary::load(path).map_err(|_| ARRANGEMENTS_UNREADABLE)?;
        Ok(members
            .map(|members| library.views(&members, &known))
            .unwrap_or_default())
    }

    /// Saves a complete, valid layout for the active group under a name; the same name replaces
    /// the earlier one.
    pub fn save_arrangement(
        &self,
        path: &Path,
        revision: &str,
        name: &str,
        layout: LayoutRequest,
    ) -> Result<Vec<ArrangementView>, String> {
        let (record, _) = self.connected_record(revision, layout)?;
        let mut library = ArrangementLibrary::load(path).map_err(|_| ARRANGEMENTS_UNREADABLE)?;
        let members = record.member_keys();
        library
            .upsert(name, record)
            .map_err(|error| error.message().to_owned())?;
        save_library(path, &library)?;
        Ok(library.views(&members, &self.known_displays()))
    }

    /// The record the active group would hold for `layout`, valid for every member and carrying
    /// the control map this computer holds for the group, stamped as the next local change; with
    /// the group it was built for.
    fn connected_record(
        &self,
        revision: &str,
        layout: LayoutRequest,
    ) -> Result<(GroupRecord, GroupDraft), String> {
        let state = lock(&self.state);
        if state.shutdown || state.view.revision != revision {
            return Err(CONNECT_TO_SAVE.into());
        }
        let draft = group_draft(&state, CONNECT_TO_SAVE)?;
        let record = group_layout(&state, &draft, layout)?;
        Ok((record, draft))
    }

    pub fn save_setup(
        &self,
        path: &Path,
        revision: &str,
        layout: LayoutRequest,
    ) -> Result<SavedSetupView, String> {
        let (checked, draft) = self.connected_record(revision, layout)?;
        let seen = lock(&self.state).clock;
        // Disk data is inert. Stop may invalidate the inspection while this write completes.
        let record = self
            .setup_file
            .write_record(path, seen, &draft.local, &checked)
            .map_err(|error| {
                error
                    .message("The setup could not be saved. Your previous setup is unchanged.")
                    .to_owned()
            })?;
        self.clear_display_notice();
        remember_applied(path, &record, &draft.local);
        crate::autostart::setup_applied(path);
        let mut state = lock(&self.state);
        state.clock = state.clock.max(record.revision());
        advance_setup_revision(&mut state);
        let live: Vec<&InspectedPeer> = live_connections(&state)
            .map(|(_, inspection)| inspection)
            .collect();
        Ok(SavedSetupView::for_group(
            &record,
            Some(&draft.local),
            &live,
            &state.view.revision,
        ))
    }

    fn launch<F, Fut>(
        &self,
        phase: &'static str,
        message: &str,
        expected_revision: Option<String>,
        kind: WorkerKind,
        peer: CertificateFingerprint,
        run: F,
    ) -> Result<SharingView, String>
    where
        F: FnOnce(
                Arc<Mutex<State>>,
                u64,
                u64,
                u64,
                RevocationSignal,
                session::SessionProgress,
            ) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + 'static,
    {
        let _operation = lock(&self.operation);
        let key = peer_key(&peer);
        let is_link = matches!(kind, WorkerKind::Link(_));
        if is_link
            && lock(&self.state)
                .peers
                .get(&key)
                .is_some_and(|entry| entry.link.is_some())
        {
            return Err("Already connected. Stop the connection before connecting again.".into());
        }
        let mut workers = lock(&self.workers);
        if self.cleanup_pending() {
            return Err(
                "Wait for local input cleanup before starting another sharing action.".into(),
            );
        }
        if workers
            .get(&key)
            .is_some_and(|worker| !worker.is_finished())
        {
            return Err("Wait for the current sharing action to finish.".into());
        }
        let finished: Vec<String> = workers
            .iter()
            .filter(|(_, worker)| worker.is_finished())
            .map(|(finished, _)| finished.clone())
            .collect();
        for previous in finished.iter().filter_map(|key| workers.remove(key)) {
            let _ = previous.join();
        }
        let mut state = lock(&self.state);
        if state.shutdown {
            return Err("MonHop is shutting down and cannot start sharing.".into());
        }
        if peer_busy(&state, &key) {
            return Err("Wait for the current sharing action to finish.".into());
        }
        if expected_revision.is_some_and(|expected| expected != state.view.revision) {
            return Err("The setup changed. Connect the computers again.".into());
        }
        if is_link {
            invalidate_authorization(&mut state, Some(&key));
        } else {
            advance_authorization(&mut state);
        }
        if state.shutdown {
            return Err("Restart MonHop before starting another session.".into());
        }
        let worker_generation = state
            .generation
            .checked_add(1)
            .ok_or("Restart MonHop before starting another session.")?;
        state.generation = worker_generation;
        let authorization_revision = state.authorization_revision;
        state.view.revision = authorization_revision.to_string();
        show(&mut state, &key, |view| {
            view.phase = phase;
            view.busy = true;
            view.sharing_active = false;
            view.message = message.to_owned();
            view.link = LinkView::default();
            if is_link {
                view.sync = SyncView::default();
            }
        });
        let editing = state.editing;
        let entry = peer_entry(&mut state.peers, &key);
        entry.worker_generation = worker_generation;
        entry.close_message = None;
        entry.failure_floor = Duration::ZERO;
        entry.running = Some(peer);
        entry.authorized = (!is_link).then_some(authorization_revision);
        // Claimed now, not when the worker starts: a Stop or switch-off from here on spends it.
        let link_epoch = if is_link {
            claim_epoch(entry)
        } else {
            entry.link_epoch
        };
        if let WorkerKind::Link(commands) = kind {
            let now = Instant::now();
            // A link reopened while the user is already arranging must say so from its first
            // connection, or the other computer would read the silence as idle.
            let _ = commands.send(LinkCommand::Arranging(editing));
            entry.link = Some(commands);
            entry.link_since = Some(now);
            entry.link_touched = Some(now);
            entry.summary_sent = None;
            entry.peer_summary = None;
            publish_summary(&mut state, &key);
        }
        let cancel = RevocationSignal::default();
        let progress = session::SessionProgress::default();
        let entry = peer_entry(&mut state.peers, &key);
        entry.cancel = Some(cancel.clone());
        entry.native_cancel = None;
        entry.progress = Some(progress.clone());
        let shared = self.state.clone();
        let ended = key.clone();
        let network = Arc::clone(&self.network);
        let worker = std::thread::Builder::new()
            .name("monhop-sharing".into())
            .spawn(move || {
                let key = ended;
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
                        link_epoch,
                        cancel.clone(),
                        progress,
                    ))
                }))
                .unwrap_or_else(|_| {
                    Err("The sharing worker stopped unexpectedly. Input sharing is off.".into())
                });
                let mut guard = lock(&shared);
                let state = &mut *guard;
                if generation_of(state, &key) != worker_generation {
                    log::debug!("worker {worker_generation} ended after being superseded");
                    return;
                }
                let superseded =
                    !is_link && peer_authorization(state, &key) != authorization_revision;
                match &result {
                    Ok(()) => log::info!(
                        "worker {worker_generation} ({}) ended{}",
                        if is_link { "setup link" } else { "sharing" },
                        state
                            .peers
                            .get(&key)
                            .and_then(|entry| entry.close_message.as_deref())
                            .map(|message| format!(": {message}"))
                            .unwrap_or_default()
                    ),
                    Err(error) => log::warn!(
                        "worker {worker_generation} ({}) ended with an error: {error}",
                        if is_link { "setup link" } else { "sharing" }
                    ),
                }
                invalidate_authorization(state, Some(&key));
                show(state, &key, |view| {
                    view.busy = false;
                    view.sharing_active = false;
                    view.link = LinkView::default();
                    if is_link && view.sync.state != "applied" {
                        view.sync = SyncView::default();
                    }
                });
                let entry = peer_entry(&mut state.peers, &key);
                entry.cancel = None;
                entry.native_cancel = None;
                entry.link = None;
                entry.link_since = None;
                entry.link_touched = None;
                entry.staged = None;
                entry.running = None;
                entry.share_started = None;
                entry.share_epoch = None;
                // Nothing carries the peer's arranging state, its summary or a proposal past
                // its link: the next link reports the first two and re-sends the last.
                entry.peer_arranging = false;
                entry.peer_summary = None;
                entry.summary_sent = None;
                entry.committed = None;
                entry.pending_proposal = None;
                entry.displays_changed_at = None;
                // "Updating" and "the other computer is choosing" both describe an exchange
                // this link was carrying, so neither may outlive it; a banner that did would
                // sit there promising a layout nothing is still working on.
                if matches!(
                    state.display_notice,
                    Some(DisplayNotice::Updating | DisplayNotice::PeerDeciding)
                ) {
                    clear_notice(state, &key);
                }
                let entry = peer_entry(&mut state.peers, &key);
                if let Some(progress) = entry.progress.take() {
                    let stats = progress.diagnostics();
                    show(state, &key, |view| {
                        view.diagnostics = ended_diagnostics(&stats)
                    });
                }
                let entry = peer_entry(&mut state.peers, &key);
                let floor = std::mem::take(&mut entry.failure_floor);
                let close_message = entry.close_message.take();
                let cleanup = NativeSessionClaim::is_claimed() && !network.holds_native();
                state.native_cleanup_pending = cleanup;
                let (phase, message) = if cleanup {
                    ("error", native_cleanup_message().to_owned())
                } else if state.shutdown {
                    ("off", SHUTTING_DOWN.to_owned())
                } else if let Some(message) = close_message {
                    // A close the user, the supervisor, or the idle window asked for is never a
                    // failure.
                    ("off", message)
                } else {
                    match result {
                        Err(error) if !superseded => {
                            // Only a real failure backs the supervisor off; every step change
                            // above returned before reaching here.
                            peer_entry(&mut state.peers, &key).worker_failed_at =
                                Some((Instant::now(), floor));
                            ("error", error)
                        }
                        _ => ("off", NOT_CONNECTED.to_owned()),
                    }
                };
                show(state, &key, |view| {
                    view.phase = phase;
                    view.message.clone_from(&message);
                });
            });
        match worker {
            Ok(worker) => {
                workers.insert(key, worker);
                Ok(view_of(&state))
            }
            Err(_) => {
                show(&mut state, &key, |view| {
                    view.busy = false;
                    view.phase = "error";
                });
                let entry = peer_entry(&mut state.peers, &key);
                entry.cancel = None;
                entry.native_cancel = None;
                entry.link = None;
                entry.link_since = None;
                entry.link_touched = None;
                entry.running = None;
                Err("The sharing worker could not start.".to_owned())
            }
        }
    }
}

/// The running link's computer, its commands and its displays.
fn live_link(
    state: &State,
    target: Option<&str>,
) -> Result<(String, UnboundedSender<LinkCommand>, InspectedPeer), &'static str> {
    const NO_LINK: &str = "Connect the computers before applying a layout.";
    let key = target.or_else(|| the_link_key(state)).ok_or(NO_LINK)?;
    let peer = state
        .peers
        .get(key)
        .filter(|peer| peer.running.is_some())
        .ok_or(NO_LINK)?;
    let commands = peer.link.clone().ok_or(NO_LINK)?;
    let inspection = peer
        .inspection
        .clone()
        .ok_or("Connect both computers first.")?;
    Ok((key.to_owned(), commands, inspection))
}

/// The computer the one live link is with, else the one whose worker runs.
fn the_link_key(state: &State) -> Option<&str> {
    state
        .peers
        .iter()
        .find(|(_, peer)| peer.running.is_some() && peer.link.is_some())
        .map(|(key, _)| key.as_str())
        .or_else(|| the_peer(state).map(|(key, _)| key))
}

/// The link's displays while the decision pass may act on `key`'s link: never while either user
/// arranges or an exchange is in flight or committed here, since the sender's close then ends
/// the link.
fn switch_inspection(state: &State, key: &str) -> Option<InspectedPeer> {
    let peer = state.peers.get(key).filter(|peer| peer.running.is_some())?;
    let view = own_view(state, key);
    if peer.link.is_none()
        || state.editing
        || peer.peer_arranging
        || peer.link_reason == Some(LinkReason::LayoutUnconfirmed)
        || view.phase != "connected"
        || peer.close_message.is_some()
        || !matches!(view.sync.state, "idle" | "rejected")
    {
        return None;
    }
    peer.inspection.clone()
}

/// Drops what the user chose so far for one computer: a reason to hold a link, the last drop and
/// its backoff, and a missing pairing.
fn forget_choice(peer: &mut PeerState) {
    peer.link_reason = None;
    peer.last_failure = None;
    peer.last_failure_at = None;
    peer.worker_failed_at = None;
    peer.not_paired = false;
}

/// The phases in which a link is up or ending.
fn linking(phase: &str) -> bool {
    matches!(
        phase,
        "connecting" | "connected" | "reconnecting" | "stopping"
    )
}

/// A finished session's counters, without a route.
fn ended_diagnostics(stats: &session::SessionDiagnostics) -> DiagnosticsView {
    DiagnosticsView {
        sent_events: stats.sent_events.to_string(),
        received_events: stats.received_events.to_string(),
        round_trip_ms: stats.round_trip_micros as f64 / 1000.0,
        active_display: None,
        active_is_local: None,
    }
}

/// Tells the network a share worker let go of its computer, however the worker ends.
struct ShareRelease(Arc<SharingNetwork>, CertificateFingerprint);

impl Drop for ShareRelease {
    fn drop(&mut self) {
        self.0.release_share(self.1);
    }
}

/// Spends the link's epoch and drops its staged record: a commit racing this finds nothing to
/// write, so it can never overwrite the applied setup.
fn retire_persistence(peer: &mut PeerState) {
    claim_epoch(peer);
    peer.staged = None;
}

/// A new persistence epoch for `peer`'s link; every earlier one is spent.
fn claim_epoch(peer: &mut PeerState) -> u64 {
    peer.link_epoch = peer.link_epoch.wrapping_add(1);
    peer.link_epoch
}

/// Shared teardown for Stop, quit and a control-record flip, for `scope` (every computer for
/// None): stop the session so its close can leave, ask a live link to close cleanly, and let
/// the cancellation watch revoke the socket after its grace. `control_change` marks the stop so
/// the peer's close reads as a resync.
fn begin_close(
    state: &mut State,
    scope: Option<&str>,
    stopping_message: &str,
    control_change: bool,
) {
    invalidate_authorization(state, scope);
    let mut stopping = Vec::new();
    for (key, peer) in state
        .peers
        .iter_mut()
        .filter(|(key, _)| scope.is_none_or(|only| only == key.as_str()))
    {
        // The peer learns from the close reason that this was the user's choice, not a drop.
        if let Some(progress) = peer.progress.as_ref() {
            progress.end_deliberately();
        }
        retire_persistence(peer);
        if let Some(native) = peer.native_cancel.as_ref() {
            if control_change {
                native.request_stop_for_control_change();
            } else {
                native.request_stop();
            }
        }
        let link_running = peer.link.is_some();
        if let Some(link) = peer.link.as_ref() {
            peer.close_message
                .get_or_insert_with(|| NOT_CONNECTED.to_owned());
            let _ = link.send(LinkCommand::Close);
        }
        if let Some(cancel) = peer.cancel.as_ref() {
            cancel.revoke();
        }
        if link_running || peer.view.as_ref().is_some_and(|view| view.busy) {
            stopping.push(key.clone());
        }
    }
    let stop = |view: &mut SharingView| {
        view.busy = true;
        view.phase = "stopping";
        view.message = stopping_message.to_owned();
    };
    let ended = |view: &mut SharingView| {
        view.sharing_active = false;
        view.sync = SyncView::default();
    };
    match scope {
        Some(key) => {
            show(state, key, ended);
            if !stopping.is_empty() {
                show(state, key, stop);
            }
        }
        None => {
            let shared_stops = state.view.busy || !stopping.is_empty();
            for view in all_views(state) {
                ended(view);
            }
            if shared_stops {
                stop(&mut state.view);
            }
            for key in &stopping {
                if let Some(view) = state.peers.get_mut(key).and_then(|peer| peer.view.as_mut()) {
                    stop(view);
                }
            }
        }
    }
}

/// Applies link events to the view and enforces the idle window without blocking the transport.
#[allow(clippy::too_many_arguments)]
async fn pump_link(
    state: Arc<Mutex<State>>,
    key: String,
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
                    if apply_link_event(&state, &key, generation, event) && closing.is_none() {
                        closing = Some(tokio::time::Instant::now() + LINK_CLOSE_GRACE);
                    }
                }
                None => open = false,
            },
            result = &mut link => {
                while let Ok(event) = events.try_recv() {
                    apply_link_event(&state, &key, generation, event);
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
                        if generation_of(&state, &key) == generation {
                            peer_entry(&mut state.peers, &key)
                                .close_message
                                .get_or_insert_with(|| PEER_STARTING_SESSION.to_owned());
                        }
                        Ok(())
                    }
                    // Displays read mid-change: the supervisor reopens the link on its next pass.
                    Err(SetupFailure::Displays) => {
                        let mut state = lock(&state);
                        if generation_of(&state, &key) == generation
                            && still_unsettled(&mut state.displays_unreadable_since, Instant::now())
                        {
                            peer_entry(&mut state.peers, &key)
                                .close_message
                                .get_or_insert_with(|| DISPLAYS_UNSETTLED.to_owned());
                            return Ok(());
                        }
                        Err(setup_message(SetupFailure::Displays))
                    }
                    Err(SetupFailure::PeerIdentityChanged) => {
                        log::warn!("link: the other computer presented an identity other than the paired one");
                        peer_entry(&mut lock(&state).peers, &key).failure_floor = PEER_IDENTITY_BACKOFF;
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
                } else if close_when_idle(&state, &key, generation, idle_window) {
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
fn open_link_for(state: &mut State, key: &str, reason: LinkReason, message: &str) {
    let peer = peer_entry(&mut state.peers, key);
    peer.link_reason = Some(reason);
    peer.close_message = Some(message.to_owned());
}

fn open_link_for_layout(state: &mut State, key: &str, message: &str) {
    open_link_for(state, key, LinkReason::LayoutMisfit, message);
}

/// A dialed session whose displays no longer fit the saved layout. That is the ordinary answer to
/// a display change, not a failure: the worker ends cleanly, so the supervisor opens the link on
/// its next pass with no backoff and one computer proposes a layout that fits.
fn note_layout_misfit(state: &mut State, key: &str) {
    log::info!(
        "sharing: the displays no longer fit the saved layout; opening the link to arrange them"
    );
    open_link_for_layout(state, key, LAYOUT_MISFIT);
}

/// The two computers' saved records disagree on who may control whom. Answered on the link like
/// a misfit, never by dialing again: the newer record's holder proposes it and both then hold it.
fn note_record_disagreement(state: &mut State, key: &str) {
    log::info!("sharing: the two computers hold different layouts; opening the link to agree");
    open_link_for_layout(state, key, RECORDS_DISAGREE);
}

/// A running session the displays outgrew. Same answer as a misfit, for the same reason: the
/// record cannot fit again, so every reconnect wait before the link opens is dead time.
fn note_displays_changed(state: &mut State, key: &str, message: &str) {
    log::info!("sharing: the displays changed during the session; opening the link to update");
    open_link_for_layout(state, key, message);
    // Already on screen from the end that classified it; held here so the close cannot drop it.
    show(state, key, |view| view.message = message.to_owned());
}

/// A running session the other computer ended to carry a control-record change. Same answer as a
/// display change: the worker ends cleanly and the link opens next to receive the new record.
fn note_control_changed(state: &mut State, key: &str) {
    log::info!(
        "sharing: the other computer changed who can control which computer; opening the link to sync"
    );
    open_link_for(state, key, LinkReason::ControlChanged, PEER_CONTROL_CHANGED);
    // Already on screen from the end that classified it; held here so the close cannot drop it.
    show(state, key, |view| {
        view.message = PEER_CONTROL_CHANGED.to_owned()
    });
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

/// Raises Home's banner for `key`'s `geometry` unless one is already raised for exactly those
/// displays.
fn raise_notice(
    state: &mut State,
    key: &str,
    kind: DisplayNotice,
    geometry: DisplayGeometry,
) -> bool {
    let peer = peer_entry(&mut state.peers, key);
    if peer
        .notice_geometry
        .as_ref()
        .is_some_and(|raised| raised.same(&geometry))
    {
        return false;
    }
    peer.notice_geometry = Some(geometry);
    peer.notice_answer = Some(kind);
    // Only the send that follows knows whether its layout lost anything; until then it has not.
    peer.notice_left_out = false;
    state.display_notice = Some(kind);
    // Only "nothing fits" asks the user for anything, so only it brings the window forward.
    if kind == DisplayNotice::Waiting {
        state.notice_window_pending = true;
    }
    true
}

/// "Nothing fits" for these displays: the supervisor proposes nothing more for them until they
/// change or a layout is applied. A banner the user dismissed for them stays down.
fn answer_waiting(state: &mut State, key: &str, geometry: DisplayGeometry) {
    let peer = peer_entry(&mut state.peers, key);
    let dismissed = state.display_notice.is_none()
        && peer
            .notice_geometry
            .as_ref()
            .is_some_and(|raised| raised.same(&geometry));
    peer.notice_geometry = Some(geometry);
    peer.notice_answer = Some(DisplayNotice::Waiting);
    peer.notice_left_out = false;
    if !dismissed {
        state.display_notice = Some(DisplayNotice::Waiting);
        state.notice_window_pending = true;
    }
    // A control change riding on these proposals cannot land either; the switches show the record.
    if state.pending_control.take().is_some() {
        show(state, key, |view| view.message = CONTROL_REFUSED.into());
    }
}

/// A refusal that says nothing about the layout itself: the displays count as unanswered, so the
/// next pass proposes again after a short wait, until the retries for them run out.
fn note_passing_refusal(state: &mut State, key: &str, geometry: DisplayGeometry) {
    let peer = peer_entry(&mut state.peers, key);
    let count = match &peer.proposal_retry {
        Some((refused, count, _)) if refused.same(&geometry) => count.saturating_add(1),
        _ => 1,
    };
    peer.proposal_retry = Some((geometry.clone(), count, Instant::now()));
    if count >= MAX_PROPOSAL_RETRIES {
        answer_waiting(state, key, geometry);
    } else if peer
        .notice_geometry
        .as_ref()
        .is_some_and(|raised| raised.same(&geometry))
    {
        peer.notice_geometry = None;
        peer.notice_answer = None;
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

/// The group a layout made in the window is for: this computer and each enabled computer, or the
/// computers of the live connections while none is enabled.
struct GroupDraft {
    /// This computer, full uppercase hex, as its live connections name it.
    local: String,
    /// The other members, lowercase and sorted.
    others: Vec<String>,
    /// Every member, each with the displays its live connection shows, else those the newest
    /// record naming it holds.
    members: Vec<GroupMember>,
    /// The members whose connection is connected and idle.
    ready: Vec<String>,
}

/// The active group as it stands now; `not_ready` is the refusal while no member's connection
/// is connected and idle. A member never seen on any connection or in any record is an error.
fn group_draft(state: &State, not_ready: &str) -> Result<GroupDraft, String> {
    let live: BTreeMap<&str, &InspectedPeer> = live_connections(state).collect();
    let mut others: Vec<String> = if state.enabled.is_empty() {
        live.keys().map(|key| (*key).to_owned()).collect()
    } else {
        state.enabled.clone()
    };
    others.sort();
    let ready: Vec<String> = others
        .iter()
        .filter(|key| {
            let view = own_view(state, key);
            live.contains_key(key.as_str()) && view.phase == "connected" && !view.busy
        })
        .cloned()
        .collect();
    let here = ready
        .first()
        .and_then(|key| live.get(key.as_str()))
        .ok_or_else(|| not_ready.to_owned())?;
    let local = here.local_fingerprint.full_hex();
    let invalid = |error: PreferenceError| error.to_string();
    let mut members =
        vec![GroupMember::new(&local, here.local_platform, &here.local_displays).map_err(invalid)?];
    for key in &others {
        let member = match live.get(key.as_str()) {
            Some(inspection) => GroupMember::new(
                &inspection.peer_fingerprint.full_hex(),
                inspection.peer_platform,
                &inspection.peer_displays,
            )
            .map_err(invalid)?,
            None => state.seen.get(key).cloned().ok_or(NEVER_SEEN)?,
        };
        members.push(member);
    }
    Ok(GroupDraft {
        local,
        others,
        members,
        ready,
    })
}

/// `layout` as the record `draft`'s group would hold, stamped as the next local change and
/// carrying the control map this computer holds for the group. The window never chooses that
/// map, and a pair is refused in the words its link would use.
fn group_layout(
    state: &State,
    draft: &GroupDraft,
    mut layout: LayoutRequest,
) -> Result<GroupRecord, String> {
    layout.control = group_control(state, &draft.local, &draft.others);
    if let [peer] = draft.others.as_slice()
        && let Some((_, inspection)) = live_connections(state).find(|(key, _)| key == peer)
    {
        validated_layout(inspection, &layout)?;
    }
    GroupRecord::checked(
        state.clock.saturating_add(1),
        &draft.local,
        draft.members.clone(),
        layout,
    )
}

/// The active record's entries with any pending flips over them while the group is its members,
/// else every member allowed: what a group without a record starts with.
fn group_control(state: &State, local: &str, others: &[String]) -> ControlMap {
    let local = fingerprint_key(local);
    state
        .active_control
        .as_ref()
        .filter(|active| active.local == local && active.members == others)
        .map(|active| overlaid(&active.control, state.pending_control.as_ref()))
        .filter(|control| control.values().any(|allowed| *allowed))
        .unwrap_or_else(|| {
            others
                .iter()
                .chain(std::iter::once(&local))
                .map(|member| (member.clone(), true))
                .collect()
        })
}

/// The active group's members, lowercase and sorted, this computer included; None while this
/// computer's identity or every other member is unknown.
fn group_keys(state: &State) -> Option<Vec<String>> {
    let mut members: Vec<String> = if state.enabled.is_empty() {
        live_connections(state)
            .map(|(key, _)| key.to_owned())
            .collect()
    } else {
        state.enabled.clone()
    };
    if members.is_empty() {
        return None;
    }
    let local = live_connections(state)
        .map(|(_, inspection)| peer_key(&inspection.local_fingerprint))
        .next()
        .or_else(|| state.local.as_deref().map(fingerprint_key))?;
    members.push(local);
    members.sort();
    members.dedup();
    Some(members)
}

/// Each computer a link or session runs with, by key, with the displays that connection shows.
fn live_connections(state: &State) -> impl Iterator<Item = (&str, &InspectedPeer)> {
    state
        .peers
        .iter()
        .filter(|(_, peer)| peer.running.is_some())
        .filter_map(|(key, peer)| Some((key.as_str(), peer.inspection.as_ref()?)))
}

/// What each computer shows as far as this one knows now: both ends of every live connection
/// and, while any is live, every other member of the active group as its record holds it. With
/// nothing live nothing is known, so nothing counts as fitting.
fn known_now(state: &State) -> KnownDisplays {
    let mut known = KnownDisplays::default();
    let mut live = false;
    for (_, inspection) in live_connections(state) {
        known.insert(
            &inspection.local_fingerprint.full_hex(),
            inspection.local_platform,
            &inspection.local_displays,
        );
        known.insert(
            &inspection.peer_fingerprint.full_hex(),
            inspection.peer_platform,
            &inspection.peer_displays,
        );
        live = true;
    }
    match state.held.as_ref().filter(|_| live) {
        Some(record) => known.with_entries_of(record),
        None => known,
    }
}

fn control_view(state: &State) -> Option<ControlView> {
    let active = state.active_control.as_ref()?;
    let control = overlaid(&active.control, state.pending_control.as_ref());
    let allowed = |member: &str| control.get(member) == Some(&true);
    let members = if active.members.len() > 1 {
        let mut members: Vec<&String> = active
            .members
            .iter()
            .chain(std::iter::once(&active.local))
            .collect();
        members.sort();
        members
            .into_iter()
            .map(|member| ControlMember {
                fingerprint: member.clone(),
                allowed: allowed(member),
            })
            .collect()
    } else {
        Vec::new()
    };
    Some(ControlView {
        local_to_peer: allowed(&active.local),
        peer_to_local: active.members.iter().any(|member| allowed(member)),
        syncing: state.pending_control.is_some(),
        members,
    })
}

/// Tells every live link this computer's arranging state; without a link there is nobody to tell.
fn publish_arranging(state: &State, arranging: bool) {
    for link in state.peers.values().filter_map(|peer| peer.link.as_ref()) {
        let _ = link.send(LinkCommand::Arranging(arranging));
    }
}

/// The setup file as the view and the link read it without a disk read. Pending flips are
/// left for the caller to reconcile.
fn mirror_file(state: &mut State, file: &SetupFile) {
    state.active = file.active().map(str::to_owned);
    state.active_control = file
        .sharing_chosen()
        .then(|| file.active_group())
        .flatten()
        .zip(file.local())
        .and_then(|(record, local)| ActiveControl::of(record, local));
    state.clock = state.clock.max(file.clock());
    state.local = file.local().map(str::to_owned);
    state.enabled = file.enabled().to_vec();
    state.paused = file.paused();
    state.held = file.active_group().cloned();
    let mut records: Vec<&GroupRecord> = file.groups().iter().collect();
    records.sort_by_key(|record| record.revision());
    state.seen = records
        .into_iter()
        .flat_map(GroupRecord::members)
        .map(|member| (fingerprint_key(member.fingerprint()), member.clone()))
        .collect();
    let linked: Vec<String> = state
        .peers
        .iter()
        .filter(|(_, peer)| peer.link.is_some())
        .map(|(key, _)| key.clone())
        .collect();
    for key in linked {
        publish_summary(state, &key);
    }
}

/// Tells `key`'s live link this computer's summary once it differs from the last one told; the
/// link itself repeats it on every connection it opens.
fn publish_summary(state: &mut State, key: &str) {
    let Some(link) = state.peers.get(key).and_then(|peer| peer.link.clone()) else {
        return;
    };
    let Some(bytes) = own_summary(state, key) else {
        return;
    };
    let peer = peer_entry(&mut state.peers, key);
    if peer.summary_sent.as_deref() != Some(bytes.as_slice()) {
        let _ = link.send(LinkCommand::Summary(bytes.clone()));
        peer.summary_sent = Some(bytes);
    }
}

/// This computer's active group and its record's stamp; None while this computer's identity is
/// unknown. `key`'s live link inspection names that identity first, the setup file after it.
fn own_summary(state: &State, key: &str) -> Option<Vec<u8>> {
    let local = state
        .peers
        .get(key)
        .and_then(|peer| peer.inspection.as_ref())
        .map(|inspection| inspection.local_fingerprint.full_hex())
        .or_else(|| state.local.clone())?;
    let mut members = state.enabled.clone();
    members.push(fingerprint_key(&local));
    members.sort();
    members.dedup();
    let record = state
        .held
        .as_ref()
        .filter(|record| record.member_keys() == members);
    let members: Vec<&str> = members.iter().map(String::as_str).collect();
    RecordSummary::new(&members, record)
        .and_then(|summary| summary.to_bytes())
        .ok()
}

/// An applied or promoted layout answers the change, so the next one raises the banner again.
fn clear_notice(state: &mut State, key: &str) {
    state.display_notice = None;
    if let Some(peer) = state.peers.get_mut(key) {
        forget_notice(peer);
    }
}

/// [`clear_notice`] for every computer at once.
fn clear_notices(state: &mut State) {
    state.display_notice = None;
    state.peers.values_mut().for_each(forget_notice);
}

fn forget_notice(peer: &mut PeerState) {
    peer.notice_geometry = None;
    peer.notice_answer = None;
    peer.notice_left_out = false;
}

/// A layout committed over the link answers the display change behind it, so the banner comes
/// down. It stays up, as "sharing continued", only where the user lost something: the computer
/// that chose a layout which had to leave a crossing or a display out. A layout that fit exactly,
/// a user's own Apply, and the computer that only accepted the other's choice all leave none. The
/// geometry memory always goes, so the next change is judged afresh.
fn resolve_notice_on_commit(state: &mut State, key: &str) {
    let continued = state.display_notice == Some(DisplayNotice::Updating)
        && state
            .peers
            .get(key)
            .is_some_and(|peer| peer.notice_left_out);
    clear_notice(state, key);
    if continued {
        state.display_notice = Some(DisplayNotice::Continued);
    }
}

/// Remembers an applied record for the displays it was made with; an identical memory is left
/// alone. `record` is already validated, `local` is this computer, and a library problem never
/// fails the Apply behind it.
fn remember_applied(setup_path: &Path, record: &GroupRecord, local: &str) {
    let path = setup_path.with_file_name(ARRANGEMENTS_FILE);
    let Ok(mut library) = ArrangementLibrary::load(&path) else {
        log::warn!("arrangements: the applied layout was not remembered; the library is damaged");
        return;
    };
    match library.remember_automatically(record.clone(), local) {
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
    if library.written_by_newer() {
        return Err(ARRANGEMENTS_FROM_NEWER.to_owned());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| "The setup folder could not be created.".to_owned())?;
    }
    library.save(path).map_err(|_| {
        "The arrangement could not be saved. Your saved arrangements are unchanged.".to_owned()
    })
}

/// Native ownership must be back before the next session claims it, unless a hub holds it for
/// another live session; a stuck release fails closed.
async fn await_native_settled(network: &SharingNetwork) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while NativeSessionClaim::is_claimed() && !network.holds_native() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// This computer's displays, read as the identity its records name it by.
pub(crate) fn current_local_displays(
    local: &str,
) -> Result<session_setup::DisplayTopology, String> {
    let local_device = CertificateFingerprint::parse_full(local)
        .map(monhop_transport::session_handshake::device_id_from_fingerprint)
        .map_err(|_| "The saved setup is damaged. Apply a layout again.".to_owned())?;
    session_native::current_displays(local_device)
        .map_err(|_| "Current display information could not be read.".to_owned())
}

/// Only arranging has an idle window; a standing link stays up until the supervisor ends it.
fn close_when_idle(
    shared: &Arc<Mutex<State>>,
    key: &str,
    generation: u64,
    idle_window: Duration,
) -> bool {
    let mut guard = lock(shared);
    let state = &mut *guard;
    let Some(peer) = state.peers.get_mut(key) else {
        return false;
    };
    // A close the user already asked for keeps its own wording.
    if peer.worker_generation != generation
        || !state.editing
        || peer.close_message.is_some()
        || !peer
            .link_touched
            .is_some_and(|touched| touched.elapsed() >= idle_window)
    {
        return false;
    }
    peer.close_message = Some(IDLE_CLOSED.to_owned());
    state.editing = false;
    show(state, key, |view| view.sync = SyncView::default());
    close_link(state, key, IDLE_CLOSED);
    true
}

/// Asks `key`'s live link to close with `message` as the view's wording meanwhile. An applied
/// sync outcome stays visible through the close.
fn close_link(state: &mut State, key: &str, message: &str) {
    show(state, key, |view| {
        view.phase = "stopping";
        view.busy = true;
        view.message = message.to_owned();
    });
    if let Some(link) = state.peers.get(key).and_then(|peer| peer.link.as_ref()) {
        let _ = link.send(LinkCommand::Close);
    }
}

/// Returns true once the link reports it has closed.
fn apply_link_event(
    shared: &Arc<Mutex<State>>,
    key: &str,
    generation: u64,
    event: LinkEvent,
) -> bool {
    let mut guard = lock(shared);
    let state = &mut *guard;
    if generation_of(state, key) != generation || state.shutdown {
        return matches!(event, LinkEvent::Closed);
    }
    match event {
        LinkEvent::Connecting { attempt } => {
            let reconnecting = state
                .peers
                .get(key)
                .is_some_and(|peer| peer.inspection.is_some());
            show(state, key, |view| {
                view.link.attempt = attempt;
                view.busy = true;
                view.phase = if reconnecting {
                    "reconnecting"
                } else {
                    "connecting"
                };
                view.message = "Connecting to the other computer.".into();
            });
        }
        LinkEvent::Connected { inspection } => {
            adopt_inspection(state, key, inspection);
            // The other computer's summary follows on this connection; one heard before it may
            // be stale.
            peer_entry(&mut state.peers, key).peer_summary = None;
            publish_summary(state, key);
            state.displays_unreadable_since = None;
            // Both computers are on the link now, so the reason that chose a link over a session
            // is spent and the ordinary decision path runs again.
            let peer = peer_entry(&mut state.peers, key);
            if peer.link_reason == Some(LinkReason::PeerHoldsLink) {
                peer.link_reason = None;
            }
            peer.not_paired = false;
            let message = connected_message(state, key);
            show(state, key, |view| view.message = message.into());
        }
        LinkEvent::PeerArranging { arranging } => {
            peer_entry(&mut state.peers, key).peer_arranging = arranging;
            if !state.editing && own_view(state, key).phase == "connected" {
                let message = if arranging {
                    CONNECTED_PEER_ARRANGING
                } else {
                    connected_message(state, key)
                };
                show(state, key, |view| view.message = message.to_owned());
            }
        }
        LinkEvent::TopologyChanged { inspection } if only_relabeled(state, key, &inspection) => {
            adopt_labels(state, key, inspection);
        }
        LinkEvent::TopologyChanged { inspection } => {
            adopt_inspection(state, key, inspection);
            show(state, key, |view| {
                view.synchronized_layout = None;
                view.message =
                    "The displays changed. Check the arrangement, then apply the layout.".into();
            });
        }
        LinkEvent::SyncStarted { sending } => {
            let (name, message) = if sending {
                ("sending", "Applying the layout on both computers.")
            } else {
                ("receiving", "The other computer is applying a layout.")
            };
            show(state, key, |view| {
                view.sync = SyncView {
                    state: name,
                    message: message.to_owned(),
                };
                view.message = message.to_owned();
            });
        }
        LinkEvent::SyncCompleted {
            inspection,
            bytes,
            sending,
        } => match shared_group_for_link(&inspection, &bytes) {
            Ok((record, _)) => {
                let (local_displays, peer_displays) = (
                    display_views(&inspection.local_displays),
                    display_views(&inspection.peer_displays),
                );
                show(state, key, |view| {
                    view.local_displays.clone_from(&local_displays);
                    view.peer_displays.clone_from(&peer_displays);
                    view.synchronized_layout = Some(record.layout().clone());
                });
                state.clock = state.clock.max(record.revision());
                let local = inspection.local_fingerprint.full_hex();
                let peer = peer_entry(&mut state.peers, key);
                peer.inspection = Some(inspection);
                let committed = peer.committed.take();
                peer.link_reason = None;
                peer.pending_proposal = None;
                peer.proposal_retry = None;
                advance_authorization(state);
                show(state, key, |view| {
                    view.phase = "connected";
                    view.busy = false;
                    view.sync = SyncView {
                        state: "applied",
                        message: LAYOUT_APPLIED_SHARING_ON.into(),
                    };
                    view.message = LAYOUT_APPLIED_SHARING_ON.into();
                });
                state.editing = false;
                // Every computer's switches follow the file the commit left, whichever computer
                // flipped them; a newer record written meanwhile is the one kept there.
                let mirrored = match committed {
                    Some(file) => {
                        mirror_file(state, &file);
                        true
                    }
                    None => match ActiveControl::of(&record, &local)
                        .filter(|control| !state.paused && control.members == state.enabled)
                    {
                        Some(control) => {
                            state.active_control = Some(control);
                            true
                        }
                        None => false,
                    },
                };
                if mirrored && reconcile_pending_control(state) {
                    show(state, key, |view| view.message = CONTROL_SUPERSEDED.into());
                }
                // The sender commits last, so only it may close; the receiver waits for that close.
                if sending {
                    close_after_apply(state, key);
                }
            }
            Err(_) => reject_sync(state, key, LinkRejectReason::Invalid, sending),
        },
        LinkEvent::SyncRejected { reason, sending } => reject_sync(state, key, reason, sending),
        LinkEvent::Disconnected { .. } if own_view(state, key).sync.state == "applied" => {
            // The setup is done on both computers; the peer leaving frees the port for sharing.
            close_after_apply(state, key);
        }
        // A standing link that lost its peer ends, so the supervisor can choose a session or a
        // fresh link; only an arranging link reconnects by itself.
        LinkEvent::Disconnected { .. }
            if !state.editing && own_view(state, key).phase == "connected" =>
        {
            peer_entry(&mut state.peers, key)
                .close_message
                .get_or_insert_with(|| PEER_LEFT.to_owned());
            close_link(state, key, PEER_LEFT);
        }
        LinkEvent::Disconnected { .. } => {
            advance_authorization(state);
            show(state, key, |view| {
                view.phase = "reconnecting";
                view.busy = true;
                view.sync = SyncView::default();
                view.message = "Lost the connection to the other computer. Reconnecting.".into();
            });
        }
        // Untrusted: a summary that does not parse, or whose stamp runs far past this computer's
        // clock, is dropped and the last good one stands.
        LinkEvent::PeerSummary { bytes } => {
            if let Ok(summary) = RecordSummary::parse(&bytes)
                && summary.stamp().is_none_or(|stamp| {
                    sharing_preferences::within_clock_step(state.clock, stamp.revision())
                })
            {
                if let Some(stamp) = summary.stamp() {
                    state.clock = state.clock.max(stamp.revision());
                }
                peer_entry(&mut state.peers, key).peer_summary = Some(summary);
            }
        }
        LinkEvent::Closed => {
            let message = peer_entry(&mut state.peers, key)
                .close_message
                .get_or_insert_with(|| NOT_CONNECTED.to_owned())
                .clone();
            show(state, key, |view| {
                if view.sync.state != "applied" {
                    view.sync = SyncView::default();
                }
                if view.phase != "stopping" {
                    view.phase = "off";
                    view.busy = false;
                    view.message.clone_from(&message);
                }
            });
            return true;
        }
    }
    false
}

/// Geometry may have changed with every connect, so the previous authorization is spent, the
/// decider waits for the displays to hold still again, and the cards redraw from them.
fn adopt_inspection(state: &mut State, key: &str, inspection: InspectedPeer) {
    advance_setup_revision(state);
    let (local_displays, peer_displays) = (
        display_views(&inspection.local_displays),
        display_views(&inspection.peer_displays),
    );
    let peer = peer_entry(&mut state.peers, key);
    peer.displays_changed_at = Some(Instant::now());
    peer.inspection = Some(inspection);
    advance_authorization(state);
    show(state, key, |view| {
        view.local_displays.clone_from(&local_displays);
        view.peer_displays.clone_from(&peer_displays);
        view.phase = "connected";
        view.busy = false;
        view.sync = SyncView::default();
    });
}

fn only_relabeled(state: &State, key: &str, inspection: &InspectedPeer) -> bool {
    state
        .peers
        .get(key)
        .and_then(|peer| peer.inspection.as_ref())
        .is_some_and(|seen| {
            seen.local_displays
                .same_geometry(&inspection.local_displays)
                && seen.peer_displays.same_geometry(&inspection.peer_displays)
        })
}

/// Labels are cosmetic: the arrangement being made, its authorization and the settle clock stand.
fn adopt_labels(state: &mut State, key: &str, inspection: InspectedPeer) {
    advance_setup_revision(state);
    let (local_displays, peer_displays) = (
        display_views(&inspection.local_displays),
        display_views(&inspection.peer_displays),
    );
    show(state, key, |view| {
        view.local_displays.clone_from(&local_displays);
        view.peer_displays.clone_from(&peer_displays);
    });
    peer_entry(&mut state.peers, key).inspection = Some(inspection);
}

/// A refusal that follows this computer's own commit means the pair no longer agrees on disk.
/// Ends the link deliberately once both computers hold the layout; the supervisor then shares.
fn close_after_apply(state: &mut State, key: &str) {
    peer_entry(&mut state.peers, key)
        .close_message
        .get_or_insert_with(|| LAYOUT_APPLIED_SHARING_ON.to_owned());
    close_link(state, key, LAYOUT_APPLIED_SHARING_ON);
}

fn reject_sync(state: &mut State, key: &str, reason: LinkRejectReason, sending: bool) {
    // Only a supervisor proposal is retried or answered; a user's Apply is theirs to repeat. The
    // other computer finding the layout unusable is the one refusal that says "nothing fits".
    let flip_pending = state.pending_control.is_some();
    let pending = state
        .peers
        .get_mut(key)
        .and_then(|peer| peer.pending_proposal.take());
    if let Some(geometry) = pending.filter(|_| sending) {
        if reason == LinkRejectReason::Invalid {
            answer_waiting(state, key, geometry);
        } else {
            note_passing_refusal(state, key, geometry);
        }
    }
    let disagreed = !sending
        && reason == LinkRejectReason::SaveFailed
        && own_view(state, key).sync.state == "applied";
    let message = if disagreed {
        ONLY_THIS_COMPUTER_SAVED
    } else if !sending && reason == LinkRejectReason::Busy {
        NEWER_HERE
    } else if flip_pending && state.pending_control.is_none() {
        CONTROL_REFUSED
    } else {
        reject_message(reason)
    }
    .to_owned();
    if disagreed {
        // Both computers must hold the same layout before either shares; one saved copy is not
        // enough, so the supervisor keeps a link up until a fresh Apply lands on both.
        let peer = peer_entry(&mut state.peers, key);
        peer.link_reason = Some(LinkReason::LayoutUnconfirmed);
        if peer.close_message.is_some() {
            peer.close_message = Some(message.clone());
        }
    }
    show(state, key, |view| {
        view.sync = SyncView {
            state: "rejected",
            message: message.clone(),
        };
        view.message.clone_from(&message);
    });
}

fn connected_message(state: &State, key: &str) -> &'static str {
    if state.pending_control.is_some() {
        return CONNECTED_CHANGING_CONTROL;
    }
    match state.peers.get(key).and_then(|peer| peer.link_reason) {
        None | Some(LinkReason::PeerHoldsLink) => CONNECTED_ARRANGE,
        Some(LinkReason::LayoutMisfit) => CONNECTED_LAYOUT_STALE,
        Some(LinkReason::LayoutUnconfirmed) => ONLY_THIS_COMPUTER_SAVED,
        Some(LinkReason::ControlChanged) => CONNECTED_CHANGING_CONTROL,
    }
}

const ONLY_THIS_COMPUTER_SAVED: &str =
    "This computer saved the layout, but the other computer could not. Apply again.";
/// Only this computer's own staging refuses as Busy: it holds a newer, different layout.
const NEWER_HERE: &str =
    "This computer holds a newer layout, so the other computer's older one was not applied.";

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
/// `epoch`, claimed when the worker was registered, binds every step to this one link, so a Stop
/// or switch-off at any point after leaves the applied setup alone even when the transport asks
/// to commit afterwards.
fn link_persist(
    state: Arc<Mutex<State>>,
    key: String,
    generation: u64,
    epoch: u64,
    path: PathBuf,
    setup_file: Arc<SetupFileLock>,
) -> LinkPersist {
    let commit_state = Arc::clone(&state);
    let commit_path = path.clone();
    let commit_key = key.clone();
    let discard_state = Arc::clone(&state);
    let discard_key = key.clone();
    LinkPersist {
        stage: Arc::new(move |fresh, bytes| {
            stage_shared_setup(&state, &key, generation, epoch, &path, fresh, bytes)
        }),
        commit: Arc::new(move |current, _| {
            // The state lock is released before the write: disk work must never block the UI thread.
            let mut applied = None;
            let local = current.local_fingerprint.full_hex();
            let (file, adopted) = setup_file.commit(&commit_path, &local, || {
                let mut state = lock(&commit_state);
                if link_retired(&state, &commit_key, generation, epoch) {
                    return Err(LinkRejectReason::Cancelled);
                }
                let (staged, left_out) = peer_entry(&mut state.peers, &commit_key)
                    .staged
                    .take()
                    .ok_or(LinkRejectReason::SaveFailed)?;
                // A label that caught up after staging is saved; a display that moved unwound this.
                let staged = staged.relabeled(&KnownDisplays::of_link(current));
                applied = Some((staged.clone(), left_out));
                Ok((staged, state.clock))
            })?;
            if let Some((applied, left_out)) = applied {
                {
                    let mut state = lock(&commit_state);
                    resolve_notice_on_commit(&mut state, &commit_key);
                    peer_entry(&mut state.peers, &commit_key).committed = Some(file);
                }
                // A layout that had to leave something out is a stopgap, not an arrangement, and
                // one the file kept a newer record over is not what this computer runs.
                if adopted && !left_out {
                    remember_applied(&commit_path, &applied, &local);
                }
                crate::autostart::setup_applied(&commit_path);
                // Only now, so a window that reads again finds the remembered layout as well.
                advance_setup_revision(&mut lock(&commit_state));
            }
            Ok(())
        }),
        discard: Arc::new(move || {
            if let Some(peer) = lock(&discard_state).peers.get_mut(&discard_key) {
                peer.staged = None;
            }
        }),
    }
}

/// True once shutdown, a Stop, or a newer link spent the epoch this persistence belongs to.
fn link_retired(state: &State, key: &str, generation: u64, epoch: u64) -> bool {
    state.shutdown
        || generation_of(state, key) != generation
        || state.peers.get(key).map_or(0, |peer| peer.link_epoch) != epoch
}

fn stage_shared_setup(
    shared: &Arc<Mutex<State>>,
    key: &str,
    generation: u64,
    epoch: u64,
    path: &Path,
    fresh: &InspectedPeer,
    bytes: &[u8],
) -> Result<(), LinkRejectReason> {
    let staged = shared_group_for_link(fresh, bytes).map_err(|error| match error {
        PreferenceError::InspectionChanged => LinkRejectReason::InspectionChanged,
        PreferenceError::Invalid => LinkRejectReason::Invalid,
    })?;
    {
        let state = lock(shared);
        if link_retired(&state, key, generation, epoch) {
            return Err(LinkRejectReason::Cancelled);
        }
        if state
            .peers
            .get(key)
            .and_then(|peer| peer.inspection.as_ref())
            .is_some_and(|current| !current.matches(fresh))
        {
            return Err(LinkRejectReason::InspectionChanged);
        }
        // Taken, a revision that far ahead would leave later changes here no newer stamp.
        if !sharing_preferences::within_clock_step(state.clock, staged.0.revision()) {
            return Err(LinkRejectReason::Invalid);
        }
    }
    // The file must be writable now: a damaged file, or one a newer MonHop wrote, would fail the
    // commit after the peer saved.
    let file = SetupFile::load(path).map_err(|_| LinkRejectReason::SaveFailed)?;
    if file.written_by_newer() {
        return Err(LinkRejectReason::SaveFailed);
    }
    if !keeps_enabled(&staged.0, &file, &fresh.local_fingerprint.full_hex()) {
        return Err(LinkRejectReason::Invalid);
    }
    // Monotone: a record older than the active one, with other content, would undo a newer
    // choice; its sender learns of the newer one from this computer's summary.
    if let Some(held) = file.active_group().map(GroupRecord::stamp) {
        let proposed = staged.0.stamp();
        if held > proposed && !held.same_content(&proposed) {
            return Err(LinkRejectReason::Busy);
        }
    }
    let mut state = lock(shared);
    if link_retired(&state, key, generation, epoch) {
        return Err(LinkRejectReason::Cancelled);
    }
    peer_entry(&mut state.peers, key).staged = Some(staged);
    Ok(())
}

/// Whether taking `staged` leaves the computers switched on here as the user set them: a group
/// must be exactly them, and a pair may switch on its own computer but never switch another off.
fn keeps_enabled(staged: &GroupRecord, file: &SetupFile, local: &str) -> bool {
    let members = staged.member_keys();
    let active = active_members(file, local);
    if members.len() > 2 {
        members == active
    } else {
        active.iter().all(|member| members.contains(member))
    }
}

/// Sorted lowercase fingerprints of `local` and every enabled computer.
fn active_members(file: &SetupFile, local: &str) -> Vec<String> {
    let mut members = file.enabled().to_vec();
    members.push(fingerprint_key(local));
    members.sort();
    members.dedup();
    members
}

impl Drop for SharingController {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

fn register_native_cancellation(
    state: &mut State,
    key: &str,
    worker_generation: u64,
    authorization_revision: u64,
    cancel: &RevocationSignal,
    native: RevocationSignal,
) -> Result<(), String> {
    if generation_of(state, key) != worker_generation
        || peer_authorization(state, key) != authorization_revision
        || state.shutdown
        || cancel.is_revoked()
        || native.is_revoked()
    {
        native.mark_revoked_without_wake();
        return Err("Sharing cancelled.".into());
    }
    // Stop sees the native latch before construction, without waiting for a relay or peer.
    peer_entry(&mut state.peers, key).native_cancel = Some(native);
    Ok(())
}

fn touch_link(peer: &mut PeerState) {
    if peer.link.is_some() {
        peer.link_touched = Some(Instant::now());
    }
}

/// Platform and control fields are derived here so no call site can publish a stale pairing.
fn view_of(state: &State) -> SharingView {
    render(state, None)
}

/// The view Home shows. With one worker running it is that computer's own view; with none, the
/// shared view, which every change reaches; with several, the furthest along of theirs, with
/// what names one computer left out. `live` carries whether native cleanup is pending, and asks
/// for each session's progress to be read as well.
fn render(state: &State, live: Option<bool>) -> SharingView {
    let running: Vec<&PeerState> = state
        .peers
        .values()
        .filter(|peer| peer.running.is_some())
        .collect();
    let single = match running.as_slice() {
        [peer] => Some(*peer),
        _ => None,
    };
    let mut view = match running.as_slice() {
        [] => {
            let mut view = state.view.clone();
            if live.is_some() {
                view.held = false;
            }
            view
        }
        [peer] => shown_view(state, peer, live.is_some()),
        several => aggregate(
            several
                .iter()
                .map(|peer| shown_view(state, peer, live.is_some()))
                .collect(),
        ),
    };
    view.revision.clone_from(&state.view.revision);
    view.link.since = single
        .and_then(|peer| peer.link_since)
        .map(|since| human_duration(since.elapsed()))
        .unwrap_or_default();
    view.setup_revision = state.setup_revision.to_string();
    let inspection = single.and_then(|peer| peer.inspection.as_ref());
    view.local_platform = platform_name(
        inspection.map_or_else(local_platform, |inspection| inspection.local_platform),
    );
    view.peer_platform = inspection.map(|inspection| platform_name(inspection.peer_platform));
    view.peer_fingerprint = single
        .and_then(|peer| peer.running)
        .map(|peer| peer_key(&peer));
    view.active.clone_from(&state.active);
    view.editing = state.editing;
    view.display_notice = state.display_notice;
    view.control = control_view(state);
    let failure = latest_failure(state);
    view.last_failure = drop_note(
        failure.and_then(|peer| peer.last_failure.as_deref()),
        failure
            .and_then(|peer| peer.last_failure_at)
            .map(|at| at.elapsed()),
    );
    view.enabled.clone_from(&state.enabled);
    view.paused = state.paused;
    view.peers = peer_views(state, live);
    view.members = member_views(state);
    view
}

/// `peer`'s own view; with `live`, as its session's progress reports it now.
fn shown_view(state: &State, peer: &PeerState, live: bool) -> SharingView {
    let mut view = peer.view.clone().unwrap_or_else(|| state.view.clone());
    if !live {
        return view;
    }
    let progress = peer.progress.as_ref().filter(|_| peer.running.is_some());
    let mut held = false;
    if let Some(progress) = progress {
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
    let started = progress.is_some_and(session::SessionProgress::is_started)
        || peer
            .share_started
            .as_ref()
            .is_some_and(|started| started.load(Ordering::Acquire));
    if view.phase == "starting" && started {
        view.phase = "sharing";
        view.sharing_active = true;
        view.message = SHARING_ENABLED.into();
    }
    view.held = held && view.phase == "sharing";
    view.blocking_presses = match progress.filter(|_| view.phase == "sharing") {
        Some(progress) => progress
            .blocking_presses()
            .into_iter()
            .map(key_names::name)
            .collect(),
        None => Vec::new(),
    };
    view
}

/// Several computers' views as one: the furthest along leads, and any of them busy, sharing or
/// held makes the whole so.
fn aggregate(views: Vec<SharingView>) -> SharingView {
    let busy = views.iter().any(|view| view.busy);
    let sharing = views.iter().any(|view| view.sharing_active);
    let held = views.iter().any(|view| view.held);
    let mut lead = views
        .into_iter()
        .enumerate()
        .max_by_key(|(index, view)| (phase_rank(view.phase), std::cmp::Reverse(*index)))
        .map(|(_, view)| view)
        .unwrap_or_default();
    lead.busy = busy;
    lead.sharing_active = sharing;
    lead.held = held;
    lead.peer_displays.clear();
    lead.link = LinkView::default();
    lead.synchronized_layout = None;
    lead
}

/// Sharing outranks starting, then a connected link, a connecting one, stopping, an error, off.
fn phase_rank(phase: &str) -> u8 {
    match phase {
        "sharing" => 7,
        "starting" => 6,
        "connected" => 5,
        "connecting" | "reconnecting" => 4,
        "stopping" => 3,
        "error" => 2,
        _ => 1,
    }
}

/// Every enabled or running computer on its own.
fn peer_views(state: &State, live: Option<bool>) -> Vec<PeerView> {
    let mut keys: BTreeSet<&str> = state.enabled.iter().map(String::as_str).collect();
    keys.extend(
        state
            .peers
            .iter()
            .filter(|(_, peer)| peer.running.is_some())
            .map(|(key, _)| key.as_str()),
    );
    keys.into_iter()
        .map(|key| {
            let peer = state.peers.get(key);
            let mut view = match peer {
                Some(peer) if peer.view.is_some() => shown_view(state, peer, live.is_some()),
                _ => SharingView {
                    message: if state.paused { PAUSED } else { NOT_CONNECTED }.into(),
                    ..SharingView::default()
                },
            };
            if live == Some(true) && !view.busy {
                view.phase = "error";
                view.message = native_cleanup_message().into();
            }
            let not_paired = peer.is_some_and(|peer| peer.not_paired && peer.running.is_none());
            let inspection = peer
                .filter(|peer| peer.running.is_some())
                .and_then(|peer| peer.inspection.as_ref());
            // A session reports no displays into its view; the ones it met are the live ones.
            let displays = match inspection {
                Some(inspection) if view.peer_displays.is_empty() => {
                    display_views(&inspection.peer_displays)
                }
                _ => view.peer_displays,
            };
            PeerView {
                fingerprint: key.to_owned(),
                platform: inspection
                    .map(|inspection| platform_name(inspection.peer_platform))
                    .or_else(|| {
                        state
                            .held
                            .as_ref()
                            .and_then(|record| record.member(key))
                            .map(|member| platform_name(member.platform().into()))
                    }),
                phase: if not_paired { "notPaired" } else { view.phase },
                message: view.message,
                busy: view.busy,
                sharing_active: view.sharing_active,
                held: view.held,
                link: LinkView {
                    attempt: view.link.attempt,
                    since: peer
                        .and_then(|peer| peer.link_since)
                        .map(|since| human_duration(since.elapsed()))
                        .unwrap_or_default(),
                },
                sync: view.sync,
                displays,
                diagnostics: view.diagnostics,
                last_failure: drop_note(
                    peer.and_then(|peer| peer.last_failure.as_deref()),
                    peer.and_then(|peer| peer.last_failure_at)
                        .map(|at| at.elapsed()),
                ),
                peer_arranging: peer.is_some_and(|peer| peer.peer_arranging),
            }
        })
        .collect()
}

/// The active group's members, each with the displays a live connection shows for it, else
/// those its record holds.
fn member_views(state: &State) -> Vec<MemberView> {
    let Some(record) = state.held.as_ref() else {
        return Vec::new();
    };
    let local = state.local.as_deref().map(fingerprint_key);
    let inspections = || {
        state
            .peers
            .values()
            .filter(|peer| peer.running.is_some())
            .filter_map(|peer| peer.inspection.as_ref())
    };
    record
        .members()
        .iter()
        .map(|member| {
            let key = fingerprint_key(member.fingerprint());
            let is_local = local.as_deref() == Some(key.as_str());
            let running = state
                .peers
                .get(&key)
                .is_some_and(|peer| peer.running.is_some());
            let live_displays = if is_local {
                inspections()
                    .next()
                    .map(|inspection| &inspection.local_displays)
            } else {
                inspections()
                    .find(|inspection| peer_key(&inspection.peer_fingerprint) == key)
                    .map(|inspection| &inspection.peer_displays)
            };
            let displays = live_displays
                .map(display_views)
                .or_else(|| topology_of(member.displays()).map(|displays| display_views(&displays)))
                .unwrap_or_default();
            MemberView {
                fingerprint: key,
                local: is_local,
                platform: platform_name(member.platform().into()),
                displays,
                live: is_local || running,
                record_revision: record.revision(),
            }
        })
        .collect()
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

/// The window only compares it for equality, so wrapping is harmless.
fn advance_setup_revision(state: &mut State) {
    state.setup_revision = state.setup_revision.wrapping_add(1);
}

/// Spends the window's authorization and `scope`'s own, and forgets `scope`'s displays (every
/// computer's for None).
fn invalidate_authorization(state: &mut State, scope: Option<&str>) {
    advance_authorization(state);
    for peer in scoped(&mut state.peers, scope) {
        peer.inspection = None;
        peer.authorized = None;
    }
    let forget = |view: &mut SharingView| {
        view.local_displays.clear();
        view.peer_displays.clear();
        view.synchronized_layout = None;
    };
    match scope {
        Some(key) => show(state, key, forget),
        None => all_views(state).for_each(forget),
    }
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
const SHUTTING_DOWN: &str = "MonHop is shutting down. Input is local.";

fn show_idle(view: &mut SharingView, shutdown: bool, cleanup: bool) {
    if cleanup {
        view.phase = "error";
        view.message = native_cleanup_message().into();
    } else {
        view.phase = "off";
        view.message = if shutdown {
            SHUTTING_DOWN
        } else {
            NOT_CONNECTED
        }
        .into();
    }
}

/// For every computer in `scope` (every one for None) whose worker is not busy: its progress and
/// cancels go, and its view says input is local, or that native cleanup still needs attention.
fn settle_idle(state: &mut State, scope: Option<&str>, shutdown: bool, cleanup: bool) {
    settle_idle_with(state, scope, shutdown, cleanup, |_| {});
}

/// [`settle_idle`], then `then` on each view it settled.
fn settle_idle_with(
    state: &mut State,
    scope: Option<&str>,
    shutdown: bool,
    cleanup: bool,
    then: impl Fn(&mut SharingView),
) {
    let idle = |view: &mut SharingView| {
        show_idle(view, shutdown, cleanup);
        then(view);
    };
    match scope {
        Some(key) => {
            if peer_busy(state, key) {
                return;
            }
            if let Some(peer) = state.peers.get_mut(key) {
                peer.progress = None;
                peer.cancel = None;
                peer.native_cancel = None;
            }
            show(state, key, idle);
        }
        None => {
            for peer in state
                .peers
                .values_mut()
                .filter(|peer| peer.view.as_ref().is_none_or(|view| !view.busy))
            {
                peer.progress = None;
                peer.cancel = None;
                peer.native_cancel = None;
                if let Some(view) = peer.view.as_mut() {
                    idle(view);
                }
            }
            if state.view.busy {
                return;
            }
            idle(&mut state.view);
        }
    }
    state.native_cleanup_pending = cleanup;
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
    use crate::sharing_preferences::both_directions;
    use crate::sharing_preferences::tests::group;
    use monhop_core::DeviceId;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    fn fixture_peer() -> CertificateFingerprint {
        CertificateFingerprint::parse_full(&"B".repeat(64)).unwrap()
    }

    fn fixture_key() -> String {
        peer_key(&fixture_peer())
    }

    /// [`link_persist`] under a newly claimed epoch, as `launch` claims one for each link worker.
    fn claimed_link_persist(
        state: Arc<Mutex<State>>,
        key: String,
        generation: u64,
        path: PathBuf,
        setup_file: Arc<SetupFileLock>,
    ) -> LinkPersist {
        let epoch = claim_epoch(peer_entry(&mut lock(&state).peers, &key));
        link_persist(state, key, generation, epoch, path, setup_file)
    }

    /// The fixture computer's entry, made on first use.
    fn fixture_entry(state: &mut State) -> &mut PeerState {
        peer_entry(&mut state.peers, &fixture_key())
    }

    /// The fixture computer's entry, marked as the one a worker runs for.
    fn live_fixture(state: &mut State) -> &mut PeerState {
        let peer = fixture_entry(state);
        peer.running = Some(fixture_peer());
        peer
    }

    fn no_inspection(controller: &SharingController) -> bool {
        lock(&controller.state)
            .peers
            .values()
            .all(|peer| peer.inspection.is_none())
    }

    static NEXT_SYNC_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    /// One fake link runner at a time; link tests also hold NATIVE_LIFECYCLE_TEST_LOCK.
    static LINK_FIXTURES: Mutex<Option<std::sync::mpsc::Sender<LinkFixture>>> = Mutex::new(None);

    pub(crate) struct LinkFixture {
        pub(crate) events: UnboundedSender<LinkEvent>,
        pub(crate) proposals: std::sync::mpsc::Receiver<Vec<u8>>,
        /// Every arranging state the controller told the link to publish, in order.
        arranging: std::sync::mpsc::Receiver<bool>,
        /// Every summary the controller told the link to publish, in order.
        pub(crate) summaries: std::sync::mpsc::Receiver<Vec<u8>>,
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
            let (summarized, summaries) = std::sync::mpsc::channel();
            if let Some(sender) = lock(&LINK_FIXTURES).as_ref() {
                let _ = sender.send(LinkFixture {
                    events: events.clone(),
                    proposals,
                    arranging,
                    summaries,
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
                        Some(LinkCommand::Summary(bytes)) => {
                            let _ = summarized.send(bytes);
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

    /// Every fake link opened from now on hands its fixture to the receiver returned.
    pub(crate) fn expect_fake_links() -> std::sync::mpsc::Receiver<LinkFixture> {
        let (sender, fixtures) = std::sync::mpsc::channel();
        *lock(&LINK_FIXTURES) = Some(sender);
        fixtures
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
        for peer in lock(&controller.state)
            .peers
            .values_mut()
            .filter(|peer| peer.running.is_some())
        {
            peer.displays_changed_at =
                Some(Instant::now() - DISPLAYS_SETTLE - Duration::from_millis(1));
        }
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
        let bytes = shared_group_bytes(&inspection, &group(&preferences), false).unwrap();
        (inspection, bytes)
    }

    /// The arrangements made for the link's two computers, each fitting while both show it.
    fn library_views(
        library: &ArrangementLibrary,
        inspection: &InspectedPeer,
    ) -> Vec<ArrangementView> {
        library.views(
            &[
                inspection.local_fingerprint.full_hex(),
                inspection.peer_fingerprint.full_hex(),
            ],
            &KnownDisplays::of_link(inspection),
        )
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
            if lock(&controller.workers)
                .values()
                .any(JoinHandle::is_finished)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let worker = {
            let mut workers = lock(&controller.workers);
            let key = workers
                .iter()
                .find(|(_, worker)| worker.is_finished())
                .or_else(|| workers.iter().next())
                .map(|(key, _)| key.clone());
            key.and_then(|key| workers.remove(&key))
        }
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
        let layout = saved.layout.clone();
        connected_revision(&controller, 4);
        live_fixture(&mut lock(&controller.state)).inspection = Some(inspected);
        let before = controller.save_setup(&path, "4", layout.clone()).unwrap();
        assert!(serde_json::to_value(before).unwrap()["layout"].is_object());
        let saved_bytes = std::fs::read(&path).unwrap();
        let file = SetupFile::load(&path).unwrap();
        assert_eq!(file.active(), Some("b".repeat(64).as_str()));
        assert_eq!(
            file.active_group().map(|saved| saved.layout()),
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
        assert!(lock(&restarted.workers).is_empty());
        drop(directory);
    }

    #[test]
    // Forgetting moved to the per-computer command, which needs no connection; see lifecycle.
    fn named_arrangements_save_replace_and_list_for_the_connected_pair() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let path = path.with_file_name(ARRANGEMENTS_FILE);
        let controller = SharingController::default();
        assert!(controller.arrangements(&path).unwrap().is_empty());
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let mut layout = saved.layout.clone();
        connected_revision(&controller, 4);
        live_fixture(&mut lock(&controller.state)).inspection = Some(inspected);
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
        assert!(file["arrangements"][0]["record"]["sharingEnabled"].is_null());
        assert_eq!(file["version"], 3);
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

    /// `state` with `peer` as the fixture computer's entry.
    fn with_fixture(mut state: State, peer: PeerState) -> State {
        state.peers.insert(fixture_key(), peer);
        state
    }

    /// The state a raised banner leaves behind, ready for a commit or a refusal to settle.
    fn raised(kind: DisplayNotice, inspected: &InspectedPeer, left_out: bool) -> State {
        with_fixture(
            State {
                display_notice: Some(kind),
                ..State::default()
            },
            PeerState {
                notice_answer: Some(kind),
                notice_geometry: Some(DisplayGeometry::of(inspected)),
                notice_left_out: left_out,
                ..PeerState::default()
            },
        )
    }

    #[test]
    fn a_commit_leaves_a_line_only_where_the_new_layout_left_something_out() {
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let key = fixture_key();
        // A layout that fit exactly, and the computer that only accepted the other's choice:
        // the change was answered in full, so Home has nothing to tell anyone.
        for raised_kind in [DisplayNotice::Updating, DisplayNotice::PeerDeciding] {
            let mut state = raised(raised_kind, &inspected, false);
            resolve_notice_on_commit(&mut state, &key);
            assert!(state.display_notice.is_none());
            // The memory goes with the answered change, so the next one is judged afresh.
            assert!(fixture_entry(&mut state).notice_geometry.is_none());
        }
        // The decider had to drop a crossing or a display to keep sharing going.
        let mut lost = raised(DisplayNotice::Updating, &inspected, true);
        resolve_notice_on_commit(&mut lost, &key);
        assert_eq!(lost.display_notice, Some(DisplayNotice::Continued));
        assert!(fixture_entry(&mut lost).notice_geometry.is_none());
        assert!(!fixture_entry(&mut lost).notice_left_out);
        // The computer that only accepted that layout still says nothing: one computer reports
        // the loss, and it is the one that chose.
        let mut accepted = raised(DisplayNotice::PeerDeciding, &inspected, true);
        resolve_notice_on_commit(&mut accepted, &key);
        assert!(accepted.display_notice.is_none());
        // A user's own Apply raised no banner, so it leaves none behind either.
        let mut applied = State::default();
        resolve_notice_on_commit(&mut applied, &key);
        assert!(applied.display_notice.is_none());
        // A dismissed banner is not brought back by the commit that answered it.
        let mut dismissed = State {
            display_notice: None,
            ..raised(DisplayNotice::Updating, &inspected, true)
        };
        resolve_notice_on_commit(&mut dismissed, &key);
        assert!(dismissed.display_notice.is_none());
    }

    #[test]
    fn a_refusal_asks_for_arranging_and_leaves_nothing_in_flight() {
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let key = fixture_key();
        let mut refused = with_fixture(
            State {
                display_notice: Some(DisplayNotice::Updating),
                ..State::default()
            },
            PeerState {
                notice_answer: Some(DisplayNotice::Updating),
                notice_geometry: Some(DisplayGeometry::of(&inspected)),
                pending_proposal: Some(DisplayGeometry::of(&inspected)),
                ..PeerState::default()
            },
        );
        reject_sync(&mut refused, &key, LinkRejectReason::Invalid, true);
        assert_eq!(refused.display_notice, Some(DisplayNotice::Waiting));
        // The answer stays with the displays it was given for, so the same change is never
        // proposed a second time, and nothing is left in flight.
        assert_eq!(
            fixture_entry(&mut refused).notice_answer,
            Some(DisplayNotice::Waiting)
        );
        assert!(fixture_entry(&mut refused).notice_geometry.is_some());
        assert!(fixture_entry(&mut refused).pending_proposal.is_none());
        // A dismissal hides the banner without unanswering the displays behind it.
        let mut dismissed = with_fixture(
            State::default(),
            PeerState {
                notice_answer: Some(DisplayNotice::Updating),
                notice_geometry: Some(DisplayGeometry::of(&inspected)),
                pending_proposal: Some(DisplayGeometry::of(&inspected)),
                ..PeerState::default()
            },
        );
        reject_sync(&mut dismissed, &key, LinkRejectReason::Invalid, true);
        assert_eq!(
            fixture_entry(&mut dismissed).notice_answer,
            Some(DisplayNotice::Waiting)
        );
        assert!(dismissed.display_notice.is_none());
        let mut plain = State::default();
        reject_sync(&mut plain, &key, LinkRejectReason::Invalid, true);
        assert!(plain.display_notice.is_none());
    }

    #[test]
    fn the_supervisor_proposes_a_layout_over_the_link_without_a_window_revision() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let idle = SharingController::default();
        let next = group(&crate::sharing_preferences::tests::preferences());
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
        // The bytes are the shared group an Apply sends: the other computer stages them the
        // same way, and they name the same layout and the same control map, stamped as this
        // computer's change past its clock.
        let (staged, left_out) = shared_group_for_link(&inspection, &proposed).unwrap();
        assert_eq!(staged.layout(), next.layout());
        assert_eq!(staged.content_digest(), next.content_digest());
        assert_eq!(
            (staged.revision(), staged.author()),
            (1, "A".repeat(64).as_str())
        );
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
        resolve_notice_on_commit(&mut lock(&controller.state), &fixture_key());
        assert!(controller.status().display_notice.is_none());
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
    }

    #[test]
    fn a_sent_layout_that_dropped_something_is_what_home_reports_after_the_commit() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        let next = group(&crate::sharing_preferences::tests::preferences());
        controller.propose_layout(&next, true).unwrap();
        assert_eq!(
            controller.status().display_notice,
            Some(DisplayNotice::Updating)
        );
        resolve_notice_on_commit(&mut lock(&controller.state), &fixture_key());
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
        let next = group(&crate::sharing_preferences::tests::preferences());
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
        fixture_entry(&mut lock(&controller.state))
            .proposal_retry
            .as_mut()
            .unwrap()
            .2 -= PROPOSAL_RETRY_AFTER;
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
        assert!(!controller.within_failure_backoff(fixture_peer(), backoff));
        controller
            .connect_link(path.clone(), "en0:4:192.168.1.4".into(), fixture_peer())
            .expect("the link must start");
        join_finished_worker(&controller);
        assert_eq!(controller.status().phase, "error");
        assert!(controller.within_failure_backoff(fixture_peer(), backoff));
        // Choosing the computer again is a fresh user intent, so the wait is over at once.
        controller
            .set_active(&path, Some(fixture_peer()), None)
            .unwrap();
        assert!(!controller.within_failure_backoff(fixture_peer(), backoff));

        // A misfit and a deliberate close are steps, not failures, and never hold anything off.
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        note_layout_misfit(&mut lock(&controller.state), &fixture_key());
        close_link(&mut lock(&controller.state), &fixture_key(), LAYOUT_MISFIT);
        join_finished_worker(&controller);
        assert!(!controller.within_failure_backoff(fixture_peer(), backoff));
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        assert!(!controller.within_failure_backoff(fixture_peer(), backoff));
        drop(directory);
    }

    #[test]
    fn a_layout_that_no_longer_fits_ends_the_worker_without_an_error_or_a_lost_reason() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        // What a dialed session does when the displays no longer fit the saved layout.
        note_layout_misfit(&mut lock(&controller.state), &fixture_key());
        assert!(controller.holds_link(fixture_peer()));
        close_link(&mut lock(&controller.state), &fixture_key(), LAYOUT_MISFIT);
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, LAYOUT_MISFIT);
        assert!(view.last_failure.is_empty());
        // The reason outlives the worker, so the supervisor opens a link instead of a session.
        assert!(controller.holds_link(fixture_peer()));
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
        note_displays_changed(
            &mut lock(&controller.state),
            &fixture_key(),
            DISPLAYS_CHANGED,
        );
        assert!(controller.holds_link(fixture_peer()));
        close_link(
            &mut lock(&controller.state),
            &fixture_key(),
            DISPLAYS_CHANGED,
        );
        join_finished_worker(&controller);
        let view = controller.status();
        // Not an error and not a drop, so the supervisor opens the link on its next pass
        // instead of waiting out ten seconds.
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, DISPLAYS_CHANGED);
        assert!(view.last_failure.is_empty());
        assert!(
            fixture_entry(&mut lock(&controller.state))
                .worker_failed_at
                .is_none()
        );
        assert!(!controller.within_failure_backoff(fixture_peer(), Duration::from_secs(10)));
        assert!(controller.holds_link(fixture_peer()));
        assert!(controller.link_is_off());
    }

    #[test]
    fn a_session_the_other_computer_ended_for_control_reopens_the_link_without_a_backoff() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        // What a session records when the other computer ends it to change control instead.
        note_control_changed(&mut lock(&controller.state), &fixture_key());
        assert!(controller.holds_link(fixture_peer()));
        close_link(
            &mut lock(&controller.state),
            &fixture_key(),
            PEER_CONTROL_CHANGED,
        );
        join_finished_worker(&controller);
        let view = controller.status();
        // Not an error and not a drop, so the supervisor opens the link on its next pass
        // instead of waiting out ten seconds.
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, PEER_CONTROL_CHANGED);
        assert!(view.last_failure.is_empty());
        assert!(
            fixture_entry(&mut lock(&controller.state))
                .worker_failed_at
                .is_none()
        );
        assert!(!controller.within_failure_backoff(fixture_peer(), Duration::from_secs(10)));
        assert!(controller.holds_link(fixture_peer()));
        assert!(controller.link_is_off());
    }

    #[test]
    fn a_link_that_meets_the_other_computer_starting_a_session_ends_without_an_error() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = controller_with(purpose_mismatch_link, Duration::from_secs(600));
        let (directory, path) = sync_test_path();
        fixture_entry(&mut lock(&controller.state)).link_reason = Some(LinkReason::LayoutMisfit);
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
        assert!(controller.holds_link(fixture_peer()));
        drop(directory);
    }

    #[test]
    fn an_applied_layout_is_remembered_for_its_displays_and_puts_the_banner_down() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let controller = SharingController::default();
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        let layout = saved.layout.clone();
        connected_revision(&controller, 4);
        live_fixture(&mut lock(&controller.state)).inspection = Some(inspected.clone());
        assert!(controller.raise_display_notice(DisplayNotice::Waiting, &inspected));
        controller.save_setup(&path, "4", layout).unwrap();
        assert!(controller.status().display_notice.is_none());
        let library = ArrangementLibrary::load(&path.with_file_name(ARRANGEMENTS_FILE)).unwrap();
        let listed = library_views(&library, &inspected);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].automatic);
        assert_eq!(
            serde_json::to_value(&listed[0]).unwrap()["automatic"],
            serde_json::json!(true)
        );
        assert!(
            library
                .automatic_fit(&group(&saved), &KnownDisplays::of_link(&inspected))
                .is_some()
        );
        let file = SetupFile::load(&path).unwrap();
        let applied = file.active_group().unwrap();
        assert_eq!(applied.content_digest(), group(&saved).content_digest());
        // A new local change, past the clock of a file that had none.
        assert_eq!(
            (applied.revision(), applied.author()),
            (1, "A".repeat(64).as_str())
        );
        assert!(applied.fits_link(&inspected));
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
        fixture_entry(&mut lock(&controller.state)).link_reason =
            Some(LinkReason::LayoutUnconfirmed);
        assert!(controller.inspection_for_switch().is_none());
        // A stale layout is exactly when a switch may act.
        fixture_entry(&mut lock(&controller.state)).link_reason = Some(LinkReason::LayoutMisfit);
        assert!(controller.inspection_for_switch().is_some());
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        assert!(controller.inspection_for_switch().is_none());
        // The peer's state never outlives its link.
        assert!(!fixture_entry(&mut lock(&controller.state)).peer_arranging);
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
        fixture_entry(&mut lock(&controller.state)).link_reason = Some(LinkReason::PeerHoldsLink);
        assert!(controller.holds_link(fixture_peer()));
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
        assert!(!controller.holds_link(fixture_peer()));
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
                &fixture_key(),
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
                &fixture_key(),
                0,
                0,
                &cancel,
                native.clone()
            )
            .is_err()
        );
        assert!(native.is_revoked());
        assert!(
            fixture_entry(&mut lock(&controller.state))
                .native_cancel
                .is_none()
        );
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
        assert!(lock(&controller.workers).is_empty());
        assert!(
            lock(&controller.state)
                .peers
                .values()
                .all(|peer| peer.cancel.is_none())
        );
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
        assert!(lock(&controller.workers).is_empty());
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
                fixture_peer(),
                |_, _, _, _, _, _| async { Ok(()) },
            )),
            "The setup changed. Connect the computers again."
        );
        assert!(lock(&controller.workers).is_empty());
        assert!(no_inspection(&controller));
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
                fixture_peer(),
                |state, _, _, _, cancel, _| async move {
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
        assert!(no_inspection(&controller));
        assert!(
            controller
                .launch(
                    "starting",
                    "fixture",
                    Some(original),
                    WorkerKind::Session,
                    fixture_peer(),
                    |_, _, _, _, _, _| async { panic!("old approval must never launch") },
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
                fixture_peer(),
                |_, _, _, _, _, _| async { Err("fixture input failure".into()) },
            )
            .unwrap();
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "error");
        assert_eq!(view.message, "fixture input failure");
        assert!(no_inspection(&controller));
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
                fixture_peer(),
                |_, _, _, _, _, _| async { Ok(()) },
            )),
            "Restart MonHop before starting another session."
        );
        assert!(lock(&controller.state).shutdown);
        assert!(lock(&controller.workers).is_empty());
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
                fixture_peer(),
                move |_, _, _, _, cancel, _| {
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
        assert!(no_inspection(&controller));
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
                fixture_peer(),
                move |_, _, _, _, cancel, _| {
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
        fixture_entry(&mut lock(&controller.state)).worker_generation = 2;
        let (directory, path) = sync_test_path();
        let original = b"previous inert setup".to_vec();
        std::fs::write(&path, &original).unwrap();
        let (fresh, bytes) = sync_payload();
        assert_eq!(
            stage_shared_setup(
                &controller.state,
                &fixture_key(),
                1,
                0,
                &path,
                &fresh,
                &bytes
            ),
            Err(LinkRejectReason::Cancelled)
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(temporary_count(&directory.0), 0);
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_none());
    }

    #[test]
    fn staged_setup_is_only_visible_after_commit() {
        let controller = SharingController::default();
        let (directory, path) = sync_test_path();
        let (fresh, bytes) = sync_payload();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&fresh, &bytes).unwrap();
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_some());
        assert!(!path.exists());
        (persist.commit)(&fresh, &bytes).unwrap();
        assert_eq!(temporary_count(&directory.0), 0);
        let file = SetupFile::load(&path).unwrap();
        assert_eq!(file.active(), Some("b".repeat(64).as_str()));
        assert_eq!(file.local(), Some("A".repeat(64).as_str()));
        // Both computers store exactly the agreed record.
        assert_eq!(
            file.active_group(),
            Some(&shared_group_for_link(&fresh, &bytes).unwrap().0)
        );

        (persist.stage)(&fresh, &bytes).unwrap();
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_some());
        (persist.discard)();
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_none());
        assert_eq!(
            (persist.commit)(&fresh, &bytes),
            Err(LinkRejectReason::SaveFailed)
        );
    }

    #[test]
    fn a_label_that_caught_up_after_staging_is_the_one_committed() {
        use session_setup::{DisplayDescription, DisplayTopology};
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let (fresh, bytes) = sync_payload();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&fresh, &bytes).unwrap();
        let mut named = fresh.clone();
        named.peer_displays = DisplayTopology::new(
            fresh
                .peer_displays
                .displays()
                .iter()
                .map(|display| DisplayDescription {
                    name: "Studio Display".into(),
                    ..display.clone()
                })
                .collect(),
        )
        .unwrap();
        (persist.commit)(&named, &bytes).unwrap();
        let file = SetupFile::load(&path).unwrap();
        let saved = file.active_group().unwrap();
        assert!(
            saved
                .member(&"B".repeat(64))
                .unwrap()
                .displays()
                .iter()
                .all(|display| display.name() == "Studio Display")
        );
    }

    #[test]
    fn only_the_links_own_pair_is_staged() {
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let trio = crate::sharing_preferences::tests::trio(3, true);
        let link =
            crate::sharing_preferences::tests::link_of(&trio, &"A".repeat(64), &"B".repeat(64));
        let bytes = shared_group_bytes(&link, &trio, false).unwrap();
        assert_eq!(
            stage_shared_setup(
                &controller.state,
                &fixture_key(),
                0,
                0,
                &path,
                &link,
                &bytes
            ),
            Err(LinkRejectReason::Invalid)
        );
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn a_pair_never_switches_off_another_computer_switched_on_here() {
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let mut file = SetupFile::default();
        file.adopt(
            &"A".repeat(64),
            crate::sharing_preferences::tests::trio(3, true),
        )
        .unwrap();
        file.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        // B's own pair record, newer than the group of three A holds.
        let preferences = crate::sharing_preferences::tests::preferences();
        let fresh = crate::sharing_preferences::tests::inspection(&preferences);
        let pair = group(&preferences).restamped(9, &"B".repeat(64)).unwrap();
        let bytes = shared_group_bytes(&fresh, &pair, false).unwrap();
        assert_eq!(
            stage_shared_setup(
                &controller.state,
                &fixture_key(),
                0,
                0,
                &path,
                &fresh,
                &bytes
            ),
            Err(LinkRejectReason::Invalid)
        );
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_pair_commit_after_another_computer_is_switched_on_writes_nothing() {
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let (fresh, bytes) = sync_payload();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&fresh, &bytes).unwrap();
        // C is switched on here, with B, while the pair was staged.
        let mut file = SetupFile::default();
        file.adopt(
            &"A".repeat(64),
            crate::sharing_preferences::tests::trio(3, true),
        )
        .unwrap();
        file.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            (persist.commit)(&fresh, &bytes),
            Err(LinkRejectReason::Invalid)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_record_stamped_far_past_the_clock_is_refused() {
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let preferences = crate::sharing_preferences::tests::preferences();
        let fresh = crate::sharing_preferences::tests::inspection(&preferences);
        let runaway = group(&preferences)
            .restamped(u64::MAX, &"B".repeat(64))
            .unwrap();
        let bytes = shared_group_bytes(&fresh, &runaway, false).unwrap();
        assert_eq!(
            stage_shared_setup(
                &controller.state,
                &fixture_key(),
                0,
                0,
                &path,
                &fresh,
                &bytes
            ),
            Err(LinkRejectReason::Invalid)
        );
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_none());
        // One a real run of changes ahead is taken.
        let ahead = group(&preferences)
            .restamped(5_000, &"B".repeat(64))
            .unwrap();
        let bytes = shared_group_bytes(&fresh, &ahead, false).unwrap();
        stage_shared_setup(
            &controller.state,
            &fixture_key(),
            0,
            0,
            &path,
            &fresh,
            &bytes,
        )
        .unwrap();
    }

    #[test]
    fn a_computer_switched_off_after_staging_stays_off_at_commit() {
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let (a, b, c) = ("A".repeat(64), "B".repeat(64), "C".repeat(64));
        let trio = crate::sharing_preferences::tests::trio(3, true);
        let mut file = SetupFile::default();
        file.adopt(&a, trio.clone()).unwrap();
        file.save(&path).unwrap();
        let link = crate::sharing_preferences::tests::link_of(&trio, &a, &b);
        let bytes = shared_group_bytes(&link, &trio, false).unwrap();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&link, &bytes).unwrap();
        controller
            .set_enabled(
                &path,
                CertificateFingerprint::parse_full(&c).unwrap(),
                false,
                None,
            )
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            (persist.commit)(&link, &bytes),
            Err(LinkRejectReason::Invalid)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(SetupFile::load(&path).unwrap().enabled(), ["b".repeat(64)]);
    }

    #[test]
    fn a_pair_commit_after_its_computer_is_switched_off_writes_nothing() {
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let (fresh, bytes) = sync_payload();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&fresh, &bytes).unwrap();
        (persist.commit)(&fresh, &bytes).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap().enabled(), ["b".repeat(64)]);
        (persist.stage)(&fresh, &bytes).unwrap();
        controller
            .set_enabled(&path, fixture_peer(), false, None)
            .unwrap();
        assert_eq!(
            (persist.commit)(&fresh, &bytes),
            Err(LinkRejectReason::Cancelled)
        );
        assert!(SetupFile::load(&path).unwrap().enabled().is_empty());
    }

    #[test]
    fn a_link_worker_that_starts_after_a_switch_off_cannot_commit() {
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let (fresh, bytes) = sync_payload();
        let earlier = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (earlier.stage)(&fresh, &bytes).unwrap();
        (earlier.commit)(&fresh, &bytes).unwrap();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (staged, outcome) = std::sync::mpsc::channel();
        let (commands, _outgoing) = tokio::sync::mpsc::unbounded_channel();
        let setup_file = Arc::clone(&controller.setup_file);
        let worker_path = path.clone();
        controller
            .launch(
                "connecting",
                "fixture",
                None,
                WorkerKind::Link(commands),
                fixture_peer(),
                move |state, generation, _, epoch, _, _| {
                    // The worker builds its persistence only after the switch-off.
                    released.recv().expect("released");
                    let persist = link_persist(
                        state,
                        fixture_key(),
                        generation,
                        epoch,
                        worker_path,
                        setup_file,
                    );
                    let _ = staged.send((persist.stage)(&fresh, &bytes));
                    async { Ok(()) }
                },
            )
            .unwrap();
        controller
            .set_enabled(&path, fixture_peer(), false, None)
            .unwrap();
        release.send(()).unwrap();
        assert_eq!(
            outcome
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the worker staged"),
            Err(LinkRejectReason::Cancelled)
        );
        assert!(SetupFile::load(&path).unwrap().enabled().is_empty());
        join_finished_worker(&controller);
    }

    #[test]
    fn an_older_proposal_is_refused_at_stage() {
        let controller = SharingController::default();
        let (_directory, path) = sync_test_path();
        let saved = crate::sharing_preferences::tests::preferences();
        let fresh = crate::sharing_preferences::tests::inspection(&saved);
        let local = "A".repeat(64);
        // This computer holds a newer record, with other content, than the one about to arrive.
        let older = group(&saved);
        let newer = older
            .with_control(control(true, false))
            .and_then(|record| record.restamped(5, &"B".repeat(64)))
            .unwrap();
        let mut file = SetupFile::default();
        file.set_interface_id(&saved.interface_id);
        file.adopt(&local, newer.clone()).unwrap();
        file.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        let bytes = |record: &GroupRecord| shared_group_bytes(&fresh, record, false).unwrap();
        assert_eq!(
            (persist.stage)(&fresh, &bytes(&older)),
            Err(LinkRejectReason::Busy)
        );
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Refusing it here says this computer's layout is the newer one.
        let mut refused = State::default();
        reject_sync(&mut refused, &fixture_key(), LinkRejectReason::Busy, false);
        assert_eq!(refused.view.message, NEWER_HERE);
        // The same content under an older stamp only confirms what is held, which stays.
        let same = newer.restamped(1, &local).unwrap();
        (persist.stage)(&fresh, &bytes(&same)).unwrap();
        (persist.commit)(&fresh, &bytes(&same)).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap().active_group(), Some(&newer));
        // A newer record is taken.
        let newest = older.restamped(6, &local).unwrap();
        (persist.stage)(&fresh, &bytes(&newest)).unwrap();
        (persist.commit)(&fresh, &bytes(&newest)).unwrap();
        assert_eq!(
            SetupFile::load(&path).unwrap().active_group(),
            Some(&newest)
        );
    }

    #[test]
    fn concurrent_commits_leave_the_newest_on_disk() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let saved = crate::sharing_preferences::tests::preferences();
        let fresh = crate::sharing_preferences::tests::inspection(&saved);
        let stamped = |control: ControlMap, revision| {
            group(&saved)
                .with_control(control)
                .and_then(|record| record.restamped(revision, &"B".repeat(64)))
                .unwrap()
        };
        let (newer, newest) = (
            stamped(control(true, false), 3),
            stamped(control(false, true), 4),
        );
        for order in [[&newest, &newer], [&newer, &newest]] {
            let (directory, path) = sync_test_path();
            crate::sharing_preferences::tests::file_with(saved.clone())
                .save(&path)
                .unwrap();
            let links: Vec<(SharingController, LinkFixture)> = order
                .iter()
                .map(|_| {
                    let controller = controller_with(fake_link, Duration::from_secs(600));
                    controller.adopt_saved(&SetupFile::load(&path).unwrap());
                    let fixture = open_fake_link(&controller, path.clone(), fixture_peer());
                    fixture
                        .events
                        .send(LinkEvent::Connected {
                            inspection: fresh.clone(),
                        })
                        .unwrap();
                    wait_for(&controller, |view| view.phase == "connected");
                    (controller, fixture)
                })
                .collect();
            // Both links stage against the same file before either commits.
            for ((_, fixture), record) in links.iter().zip(order) {
                let bytes = shared_group_bytes(&fresh, record, false).unwrap();
                (fixture.persist.stage)(&fresh, &bytes).unwrap();
            }
            for ((controller, fixture), record) in links.iter().zip(order) {
                let bytes = shared_group_bytes(&fresh, record, false).unwrap();
                (fixture.persist.commit)(&fresh, &bytes).unwrap();
                fixture
                    .events
                    .send(LinkEvent::SyncCompleted {
                        inspection: fresh.clone(),
                        bytes,
                        sending: false,
                    })
                    .unwrap();
                wait_for(controller, |view| view.sync.state == "applied");
            }
            // Whichever commit came last, the newest record stays on disk, and the last computer
            // to commit shows the switches the file holds rather than the ones it was sent.
            assert_eq!(
                SetupFile::load(&path).unwrap().active_group(),
                Some(&newest)
            );
            let (last, _) = links.last().unwrap();
            assert_eq!(last.status().control, switches(false, true, false));
            for (controller, _) in &links {
                controller.stop_with(NOT_CONNECTED);
                join_finished_worker(controller);
            }
            drop(directory);
        }
    }

    #[test]
    fn summaries_are_sent_at_link_start_and_on_change() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let saved = crate::sharing_preferences::tests::preferences();
        let fresh = crate::sharing_preferences::tests::inspection(&saved);
        crate::sharing_preferences::tests::file_with(saved.clone())
            .save(&path)
            .unwrap();
        let controller = controller_with(fake_link, Duration::from_secs(600));
        controller.adopt_saved(&SetupFile::load(&path).unwrap());
        let told = |fixture: &LinkFixture| {
            let bytes = fixture
                .summaries
                .recv_timeout(Duration::from_secs(1))
                .expect("the link is told a summary");
            RecordSummary::parse(&bytes).unwrap()
        };
        // The link starts with this computer's group and its record's stamp, never a layout.
        let fixture = open_fake_link(&controller, path.clone(), fixture_peer());
        let first = told(&fixture);
        assert_eq!(first.members(), ["a".repeat(64), "b".repeat(64)]);
        assert_eq!(first.stamp(), Some(group(&saved).stamp()));
        // Connecting, and each supervisor pass mirroring the same file, repeat nothing.
        fixture
            .events
            .send(LinkEvent::Connected {
                inspection: fresh.clone(),
            })
            .unwrap();
        wait_for(&controller, |view| view.phase == "connected");
        controller.adopt_saved(&SetupFile::load(&path).unwrap());
        assert!(
            fixture
                .summaries
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        // A change to the held record is told at once.
        controller
            .update_setup_file(&path, |file| assert!(file.touch_active()))
            .unwrap();
        let touched = told(&fixture);
        assert_eq!(touched.stamp().map(|stamp| stamp.revision()), Some(3));

        // The other computer's summary raises the clock; one that does not parse, or that would
        // run the clock to its end, is dropped.
        let theirs = group(&saved).restamped(9, &"B".repeat(64)).unwrap();
        let runaway = group(&saved).restamped(u64::MAX, &"B".repeat(64)).unwrap();
        let summary_of = |record: &GroupRecord| {
            RecordSummary::new(&[&"B".repeat(64), &"A".repeat(64)], Some(record))
                .and_then(|summary| summary.to_bytes())
                .unwrap()
        };
        for bytes in [
            b"not a summary".to_vec(),
            summary_of(&runaway),
            summary_of(&theirs),
        ] {
            fixture
                .events
                .send(LinkEvent::PeerSummary { bytes })
                .unwrap();
        }
        for _ in 0..400 {
            if controller.peer_stamp_for(&fresh).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            controller.peer_stamp_for(&fresh),
            Some(Some(theirs.stamp()))
        );
        // So the next change made here is stamped past it.
        controller
            .update_setup_file(&path, |file| assert!(file.touch_active()))
            .unwrap();
        assert_eq!(
            told(&fixture).stamp().map(|stamp| stamp.revision()),
            Some(10)
        );
        // A link opened again starts with the summary afresh.
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        assert!(controller.peer_stamp_for(&fresh).is_none());
        let again = open_fake_link(&controller, path.clone(), fixture_peer());
        assert_eq!(told(&again).stamp().map(|stamp| stamp.revision()), Some(10));
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        drop(directory);
    }

    #[test]
    fn a_network_change_no_longer_misfits_the_record() {
        let saved = crate::sharing_preferences::tests::preferences();
        let mut file = crate::sharing_preferences::tests::file_with(saved.clone());
        // The user picks another network; the record made on the old one stays as it is.
        file.set_interface_id("en1:7:192.168.1.5");
        let plan = SessionPlan::of(&file).expect("the active computer has a record");
        assert_eq!(plan.interface_id, "en1:7:192.168.1.5");
        let mut there = crate::sharing_preferences::tests::inspection(&saved);
        there.interface_id = "en1:7:192.168.1.5".into();
        // A session on the new network finds the same displays, so it runs instead of opening a
        // link, where the pairwise record would have misfit.
        assert!(plan.fits_local(&there.local_displays));
        assert!(plan.topology(&there).is_some());
        assert!(file.active_group().unwrap().fits_link(&there));
        let card = SavedSetupView::from_group(
            file.active_group(),
            file.local(),
            &"b".repeat(64),
            Some(&there),
            "1",
        );
        assert!(serde_json::to_value(card).unwrap()["layout"].is_object());
        // Displays that moved still misfit, whatever the network.
        let mut moved = saved.clone();
        moved.move_local_display_for_test("1", [0.0, 240.0]);
        let moved = crate::sharing_preferences::tests::inspection(&moved);
        assert!(!plan.fits_local(&moved.local_displays));
        assert!(plan.topology(&moved).is_none());
        // A session with another computer than the chosen one never runs under this record.
        let other = crate::sharing_preferences::tests::inspection(
            &crate::sharing_preferences::tests::preferences_for_peer('C'),
        );
        assert!(plan.topology(&other).is_none());
    }

    #[test]
    fn the_setup_revision_moves_on_with_each_write_and_commit_and_never_with_a_poll() {
        let controller = SharingController::default();
        let (directory, path) = sync_test_path();
        let revision = || controller.status().setup_revision;
        let start = revision();
        // A status poll, and the supervisor mirroring the file into the view every pass, write
        // nothing.
        controller.adopt_saved(&SetupFile::load(&path).unwrap());
        assert_eq!(revision(), start);
        controller
            .update_setup_file(&path, |file| file.set_interface_id("en0:4:192.168.1.4"))
            .unwrap();
        let written = revision();
        assert_ne!(written, start);
        assert_eq!(revision(), written);

        let (fresh, bytes) = sync_payload();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&fresh, &bytes).unwrap();
        assert_eq!(revision(), written);
        (persist.commit)(&fresh, &bytes).unwrap();
        let committed = revision();
        assert_ne!(committed, written);
        // By then the layout is remembered as well, so a window reading again lists it.
        let library = ArrangementLibrary::load(&path.with_file_name(ARRANGEMENTS_FILE)).unwrap();
        assert_eq!(library_views(&library, &fresh).len(), 1);
        // A commit with nothing staged writes nothing, so nothing moves.
        assert_eq!(
            (persist.commit)(&fresh, &bytes),
            Err(LinkRejectReason::SaveFailed)
        );
        assert_eq!(revision(), committed);
        drop(directory);
    }

    #[test]
    fn a_setup_from_a_newer_version_is_never_saved_over() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = SharingController::default();
        let (directory, path) = sync_test_path();
        let newer = br#"{"version":5,"groups":{"future":true}}"#;
        std::fs::write(&path, newer).unwrap();
        assert_eq!(
            controller
                .update_setup_file(&path, |file| file.set_interface_id("en0:4:192.168.1.4"))
                .err()
                .as_deref(),
            Some(SETUP_FROM_NEWER)
        );
        let saved = crate::sharing_preferences::tests::preferences();
        let inspected = crate::sharing_preferences::tests::inspection(&saved);
        connected_revision(&controller, 4);
        live_fixture(&mut lock(&controller.state)).inspection = Some(inspected);
        assert_eq!(
            controller
                .save_setup(&path, "4", saved.layout.clone())
                .err()
                .as_deref(),
            Some(SETUP_FROM_NEWER)
        );
        // A layout the other computer sends is refused before either computer commits it.
        let (fresh, bytes) = sync_payload();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        assert_eq!(
            (persist.stage)(&fresh, &bytes),
            Err(LinkRejectReason::SaveFailed)
        );
        assert_eq!(std::fs::read(&path).unwrap(), newer);
        drop(directory);
    }

    #[test]
    fn displays_a_link_reports_move_the_setup_revision_on() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, inspection) = connected_link(Duration::from_secs(600));
        let connected = controller.status().setup_revision;
        assert_ne!(connected, SharingView::default().setup_revision);
        let mut moved = crate::sharing_preferences::tests::preferences();
        moved.move_local_display_for_test("1", [0.0, 240.0]);
        let changed = crate::sharing_preferences::tests::inspection(&moved);
        assert!(!changed.matches(&inspection));
        fixture
            .events
            .send(LinkEvent::TopologyChanged {
                inspection: changed,
            })
            .unwrap();
        wait_for(&controller, |view| view.setup_revision != connected);
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
    }

    #[test]
    fn a_relabel_the_link_reports_shows_the_new_name_and_leaves_the_arrangement_alone() {
        use session_setup::{DisplayDescription, DisplayTopology};
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, inspection) = connected_link(Duration::from_secs(600));
        let before = controller.status();
        let connected = before.setup_revision;
        let settled_at = fixture_entry(&mut lock(&controller.state)).displays_changed_at;
        let mut relabeled = inspection.clone();
        relabeled.local_displays = DisplayTopology::new(
            inspection
                .local_displays
                .displays()
                .iter()
                .map(|display| DisplayDescription {
                    name: format!("{} (named)", display.name),
                    ..display.clone()
                })
                .collect(),
        )
        .unwrap();
        fixture
            .events
            .send(LinkEvent::TopologyChanged {
                inspection: relabeled,
            })
            .unwrap();
        let view = wait_for(&controller, |view| view.setup_revision != connected);
        assert!(
            view.local_displays
                .iter()
                .all(|display| display.name.ends_with(" (named)"))
        );
        assert_eq!(view.message, before.message);
        assert_eq!(view.revision, before.revision);
        assert_eq!(view.sync.state, before.sync.state);
        assert_eq!(
            fixture_entry(&mut lock(&controller.state)).displays_changed_at,
            settled_at
        );
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
    }

    #[test]
    fn stop_between_staging_and_commit_leaves_the_applied_setup_untouched() {
        let (directory, path) = sync_test_path();
        // C's setup, with C switched off, so B's pair may be staged.
        let mut file = crate::sharing_preferences::tests::file_with(
            crate::sharing_preferences::tests::preferences_for_peer('C'),
        );
        file.set_enabled(
            &CertificateFingerprint::parse_full(&"C".repeat(64)).unwrap(),
            false,
        )
        .unwrap();
        file.save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let controller = SharingController::default();
        let (fresh, bytes) = sync_payload();
        let persist = claimed_link_persist(
            Arc::clone(&controller.state),
            fixture_key(),
            0,
            path.clone(),
            Arc::clone(&controller.setup_file),
        );
        (persist.stage)(&fresh, &bytes).unwrap();
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_some());

        begin_close(&mut lock(&controller.state), None, "fixture stop", false);
        assert!(fixture_entry(&mut lock(&controller.state)).staged.is_none());
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
        let layout = saved.layout.clone();
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
            .apply_setup(&controller.status().revision, saved.layout.clone())
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
        assert!(!controller.holds_link(fixture_peer()));
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
        assert!(!controller.holds_link(fixture_peer()));

        fixture
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::SaveFailed,
                sending: false,
            })
            .unwrap();
        let rejected = wait_for(&controller, |view| view.sync.state == "rejected");
        assert_eq!(rejected.sync.message, ONLY_THIS_COMPUTER_SAVED);
        assert!(controller.holds_link(fixture_peer()));
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
        assert!(
            lock(&controller.state)
                .peers
                .values()
                .all(|peer| peer.link.is_none())
        );
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
        fixture_entry(&mut lock(&controller.state)).link_reason = Some(LinkReason::LayoutMisfit);
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        assert!(controller.holds_link(fixture_peer()));
    }

    #[test]
    fn choosing_a_computer_records_the_network_and_pausing_keeps_it() {
        let controller = SharingController::default();
        let (directory, path) = sync_test_path();
        controller
            .set_active(&path, Some(fixture_peer()), Some("en0"))
            .unwrap();
        assert_eq!(SetupFile::load(&path).unwrap().interface_id(), Some("en0"));
        controller.set_active(&path, None, Some("en7")).unwrap();
        let paused = SetupFile::load(&path).unwrap();
        assert!(paused.active().is_none());
        assert_eq!(paused.interface_id(), Some("en0"));
        controller
            .set_active(&path, Some(fixture_peer()), Some(""))
            .unwrap();
        assert_eq!(SetupFile::load(&path).unwrap().interface_id(), Some("en0"));
        drop(directory);
    }

    #[test]
    fn pausing_ends_the_link_with_the_paused_wording() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        let (directory, path) = sync_test_path();
        let paused = controller.set_active(&path, None, None).unwrap();
        assert_eq!(paused.phase, "stopping");
        assert!(paused.active.is_none());
        join_finished_worker(&controller);
        let view = controller.status();
        assert_eq!(view.phase, "off");
        assert_eq!(view.message, PAUSED);
        assert!(SetupFile::load(&path).unwrap().active().is_none());
        // Choosing the computer the link is already with keeps the link.
        let (controller, _fixture, _) = connected_link(Duration::from_secs(600));
        let kept = controller
            .set_active(&path, Some(fixture_peer()), None)
            .unwrap();
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
    fn active_file(path: &Path, control: ControlMap) -> GroupRecord {
        let mut record = crate::sharing_preferences::tests::preferences();
        record.layout.control = control;
        crate::sharing_preferences::tests::file_with(record.clone())
            .save(path)
            .unwrap();
        group(&record)
    }

    fn switches(local_to_peer: bool, peer_to_local: bool, syncing: bool) -> Option<ControlView> {
        Some(ControlView {
            local_to_peer,
            peer_to_local,
            syncing,
            members: Vec::new(),
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

    fn sorted_keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn the_sharing_view_keeps_every_key_the_window_reads() {
        let (directory, path) = sync_test_path();
        let controller = SharingController::default();
        let mut file = SetupFile::default();
        file.adopt(
            &"A".repeat(64),
            crate::sharing_preferences::tests::trio(3, true),
        )
        .unwrap();
        file.save(&path).unwrap();
        controller.adopt_saved(&SetupFile::load(&path).unwrap());
        let view = serde_json::to_value(controller.status()).unwrap();
        assert_eq!(
            sorted_keys(&view),
            [
                "active",
                "blockingPresses",
                "busy",
                "control",
                "diagnostics",
                "displayNotice",
                "editing",
                "enabled",
                "held",
                "lastFailure",
                "link",
                "localDisplays",
                "localPlatform",
                "members",
                "message",
                "paused",
                "peerDisplays",
                "peerFingerprint",
                "peerPlatform",
                "peers",
                "phase",
                "revision",
                "setupRevision",
                "sharingActive",
                "sync",
                "synchronizedLayout",
            ]
        );
        assert_eq!(
            view["enabled"],
            serde_json::json!(["b".repeat(64), "c".repeat(64)])
        );
        assert_eq!(
            sorted_keys(&view["peers"][0]),
            [
                "busy",
                "diagnostics",
                "displays",
                "fingerprint",
                "held",
                "lastFailure",
                "link",
                "message",
                "peerArranging",
                "phase",
                "platform",
                "sharingActive",
                "sync",
            ]
        );
        assert_eq!(
            sorted_keys(&view["members"][0]),
            [
                "displays",
                "fingerprint",
                "live",
                "local",
                "platform",
                "recordRevision"
            ]
        );
        assert_eq!(
            sorted_keys(&view["control"]),
            ["localToPeer", "members", "peerToLocal", "syncing"]
        );
        drop(directory);
    }

    #[test]
    fn sharing_set_enabled_touches_or_derives_a_record() {
        // The idle view reads the process-wide native claim that lifecycle tests hold.
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let controller = SharingController::default();
        let (a, b, c) = ("A".repeat(64), "B".repeat(64), "C".repeat(64));
        let parsed = |fingerprint: &str| CertificateFingerprint::parse_full(fingerprint).unwrap();
        let pair = group(&crate::sharing_preferences::tests::preferences());
        let three = crate::sharing_preferences::tests::trio(5, true);
        let mut file = SetupFile::default();
        file.adopt(&a, pair.clone()).unwrap();
        file.adopt(&a, three.clone()).unwrap();
        file.save(&path).unwrap();

        // Switching C off makes the pair's own record the active one, touched as this
        // computer's newest choice.
        let view = controller
            .set_enabled(&path, parsed(&c), false, None)
            .unwrap();
        assert_eq!(view.enabled, ["b".repeat(64)]);
        let saved = SetupFile::load(&path).unwrap();
        let active = saved.active_group().unwrap();
        assert!(active.same_content_as(&pair));
        assert_eq!((active.revision(), active.author()), (6, a.as_str()));
        // Switching it on again touches the three's record and records the network chosen.
        controller
            .set_enabled(&path, parsed(&c), true, Some("en1:7:192.168.1.5"))
            .unwrap();
        let saved = SetupFile::load(&path).unwrap();
        assert_eq!(saved.enabled(), ["b".repeat(64), "c".repeat(64)]);
        assert_eq!(saved.interface_id(), Some("en1:7:192.168.1.5"));
        let active = saved.active_group().unwrap();
        assert!(active.same_content_as(&three));
        assert_eq!(active.revision(), 7);

        // Without a record for the pair, switching C off derives one from the three's: C's
        // displays, crossings and switch go, and the rest stays.
        let controller = SharingController::default();
        let mut file = SetupFile::default();
        file.adopt(&a, three.clone()).unwrap();
        file.save(&path).unwrap();
        controller
            .set_enabled(&path, parsed(&c), false, None)
            .unwrap();
        let saved = SetupFile::load(&path).unwrap();
        let derived = saved
            .active_group()
            .expect("derived from the three's record");
        assert_eq!(derived.member_keys(), ["a".repeat(64), "b".repeat(64)]);
        assert_eq!(derived.layout().links.len(), 2);
        assert_eq!(derived.layout().control, both_directions(&a, &b));
        assert_eq!((derived.revision(), derived.author()), (6, a.as_str()));
        assert_eq!(saved.groups().len(), 2);

        // Switching the last computer off leaves nothing to share and makes no record.
        let off = controller
            .set_enabled(&path, parsed(&b), false, None)
            .unwrap();
        assert!(off.enabled.is_empty());
        assert_eq!((off.phase, off.message.as_str()), ("off", NOT_CONNECTED));
        let saved = SetupFile::load(&path).unwrap();
        assert!(!saved.sharing_chosen());
        assert_eq!((saved.groups().len(), saved.clock()), (2, 6));
        // This computer is never one of the others, and a refusal writes nothing.
        let before = std::fs::read(&path).unwrap();
        assert!(
            controller
                .set_enabled(&path, parsed(&a), true, None)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(directory);
    }

    #[test]
    fn set_control_peer_to_local_names_any_member() {
        let (directory, path) = sync_test_path();
        let controller = SharingController::default();
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        let mut file = SetupFile::default();
        file.adopt(
            &"A".repeat(64),
            crate::sharing_preferences::tests::trio(3, true),
        )
        .unwrap();
        file.save(&path).unwrap();
        let entries = |view: SharingView| {
            view.control
                .expect("the group has a record")
                .members
                .into_iter()
                .map(|member| (member.fingerprint, member.allowed))
                .collect::<Vec<_>>()
        };
        // "peerToLocal" is the named member's own entry, whichever member that is.
        let view = controller
            .set_control(&path, &c, "peerToLocal", false)
            .unwrap();
        let control = view.control.clone().unwrap();
        assert!(control.local_to_peer);
        assert!(control.peer_to_local, "B may still control this computer");
        assert!(control.syncing);
        assert_eq!(
            entries(view),
            [(a.clone(), true), (b.clone(), true), (c.clone(), false)]
        );
        // "localToPeer" is this computer's own entry, reached through any member.
        let view = controller
            .set_control(&path, &b.to_uppercase(), "localToPeer", false)
            .unwrap();
        assert!(!view.control.clone().unwrap().local_to_peer);
        assert_eq!(
            entries(view),
            [(a.clone(), false), (b.clone(), true), (c.clone(), false)]
        );
        // B is the last member left in control.
        assert_eq!(
            error(controller.set_control(&path, &b, "peerToLocal", false)),
            LAST_DIRECTION
        );
        // A computer outside the group has no entry to flip.
        assert!(
            controller
                .set_control(&path, &"d".repeat(64), "peerToLocal", false)
                .is_err()
        );
        // A layout made here for the group carries the pending flips.
        assert_eq!(
            group_control(
                &lock(&controller.state),
                &"A".repeat(64),
                &[b.clone(), c.clone()]
            ),
            ControlMap::from([(a, false), (b, true), (c, false)])
        );
        drop(directory);
    }

    #[test]
    fn apply_sends_one_group_proposal_per_connected_link() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (directory, path) = sync_test_path();
        let three = crate::sharing_preferences::tests::trio(3, true);
        let (a, b, c) = ("A".repeat(64), "B".repeat(64), "C".repeat(64));
        let parsed = |fingerprint: &str| CertificateFingerprint::parse_full(fingerprint).unwrap();
        let mut file = SetupFile::default();
        file.set_interface_id("en0:4:192.168.1.4");
        file.adopt(&a, three.clone()).unwrap();
        file.save(&path).unwrap();
        let controller = controller_with(fake_link, Duration::from_secs(600));
        controller.adopt_saved(&file);
        let phase_of = |controller: &SharingController, member: &str| {
            let key = fingerprint_key(member);
            controller
                .status()
                .peers
                .into_iter()
                .find(|peer| peer.fingerprint == key)
                .map(|peer| (peer.phase, peer.sync.state))
        };
        let mut links = Vec::new();
        for member in [&b, &c] {
            let link = open_fake_link(&controller, path.clone(), parsed(member));
            let inspection = crate::sharing_preferences::tests::link_of(&three, &a, member);
            link.events
                .send(LinkEvent::Connected {
                    inspection: inspection.clone(),
                })
                .unwrap();
            wait_for(&controller, |_| {
                phase_of(&controller, member).is_some_and(|(phase, _)| phase == "connected")
            });
            links.push((link, inspection));
        }
        let layout = three.layout().clone();

        // One Apply sends the whole group's record, once over each connected link.
        let revision = controller.status().revision;
        controller.apply_setup(&revision, layout.clone()).unwrap();
        let mut proposed = Vec::new();
        for (link, inspection) in &links {
            let bytes = link
                .proposals
                .recv_timeout(Duration::from_secs(1))
                .expect("each connected link gets the proposal");
            assert!(link.proposals.try_recv().is_err());
            let (record, left_out) = shared_group_for_link(inspection, &bytes).unwrap();
            assert!(!left_out);
            assert!(record.same_content_as(&three));
            assert_eq!((record.revision(), record.author()), (4, a.as_str()));
            proposed.push(record);
        }
        assert_eq!(proposed[0], proposed[1]);
        for member in [&b, &c] {
            assert_eq!(
                phase_of(&controller, member),
                Some(("connected", "sending"))
            );
        }
        assert_eq!(
            error(controller.apply_setup(&revision, layout.clone())),
            "Wait for the current layout to finish applying."
        );

        // With C gone, its entry is carried as it was last seen and only B gets the proposal.
        controller.stop_peer(parsed(&c), NOT_CONNECTED);
        for _ in 0..400 {
            if !controller.worker_alive(parsed(&c)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let (b_link, b_inspection) = &links[0];
        b_link
            .events
            .send(LinkEvent::SyncRejected {
                reason: LinkRejectReason::Busy,
                sending: true,
            })
            .unwrap();
        wait_for(&controller, |_| {
            phase_of(&controller, &b).is_some_and(|(_, sync)| sync == "rejected")
        });
        let revision = controller.status().revision;
        controller.apply_setup(&revision, layout.clone()).unwrap();
        let bytes = b_link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the connected member gets the proposal");
        let (record, _) = shared_group_for_link(b_inspection, &bytes).unwrap();
        assert!(record.same_content_as(&three));
        assert!(links[1].0.proposals.try_recv().is_err());

        // A member no connection or record ever showed cannot be arranged.
        let mut grown = file.clone();
        grown.set_enabled(&parsed(&"D".repeat(64)), true).unwrap();
        controller.adopt_saved(&grown);
        assert_eq!(
            error(controller.apply_setup(&controller.status().revision, layout)),
            NEVER_SEEN
        );
        controller.stop_with(NOT_CONNECTED);
        join_finished_worker(&controller);
        join_finished_worker(&controller);
        drop(directory);
    }

    #[test]
    fn new_layout_defaults_to_both() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let mut layout = crate::sharing_preferences::tests::preferences().layout;
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
            let inspection = fixture_entry(&mut lock(&controller.state))
                .inspection
                .clone()
                .unwrap();
            shared_group_for_link(&inspection, &bytes)
                .unwrap()
                .0
                .layout()
                .control
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
        let inspection = crate::sharing_preferences::tests::inspection(
            &crate::sharing_preferences::tests::preferences(),
        );
        let controller = controller_with(fake_link, Duration::from_secs(600));
        controller.adopt_saved(&SetupFile::load(&path).unwrap());
        // A running session: busy, no link, and it ends only when asked to.
        controller
            .launch(
                "starting",
                "fixture",
                None,
                WorkerKind::Session,
                fixture_peer(),
                |_, _, _, _, cancel, _| async move {
                    while !cancel.is_revoked() {
                        tokio::task::yield_now().await;
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert!(!controller.end_session_for_control(fixture_peer()));
        let flipped = controller
            .set_control(&path, &"b".repeat(64), "peerToLocal", false)
            .unwrap();
        assert_eq!(flipped.control, switches(true, false, true));
        assert!(controller.end_session_for_control(fixture_peer()));
        // The close is already under way; a second pass does not stop it again.
        assert!(!controller.end_session_for_control(fixture_peer()));
        join_finished_worker(&controller);
        let ended = controller.status();
        assert_eq!(ended.phase, "off");
        assert_eq!(ended.message, CHANGING_CONTROL);
        assert!(ended.last_failure.is_empty());
        assert!(!controller.within_failure_backoff(fixture_peer(), Duration::from_secs(10)));
        // The flip keeps a link up instead of a session until both computers commit it.
        assert!(controller.holds_link(fixture_peer()));
        let fixture = open_fake_link(&controller, path.clone(), fixture_peer());
        fixture
            .events
            .send(LinkEvent::Connected {
                inspection: inspection.clone(),
            })
            .unwrap();
        let connected = wait_for(&controller, |view| view.phase == "connected");
        assert_eq!(connected.message, CONNECTED_CHANGING_CONTROL);
        assert!(!controller.end_session_for_control(fixture_peer()));
        controller.propose_record(&saved).unwrap();
        assert_eq!(controller.status().message, UPDATING_CONTROL);
        let bytes = fixture
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the flip travels as one proposal");
        let (agreed, _) = shared_group_for_link(&inspection, &bytes).unwrap();
        assert_eq!(agreed.layout().control, control(true, false));
        // The flip is this computer's change, stamped past the clock it read from the file.
        assert_eq!(
            (agreed.revision(), agreed.author()),
            (saved.revision() + 1, "A".repeat(64).as_str())
        );
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
        assert!(!controller.holds_link(fixture_peer()));
        assert!(controller.link_is_off());
        join_finished_worker(&controller);
        let committed = SetupFile::load(&path).unwrap();
        let committed = committed.active_group().unwrap();
        assert_eq!(committed.layout().control, control(true, false));
        assert_eq!(
            group_record::wire_control(
                &committed.layout().control,
                &"A".repeat(64),
                &"B".repeat(64)
            ),
            Some(ControlPermissions {
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
        let key = fixture_key();
        let waiting =
            |state: &mut State| fixture_entry(state).notice_answer == Some(DisplayNotice::Waiting);
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
                fixture_entry(&mut state).pending_proposal = Some(geometry());
                reject_sync(&mut state, &key, reason, true);
                assert!(fixture_entry(&mut state).pending_proposal.is_none());
                let (_, count, _) = fixture_entry(&mut state).proposal_retry.as_ref().unwrap();
                assert_eq!(*count, attempt);
                assert_eq!(
                    waiting(&mut state),
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
        let mut invalid = with_fixture(
            State::default(),
            PeerState {
                pending_proposal: Some(geometry()),
                ..PeerState::default()
            },
        );
        reject_sync(&mut invalid, &key, LinkRejectReason::Invalid, true);
        assert!(waiting(&mut invalid));
        // Displays that changed start their own count.
        let mut changed = saved.clone();
        changed.set_local_displays_for_test(&["1", "3"]);
        let changed = crate::sharing_preferences::tests::inspection(&changed);
        let mut state = State::default();
        for _ in 1..MAX_PROPOSAL_RETRIES {
            note_passing_refusal(&mut state, &key, geometry());
        }
        note_passing_refusal(&mut state, &key, DisplayGeometry::of(&changed));
        assert!(!waiting(&mut state));
        // A user's own Apply is theirs to repeat: its refusal leaves no retry behind.
        let mut user = State::default();
        reject_sync(&mut user, &key, LinkRejectReason::Busy, true);
        assert!(fixture_entry(&mut user).proposal_retry.is_none());
    }

    #[test]
    fn a_proposal_voided_by_a_display_change_is_made_again() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let (controller, fixture, inspection) = connected_link(Duration::from_secs(600));
        settle_displays(&controller);
        assert!(controller.displays_settled());
        let next = crate::sharing_preferences::tests::preferences();
        controller.propose_layout(&group(&next), false).unwrap();
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
            let bytes = shared_group_bytes(&fresh, &group(&saved), left_out).unwrap();
            let persist = claimed_link_persist(
                Arc::clone(&controller.state),
                fixture_key(),
                0,
                path.clone(),
                Arc::clone(&controller.setup_file),
            );
            (persist.stage)(&fresh, &bytes).unwrap();
            (persist.commit)(&fresh, &bytes).unwrap();
            // Both computers still hold the stopgap, so sharing continues on it.
            assert!(SetupFile::load(&path).unwrap().active_group().is_some());
            let library =
                ArrangementLibrary::load(&path.with_file_name(ARRANGEMENTS_FILE)).unwrap();
            assert_eq!(
                library
                    .automatic_fit(&group(&saved), &KnownDisplays::of_link(&fresh))
                    .is_some(),
                !left_out
            );
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
        assert!(!controller.within_failure_backoff(fixture_peer(), Duration::from_secs(10)));
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
        assert!(controller.within_failure_backoff(fixture_peer(), Duration::from_secs(10)));
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
        let (_, floor) = fixture_entry(&mut lock(&controller.state))
            .worker_failed_at
            .unwrap();
        assert_eq!(floor, PEER_IDENTITY_BACKOFF);
        // The ordinary 10 s backoff is long spent before a minute is.
        assert!(controller.within_failure_backoff(fixture_peer(), Duration::from_secs(10)));
        fixture_entry(&mut lock(&controller.state)).worker_failed_at =
            Some((Instant::now() - Duration::from_secs(30), floor));
        assert!(controller.within_failure_backoff(fixture_peer(), Duration::from_secs(10)));
        // A user's choice ends the wait at once.
        controller
            .set_active(&path, Some(fixture_peer()), None)
            .unwrap();
        assert!(!controller.within_failure_backoff(fixture_peer(), Duration::from_secs(10)));
        drop(directory);
    }

    /// The fixture computer answers with a changed identity; any other computer's network fails.
    fn identity_changed_for_the_fixture_link(run: LinkRun) -> LinkFuture {
        let failure = if run.peer == fixture_peer() {
            SetupFailure::PeerIdentityChanged
        } else {
            SetupFailure::NetworkRoute
        };
        Box::pin(async move {
            drop(run);
            Err(failure)
        })
    }

    #[test]
    fn one_computers_failure_never_moves_another_computers_backoff() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = controller_with(
            identity_changed_for_the_fixture_link,
            Duration::from_secs(600),
        );
        let (directory, path) = sync_test_path();
        let first = fixture_peer();
        let second = CertificateFingerprint::parse_full(&"C".repeat(64)).unwrap();
        let backoff = Duration::from_secs(10);
        controller
            .connect_link(path.clone(), "en0:4:192.168.1.4".into(), first)
            .expect("the link must start");
        join_finished_worker(&controller);
        assert!(controller.within_failure_backoff(first, backoff));
        assert!(!controller.within_failure_backoff(second, backoff));
        // The second computer's own failure starts its own wait, with its own floor.
        controller
            .connect_link(path, "en0:4:192.168.1.4".into(), second)
            .expect("the link must start");
        join_finished_worker(&controller);
        assert!(controller.within_failure_backoff(second, backoff));
        assert!(!controller.within_failure_backoff(second, Duration::ZERO));
        // The first computer still waits out the minute its changed identity asked for.
        assert!(controller.within_failure_backoff(first, Duration::ZERO));
        // A reason to hold a link belongs to its computer as well.
        controller.note_local_misfit(second);
        assert!(controller.holds_link(second));
        assert!(!controller.holds_link(first));
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
