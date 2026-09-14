//! Explicit pairing only. The controller has no capture, injection, or input protocol access.

use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use monhop_core::{Platform, RevocationSignal};
use monhop_transport::{
    crypto::CertificateFingerprint,
    guarded_endpoint::{EndpointRevoker, GuardedEndpoint, NetworkSelection, RouteCheckFailure},
    identity_store::{ProtectedPeerStore, create_identity, load_identity},
    native_storage::{NativeIdentityStore, NativePeerStore},
    pairing::{
        ConfirmedPeerRecord, PAIR_FRAME_BYTES, PAIRING_PORT, PairFrameKind, PairingOffer,
        encode_pair_frame, initiates_connection, validate_pair_frame,
    },
    policy::PolicyError,
};
use serde::Serialize;

const PAIRING_WINDOW: Duration = Duration::from_secs(120);
const DIAL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(4);
const DIAL_RETRY_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(any(target_os = "macos", test))]
const NETWORK_ACCESS_DEADLINE: Duration = Duration::from_secs(30);
#[cfg(any(target_os = "macos", test))]
const NETWORK_ACCESS_TIMEOUT_REASON: u8 = 3;

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

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingView {
    phase: &'static str,
    local_code: Option<String>,
    local_fingerprint: Option<String>,
    peer_fingerprint: Option<String>,
    peer_address: Option<String>,
    candidate_id: Option<u64>,
    role: Option<&'static str>,
    storage_outcome: &'static str,
    message: String,
    busy: bool,
    network_access: &'static str,
    network_access_message: String,
    local_platform: &'static str,
    peer_platform: Option<&'static str>,
}

impl Default for PairingView {
    fn default() -> Self {
        Self {
            phase: "closed",
            local_code: None,
            local_fingerprint: None,
            peer_fingerprint: None,
            peer_address: None,
            candidate_id: None,
            role: None,
            storage_outcome: "unchanged",
            message: "Open pairing to read this computer's saved identity. Sharing stays off."
                .into(),
            busy: false,
            network_access: "not-verified",
            network_access_message: String::new(),
            local_platform: platform_code(local_platform()),
            peer_platform: None,
        }
    }
}

#[derive(Clone)]
struct Candidate {
    id: u64,
    interface_id: String,
    local: PairingOffer,
    peer: PairingOffer,
}

/// One pairing that reached protected storage, for the app to list and make active.
#[derive(Clone, PartialEq, Eq)]
pub struct CompletedPairing {
    pub fingerprint: CertificateFingerprint,
    pub address: std::net::SocketAddrV4,
    pub platform: Option<Platform>,
    pub interface_id: String,
}

/// A computer protected storage trusts, as its record describes it.
#[derive(Clone, PartialEq, Eq)]
pub struct PairedPeer {
    pub fingerprint: CertificateFingerprint,
    pub address: std::net::SocketAddrV4,
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
    candidate: Option<Candidate>,
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

#[derive(Default)]
struct PairingControl {
    reason: AtomicU8,
    cancel: RevocationSignal,
    revoker: Mutex<Option<EndpointRevoker>>,
}

impl PairingControl {
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

    fn check(&self) -> Result<(), String> {
        match self.reason.load(Ordering::Acquire) {
            1 => return Err("Pairing stopped. Sharing is off.".into()),
            2 => {
                return Err(
                    "Pairing timed out. Start the waiting computer first, then connect again."
                        .into(),
                );
            }
            _ => {}
        }
        if lock(&self.revoker)
            .as_ref()
            .is_some_and(EndpointRevoker::is_revoked)
        {
            return Err("The network changed. Pairing stopped. Choose the physical network and reconnect explicitly.".into());
        }
        Ok(())
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

    /// True while a code exchange is in progress: from inspecting a code until it is paired or
    /// cancelled. Pairing and sharing use one port, so no connection may run meanwhile.
    pub fn occupies_port(&self) -> bool {
        let state = lock(&self.state);
        state.candidate.is_some() && state.view.phase != "paired"
    }

    pub fn code_for_copy(&self) -> Result<String, String> {
        lock(&self.state)
            .view
            .local_code
            .clone()
            .ok_or_else(|| "Open pairing before copying this computer's code.".into())
    }

    fn require_idle(&self) -> Result<(), String> {
        self.require_open_app()?;
        let mut worker = lock(&self.worker);
        if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Err(
                "Pairing is still finishing. Complete any system prompt, or cancel and wait."
                    .into(),
            );
        }
        if let Some(worker) = worker.take() {
            worker.join().map_err(|_| {
                "The pairing worker stopped unexpectedly. Reopen pairing before continuing."
                    .to_owned()
            })?;
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
            state.candidate = None;
            state.saved.clear();
            state.local = None;
            state.control = None;
            state.interface_id = Some(interface_id.clone());
            state.view = PairingView::default();
            state.generation
        };
        let result = (|| {
            let adapter = selected_network(&interface_id)?;
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
            let local = PairingOffer::new(adapter.local, identity.certificate_der())
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
                state.view.local_code = Some(local.to_code());
                state.view.local_fingerprint = Some(local.fingerprint().full_hex());
                state.local = Some(local);
                state.saved = saved;
                state.view.phase = "ready";
                state.view.message = "Exchange connection codes, then compare the full fingerprints on both screens.".into();
            }
            Err(error) => {
                state.view.phase = "error";
                state.view.message = error;
            }
        }
        Ok(state.view.clone())
    }

    pub fn inspect(&self, code: String) -> Result<PairingView, String> {
        let _operation = lock(&self.operation);
        self.require_idle()?;
        let mut state = lock(&self.state);
        self.require_open_app()?;
        state.generation += 1;
        state.candidate = None;
        state.view.candidate_id = None;
        let peer = PairingOffer::parse(&code).map_err(display_error)?;
        if state
            .local
            .as_ref()
            .is_some_and(|local| local.fingerprint() == peer.fingerprint())
        {
            return Err(
                "That is this computer's own code. Paste the other computer's code.".into(),
            );
        }
        if state
            .saved
            .iter()
            .any(|saved| saved.peer().fingerprint() == peer.fingerprint())
        {
            return Err(
                "That computer is already paired. Choose it on Home to share with it.".into(),
            );
        }
        set_candidate(&mut state, peer)?;
        state.view.phase = "review";
        state.view.message = "Compare the full peer fingerprint here with This computer on the other screen. Confirm on both computers.".into();
        Ok(state.view.clone())
    }

    pub fn confirm(&self, candidate_id: u64) -> Result<PairingView, String> {
        let _operation = lock(&self.operation);
        self.require_idle()?;
        let mut state = lock(&self.state);
        self.require_open_app()?;
        let candidate = state
            .candidate
            .as_ref()
            .filter(|candidate| candidate.id == candidate_id)
            .cloned()
            .ok_or(
                "That confirmation is no longer current. Inspect the other computer's code again.",
            )?;
        let control = Arc::new(PairingControl::default());
        state.control = Some(control.clone());
        state.view.busy = true;
        state.view.storage_outcome = "unchanged";
        state.view.phase = "connecting";
        state.view.message =
            "Checking the saved identity and selected physical network. Sharing stays off.".into();
        let shared = self.state.clone();
        let spawn = std::thread::Builder::new()
            .name("monhop-pairing".into())
            .spawn(move || {
                let result = run_worker(&candidate, &control, &shared);
                let mut state = lock(&shared);
                finish_exchange(
                    &mut state,
                    candidate.id,
                    result.and_then(|_| control.check()),
                );
                if let Some(revoker) = lock(&control.revoker).take() {
                    revoker.revoke();
                }
            });
        match spawn {
            Ok(worker) => {
                *lock(&self.worker) = Some(worker);
            }
            Err(_) => {
                state.view.busy = false;
                state.view.phase = "error";
                state.view.message =
                    "The pairing worker could not start. Nothing was connected.".into();
            }
        }
        Ok(state.view.clone())
    }

    pub fn request_network_access(&self, candidate_id: u64) -> Result<PairingView, String> {
        #[cfg(target_os = "macos")]
        {
            self.request_network_access_with(candidate_id, |candidate, cancel| {
                if cancel.is_revoked() {
                    return Err("Network access request stopped.".into());
                }
                let mut selection = selected_network(&candidate.interface_id)?;
                if cancel.is_revoked() {
                    return Err("Network access request stopped.".into());
                }
                if selection.local != candidate.local.endpoint() {
                    return Err("The network address changed. Open pairing again.".into());
                }
                selection.peer = candidate.peer.endpoint();
                GuardedEndpoint::request_local_network_access_after_local_action(selection, cancel)
                    .map_err(network_preparation_error)
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = candidate_id;
            Err("Local Network permission requests are macOS-only.".into())
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn request_network_access_with(
        &self,
        candidate_id: u64,
        request: impl FnOnce(&Candidate, &RevocationSignal) -> Result<(), String> + Send + 'static,
    ) -> Result<PairingView, String> {
        let _operation = lock(&self.operation);
        self.require_idle()?;
        let mut state = lock(&self.state);
        self.require_open_app()?;
        if state.view.phase != "review" {
            return Err(
                "Inspect the other computer's code before requesting network access.".into(),
            );
        }
        let candidate = state
            .candidate
            .as_ref()
            .filter(|candidate| candidate.id == candidate_id)
            .cloned()
            .ok_or("That code is no longer current. Inspect it again.")?;
        let previous_phase = state.view.phase;
        let control = Arc::new(PairingControl::default());
        state.control = Some(control.clone());
        state.view.busy = true;
        state.view.phase = "requesting-network";
        state.view.network_access = "requesting";
        state.view.network_access_message =
            "Asking macOS using the selected computer. No data is sent and sharing stays off."
                .into();
        state.view.message = state.view.network_access_message.clone();
        let shared = self.state.clone();
        let spawn = std::thread::Builder::new().name("monhop-network-access".into()).spawn(move || {
            let _watchdog = watch_deadline(&control, NETWORK_ACCESS_DEADLINE, NETWORK_ACCESS_TIMEOUT_REASON);
            let result = control.check()
                .and_then(|_| request(&candidate, &control.cancel))
                .and_then(|_| control.check());
            let timed_out = control.reason.load(Ordering::Acquire) == NETWORK_ACCESS_TIMEOUT_REASON;
            let mut state = lock(&shared);
            if state.candidate.as_ref().is_some_and(|value| value.id == candidate.id) {
                state.view.phase = previous_phase;
                if timed_out {
                    state.view.network_access = "incomplete";
                    state.view.network_access_message =
                        "The network access request did not finish. Try again.".into();
                } else {
                    match result {
                        Ok(()) => {
                            state.view.network_access = "attempted";
                            state.view.network_access_message = "Request attempted. If macOS asks, allow MonHop, then connect. macOS does not report the permission choice here.".into();
                        }
                        Err(error) => {
                            state.view.network_access = "incomplete";
                            state.view.network_access_message = error;
                        }
                    }
                }
                state.view.message = state.view.network_access_message.clone();
            } else {
                state.view.phase = "ready";
                state.view.network_access = "not-verified";
                state.view.network_access_message = "Request stopped. Open/reload pairing to continue. No permission result was confirmed.".into();
                state.view.message = state.view.network_access_message.clone();
            }
            state.view.busy = false;
        });
        match spawn {
            Ok(worker) => *lock(&self.worker) = Some(worker),
            Err(_) => {
                state.view.busy = false;
                state.view.phase = previous_phase;
                state.view.network_access = "incomplete";
                state.view.network_access_message =
                    "The request could not start. Try again.".into();
            }
        }
        Ok(state.view.clone())
    }

    pub fn cancel(&self) -> PairingView {
        let mut state = lock(&self.state);
        if let Some(control) = &state.control {
            control.stop(1);
        }
        state.generation += 1;
        state.candidate = None;
        state.view.candidate_id = None;
        state.view.phase = if state.view.busy { "stopping" } else { "ready" };
        state.view.message = if state.view.busy {
            "Connection stopped. Finish any open system prompt before another pairing action."
                .into()
        } else {
            "Pairing stopped. Open/reload pairing to continue. Sharing is off.".into()
        };
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
        state.candidate = None;
        state.view.candidate_id = None;
        state.view.peer_fingerprint = None;
        state.view.peer_address = None;
        state.view.peer_platform = None;
        state.view.busy = false;
        match &result {
            Ok(()) => {
                state
                    .saved
                    .retain(|saved| saved.peer().fingerprint() != peer);
                state.view.storage_outcome = "unchanged";
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

fn set_candidate(state: &mut PairingState, peer: PairingOffer) -> Result<(), String> {
    let local = state
        .local
        .clone()
        .ok_or("Open pairing on the selected network first.")?;
    let record =
        ConfirmedPeerRecord::new(local.fingerprint(), peer.clone()).map_err(display_error)?;
    if local.endpoint().ip() == peer.endpoint().ip() {
        return Err("The other computer must have a different local address.".into());
    }
    if record.encode().len() > monhop_transport::pairing::MAX_PEER_RECORD_BYTES {
        return Err("That identity is too large to store safely.".into());
    }
    state.generation += 1;
    state.view.network_access = "not-verified";
    state.view.network_access_message.clear();
    state.view.peer_fingerprint = Some(peer.fingerprint().full_hex());
    state.view.peer_address = Some(peer.endpoint().to_string());
    state.view.peer_platform = peer.platform().map(platform_code);
    state.view.candidate_id = Some(state.generation);
    state.view.role = Some(
        if initiates_connection(
            local_platform(),
            local.fingerprint(),
            peer.platform(),
            peer.fingerprint(),
        ) {
            "connect"
        } else {
            "listen"
        },
    );
    state.candidate = Some(Candidate {
        id: state.generation,
        interface_id: state
            .interface_id
            .clone()
            .ok_or("Choose a physical network first.")?,
        local,
        peer,
    });
    Ok(())
}

/// The exchange's terminal state. A failed exchange drops its candidate so the port goes back to
/// the supervisor; the peer fields stay for the message. The user inspects a code again to retry.
fn finish_exchange(state: &mut PairingState, candidate_id: u64, result: Result<(), String>) {
    if state
        .candidate
        .as_ref()
        .is_some_and(|value| value.id == candidate_id)
    {
        match result {
            Ok(()) => {
                state.view.phase = "paired";
                state.view.message =
                    "Identity check passed on both computers. MonHop connects to it now.".into();
            }
            Err(error) => {
                state.candidate = None;
                state.view.candidate_id = None;
                state.view.phase = "error";
                state.view.message = match state.view.storage_outcome {
                    "unverified" => format!(
                        "Save result not confirmed. Reload or explicitly forget the saved device. {error}"
                    ),
                    "verified" => format!(
                        "The device is saved here; the connection check did not finish. {error}"
                    ),
                    _ => error,
                };
            }
        }
    }
    if state.view.phase == "stopping" {
        state.view.phase = "error";
        state.view.message = if state.view.storage_outcome == "unverified" {
            "Pairing stopped. The save result is not confirmed. Reload the saved device before continuing."
        } else {
            "Pairing stopped. Open pairing again to continue. Sharing is off."
        }
        .into();
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

/// Stops `control` with `reason` once `deadline` elapses; dropping the guard first disarms it.
#[cfg(any(target_os = "macos", test))]
fn watch_deadline(
    control: &Arc<PairingControl>,
    deadline: Duration,
    reason: u8,
) -> std::sync::mpsc::Sender<()> {
    let (armed, disarm) = std::sync::mpsc::channel::<()>();
    let control = control.clone();
    std::thread::spawn(move || {
        if disarm.recv_timeout(deadline) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            control.stop(reason);
        }
    });
    armed
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

/// Windows dials repeatedly since macOS may not have pressed Connect yet.
async fn dial_until_connected(
    endpoint: &GuardedEndpoint,
    control: &PairingControl,
) -> Result<quinn::Connection, String> {
    retry_until_cancelled(
        control,
        DIAL_ATTEMPT_TIMEOUT,
        DIAL_RETRY_INTERVAL,
        || async { endpoint.connect().map_err(|_| ())?.await.map_err(|_| ()) },
    )
    .await
}

fn run_worker(
    candidate: &Candidate,
    control: &Arc<PairingControl>,
    state: &Arc<Mutex<PairingState>>,
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|_| "The pairing runtime could not start.")?;
    runtime.block_on(async {
        let deadline_control = control.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(PAIRING_WINDOW).await;
            deadline_control.stop(2);
        });
        let result = run_session(candidate, control, state).await;
        timer.abort();
        result
    })
}

async fn run_session(
    candidate: &Candidate,
    control: &Arc<PairingControl>,
    state: &Arc<Mutex<PairingState>>,
) -> Result<(), String> {
    control.check()?;
    let identity = load_identity(&NativeIdentityStore)
        .map_err(display_error)?
        .ok_or("The local identity is missing. Nothing was replaced.")?;
    control.check()?;
    if identity.fingerprint() != candidate.local.fingerprint() {
        return Err("This computer's identity changed. Open pairing and compare again.".into());
    }
    update(state, candidate.id, |view| {
        view.storage_outcome = "unverified";
    });
    let saved = find_saved(
        &NativePeerStore,
        identity.fingerprint(),
        candidate.peer.fingerprint(),
    )?;
    if saved
        .as_ref()
        .is_some_and(|saved| saved.peer() != &candidate.peer)
    {
        return Err(
            "That computer's saved record differs from its code. Forget it, then pair again."
                .into(),
        );
    }
    update(state, candidate.id, |view| {
        view.storage_outcome = if saved.is_some() {
            "verified"
        } else {
            "unchanged"
        };
    });
    control.check()?;
    let mut selection = selected_network(&candidate.interface_id)?;
    if selection.local != candidate.local.endpoint() {
        return Err("The selected network address changed. Open pairing again.".into());
    }
    selection.peer = candidate.peer.endpoint();
    let peer = candidate.peer.verified_peer().map_err(display_error)?;
    control.check()?;
    let endpoint = GuardedEndpoint::bind_after_local_enable(selection, &identity, &peer)
        .map_err(network_preparation_error)?;
    control.attach(endpoint.revoker());
    control.check()?;
    let initiate = initiates_connection(
        local_platform(),
        candidate.local.fingerprint(),
        candidate.peer.platform(),
        candidate.peer.fingerprint(),
    );
    update(state, candidate.id, |view| {
        view.phase = if initiate { "connecting" } else { "waiting" };
        view.message = if initiate {
            "Connecting to the other computer."
        } else {
            "Waiting for the other computer to connect."
        }
        .into();
    });
    let connection = if initiate {
        dial_until_connected(&endpoint, control).await?
    } else {
        endpoint
            .accept()
            .await
            .map_err(|_| connection_error("waiting for an incoming connection"))?
    };
    control.check()?;
    let local = identity.fingerprint();
    let remote = candidate.peer.fingerprint();
    exchange_pairing(&connection, initiate, local, remote, control, || {
        update(state, candidate.id, |view| {
            view.phase = "saving";
            view.storage_outcome = "unverified";
            view.message = "Identity verified. Saving this device in OS-protected storage.".into();
        });
        let record =
            ConfirmedPeerRecord::new(local, candidate.peer.clone()).map_err(display_error)?;
        persist_confirmed(
            &NativePeerStore,
            &record,
            local,
            || control.check(),
            || {
                update(state, candidate.id, |view| {
                    view.storage_outcome = "unverified";
                })
            },
        )?;
        update(state, candidate.id, |view| {
            view.storage_outcome = "verified";
        });
        let mut locked = lock(state);
        locked.completed.push(CompletedPairing {
            fingerprint: candidate.peer.fingerprint(),
            address: candidate.peer.endpoint(),
            platform: candidate.peer.platform(),
            interface_id: candidate.interface_id.clone(),
        });
        if locked
            .candidate
            .as_ref()
            .is_some_and(|c| c.id == candidate.id)
        {
            locked.saved.push(record);
        }
        Ok(())
    })
    .await?;
    // Both records are already saved and read back; a teardown race must not report failure.
    let _ = endpoint.close_and_wait_idle().await;
    lock(&control.revoker).take();
    Ok(())
}

async fn exchange_pairing(
    connection: &quinn::Connection,
    initiate: bool,
    local: CertificateFingerprint,
    remote: CertificateFingerprint,
    control: &PairingControl,
    persist: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    control.check()?;
    let (mut send, mut receive) = if initiate {
        connection
            .open_bi()
            .await
            .map_err(|_| connection_error("opening the identity exchange"))?
    } else {
        connection
            .accept_bi()
            .await
            .map_err(|_| connection_error("waiting for the identity exchange"))?
    };
    if initiate {
        send_frame(&mut send, PairFrameKind::Hello, local, remote).await?;
    }
    receive_frame(&mut receive, PairFrameKind::Hello, remote, local).await?;
    control.check()?;
    if !initiate {
        send_frame(&mut send, PairFrameKind::Hello, local, remote).await?;
    }
    control.check()?;
    persist()?;
    control.check()?;
    send_frame(&mut send, PairFrameKind::Saved, local, remote).await?;
    send.finish()
        .map_err(|_| connection_error("finishing the saved-device message"))?;
    receive_frame(&mut receive, PairFrameKind::Saved, remote, local).await?;
    let mut extra = [0];
    reject_if_unexpected_extra_data(receive.read(&mut extra).await)?;
    let _ = send.stopped().await;
    Ok(())
}

/// After Saved, a read error is a benign teardown race; only real extra data is a protocol failure.
fn reject_if_unexpected_extra_data<E>(read: Result<Option<usize>, E>) -> Result<(), String> {
    if matches!(read, Ok(Some(_))) {
        return Err("Unexpected extra pairing data. The connection was closed.".into());
    }
    Ok(())
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
                "That computer's saved record differs from its code. Forget it, then pair again."
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

async fn send_frame(
    send: &mut quinn::SendStream,
    kind: PairFrameKind,
    local: CertificateFingerprint,
    peer: CertificateFingerprint,
) -> Result<(), String> {
    send.write_all(&encode_pair_frame(kind, local, peer))
        .await
        .map_err(|_| connection_error("sending the pairing message"))
}

async fn receive_frame(
    receive: &mut quinn::RecvStream,
    kind: PairFrameKind,
    peer: CertificateFingerprint,
    local: CertificateFingerprint,
) -> Result<(), String> {
    let mut bytes = [0; PAIR_FRAME_BYTES];
    receive
        .read_exact(&mut bytes)
        .await
        .map_err(|_| connection_error("reading the pairing message"))?;
    validate_pair_frame(&bytes, kind, peer, local).map_err(display_error)
}

fn update(state: &Mutex<PairingState>, id: u64, change: impl FnOnce(&mut PairingView)) {
    let mut state = lock(state);
    if state
        .candidate
        .as_ref()
        .is_some_and(|candidate| candidate.id == id)
    {
        change(&mut state.view);
    }
}

fn selected_network(interface_id: &str) -> Result<NetworkSelection, String> {
    #[cfg(target_os = "macos")]
    let adapters = monhop_platform_macos::network::enumerate_adapters_with_attachment()
        .map_err(display_error)?;
    #[cfg(windows)]
    let adapters = monhop_platform_windows::network::enumerate_adapters().map_err(display_error)?;
    let mut matches = adapters.into_iter().filter(|adapter| {
        format!(
            "{}:{}:{}",
            adapter.stable_id, adapter.index, adapter.address
        ) == interface_id
    });
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
    Ok(NetworkSelection {
        stable_id: adapter.stable_id,
        interface_index: adapter.index,
        local: std::net::SocketAddrV4::new(adapter.address, PAIRING_PORT),
        peer: std::net::SocketAddrV4::new(adapter.address, PAIRING_PORT),
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
            PolicyError::OffLinkPeer => "The other computer is outside this network. Connect both computers to the same local Wi-Fi or Ethernet, then check their connection codes again.".into(),
            _ => format!("Network check stopped: {policy}. Check the selected network, then try again."),
        };
    }
    format!(
        "Could not prepare the selected network ({:?}). Check its status and try again. This result does not identify a permission or firewall problem.",
        error.kind()
    )
}

fn connection_error(stage: &str) -> String {
    format!(
        "Pairing stopped while {stage}. Start the waiting computer first. Check Local Network access on macOS and MonHop firewall access on Windows, then retry. This error does not identify which permission or network check failed."
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
    use monhop_transport::{
        crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig},
        identity_store::{StorageError, StoredPeer},
    };
    use std::cell::{Cell, RefCell};
    use zeroize::Zeroizing;

    #[test]
    fn shutdown_rejects_all_mutating_actions_before_native_work() {
        let controller = PairingController::default();
        controller.request_shutdown();
        assert!(controller.shutdown_ready());
        assert!(controller.open("physical-network".into(), false).is_err());
        assert!(controller.open("physical-network".into(), true).is_err());
        assert!(controller.inspect("not-a-code".into()).is_err());
        assert!(controller.confirm(1).is_err());
        assert!(
            controller
                .forget(fixtures().2.peer().fingerprint())
                .is_err()
        );
        assert!(
            controller
                .request_network_access_with(1, |_, _| panic!("request must not run"))
                .is_err()
        );
        assert!(lock(&controller.worker).is_none());
    }

    #[test]
    fn queued_confirmation_cannot_outlive_shutdown() {
        let controller = Arc::new(PairingController::default());
        let operation = lock(&controller.operation);
        let queued = controller.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            send.send(queued.confirm(1).is_err()).unwrap();
        });
        controller.request_shutdown();
        drop(operation);
        assert!(receive.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
        assert!(lock(&controller.worker).is_none());
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

    fn request_controller() -> PairingController {
        let (local, _, record) = fixtures();
        let controller = PairingController::default();
        {
            let mut state = lock(&controller.state);
            state.interface_id = Some("fixture-only".into());
            state.local = Some(
                PairingOffer::new(
                    "192.168.1.1:24872".parse().unwrap(),
                    local.certificate_der(),
                )
                .unwrap(),
            );
            set_candidate(&mut state, record.peer().clone()).unwrap();
            state.view.phase = "review";
        }
        controller
    }

    #[test]
    fn network_request_needs_current_candidate_and_does_not_establish_trust() {
        let controller = PairingController::default();
        assert!(
            controller
                .request_network_access_with(1, |_, _| panic!("startup request"))
                .is_err()
        );
        let controller = request_controller();
        let id = controller.status().candidate_id.unwrap();
        assert!(
            controller
                .request_network_access_with(id + 1, |_, _| panic!("stale request"))
                .is_err()
        );
        controller
            .request_network_access_with(id, |_, cancel| {
                assert!(!cancel.is_revoked());
                Ok(())
            })
            .unwrap();
        lock(&controller.worker).take().unwrap().join().unwrap();
        let view = controller.status();
        assert_eq!(view.phase, "review");
        assert_eq!(view.network_access, "attempted");
        assert_eq!(view.storage_outcome, "unchanged");
        assert!(lock(&controller.state).saved.is_empty());
    }

    #[test]
    fn cancelling_network_request_blocks_late_success_and_concurrent_actions() {
        {
            let controller = request_controller();
            let id = controller.status().candidate_id.unwrap();
            assert!(controller.occupies_port());
            let (started, ready) = std::sync::mpsc::channel();
            let (finish, released) = std::sync::mpsc::channel();
            controller
                .request_network_access_with(id, move |_, cancel| {
                    started.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(2)).unwrap();
                    assert!(cancel.is_revoked());
                    Ok(())
                })
                .unwrap();
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(controller.confirm(id).is_err());
            assert!(controller.open("fixture-only".into(), false).is_err());
            assert!(
                controller
                    .request_network_access_with(id, |_, _| panic!("concurrent request"))
                    .is_err()
            );
            assert_eq!(controller.cancel().phase, "stopping");
            finish.send(()).unwrap();
            lock(&controller.worker).take().unwrap().join().unwrap();
            let view = controller.status();
            assert_eq!(view.network_access, "not-verified");
            assert_eq!(view.phase, "ready");
            assert!(!view.busy);
            assert!(view.candidate_id.is_none());
            assert!(!controller.occupies_port());
        }
    }

    #[test]
    fn cancel_after_a_network_request_releases_the_port_and_the_candidate() {
        let controller = request_controller();
        let id = controller.status().candidate_id.unwrap();
        controller
            .request_network_access_with(id, |_, _| Ok(()))
            .unwrap();
        lock(&controller.worker).take().unwrap().join().unwrap();
        assert_eq!(controller.status().phase, "review");
        assert!(controller.occupies_port());
        let stopped = controller.cancel();
        assert_eq!(stopped.phase, "ready");
        assert_eq!(stopped.candidate_id, None);
        assert!(!stopped.busy);
        assert!(!controller.occupies_port());
        assert!(controller.confirm(id).is_err());
    }

    #[test]
    fn inspect_refuses_this_computers_own_code_and_an_already_paired_computer() {
        let (local, _, record) = fixtures();
        let controller = PairingController::default();
        let own_offer = PairingOffer::new(
            "192.168.1.1:24872".parse().unwrap(),
            local.certificate_der(),
        )
        .unwrap();
        {
            let mut state = lock(&controller.state);
            state.interface_id = Some("fixture-only".into());
            state.local = Some(own_offer.clone());
            state.saved = vec![record.clone()];
            state.view.phase = "ready";
        }
        assert_eq!(
            controller.inspect(own_offer.to_code()).err().as_deref(),
            Some("That is this computer's own code. Paste the other computer's code.")
        );
        assert_eq!(
            controller.inspect(record.peer().to_code()).err().as_deref(),
            Some("That computer is already paired. Choose it on Home to share with it.")
        );
        assert!(!controller.occupies_port());
        let other = DeviceIdentity::generate().unwrap();
        let fresh = PairingOffer::new(
            "192.168.1.3:24872".parse().unwrap(),
            other.certificate_der(),
        )
        .unwrap();
        let review = controller.inspect(fresh.to_code()).unwrap();
        assert_eq!(review.phase, "review");
        assert_eq!(
            review.peer_fingerprint,
            Some(other.fingerprint().full_hex())
        );
        assert!(controller.occupies_port());
    }

    #[test]
    fn a_failed_exchange_releases_the_port_and_a_finished_one_keeps_the_candidate() {
        let controller = request_controller();
        let id = controller.status().candidate_id.unwrap();
        assert!(controller.occupies_port());
        finish_exchange(&mut lock(&controller.state), id + 1, Err("stale".into()));
        assert!(controller.occupies_port());
        assert_eq!(controller.status().phase, "review");
        finish_exchange(&mut lock(&controller.state), id, Err("no route".into()));
        let view = controller.status();
        assert_eq!(view.phase, "error");
        assert_eq!(view.message, "no route");
        assert!(view.candidate_id.is_none());
        assert!(view.peer_fingerprint.is_some());
        assert!(!view.busy);
        assert!(!controller.occupies_port());
        assert!(controller.confirm(id).is_err());

        let controller = request_controller();
        let id = controller.status().candidate_id.unwrap();
        finish_exchange(&mut lock(&controller.state), id, Ok(()));
        assert_eq!(controller.status().phase, "paired");
        assert!(!controller.occupies_port());
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
    fn network_request_error_is_not_a_permission_verdict() {
        let controller = request_controller();
        let id = controller.status().candidate_id.unwrap();
        controller
            .request_network_access_with(id, |_, _| Err("Network changed".into()))
            .unwrap();
        lock(&controller.worker).take().unwrap().join().unwrap();
        let view = controller.status();
        assert_eq!(view.phase, "review");
        assert_eq!(view.network_access, "incomplete");
        assert_eq!(view.network_access_message, "Network changed");
        assert!(!view.busy);
    }

    #[test]
    fn controller_startup_status_and_invalid_confirmation_are_inert() {
        let controller = PairingController::default();
        assert_eq!(controller.status().phase, "closed");
        assert!(controller.confirm(1).is_err());
        assert!(controller.inspect("not a code".into()).is_err());
        assert!(lock(&controller.worker).is_none());
        assert!(!controller.occupies_port());
    }

    #[test]
    fn copy_writes_only_the_current_public_offer_without_changing_pairing() {
        let (identity, _, _) = fixtures();
        let offer = PairingOffer::new(
            "192.168.1.1:24872".parse().unwrap(),
            identity.certificate_der(),
        )
        .unwrap();
        let controller = PairingController::default();
        lock(&controller.state).view.local_code = Some(offer.to_code());
        let before = serde_json::to_value(controller.status()).unwrap();
        let mut writes = Vec::new();
        crate::public_code_copy::copy(&controller, |text| {
            writes.push(text);
            Ok(())
        })
        .unwrap();
        assert_eq!(writes, [offer.to_code()]);
        assert!(PairingOffer::parse(&writes[0]).unwrap() == offer);
        assert_eq!(serde_json::to_value(controller.status()).unwrap(), before);
        assert!(lock(&controller.worker).is_none());

        assert_eq!(
            crate::public_code_copy::copy(&controller, |_| Err("busy".into())),
            Err("busy".into())
        );
        assert_eq!(serde_json::to_value(controller.status()).unwrap(), before);
        lock(&controller.state).view.local_code = None;
        assert!(crate::public_code_copy::copy(&controller, |_| panic!("stale code")).is_err());
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
    fn stale_candidate_cannot_start_after_cancel() {
        let (identity, _, record) = fixtures();
        let controller = PairingController::default();
        let id = {
            let mut state = lock(&controller.state);
            state.local = Some(
                PairingOffer::new(
                    "192.168.1.1:24872".parse().unwrap(),
                    identity.certificate_der(),
                )
                .unwrap(),
            );
            state.interface_id = Some("fixture".into());
            set_candidate(&mut state, record.peer().clone()).unwrap();
            state.view.candidate_id.unwrap()
        };
        controller.cancel();
        assert!(controller.confirm(id).is_err());
        assert!(lock(&controller.worker).is_none());
    }

    #[derive(Debug)]
    struct ClosingSocket {
        inner: Arc<dyn quinn::AsyncUdpSocket>,
        closed: std::sync::atomic::AtomicBool,
    }
    impl quinn::AsyncUdpSocket for ClosingSocket {
        fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn quinn::UdpPoller>> {
            self.inner.clone().create_io_poller()
        }
        fn try_send(&self, transmit: &quinn::udp::Transmit) -> std::io::Result<()> {
            if self.closed.load(Ordering::Acquire) {
                return Err(std::io::ErrorKind::ConnectionAborted.into());
            }
            self.inner.try_send(transmit)
        }
        fn poll_recv(
            &self,
            cx: &mut std::task::Context,
            bufs: &mut [std::io::IoSliceMut<'_>],
            meta: &mut [quinn::udp::RecvMeta],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.closed.load(Ordering::Acquire) {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::ConnectionAborted.into()));
            }
            self.inner.poll_recv(cx, bufs, meta)
        }
        fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
            self.inner.local_addr()
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
        let control = Arc::new(PairingControl::default());
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
        control.stop(1);
        assert!(control.check().is_err());
        released.wait();
        assert!(writer.join().unwrap().is_err());
        assert!(lock(&store.bytes).is_some());
    }

    #[test]
    fn canceled_before_worker_starts_does_not_access_native_identity() {
        let (identity, _, record) = fixtures();
        let candidate = Candidate {
            id: 1,
            interface_id: "not a native interface".into(),
            local: PairingOffer::new(
                "192.168.1.1:24872".parse().unwrap(),
                identity.certificate_der(),
            )
            .unwrap(),
            peer: record.peer().clone(),
        };
        let control = Arc::new(PairingControl::default());
        control.stop(1);
        let result = run_worker(
            &candidate,
            &control,
            &Arc::new(Mutex::new(PairingState::default())),
        );
        assert_eq!(result.unwrap_err(), "Pairing stopped. Sharing is off.");
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

    // Only this opt-in fixture uses loopback; production sockets require GuardedEndpoint.
    async fn loopback_connections(
        a: &DeviceIdentity,
        b: &DeviceIdentity,
    ) -> (
        quinn::Endpoint,
        quinn::Endpoint,
        quinn::Connection,
        quinn::Connection,
        Arc<ClosingSocket>,
    ) {
        let ap = PairingOffer::new("192.168.1.1:24872".parse().unwrap(), a.certificate_der())
            .unwrap()
            .verified_peer()
            .unwrap();
        let bp = PairingOffer::new("192.168.1.2:24872".parse().unwrap(), b.certificate_der())
            .unwrap()
            .verified_peer()
            .unwrap();
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(SecureQuicConfig::client(a, &bp).unwrap());
        use quinn::Runtime;
        let native = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let socket = Arc::new(ClosingSocket {
            inner: quinn::TokioRuntime.wrap_udp_socket(native).unwrap(),
            closed: std::sync::atomic::AtomicBool::new(false),
        });
        let server = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(SecureQuicConfig::server(b, &ap).unwrap()),
            socket.clone(),
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();
        let (left, right) = tokio::join!(
            client
                .connect(server.local_addr().unwrap(), LOCAL_TLS_SERVER_NAME)
                .unwrap(),
            async { server.accept().await.unwrap().await }
        );
        (client, server, left.unwrap(), right.unwrap(), socket)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "explicit bounded localhost TLS pairing probe, no user storage"]
    async fn native_pairing_exchange_and_one_sided_failure() {
        tokio::time::timeout(Duration::from_secs(8), async {
            for iteration in 0..24 {
                let fail = iteration == 23;
                let (a, b, record) = fixtures();
                let (_client, server, left, right, socket) = loopback_connections(&a, &b).await;
                let saved_a = Cell::new(false);
                let saved_b = Cell::new(false);
                let ca = PairingControl::default();
                let cb = PairingControl::default();
                let (ra, rb) = tokio::join!(
                    exchange_pairing(&left, true, a.fingerprint(), b.fingerprint(), &ca, || {
                        saved_a.set(true);
                        Ok(())
                    }),
                    async {
                        let result = exchange_pairing(
                            &right,
                            false,
                            b.fingerprint(),
                            a.fingerprint(),
                            &cb,
                            || {
                                if fail {
                                    Err("store denied".into())
                                } else {
                                    saved_b.set(true);
                                    Ok(())
                                }
                            },
                        )
                        .await;
                        server.close(0_u32.into(), b"fixture finished");
                        server.wait_idle().await;
                        socket.closed.store(true, Ordering::Release);
                        result
                    }
                );
                assert_eq!(ra.is_ok(), !fail);
                assert_eq!(rb.is_ok(), !fail);
                assert_eq!(saved_b.get(), !fail);
                if !fail {
                    assert!(saved_a.get());
                    assert!(record.encode().len() < 4096);
                }
            }
        })
        .await
        .expect("pairing fixture exceeded bound");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "explicit bounded localhost negative pairing probe, no user storage"]
    async fn native_pairing_rejects_wrong_identity_trailing_data_and_late_cancel() {
        tokio::time::timeout(Duration::from_secs(8), async {
            for case in ["wrong-identity", "trailing", "cancel"] {
                let (a, b, _) = fixtures();
                let (_client, server, left, right, _socket) = loopback_connections(&a, &b).await;
                let control = PairingControl::default();
                let saved = Cell::new(false);
                let (result, _) = tokio::join!(
                    exchange_pairing(
                        &left,
                        true,
                        a.fingerprint(),
                        b.fingerprint(),
                        &control,
                        || {
                            saved.set(true);
                            if case == "cancel" {
                                control.stop(1);
                            }
                            Ok(())
                        }
                    ),
                    async {
                        let (mut send, mut receive) = right.accept_bi().await.unwrap();
                        receive_frame(
                            &mut receive,
                            PairFrameKind::Hello,
                            a.fingerprint(),
                            b.fingerprint(),
                        )
                        .await
                        .unwrap();
                        let intended = if case == "wrong-identity" {
                            b.fingerprint()
                        } else {
                            a.fingerprint()
                        };
                        send_frame(&mut send, PairFrameKind::Hello, b.fingerprint(), intended)
                            .await
                            .unwrap();
                        if case == "trailing" {
                            send_frame(
                                &mut send,
                                PairFrameKind::Saved,
                                b.fingerprint(),
                                a.fingerprint(),
                            )
                            .await
                            .unwrap();
                            send.write_all(b"extra").await.unwrap();
                        }
                        send.finish().unwrap();
                        let _ = send.stopped().await;
                    }
                );
                assert!(result.is_err(), "{case}");
                assert_eq!(saved.get(), case != "wrong-identity");
                server.close(0_u32.into(), b"fixture ended");
            }
        })
        .await
        .expect("negative fixture exceeded bound");
    }
}
