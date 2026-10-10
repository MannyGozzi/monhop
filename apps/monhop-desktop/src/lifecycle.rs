//! Serializes explicit setup actions without holding a lock across native prompts, and keeps
//! the active computer connected.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime},
};

use monhop_transport::{
    crypto::CertificateFingerprint,
    session_setup::{DisplayTopology, InspectedPeer, same_network},
};

use crate::{
    arrangement_library::ArrangementLibrary,
    clipboard::ClipboardHub,
    computers::{ComputerList, ComputersView, LIST_FILE, LiveInspection},
    group_record::{GroupRecord, KnownDisplays},
    pairing::{PairingController, PairingView},
    sharing::{
        ARRANGEMENTS_FILE, DisplayNotice, LayoutRequest, PAUSED, SWITCHING_COMPUTER, SessionPlan,
        SharingController, SharingView, current_local_displays, local_platform, still_unsettled,
    },
    sharing_preferences::{
        DisplayGeometry, SetupFile, fingerprint_key, local_decides, parse_fingerprint,
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
    /// Stopped with the app; its threads must end before the process does.
    clipboard: Mutex<Option<Arc<ClipboardHub>>>,
}

/// The sharing session and the setup link share one port, so a link waits for the session to stop.
const SESSION_PAUSE_WINDOW: Duration = Duration::from_secs(3);
const SUPERVISOR_BACKOFF: Duration = Duration::from_secs(10);
/// With nothing else reporting them, this computer's displays are read at most this often; with
/// the window's one-second poll, a change reaches its cards within about two seconds.
const DISPLAYS_WATCH: Duration = Duration::from_secs(1);
const PAIRING_HOLDS_PORT: &str = "Pairing in progress. Sharing resumes afterwards.";
const SWITCHING_NETWORK: &str = "Switching to the chosen network.";
const SETUP_UNREADABLE: &str = "The saved setup could not be read. Apply a layout again.";

impl AppController {
    #[cfg(test)]
    pub(crate) fn with_sharing(sharing: SharingController) -> Self {
        Self {
            sharing: Arc::new(sharing),
            ..Self::default()
        }
    }

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

    /// A code is only shown while the list has room, so no pairing can end as a trust record
    /// without a card.
    pub fn pairing_show_code(&self) -> Result<PairingView, String> {
        let _lease = self.gate.begin()?;
        ComputerList::load(&self.list_path()?)?.require_room()?;
        self.release_port_for_pairing()?;
        self.pairing.show_code()
    }

    /// A typo or a code for another network is refused before sharing pauses.
    pub fn pairing_enter_code(&self, code: &str) -> Result<PairingView, String> {
        let _lease = self.gate.begin()?;
        ComputerList::load(&self.list_path()?)?.require_room()?;
        let entered = self.pairing.check_code(code)?;
        self.release_port_for_pairing()?;
        self.pairing.enter_code(entered)
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

    /// Every Share session attaches to `hub`, which stops when the app does.
    pub fn use_clipboard(&self, hub: Arc<ClipboardHub>) {
        self.sharing.use_clipboard(Arc::clone(&hub));
        *lock(&self.clipboard) = Some(hub);
    }

    fn setup_path(&self) -> Result<PathBuf, String> {
        lock(&self.setup_path)
            .clone()
            .ok_or_else(|| "The setup folder could not be located.".to_owned())
    }

    fn list_path(&self) -> Result<PathBuf, String> {
        Ok(self.setup_path()?.with_file_name(LIST_FILE))
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
        // Until this version writes its own file, the file it migrates from carries the stamp.
        let stamp = std::fs::metadata(SetupFile::stored_at(path))
            .ok()
            .and_then(|metadata| {
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

    /// [`Self::quiesce_connection`] for `peer` alone: every other computer goes on sharing.
    fn quiesce_peer(&self, peer: CertificateFingerprint, message: &str) -> Result<(), String> {
        if !self.sharing.worker_alive(peer) && self.sharing.link_is_off_for(peer) {
            return Ok(());
        }
        self.sharing.stop_peer(peer, message);
        let deadline = Instant::now() + SESSION_PAUSE_WINDOW;
        while self.sharing.worker_alive(peer) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.sharing.worker_alive(peer) {
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

    /// Removes the trust record, the list entry, and the computer from every saved layout; a
    /// connection with it ends first so no session survives its own pairing. The groups it was
    /// in carry on without it.
    pub fn forget_computer(&self, fingerprint: &str) -> Result<ComputersView, String> {
        let _lease = self.gate.begin()?;
        let fingerprint = parse_fingerprint(fingerprint)?;
        let setup_path = self.setup_path()?;
        let list_path = self.list_path()?;
        let file = self.load_setup(&setup_path, "The saved setup could not be read.")?;
        log::info!("user: forgot computer {}", short(fingerprint));
        self.release_forgotten(&file, fingerprint)?;
        self.pairing.forget(fingerprint)?;
        // The list goes first: the setup write bumps the revision a reload keys on.
        let mut list = ComputerList::load(&list_path)?;
        list.forget(fingerprint);
        list.save(&list_path)?;
        self.sharing
            .update_setup_file(&setup_path, |file| file.forget(&fingerprint.full_hex()))?;
        crate::autostart::computer_forgotten(&setup_path);
        self.computers_view()
    }

    /// Ends every connection with a computer about to be forgotten, so none survives its
    /// pairing. With other computers enabled, only its own ends and theirs go on.
    pub(crate) fn release_forgotten(
        &self,
        file: &SetupFile,
        fingerprint: CertificateFingerprint,
    ) -> Result<(), String> {
        let key = fingerprint_key(&fingerprint.full_hex());
        if file.enabled().contains(&key) || self.sharing.worker_alive(fingerprint) {
            if file.enabled().iter().any(|other| *other != key) {
                self.sharing.clear_peer_choice(fingerprint);
                self.quiesce_peer(fingerprint, PAUSED)?;
            } else {
                self.sharing.clear_choice();
                self.quiesce_connection(PAUSED)?;
            }
        }
        self.sharing.forget_on_network(fingerprint);
        Ok(())
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

    /// Switches one computer in or out of the group shared with; the rest of the group is kept.
    /// Like [`Self::set_active`], the connections follow within a tick.
    pub fn set_enabled(
        &self,
        fingerprint: &str,
        enabled: bool,
        interface_id: Option<&str>,
    ) -> Result<SharingView, String> {
        let view = {
            let _lease = self.gate.begin()?;
            if !self.pairing.shutdown_ready() {
                return Err("Finish or cancel pairing before changing computers.".into());
            }
            let fingerprint = parse_fingerprint(fingerprint)?;
            log::info!(
                "user: switched computer {} {}",
                short(fingerprint),
                if enabled { "on" } else { "off" }
            );
            let path = self.setup_path()?;
            if enabled && let Some(interface_id) = interface_id {
                self.leave_network_for(&path, interface_id)?;
            }
            let view = self
                .sharing
                .set_enabled(&path, fingerprint, enabled, interface_id)?;
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

    /// Opens a setup link for arranging with every computer of the group, switching `fingerprint`
    /// on first when one is given; each session ends first, a link already up is kept.
    pub fn edit_begin(
        &self,
        interface_id: String,
        fingerprint: Option<&str>,
    ) -> Result<SharingView, String> {
        let _lease = self.gate.begin()?;
        if !self.pairing.shutdown_ready() || self.pairing.occupies_port() {
            return Err("Finish or cancel pairing before arranging displays.".into());
        }
        let fingerprint = fingerprint.map(parse_fingerprint).transpose()?;
        let path = self.setup_path()?;
        match fingerprint {
            Some(fingerprint) => log::info!("user: started arranging with {}", short(fingerprint)),
            None => log::info!("user: started arranging"),
        }
        self.leave_network_for(&path, &interface_id)?;
        let file = self.sharing.try_update_setup_file(&path, |file| {
            if let Some(fingerprint) = &fingerprint {
                file.set_enabled(fingerprint, true)?;
            }
            file.set_interface_id(&interface_id);
            Ok(())
        })?;
        if !file.sharing_chosen() {
            return Err("Switch on a computer to arrange the displays with.".into());
        }
        let group = file
            .enabled()
            .iter()
            .map(|peer| parse_fingerprint(peer))
            .collect::<Result<Vec<_>, _>>()?;
        *lock(&self.sharing_retry_after) = None;
        self.sharing.clear_failure_backoff();
        for peer in group {
            if self.sharing.worker_alive(peer) && !self.sharing.link_is_off_for(peer) {
                continue;
            }
            self.quiesce_peer(peer, "Opening the link for arranging.")?;
            self.sharing
                .connect_link(path.clone(), interface_id.clone(), peer)?;
        }
        Ok(self.sharing.begin_editing())
    }

    /// Every connection runs on the one network the setup file records, and the endpoint they
    /// share rebinds for any other one; so before `interface_id` replaces it, every connection
    /// ends and is waited for.
    fn leave_network_for(&self, path: &Path, interface_id: &str) -> Result<(), String> {
        if interface_id.is_empty() {
            return Ok(());
        }
        let file = self.load_setup(path, "The saved setup could not be read.")?;
        if file
            .interface_id()
            .is_some_and(|saved| same_network(saved, interface_id))
        {
            return Ok(());
        }
        log::info!("user: chose another network; ending every connection first");
        self.quiesce_connection(SWITCHING_NETWORK)
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

    /// Ok(None) means nothing to do: no enabled computer, or another action owns input or the
    /// port. Each enabled computer is kept on its own: a record that fits this computer's
    /// displays runs as a session, anything else keeps a link, on which one computer proposes the
    /// record both then hold. A computer no longer enabled lets go. Ok(Some) is the log line.
    fn keep_connected(&self, path: &Path) -> Result<Option<String>, String> {
        if !self.pairing.shutdown_ready() || self.pairing.occupies_port() {
            return Ok(None);
        }
        let file = self.cached_setup(path, SETUP_UNREADABLE)?;
        self.sharing.adopt_saved(&file);
        self.sharing.sync_group(&file);
        let enabled = if file.sharing_chosen() {
            file.enabled()
                .iter()
                .map(|peer| parse_fingerprint(peer))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let mut lines = Vec::new();
        // The group stays as it is; only its connections end.
        let released = if file.paused() {
            PAUSED
        } else {
            SWITCHING_COMPUTER
        };
        for live in self.sharing.live_peers() {
            if !enabled.contains(&live) && self.sharing.release_peer(live, released) {
                lines.push(transition(
                    "ended the connection with a computer no longer switched on",
                    None,
                    None,
                ));
            }
        }
        let alone = enabled.len() == 1;
        // This computer's displays are read at most once a pass, whichever computer asks first.
        let mut local_fit = None;
        for peer in enabled {
            match self.keep_peer(path, &file, peer, &mut local_fit) {
                Ok(Some(line)) => lines.push(line),
                Ok(None) => {}
                Err(message) if alone => return Err(message),
                // One computer that cannot start waits out its own backoff; the others go on.
                Err(message) => {
                    log::warn!("supervisor: could not connect {}: {message}", short(peer));
                    self.sharing.note_peer_failure(peer, message);
                }
            }
        }
        Ok((!lines.is_empty()).then(|| lines.join("; ")))
    }

    /// One enabled computer's step of [`Self::keep_connected`].
    fn keep_peer(
        &self,
        path: &Path,
        file: &SetupFile,
        peer: CertificateFingerprint,
        local_fit: &mut Option<Option<bool>>,
    ) -> Result<Option<String>, String> {
        let saved = file.active_group().zip(file.local());
        if self.sharing.worker_alive(peer) {
            if self.sharing.end_session_for_control(peer) {
                return Ok(Some(transition(
                    "ending the session to change who can control which computer",
                    saved,
                    None,
                )));
            }
            let Some(inspection) = self.sharing.inspection_for_switch_with(peer) else {
                return Ok(None);
            };
            return self.decide_on_link(path, file.active_group(), &inspection);
        }
        if !self.sharing.native_settled() || !self.sharing.link_is_off_for(peer) {
            return Ok(None);
        }
        // A worker that ended with a real failure waits out its backoff here; relaunching it
        // every pass would only repeat the failure. Any user choice clears it.
        if self
            .sharing
            .within_failure_backoff(peer, SUPERVISOR_BACKOFF)
        {
            return Ok(None);
        }
        if let Some(plan) = SessionPlan::for_peer(file, &peer.full_hex())
            .filter(|_| !self.sharing.editing() && !self.sharing.holds_link(peer))
        {
            let fits = match *local_fit {
                Some(fits) => fits,
                None => {
                    let fits = self.local_displays_fit(&plan)?;
                    *local_fit = Some(fits);
                    fits
                }
            };
            match fits {
                None => return Ok(None),
                // A pair where neither computer may control the other gets no Share connection.
                Some(true) if plan.control().is_none() => return Ok(None),
                Some(true) => {
                    let line = transition(
                        "started sharing with the active computer",
                        Some((plan.record(), plan.local())),
                        None,
                    );
                    self.sharing.start_sharing(plan)?;
                    return Ok(Some(line));
                }
                // Never promoted here: a record changes only by a proposal on the link.
                Some(false) => self.sharing.note_local_misfit(peer),
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
    fn local_displays_fit(&self, plan: &SessionPlan) -> Result<Option<bool>, String> {
        let mut since = lock(&self.displays_unreadable_since);
        match self.sharing.read_local_displays(plan.local()) {
            Ok(current) => {
                *since = None;
                Ok(Some(plan.fits_local(&current)))
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
            .sharing_chosen()
            .then(|| file.active_group())
            .flatten()
            .and(file.local())
            .and_then(|local| self.sharing.read_local_displays(local).ok())
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

    /// The link's decision pass: exactly one computer proposes, and only the commit that follows
    /// ends the link. A pending switch flip here is proposed whatever the stamps; otherwise the
    /// holder of the newer record proposes it, and when both hold the same content the lower
    /// DeviceId confirms it, or refits it when it no longer fits. The other computer waits.
    fn decide_on_link(
        &self,
        path: &Path,
        saved: Option<&GroupRecord>,
        inspection: &InspectedPeer,
    ) -> Result<Option<String>, String> {
        // In flight, answered "nothing fits", refused too recently, or the other computer's
        // summary is not in. A banner is none of these: it outlives the link a proposal died with.
        if self.sharing.proposal_pending_for(inspection)
            || self.sharing.waiting_notice_for(inspection)
            || self.sharing.retry_wait_for(inspection)
        {
            return Ok(None);
        }
        let Some(theirs) = self.sharing.peer_stamp_for(inspection) else {
            return Ok(None);
        };
        let local = inspection.local_fingerprint.full_hex();
        let peer = inspection.peer_fingerprint.full_hex();
        let saved = saved.filter(|record| record.has_member(&local) && record.has_member(&peer));
        let fits = saved.is_some_and(|record| record.fits_link(inspection));
        if let Some(saved) = saved.filter(|_| self.sharing.control_pending()) {
            if fits {
                return Ok(self.send(
                    saved,
                    None,
                    inspection,
                    "sent the other computer a control change",
                ));
            }
            return self.refit(path, saved, inspection);
        }
        let mine = saved.map(GroupRecord::stamp);
        let same = mine
            .zip(theirs)
            .is_some_and(|(mine, theirs)| mine.same_content(&theirs));
        let proposes = if same {
            local_decides(inspection)
        } else {
            mine > theirs
        };
        let Some(saved) = saved else {
            return Ok(None);
        };
        if !proposes {
            if !fits
                && self
                    .sharing
                    .raise_display_notice(DisplayNotice::PeerDeciding, inspection)
            {
                return Ok(Some(transition(
                    "waiting for the other computer to choose the layout",
                    Some((saved, &local)),
                    Some(inspection),
                )));
            }
            return Ok(None);
        }
        // Every in-between display state would otherwise be proposed and committed in turn.
        if !self.sharing.displays_settled_for(inspection) {
            return Ok(None);
        }
        if fits {
            return Ok(self.send(
                saved,
                None,
                inspection,
                if same {
                    "confirmed the layout both computers hold"
                } else {
                    "sent the other computer this computer's newer layout"
                },
            ));
        }
        self.refit(path, saved, inspection)
    }

    /// Proposes `saved` rebuilt for the displays both link ends show now, as this computer's
    /// newest change, once they hold still; raises "nothing fits" when no rebuild is valid.
    fn refit(
        &self,
        path: &Path,
        saved: &GroupRecord,
        inspection: &InspectedPeer,
    ) -> Result<Option<String>, String> {
        if !self.sharing.displays_settled_for(inspection) {
            return Ok(None);
        }
        let local = inspection.local_fingerprint.full_hex();
        let library = self.library(path);
        let known = KnownDisplays::of_link(inspection);
        // The memory that fits exactly wins and loses nothing; otherwise the record is rebuilt
        // from the memory made with exactly these monitors, so a display that only moved keeps
        // every crossing made for it, and from the running record when there is no such memory.
        // Who may control whom is the group's current choice, never a memory's.
        let next = library
            .as_ref()
            .and_then(|library| library.automatic_fit(saved, &known))
            .map(|record| (record, false))
            .or_else(|| {
                library
                    .as_ref()
                    .and_then(|library| library.automatic_for_same_displays(saved, &known))
                    .and_then(|basis| basis.adapt(&known.with_entries_of(saved)))
                    .map(|adapted| (adapted.record, adapted.left_out))
            })
            .or_else(|| {
                saved
                    .adapt(&known)
                    .map(|adapted| (adapted.record, adapted.left_out))
            })
            .and_then(|(record, left_out)| {
                record
                    .with_control(saved.layout().control.clone())
                    .ok()
                    .map(|record| (record, left_out))
            });
        let Some((next, left_out)) = next else {
            if self
                .sharing
                .raise_display_notice(DisplayNotice::Waiting, inspection)
            {
                return Ok(Some(transition(
                    "no layout fits these displays; waiting for arranging",
                    Some((saved, &local)),
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
        next: &GroupRecord,
        display_change: Option<bool>,
        inspection: &InspectedPeer,
        action: &'static str,
    ) -> Option<String> {
        let sent = match display_change {
            Some(left_out) => self.sharing.propose_layout_to(inspection, next, left_out),
            None => self.sharing.propose_record_to(inspection, next),
        };
        let local = inspection.local_fingerprint.full_hex();
        match sent {
            Ok(()) => Some(transition(action, Some((next, &local)), Some(inspection))),
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

    /// Lists each newly paired computer and makes it the active one unless several computers
    /// already share. A failed write hands the pairings back so the next tick records them; a
    /// full list drops the entry, and the trust record still shows as a computer so it can be
    /// forgotten.
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
                    Ok(()) => log::info!("pairing: {} paired", short(pairing.fingerprint)),
                    Err(message) => log::warn!(
                        "pairing: {} paired but not listed: {message}",
                        short(pairing.fingerprint)
                    ),
                }
            }
            list.save(&list_path)?;
            self.sharing.update_setup_file(&setup_path, |file| {
                for pairing in &completed {
                    if file.enabled().len() <= 1 {
                        file.choose(&pairing.fingerprint);
                        log::info!("pairing: {} made active", short(pairing.fingerprint));
                    }
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
        let setup_path = self.setup_path()?;
        let file = self.load_setup(&setup_path, "The saved setup could not be read.")?;
        let list = ComputerList::load(&self.list_path()?)?;
        // A damaged library is reported where it is read; here it only is not a newer one.
        let arrangements_from_newer =
            ArrangementLibrary::load(&setup_path.with_file_name(ARRANGEMENTS_FILE))
                .is_ok_and(|library| library.written_by_newer());
        let live = self.sharing.live_inspections();
        let file = drawn_setup(&file, &live);
        let live: Vec<LiveInspection<'_>> = live
            .iter()
            .map(|(fingerprint, inspection)| LiveInspection {
                fingerprint,
                inspection,
            })
            .collect();
        Ok(ComputersView::assemble(
            &list,
            &self.pairing.paired_peers(),
            self.pairing.local_fingerprint(),
            &file,
            &live,
            &self.sharing.revision(),
            arrangements_from_newer,
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
    /// `members` names the entry's computers when several of that name include it.
    pub fn forget_arrangement(
        &self,
        fingerprint: &str,
        name: &str,
        members: Option<&[String]>,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
        let _lease = self.gate.begin()?;
        let members = members
            .map(|members| {
                members
                    .iter()
                    .map(|member| parse_fingerprint(member).map(|member| member.full_hex()))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        self.arrangement_views(fingerprint, Some((name, members.as_deref())))
    }

    fn arrangement_views(
        &self,
        fingerprint: &str,
        forget: Option<(&str, Option<&[String]>)>,
    ) -> Result<Vec<crate::arrangement_library::ArrangementView>, String> {
        let peer = parse_fingerprint(fingerprint)?;
        let library_path = self.setup_path()?.with_file_name(ARRANGEMENTS_FILE);
        // This library is this computer's own, so an entry belongs to whichever computers its
        // record names; nothing else has to be resolved before listing or forgetting.
        let peer_hex = peer.full_hex();
        let mut library = ArrangementLibrary::load(&library_path)
            .map_err(|_| crate::sharing::ARRANGEMENTS_UNREADABLE.to_owned())?;
        if let Some((name, members)) = forget {
            library
                .remove_for(&peer_hex, name, members)
                .map_err(|error| error.message().to_owned())?;
            crate::sharing::save_library(&library_path, &library)?;
            log::info!("user: forgot an arrangement for {}", short(peer));
        }
        Ok(library.views_for(&peer_hex, &self.sharing.known_displays()))
    }

    pub fn request_shutdown(&self) -> bool {
        let first = self.gate.request_shutdown();
        self.pairing.request_shutdown();
        self.sharing.request_shutdown();
        if let Some(clipboard) = lock(&self.clipboard).as_ref() {
            clipboard.request_shutdown();
        }
        first
    }

    pub fn shutdown_ready(&self) -> bool {
        self.gate.is_drained()
            && self.pairing.shutdown_ready()
            && self.sharing.shutdown_ready()
            && lock(&self.clipboard)
                .as_ref()
                .is_none_or(|clipboard| clipboard.is_stopped())
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
/// links and sessions report them for their computers, else as this computer reads its own beside
/// the others' last seen. A record whose displays cannot be read is drawn as saved.
fn drawn_setup(file: &SetupFile, live: &[(String, InspectedPeer)]) -> SetupFile {
    let Some(local) = file.local() else {
        return file.clone();
    };
    let mut known = KnownDisplays::default();
    for (_, inspection) in live {
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
    }
    // Every live connection reports this computer's displays; without one they are read once.
    if live.is_empty()
        && !file.groups().is_empty()
        && let Ok(now) = current_local_displays(local)
    {
        known.insert(local, local_platform(), &now);
    }
    file.previewed(|record| record.preview_for(&known))
}

/// One log line per supervisor step: each side's display count and geometry digest, the record's
/// digest and the decider. Nothing in it names a key or typed text. `saved` is the record with
/// this computer's fingerprint.
fn transition(
    action: &str,
    saved: Option<(&GroupRecord, &str)>,
    inspection: Option<&InspectedPeer>,
) -> String {
    let displays = inspection.map_or_else(
        || {
            saved.map_or_else(
                || "no displays known".to_owned(),
                |(record, local)| record.describe_for(local),
            )
        },
        |inspection| DisplayGeometry::of(inspection).describe(),
    );
    let record = saved.map_or_else(
        || "none".to_owned(),
        |(record, _)| format!("#{}", record.digest()),
    );
    let decider = match inspection
        .map(local_decides)
        .or_else(|| saved.map(|(record, local)| record.decided_by(local)))
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
    use crate::group_record::RecordSummary;
    use crate::sharing::tests::{
        LinkFixture, fake_link, join_finished_worker, open_fake_link, settle_displays, sync_state,
        wait_for,
    };
    use crate::sharing_preferences::tests::{
        LOCAL, V3_FILE, WINDOWS_PC, file_with, group, inspection, link_of, load_as,
        mirrored_v3_file, opposite, preferences, trio, two_display_record,
    };
    use crate::sharing_preferences::{ControlMap, topology_of};
    use monhop_core::Platform;
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
    fn stale_record_of_the_other_computer() -> GroupRecord {
        let mut moved = preferences();
        moved.move_local_display_for_test("1", [0.0, 240.0]);
        GroupRecord::for_link(
            &opposite(&inspection(&moved)),
            preferences().layout.clone(),
            1,
        )
        .expect("a valid record from the other computer's side")
    }

    /// An app controller with a connected fake link to `peer` showing `inspection`, mirroring the
    /// setup file at `path` first as a supervisor pass does.
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
        controller
            .sharing
            .adopt_saved(&SetupFile::load(path).expect("a readable setup"));
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

    /// Waits for what a link event, handled on the link's own thread, makes true.
    fn wait_until(ready: impl Fn() -> bool) {
        for _ in 0..400 {
            if ready() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the condition was never reached");
    }

    /// The summary a link carries now: the last one its computer told it.
    fn carried_summary(fixture: &LinkFixture) -> Vec<u8> {
        let mut bytes = fixture
            .summaries
            .recv_timeout(Duration::from_secs(1))
            .expect("every link carries this computer's summary");
        while let Ok(newer) = fixture.summaries.try_recv() {
            bytes = newer;
        }
        bytes
    }

    /// Hands each computer the summary the other's link carries, as both links do on connect.
    fn exchange_summaries(
        one: (&AppController, &LinkFixture, &InspectedPeer),
        other: (&AppController, &LinkFixture, &InspectedPeer),
    ) {
        let (first, second) = (carried_summary(one.1), carried_summary(other.1));
        for ((controller, link, view), bytes) in [(one, second), (other, first)] {
            link.events.send(LinkEvent::PeerSummary { bytes }).unwrap();
            wait_until(|| controller.sharing.peer_stamp_for(view).is_some());
        }
    }

    /// A setup file on the fixture network holding `record` for `local`, the rest enabled.
    fn file_holding(local: &str, record: &GroupRecord) -> SetupFile {
        let mut file = SetupFile::default();
        file.set_interface_id("en0:4:192.168.1.4");
        file.adopt(local, record.clone()).unwrap();
        file
    }

    fn stop(controller: &AppController) {
        controller.sharing.stop_with("Stopped.");
        join_finished_worker(&controller.sharing);
    }

    #[test]
    fn the_newer_record_is_proposed_by_its_holder_and_the_older_waits() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("newer-proposes");
        let (a_path, b_path) = (
            folder.join("a").join("sharing.json"),
            folder.join("b").join("sharing.json"),
        );
        let a_view = inspection(&preferences());
        let b_view = opposite(&a_view);
        // A has the lower DeviceId, yet holds the older record, made while its display sat
        // elsewhere; B's newer one fits both computers.
        assert!(local_decides(&a_view));
        let mut moved = preferences();
        moved.move_local_display_for_test("1", [0.0, 240.0]);
        let older = group(&moved);
        let newer = group(&preferences()).restamped(5, &"B".repeat(64)).unwrap();
        assert!(!older.fits_link(&a_view));
        assert!(newer.fits_link(&b_view));
        file_holding(&"A".repeat(64), &older).save(&a_path).unwrap();
        file_holding(&"B".repeat(64), &newer).save(&b_path).unwrap();
        let (a, a_link) = linked_controller(&a_path, fingerprint('B'), &a_view);
        let (b, b_link) = linked_controller(&b_path, fingerprint('A'), &b_view);
        settle_displays(&a.sharing);
        settle_displays(&b.sharing);
        // Nothing is decided before the other computer's summary is in.
        assert_eq!(
            a.decide_on_link(&a_path, Some(&older), &a_view).unwrap(),
            None
        );
        assert_eq!(
            b.decide_on_link(&b_path, Some(&newer), &b_view).unwrap(),
            None
        );
        exchange_summaries((&a, &a_link, &a_view), (&b, &b_link, &b_view));

        // The older record's holder waits and says the other computer is choosing, since its
        // own no longer fits; it adapts and writes nothing.
        assert!(
            a.decide_on_link(&a_path, Some(&older), &a_view)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            a.sharing.status().display_notice,
            Some(DisplayNotice::PeerDeciding)
        );
        assert_eq!(
            a.decide_on_link(&a_path, Some(&older), &a_view).unwrap(),
            None
        );
        assert_eq!(
            SetupFile::load(&a_path).unwrap().active_group(),
            Some(&older)
        );
        assert!(!a_path.with_file_name(ARRANGEMENTS_FILE).exists());
        // The newer record's holder proposes it as it is: it fits both computers.
        assert!(
            b.decide_on_link(&b_path, Some(&newer), &b_view)
                .unwrap()
                .is_some()
        );
        let bytes = b_link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the newer record's holder proposes it");
        assert_eq!(
            crate::group_record::shared_group_for_link(&b_view, &bytes)
                .unwrap()
                .0,
            newer
        );
        // An older record that fits is not proposed either, and raises nothing.
        a.sharing.clear_display_notice();
        let fitting_older = group(&preferences())
            .with_control([("a".repeat(64), true), ("b".repeat(64), false)].into())
            .unwrap();
        assert!(fitting_older.fits_link(&a_view));
        assert_eq!(
            a.decide_on_link(&a_path, Some(&fitting_older), &a_view)
                .unwrap(),
            None
        );
        assert!(a.sharing.status().display_notice.is_none());
        assert!(a_link.proposals.try_recv().is_err());
        assert!(b_link.proposals.try_recv().is_err());
        // The older side takes the proposal: it holds nothing newer.
        (a_link.persist.stage)(&a_view, &bytes).unwrap();
        stop(&a);
        stop(&b);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn equal_fitting_records_are_confirmed_by_the_lower_device() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("equal-confirmed");
        let (a_path, b_path) = (
            folder.join("a").join("sharing.json"),
            folder.join("b").join("sharing.json"),
        );
        let a_view = inspection(&preferences());
        let b_view = opposite(&a_view);
        // Both hold one layout; B chose its computer again since, so its copy's stamp is newer.
        let agreed = group(&preferences());
        let touched = agreed.restamped(4, &"B".repeat(64)).unwrap();
        assert!(touched.stamp() > agreed.stamp());
        file_holding(&"A".repeat(64), &agreed)
            .save(&a_path)
            .unwrap();
        file_holding(&"B".repeat(64), &touched)
            .save(&b_path)
            .unwrap();
        let (a, a_link) = linked_controller(&a_path, fingerprint('B'), &a_view);
        let (b, b_link) = linked_controller(&b_path, fingerprint('A'), &b_view);
        settle_displays(&a.sharing);
        settle_displays(&b.sharing);
        exchange_summaries((&a, &a_link, &a_view), (&b, &b_link, &b_view));

        // The same content: the higher DeviceId waits whatever its stamp, and says nothing.
        assert_eq!(
            b.decide_on_link(&b_path, Some(&touched), &b_view).unwrap(),
            None
        );
        assert!(b.sharing.status().display_notice.is_none());
        // The lower confirms its record as it is, with no new stamp.
        assert!(
            a.decide_on_link(&a_path, Some(&agreed), &a_view)
                .unwrap()
                .is_some()
        );
        let bytes = a_link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the lower DeviceId confirms");
        let (confirmed, left_out) =
            crate::group_record::shared_group_for_link(&a_view, &bytes).unwrap();
        assert_eq!(confirmed, agreed);
        assert!(!left_out);
        assert!(b_link.proposals.try_recv().is_err());
        // Staged and committed on both; each file keeps its greater stamp of the one content.
        for (link, view) in [(&a_link, &a_view), (&b_link, &b_view)] {
            (link.persist.stage)(view, &bytes).unwrap();
            (link.persist.commit)(view, &bytes).unwrap();
        }
        assert_eq!(
            SetupFile::load(&a_path).unwrap().active_group(),
            Some(&agreed)
        );
        assert_eq!(
            SetupFile::load(&b_path).unwrap().active_group(),
            Some(&touched)
        );
        stop(&a);
        stop(&b);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn equal_records_that_do_not_fit_are_refit_by_the_lower_device() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("equal-refit");
        let (a_path, b_path) = (
            folder.join("a").join("sharing.json"),
            folder.join("b").join("sharing.json"),
        );
        let agreed = group(&preferences());
        file_holding(&"A".repeat(64), &agreed)
            .save(&a_path)
            .unwrap();
        file_holding(&"B".repeat(64), &agreed)
            .save(&b_path)
            .unwrap();
        // A's display moved, and the link reports it to both.
        let mut moved = preferences();
        moved.move_local_display_for_test("1", [0.0, 240.0]);
        let a_view = inspection(&moved);
        let b_view = opposite(&a_view);
        assert!(!agreed.fits_link(&a_view));
        let (a, a_link) = linked_controller(&a_path, fingerprint('B'), &a_view);
        let (b, b_link) = linked_controller(&b_path, fingerprint('A'), &b_view);
        settle_displays(&a.sharing);
        settle_displays(&b.sharing);
        exchange_summaries((&a, &a_link, &a_view), (&b, &b_link, &b_view));

        // The higher DeviceId waits and says the other computer is choosing.
        assert!(
            b.decide_on_link(&b_path, Some(&agreed), &b_view)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            b.sharing.status().display_notice,
            Some(DisplayNotice::PeerDeciding)
        );
        assert!(b_link.proposals.try_recv().is_err());
        // The lower rebuilds the record for the displays both show, as its newest change.
        assert!(
            a.decide_on_link(&a_path, Some(&agreed), &a_view)
                .unwrap()
                .is_some()
        );
        let bytes = a_link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the lower DeviceId refits");
        let (refit, left_out) =
            crate::group_record::shared_group_for_link(&a_view, &bytes).unwrap();
        assert!(!left_out);
        assert!(refit.fits_link(&a_view));
        assert_eq!(
            (refit.revision(), refit.author()),
            (agreed.revision() + 1, "A".repeat(64).as_str())
        );
        assert_eq!(
            a.sharing.status().display_notice,
            Some(DisplayNotice::Updating)
        );
        for (link, view) in [(&a_link, &a_view), (&b_link, &b_view)] {
            (link.persist.stage)(view, &bytes).unwrap();
            (link.persist.commit)(view, &bytes).unwrap();
        }
        for path in [&a_path, &b_path] {
            assert_eq!(SetupFile::load(path).unwrap().active_group(), Some(&refit));
        }
        stop(&a);
        stop(&b);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_migrated_pair_that_disagreed_converges_to_the_old_deciders_layout() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("migrated-disagreed");
        let (mac_path, pc_path) = (
            folder.join("mac").join("sharing.json"),
            folder.join("pc").join("sharing.json"),
        );
        // The Mac's copy of the pair drifted before the upgrade: there, the PC may not control it.
        let mut drifted: serde_json::Value = serde_json::from_str(V3_FILE).unwrap();
        drifted["computers"][fingerprint_key(WINDOWS_PC)]["layout"]["control"]
            [fingerprint_key(WINDOWS_PC)] = false.into();
        let mac = load_as(&drifted.to_string(), Platform::MacOs);
        let pc = load_as(&mirrored_v3_file(), Platform::Windows);
        let mac_record = mac.active_group().unwrap().clone();
        let pc_record = pc.active_group().unwrap().clone();
        assert_ne!(mac_record.content_digest(), pc_record.content_digest());
        mac.save(&mac_path).unwrap();
        pc.save(&pc_path).unwrap();
        let pc_view = link_of(&pc_record, WINDOWS_PC, LOCAL);
        let mac_view = opposite(&pc_view);
        // The PC decided before the upgrade, so its own copy is the newer one; both still fit.
        assert!(local_decides(&pc_view));
        assert!(pc_record.stamp() > mac_record.stamp());
        assert!(mac_record.fits_link(&mac_view));
        let parsed = |fingerprint: &str| CertificateFingerprint::parse_full(fingerprint).unwrap();
        let (mac_side, mac_link) = linked_controller(&mac_path, parsed(WINDOWS_PC), &mac_view);
        let (pc_side, pc_link) = linked_controller(&pc_path, parsed(LOCAL), &pc_view);
        settle_displays(&mac_side.sharing);
        settle_displays(&pc_side.sharing);
        exchange_summaries(
            (&mac_side, &mac_link, &mac_view),
            (&pc_side, &pc_link, &pc_view),
        );

        assert_eq!(
            mac_side
                .decide_on_link(&mac_path, Some(&mac_record), &mac_view)
                .unwrap(),
            None
        );
        assert!(
            pc_side
                .decide_on_link(&pc_path, Some(&pc_record), &pc_view)
                .unwrap()
                .is_some()
        );
        let bytes = pc_link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the old decider's newer copy is proposed");
        assert!(mac_link.proposals.try_recv().is_err());
        for (link, view) in [(&pc_link, &pc_view), (&mac_link, &mac_view)] {
            (link.persist.stage)(view, &bytes).unwrap();
            (link.persist.commit)(view, &bytes).unwrap();
        }
        // Both now hold the PC's layout, the one it decided before the upgrade.
        for path in [&mac_path, &pc_path] {
            assert_eq!(
                SetupFile::load(path).unwrap().active_group(),
                Some(&pc_record)
            );
        }
        stop(&mac_side);
        stop(&pc_side);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_control_flip_is_proposed_by_the_flipper_whatever_the_stamps() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("flip-proposed");
        let (a_path, b_path) = (
            folder.join("a").join("sharing.json"),
            folder.join("b").join("sharing.json"),
        );
        let a_view = inspection(&preferences());
        let b_view = opposite(&a_view);
        // A holds a newer, different record: only B may control the other there.
        let older = group(&preferences());
        let newer = older
            .with_control([("a".repeat(64), false), ("b".repeat(64), true)].into())
            .and_then(|record| record.restamped(5, &"A".repeat(64)))
            .unwrap();
        file_holding(&"A".repeat(64), &newer).save(&a_path).unwrap();
        file_holding(&"B".repeat(64), &older).save(&b_path).unwrap();
        let (a, a_link) = linked_controller(&a_path, fingerprint('B'), &a_view);
        let (b, b_link) = linked_controller(&b_path, fingerprint('A'), &b_view);
        exchange_summaries((&a, &a_link, &a_view), (&b, &b_link, &b_view));
        // B's user turns off B's own control of A.
        b.sharing
            .set_control(&b_path, &"a".repeat(64), "localToPeer", false)
            .unwrap();
        assert!(b.sharing.control_pending());

        // B holds the older record, yet proposes: the flip is the newest choice there is.
        assert!(
            b.decide_on_link(&b_path, Some(&older), &b_view)
                .unwrap()
                .is_some()
        );
        let bytes = b_link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the flipper proposes");
        let (flipped, _) = crate::group_record::shared_group_for_link(&b_view, &bytes).unwrap();
        assert_eq!(
            flipped.layout().control,
            ControlMap::from([("a".repeat(64), true), ("b".repeat(64), false)])
        );
        // Stamped past everything B saw, A's newer record included, so A stages it over its own.
        assert_eq!(
            (flipped.revision(), flipped.author()),
            (newer.revision() + 1, "B".repeat(64).as_str())
        );
        assert!(flipped.stamp() > newer.stamp());
        (a_link.persist.stage)(&a_view, &bytes).unwrap();
        stop(&a);
        stop(&b);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn two_computers_make_one_proposal_and_one_commit_before_a_session() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("two-computers");
        let decider_path = folder.join("decider").join("sharing.json");
        let other_path = folder.join("other").join("sharing.json");
        let fitting = group(&preferences());
        let decider_view = inspection(&preferences());
        let other_view = opposite(&decider_view);
        assert!(local_decides(&decider_view));
        assert!(!local_decides(&other_view));
        let stale = stale_record_of_the_other_computer();
        assert!(fitting.stamp() > stale.stamp());
        file_with(preferences()).save(&decider_path).unwrap();
        let mut other_file = SetupFile::default();
        other_file.set_interface_id(&other_view.interface_id);
        other_file
            .store_agreed(&"B".repeat(64), stale.clone())
            .unwrap();
        other_file.save(&other_path).unwrap();
        let (decider, decider_link) =
            linked_controller(&decider_path, fingerprint('B'), &decider_view);
        let (other, other_link) = linked_controller(&other_path, fingerprint('A'), &other_view);
        exchange_summaries(
            (&decider, &decider_link, &decider_view),
            (&other, &other_link, &other_view),
        );

        // The other computer's record is older and stale: it waits and proposes nothing.
        assert!(
            other
                .decide_on_link(&other_path, Some(&stale), &other_view)
                .unwrap()
                .is_some()
        );
        // The newer record's holder waits for the displays to hold still, then proposes once.
        assert_eq!(
            decider
                .decide_on_link(&decider_path, Some(&fitting), &decider_view)
                .unwrap(),
            None
        );
        settle_displays(&decider.sharing);
        assert!(
            decider
                .decide_on_link(&decider_path, Some(&fitting), &decider_view)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            decider
                .decide_on_link(&decider_path, Some(&fitting), &decider_view)
                .unwrap(),
            None
        );
        let bytes = decider_link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the decider proposes its record");
        assert!(decider_link.proposals.try_recv().is_err());
        assert!(other_link.proposals.try_recv().is_err());
        // Its record fits, so it goes as it is, with its own stamp.
        assert_eq!(
            crate::group_record::shared_group_for_link(&decider_view, &bytes)
                .unwrap()
                .0,
            fitting
        );

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
        // Only the sender leaves, after the commit; the other computer waits for that close.
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
            assert_eq!(file.active_group(), Some(&fitting));
            let plan = SessionPlan::of(&file).expect("the active computer has a record");
            assert!(plan.fits_local(&view.local_displays));
            assert!(plan.topology(view).is_some());
            assert!(!controller.sharing.holds_link(view.peer_fingerprint));
            assert!(controller.sharing.link_is_off());
            assert!(controller.sharing.status().display_notice.is_none());
        }
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_transition_line_names_displays_record_and_decider_but_no_identity() {
        let saved = group(&preferences());
        let (local, peer) = ("A".repeat(64), "B".repeat(64));
        let here = inspection(&preferences());
        let line = transition("sent", Some((&saved, &local)), Some(&here));
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
        let there = transition("waiting", Some((&saved, &peer)), Some(&opposite(&here)));
        assert!(there.ends_with("; decider the other computer]"), "{there}");
        // Without a link the record names both sides and the decider itself.
        let unlinked = transition("started", Some((&saved, &local)), None);
        assert_eq!(
            unlinked,
            line.replacen("sent", "started", 1),
            "the record holds the displays the link reports"
        );
        let from_there = transition("started", Some((&saved, &peer)), None);
        assert!(
            from_there.starts_with("started [this computer 1 display #"),
            "{from_there}"
        );
        assert!(
            from_there.ends_with("; decider the other computer]"),
            "{from_there}"
        );
        assert_eq!(
            transition("idle", None, None),
            "idle [no displays known; record none; decider unknown]"
        );
    }

    #[test]
    fn migrated_pair_with_identical_layouts_starts_sharing_without_a_link() {
        // Both computers upgrade with the displays they had: each migrates its own file.
        let mac = load_as(V3_FILE, Platform::MacOs);
        let pc = load_as(&mirrored_v3_file(), Platform::Windows);
        let mac_plan = SessionPlan::of(&mac).expect("the Mac shares with the PC it chose");
        let pc_plan = SessionPlan::of(&pc).expect("the PC shares with the Mac it chose");
        let displays = |plan: &SessionPlan, owner: &str| {
            topology_of(plan.record().member(owner).unwrap().displays()).unwrap()
        };
        // Each finds its own displays as its record holds them, so an idle pass shares at once.
        assert_eq!(mac_plan.local(), LOCAL);
        assert_eq!(pc_plan.local(), WINDOWS_PC);
        assert!(mac_plan.fits_local(&displays(&mac_plan, LOCAL)));
        assert!(pc_plan.fits_local(&displays(&pc_plan, WINDOWS_PC)));
        // The session each dials meets the displays both records hold, and both offer the same
        // permissions: nothing sends either computer to a link.
        let mac_view = link_of(mac_plan.record(), LOCAL, WINDOWS_PC);
        let pc_view = opposite(&mac_view);
        assert!(mac_plan.topology(&mac_view).is_some());
        assert!(pc_plan.topology(&pc_view).is_some());
        assert!(mac_plan.control().is_some());
        assert_eq!(mac_plan.control(), pc_plan.control());
        assert_eq!(
            mac_plan.record().content_digest(),
            pc_plan.record().content_digest()
        );
        let controller = AppController::default();
        controller.sharing.adopt_saved(&mac);
        assert!(!controller.sharing.holds_link(mac_view.peer_fingerprint));
    }

    #[test]
    fn an_automatic_memory_switches_back_in_for_the_same_displays() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("memory-switches-back");
        let path = folder.join("sharing.json");
        // The pair was arranged with a second monitor under this computer's display, then
        // again without it, and runs on the second layout.
        let docked = two_display_record();
        let running = group(&preferences());
        let mut library = ArrangementLibrary::default();
        for record in [group(&docked), running.clone()] {
            library
                .remember_automatically(record, &"A".repeat(64))
                .unwrap();
        }
        library
            .save(&path.with_file_name(ARRANGEMENTS_FILE))
            .unwrap();
        // The monitor comes back, under a new id.
        let mut back = docked.clone();
        back.set_local_display_id_for_test("3", "7");
        let here = inspection(&back);
        assert!(local_decides(&here));
        assert!(!running.fits_link(&here));
        let (controller, link) = linked_controller(&path, fingerprint('B'), &here);
        settle_displays(&controller.sharing);
        // The other computer runs the same record, so the lower DeviceId, this one, refits it.
        let theirs = RecordSummary::new(&[&"A".repeat(64), &"B".repeat(64)], Some(&running))
            .and_then(|summary| summary.to_bytes())
            .unwrap();
        link.events
            .send(LinkEvent::PeerSummary { bytes: theirs })
            .unwrap();
        wait_until(|| controller.sharing.peer_stamp_for(&here).is_some());
        assert!(
            controller
                .decide_on_link(&path, Some(&running), &here)
                .unwrap()
                .is_some()
        );
        let bytes = link
            .proposals
            .recv_timeout(Duration::from_secs(1))
            .expect("the lower DeviceId proposes the memory for these monitors");
        let (proposed, left_out) =
            crate::group_record::shared_group_for_link(&here, &bytes).unwrap();
        // The memory comes back whole, crossing to the id the monitor carries now, instead of the
        // running layout rebuilt around a monitor it never crossed to; nothing was left out.
        assert!(!left_out);
        assert_eq!(proposed.layout().links.len(), docked.layout.links.len());
        assert_eq!(proposed.layout().links[0].to_display, "7");
        assert_eq!(proposed.layout().links[1].from_display, "7");
        assert_eq!(proposed.author(), "A".repeat(64));
        assert!(proposed.fits_link(&here));
        controller.sharing.stop_with("Stopped.");
        join_finished_worker(&controller.sharing);
        let _ = std::fs::remove_dir_all(folder);
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
        library.upsert("Desk", group(&saved)).unwrap();
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
        assert!(controller.forget_arrangement(&other, "Desk", None).is_err());
        assert!(controller.arrangements_for("not a fingerprint").is_err());
        assert!(
            controller
                .forget_arrangement(&peer, "Desk", None)
                .unwrap()
                .is_empty()
        );
        assert!(controller.arrangements_for(&peer).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn enabling_a_third_computer_needs_an_arrangement_when_no_record_exists() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("third-computer");
        let path = folder.join("sharing.json");
        let (b, c) = ("b".repeat(64), "c".repeat(64));
        // The pair shares; no network is recorded, so no supervisor pass dials anything.
        let pair = group(&preferences());
        let mut file = SetupFile::default();
        file.adopt(&"A".repeat(64), pair.clone()).unwrap();
        file.save(&path).unwrap();
        let controller = AppController::default();
        controller.use_setup_path(path.clone());

        let view = serde_json::to_value(controller.set_enabled(&c, true, None).unwrap()).unwrap();
        assert_eq!(view["enabled"], serde_json::json!([b, c]));
        assert_eq!(view["members"], serde_json::json!([]));
        assert!(view["control"].is_null());
        // No record names the three, and nothing is made up for them: they need arranging.
        let saved = SetupFile::load(&path).unwrap();
        assert!(saved.active_group().is_none());
        assert_eq!(saved.groups(), std::slice::from_ref(&pair));
        let computers = serde_json::to_value(controller.computers_load().unwrap()).unwrap();
        assert_eq!(computers["enabled"], serde_json::json!([b, c]));
        assert_eq!(computers["paused"], false);
        assert!(computers["group"].is_null());
        assert!(computers["active"].is_null());
        let card = computers["computers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|computer| computer["fingerprint"] == b)
            .expect("the pair's computer has a card");
        assert_eq!(card["enabled"], true);
        assert_eq!(card["member"], false);
        assert_eq!(card["setup"]["saved"], true);

        // Switching C off again goes back to the pair's record, touched.
        controller.set_enabled(&c, false, None).unwrap();
        let saved = SetupFile::load(&path).unwrap();
        let active = saved.active_group().expect("the pair's record");
        assert!(active.same_content_as(&pair));
        assert_eq!(active.revision(), pair.revision() + 1);
        let computers = serde_json::to_value(controller.computers_load().unwrap()).unwrap();
        assert_eq!(computers["active"], b);
        assert_eq!(
            computers["group"]["members"][1]["fingerprint"],
            serde_json::json!(b)
        );
        assert_eq!(
            computers["group"]["recordRevision"],
            serde_json::json!(active.revision())
        );
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn arrangement_forget_disambiguates_by_members() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("forget-by-members");
        let setup_path = folder.join("sharing.json");
        let controller = AppController::default();
        controller.use_setup_path(setup_path.clone());
        let (a, b, c) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        // Two entries named alike both include B: the pair's and the three's.
        let mut library = ArrangementLibrary::default();
        library.upsert("Desk", group(&preferences())).unwrap();
        library.upsert("Desk", trio(1, true)).unwrap();
        library
            .save(&setup_path.with_file_name(ARRANGEMENTS_FILE))
            .unwrap();
        let members = |entries: &[crate::arrangement_library::ArrangementView]| {
            entries
                .iter()
                .map(|entry| entry.members.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(controller.arrangements_for(&b).unwrap().len(), 2);

        // The members name the one to forget, in any case and order.
        let three = [c.to_uppercase(), a.clone(), b.clone()];
        let left = controller
            .forget_arrangement(&b, "Desk", Some(&three[..]))
            .unwrap();
        assert_eq!(members(&left), [vec![a.clone(), b.clone()]]);
        // Members naming no entry, or not a computer at all, forget nothing.
        assert!(
            controller
                .forget_arrangement(&b, "Desk", Some(&three[..]))
                .is_err()
        );
        assert!(
            controller
                .forget_arrangement(&b, "Desk", Some(&["not a fingerprint".to_owned()][..]))
                .is_err()
        );
        assert_eq!(controller.arrangements_for(&b).unwrap().len(), 1);
        // Without members the only entry of that name goes.
        assert!(
            controller
                .forget_arrangement(&b, "Desk", None)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_newer_file_is_reported_in_the_view() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let folder = folder("newer-files");
        let setup_path = folder.join("sharing.json");
        let controller = AppController::default();
        controller.use_setup_path(setup_path.clone());
        let flags = |controller: &AppController| {
            let view = serde_json::to_value(controller.computers_load().unwrap()).unwrap();
            (
                view["setupWrittenByNewer"].as_bool(),
                view["arrangementsWrittenByNewer"].as_bool(),
            )
        };
        assert_eq!(flags(&controller), (Some(false), Some(false)));
        std::fs::write(&setup_path, br#"{"version":99}"#).unwrap();
        assert_eq!(flags(&controller), (Some(true), Some(false)));
        std::fs::write(
            setup_path.with_file_name(ARRANGEMENTS_FILE),
            br#"{"version":99,"arrangements":[]}"#,
        )
        .unwrap();
        assert_eq!(flags(&controller), (Some(true), Some(true)));
        // Both stay as they are: this version never saves over either.
        assert_eq!(
            std::fs::read(&setup_path).unwrap(),
            br#"{"version":99}"#.to_vec()
        );
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
        assert!(controller.forget_arrangement(&peer, "Desk", None).is_err());
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
                .edit_begin("invalid".into(), Some(&"B".repeat(64)))
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
                .edit_begin("unused-network".into(), Some(&"B".repeat(64)))
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
