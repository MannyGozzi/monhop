//! Serializes explicit setup actions without holding a lock across native prompts, and keeps
//! the active computer connected.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime},
};

use monhop_transport::{crypto::CertificateFingerprint, session_setup::InspectedPeer};

use crate::{
    arrangement_library::ArrangementLibrary,
    computers::{ComputerList, ComputersView, LiveInspection},
    pairing::{PairingController, PairingView},
    sharing::{
        ARRANGEMENTS_FILE, DisplayNotice, LayoutRequest, SharingController, SharingView,
        current_local_displays,
    },
    sharing_preferences::{
        SetupFile, SharingPreferences, adapt_to_inspection, fingerprint_key, parse_fingerprint,
    },
};

#[derive(Default)]
struct GateState {
    action_running: bool,
    shutting_down: bool,
    updating: bool,
}

#[derive(Default)]
struct ActionGate(Mutex<GateState>);

struct ActionLease<'a>(&'a ActionGate);

pub struct UpdateLease(Arc<ActionGate>);

impl ActionGate {
    fn begin(&self) -> Result<ActionLease<'_>, String> {
        let mut state = lock(&self.0);
        if state.shutting_down {
            return Err("MonHop is closing. Finish any open system prompt.".into());
        }
        if state.action_running {
            return Err("Finish the current setup action before starting another.".into());
        }
        if state.updating {
            return Err(
                "An update is in progress. Wait for it to finish before starting sharing.".into(),
            );
        }
        state.action_running = true;
        Ok(ActionLease(self))
    }

    fn request_shutdown(&self) -> bool {
        let mut state = lock(&self.0);
        let first = !state.shutting_down;
        state.shutting_down = true;
        first
    }

    fn is_drained(&self) -> bool {
        let state = lock(&self.0);
        state.shutting_down && !state.action_running
    }
}

impl Drop for UpdateLease {
    fn drop(&mut self) {
        lock(&self.0.0).updating = false;
    }
}

impl Drop for ActionLease<'_> {
    fn drop(&mut self) {
        lock(&self.0.0).action_running = false;
    }
}

/// The setup file's parse, valid as long as its modification time and length are unchanged; every
/// writer replaces the file through a temp-file rename, so a stale stamp can never hide an edit.
struct CachedSetup {
    stamp: (SystemTime, u64),
    file: SetupFile,
}

#[derive(Default)]
pub struct AppController {
    pub pairing: Arc<PairingController>,
    pub sharing: Arc<SharingController>,
    pub trial: Arc<crate::trial::TrialController>,
    gate: Arc<ActionGate>,
    /// Resolved once at startup so actions with fixed signatures can still reach the setup file.
    setup_path: Mutex<Option<PathBuf>>,
    /// A failed start waits this long before the supervisor tries again.
    sharing_retry_after: Mutex<Option<Instant>>,
    /// The supervisor's last parse of the setup file; see `cached_setup`.
    setup_cache: Mutex<Option<CachedSetup>>,
}

/// The sharing session and the setup link share one port, so a link waits for the session to stop.
const SESSION_PAUSE_WINDOW: Duration = Duration::from_secs(3);
const SUPERVISOR_BACKOFF: Duration = Duration::from_secs(10);
const PAIRING_HOLDS_PORT: &str = "Pairing in progress. Sharing resumes afterwards.";

impl AppController {
    /// The same gate reserves sharing starts and update work, including across async downloads.
    pub fn begin_update(&self, installing: bool) -> Result<UpdateLease, String> {
        let mut state = lock(&self.gate.0);
        if state.action_running || state.updating || (state.shutting_down && !installing) {
            return Err("Finish the current action before updating MonHop.".into());
        }
        if !self.sharing.shutdown_ready()
            || !self.pairing.shutdown_ready()
            || self.pairing.occupies_port()
            || self.trial.is_open()
        {
            return Err(
                "Pause sharing and finish pairing before checking or installing updates.".into(),
            );
        }
        state.updating = true;
        Ok(UpdateLease(self.gate.clone()))
    }

    /// Trust changes use the sharing port, so the live connection ends first; the supervisor
    /// reconnects once the exchange is over.
    pub fn pairing_action(
        &self,
        action: impl FnOnce(&PairingController) -> Result<PairingView, String>,
    ) -> Result<PairingView, String> {
        let _lease = self.gate.begin()?;
        self.release_port_for_pairing()?;
        action(&self.pairing)
    }

    /// A code is only inspected while the list has room, so no pairing can end as a trust
    /// record without a card.
    pub fn pairing_inspect(&self, code: String) -> Result<PairingView, String> {
        let _lease = self.gate.begin()?;
        ComputerList::load(&self.list_path()?)?.require_room()?;
        self.release_port_for_pairing()?;
        self.pairing.inspect(code)
    }

    fn release_port_for_pairing(&self) -> Result<(), String> {
        self.quiesce_connection(PAIRING_HOLDS_PORT)?;
        self.sharing.invalidate_if_idle()
    }

    /// Reading the saved pairing changes no trust, so a running connection does not block it.
    pub fn pairing_read_action(
        &self,
        action: impl FnOnce(&PairingController) -> Result<PairingView, String>,
    ) -> Result<PairingView, String> {
        let _lease = self.gate.begin()?;
        action(&self.pairing)
    }

    pub fn use_setup_path(&self, path: PathBuf) {
        *lock(&self.setup_path) = Some(path);
    }

    fn setup_path(&self) -> Result<PathBuf, String> {
        lock(&self.setup_path)
            .clone()
            .ok_or_else(|| "The setup folder could not be located.".to_owned())
    }

    fn list_path(&self) -> Result<PathBuf, String> {
        Ok(self.setup_path()?.with_file_name("dashboard.json"))
    }

    /// Wraps `SetupFile::load` with the message a caller wants for an unreadable file.
    fn load_setup(&self, path: &Path, unreadable_message: &str) -> Result<SetupFile, String> {
        SetupFile::load(path).map_err(|_| unreadable_message.to_owned())
    }

    /// Like `load_setup`, but skips the parse when the file's modification stamp still matches
    /// the last parse. Used only by the once-a-second supervisor tick, which reloads the file
    /// even when nothing changed.
    fn cached_setup(&self, path: &Path, unreadable_message: &str) -> Result<SetupFile, String> {
        let stamp = std::fs::metadata(path).ok().and_then(|metadata| {
            metadata
                .modified()
                .ok()
                .map(|modified| (modified, metadata.len()))
        });
        if let Some(stamp) = stamp {
            let cached = lock(&self.setup_cache)
                .as_ref()
                .filter(|cached| cached.stamp == stamp)
                .map(|cached| cached.file.clone());
            if let Some(file) = cached {
                return Ok(file);
            }
        }
        let file = self.load_setup(path, unreadable_message)?;
        *lock(&self.setup_cache) = stamp.map(|stamp| CachedSetup {
            stamp,
            file: file.clone(),
        });
        Ok(file)
    }

    /// Ends a live link or session and waits briefly for the port; `message` is what the view
    /// says once input is local.
    fn quiesce_connection(&self, message: &str) -> Result<(), String> {
        if self.sharing.live_peer().is_none() && self.sharing.link_is_off() {
            return Ok(());
        }
        self.sharing.stop_with(message);
        let deadline = Instant::now() + SESSION_PAUSE_WINDOW;
        while !self.sharing.shutdown_ready() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if !self.sharing.shutdown_ready() {
            return Err("Sharing is still stopping. Try again in a moment.".into());
        }
        Ok(())
    }

    pub fn computers_load(&self) -> Result<ComputersView, String> {
        let _lease = self.gate.begin()?;
        self.adopt_completed_pairings()?;
        self.computers_view()
    }

    pub fn computers_rename(&self, fingerprint: &str, name: &str) -> Result<ComputersView, String> {
        let _lease = self.gate.begin()?;
        let fingerprint = parse_fingerprint(fingerprint)?;
        let path = self.list_path()?;
        let mut list = ComputerList::load(&path)?;
        list.rename(fingerprint, name)?;
        list.save(&path)?;
        self.computers_view()
    }

    /// Removes the trust record, the saved layout, and the list entry; the active computer
    /// is paused first so no session survives its own pairing.
    pub fn forget_computer(&self, fingerprint: &str) -> Result<ComputersView, String> {
        let _lease = self.gate.begin()?;
        let fingerprint = parse_fingerprint(fingerprint)?;
        let setup_path = self.setup_path()?;
        let list_path = self.list_path()?;
        let file = self.load_setup(&setup_path, "The saved setup could not be read.")?;
        log::info!("user: forgot computer {}", short(fingerprint));
        if file.active() == Some(fingerprint_key(&fingerprint.full_hex()).as_str()) {
            self.sharing.set_active(&setup_path, None)?;
            self.quiesce_connection(crate::sharing::PAUSED)?;
        }
        self.pairing.forget(fingerprint)?;
        self.sharing
            .update_setup_file(&setup_path, |file| file.remove(&fingerprint.full_hex()))?;
        let mut list = ComputerList::load(&list_path)?;
        list.forget(fingerprint);
        list.save(&list_path)?;
        crate::autostart::computer_forgotten(&setup_path);
        self.computers_view()
    }

    /// Chooses the computer to share with; None pauses. The supervisor connects within a second.
    pub fn set_active(&self, fingerprint: Option<&str>) -> Result<SharingView, String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() {
            return Err("Finish or cancel pairing before changing computers.".into());
        }
        let fingerprint = fingerprint.map(parse_fingerprint).transpose()?;
        match fingerprint {
            Some(fingerprint) => log::info!("user: chose computer {}", short(fingerprint)),
            None => log::info!("user: paused sharing"),
        }
        let path = self.setup_path()?;
        let view = self.sharing.set_active(&path, fingerprint)?;
        *lock(&self.sharing_retry_after) = None;
        Ok(view)
    }

    /// Makes the computer active and opens the setup link for arranging; a session with it
    /// ends first, a link already up with it is kept.
    pub fn edit_begin(
        &self,
        interface_id: String,
        fingerprint: &str,
    ) -> Result<SharingView, String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() || self.pairing.occupies_port() {
            return Err("Finish or cancel pairing before arranging displays.".into());
        }
        if self.trial.is_open() {
            return Err("Close the test window before arranging displays.".into());
        }
        let fingerprint = parse_fingerprint(fingerprint)?;
        let path = self.setup_path()?;
        log::info!("user: started arranging with {}", short(fingerprint));
        self.sharing.update_setup_file(&path, |file| {
            file.set_active(Some(&fingerprint));
            file.set_interface_id(&interface_id);
        })?;
        *lock(&self.sharing_retry_after) = None;
        if self.sharing.live_peer() == Some(fingerprint) && !self.sharing.link_is_off() {
            return Ok(self.sharing.begin_editing());
        }
        self.quiesce_connection("Opening the link for arranging.")?;
        self.sharing.connect_link(path, interface_id, fingerprint)?;
        Ok(self.sharing.begin_editing())
    }

    /// Ends arranging; the supervisor resumes the session, or a standing link without a layout.
    pub fn edit_end(&self) -> Result<SharingView, String> {
        let _lease = self.gate.begin()?;
        log::info!("user: ended arranging");
        *lock(&self.sharing_retry_after) = None;
        Ok(self.sharing.end_editing())
    }

    pub fn apply_setup(
        &self,
        revision: String,
        source: String,
        layout: LayoutRequest,
    ) -> Result<SharingView, String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() {
            return Err("Finish or cancel pairing before applying a layout.".into());
        }
        if self.trial.is_open() {
            return Err("Close the test window before applying a layout.".into());
        }
        log::info!("user: applied a layout");
        self.sharing.apply_setup(&revision, &source, layout)
    }

    /// Runs every second: keeps the active computer connected while nothing else owns the port.
    pub fn supervise_sharing(&self) {
        let Ok(_lease) = self.gate.begin() else {
            return;
        };
        if let Err(message) = self.adopt_completed_pairings() {
            log::warn!("supervisor: could not record a completed pairing: {message}");
        }
        if lock(&self.sharing_retry_after).is_some_and(|until| Instant::now() < until) {
            return;
        }
        let Ok(path) = self.setup_path() else {
            return;
        };
        match self.keep_connected(&path) {
            Ok(Some(started)) => log::info!("supervisor: {started}"),
            Ok(None) => {}
            Err(message) => {
                log::warn!("supervisor: could not connect: {message}");
                *lock(&self.sharing_retry_after) = Some(Instant::now() + SUPERVISOR_BACKOFF);
                self.sharing.note_supervisor_failure(message);
            }
        }
    }

    /// Ok(None) means nothing to do: no active computer, or another action owns input or the port.
    /// A saved layout that fits both computers' displays runs as a session; anything else keeps a
    /// standing link so the displays can be arranged.
    fn keep_connected(&self, path: &Path) -> Result<Option<&'static str>, String> {
        if self.trial.is_open() || !self.pairing.shutdown_ready() || self.pairing.occupies_port() {
            return Ok(None);
        }
        let file = self.cached_setup(
            path,
            "The saved setup could not be read. Apply a layout again.",
        )?;
        self.sharing.adopt_saved(&file);
        let Some(active) = file.active() else {
            return Ok(None);
        };
        let peer = parse_fingerprint(active)?;
        let saved = file.computer(active);
        if self.sharing.live_peer().is_some() {
            if saved.is_some_and(|saved| self.sharing.yield_link_when_layout_fits(saved)) {
                return Ok(Some(
                    "the displays fit the saved layout again; ending the link",
                ));
            }
            let Some((saved, inspection)) = saved.zip(self.sharing.inspection_for_switch()) else {
                return Ok(None);
            };
            return self.switch_or_continue(path, saved, &inspection);
        }
        if !self.sharing.shutdown_ready() || !self.sharing.link_is_off() {
            return Ok(None);
        }
        let candidate = saved.filter(|_| !self.sharing.editing() && !self.sharing.holds_link());
        let current = candidate.map(current_local_displays).transpose()?;
        if let Some((saved, current)) = candidate.zip(current.as_ref()) {
            if saved.matches_local_displays(current) {
                // A layout applied before memories existed is remembered from its first session.
                crate::sharing::remember_applied(path, saved);
                self.sharing.start_sharing(saved.clone())?;
                return Ok(Some("started sharing with the active computer"));
            }
            if let Some(remembered) = self.remembered_for_local_displays(path, saved, current) {
                self.sharing.write_active_setup(path, remembered.clone())?;
                *lock(&self.setup_cache) = None;
                self.sharing.start_sharing(remembered)?;
                return Ok(Some(
                    "started sharing with the arrangement remembered for these displays",
                ));
            }
        }
        let interface_id = file
            .interface_id()
            .ok_or("Choose the physical network and pair the computer again.")?
            .to_owned();
        self.sharing
            .connect_link(path.to_path_buf(), interface_id, peer)?;
        Ok(Some(if self.sharing.editing() {
            "reopened the link for arranging"
        } else {
            "opened a standing link; no layout fits yet"
        }))
    }

    /// The active record no longer fits the connected displays: switch to the arrangement
    /// remembered for them, keep sharing with what survives of this one, or wait and say so.
    /// The library is read only here, never while the applied layout still fits.
    fn switch_or_continue(
        &self,
        path: &Path,
        saved: &SharingPreferences,
        inspection: &InspectedPeer,
    ) -> Result<Option<&'static str>, String> {
        if self.sharing.notice_raised_for(inspection) {
            return Ok(None);
        }
        let library = self.library(path);
        if let Some(remembered) = library
            .as_ref()
            .and_then(|library| library.automatic_fit(inspection))
        {
            self.sharing.write_active_setup(path, remembered.clone())?;
            *lock(&self.setup_cache) = None;
            // A reconnected monitor carries a new id; the memory takes it so it fits exactly next time.
            crate::sharing::remember_applied(path, &remembered);
            self.sharing.yield_link_when_layout_fits(&remembered);
            return Ok(Some(
                "switched to the arrangement remembered for these displays",
            ));
        }
        // Rebuilt from the memory made with exactly these monitors when there is one, so a
        // display that only moved keeps every crossing; the record that was running is the
        // fallback, so a memory that no longer validates never costs what still works.
        let adapted = library
            .as_ref()
            .and_then(|library| library.automatic_for_same_displays(inspection))
            .and_then(|basis| adapt_to_inspection(basis, inspection))
            .or_else(|| adapt_to_inspection(saved, inspection));
        let Some(adapted) = adapted else {
            self.sharing
                .raise_display_notice(DisplayNotice::Waiting, inspection);
            return Ok(None);
        };
        self.sharing.write_active_setup(path, adapted.clone())?;
        *lock(&self.setup_cache) = None;
        crate::sharing::remember_applied(path, &adapted);
        self.sharing
            .raise_display_notice(DisplayNotice::Continued, inspection);
        self.sharing.yield_link_when_layout_fits(&adapted);
        Ok(Some("kept sharing with the displays that are still there"))
    }

    /// An arrangement remembered for the displays this computer shows now; the other computer's
    /// displays are checked when the session connects.
    fn remembered_for_local_displays(
        &self,
        path: &Path,
        saved: &SharingPreferences,
        current: &monhop_transport::session_setup::DisplayTopology,
    ) -> Option<SharingPreferences> {
        self.library(path)
            .and_then(|library| library.automatic_for_local(saved, current))
    }

    fn library(&self, path: &Path) -> Option<ArrangementLibrary> {
        match ArrangementLibrary::load(&path.with_file_name(ARRANGEMENTS_FILE)) {
            Ok(library) => Some(library),
            Err(_) => {
                log::warn!("supervisor: the saved arrangements could not be read");
                None
            }
        }
    }

    /// Lists each newly paired computer and makes it active. A failed write hands the pairings
    /// back so the next tick records them; a full list drops the entry, and the trust record
    /// still shows as a computer so it can be forgotten.
    fn adopt_completed_pairings(&self) -> Result<(), String> {
        let completed = self.pairing.take_completed();
        if completed.is_empty() {
            return Ok(());
        }
        let result = (|| {
            let setup_path = self.setup_path()?;
            let list_path = self.list_path()?;
            let mut list = ComputerList::load(&list_path)?;
            for pairing in &completed {
                match list.remember(pairing.fingerprint, pairing.address, pairing.platform) {
                    Ok(()) => log::info!(
                        "pairing: {} paired and made active",
                        short(pairing.fingerprint)
                    ),
                    Err(message) => log::warn!(
                        "pairing: {} paired but not listed: {message}",
                        short(pairing.fingerprint)
                    ),
                }
            }
            list.save(&list_path)?;
            self.sharing.update_setup_file(&setup_path, |file| {
                for pairing in &completed {
                    file.set_active(Some(&pairing.fingerprint));
                    file.set_interface_id(&pairing.interface_id);
                }
            })?;
            *lock(&self.sharing_retry_after) = None;
            Ok(())
        })();
        if result.is_err() {
            self.pairing.requeue_completed(completed);
        }
        result
    }

    fn computers_view(&self) -> Result<ComputersView, String> {
        let file = self.load_setup(&self.setup_path()?, "The saved setup could not be read.")?;
        let list = ComputerList::load(&self.list_path()?)?;
        let live = self.sharing.live_inspection();
        let live = live
            .as_ref()
            .map(|(fingerprint, inspection)| LiveInspection {
                fingerprint,
                inspection,
            });
        Ok(ComputersView::assemble(
            &list,
            &self.pairing.paired_peers(),
            &file,
            live,
            &self.sharing.revision(),
        ))
    }

    pub fn select_source(&self, revision: &str, source: &str) -> Result<SharingView, String> {
        let _lease = self.gate.begin()?;
        if self.trial.is_open() || !self.pairing.shutdown_ready() {
            return Err("Finish the current connection before changing the input computer.".into());
        }
        self.sharing.select_source(revision, source)
    }

    /// Resolves the setup file for callers that hold only the ticket.
    pub fn validate_trial_setup(
        &self,
        revision: &str,
        layout: &LayoutRequest,
    ) -> Result<(), String> {
        let path = self.setup_path()?;
        self.sharing
            .validate_trial_setup(&path, revision, layout)
            .map(|_| ())
    }

    pub fn prepare_trial(
        &self,
        revision: String,
        layout: LayoutRequest,
        peer_name: String,
    ) -> Result<(), String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() || !self.sharing.shutdown_ready() {
            return Err("Finish the current connection before opening a test window.".into());
        }
        if !self.sharing.link_is_off() {
            return Err("Stop the connection before opening the test window.".into());
        }
        self.validate_trial_setup(&revision, &layout)?;
        self.trial.prepare(revision, layout, peer_name)
    }

    pub fn start_trial(
        &self,
        revision: String,
        layout: LayoutRequest,
        authorization: monhop_transport::session_trial::TrialAuthorization,
    ) -> Result<SharingView, String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() {
            return Err("Finish pairing before starting the test.".into());
        }
        let path = self.setup_path()?;
        self.sharing
            .enable_trial(&path, revision, layout, authorization)
    }

    pub(crate) fn metadata_action<T>(
        &self,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _lease = self.gate.begin()?;
        action()
    }

    pub fn save_setup(
        &self,
        path: &Path,
        revision: &str,
        layout: LayoutRequest,
    ) -> Result<crate::sharing_preferences::SavedSetupView, String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() {
            return Err("Finish or cancel pairing before saving a sharing setup.".into());
        }
        self.sharing.save_setup(path, revision, layout)
    }

    pub fn arrangements(
        &self,
        path: &Path,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
        let _lease = self.gate.begin()?;
        self.sharing.arrangements(path)
    }

    pub fn save_arrangement(
        &self,
        path: &Path,
        revision: &str,
        name: &str,
        layout: LayoutRequest,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() {
            return Err("Finish or cancel pairing before saving an arrangement.".into());
        }
        self.sharing.save_arrangement(path, revision, name, layout)
    }

    pub fn delete_arrangement(
        &self,
        path: &Path,
        name: &str,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
        let _lease = self.gate.begin()?;
        self.sharing.delete_arrangement(path, name)
    }

    pub fn request_shutdown(&self) -> bool {
        let first = self.gate.request_shutdown();
        self.trial.revoke();
        self.pairing.request_shutdown();
        self.sharing.request_shutdown();
        first
    }

    pub fn shutdown_ready(&self) -> bool {
        self.gate.is_drained() && self.pairing.shutdown_ready() && self.sharing.shutdown_ready()
    }

    /// Requests shutdown and waits, bounded, until nothing holds input or the port: the way out.
    pub fn drain_for_exit(&self) {
        self.request_shutdown();
        let started = Instant::now();
        while !self.shutdown_ready() && started.elapsed() < EXIT_DRAIN_LIMIT {
            std::thread::sleep(Duration::from_millis(20));
        }
        let elapsed = started.elapsed().as_millis();
        if self.shutdown_ready() {
            log::info!("lifecycle: drained for exit in {elapsed} ms");
        } else {
            log::warn!("lifecycle: exit drain still busy after {elapsed} ms");
        }
    }
}

/// How long a quit waits for held input and sockets to let go before the process ends anyway.
const EXIT_DRAIN_LIMIT: Duration = Duration::from_secs(5);

/// The first hex digits of a fingerprint: enough to tell computers apart in a log line.
fn short(fingerprint: CertificateFingerprint) -> String {
    fingerprint.full_hex()[..8].to_ascii_lowercase()
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
    fn actions_are_exclusive_and_shutdown_does_not_wait_for_a_prompt() {
        let gate = ActionGate::default();
        let action = gate.begin().unwrap();
        assert!(gate.begin().is_err());
        assert!(gate.request_shutdown());
        assert!(!gate.request_shutdown());
        assert!(!gate.is_drained());
        drop(action);
        assert!(gate.is_drained());
        assert!(gate.begin().is_err());
    }

    #[test]
    fn completed_actions_release_the_gate() {
        let gate = ActionGate::default();
        drop(gate.begin().unwrap());
        assert!(gate.begin().is_ok());
    }

    #[test]
    fn the_supervisor_is_inert_without_a_setup_folder_or_an_active_computer() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        controller.supervise_sharing();
        assert_eq!(controller.sharing.status().phase, "off");
        let folder = std::env::temp_dir().join(format!("monhop-supervisor-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        controller.use_setup_path(folder.join("sharing.json"));
        controller.supervise_sharing();
        let view = controller.sharing.status();
        assert_eq!(view.phase, "off");
        assert!(view.message.contains("Input is local"));
        assert!(view.active.is_none());
        assert!(view.peer_fingerprint.is_none());
        assert!(!folder.join("sharing.json").exists());
        let computers = serde_json::to_value(controller.computers_load().unwrap()).unwrap();
        assert_eq!(computers["computers"], serde_json::json!([]));
        assert!(computers["active"].is_null());
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn choosing_and_pausing_a_computer_is_saved_and_shown_without_a_connection() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let folder = std::env::temp_dir().join(format!("monhop-active-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        controller.use_setup_path(folder.join("sharing.json"));
        assert!(controller.set_active(Some("not a fingerprint")).is_err());
        let chosen = controller.set_active(Some(&"B".repeat(64))).unwrap();
        assert_eq!(chosen.active.as_deref(), Some("b".repeat(64).as_str()));
        assert_eq!(
            SetupFile::load(&folder.join("sharing.json"))
                .unwrap()
                .active(),
            Some("b".repeat(64).as_str())
        );
        let paused = controller.set_active(None).unwrap();
        assert!(paused.active.is_none());
        assert_eq!(paused.phase, "off");
        assert_eq!(paused.message, crate::sharing::PAUSED);
        assert!(!paused.editing);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn shutdown_is_permanent_and_inert_when_no_action_started() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        assert!(!controller.shutdown_ready());
        assert!(controller.request_shutdown());
        assert!(controller.shutdown_ready());
        assert!(
            controller
                .pairing_action(|_| panic!("must not run"))
                .is_err()
        );
        assert!(
            controller
                .edit_begin("invalid".into(), &"B".repeat(64))
                .is_err()
        );
        assert!(controller.set_active(None).is_err());
    }

    #[test]
    fn metadata_updates_drain_before_exit_and_cannot_restart_after_quit() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        controller
            .metadata_action(|| {
                assert!(controller.request_shutdown());
                assert!(!controller.shutdown_ready());
                Ok(())
            })
            .unwrap();
        assert!(controller.shutdown_ready());
        assert!(
            controller
                .metadata_action(|| -> Result<(), String> { panic!("must not write after quit") })
                .is_err()
        );
    }

    #[test]
    fn arranging_needs_a_resolved_setup_folder_and_leaves_no_worker() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        assert_eq!(
            controller
                .edit_begin("unused-network".into(), &"B".repeat(64))
                .err(),
            Some("The setup folder could not be located.".to_owned())
        );
        assert!(controller.edit_end().is_ok());
        controller.use_setup_path(std::env::temp_dir().join("monhop-lifecycle-unused.json"));
        controller.sharing.stop();
        assert!(controller.sharing.shutdown_ready());
    }

    #[test]
    fn retained_input_cleanup_blocks_pairing_mutations_and_exit() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let ownership = monhop_core::NativeInputOwnership::claim().unwrap();
        assert!(
            controller
                .pairing_action(|_| panic!("must not mutate trust"))
                .is_err()
        );
        controller.request_shutdown();
        assert!(!controller.shutdown_ready());
        drop(ownership);
        assert!(controller.shutdown_ready());
    }
    #[test]
    fn update_work_and_sharing_actions_exclude_each_other_in_both_orders() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let update = controller.begin_update(false).unwrap();
        assert!(
            controller
                .metadata_action(|| -> Result<(), String> {
                    panic!("action cannot start during update work")
                })
                .is_err()
        );
        assert!(controller.begin_update(true).is_err());
        drop(update);
        controller
            .metadata_action(|| {
                assert!(controller.begin_update(false).is_err());
                assert!(controller.begin_update(true).is_err());
                Ok(())
            })
            .unwrap();
        assert!(controller.begin_update(false).is_ok());
    }

    #[test]
    fn native_cleanup_blocks_checks_even_when_the_view_is_not_sharing() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let ownership = monhop_core::NativeInputOwnership::claim().unwrap();
        assert!(!controller.sharing.status().sharing_active);
        assert!(controller.begin_update(false).is_err());
        assert!(controller.begin_update(true).is_err());
        drop(ownership);
        assert!(controller.begin_update(false).is_ok());
    }

    #[test]
    fn shutdown_allows_installation_but_never_new_network_requests() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        controller.request_shutdown();
        assert!(controller.begin_update(false).is_err());
        let install = controller.begin_update(true).unwrap();
        assert!(controller.gate.begin().is_err());
        drop(install);
        assert!(controller.shutdown_ready());
    }
}
