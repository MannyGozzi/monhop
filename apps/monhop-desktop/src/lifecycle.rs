//! Serializes explicit setup actions without holding a lock across native prompts, and keeps
//! the active computer connected.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime},
};

use monhop_transport::{
    crypto::CertificateFingerprint,
    session_setup::{DisplayTopology, InspectedPeer},
};

use crate::{
    arrangement_library::ArrangementLibrary,
    computers::{ComputerList, ComputersView, LiveInspection},
    pairing::{PairingController, PairingView},
    sharing::{
        ARRANGEMENTS_FILE, DisplayNotice, LayoutRequest, SharingController, SharingView,
        current_local_displays, still_unsettled,
    },
    sharing_preferences::{
        DisplayGeometry, SetupFile, SharingPreferences, adapt_to_inspection, fingerprint_key,
        local_decides, parse_fingerprint,
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

/// This computer's displays as `watch_local_displays` last read them, and when it last tried.
#[derive(Default)]
struct DisplaysWatch {
    read_at: Option<Instant>,
    displays: Option<DisplayTopology>,
}

#[derive(Default)]
pub struct AppController {
    pub pairing: Arc<PairingController>,
    pub sharing: Arc<SharingController>,
    gate: Arc<ActionGate>,
    /// Resolved once at startup so actions with fixed signatures can still reach the setup file.
    setup_path: Mutex<Option<PathBuf>>,
    /// A failed start waits this long before the supervisor tries again.
    sharing_retry_after: Mutex<Option<Instant>>,
    /// The supervisor's last parse of the setup file; see `cached_setup`.
    setup_cache: Mutex<Option<CachedSetup>>,
    /// When this computer's displays first failed to read in a row; see `local_displays_fit`.
    displays_unreadable_since: Mutex<Option<Instant>>,
    displays_watch: Mutex<DisplaysWatch>,
}

/// The sharing session and the setup link share one port, so a link waits for the session to stop.
const SESSION_PAUSE_WINDOW: Duration = Duration::from_secs(3);
const SUPERVISOR_BACKOFF: Duration = Duration::from_secs(10);
/// With nothing else reporting them, this computer's displays are read at most this often; with
/// the window's one-second poll, a change reaches its cards within about two seconds.
const DISPLAYS_WATCH: Duration = Duration::from_secs(1);
const PAIRING_HOLDS_PORT: &str = "Pairing in progress. Sharing resumes afterwards.";
const SETUP_UNREADABLE: &str = "The saved setup could not be read. Apply a layout again.";

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
    /// the last parse. Used only by the supervisor tick, which reloads the file on every pass
    /// even when nothing changed. Correctness rests on the temp-file rename moving the
    /// modification time: an edit that left the stamp alone would be invisible here.
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

    /// Only adopting a finished pairing writes, so only that needs the gate, and a busy gate leaves
    /// it to the supervisor's next pass. The view reads files every writer replaces by rename, so a
    /// pass or an action holding the gate never fails it.
    pub fn computers_load(&self) -> Result<ComputersView, String> {
        if let Ok(_lease) = self.gate.begin() {
            self.adopt_completed_pairings()?;
        }
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
            self.sharing.set_active(&setup_path, None, None)?;
            self.quiesce_connection(crate::sharing::PAUSED)?;
        }
        self.pairing.forget(fingerprint)?;
        // The list goes first: the setup write bumps the revision a reload keys on.
        let mut list = ComputerList::load(&list_path)?;
        list.forget(fingerprint);
        list.save(&list_path)?;
        self.sharing
            .update_setup_file(&setup_path, |file| file.remove(&fingerprint.full_hex()))?;
        crate::autostart::computer_forgotten(&setup_path);
        self.computers_view()
    }

    /// Chooses the computer to share with; None pauses. A supervisor pass runs immediately; the
    /// connection itself follows within a tick, because the worker for the old computer has to
    /// stop before the port is free.
    pub fn set_active(
        &self,
        fingerprint: Option<&str>,
        interface_id: Option<&str>,
    ) -> Result<SharingView, String> {
        let view = {
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
            let view = self.sharing.set_active(&path, fingerprint, interface_id)?;
            *lock(&self.sharing_retry_after) = None;
            view
        };
        self.nudge();
        Ok(view)
    }

    /// One supervisor pass, right after a change that makes a different connection the correct
    /// one. The pass runs immediately, but the connection it wants may still be blocked by the
    /// previous worker stopping, so it lands on one of the next ticks rather than at once.
    /// The caller must have released the action gate first: a pass that finds it held does
    /// nothing, and a pass never nudges, so this can neither block nor recurse.
    pub fn nudge(&self) {
        // A user action is behind every nudge, so a failed worker's backoff is spent.
        self.sharing.clear_failure_backoff();
        self.supervise_sharing();
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
        let fingerprint = parse_fingerprint(fingerprint)?;
        let path = self.setup_path()?;
        log::info!("user: started arranging with {}", short(fingerprint));
        self.sharing.update_setup_file(&path, |file| {
            file.set_active(Some(&fingerprint));
            file.set_interface_id(&interface_id);
        })?;
        *lock(&self.sharing_retry_after) = None;
        self.sharing.clear_failure_backoff();
        if self.sharing.live_peer() == Some(fingerprint) && !self.sharing.link_is_off() {
            return Ok(self.sharing.begin_editing());
        }
        self.quiesce_connection("Opening the link for arranging.")?;
        self.sharing.connect_link(path, interface_id, fingerprint)?;
        Ok(self.sharing.begin_editing())
    }

    /// Ends arranging. The arranging link closes first, so the session, or a standing link when
    /// no layout fits, follows within a tick rather than at once.
    pub fn edit_end(&self) -> Result<SharingView, String> {
        let view = {
            let _lease = self.gate.begin()?;
            log::info!("user: ended arranging");
            *lock(&self.sharing_retry_after) = None;
            self.sharing.end_editing()
        };
        self.nudge();
        Ok(view)
    }

    pub fn apply_setup(
        &self,
        revision: String,
        layout: LayoutRequest,
    ) -> Result<SharingView, String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() {
            return Err("Finish or cancel pairing before applying a layout.".into());
        }
        log::info!("user: applied a layout");
        self.sharing.apply_setup(&revision, layout)
    }

    /// Flips one of the two switches for the active computer. The session ends on the next
    /// pass and the change travels over the link to both computers' records.
    pub fn set_control(
        &self,
        fingerprint: &str,
        direction: &str,
        allowed: bool,
    ) -> Result<SharingView, String> {
        {
            let _lease = self.gate.begin()?;
            if !self.pairing.shutdown_ready() {
                return Err(
                    "Finish or cancel pairing before changing who can control which computer."
                        .into(),
                );
            }
            let path = self.setup_path()?;
            self.sharing
                .set_control(&path, fingerprint, direction, allowed)?;
            log::info!(
                "user: {} {direction}",
                if allowed { "allowed" } else { "turned off" }
            );
        }
        self.nudge();
        Ok(self.sharing.status())
    }

    /// Runs on every supervisor tick: keeps the active computer connected while nothing else
    /// owns the port.
    pub fn supervise_sharing(&self) {
        let Ok(_lease) = self.gate.begin() else {
            return;
        };
        if let Err(message) = self.adopt_completed_pairings() {
            log::warn!("supervisor: could not record a completed pairing: {message}");
        }
        let Ok(path) = self.setup_path() else {
            return;
        };
        self.watch_local_displays(&path);
        if lock(&self.sharing_retry_after).is_some_and(|until| Instant::now() < until) {
            return;
        }
        match self.keep_connected(&path) {
            Ok(Some(transition)) => log::info!("supervisor: {transition}"),
            Ok(None) => {}
            Err(message) => {
                log::warn!("supervisor: could not connect: {message}");
                *lock(&self.sharing_retry_after) = Some(Instant::now() + SUPERVISOR_BACKOFF);
                self.sharing.note_supervisor_failure(message);
            }
        }
    }

    /// Ok(None) means nothing to do: no active computer, or another action owns input or the port.
    /// A record that fits this computer's displays runs as a session; anything else keeps a link,
    /// on which the decider picks the record both computers hold. Ok(Some) is the log line.
    fn keep_connected(&self, path: &Path) -> Result<Option<String>, String> {
        if !self.pairing.shutdown_ready() || self.pairing.occupies_port() {
            return Ok(None);
        }
        let file = self.cached_setup(path, SETUP_UNREADABLE)?;
        self.sharing.adopt_saved(&file);
        let Some(active) = file.active() else {
            return Ok(None);
        };
        let peer = parse_fingerprint(active)?;
        let saved = file.computer(active);
        if self.sharing.live_peer().is_some() {
            if self.sharing.end_session_for_control() {
                return Ok(Some(transition(
                    "ending the session to change who can control which computer",
                    saved,
                    None,
                )));
            }
            let Some((saved, inspection)) = saved.zip(self.sharing.inspection_for_switch()) else {
                return Ok(None);
            };
            return self.switch_or_continue(path, saved, &inspection);
        }
        if !self.sharing.shutdown_ready() || !self.sharing.link_is_off() {
            return Ok(None);
        }
        // A worker that ended with a real failure waits out its backoff here; relaunching it
        // every pass would only repeat the failure. Any user choice clears it.
        if self.sharing.within_failure_backoff(SUPERVISOR_BACKOFF) {
            return Ok(None);
        }
        if let Some(saved) = saved.filter(|_| !self.sharing.editing() && !self.sharing.holds_link())
        {
            match self.local_displays_fit(saved)? {
                None => return Ok(None),
                Some(true) => {
                    self.sharing.start_sharing(saved.clone())?;
                    return Ok(Some(transition(
                        "started sharing with the active computer",
                        Some(saved),
                        None,
                    )));
                }
                // Never promoted here: only the decider picks a record, and only on the link.
                Some(false) => self.sharing.note_local_misfit(),
            }
        }
        let interface_id = file
            .interface_id()
            .ok_or("Choose the physical network and pair the computer again.")?
            .to_owned();
        self.sharing
            .connect_link(path.to_path_buf(), interface_id, peer)?;
        Ok(Some(transition(
            if self.sharing.editing() {
                "reopened the link for arranging"
            } else {
                "opened a standing link"
            },
            saved,
            None,
        )))
    }

    /// Whether this computer still shows the record's displays. None while a read that failed
    /// mid-change is retried quietly; past the unsettled window it is a failure again.
    fn local_displays_fit(&self, saved: &SharingPreferences) -> Result<Option<bool>, String> {
        let mut since = lock(&self.displays_unreadable_since);
        match current_local_displays(saved) {
            Ok(current) => {
                *since = None;
                Ok(Some(saved.matches_local_displays(&current)))
            }
            Err(_) if still_unsettled(&mut since, Instant::now()) => Ok(None),
            Err(message) => {
                *since = None;
                Err(message)
            }
        }
    }

    /// Reads this computer's displays at most once a watch interval while no link or session
    /// reports them, and moves the setup revision on when they changed, so its cards redraw.
    fn watch_local_displays(&self, path: &Path) {
        // A link reports every change itself, and a session ends on one.
        if self.sharing.live_inspection().is_some() {
            return;
        }
        let mut watch = lock(&self.displays_watch);
        if watch
            .read_at
            .is_some_and(|read_at| read_at.elapsed() < DISPLAYS_WATCH)
        {
            return;
        }
        watch.read_at = Some(Instant::now());
        let Ok(file) = self.cached_setup(path, SETUP_UNREADABLE) else {
            return;
        };
        let Some(now) = file
            .active_computer()
            .and_then(|record| current_local_displays(record).ok())
        else {
            return;
        };
        if watch
            .displays
            .as_ref()
            .is_none_or(|seen| !seen.same_geometry(&now) || !seen.same_labels(&now))
        {
            self.sharing.advance_setup_revision();
        }
        watch.displays = Some(now);
    }

    /// The link's decision pass. Only the decider (lower DeviceId) picks the record, leaving via
    /// the close after its commit; the other computer only proposes a flip over a fitting record.
    fn switch_or_continue(
        &self,
        path: &Path,
        saved: &SharingPreferences,
        inspection: &InspectedPeer,
    ) -> Result<Option<String>, String> {
        // In flight, answered "nothing fits", or refused too recently. A banner is none of these:
        // it outlives the link a proposal died with.
        if self.sharing.proposal_pending_for(inspection)
            || self.sharing.waiting_notice_for(inspection)
            || self.sharing.retry_wait_for(inspection)
        {
            return Ok(None);
        }
        let fits = saved.fits_displays(inspection);
        if !local_decides(inspection) {
            if fits && self.sharing.control_pending() {
                return Ok(self.send(
                    saved,
                    None,
                    inspection,
                    "sent the other computer a control change",
                ));
            }
            if !fits
                && self
                    .sharing
                    .raise_display_notice(DisplayNotice::PeerDeciding, inspection)
            {
                return Ok(Some(transition(
                    "waiting for the other computer to choose the layout",
                    Some(saved),
                    Some(inspection),
                )));
            }
            return Ok(None);
        }
        // Every in-between display state would otherwise be proposed and committed in turn.
        if !self.sharing.displays_settled() {
            return Ok(None);
        }
        if fits {
            return Ok(self.send(
                saved,
                None,
                inspection,
                "sent the other computer this computer's layout",
            ));
        }
        let library = self.library(path);
        // The memory that fits exactly wins and loses nothing; otherwise the record is rebuilt
        // from the memory made with exactly these monitors, so a display that only moved keeps
        // every crossing made for it, and from the running record when there is no such memory.
        // Who may control whom is the pair's current choice, never a memory's.
        let next = library
            .as_ref()
            .and_then(|library| library.automatic_fit(inspection))
            .map(|record| (record, false))
            .or_else(|| {
                library
                    .as_ref()
                    .and_then(|library| library.automatic_for_same_displays(inspection))
                    .and_then(|basis| adapt_to_inspection(basis, inspection))
                    .map(|adapted| (adapted.record, adapted.left_out))
            })
            .or_else(|| {
                adapt_to_inspection(saved, inspection)
                    .map(|adapted| (adapted.record, adapted.left_out))
            })
            .and_then(|(record, left_out)| {
                record
                    .with_control(saved.control().clone())
                    .map(|record| (record, left_out))
            });
        let Some((next, left_out)) = next else {
            if self
                .sharing
                .raise_display_notice(DisplayNotice::Waiting, inspection)
            {
                return Ok(Some(transition(
                    "no layout fits these displays; waiting for arranging",
                    Some(saved),
                    Some(inspection),
                )));
            }
            return Ok(None);
        };
        Ok(self.send(
            &next,
            Some(left_out),
            inspection,
            "sent the other computer a layout for the displays it shows now",
        ))
    }

    /// Both computers commit the agreed bytes, then the sender closes the link. A proposal that
    /// cannot leave is retried like a passing refusal and never backs the supervisor off.
    fn send(
        &self,
        next: &SharingPreferences,
        display_change: Option<bool>,
        inspection: &InspectedPeer,
        action: &'static str,
    ) -> Option<String> {
        let sent = match display_change {
            Some(left_out) => self.sharing.propose_layout(next, left_out),
            None => self.sharing.propose_record(next),
        };
        match sent {
            Ok(()) => Some(transition(action, Some(next), Some(inspection))),
            Err(message) => {
                log::warn!("supervisor: the proposal was not sent: {message}");
                self.sharing.note_proposal_failed(inspection);
                None
            }
        }
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
        let file = drawn_setup(&file, live.as_ref());
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

    /// Only reads a file that is replaced whole, so like `computers_load` it never waits for the gate.
    pub fn arrangements(
        &self,
        path: &Path,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
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

    /// Every arrangement kept for one paired computer, whether or not it is connected. Only a
    /// live link to that same computer can mark an entry as one that fits right now. It only
    /// reads, so like `computers_load` it never waits for the gate.
    pub fn arrangements_for(
        &self,
        fingerprint: &str,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
        self.arrangement_views(fingerprint, None)
    }

    /// Forgets one of that computer's arrangements and lists what is left; no connection needed.
    pub fn forget_arrangement(
        &self,
        fingerprint: &str,
        name: &str,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
        let _lease = self.gate.begin()?;
        self.arrangement_views(fingerprint, Some(name))
    }

    fn arrangement_views(
        &self,
        fingerprint: &str,
        forget: Option<&str>,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
        let peer = parse_fingerprint(fingerprint)?;
        let library_path = self.setup_path()?.with_file_name(ARRANGEMENTS_FILE);
        // This library is this computer's own, so an entry belongs to whichever computer its
        // record names; nothing else has to be resolved before listing or forgetting.
        let peer_hex = peer.full_hex();
        let mut library = ArrangementLibrary::load(&library_path)
            .map_err(|_| crate::sharing::ARRANGEMENTS_UNREADABLE.to_owned())?;
        if let Some(name) = forget {
            library
                .remove_for(&peer_hex, name)
                .map_err(|error| error.message().to_owned())?;
            crate::sharing::save_library(&library_path, &library)?;
            log::info!("user: forgot an arrangement for {}", short(peer));
        }
        let live = self.sharing.live_inspection();
        Ok(library.views_for(&peer_hex, live.as_ref().map(|(_, inspection)| inspection)))
    }

    pub fn request_shutdown(&self) -> bool {
        let first = self.gate.request_shutdown();
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

/// The setup as the computer cards draw it: each record for the displays there are now, as the
/// link or session reports them for its computer, else as this computer reads its own beside the
/// other's last seen. A record whose displays cannot be read is drawn as saved.
fn drawn_setup(file: &SetupFile, live: Option<&(String, InspectedPeer)>) -> SetupFile {
    // One read per identity this computer has held; every record normally names the same one.
    let mut local_now: BTreeMap<String, Option<DisplayTopology>> = BTreeMap::new();
    file.previewed(|record| {
        let linked = live
            .filter(|(fingerprint, _)| *fingerprint == fingerprint_key(record.peer_fingerprint()))
            .map(|(_, inspection)| inspection.clone());
        let now = linked.or_else(|| {
            local_now
                .entry(record.local_fingerprint().to_owned())
                .or_insert_with(|| current_local_displays(record).ok())
                .clone()
                .and_then(|local| record.with_local_displays(local))
        });
        now.map_or_else(|| record.clone(), |now| record.preview_for(&now))
    })
}

/// One log line per supervisor step: each side's display count and geometry digest, the record's
/// digest and the decider. Nothing in it names a key or typed text.
fn transition(
    action: &str,
    saved: Option<&SharingPreferences>,
    inspection: Option<&InspectedPeer>,
) -> String {
    let displays = inspection.map_or_else(
        || {
            saved.map_or_else(
                || "no displays known".to_owned(),
                SharingPreferences::describe,
            )
        },
        |inspection| DisplayGeometry::of(inspection).describe(),
    );
    let record = saved.map_or_else(|| "none".to_owned(), |saved| format!("#{}", saved.digest()));
    let decider = match inspection
        .map(local_decides)
        .or_else(|| saved.map(SharingPreferences::local_decides))
    {
        Some(true) => "this computer",
        Some(false) => "the other computer",
        None => "unknown",
    };
    format!("{action} [{displays}; record {record}; decider {decider}]")
}

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
    use crate::sharing::tests::{
        LinkFixture, fake_link, join_finished_worker, open_fake_link, settle_displays, sync_state,
        wait_for,
    };
    use crate::sharing_preferences::tests::{file_with, inspection, opposite, preferences};
    use monhop_transport::session_link::{LinkDisconnect, LinkEvent};

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
        assert!(
            controller
                .set_active(Some("not a fingerprint"), None)
                .is_err()
        );
        let chosen = controller.set_active(Some(&"B".repeat(64)), None).unwrap();
        assert_eq!(chosen.active.as_deref(), Some("b".repeat(64).as_str()));
        assert_eq!(
            SetupFile::load(&folder.join("sharing.json"))
                .unwrap()
                .active(),
            Some("b".repeat(64).as_str())
        );
        let paused = controller.set_active(None, None).unwrap();
        assert!(paused.active.is_none());
        assert_eq!(paused.phase, "off");
        assert_eq!(paused.message, crate::sharing::PAUSED);
        assert!(!paused.editing);
        let _ = std::fs::remove_dir_all(folder);
    }

    /// A fresh, empty setup folder that the test owns and removes.
    fn folder(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("monhop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn fingerprint(letter: char) -> CertificateFingerprint {
        CertificateFingerprint::parse_full(&letter.to_string().repeat(64)).unwrap()
    }

    /// The fixture pair's record as the other computer holds it, made while this computer's
    /// display 1 sat elsewhere: it no longer fits.
    fn stale_record_of_the_other_computer() -> SharingPreferences {
        let mut moved = preferences();
        moved.move_local_display_for_test("1", [0.0, 240.0]);
        SharingPreferences::from_inspection(
            &opposite(&inspection(&moved)),
            preferences().layout().clone(),
        )
        .expect("a valid record from the other computer's side")
    }

    #[test]
    fn the_non_decider_waits_while_the_decider_chooses_the_layout() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let folder = folder("peer-decides");
        let path = folder.join("sharing.json");
        let here = opposite(&inspection(&preferences()));
        assert!(!local_decides(&here));
        let stale = stale_record_of_the_other_computer();
        assert!(!stale.fits_displays(&here));
        assert!(
            controller
                .switch_or_continue(&path, &stale, &here)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            controller.sharing.status().display_notice,
            Some(DisplayNotice::PeerDeciding)
        );
        // Nothing is promoted, adapted or written on this side; the layout arrives over the link.
        assert!(!path.exists());
        assert!(!folder.join(ARRANGEMENTS_FILE).exists());
        assert_eq!(
            controller.switch_or_continue(&path, &stale, &here).unwrap(),
            None
        );
        // A record that fits is not proposed from this side either, unless a switch flip rides
        // on it; it waits for the decider's.
        controller.sharing.clear_display_notice();
        let fitting =
            SharingPreferences::from_inspection(&here, preferences().layout().clone()).unwrap();
        assert_eq!(
            controller
                .switch_or_continue(&path, &fitting, &here)
                .unwrap(),
            None
        );
        assert!(controller.sharing.status().display_notice.is_none());
        let _ = std::fs::remove_dir_all(folder);
    }

    /// An app controller with a connected fake link to `peer` showing `inspection`.
    fn linked_controller(
        path: &Path,
        peer: CertificateFingerprint,
        inspection: &InspectedPeer,
    ) -> (AppController, LinkFixture) {
        let controller = AppController {
            sharing: Arc::new(SharingController::with_link_runner(
                fake_link,
                Duration::from_secs(600),
            )),
            ..AppController::default()
        };
        controller.use_setup_path(path.to_path_buf());
        let fixture = open_fake_link(&controller.sharing, path.to_path_buf(), peer);
        fixture
            .events
            .send(LinkEvent::Connected {
                inspection: inspection.clone(),
            })
            .unwrap();
        wait_for(&controller.sharing, |view| view.phase == "connected");
        (controller, fixture)
    }

    #[test]
    fn two_computers_make_one_proposal_and_one_commit_before_a_session() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("two-computers");
        let decider_path = folder.join("decider").join("sharing.json");
        let other_path = folder.join("other").join("sharing.json");
        let fitting = preferences();
        let decider_view = inspection(&fitting);
        let other_view = opposite(&decider_view);
        assert!(local_decides(&decider_view));
        assert!(!local_decides(&other_view));
        let stale = stale_record_of_the_other_computer();
        file_with(fitting.clone()).save(&decider_path).unwrap();
        file_with(stale.clone()).save(&other_path).unwrap();
        let (decider, decider_link) =
            linked_controller(&decider_path, fingerprint('B'), &decider_view);
        let (other, other_link) = linked_controller(&other_path, fingerprint('A'), &other_view);

        // The other computer's record is stale: it waits and proposes nothing.
        assert!(
            other
                .switch_or_continue(&other_path, &stale, &other_view)
                .unwrap()
                .is_some()
        );
        // The decider waits for the displays to hold still, then proposes its record once.
        assert_eq!(
            decider
                .switch_or_continue(&decider_path, &fitting, &decider_view)
                .unwrap(),
            None
        );
        settle_displays(&decider.sharing);
        assert!(
            decider
                .switch_or_continue(&decider_path, &fitting, &decider_view)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            decider
                .switch_or_continue(&decider_path, &fitting, &decider_view)
                .unwrap(),
            None
        );
        let bytes = decider_link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the decider proposes its record");
        assert!(decider_link.proposals.try_recv().is_err());
        assert!(other_link.proposals.try_recv().is_err());

        // The link commits the same bytes on both computers.
        for (link, view) in [(&decider_link, &decider_view), (&other_link, &other_view)] {
            (link.persist.stage)(view, &bytes).unwrap();
            (link.persist.commit)(view, &bytes).unwrap();
        }
        other_link
            .events
            .send(LinkEvent::SyncCompleted {
                inspection: other_view.clone(),
                bytes: bytes.clone(),
                sending: false,
            })
            .unwrap();
        wait_for(&other.sharing, |view| sync_state(view) == "applied");
        // Only the decider leaves, after the commit; the other computer waits for that close.
        assert_eq!(other.sharing.status().phase, "connected");
        decider_link
            .events
            .send(LinkEvent::SyncCompleted {
                inspection: decider_view.clone(),
                bytes,
                sending: true,
            })
            .unwrap();
        wait_for(&decider.sharing, |view| view.phase == "off");
        other_link
            .events
            .send(LinkEvent::Disconnected {
                reason: LinkDisconnect::PeerClosed,
            })
            .unwrap();
        wait_for(&other.sharing, |view| view.phase == "off");
        join_finished_worker(&decider.sharing);
        join_finished_worker(&other.sharing);

        // Both hold one record that fits and nothing holds a link, so each next pass starts the
        // session.
        for (controller, path, view) in [
            (&decider, &decider_path, &decider_view),
            (&other, &other_path, &other_view),
        ] {
            let file = SetupFile::load(path).unwrap();
            assert!(file.active_computer().unwrap().fits_displays(view));
            assert!(!controller.sharing.holds_link());
            assert!(controller.sharing.link_is_off());
            assert!(controller.sharing.status().display_notice.is_none());
        }
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_transition_line_names_displays_record_and_decider_but_no_identity() {
        let saved = preferences();
        let here = inspection(&saved);
        let line = transition("sent", Some(&saved), Some(&here));
        assert!(
            line.starts_with("sent [this computer 1 display #"),
            "{line}"
        );
        assert!(line.contains(", other computer 1 display #"), "{line}");
        assert!(
            line.contains(&format!("; record #{}; ", saved.digest())),
            "{line}"
        );
        assert!(line.ends_with("; decider this computer]"), "{line}");
        for identity in ["aaaaaaaa", "AAAAAAAA", "bbbbbbbb", "BBBBBBBB"] {
            assert!(!line.contains(identity), "{line}");
        }
        let there = transition("waiting", Some(&saved), Some(&opposite(&here)));
        assert!(there.ends_with("; decider the other computer]"), "{there}");
        assert_eq!(
            transition("idle", None, None),
            "idle [no displays known; record none; decider unknown]"
        );
    }

    #[test]
    fn a_computers_layout_history_lists_and_forgets_while_it_is_not_connected() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let folder = folder("layout-history");
        let setup_path = folder.join("sharing.json");
        controller.use_setup_path(setup_path.clone());
        let peer = "B".repeat(64);
        // An empty library lists nothing; there is no setup file yet either.
        assert!(controller.arrangements_for(&peer).unwrap().is_empty());
        assert!(!setup_path.exists());

        let saved = crate::sharing_preferences::tests::preferences();
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", saved).unwrap();
        library
            .save(&setup_path.with_file_name(ARRANGEMENTS_FILE))
            .unwrap();

        // The library is this computer's own, so no setup-file record is needed to claim it.
        let listed = controller.arrangements_for(&peer.to_lowercase()).unwrap();
        assert!(!setup_path.exists());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Desk");
        // Nothing is connected, so nothing fits and nothing offers a layout to load.
        assert!(!listed[0].fits);
        assert!(listed[0].layout.is_none());
        let other = "C".repeat(64);
        assert!(controller.arrangements_for(&other).unwrap().is_empty());
        assert!(controller.forget_arrangement(&other, "Desk").is_err());
        assert!(controller.arrangements_for("not a fingerprint").is_err());
        assert!(
            controller
                .forget_arrangement(&peer, "Desk")
                .unwrap()
                .is_empty()
        );
        assert!(controller.arrangements_for(&peer).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn the_computers_and_their_layouts_read_while_another_pass_holds_the_gate() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let folder = folder("busy-gate");
        controller.use_setup_path(folder.join("sharing.json"));
        let peer = "B".repeat(64);
        let pass = controller
            .gate
            .begin()
            .expect("a supervisor pass holds the gate");
        let computers = serde_json::to_value(controller.computers_load().unwrap()).unwrap();
        assert_eq!(computers["computers"], serde_json::json!([]));
        assert!(controller.arrangements_for(&peer).unwrap().is_empty());
        // Whatever writes still waits for the pass to end.
        assert!(controller.computers_rename(&peer, "Desk").is_err());
        assert!(controller.forget_arrangement(&peer, "Desk").is_err());
        drop(pass);
        assert!(controller.computers_load().is_ok());
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
        assert!(controller.set_active(None, None).is_err());
    }

    #[test]
    fn metadata_updates_drain_before_exit_and_cannot_restart_after_quit() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        {
            let _lease = controller.gate.begin().unwrap();
            assert!(controller.request_shutdown());
            assert!(!controller.shutdown_ready());
        }
        assert!(controller.shutdown_ready());
        assert!(controller.gate.begin().is_err());
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
        controller.sharing.stop_with("Stopped.");
        assert!(controller.sharing.shutdown_ready());
    }

    #[test]
    fn retained_input_cleanup_blocks_pairing_mutations_and_exit() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let ownership = monhop_core::NativeSessionClaim::claim().unwrap();
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
        assert!(controller.gate.begin().is_err());
        assert!(controller.begin_update(true).is_err());
        drop(update);
        {
            let _lease = controller.gate.begin().unwrap();
            assert!(controller.begin_update(false).is_err());
            assert!(controller.begin_update(true).is_err());
        }
        assert!(controller.begin_update(false).is_ok());
    }

    #[test]
    fn native_cleanup_blocks_checks_even_when_the_view_is_not_sharing() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let controller = AppController::default();
        let ownership = monhop_core::NativeSessionClaim::claim().unwrap();
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
