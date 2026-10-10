//! Explicit pairing only. The controller has no capture, injection, or input protocol access.

use std::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use monhop_core::{Platform, RevocationSignal};
#[cfg(target_os = "macos")]
use monhop_transport::guarded_endpoint::GuardedEndpoint;
use monhop_transport::{
    crypto::{CertificateFingerprint, DeviceIdentity},
    guarded_endpoint::{
        EndpointRevoker, ListenSelection, NetworkSelection, PairingEndpoint, RouteCheckFailure,
    },
    identity_store::{ProtectedPeerStore, create_identity, load_identity},
    native_storage::{NativeIdentityStore, NativePeerStore},
    pairing::{
        ConfirmedPeerRecord, MAX_PEER_RECORD_BYTES, PAIRING_PORT, PairBadge, PairingOffer,
        pair_badge,
    },
    pairing_code::{PairingCode, ShownCode},
    pairing_exchange::{PairingFailure, PairingRole, confirm_pairing},
    policy::PolicyError,
    session_setup::names_adapter,
};
use serde::{Serialize, Serializer};

/// How long a shown code works.
const PAIRING_WINDOW: Duration = Duration::from_secs(120);
/// How long the entering computer keeps dialing the computer showing the code.
const DIAL_WINDOW: Duration = Duration::from_secs(30);
const DIAL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(4);
const DIAL_RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// How long Show a code waits for the listener before answering with the code still pending.
const SHOW_READY_WAIT: Duration = Duration::from_secs(5);
const STOPPED: u8 = 1;
const TIMED_OUT: u8 = 2;

const READY: &str = "Show a code on this computer, or enter the code another computer shows.";
const PREPARING: &str = "Preparing a code.";
const SHOWING: &str = "Type this code on the other computer. It works once, for 2 minutes.";
const CONNECTING: &str = "Connecting to the other computer.";
const VERIFYING: &str = "Checking the code with the other computer.";
const SAVING: &str = "Code confirmed. Saving the other computer in OS-protected storage.";
const PAIRED: &str = "Paired. The other computer shows the same picture.";
const STOPPING: &str = "Stopping. Finish any open system prompt before another pairing action.";
const PAIRING_STOPPED: &str = "Pairing stopped. Sharing resumes.";
const CODE_EXPIRED: &str = "This code expired. Show a new code.";
const ENTERING_TIMED_OUT: &str = "Pairing timed out. Ask for a new code on the other computer.";
const WRONG_CODE_SHOWN: &str = "Someone entered a wrong code. This code no longer works.";
const WRONG_CODE_ENTERED: &str =
    "That code didn't match. Ask for a new code on the other computer.";
const UNREACHABLE: &str = "Could not reach the other computer. Check that it still shows the code and that both are on the same network.";
const LISTENER_STOPPED: &str = "This code stopped working. Show a new code.";
const NETWORK_CHANGED: &str =
    "The network changed. Pairing stopped. Choose the physical network and try again.";
const OPEN_FIRST: &str = "Open pairing on the selected network first.";
const STILL_FINISHING: &str =
    "Pairing is still finishing. Complete any system prompt, or cancel and wait.";
const WORKER_FAILED: &str =
    "The pairing worker stopped unexpectedly. Reopen pairing before continuing.";

#[cfg(windows)]
fn local_platform() -> Platform {
    Platform::Windows
}
#[cfg(target_os = "macos")]
fn local_platform() -> Platform {
    Platform::MacOs
}

fn platform_code(platform: Platform) -> &'static str {
    match platform {
        Platform::Windows => "windows",
        Platform::MacOs => "macos",
    }
}

fn role_code(role: PairingRole) -> &'static str {
    match role {
        PairingRole::Showing => "showing",
        PairingRole::Entering => "entering",
    }
}

/// The match badge as the window draws it: one of eight colors and three distinct symbols.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct BadgeView {
    color: u8,
    symbols: [u8; 3],
}

impl BadgeView {
    pub fn between(a: CertificateFingerprint, b: CertificateFingerprint) -> Self {
        pair_badge(a, b).into()
    }
}

impl From<PairBadge> for BadgeView {
    fn from(badge: PairBadge) -> Self {
        Self {
            color: badge.color,
            symbols: badge.symbols,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingView {
    phase: &'static str,
    role: Option<&'static str>,
    #[serde(serialize_with = "shown_code")]
    code: Option<ShownCode>,
    code_expires_at_ms: Option<u64>,
    message: String,
    busy: bool,
    peer_fingerprint: Option<String>,
    peer_platform: Option<&'static str>,
    peer_address: Option<String>,
    badge: Option<BadgeView>,
    storage_outcome: &'static str,
    local_platform: &'static str,
    local_fingerprint: Option<String>,
}

impl Default for PairingView {
    fn default() -> Self {
        Self {
            phase: "closed",
            role: None,
            code: None,
            code_expires_at_ms: None,
            message: "Open pairing to read this computer's saved identity. Sharing stays off."
                .into(),
            busy: false,
            peer_fingerprint: None,
            peer_platform: None,
            peer_address: None,
            badge: None,
            storage_outcome: "unchanged",
            local_platform: platform_code(local_platform()),
            local_fingerprint: None,
        }
    }
}

impl PairingView {
    /// Forgets everything about the last attempt; the code string is wiped as it drops.
    fn clear_attempt(&mut self) {
        self.clear_code();
        self.role = None;
        self.peer_fingerprint = None;
        self.peer_platform = None;
        self.peer_address = None;
        self.badge = None;
        self.storage_outcome = "unchanged";
    }

    fn clear_code(&mut self) {
        self.code = None;
        self.code_expires_at_ms = None;
    }
}

fn shown_code<S: Serializer>(code: &Option<ShownCode>, serializer: S) -> Result<S::Ok, S::Error> {
    code.as_ref()
        .map(|code| code.as_str())
        .serialize(serializer)
}

/// One pairing attempt: this computer showing a code, or entering one.
#[derive(Clone)]
struct Attempt {
    id: u64,
    role: PairingRole,
    interface_id: String,
    local: PairingOffer,
}

/// What the attempt's worker does: show a fresh code, or dial the computer showing `code`.
enum Job {
    Show,
    Enter {
        code: PairingCode,
        showing: Ipv4Addr,
    },
}

/// A typed code that passed its check and points at a host of the selected network.
pub struct EnteredCode {
    code: PairingCode,
    showing: Ipv4Addr,
}

/// One pairing that reached protected storage, for the app to list and make active.
#[derive(Clone, PartialEq, Eq)]
pub struct CompletedPairing {
    pub fingerprint: CertificateFingerprint,
    pub address: SocketAddrV4,
    pub platform: Option<Platform>,
    pub interface_id: String,
}

/// A computer protected storage trusts, as its record describes it.
#[derive(Clone, PartialEq, Eq)]
pub struct PairedPeer {
    pub fingerprint: CertificateFingerprint,
    pub address: SocketAddrV4,
    pub platform: Option<Platform>,
}

#[derive(Default)]
struct PairingState {
    view: PairingView,
    generation: u64,
    interface_id: Option<String>,
    local: Option<PairingOffer>,
    /// Every computer paired with this identity, as read when pairing opened.
    saved: Vec<ConfirmedPeerRecord>,
    attempt: Option<Attempt>,
    control: Option<Arc<PairingControl>>,
    /// Handed over once through `take_completed`, even when the view moved on.
    completed: Vec<CompletedPairing>,
}

#[derive(Default)]
pub struct PairingController {
    shutting_down: AtomicBool,
    operation: Mutex<()>,
    state: Arc<Mutex<PairingState>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

struct PairingControl {
    role: PairingRole,
    reason: AtomicU8,
    cancel: RevocationSignal,
    revoker: Mutex<Option<EndpointRevoker>>,
}

impl PairingControl {
    fn new(role: PairingRole) -> Self {
        Self {
            role,
            reason: AtomicU8::new(0),
            cancel: RevocationSignal::default(),
            revoker: Mutex::new(None),
        }
    }

    fn stop(&self, reason: u8) {
        self.cancel.revoke();
        let _ = self
            .reason
            .compare_exchange(0, reason, Ordering::AcqRel, Ordering::Acquire);
        if let Some(revoker) = lock(&self.revoker).as_ref() {
            revoker.revoke();
        }
    }

    fn attach(&self, revoker: EndpointRevoker) {
        let mut slot = lock(&self.revoker);
        if self.reason.load(Ordering::Acquire) != 0 {
            revoker.revoke();
        }
        *slot = Some(revoker);
    }

    /// Why the user or the window stopped this attempt, if either did.
    fn stopped(&self) -> Option<String> {
        match (self.reason.load(Ordering::Acquire), self.role) {
            (STOPPED, _) => Some(PAIRING_STOPPED.into()),
            (TIMED_OUT, PairingRole::Showing) => Some(CODE_EXPIRED.into()),
            (TIMED_OUT, PairingRole::Entering) => Some(ENTERING_TIMED_OUT.into()),
            _ => None,
        }
    }

    fn check(&self) -> Result<(), String> {
        if let Some(reason) = self.stopped() {
            return Err(reason);
        }
        if lock(&self.revoker)
            .as_ref()
            .is_some_and(EndpointRevoker::is_revoked)
        {
            return Err(NETWORK_CHANGED.into());
        }
        Ok(())
    }

    /// The control's own reason when it ended the attempt, else `fallback`.
    fn or(&self, fallback: impl Into<String>) -> String {
        self.check().err().unwrap_or_else(|| fallback.into())
    }
}

impl PairingController {
    pub fn request_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.cancel();
    }

    pub fn shutdown_ready(&self) -> bool {
        lock(&self.worker)
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }

    fn require_open_app(&self) -> Result<(), String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("MonHop is closing. No new pairing action can start.".into());
        }
        Ok(())
    }

    pub fn status(&self) -> PairingView {
        lock(&self.state).view.clone()
    }

    /// This computer's identity as pairing last read it; none before pairing opens.
    pub fn local_fingerprint(&self) -> Option<CertificateFingerprint> {
        lock(&self.state)
            .local
            .as_ref()
            .map(PairingOffer::fingerprint)
    }

    /// The trust records read when pairing last opened, plus any exchange finished since; empty
    /// until then, because protected storage is only read after a user action.
    pub fn paired_peers(&self) -> Vec<PairedPeer> {
        lock(&self.state)
            .saved
            .iter()
            .map(|saved| PairedPeer {
                fingerprint: saved.peer().fingerprint(),
                address: saved.peer().endpoint(),
                platform: saved.peer().platform(),
            })
            .collect()
    }

    /// Pairings finished since the last call, oldest first; each is reported once.
    pub fn take_completed(&self) -> Vec<CompletedPairing> {
        std::mem::take(&mut lock(&self.state).completed)
    }

    /// Hands pairings back that could not be recorded, ahead of any newer ones.
    pub fn requeue_completed(&self, mut pairings: Vec<CompletedPairing>) {
        let mut state = lock(&self.state);
        pairings.append(&mut state.completed);
        state.completed = pairings;
    }

    /// True while a pairing attempt is in progress: from showing or entering a code until it is
    /// paired or ends. Pairing and sharing use one port, so no connection may run meanwhile.
    pub fn occupies_port(&self) -> bool {
        let state = lock(&self.state);
        state.attempt.is_some() && state.view.phase != "paired"
    }

    fn require_idle(&self) -> Result<(), String> {
        self.require_open_app()?;
        let mut worker = lock(&self.worker);
        if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Err(STILL_FINISHING.into());
        }
        if let Some(worker) = worker.take() {
            worker.join().map_err(|_| WORKER_FAILED.to_owned())?;
        }
        Ok(())
    }

    pub fn open(&self, interface_id: String, create: bool) -> Result<PairingView, String> {
        let _operation = lock(&self.operation);
        self.require_idle()?;
        if interface_id.is_empty() || interface_id.len() > 512 {
            return Err("Choose a physical network first.".into());
        }
        let generation = {
            let mut state = lock(&self.state);
            self.require_open_app()?;
            state.generation += 1;
            state.attempt = None;
            state.saved.clear();
            state.local = None;
            state.control = None;
            state.interface_id = Some(interface_id.clone());
            state.view = PairingView::default();
            state.generation
        };
        let result = (|| {
            let adapter = selected_adapter(&interface_id)?;
            self.require_open_app()?;
            let identity = if create {
                Some(create_identity(&NativeIdentityStore).map_err(display_error)?)
            } else {
                load_identity(&NativeIdentityStore).map_err(display_error)?
            };
            let Some(identity) = identity else {
                return Ok(None);
            };
            self.require_open_app()?;
            let local = PairingOffer::new(adapter.local(), identity.certificate_der())
                .map_err(display_error)?
                .with_platform(local_platform());
            let saved = read_saved(&NativePeerStore, identity.fingerprint())?;
            Ok::<_, String>(Some((local, saved)))
        })();
        let mut state = lock(&self.state);
        if state.generation != generation || self.shutting_down.load(Ordering::Acquire) {
            return Ok(state.view.clone());
        }
        match result {
            Ok(None) => {
                state.view.phase = "identity-missing";
                state.view.message = "Create an identity for this computer. Its private key stays in OS-protected storage.".into();
            }
            Ok(Some((local, saved))) => {
                state.view.local_fingerprint = Some(local.fingerprint().full_hex());
                state.local = Some(local);
                state.saved = saved;
                state.view.phase = "ready";
                state.view.message = READY.into();
            }
            Err(error) => {
                state.view.phase = "error";
                state.view.message = error;
            }
        }
        Ok(state.view.clone())
    }

    /// Shows a fresh code and listens for the computer that types it; a code already showing is
    /// spent first. Waits briefly so the returned view usually carries the code.
    pub fn show_code(&self) -> Result<PairingView, String> {
        let _operation = lock(&self.operation);
        self.require_open_app()?;
        self.spend_shown_code()?;
        self.require_idle()?;
        let (ready, shown) = mpsc::channel();
        self.start(PairingRole::Showing, Job::Show, Some(ready))?;
        let _ = shown.recv_timeout(SHOW_READY_WAIT);
        Ok(self.status())
    }

    /// Checks a typed code without sending anything: its format and check symbol, and that it
    /// names another host of the selected network.
    pub fn check_code(&self, typed: &str) -> Result<EnteredCode, String> {
        self.require_open_app()?;
        let code = PairingCode::parse(typed).map_err(display_error)?;
        let interface_id = {
            let state = lock(&self.state);
            state.local.as_ref().ok_or(OPEN_FIRST)?;
            state.interface_id.clone().ok_or(OPEN_FIRST)?
        };
        let adapter = selected_adapter(&interface_id)?;
        let showing = code
            .showing_address(adapter.address, adapter.prefix_len)
            .map_err(display_error)?;
        Ok(EnteredCode { code, showing })
    }

    /// Dials the computer showing `entered` and confirms the code with it; a code this computer
    /// shows is spent first.
    pub fn enter_code(&self, entered: EnteredCode) -> Result<PairingView, String> {
        let _operation = lock(&self.operation);
        self.require_open_app()?;
        self.spend_shown_code()?;
        self.require_idle()?;
        let EnteredCode { code, showing } = entered;
        self.start(PairingRole::Entering, Job::Enter { code, showing }, None)?;
        Ok(self.status())
    }

    /// A shown code is burned before another attempt starts, and its listener closes.
    fn spend_shown_code(&self) -> Result<(), String> {
        if lock(&self.state).view.phase != "showing" {
            return Ok(());
        }
        self.cancel();
        let worker = lock(&self.worker).take();
        if let Some(worker) = worker {
            worker.join().map_err(|_| WORKER_FAILED.to_owned())?;
        }
        Ok(())
    }

    fn start(
        &self,
        role: PairingRole,
        job: Job,
        ready: Option<mpsc::Sender<()>>,
    ) -> Result<(), String> {
        let mut state = lock(&self.state);
        self.require_open_app()?;
        let (attempt, control) = begin_attempt(&mut state, role)?;
        let shared = self.state.clone();
        let spawn = std::thread::Builder::new()
            .name("monhop-pairing".into())
            .spawn(move || {
                // Success means both records are saved, so only a failure takes the stop reason.
                let result = run_worker(&attempt, job, &control, &shared, ready)
                    .map_err(|error| control.stopped().unwrap_or(error));
                finish_attempt(&mut lock(&shared), attempt.id, result);
                if let Some(revoker) = lock(&control.revoker).take() {
                    revoker.revoke();
                }
            });
        match spawn {
            Ok(worker) => *lock(&self.worker) = Some(worker),
            Err(_) => {
                state.attempt = None;
                state.view.busy = false;
                state.view.phase = "error";
                state.view.message =
                    "The pairing worker could not start. Nothing was connected.".into();
            }
        }
        Ok(())
    }

    /// Burns any shown code and closes the listener or dial. A running worker reports
    /// `stopping` until it has let go of the network.
    pub fn cancel(&self) -> PairingView {
        let mut state = lock(&self.state);
        if let Some(control) = &state.control {
            control.stop(STOPPED);
        }
        state.generation += 1;
        state.attempt = None;
        state.view.clear_code();
        if state.view.busy {
            state.view.phase = "stopping";
            state.view.message = STOPPING.into();
        } else if state.local.is_some() {
            state.view.clear_attempt();
            state.view.phase = "ready";
            state.view.message = READY.into();
        }
        state.view.clone()
    }

    /// Removes every trust record for that computer; a running exchange is stopped first.
    /// The records are listed fresh from the store, so a stale in-memory list never decides.
    pub fn forget(&self, peer: CertificateFingerprint) -> Result<(), String> {
        self.cancel();
        let _operation = lock(&self.operation);
        self.require_open_app()?;
        self.cancel();
        let worker = { lock(&self.worker).take() };
        if let Some(worker) = worker {
            worker.join().map_err(|_| "The pairing worker stopped unexpectedly. Reload the saved device before forgetting it.".to_owned())?;
        }
        self.require_open_app()?;
        let result = (|| {
            let identity = load_identity(&NativeIdentityStore)
                .map_err(display_error)?
                .ok_or("This computer has no identity, so nothing is paired.")?;
            forget_peer(&NativePeerStore, identity.fingerprint(), peer)
        })();
        let mut state = lock(&self.state);
        state.attempt = None;
        state.view.clear_attempt();
        state.view.busy = false;
        match &result {
            Ok(()) => {
                state
                    .saved
                    .retain(|saved| saved.peer().fingerprint() != peer);
                if state.local.is_some() {
                    state.view.phase = "ready";
                    state.view.message =
                        "Computer forgotten. This computer's identity and permissions are unchanged."
                            .into();
                }
            }
            Err(error) => {
                state.view.storage_outcome = "unverified";
                state.view.phase = "error";
                state.view.message = format!(
                    "Forget was not confirmed. No replacement is allowed until you reload or successfully forget. {error}"
                );
                state.local = None;
            }
        }
        result
    }
}

impl Drop for PairingController {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Starts an attempt from a settled phase; a running one must end or be cancelled first.
fn begin_attempt(
    state: &mut PairingState,
    role: PairingRole,
) -> Result<(Attempt, Arc<PairingControl>), String> {
    let local = state.local.clone().ok_or(OPEN_FIRST)?;
    let interface_id = state.interface_id.clone().ok_or(OPEN_FIRST)?;
    if !matches!(state.view.phase, "ready" | "error" | "paired") {
        return Err(STILL_FINISHING.into());
    }
    state.generation += 1;
    let attempt = Attempt {
        id: state.generation,
        role,
        interface_id,
        local,
    };
    let control = Arc::new(PairingControl::new(role));
    state.attempt = Some(attempt.clone());
    state.control = Some(control.clone());
    state.view.clear_attempt();
    state.view.role = Some(role_code(role));
    state.view.busy = true;
    (state.view.phase, state.view.message) = match role {
        PairingRole::Showing => ("showing", PREPARING.into()),
        PairingRole::Entering => ("connecting", CONNECTING.into()),
    };
    Ok((attempt, control))
}

/// The attempt's terminal state. A failed attempt drops out so the port goes back to the
/// supervisor; its role and message stay for the window.
fn finish_attempt(state: &mut PairingState, id: u64, result: Result<(), String>) {
    if state
        .attempt
        .as_ref()
        .is_some_and(|attempt| attempt.id == id)
    {
        state.view.clear_code();
        match result {
            Ok(()) => {
                state.view.phase = "paired";
                state.view.message = PAIRED.into();
            }
            Err(error) => {
                state.attempt = None;
                state.view.badge = None;
                state.view.phase = "error";
                state.view.message = match state.view.storage_outcome {
                    "unverified" => format!(
                        "Save result not confirmed. Reload or explicitly forget the saved device. {error}"
                    ),
                    "verified" => format!(
                        "The other computer is saved here, but it did not confirm saving this one. {error}"
                    ),
                    _ => error,
                };
            }
        }
    }
    if state.view.phase == "stopping" {
        state.view.clear_code();
        if state.view.storage_outcome == "unverified" {
            state.view.phase = "error";
            state.view.message = "Pairing stopped. The save result is not confirmed. Reload pairing before continuing.".into();
        } else {
            state.view.clear_attempt();
            state.view.phase = "ready";
            state.view.message = PAIRING_STOPPED.into();
        }
    }
    state.view.busy = false;
}

/// Every record this identity holds. A record another identity wrote is a corrupt store, not a
/// missing pairing, so it fails the read instead of being skipped.
fn read_saved(
    store: &impl ProtectedPeerStore,
    local: CertificateFingerprint,
) -> Result<Vec<ConfirmedPeerRecord>, String> {
    store
        .list()
        .map_err(display_error)?
        .iter()
        .map(|stored| ConfirmedPeerRecord::decode(&stored.record, local).map_err(display_error))
        .collect()
}

/// The record for `peer` among this identity's pairings, when there is one.
fn find_saved(
    store: &impl ProtectedPeerStore,
    local: CertificateFingerprint,
    peer: CertificateFingerprint,
) -> Result<Option<ConfirmedPeerRecord>, String> {
    Ok(read_saved(store, local)?
        .into_iter()
        .find(|saved| saved.peer().fingerprint() == peer))
}

/// Deletes every record naming `peer`, including one an earlier build stored under the legacy
/// name, then confirms none is left.
fn forget_peer(
    store: &impl ProtectedPeerStore,
    local: CertificateFingerprint,
    peer: CertificateFingerprint,
) -> Result<(), String> {
    let handles: Vec<String> = store
        .list()
        .map_err(display_error)?
        .into_iter()
        .filter(|stored| {
            ConfirmedPeerRecord::decode(&stored.record, local)
                .is_ok_and(|record| record.peer().fingerprint() == peer)
        })
        .map(|stored| stored.handle)
        .collect();
    for handle in handles {
        store.delete(&handle).map_err(display_error)?;
    }
    if find_saved(store, local, peer)?.is_some() {
        return Err("The computer is still in protected storage.".into());
    }
    Ok(())
}

/// Retries `attempt` until it succeeds, each try bounded by `attempt_timeout` and
/// spaced by `retry_interval`; stops only on `control`'s own cancellation/timeout,
/// never by revoking anything itself. A failed or timed-out attempt is not an error.
async fn retry_until_cancelled<T, F, Fut>(
    control: &PairingControl,
    attempt_timeout: Duration,
    retry_interval: Duration,
    mut attempt: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ()>>,
{
    loop {
        control.check()?;
        if let Ok(Ok(value)) = tokio::time::timeout(attempt_timeout, attempt()).await {
            return Ok(value);
        }
        control.check()?;
        tokio::time::sleep(retry_interval).await;
    }
}

fn run_worker(
    attempt: &Attempt,
    job: Job,
    control: &Arc<PairingControl>,
    state: &Arc<Mutex<PairingState>>,
    ready: Option<mpsc::Sender<()>>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "The pairing runtime could not start.")?;
    let expires = SystemTime::now() + PAIRING_WINDOW;
    runtime.block_on(async {
        let deadline_control = control.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(PAIRING_WINDOW).await;
            deadline_control.stop(TIMED_OUT);
        });
        let result = run_session(attempt, job, control, state, ready, expires).await;
        timer.abort();
        result
    })
}

async fn run_session(
    attempt: &Attempt,
    job: Job,
    control: &Arc<PairingControl>,
    state: &Arc<Mutex<PairingState>>,
    ready: Option<mpsc::Sender<()>>,
    expires: SystemTime,
) -> Result<(), String> {
    control.check()?;
    let identity = load_identity(&NativeIdentityStore)
        .map_err(display_error)?
        .ok_or("The local identity is missing. Nothing was replaced.")?;
    control.check()?;
    if identity.fingerprint() != attempt.local.fingerprint() {
        return Err("This computer's identity changed. Open pairing again.".into());
    }
    let adapter = selected_adapter(&attempt.interface_id)?;
    if adapter.local() != attempt.local.endpoint() {
        return Err("The selected network address changed. Open pairing again.".into());
    }
    control.check()?;
    let (endpoint, connection, code) = match job {
        Job::Show => {
            let code = PairingCode::generate(adapter.address).map_err(display_error)?;
            let (endpoint, connection) = show(
                attempt, &adapter, &code, &identity, control, state, ready, expires,
            )
            .await?;
            (endpoint, connection, code)
        }
        Job::Enter { code, showing } => {
            let (endpoint, connection) = enter(&adapter, showing, &identity, control).await?;
            (endpoint, connection, code)
        }
    };
    update(state, attempt.id, |view| {
        view.clear_code();
        view.phase = "verifying";
        view.message = VERIFYING.into();
    });
    let confirmed = confirm_pairing(
        &connection,
        attempt.role,
        &code,
        &identity,
        attempt.local.endpoint(),
        local_platform(),
    )
    .await
    .map_err(|failure| control.or(failure_message(attempt.role, &failure)))?;
    drop(code);

    let local = identity.fingerprint();
    let offer = confirmed.peer().offer().map_err(display_error)?;
    let peer = offer.fingerprint();
    let record = ConfirmedPeerRecord::new(local, offer.clone()).map_err(display_error)?;
    if record.encode().len() > MAX_PEER_RECORD_BYTES {
        return Err("That identity is too large to store safely.".into());
    }
    update(state, attempt.id, |view| {
        view.phase = "saving";
        view.message = SAVING.into();
        view.peer_fingerprint = Some(peer.full_hex().to_ascii_lowercase());
        view.peer_platform = offer.platform().map(platform_code);
        view.peer_address = Some(offer.endpoint().to_string());
    });
    persist_confirmed(
        &NativePeerStore,
        &record,
        local,
        || control.check(),
        || {
            update(state, attempt.id, |view| {
                view.storage_outcome = "unverified";
            })
        },
    )?;
    update(state, attempt.id, |view| {
        view.storage_outcome = "verified";
    });
    {
        let mut locked = lock(state);
        locked.completed.push(CompletedPairing {
            fingerprint: peer,
            address: offer.endpoint(),
            platform: offer.platform(),
            interface_id: attempt.interface_id.clone(),
        });
        if locked
            .attempt
            .as_ref()
            .is_some_and(|current| current.id == attempt.id)
            && !locked.saved.contains(&record)
        {
            locked.saved.push(record);
        }
    }
    confirmed
        .finish_saved()
        .await
        .map_err(|failure| control.or(failure.to_string()))?;
    update(state, attempt.id, |view| {
        view.badge = Some(BadgeView::between(local, peer));
    });
    // Both records are already saved and read back; a teardown race must not report failure.
    let _ = endpoint.close_and_wait_idle().await;
    lock(&control.revoker).take();
    Ok(())
}

/// Listens with `code` shown until one connection arrives; the window then shows it no more.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site, each input distinct"
)]
async fn show(
    attempt: &Attempt,
    adapter: &SelectedAdapter,
    code: &PairingCode,
    identity: &DeviceIdentity,
    control: &PairingControl,
    state: &Mutex<PairingState>,
    ready: Option<mpsc::Sender<()>>,
    expires: SystemTime,
) -> Result<(PairingEndpoint, quinn::Connection), String> {
    #[cfg(target_os = "macos")]
    if let Some(neighbor) =
        monhop_transport::policy::other_subnet_host(adapter.address, adapter.prefix_len)
    {
        request_local_network(adapter.toward(neighbor), control);
    }
    control.check()?;
    let endpoint = PairingEndpoint::listen_after_local_action(adapter.listen(), identity)
        .map_err(network_preparation_error)?;
    control.attach(endpoint.revoker());
    control.check()?;
    let shown = code.display();
    update(state, attempt.id, |view| {
        view.code = Some(shown);
        view.code_expires_at_ms = unix_millis(expires);
        view.message = SHOWING.into();
    });
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    let connection = endpoint
        .accept()
        .await
        .map_err(|_| control.or(LISTENER_STOPPED))?;
    Ok((endpoint, connection))
}

/// Dials the computer showing the code until it answers or `DIAL_WINDOW` passes.
async fn enter(
    adapter: &SelectedAdapter,
    showing: Ipv4Addr,
    identity: &DeviceIdentity,
    control: &PairingControl,
) -> Result<(PairingEndpoint, quinn::Connection), String> {
    let selection = adapter.toward(showing);
    #[cfg(target_os = "macos")]
    request_local_network(selection.clone(), control);
    control.check()?;
    let endpoint = PairingEndpoint::dial_after_local_action(selection, identity)
        .map_err(network_preparation_error)?;
    control.attach(endpoint.revoker());
    control.check()?;
    let connection = tokio::time::timeout(
        DIAL_WINDOW,
        retry_until_cancelled(
            control,
            DIAL_ATTEMPT_TIMEOUT,
            DIAL_RETRY_INTERVAL,
            || async { endpoint.dial().await.map_err(|_| ()) },
        ),
    )
    .await
    .map_err(|_| control.or(UNREACHABLE))??;
    Ok((endpoint, connection))
}

/// Pairing is an explicit local action, so it may raise the macOS Local Network prompt; it sends
/// nothing. A failed request only means the bind that follows reports the problem.
#[cfg(target_os = "macos")]
fn request_local_network(selection: NetworkSelection, control: &PairingControl) {
    if let Err(error) =
        GuardedEndpoint::request_local_network_access_after_local_action(selection, &control.cancel)
    {
        log::info!(
            "pairing: the Local Network request did not complete ({:?})",
            error.kind()
        );
    }
}

fn failure_message(role: PairingRole, failure: &PairingFailure) -> String {
    match (failure, role) {
        (PairingFailure::NotConfirmed, PairingRole::Showing) => WRONG_CODE_SHOWN.into(),
        (PairingFailure::NotConfirmed, PairingRole::Entering) => WRONG_CODE_ENTERED.into(),
        (failure, _) => failure.to_string(),
    }
}

fn unix_millis(at: SystemTime) -> Option<u64> {
    u64::try_from(at.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// Writes one record per computer and reads it back; an existing record for the same computer
/// is kept as it is, never replaced.
fn persist_confirmed(
    store: &impl ProtectedPeerStore,
    record: &ConfirmedPeerRecord,
    local: CertificateFingerprint,
    check: impl Fn() -> Result<(), String>,
    write_attempted: impl FnOnce(),
) -> Result<(), String> {
    let peer = record.peer().fingerprint();
    check()?;
    if let Some(existing) = find_saved(store, local, peer)? {
        if &existing != record {
            return Err(
                "That computer's saved record differs from this pairing. Forget it, then pair again."
                    .into(),
            );
        }
    } else {
        check()?;
        write_attempted();
        store
            .create_new(&peer.full_hex().to_ascii_lowercase(), &record.encode())
            .map_err(display_error)?;
    }
    check()?;
    let readback = find_saved(store, local, peer)?
        .ok_or("The saved device was missing during verification.")?;
    check()?;
    if &readback != record {
        return Err("The saved device did not match. No pairing was confirmed.".into());
    }
    Ok(())
}

fn update(state: &Mutex<PairingState>, id: u64, change: impl FnOnce(&mut PairingView)) {
    let mut state = lock(state);
    if state
        .attempt
        .as_ref()
        .is_some_and(|attempt| attempt.id == id)
    {
        change(&mut state.view);
    }
}

/// The selected adapter as pairing binds it.
struct SelectedAdapter {
    stable_id: String,
    index: u32,
    address: Ipv4Addr,
    prefix_len: u8,
}

impl SelectedAdapter {
    fn local(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.address, PAIRING_PORT)
    }

    fn listen(&self) -> ListenSelection {
        ListenSelection {
            stable_id: self.stable_id.clone(),
            interface_index: self.index,
            local: self.local(),
        }
    }

    fn toward(&self, peer: Ipv4Addr) -> NetworkSelection {
        NetworkSelection {
            stable_id: self.stable_id.clone(),
            interface_index: self.index,
            local: self.local(),
            peer: SocketAddrV4::new(peer, PAIRING_PORT),
        }
    }
}

fn selected_adapter(interface_id: &str) -> Result<SelectedAdapter, String> {
    #[cfg(target_os = "macos")]
    let adapters = monhop_platform_macos::network::enumerate_adapters_with_attachment()
        .map_err(display_error)?;
    #[cfg(windows)]
    let adapters = monhop_platform_windows::network::enumerate_adapters().map_err(display_error)?;
    let mut matches = adapters
        .into_iter()
        .filter(|adapter| names_adapter(interface_id, &adapter.stable_id, adapter.address));
    let adapter = matches
        .next()
        .ok_or("That physical network is no longer available. Check status and choose again.")?;
    if matches.next().is_some()
        || !adapter.physical
        || !adapter.up
        || !(adapter.wifi || adapter.ethernet)
        || adapter.attachment.is_none()
    {
        return Err("This network is not recognized as connected physical Wi-Fi or Ethernet. Check its status first.".into());
    }
    Ok(SelectedAdapter {
        stable_id: adapter.stable_id,
        index: adapter.index,
        address: adapter.address,
        prefix_len: adapter.prefix_len,
    })
}

fn network_preparation_error(error: std::io::Error) -> String {
    if error
        .get_ref()
        .is_some_and(|cause| cause.is::<RouteCheckFailure>())
    {
        return "Cannot verify the route on your selected Wi-Fi or Ethernet. Check the local network and VPN settings, then try again. No fallback was used. This does not report Local Network permission.".into();
    }
    if let Some(policy) = error
        .get_ref()
        .and_then(|cause| cause.downcast_ref::<PolicyError>())
    {
        return match policy {
            PolicyError::WrongInterface | PolicyError::RoutedPeer => "The route to the other computer uses a different adapter or a gateway. Use a direct route on the selected Wi-Fi or Ethernet, then try again. No fallback was used.".into(),
            PolicyError::OffLinkPeer => "The other computer is outside this network. Connect both computers to the same local Wi-Fi or Ethernet, then try again.".into(),
            _ => format!("Network check stopped: {policy}. Check the selected network, then try again."),
        };
    }
    format!(
        "Could not prepare the selected network ({:?}). Check its status and try again. This result does not identify a permission or firewall problem.",
        error.kind()
    )
}

fn display_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_transport::{
        crypto::DeviceIdentity,
        identity_store::{StorageError, StoredPeer},
    };
    use std::cell::{Cell, RefCell};
    use zeroize::Zeroizing;

    #[test]
    fn network_messages_identify_route_failures_without_guessing_permissions() {
        let route = network_preparation_error(std::io::Error::other(RouteCheckFailure));
        assert!(route.starts_with("Cannot verify the route"));
        assert!(route.contains("does not report Local Network permission"));

        for policy in [PolicyError::WrongInterface, PolicyError::RoutedPeer] {
            let message = network_preparation_error(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                policy,
            ));
            assert!(message.contains("different adapter or a gateway"));
            assert!(!message.contains("permission"));
        }
        let message = network_preparation_error(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            PolicyError::OffLinkPeer,
        ));
        assert!(message.contains("outside this network"));

        let unknown = network_preparation_error(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private platform details",
        ));
        assert!(!unknown.contains("private platform details"));
        assert!(!unknown.contains("VPN"));
        assert!(unknown.contains("does not identify a permission or firewall problem"));
    }

    #[test]
    fn shutdown_rejects_all_mutating_actions_before_native_work() {
        let controller = PairingController::default();
        controller.request_shutdown();
        assert!(controller.shutdown_ready());
        assert!(controller.open("physical-network".into(), false).is_err());
        assert!(controller.open("physical-network".into(), true).is_err());
        assert!(controller.show_code().is_err());
        assert!(controller.check_code("not-a-code").is_err());
        assert!(
            controller
                .forget(fixtures().2.peer().fingerprint())
                .is_err()
        );
        assert!(lock(&controller.worker).is_none());
    }

    #[test]
    fn a_queued_show_cannot_outlive_shutdown() {
        let controller = Arc::new(PairingController::default());
        let operation = lock(&controller.operation);
        let queued = controller.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(queued.show_code().is_err()).unwrap();
        });
        controller.request_shutdown();
        drop(operation);
        assert!(receive.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
        assert!(lock(&controller.worker).is_none());
    }

    #[test]
    fn a_typo_is_refused_locally_before_any_network_or_storage_work() {
        let controller = opened_controller();
        let before = serde_json::to_value(controller.status()).unwrap();
        let code = PairingCode::generate(Ipv4Addr::new(192, 168, 1, 2))
            .unwrap()
            .display();
        let mut typo = code.to_string().into_bytes();
        typo[0] = if typo[0] == b'7' { b'8' } else { b'7' };
        for (typed, message) in [
            (
                String::from_utf8(typo).unwrap(),
                "That code has a typo. Check each character against the other computer.",
            ),
            (
                "1234-5678".to_owned(),
                "Enter the 12 letters and digits the other computer shows.",
            ),
            (
                "x".repeat(65),
                "Enter the 12 letters and digits the other computer shows.",
            ),
        ] {
            assert_eq!(
                controller.check_code(&typed).err().as_deref(),
                Some(message)
            );
        }
        assert_eq!(serde_json::to_value(controller.status()).unwrap(), before);
        assert!(lock(&controller.worker).is_none());
        assert!(!controller.occupies_port());
    }

    #[test]
    fn showing_or_entering_needs_an_opened_identity() {
        let controller = PairingController::default();
        assert_eq!(controller.show_code().err().as_deref(), Some(OPEN_FIRST));
        let code = PairingCode::generate(Ipv4Addr::new(192, 168, 1, 2))
            .unwrap()
            .display();
        assert_eq!(
            controller.check_code(&code).err().as_deref(),
            Some(OPEN_FIRST)
        );
        assert_eq!(controller.status().phase, "closed");
        assert!(lock(&controller.worker).is_none());
        assert!(!controller.occupies_port());
    }

    #[test]
    fn the_view_serializes_the_contract_the_window_reads() {
        let controller = opened_controller();
        let showing = {
            let mut state = lock(&controller.state);
            let (attempt, _) = begin_attempt(&mut state, PairingRole::Showing).unwrap();
            state.view.code = Some(Zeroizing::new("7KQ4-M9XR-2HTW".to_owned()));
            state.view.code_expires_at_ms = Some(1_700_000_000_000);
            attempt
        };
        let view = serde_json::to_value(controller.status()).unwrap();
        let mut keys: Vec<_> = view.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "badge",
                "busy",
                "code",
                "codeExpiresAtMs",
                "localFingerprint",
                "localPlatform",
                "message",
                "peerAddress",
                "peerFingerprint",
                "peerPlatform",
                "phase",
                "role",
                "storageOutcome",
            ]
        );
        assert_eq!(view["phase"], "showing");
        assert_eq!(view["role"], "showing");
        assert_eq!(view["code"], "7KQ4-M9XR-2HTW");
        assert_eq!(view["codeExpiresAtMs"], 1_700_000_000_000_u64);
        assert_eq!(view["busy"], true);
        assert!(controller.occupies_port());

        let local = lock(&controller.state)
            .local
            .as_ref()
            .unwrap()
            .fingerprint();
        let peer = fixtures().1.fingerprint();
        update(&controller.state, showing.id, |view| {
            view.badge = Some(BadgeView::between(local, peer));
            view.peer_fingerprint = Some(peer.full_hex().to_ascii_lowercase());
        });
        finish_attempt(&mut lock(&controller.state), showing.id, Ok(()));
        let view = serde_json::to_value(controller.status()).unwrap();
        assert_eq!(view["phase"], "paired");
        assert!(view["code"].is_null());
        assert!(view["codeExpiresAtMs"].is_null());
        let badge = pair_badge(peer, local);
        assert_eq!(
            view["badge"],
            serde_json::json!({ "color": badge.color, "symbols": badge.symbols })
        );
        assert_eq!(
            view["peerFingerprint"],
            peer.full_hex().to_ascii_lowercase()
        );
        assert_eq!(view["busy"], false);
        assert!(!controller.occupies_port());
    }

    #[test]
    fn a_wrong_code_spends_the_shown_code_and_releases_the_port_with_the_role_kept() {
        for (role, message) in [
            (PairingRole::Showing, WRONG_CODE_SHOWN),
            (PairingRole::Entering, WRONG_CODE_ENTERED),
        ] {
            let controller = opened_controller();
            let attempt = {
                let mut state = lock(&controller.state);
                let (attempt, _) = begin_attempt(&mut state, role).unwrap();
                state.view.code = Some(Zeroizing::new("7KQ4-M9XR-2HTW".to_owned()));
                attempt
            };
            assert!(controller.occupies_port());
            // A stale attempt's end changes nothing.
            finish_attempt(
                &mut lock(&controller.state),
                attempt.id + 1,
                Err("stale".into()),
            );
            assert!(controller.occupies_port());
            finish_attempt(
                &mut lock(&controller.state),
                attempt.id,
                Err(failure_message(role, &PairingFailure::NotConfirmed)),
            );
            let view = controller.status();
            assert_eq!(view.phase, "error");
            assert_eq!(view.role, Some(role_code(role)));
            assert_eq!(view.message, message);
            assert!(view.code.is_none());
            assert!(view.badge.is_none());
            assert!(!view.busy);
            assert!(!controller.occupies_port());
        }
    }

    #[test]
    fn an_expired_or_cancelled_attempt_says_why() {
        let control = PairingControl::new(PairingRole::Showing);
        control.stop(TIMED_OUT);
        assert_eq!(control.check().unwrap_err(), CODE_EXPIRED);
        // The first reason wins: a later cancel does not rename an expiry.
        control.stop(STOPPED);
        assert_eq!(control.stopped().as_deref(), Some(CODE_EXPIRED));
        let control = PairingControl::new(PairingRole::Entering);
        control.stop(TIMED_OUT);
        assert_eq!(control.check().unwrap_err(), ENTERING_TIMED_OUT);
        let control = PairingControl::new(PairingRole::Entering);
        assert_eq!(control.or(UNREACHABLE), UNREACHABLE);
        control.stop(STOPPED);
        assert_eq!(control.or(UNREACHABLE), PAIRING_STOPPED);

        let controller = opened_controller();
        let attempt = {
            let mut state = lock(&controller.state);
            let (attempt, _) = begin_attempt(&mut state, PairingRole::Showing).unwrap();
            state.view.code = Some(Zeroizing::new("7KQ4-M9XR-2HTW".to_owned()));
            attempt
        };
        finish_attempt(
            &mut lock(&controller.state),
            attempt.id,
            Err(CODE_EXPIRED.into()),
        );
        let view = controller.status();
        assert_eq!((view.phase, view.role), ("error", Some("showing")));
        assert_eq!(view.message, CODE_EXPIRED);
        assert!(view.code.is_none() && view.code_expires_at_ms.is_none());
        assert!(!controller.occupies_port());
    }

    #[test]
    fn cancel_spends_the_code_and_a_running_worker_ends_ready() {
        let controller = opened_controller();
        let (attempt, control) = {
            let mut state = lock(&controller.state);
            let started = begin_attempt(&mut state, PairingRole::Showing).unwrap();
            state.view.code = Some(Zeroizing::new("7KQ4-M9XR-2HTW".to_owned()));
            started
        };
        let stopping = controller.cancel();
        assert_eq!(stopping.phase, "stopping");
        assert!(stopping.busy);
        assert!(stopping.code.is_none());
        assert!(control.check().is_err());
        assert!(!controller.occupies_port());
        finish_attempt(
            &mut lock(&controller.state),
            attempt.id,
            Err(control.stopped().unwrap()),
        );
        let view = controller.status();
        assert_eq!(view.phase, "ready");
        assert_eq!(view.message, PAIRING_STOPPED);
        assert!(!view.busy);
        assert!(view.role.is_none());

        // Nothing running: cancel settles at once, and never opens an unopened controller.
        assert_eq!(controller.cancel().phase, "ready");
        assert_eq!(PairingController::default().cancel().phase, "closed");
    }

    #[test]
    fn an_attempt_starts_only_from_a_settled_phase() {
        let controller = opened_controller();
        let mut state = lock(&controller.state);
        for phase in ["showing", "connecting", "verifying", "saving", "stopping"] {
            state.view.phase = phase;
            assert_eq!(
                begin_attempt(&mut state, PairingRole::Entering)
                    .err()
                    .as_deref(),
                Some(STILL_FINISHING)
            );
        }
        for phase in ["ready", "error", "paired"] {
            state.view.phase = phase;
            assert!(begin_attempt(&mut state, PairingRole::Entering).is_ok());
            assert_eq!(state.view.phase, "connecting");
            assert_eq!(state.view.role, Some("entering"));
        }
    }

    /// Records by handle; the handle is the key the controller chose, or a legacy name.
    #[derive(Default)]
    struct MemoryStore {
        records: RefCell<Vec<(String, Vec<u8>)>>,
        creates: Cell<usize>,
        loads: Cell<usize>,
        fail_read: Cell<usize>,
        fail_create: Cell<bool>,
    }
    impl MemoryStore {
        fn bytes(&self) -> Option<Vec<u8>> {
            self.records
                .borrow()
                .first()
                .map(|(_, bytes)| bytes.clone())
        }
    }
    impl ProtectedPeerStore for MemoryStore {
        fn list(&self) -> Result<Vec<StoredPeer>, StorageError> {
            let count = self.loads.get() + 1;
            self.loads.set(count);
            if self.fail_read.get() == count {
                return Err(StorageError::AccessDenied);
            }
            Ok(self
                .records
                .borrow()
                .iter()
                .map(|(handle, bytes)| StoredPeer {
                    handle: handle.clone(),
                    record: Zeroizing::new(bytes.clone()),
                })
                .collect())
        }
        fn create_new(&self, key: &str, record: &[u8]) -> Result<(), StorageError> {
            if self.fail_create.get() {
                return Err(StorageError::AccessDenied);
            }
            if self
                .records
                .borrow()
                .iter()
                .any(|(handle, _)| handle == key)
            {
                return Err(StorageError::AlreadyExists);
            }
            self.creates.set(self.creates.get() + 1);
            self.records
                .borrow_mut()
                .push((key.to_owned(), record.to_vec()));
            Ok(())
        }
        fn delete(&self, handle: &str) -> Result<(), StorageError> {
            let mut records = self.records.borrow_mut();
            let before = records.len();
            records.retain(|(stored, _)| stored != handle);
            if records.len() == before {
                return Err(StorageError::ReadFailed);
            }
            Ok(())
        }
    }
    fn fixtures() -> (DeviceIdentity, DeviceIdentity, ConfirmedPeerRecord) {
        let local = DeviceIdentity::generate().unwrap();
        let peer = DeviceIdentity::generate().unwrap();
        let offer = PairingOffer::new("192.168.1.2:24872".parse().unwrap(), peer.certificate_der())
            .unwrap();
        let record = ConfirmedPeerRecord::new(local.fingerprint(), offer).unwrap();
        (local, peer, record)
    }

    /// A controller as `open` leaves it, without native storage or network reads.
    fn opened_controller() -> PairingController {
        let (local, _, _) = fixtures();
        let controller = PairingController::default();
        {
            let mut state = lock(&controller.state);
            state.interface_id = Some("fixture-only".into());
            let offer = PairingOffer::new(
                "192.168.1.1:24872".parse().unwrap(),
                local.certificate_der(),
            )
            .unwrap();
            state.view.local_fingerprint = Some(offer.fingerprint().full_hex());
            state.local = Some(offer);
            state.view.phase = "ready";
            state.view.message = READY.into();
        }
        controller
    }

    #[test]
    fn completed_pairings_are_handed_over_once() {
        let (_, _, record) = fixtures();
        let controller = PairingController::default();
        assert!(controller.take_completed().is_empty());
        let completed = CompletedPairing {
            fingerprint: record.peer().fingerprint(),
            address: record.peer().endpoint(),
            platform: record.peer().platform(),
            interface_id: "fixture-only".into(),
        };
        lock(&controller.state).completed.push(completed.clone());
        assert!(controller.take_completed() == vec![completed]);
        assert!(controller.take_completed().is_empty());
    }

    #[test]
    fn forget_removes_every_record_for_that_computer_and_keeps_the_others() {
        let (identity, _, record) = fixtures();
        let other = DeviceIdentity::generate().unwrap();
        let other_offer = PairingOffer::new(
            "192.168.1.3:24872".parse().unwrap(),
            other.certificate_der(),
        )
        .unwrap();
        let other_record = ConfirmedPeerRecord::new(identity.fingerprint(), other_offer).unwrap();
        let store = MemoryStore::default();
        store.create_new("ConfirmedPeer", &record.encode()).unwrap();
        store
            .create_new("ConfirmedPeer/dup", &record.encode())
            .unwrap();
        store
            .create_new("ConfirmedPeer/other", &other_record.encode())
            .unwrap();
        assert_eq!(read_saved(&store, identity.fingerprint()).unwrap().len(), 3);
        forget_peer(&store, identity.fingerprint(), record.peer().fingerprint()).unwrap();
        let remaining = read_saved(&store, identity.fingerprint()).unwrap();
        assert!(remaining == vec![other_record.clone()]);
        assert!(
            find_saved(&store, identity.fingerprint(), record.peer().fingerprint())
                .unwrap()
                .is_none()
        );
        // Forgetting an unknown computer changes nothing and is not an error.
        forget_peer(&store, identity.fingerprint(), record.peer().fingerprint()).unwrap();
        assert!(read_saved(&store, identity.fingerprint()).unwrap() == vec![other_record]);
    }

    #[test]
    fn saved_peer_restart_and_duplicates_preserve_the_record() {
        let (identity, _, record) = fixtures();
        let store = MemoryStore::default();
        persist_confirmed(&store, &record, identity.fingerprint(), || Ok(()), || {}).unwrap();
        let before = store.records.borrow().clone();
        persist_confirmed(
            &store,
            &record,
            identity.fingerprint(),
            || Ok(()),
            || panic!("must not overwrite"),
        )
        .unwrap();
        assert_eq!(store.creates.get(), 1);
        assert!(read_saved(&store, identity.fingerprint()).unwrap() == vec![record.clone()]);
        assert_eq!(*store.records.borrow(), before);
        assert_eq!(
            store.records.borrow()[0].0,
            record.peer().fingerprint().full_hex().to_ascii_lowercase()
        );
        let different = DeviceIdentity::generate().unwrap();
        assert!(read_saved(&store, different.fingerprint()).is_err());
        // A second computer gets its own record beside the first.
        let other = DeviceIdentity::generate().unwrap();
        let other_offer = PairingOffer::new(
            "192.168.1.3:24872".parse().unwrap(),
            other.certificate_der(),
        )
        .unwrap();
        let other_record = ConfirmedPeerRecord::new(identity.fingerprint(), other_offer).unwrap();
        persist_confirmed(
            &store,
            &other_record,
            identity.fingerprint(),
            || Ok(()),
            || {},
        )
        .unwrap();
        assert_eq!(store.creates.get(), 2);
        assert_eq!(read_saved(&store, identity.fingerprint()).unwrap().len(), 2);
    }

    #[test]
    fn a_saved_record_that_differs_is_never_replaced() {
        let (identity, peer, record) = fixtures();
        let store = MemoryStore::default();
        persist_confirmed(&store, &record, identity.fingerprint(), || Ok(()), || {}).unwrap();
        let moved = ConfirmedPeerRecord::new(
            identity.fingerprint(),
            PairingOffer::new("192.168.1.9:24872".parse().unwrap(), peer.certificate_der())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            persist_confirmed(
                &store,
                &moved,
                identity.fingerprint(),
                || Ok(()),
                || panic!("must not replace")
            )
            .unwrap_err(),
            "That computer's saved record differs from this pairing. Forget it, then pair again."
        );
        assert!(read_saved(&store, identity.fingerprint()).unwrap() == vec![record]);
    }

    #[test]
    fn corruption_denial_and_uncertain_readback_never_claim_saved() {
        let (identity, _, record) = fixtures();
        let store = MemoryStore::default();
        store
            .records
            .borrow_mut()
            .push(("ConfirmedPeer".into(), b"corrupt".to_vec()));
        assert!(
            persist_confirmed(
                &store,
                &record,
                identity.fingerprint(),
                || Ok(()),
                || panic!("must not write")
            )
            .is_err()
        );
        assert_eq!(store.creates.get(), 0);
        let store = MemoryStore::default();
        store.fail_create.set(true);
        let attempted = Cell::new(false);
        assert!(
            persist_confirmed(
                &store,
                &record,
                identity.fingerprint(),
                || Ok(()),
                || attempted.set(true)
            )
            .is_err()
        );
        assert!(attempted.get());
        let store = MemoryStore::default();
        store.fail_read.set(2);
        assert!(
            persist_confirmed(&store, &record, identity.fingerprint(), || Ok(()), || {}).is_err()
        );
        assert_eq!(store.creates.get(), 1);
        assert!(store.bytes().is_some());
    }

    #[test]
    fn cancellation_before_write_and_after_write_or_readback_cannot_succeed() {
        let (identity, _, record) = fixtures();
        for stop_at in 1..=4 {
            let store = MemoryStore::default();
            let count = Cell::new(0);
            let result = persist_confirmed(
                &store,
                &record,
                identity.fingerprint(),
                || {
                    count.set(count.get() + 1);
                    if count.get() == stop_at {
                        Err("canceled".into())
                    } else {
                        Ok(())
                    }
                },
                || {},
            );
            assert!(result.is_err(), "checkpoint {stop_at}");
            assert_eq!(store.creates.get(), usize::from(stop_at >= 3));
        }
    }

    #[test]
    fn cancellation_during_a_blocked_store_write_preserves_unknown_outcome() {
        struct BlockedStore {
            entered: Arc<std::sync::Barrier>,
            released: Arc<std::sync::Barrier>,
            bytes: Mutex<Option<Vec<u8>>>,
        }
        impl ProtectedPeerStore for BlockedStore {
            fn list(&self) -> Result<Vec<StoredPeer>, StorageError> {
                Ok(lock(&self.bytes)
                    .iter()
                    .map(|bytes| StoredPeer {
                        handle: "blocked".into(),
                        record: Zeroizing::new(bytes.clone()),
                    })
                    .collect())
            }
            fn create_new(&self, _: &str, bytes: &[u8]) -> Result<(), StorageError> {
                self.entered.wait();
                self.released.wait();
                *lock(&self.bytes) = Some(bytes.to_vec());
                Ok(())
            }
            fn delete(&self, _: &str) -> Result<(), StorageError> {
                Err(StorageError::Unavailable)
            }
        }
        let (identity, _, record) = fixtures();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let released = Arc::new(std::sync::Barrier::new(2));
        let store = Arc::new(BlockedStore {
            entered: entered.clone(),
            released: released.clone(),
            bytes: Mutex::new(None),
        });
        let control = Arc::new(PairingControl::new(PairingRole::Entering));
        let writer_store = store.clone();
        let writer_control = control.clone();
        let writer = std::thread::spawn(move || {
            persist_confirmed(
                writer_store.as_ref(),
                &record,
                identity.fingerprint(),
                || writer_control.check(),
                || {},
            )
        });
        entered.wait();
        control.stop(STOPPED);
        assert!(control.check().is_err());
        released.wait();
        assert!(writer.join().unwrap().is_err());
        assert!(lock(&store.bytes).is_some());
    }

    #[test]
    fn canceled_before_worker_starts_does_not_access_native_identity() {
        let (identity, _, _) = fixtures();
        let attempt = Attempt {
            id: 1,
            role: PairingRole::Showing,
            interface_id: "not a native interface".into(),
            local: PairingOffer::new(
                "192.168.1.1:24872".parse().unwrap(),
                identity.certificate_der(),
            )
            .unwrap(),
        };
        let control = Arc::new(PairingControl::new(PairingRole::Showing));
        control.stop(STOPPED);
        let result = run_worker(
            &attempt,
            Job::Show,
            &control,
            &Arc::new(Mutex::new(PairingState::default())),
            None,
        );
        assert_eq!(result.unwrap_err(), PAIRING_STOPPED);
    }

    #[test]
    fn repeated_saved_peer_validation_failure_cannot_reuse_previous_success() {
        let (identity, _, record) = fixtures();
        let store = MemoryStore::default();
        persist_confirmed(&store, &record, identity.fingerprint(), || Ok(()), || {}).unwrap();
        store.fail_read.set(store.loads.get() + 2);
        assert!(
            persist_confirmed(
                &store,
                &record,
                identity.fingerprint(),
                || Ok(()),
                || panic!("must not replace")
            )
            .is_err()
        );
        assert_eq!(store.creates.get(), 1);
    }
}
