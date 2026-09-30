//! The sharing network thread. One current-thread runtime owns the group endpoint bound to the
//! chosen network, the share hub for the active group, each computer's Share connect and every
//! setup link. The controller reaches it only through commands and answers: nothing here touches
//! the controller's state, and nothing that can block for long runs here.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use monhop_core::{DeviceId, NativeSessionClaim, RevocationSignal};
use monhop_protocol::SessionPurpose;
use monhop_transport::{
    crypto::CertificateFingerprint,
    session::{SessionDiagnostics, SessionFailure, SessionProgress},
    session_handshake::{NegotiatedSession, device_id_from_fingerprint},
    session_hub::{HubClosed, HubConfig, HubEvent, HubEvents, PeerEnd, ShareHub, start_share_hub},
    session_link::{LinkCommand, LinkEvent, LinkPersist, run_setup_link_on},
    session_setup::{GroupEndpoint, GroupMemberRecord, InspectedPeer, SetupFailure},
};
use tokio::{
    sync::{
        Notify,
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    task::JoinHandle,
};

use crate::{
    clipboard::{ClipboardAttachment, ClipboardHub},
    group_record::{GroupRecord, wire_control},
    sharing::{LinkFuture, validated_layout},
    sharing_preferences::{fingerprint_key, topology_of},
};

const NETWORK_THREAD: &str = "monhop-network";
/// How often a running hub reads its endpoint's revocation. The endpoint's signal wakes only its
/// socket, so nothing can wait on it instead.
const BRIDGE_POLL: Duration = Duration::from_millis(10);
/// A hub stopped because the group's record changed: the other computers read it as a resync.
const RECORD_CHANGED: PeerEnd = PeerEnd {
    deliberate: true,
    control_change: true,
};
const DELIBERATE: PeerEnd = PeerEnd {
    deliberate: true,
    control_change: false,
};

/// Native capture routes under a generation no earlier hub in this process used.
static NEXT_HUB_GENERATION: AtomicU64 = AtomicU64::new(1);

/// The clipboard hub the app starts, once it has; a Share session attaches to it.
pub(crate) type ClipboardSlot = Arc<Mutex<Option<Arc<ClipboardHub>>>>;

pub(crate) type LocalFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// The group one hub runs for: this computer, the record every member agreed on, and that
/// record's content digest, which every Share Hello binds.
#[derive(Clone)]
pub(crate) struct GroupPlan {
    /// Moves on whenever the controller's group changes; an older plan never replaces a newer.
    pub(crate) epoch: u64,
    /// This computer's fingerprint as the record names it.
    pub(crate) local: String,
    pub(crate) record: GroupRecord,
    pub(crate) agreement: [u8; 32],
}

impl GroupPlan {
    /// Every other member this computer may share with, and under which control. A pair where
    /// neither may control the other is left out, so it never gets a Share connection.
    fn member_controls(&self) -> Vec<GroupMemberRecord> {
        let control = &self.record.layout().control;
        self.record
            .members()
            .iter()
            .filter_map(|member| {
                Some(GroupMemberRecord {
                    fingerprint: CertificateFingerprint::parse_full(member.fingerprint()).ok()?,
                    control: wire_control(control, &self.local, member.fingerprint())?,
                })
            })
            .collect()
    }

    /// The whole group as this computer runs it; None when the record cannot build it here.
    fn hub_config(&self, generation: u64) -> Option<HubConfig> {
        let local = CertificateFingerprint::parse_full(&self.local).ok()?;
        let local_key = fingerprint_key(&self.local);
        let members = self
            .record
            .members()
            .iter()
            .filter(|member| fingerprint_key(member.fingerprint()) != local_key)
            .map(|member| {
                CertificateFingerprint::parse_full(member.fingerprint())
                    .ok()
                    .map(device_id_from_fingerprint)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(HubConfig {
            local: device_id_from_fingerprint(local),
            local_displays: topology_of(self.record.member(&self.local)?.displays())?,
            group: self.record.topology_for(&self.local)?,
            members,
            generation,
            revocation: RevocationSignal::default(),
        })
    }

    /// Whether a session that met `peer` as `inspection` shows runs under this record.
    fn fits(&self, peer: CertificateFingerprint, inspection: &InspectedPeer) -> bool {
        inspection.peer_fingerprint == peer
            && self.record.fits_link(inspection)
            && (self.record.members().len() != 2
                || validated_layout(inspection, self.record.layout()).is_ok())
    }
}

/// One Share connect for a share worker.
pub(crate) struct ShareRequest {
    pub(crate) interface_id: String,
    pub(crate) plan: GroupPlan,
    pub(crate) peer: CertificateFingerprint,
    /// The worker's own cancel: revoking it ends the connect.
    pub(crate) cancel: RevocationSignal,
    pub(crate) progress: SessionProgress,
}

/// A Share session the hub took.
pub(crate) struct ShareUp {
    pub(crate) inspection: InspectedPeer,
    /// That session's own cancel: a stop on it ends this session alone, with its close reason.
    pub(crate) cancel: RevocationSignal,
    /// Set once both directions of the session finished startup.
    pub(crate) started: Arc<AtomicBool>,
    pub(crate) ended: oneshot::Receiver<ShareEnd>,
}

#[derive(Debug)]
pub(crate) enum ShareFailure {
    Setup(SetupFailure),
    /// The record does not fit the displays the connection met, or cannot build the group here.
    Misfit,
    /// A newer group replaced the one this connect was for.
    Stale,
    /// Native input from an earlier hub is still held.
    NativeCleanup,
    /// The hub for this record could not start.
    Hub(SessionFailure),
}

/// How one Share session ended.
pub(crate) struct ShareEnd {
    pub(crate) result: Result<(), SessionFailure>,
    pub(crate) diagnostics: SessionDiagnostics,
    /// The session's clipboard attachment. Detaching can wait up to a second for a clipboard
    /// read, so the network thread hands it over for the controller to drop.
    pub(crate) detached: Detached,
}

impl ShareEnd {
    /// The network thread went away without reporting the session's end.
    pub(crate) fn lost() -> Self {
        Self {
            result: Err(SessionFailure::Revoked),
            diagnostics: SessionDiagnostics::default(),
            detached: Detached(None),
        }
    }
}

/// Something only its drop matters for.
pub(crate) struct Detached(Option<Box<dyn Send>>);

impl Detached {
    fn of(value: impl Send + 'static) -> Self {
        Self(Some(Box::new(value)))
    }
}

/// Drops `detached` on a thread of its own when its receiver is gone: never here.
fn drop_off_thread(detached: Detached) {
    if detached.0.is_none() {
        return;
    }
    if let Err(error) = std::thread::Builder::new()
        .name("monhop-clipboard-detach".into())
        .spawn(move || drop(detached))
    {
        log::warn!("network: a clipboard attachment was released on the network thread: {error}");
    }
}

/// One setup link for a link worker.
pub(crate) struct SetupLinkRequest {
    pub(crate) interface_id: String,
    pub(crate) peer: CertificateFingerprint,
    pub(crate) cancel: RevocationSignal,
    pub(crate) persist: LinkPersist,
    pub(crate) events: UnboundedSender<LinkEvent>,
    pub(crate) commands: UnboundedReceiver<LinkCommand>,
}

enum Command {
    Group(Box<GroupPlan>),
    Share(
        Box<ShareRequest>,
        oneshot::Sender<Result<ShareUp, ShareFailure>>,
    ),
    /// The share worker for this computer ended.
    Release(String),
    Link(
        Box<SetupLinkRequest>,
        oneshot::Sender<Result<(), SetupFailure>>,
    ),
    Forget(CertificateFingerprint),
    /// Sent by the network thread to itself when a task ended; checks whether it is idle.
    Tick,
    Shutdown,
}

/// One network thread's state as the controller reads it. Each thread gets its own, so one still
/// ending never overwrites a newer thread's.
#[derive(Default)]
struct Flags {
    stopped: AtomicBool,
    /// Commands sent and not handled yet.
    in_flight: AtomicUsize,
    bound: AtomicBool,
    hub: AtomicBool,
    /// A hub took a session and has not reported native input idle since.
    native: AtomicBool,
    /// Times the thread's runtime woke.
    #[cfg(test)]
    wakes: AtomicUsize,
}

impl Flags {
    fn quiet(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
            || (self.in_flight.load(Ordering::Acquire) == 0
                && !self.bound.load(Ordering::Acquire)
                && !self.hub.load(Ordering::Acquire))
    }
}

type Launch = Box<
    dyn Fn(
            UnboundedReceiver<Command>,
            UnboundedSender<Command>,
            Arc<Flags>,
            Arc<AtomicBool>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>
        + Send
        + Sync,
>;

struct Running {
    commands: UnboundedSender<Command>,
    flags: Arc<Flags>,
    thread: std::thread::JoinHandle<()>,
}

impl Running {
    fn deliver(&self, command: Command) -> bool {
        self.flags.in_flight.fetch_add(1, Ordering::AcqRel);
        if self.commands.send(command).is_err() {
            self.flags.in_flight.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }
}

#[derive(Default)]
struct Threads {
    current: Option<Running>,
    /// Earlier threads that had not finished when a newer one started; the port is not free
    /// until they have.
    ending: Vec<std::thread::JoinHandle<()>>,
}

/// The controller's handle on the network thread, which starts with the first command.
pub(crate) struct SharingNetwork {
    launch: Launch,
    threads: Mutex<Threads>,
    shutdown: Arc<AtomicBool>,
    /// The latest group, handed to every thread before anything else so its first bind names
    /// the group's members.
    plan: Mutex<Option<GroupPlan>>,
}

impl SharingNetwork {
    /// The real transport, attaching Share sessions to the clipboard hub in `clipboard`.
    pub(crate) fn transport(clipboard: ClipboardSlot) -> Self {
        Self::with_port(move || TransportPort {
            clipboard: Arc::clone(&clipboard),
        })
    }

    /// A network thread driving the port `make` builds on that thread.
    pub(crate) fn with_port<P: ShareHubPort>(make: impl Fn() -> P + Send + Sync + 'static) -> Self {
        let make = Arc::new(make);
        Self {
            launch: Box::new(move |incoming, to_self, flags, shutdown| {
                let make = Arc::clone(&make);
                std::thread::Builder::new()
                    .name(NETWORK_THREAD.into())
                    .spawn(move || run_thread(make(), incoming, to_self, flags, shutdown))
            }),
            threads: Mutex::default(),
            shutdown: Arc::default(),
            plan: Mutex::default(),
        }
    }

    /// Sends `command`, starting the thread first when none runs. False once shut down or when
    /// the thread cannot start; the command's answer channel then reads as closed.
    fn send(&self, command: Command) -> bool {
        let mut threads = lock(&self.threads);
        if threads
            .current
            .as_ref()
            .is_none_or(|running| running.commands.is_closed())
        {
            if matches!(command, Command::Shutdown) || self.shutdown.load(Ordering::Acquire) {
                return false;
            }
            let (commands, incoming) = mpsc::unbounded_channel();
            let flags = Arc::new(Flags::default());
            let launched = (self.launch)(
                incoming,
                commands.clone(),
                Arc::clone(&flags),
                Arc::clone(&self.shutdown),
            );
            let thread = match launched {
                Ok(thread) => thread,
                Err(error) => {
                    log::warn!("network: the sharing network thread could not start: {error}");
                    return false;
                }
            };
            threads.ending.retain(|thread| !thread.is_finished());
            let running = Running {
                commands,
                flags,
                thread,
            };
            if let Some(plan) = lock(&self.plan).clone() {
                running.deliver(Command::Group(Box::new(plan)));
            }
            if let Some(earlier) = threads.current.replace(running) {
                threads.ending.push(earlier.thread);
            }
        }
        threads
            .current
            .as_ref()
            .is_some_and(|running| running.deliver(command))
    }

    fn started(&self) -> bool {
        lock(&self.threads).current.is_some()
    }

    /// The active group's record changed: a hub running under another one stops.
    pub(crate) fn set_group(&self, plan: GroupPlan) {
        {
            let mut latest = lock(&self.plan);
            if latest
                .as_ref()
                .is_some_and(|latest| latest.epoch >= plan.epoch)
            {
                return;
            }
            *latest = Some(plan.clone());
        }
        if self.started() {
            self.send(Command::Group(Box::new(plan)));
        }
    }

    /// Connects `request.peer` for sharing and hands the session to the hub. Resolves once the
    /// hub took it, or with why not.
    pub(crate) fn connect_share(
        &self,
        request: ShareRequest,
    ) -> impl Future<Output = Result<ShareUp, ShareFailure>> + use<> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Share(Box::new(request), reply));
        async move {
            answer
                .await
                .unwrap_or(Err(ShareFailure::Setup(SetupFailure::Cancelled)))
        }
    }

    /// The share worker for `peer` ended; the endpoint no longer serves it for sharing.
    pub(crate) fn release_share(&self, peer: CertificateFingerprint) {
        if self.started() {
            self.send(Command::Release(fingerprint_key(&peer.full_hex())));
        }
    }

    /// Runs a setup link on the shared endpoint. Dropping the future ends the link, which closes
    /// that computer's connection alone.
    pub(crate) fn open_link(&self, request: SetupLinkRequest) -> LinkFuture {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Link(Box::new(request), reply));
        Box::pin(async move { answer.await.unwrap_or(Err(SetupFailure::Cancelled)) })
    }

    /// A forgotten computer is no longer admitted, and its connection closes.
    pub(crate) fn forget(&self, peer: CertificateFingerprint) {
        if self.started() {
            self.send(Command::Forget(peer));
        }
    }

    /// Stops the hub, ends every link and releases the network; nothing starts again.
    pub(crate) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.send(Command::Shutdown);
    }

    /// Nothing bound, no hub, and nothing on its way that could bind: the port is free.
    pub(crate) fn is_quiet(&self) -> bool {
        let threads = lock(&self.threads);
        threads
            .ending
            .iter()
            .all(std::thread::JoinHandle::is_finished)
            && threads
                .current
                .as_ref()
                .is_none_or(|running| running.flags.quiet())
    }

    /// True while a hub may legitimately hold the native input claim.
    pub(crate) fn holds_native(&self) -> bool {
        lock(&self.threads).current.as_ref().is_some_and(|running| {
            running.flags.native.load(Ordering::Acquire)
                && !running.flags.stopped.load(Ordering::Acquire)
        })
    }

    /// Times the current thread's runtime woke.
    #[cfg(test)]
    pub(crate) fn wakes(&self) -> usize {
        lock(&self.threads)
            .current
            .as_ref()
            .map_or(0, |running| running.flags.wakes.load(Ordering::Acquire))
    }
}

/// One member's Share connection as a port hands it over.
pub(crate) struct PortMember<S> {
    pub(crate) session: S,
    pub(crate) inspection: InspectedPeer,
    pub(crate) cancel: RevocationSignal,
}

/// The transport the network thread drives; tests swap it for a fake, as `LinkRunner` is
/// swapped for the setup link.
pub(crate) trait ShareHubPort: 'static {
    type Session: 'static;
    type Endpoint: PortEndpoint<Session = Self::Session>;
    type Hub: PortHub<Session = Self::Session>;
    type Events: PortEvents;
    type Attachment: Send + 'static;

    fn bind(
        &self,
        interface_id: &str,
        members: &[GroupMemberRecord],
        cancel: &RevocationSignal,
    ) -> Result<Rc<Self::Endpoint>, SetupFailure>;

    #[allow(clippy::type_complexity)]
    fn start_hub(
        &self,
        config: HubConfig,
    ) -> Result<
        (
            Self::Hub,
            Self::Events,
            LocalFuture<Result<(), SessionFailure>>,
        ),
        SessionFailure,
    >;

    fn attach(&self, member: CertificateFingerprint, session: &Self::Session) -> Self::Attachment;
}

pub(crate) trait PortEndpoint: 'static {
    type Session;

    fn revocation(&self) -> RevocationSignal;

    fn set_members(&self, members: &[GroupMemberRecord]);

    /// Whether `member` was admitted at bind and not forgotten since.
    fn admits(&self, member: CertificateFingerprint) -> bool;

    async fn connect(
        &self,
        member: CertificateFingerprint,
        agreement: [u8; 32],
        cancel: &RevocationSignal,
    ) -> Result<PortMember<Self::Session>, SetupFailure>;

    fn close_member(&self, member: CertificateFingerprint);

    fn forget(&self, member: CertificateFingerprint);

    /// Every other handle must be gone first.
    async fn retire(self: Rc<Self>);

    async fn setup_link(
        &self,
        member: CertificateFingerprint,
        cancel: &RevocationSignal,
        persist: LinkPersist,
        events: UnboundedSender<LinkEvent>,
        commands: UnboundedReceiver<LinkCommand>,
    ) -> Result<(), SetupFailure>;
}

pub(crate) trait PortHub: 'static {
    type Session;

    fn add_peer(
        &self,
        session: Self::Session,
        progress: SessionProgress,
        cancel: RevocationSignal,
    ) -> Result<(), HubClosed>;

    fn remove_peer(&self, peer: DeviceId, end: PeerEnd);

    fn stop(&self, end: PeerEnd);

    /// The hub's loop stopped taking commands: it is ending, and a session added now is lost.
    fn is_closed(&self) -> bool;
}

pub(crate) trait PortEvents: 'static {
    async fn recv(&mut self) -> Option<HubEvent>;
}

struct TransportPort {
    clipboard: ClipboardSlot,
}

struct TransportHub {
    hub: ShareHub,
    /// This computer, which never has a link in its own hub.
    local: DeviceId,
}

impl ShareHubPort for TransportPort {
    type Session = NegotiatedSession;
    type Endpoint = GroupEndpoint;
    type Hub = TransportHub;
    type Events = HubEvents;
    type Attachment = Option<ClipboardAttachment>;

    fn bind(
        &self,
        interface_id: &str,
        members: &[GroupMemberRecord],
        cancel: &RevocationSignal,
    ) -> Result<Rc<GroupEndpoint>, SetupFailure> {
        GroupEndpoint::bind(interface_id, members, cancel)
    }

    fn start_hub(
        &self,
        config: HubConfig,
    ) -> Result<
        (
            TransportHub,
            HubEvents,
            LocalFuture<Result<(), SessionFailure>>,
        ),
        SessionFailure,
    > {
        let local = config.local;
        let (hub, events, run) = start_share_hub(config)?;
        Ok((TransportHub { hub, local }, events, Box::pin(run.run())))
    }

    fn attach(
        &self,
        member: CertificateFingerprint,
        session: &NegotiatedSession,
    ) -> Option<ClipboardAttachment> {
        let hub = lock(&self.clipboard).clone()?;
        Some(hub.attach(member, session.connection.clone(), session.initial_epoch))
    }
}

impl PortEndpoint for GroupEndpoint {
    type Session = NegotiatedSession;

    fn revocation(&self) -> RevocationSignal {
        Self::revocation(self)
    }

    fn set_members(&self, members: &[GroupMemberRecord]) {
        Self::set_members(self, members);
    }

    fn admits(&self, member: CertificateFingerprint) -> bool {
        Self::admits(self, member)
    }

    async fn connect(
        &self,
        member: CertificateFingerprint,
        agreement: [u8; 32],
        cancel: &RevocationSignal,
    ) -> Result<PortMember<NegotiatedSession>, SetupFailure> {
        let paired = Self::connect(self, member, SessionPurpose::Share, agreement, cancel).await?;
        Ok(PortMember {
            session: paired.session,
            inspection: paired.inspection,
            cancel: paired.cancel,
        })
    }

    fn close_member(&self, member: CertificateFingerprint) {
        Self::close_member(self, member);
    }

    fn forget(&self, member: CertificateFingerprint) {
        Self::forget(self, member);
    }

    async fn retire(self: Rc<Self>) {
        Self::retire(self).await;
    }

    async fn setup_link(
        &self,
        member: CertificateFingerprint,
        cancel: &RevocationSignal,
        persist: LinkPersist,
        events: UnboundedSender<LinkEvent>,
        commands: UnboundedReceiver<LinkCommand>,
    ) -> Result<(), SetupFailure> {
        run_setup_link_on(self, member, cancel, persist, events, commands).await
    }
}

impl PortHub for TransportHub {
    type Session = NegotiatedSession;

    fn add_peer(
        &self,
        session: NegotiatedSession,
        progress: SessionProgress,
        cancel: RevocationSignal,
    ) -> Result<(), HubClosed> {
        self.hub.add_peer(session, progress, cancel)
    }

    fn remove_peer(&self, peer: DeviceId, end: PeerEnd) {
        let _ = self.hub.remove_peer(peer, end);
    }

    fn stop(&self, end: PeerEnd) {
        self.hub.stop(end);
    }

    fn is_closed(&self) -> bool {
        // The hub has no closed check of its own. Removing this computer ends nothing, and the
        // loop refuses it only once it stopped taking commands.
        self.hub
            .remove_peer(self.local, PeerEnd::default())
            .is_err()
    }
}

impl PortEvents for HubEvents {
    async fn recv(&mut self) -> Option<HubEvent> {
        Self::recv(self).await
    }
}

fn run_thread<P: ShareHubPort>(
    port: P,
    incoming: UnboundedReceiver<Command>,
    to_self: UnboundedSender<Command>,
    flags: Arc<Flags>,
    shutdown: Arc<AtomicBool>,
) {
    monhop_transport::session_threads::mark_time_sensitive();
    let finished = Arc::clone(&flags);
    let ran = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut builder = tokio::runtime::Builder::new_current_thread();
        builder.enable_all();
        #[cfg(test)]
        {
            let flags = Arc::clone(&flags);
            builder.on_thread_unpark(move || {
                flags.wakes.fetch_add(1, Ordering::AcqRel);
            });
        }
        let runtime = builder
            .build()
            .map_err(|_| log::warn!("network: the sharing runtime could not start"))?;
        let local = tokio::task::LocalSet::new();
        local.block_on(
            &runtime,
            run_network(port, incoming, to_self, flags, shutdown),
        );
        Ok::<(), ()>(())
    }));
    if ran.is_err() {
        log::warn!("network: the sharing network thread stopped unexpectedly");
    }
    finished.bound.store(false, Ordering::Release);
    finished.hub.store(false, Ordering::Release);
    finished.native.store(false, Ordering::Release);
    finished.in_flight.store(0, Ordering::Release);
    finished.stopped.store(true, Ordering::Release);
}

struct Net<P: ShareHubPort> {
    port: P,
    flags: Arc<Flags>,
    shutdown: Arc<AtomicBool>,
    to_self: UnboundedSender<Command>,
    plan: Option<GroupPlan>,
    endpoint: Option<Bound<P::Endpoint>>,
    hub: Option<RunningHub<P::Hub>>,
    /// Hub runs not finished yet, the current hub's included. A hub starts only when none is
    /// left, so two never hold native input at once.
    hub_runs: usize,
    /// Signalled each time a hub run finishes.
    hub_ended: Rc<Notify>,
    /// A session of the current hub started since its native input last went idle, so native
    /// input ran and only its idle report says it let go.
    native_started: bool,
    /// By lowercase fingerprint.
    members: BTreeMap<String, Member<P::Attachment>>,
}

struct Bound<E> {
    endpoint: Rc<E>,
    interface_id: String,
    /// The group members the bind named, which won their recorded addresses over other records.
    named: Vec<CertificateFingerprint>,
}

impl<E: PortEndpoint> Bound<E> {
    /// Whether `plan` names a member this endpoint left out without having named it at bind;
    /// named, it would win its recorded address, so only a new bind can admit it.
    fn left_out(&self, plan: Option<&GroupPlan>) -> bool {
        plan.is_some_and(|plan| {
            plan.member_controls().iter().any(|member| {
                !self.named.contains(&member.fingerprint)
                    && !self.endpoint.admits(member.fingerprint)
            })
        })
    }
}

struct RunningHub<H> {
    id: u64,
    handle: H,
    agreement: [u8; 32],
    run: JoinHandle<()>,
    events: JoinHandle<()>,
}

struct Member<A> {
    fingerprint: CertificateFingerprint,
    device: DeviceId,
    /// A share worker holds this computer until it lets go.
    enabled: bool,
    connects: Vec<JoinHandle<()>>,
    session: Option<Session<A>>,
    links: Vec<JoinHandle<()>>,
    /// Hubs that still owe an end report for a session replaced here. That report belongs to
    /// the replaced session, so it is dropped instead of ending the one that replaced it.
    owed: Vec<u64>,
}

struct Session<A> {
    hub: u64,
    attachment: A,
    ended: oneshot::Sender<ShareEnd>,
    started: Arc<AtomicBool>,
}

type Shared<P> = Rc<RefCell<Net<P>>>;

impl<P: ShareHubPort> Net<P> {
    fn member(&mut self, peer: CertificateFingerprint) -> &mut Member<P::Attachment> {
        self.members
            .entry(fingerprint_key(&peer.full_hex()))
            .or_insert_with(|| Member {
                fingerprint: peer,
                device: device_id_from_fingerprint(peer),
                enabled: false,
                connects: Vec::new(),
                session: None,
                links: Vec::new(),
                owed: Vec::new(),
            })
    }

    fn idle(&mut self) -> bool {
        self.members.values_mut().all(|member| {
            member.connects.retain(|task| !task.is_finished());
            member.links.retain(|task| !task.is_finished());
            !member.enabled
                && member.session.is_none()
                && member.connects.is_empty()
                && member.links.is_empty()
        })
    }

    fn tick(&self) {
        let _ = self.to_self.send(Command::Tick);
    }

    fn sessions_of(&self, hub: u64) -> bool {
        self.members
            .values()
            .any(|member| member.session.as_ref().is_some_and(|s| s.hub == hub))
    }

    fn is_current(&self, hub: u64) -> bool {
        self.hub.as_ref().is_some_and(|running| running.id == hub)
    }

    /// A hub with no session left: nothing needs it until a connect brings one.
    fn unused_hub(&self) -> bool {
        self.hub
            .as_ref()
            .is_some_and(|running| !self.sessions_of(running.id))
    }
}

async fn run_network<P: ShareHubPort>(
    port: P,
    mut incoming: UnboundedReceiver<Command>,
    to_self: UnboundedSender<Command>,
    flags: Arc<Flags>,
    shutdown: Arc<AtomicBool>,
) {
    let net: Shared<P> = Rc::new(RefCell::new(Net {
        port,
        flags: Arc::clone(&flags),
        shutdown,
        to_self,
        plan: None,
        endpoint: None,
        hub: None,
        hub_runs: 0,
        hub_ended: Rc::new(Notify::new()),
        native_started: false,
        members: BTreeMap::new(),
    }));
    while let Some(command) = incoming.recv().await {
        let counted = !matches!(command, Command::Tick);
        let shutdown = matches!(command, Command::Shutdown);
        match command {
            Command::Group(plan) => adopt_plan(&net, *plan).await,
            Command::Share(request, reply) => share(&net, *request, reply).await,
            Command::Release(key) => release(&net, &key).await,
            Command::Link(request, reply) => link(&net, *request, reply).await,
            Command::Forget(peer) => forget(&net, peer),
            Command::Tick => settle(&net).await,
            Command::Shutdown => teardown(&net).await,
        }
        if counted {
            flags.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
        if shutdown {
            break;
        }
    }
    log::info!("network: the sharing network stopped");
}

/// A newer plan replaces the current one; a hub under another agreement stops.
async fn adopt_plan<P: ShareHubPort>(net: &Shared<P>, plan: GroupPlan) {
    let stale = {
        let mut state = net.borrow_mut();
        if state
            .plan
            .as_ref()
            .is_some_and(|current| current.epoch >= plan.epoch)
        {
            return;
        }
        if let Some(bound) = &state.endpoint {
            bound.endpoint.set_members(&plan.member_controls());
        }
        let stale = state
            .hub
            .as_ref()
            .is_some_and(|hub| hub.agreement != plan.agreement);
        state.plan = Some(plan);
        stale
    };
    if stale {
        log::info!("network: the group's record changed; restarting the hub");
        stop_hub(net, RECORD_CHANGED).await;
    }
}

/// Binds for `request` and starts its connect. The hub starts only once a session is ready for
/// it, so a computer that is off costs no hub while it is retried.
async fn share<P: ShareHubPort>(
    net: &Shared<P>,
    request: ShareRequest,
    reply: oneshot::Sender<Result<ShareUp, ShareFailure>>,
) {
    if abandoned(net, &request) {
        let _ = reply.send(Err(ShareFailure::Setup(SetupFailure::Cancelled)));
        return;
    }
    let newer = net
        .borrow()
        .plan
        .as_ref()
        .is_some_and(|current| current.epoch > request.plan.epoch);
    if newer {
        let _ = reply.send(Err(ShareFailure::Stale));
        return;
    }
    adopt_plan(net, request.plan.clone()).await;
    // Per computer, Share and Setup are exclusive: its link ends before its Share starts.
    let links = std::mem::take(&mut net.borrow_mut().member(request.peer).links);
    for link in links {
        link.abort();
        let _ = link.await;
    }
    net.borrow_mut().member(request.peer).enabled = true;
    if NativeSessionClaim::is_claimed() && !net.borrow().flags.native.load(Ordering::Acquire) {
        let _ = reply.send(Err(ShareFailure::NativeCleanup));
        return;
    }
    if request.plan.hub_config(0).is_none() {
        let _ = reply.send(Err(ShareFailure::Misfit));
        return;
    }
    if abandoned(net, &request) {
        let _ = reply.send(Err(ShareFailure::Setup(SetupFailure::Cancelled)));
        return;
    }
    let endpoint = match ensure_endpoint(net, &request.interface_id).await {
        Ok(endpoint) => endpoint,
        Err(failure) => {
            let _ = reply.send(Err(ShareFailure::Setup(failure)));
            return;
        }
    };
    let peer = request.peer;
    let task = tokio::task::spawn_local(connect(Rc::clone(net), endpoint, request, reply));
    net.borrow_mut().member(peer).connects.push(task);
}

/// Its worker gave up on it, or the app is quitting: nothing binds or starts for it.
fn abandoned<P: ShareHubPort>(net: &Shared<P>, request: &ShareRequest) -> bool {
    request.cancel.is_revoked() || net.borrow().shutdown.load(Ordering::Acquire)
}

/// The endpoint on `interface_id`, bound on first use with the group's members named. One on
/// another network, revoked, or that left out a member the group names since, is retired first,
/// ending everything on it.
async fn ensure_endpoint<P: ShareHubPort>(
    net: &Shared<P>,
    interface_id: &str,
) -> Result<Rc<P::Endpoint>, SetupFailure> {
    let current = {
        let state = net.borrow();
        state.endpoint.as_ref().map(|bound| {
            let left_out = bound.left_out(state.plan.as_ref());
            if left_out {
                log::info!(
                    "network: the group names a computer the endpoint left out; binding again"
                );
            }
            (
                bound.interface_id == interface_id
                    && !bound.endpoint.revocation().is_stopping()
                    && !left_out,
                Rc::clone(&bound.endpoint),
            )
        })
    };
    match current {
        Some((true, endpoint)) => return Ok(endpoint),
        Some((false, endpoint)) => {
            drop(endpoint);
            teardown(net).await;
        }
        None => {}
    }
    let state = net.borrow();
    let members = state
        .plan
        .as_ref()
        .map(GroupPlan::member_controls)
        .unwrap_or_default();
    let endpoint = state
        .port
        .bind(interface_id, &members, &RevocationSignal::default())?;
    drop(state);
    let mut state = net.borrow_mut();
    state.endpoint = Some(Bound {
        endpoint: Rc::clone(&endpoint),
        interface_id: interface_id.to_owned(),
        named: members.iter().map(|member| member.fingerprint).collect(),
    });
    state.flags.bound.store(true, Ordering::Release);
    Ok(endpoint)
}

enum Joinable {
    Hub(u64),
    /// An earlier hub has not finished ending.
    Wait,
    Refused(ShareFailure),
}

/// The hub a session under `plan` joins, started here when none runs. A hub that is still
/// ending is waited out first: native input belongs to one hub at a time, and a session added to
/// a hub that stopped taking commands is lost.
fn joinable_hub<P: ShareHubPort>(
    net: &Shared<P>,
    endpoint: &Rc<P::Endpoint>,
    plan: &GroupPlan,
) -> Joinable {
    let mut state = net.borrow_mut();
    match &state.hub {
        Some(hub) if hub.agreement != plan.agreement => {
            return Joinable::Refused(ShareFailure::Stale);
        }
        Some(hub) if hub.handle.is_closed() => return Joinable::Wait,
        Some(hub) => return Joinable::Hub(hub.id),
        None if state.hub_runs > 0 => return Joinable::Wait,
        None => {}
    }
    if NativeSessionClaim::is_claimed() && !state.flags.native.load(Ordering::Acquire) {
        return Joinable::Refused(ShareFailure::NativeCleanup);
    }
    match start_hub(net, &mut state, endpoint, plan) {
        Ok(id) => Joinable::Hub(id),
        Err(failure) => Joinable::Refused(failure),
    }
}

fn start_hub<P: ShareHubPort>(
    net: &Shared<P>,
    state: &mut Net<P>,
    endpoint: &Rc<P::Endpoint>,
    plan: &GroupPlan,
) -> Result<u64, ShareFailure> {
    let id = NEXT_HUB_GENERATION.fetch_add(1, Ordering::Relaxed);
    let config = plan.hub_config(id).ok_or(ShareFailure::Misfit)?;
    // Hub-only: a native failure it requests a stop on must not end setup links or clipboard
    // streams, while a network revocation still ends the hub.
    let revocation = config.revocation.clone();
    let (handle, events, run) = state.port.start_hub(config).map_err(|failure| {
        log::warn!("network: the hub could not start for this record: {failure:?}");
        ShareFailure::Hub(failure)
    })?;
    let run = tokio::task::spawn_local(run_hub(
        Rc::clone(net),
        id,
        run,
        endpoint.revocation(),
        revocation,
    ));
    let events = tokio::task::spawn_local(pump_hub::<P>(Rc::clone(net), id, events));
    state.hub = Some(RunningHub {
        id,
        handle,
        agreement: plan.agreement,
        run,
        events,
    });
    state.hub_runs += 1;
    state.native_started = false;
    state.flags.hub.store(true, Ordering::Release);
    log::info!(
        "network: the hub started for record #{}",
        plan.record.digest()
    );
    Ok(id)
}

/// Runs hub `id` to its end, ending it when the endpoint it serves is revoked; the hub's own
/// signal ends nothing else.
async fn run_hub<P: ShareHubPort>(
    net: Shared<P>,
    id: u64,
    mut run: LocalFuture<Result<(), SessionFailure>>,
    endpoint: RevocationSignal,
    hub: RevocationSignal,
) {
    let bridge = async {
        let mut asked = false;
        while !endpoint.is_revoked() {
            if endpoint.is_stopping() && !asked {
                hub.request_stop();
                asked = true;
            }
            tokio::time::sleep(BRIDGE_POLL).await;
        }
        hub.revoke();
    };
    let result = tokio::select! {
        result = &mut run => result,
        () = bridge => run.await,
    };
    if let Err(failure) = result {
        log::warn!("network: the hub ended with {failure:?}");
    }
    let mut state = net.borrow_mut();
    state.hub_runs = state.hub_runs.saturating_sub(1);
    if state.is_current(id) {
        state.hub = None;
    }
    if state.hub.is_none() {
        state.native_started = false;
        state.flags.hub.store(false, Ordering::Release);
        state.flags.native.store(false, Ordering::Release);
    }
    state.hub_ended.notify_waiters();
    state.tick();
}

/// Maps the hub's reports onto its sessions. Every session the hub took ends here exactly once.
async fn pump_hub<P: ShareHubPort>(net: Shared<P>, id: u64, mut events: P::Events) {
    while let Some(event) = events.recv().await {
        match event {
            HubEvent::PeerStarted { peer } => {
                let mut state = net.borrow_mut();
                if let Some(session) = state
                    .members
                    .values()
                    .filter(|member| member.device == peer)
                    .find_map(|member| member.session.as_ref().filter(|s| s.hub == id))
                {
                    session.started.store(true, Ordering::Release);
                }
                if state.is_current(id) {
                    state.native_started = true;
                }
            }
            HubEvent::PeerEnded {
                peer,
                result,
                diagnostics,
            } => {
                if take_owed::<P>(&net, id, peer) {
                    continue;
                }
                let ended = end_session::<P>(&net, id, |member| member.device == peer);
                for session in ended {
                    deliver(session, result, diagnostics);
                }
                // A hub whose native input never started reports no idle to wait for.
                let state = net.borrow();
                if state.is_current(id) && !state.sessions_of(id) {
                    state
                        .flags
                        .native
                        .store(state.native_started, Ordering::Release);
                }
                state.tick();
            }
            HubEvent::NativeIdle => {
                let mut state = net.borrow_mut();
                if state.is_current(id) {
                    state.native_started = false;
                    let held = state.sessions_of(id);
                    state.flags.native.store(held, Ordering::Release);
                }
            }
        }
    }
    // A session the hub never reported still ended with it, and the hub owes nothing more.
    for session in end_session::<P>(&net, id, |_| true) {
        deliver(
            session,
            Err(SessionFailure::Revoked),
            SessionDiagnostics::default(),
        );
    }
    for member in net.borrow_mut().members.values_mut() {
        member.owed.retain(|hub| *hub != id);
    }
    net.borrow().tick();
}

/// Settles one end report hub `id` owed `peer` for a replaced session; true when this was one.
fn take_owed<P: ShareHubPort>(net: &Shared<P>, id: u64, peer: DeviceId) -> bool {
    net.borrow_mut()
        .members
        .values_mut()
        .filter(|member| member.device == peer)
        .any(|member| {
            let owed = member.owed.iter().position(|hub| *hub == id);
            owed.map(|at| member.owed.remove(at)).is_some()
        })
}

/// Takes the sessions of hub `id` whose member `matches`, closing each member's connection
/// unless a setup link now owns it.
fn end_session<P: ShareHubPort>(
    net: &Shared<P>,
    id: u64,
    matches: impl Fn(&Member<P::Attachment>) -> bool,
) -> Vec<Session<P::Attachment>> {
    let mut state = net.borrow_mut();
    let endpoint = state
        .endpoint
        .as_ref()
        .map(|bound| Rc::clone(&bound.endpoint));
    let mut ended = Vec::new();
    for member in state.members.values_mut().filter(|member| matches(member)) {
        if member
            .session
            .as_ref()
            .is_none_or(|session| session.hub != id)
        {
            continue;
        }
        if let Some(session) = member.session.take() {
            member.links.retain(|task| !task.is_finished());
            if member.links.is_empty()
                && let Some(endpoint) = &endpoint
            {
                endpoint.close_member(member.fingerprint);
            }
            ended.push(session);
        }
    }
    ended
}

fn deliver<A: Send + 'static>(
    session: Session<A>,
    result: Result<(), SessionFailure>,
    diagnostics: SessionDiagnostics,
) {
    let end = ShareEnd {
        result,
        diagnostics,
        detached: Detached::of(session.attachment),
    };
    if let Err(end) = session.ended.send(end) {
        drop_off_thread(end.detached);
    }
}

async fn connect<P: ShareHubPort>(
    net: Shared<P>,
    endpoint: Rc<P::Endpoint>,
    request: ShareRequest,
    reply: oneshot::Sender<Result<ShareUp, ShareFailure>>,
) {
    let ShareRequest {
        plan,
        peer,
        cancel,
        progress,
        ..
    } = request;
    let outcome = match endpoint.connect(peer, plan.agreement, &cancel).await {
        Ok(member) => admit(&net, &endpoint, &plan, peer, &cancel, progress, member).await,
        Err(failure) => Err(ShareFailure::Setup(failure)),
    };
    drop(endpoint);
    // A worker that stopped waiting still has its session in the hub: end it there.
    if let Err(Ok(up)) = reply.send(outcome) {
        up.cancel.request_stop();
    }
    net.borrow().tick();
}

/// Attaches the clipboard, then hands the session to the hub, starting one when none runs. A
/// session that cannot run under the current plan is closed instead.
async fn admit<P: ShareHubPort>(
    net: &Shared<P>,
    endpoint: &Rc<P::Endpoint>,
    plan: &GroupPlan,
    peer: CertificateFingerprint,
    cancel: &RevocationSignal,
    progress: SessionProgress,
    member: PortMember<P::Session>,
) -> Result<ShareUp, ShareFailure> {
    let refuse = |failure| {
        endpoint.close_member(peer);
        Err(failure)
    };
    let hub = loop {
        let ended = Rc::clone(&net.borrow().hub_ended);
        let hub_ended = ended.notified();
        let stale = net
            .borrow()
            .plan
            .as_ref()
            .is_none_or(|current| current.epoch != plan.epoch);
        let joinable = if cancel.is_revoked() {
            Joinable::Refused(ShareFailure::Setup(SetupFailure::Cancelled))
        } else if stale {
            Joinable::Refused(ShareFailure::Stale)
        } else if !plan.fits(peer, &member.inspection) {
            Joinable::Refused(ShareFailure::Misfit)
        } else {
            joinable_hub(net, endpoint, plan)
        };
        match joinable {
            Joinable::Hub(id) => break id,
            Joinable::Wait => hub_ended.await,
            Joinable::Refused(failure) => return refuse(failure),
        }
    };
    let mut state = net.borrow_mut();
    // Attached first, so the clipboard link holds its stream credit before any input flows.
    let attachment = state.port.attach(peer, &member.session);
    let added = state.hub.as_ref().is_some_and(|running| {
        running
            .handle
            .add_peer(member.session, progress, member.cancel.clone())
            .is_ok()
    });
    if !added {
        drop(state);
        endpoint.close_member(peer);
        drop_off_thread(Detached::of(attachment));
        return Err(ShareFailure::Setup(SetupFailure::Cancelled));
    }
    state.flags.native.store(true, Ordering::Release);
    let (ended, receiver) = oneshot::channel();
    let started = Arc::new(AtomicBool::new(false));
    let entry = state.member(peer);
    let previous = entry.session.replace(Session {
        hub,
        attachment,
        ended,
        started: Arc::clone(&started),
    });
    if let Some(previous) = &previous {
        entry.owed.push(previous.hub);
    }
    drop(state);
    if let Some(previous) = previous {
        deliver(
            previous,
            Err(SessionFailure::Revoked),
            SessionDiagnostics::default(),
        );
    }
    Ok(ShareUp {
        inspection: member.inspection,
        cancel: member.cancel,
        started,
        ended: receiver,
    })
}

async fn release<P: ShareHubPort>(net: &Shared<P>, key: &str) {
    let tasks = {
        let mut state = net.borrow_mut();
        let Some(member) = state.members.get_mut(key) else {
            return;
        };
        member.enabled = false;
        let device = member.device;
        let live = member.session.is_some();
        let tasks = std::mem::take(&mut member.connects);
        if live && let Some(hub) = &state.hub {
            hub.handle.remove_peer(device, DELIBERATE);
        }
        tasks
    };
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
    settle(net).await;
}

async fn link<P: ShareHubPort>(
    net: &Shared<P>,
    request: SetupLinkRequest,
    reply: oneshot::Sender<Result<(), SetupFailure>>,
) {
    {
        let mut state = net.borrow_mut();
        let member = state.member(request.peer);
        let (device, live) = (member.device, member.session.is_some());
        if live && let Some(hub) = &state.hub {
            hub.handle.remove_peer(device, DELIBERATE);
        }
    }
    let endpoint = match ensure_endpoint(net, &request.interface_id).await {
        Ok(endpoint) => endpoint,
        Err(failure) => {
            let _ = reply.send(Err(failure));
            settle(net).await;
            return;
        }
    };
    let peer = request.peer;
    let task = tokio::task::spawn_local(run_link(Rc::clone(net), endpoint, request, reply));
    net.borrow_mut().member(peer).links.push(task);
}

async fn run_link<P: ShareHubPort>(
    net: Shared<P>,
    endpoint: Rc<P::Endpoint>,
    request: SetupLinkRequest,
    mut reply: oneshot::Sender<Result<(), SetupFailure>>,
) {
    let SetupLinkRequest {
        peer,
        cancel,
        persist,
        events,
        commands,
        ..
    } = request;
    let finished = tokio::select! {
        result = endpoint.setup_link(peer, &cancel, persist, events, commands) => Some(result),
        () = reply.closed() => None,
    };
    match finished {
        Some(result) => {
            let _ = reply.send(result);
        }
        // The worker dropped its link: only this computer's connection goes with it.
        None => endpoint.close_member(peer),
    }
    drop(endpoint);
    net.borrow().tick();
}

fn forget<P: ShareHubPort>(net: &Shared<P>, peer: CertificateFingerprint) {
    let mut state = net.borrow_mut();
    if let Some(bound) = &state.endpoint {
        bound.endpoint.forget(peer);
    }
    let member = state.member(peer);
    let (device, live) = (member.device, member.session.is_some());
    if live && let Some(hub) = &state.hub {
        hub.handle.remove_peer(device, DELIBERATE);
    }
}

/// With no worker, session or link left, the hub stops and the endpoint lets go of the port. A
/// hub left without sessions stops on its own; the next session starts another.
async fn settle<P: ShareHubPort>(net: &Shared<P>) {
    if net.borrow_mut().idle() {
        teardown(net).await;
    } else if net.borrow().unused_hub() {
        stop_hub(net, DELIBERATE).await;
    }
}

/// Ends every connect and link, stops the hub and retires the endpoint. Connects end first, so
/// none can start another hub while this one stops.
async fn teardown<P: ShareHubPort>(net: &Shared<P>) {
    let tasks: Vec<JoinHandle<()>> = net
        .borrow_mut()
        .members
        .values_mut()
        .flat_map(|member| {
            member
                .connects
                .drain(..)
                .chain(member.links.drain(..))
                .collect::<Vec<_>>()
        })
        .collect();
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
    stop_hub(net, DELIBERATE).await;
    let bound = net.borrow_mut().endpoint.take();
    if let Some(bound) = bound {
        bound.endpoint.retire().await;
        log::info!("network: the sharing endpoint let go of the network");
    }
    net.borrow().flags.bound.store(false, Ordering::Release);
}

/// Stops the current hub and waits for its run, which clears the hub's flags as it finishes.
async fn stop_hub<P: ShareHubPort>(net: &Shared<P>, end: PeerEnd) {
    let Some(RunningHub {
        handle,
        run,
        events,
        ..
    }) = net.borrow_mut().hub.take()
    else {
        return;
    };
    handle.stop(end);
    // The event pump below ends only once nothing can report into it.
    drop(handle);
    let _ = run.await;
    let _ = events.await;
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group_record::{GroupMember, shared_group_bytes};
    use crate::lifecycle::AppController;
    use crate::sharing::{
        LayoutRequest, LinkRequest, NOT_CONNECTED, PAUSED, SharingController, SharingView,
        tests::{expect_fake_links, fake_link},
    };
    use crate::sharing_preferences::{
        ControlMap, SetupFile,
        tests::{inspection, link_of, preferences, trio},
    };
    use monhop_core::{DisplayId, Platform, Point};
    use monhop_protocol::{DisplayDescription, DisplayTopology};
    use monhop_transport::session::{LinkClose, LinkStats};
    use std::{
        collections::{BTreeSet, VecDeque},
        path::{Path, PathBuf},
        time::Instant,
    };

    const PATIENCE: Duration = Duration::from_secs(10);

    fn fingerprint(letter: char) -> CertificateFingerprint {
        CertificateFingerprint::parse_full(&letter.to_string().repeat(64)).unwrap()
    }

    fn key(letter: char) -> String {
        letter.to_ascii_lowercase().to_string().repeat(64)
    }

    fn hex(letter: char) -> String {
        letter.to_ascii_uppercase().to_string().repeat(64)
    }

    fn device(key: &str) -> DeviceId {
        device_id_from_fingerprint(CertificateFingerprint::parse_full(key).unwrap())
    }

    #[derive(Clone)]
    enum Answer {
        Accept(Box<InspectedPeer>),
        Refuse(SetupFailure),
    }

    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Bind(Vec<String>),
        Members(Vec<String>),
        Connect(String, [u8; 32]),
        Attach(String),
        AddPeer(String),
        RemovePeer(String),
        StartHub(Vec<DeviceId>, String),
        StopHub,
        CloseMember(String),
        Forget(String),
        Retire,
        /// An attachment dropped, and the thread it dropped on.
        Detached(String, Option<String>),
        /// The run of the fake's hub with this number returned.
        HubEnded(u64),
    }

    struct Live {
        hub: u64,
        key: String,
        cancel: RevocationSignal,
        removed: bool,
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<Call>,
        answers: BTreeMap<String, VecDeque<Answer>>,
        sessions: Vec<Live>,
        ends: Vec<(String, Result<(), SessionFailure>, LinkClose)>,
        next_hub: u64,
        stopped: BTreeSet<u64>,
        /// Every hub start fails with this.
        hub_failure: Option<SessionFailure>,
        /// Hubs whose loop stopped taking commands; each holds its sessions until released.
        closed: BTreeSet<u64>,
        released: BTreeSet<u64>,
        /// Computers whose recorded address another pairing record shares: admitted only when
        /// the bind names them.
        tied: BTreeSet<String>,
        /// Computers never admitted, named or not.
        off_subnet: BTreeSet<String>,
        /// The next forget panics the network thread.
        panic_on_forget: bool,
        /// The next endpoint dropped waits here, marking `dropping` first.
        drop_gate: Option<std::sync::mpsc::Receiver<()>>,
        dropping: bool,
    }

    /// The transport as the network thread sees it, scripted by the test.
    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<FakeState>>);

    impl Fake {
        fn state(&self) -> MutexGuard<'_, FakeState> {
            lock(&self.0)
        }

        fn call(&self, call: Call) {
            self.state().calls.push(call);
        }

        fn calls(&self) -> Vec<Call> {
            self.state().calls.clone()
        }

        fn count(&self, matches: impl Fn(&Call) -> bool) -> usize {
            self.state()
                .calls
                .iter()
                .filter(|call| matches(call))
                .count()
        }

        fn added(&self, peer: char) -> usize {
            self.count(|call| *call == Call::AddPeer(key(peer)))
        }

        fn connects(&self, peer: char) -> usize {
            self.count(|call| matches!(call, Call::Connect(to, _) if *to == key(peer)))
        }

        fn answer(&self, peer: char, answer: Answer) {
            self.state()
                .answers
                .entry(key(peer))
                .or_default()
                .push_back(answer);
        }

        /// The next connect with `peer` meets it showing exactly `record`'s entries.
        fn accept(&self, peer: char, record: &GroupRecord) {
            let seen = link_of(record, &hex('A'), &hex(peer));
            self.answer(peer, Answer::Accept(Box::new(seen)));
        }

        /// Ends `peer`'s session in the hub as the transport would report it.
        fn end(&self, peer: char, result: Result<(), SessionFailure>, close: LinkClose) {
            self.state().ends.push((key(peer), result, close));
        }

        fn live(&self, peer: char) -> bool {
            self.state()
                .sessions
                .iter()
                .any(|live| live.key == key(peer) && !live.removed)
        }
    }

    struct FakePort(Fake);

    struct FakeEndpoint {
        fake: Fake,
        revocation: RevocationSignal,
        /// Paired computers this bind left out.
        excluded: BTreeSet<String>,
    }

    impl Drop for FakeEndpoint {
        fn drop(&mut self) {
            let gate = self.fake.state().drop_gate.take();
            if let Some(gate) = gate {
                self.fake.state().dropping = true;
                let _ = gate.recv();
            }
        }
    }

    struct FakeSession(String);

    struct FakeHub {
        fake: Fake,
        id: u64,
        events: UnboundedSender<HubEvent>,
    }

    struct FakeEvents(UnboundedReceiver<HubEvent>);

    struct FakeAttachment {
        fake: Fake,
        key: String,
    }

    impl Drop for FakeAttachment {
        fn drop(&mut self) {
            let thread = std::thread::current().name().map(str::to_owned);
            self.fake.call(Call::Detached(self.key.clone(), thread));
        }
    }

    fn member_keys(members: &[GroupMemberRecord]) -> Vec<String> {
        members
            .iter()
            .map(|member| fingerprint_key(&member.fingerprint.full_hex()))
            .collect()
    }

    impl ShareHubPort for FakePort {
        type Session = FakeSession;
        type Endpoint = FakeEndpoint;
        type Hub = FakeHub;
        type Events = FakeEvents;
        type Attachment = FakeAttachment;

        fn bind(
            &self,
            _interface_id: &str,
            members: &[GroupMemberRecord],
            _cancel: &RevocationSignal,
        ) -> Result<Rc<FakeEndpoint>, SetupFailure> {
            let named = member_keys(members);
            let excluded = {
                let state = self.0.state();
                let tied = state.tied.iter().filter(|key| !named.contains(key));
                state.off_subnet.iter().chain(tied).cloned().collect()
            };
            self.0.call(Call::Bind(named));
            Ok(Rc::new(FakeEndpoint {
                fake: self.0.clone(),
                revocation: RevocationSignal::default(),
                excluded,
            }))
        }

        fn start_hub(
            &self,
            config: HubConfig,
        ) -> Result<(FakeHub, FakeEvents, LocalFuture<Result<(), SessionFailure>>), SessionFailure>
        {
            let id = {
                let mut state = self.0.state();
                state.calls.push(Call::StartHub(
                    config.members.clone(),
                    format!("{:?}", config.group),
                ));
                if let Some(failure) = state.hub_failure {
                    return Err(failure);
                }
                state.next_hub += 1;
                state.next_hub
            };
            let (events, receiver) = mpsc::unbounded_channel();
            let hub = FakeHub {
                fake: self.0.clone(),
                id,
                events: events.clone(),
            };
            Ok((
                hub,
                FakeEvents(receiver),
                Box::pin(run_fake_hub(self.0.clone(), id, events)),
            ))
        }

        fn attach(&self, _member: CertificateFingerprint, session: &FakeSession) -> FakeAttachment {
            self.0.call(Call::Attach(session.0.clone()));
            FakeAttachment {
                fake: self.0.clone(),
                key: session.0.clone(),
            }
        }
    }

    impl PortEndpoint for FakeEndpoint {
        type Session = FakeSession;

        fn revocation(&self) -> RevocationSignal {
            self.revocation.clone()
        }

        fn set_members(&self, members: &[GroupMemberRecord]) {
            self.fake.call(Call::Members(member_keys(members)));
        }

        fn admits(&self, member: CertificateFingerprint) -> bool {
            !self.excluded.contains(&fingerprint_key(&member.full_hex()))
        }

        async fn connect(
            &self,
            member: CertificateFingerprint,
            agreement: [u8; 32],
            cancel: &RevocationSignal,
        ) -> Result<PortMember<FakeSession>, SetupFailure> {
            let key = fingerprint_key(&member.full_hex());
            self.fake.call(Call::Connect(key.clone(), agreement));
            // As the transport reports a paired computer the bind left out.
            if self.excluded.contains(&key) {
                return Err(SetupFailure::NetworkRoute);
            }
            loop {
                let answer = self
                    .fake
                    .state()
                    .answers
                    .get_mut(&key)
                    .and_then(VecDeque::pop_front);
                match answer {
                    Some(Answer::Accept(inspection)) => {
                        return Ok(PortMember {
                            session: FakeSession(key),
                            inspection: *inspection,
                            cancel: RevocationSignal::default(),
                        });
                    }
                    Some(Answer::Refuse(failure)) => return Err(failure),
                    None if cancel.is_stopping() => return Err(SetupFailure::Cancelled),
                    None => tokio::time::sleep(Duration::from_millis(2)).await,
                }
            }
        }

        fn close_member(&self, member: CertificateFingerprint) {
            self.fake
                .call(Call::CloseMember(fingerprint_key(&member.full_hex())));
        }

        fn forget(&self, member: CertificateFingerprint) {
            let panics = std::mem::take(&mut self.fake.state().panic_on_forget);
            assert!(!panics, "the network thread fails");
            self.fake
                .call(Call::Forget(fingerprint_key(&member.full_hex())));
        }

        async fn retire(self: Rc<Self>) {
            self.fake.call(Call::Retire);
        }

        async fn setup_link(
            &self,
            _member: CertificateFingerprint,
            cancel: &RevocationSignal,
            _persist: LinkPersist,
            _events: UnboundedSender<LinkEvent>,
            _commands: UnboundedReceiver<LinkCommand>,
        ) -> Result<(), SetupFailure> {
            while !cancel.is_stopping() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(SetupFailure::Cancelled)
        }
    }

    impl PortHub for FakeHub {
        type Session = FakeSession;

        fn add_peer(
            &self,
            session: FakeSession,
            _progress: SessionProgress,
            cancel: RevocationSignal,
        ) -> Result<(), HubClosed> {
            let mut state = self.fake.state();
            if state.stopped.contains(&self.id) || state.closed.contains(&self.id) {
                return Err(HubClosed);
            }
            state.calls.push(Call::AddPeer(session.0.clone()));
            let peer = device(&session.0);
            state.sessions.push(Live {
                hub: self.id,
                key: session.0,
                cancel,
                removed: false,
            });
            let _ = self.events.send(HubEvent::PeerStarted { peer });
            Ok(())
        }

        fn remove_peer(&self, peer: DeviceId, _end: PeerEnd) {
            let mut state = self.fake.state();
            let id = self.id;
            let mut removed = Vec::new();
            for live in state
                .sessions
                .iter_mut()
                .filter(|live| live.hub == id && device(&live.key) == peer)
            {
                live.removed = true;
                removed.push(Call::RemovePeer(live.key.clone()));
            }
            state.calls.extend(removed);
        }

        fn stop(&self, _end: PeerEnd) {
            let mut state = self.fake.state();
            state.stopped.insert(self.id);
            state.calls.push(Call::StopHub);
        }

        fn is_closed(&self) -> bool {
            let state = self.fake.state();
            state.stopped.contains(&self.id) || state.closed.contains(&self.id)
        }
    }

    impl PortEvents for FakeEvents {
        async fn recv(&mut self) -> Option<HubEvent> {
            self.0.recv().await
        }
    }

    /// Ends sessions as the hub's housekeeping does: on their cancel, a removal or the hub's
    /// stop, and wherever the test says. A closed hub holds everything until released, then
    /// ends every session and fails.
    async fn run_fake_hub(
        fake: Fake,
        id: u64,
        events: UnboundedSender<HubEvent>,
    ) -> Result<(), SessionFailure> {
        loop {
            let (ended, outcome) = {
                let mut state = fake.state();
                let dying = state.closed.contains(&id);
                let held = dying && !state.released.contains(&id);
                let stopped = !held && (dying || state.stopped.contains(&id));
                let mut requested = std::mem::take(&mut state.ends);
                let mut ended = Vec::new();
                let mut kept = Vec::new();
                for live in std::mem::take(&mut state.sessions) {
                    if live.hub != id || held {
                        kept.push(live);
                    } else if stopped || live.removed || live.cancel.is_stopping() {
                        ended.push((live.key, Err(SessionFailure::Revoked), LinkClose::Local));
                    } else if let Some(at) = requested.iter().position(|(key, ..)| *key == live.key)
                    {
                        ended.push(requested.remove(at));
                    } else {
                        kept.push(live);
                    }
                }
                state.sessions = kept;
                state.ends = requested;
                let failed = if dying {
                    Err(SessionFailure::Native)
                } else {
                    Ok(())
                };
                (ended, stopped.then_some(failed))
            };
            for (key, result, close) in ended {
                let _ = events.send(HubEvent::PeerEnded {
                    peer: device(&key),
                    result,
                    diagnostics: SessionDiagnostics {
                        link: Some(LinkStats {
                            close,
                            ..LinkStats::default()
                        }),
                        ..SessionDiagnostics::default()
                    },
                });
            }
            if let Some(outcome) = outcome {
                fake.call(Call::HubEnded(id));
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn screen(id: u64, width: u32) -> DisplayTopology {
        DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(id),
            name: format!("Display {id}"),
            native_width: width,
            native_height: 1080,
            logical_origin: Point::new(0.0, 0.0),
            logical_size: Point::new(f64::from(width), 1080.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap()
    }

    /// This computer's one display, as every fixture record holds it.
    fn local_screen(_local: &str) -> Result<DisplayTopology, String> {
        Ok(screen(1, 1920))
    }

    fn crossing(from: &str, from_edge: &str, to: &str, to_edge: &str) -> [LinkRequest; 2] {
        let one_way = |from: &str, from_edge: &str, to: &str, to_edge: &str| LinkRequest {
            from_display: from.into(),
            from_edge: from_edge.into(),
            from_span: [0.0, 1.0],
            to_display: to.into(),
            to_edge: to_edge.into(),
            to_span: [0.0, 1.0],
            hysteresis: 1.0,
        };
        [
            one_way(from, from_edge, to, to_edge),
            one_way(to, to_edge, from, from_edge),
        ]
    }

    /// One other computer of a fixture group: its letter, its one display's id and width, and
    /// the edges of this computer's display and of its own that the crossing joins.
    type Peer = (char, u64, u32, &'static str, &'static str);

    /// This computer (A, a Mac, display 1) and one PC per `peers`, stamped by A at `revision`.
    fn group(revision: u64, peers: &[Peer], control: ControlMap) -> GroupRecord {
        let mut members =
            vec![GroupMember::new(&hex('A'), Platform::MacOs, &screen(1, 1920)).unwrap()];
        let mut links = Vec::new();
        for &(peer, id, width, edge, back) in peers {
            members
                .push(GroupMember::new(&hex(peer), Platform::Windows, &screen(id, width)).unwrap());
            links.extend(crossing("1", edge, &id.to_string(), back));
        }
        GroupRecord::new(
            revision,
            &hex('A'),
            members,
            LayoutRequest {
                links,
                arrangement: None,
                control,
            },
        )
        .expect("a valid fixture record")
    }

    fn everyone(peers: &[char]) -> ControlMap {
        peers
            .iter()
            .chain(std::iter::once(&'A'))
            .map(|peer| (key(*peer), true))
            .collect()
    }

    const B: Peer = ('B', 2, 1920, "right", "left");
    const C: Peer = ('C', 3, 1920, "left", "right");

    /// A folder of the test's own, holding a setup file with `record` active on the fixture
    /// network.
    fn setup_holding(name: &str, record: &GroupRecord) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("monhop-hub-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("sharing.json");
        let mut file = SetupFile::default();
        file.set_interface_id("en0:4:192.168.1.4");
        file.adopt(&hex('A'), record.clone()).unwrap();
        file.save(&path).unwrap();
        path
    }

    fn app(fake: &Fake, path: &Path) -> AppController {
        let port = fake.clone();
        let app = AppController::with_sharing(SharingController::with_network(
            fake_link,
            Duration::from_secs(600),
            SharingNetwork::with_port(move || FakePort(port.clone())),
            local_screen,
        ));
        app.use_setup_path(path.to_path_buf());
        app
    }

    /// Runs supervisor passes until `ready` holds.
    #[track_caller]
    fn supervise_until(app: &AppController, ready: impl Fn() -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !ready() {
            assert!(Instant::now() < deadline, "the state was never reached");
            app.supervise_sharing();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Supervisor passes that should change nothing.
    fn supervise_idle(app: &AppController) {
        for _ in 0..10 {
            app.supervise_sharing();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[track_caller]
    fn eventually(ready: impl Fn() -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !ready() {
            assert!(Instant::now() < deadline, "the state was never reached");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn json(view: SharingView) -> serde_json::Value {
        serde_json::to_value(view).unwrap()
    }

    /// The first status that satisfies `ready`, read once.
    #[track_caller]
    fn status_when(
        app: &AppController,
        ready: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let view = json(app.sharing.status());
            if ready(&view) {
                return view;
            }
            assert!(
                Instant::now() < deadline,
                "the state was never reached: {view}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn peer_json(app: &AppController, peer: char) -> serde_json::Value {
        json(app.sharing.status())["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["fingerprint"] == key(peer))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    fn sharing_with(app: &AppController, peer: char) -> bool {
        peer_json(app, peer)["sharingActive"] == true
    }

    /// Stops everything and waits until the network let go of the port.
    #[track_caller]
    fn finish(app: &AppController, path: &Path) {
        app.sharing.stop_with(NOT_CONNECTED);
        eventually(|| app.sharing.shutdown_ready());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn position(fake: &Fake, call: &Call) -> usize {
        fake.calls()
            .iter()
            .position(|seen| seen == call)
            .unwrap_or_else(|| panic!("{call:?} never happened"))
    }

    fn topology(record: &GroupRecord) -> String {
        format!("{:?}", record.topology_for(&hex('A')).unwrap())
    }

    fn connects_under(fake: &Fake, record: &GroupRecord) -> usize {
        let agreement = record.content_digest();
        fake.count(|call| matches!(call, Call::Connect(_, digest) if *digest == agreement))
    }

    fn hub_starts(fake: &Fake) -> usize {
        fake.count(|call| matches!(call, Call::StartHub(..)))
    }

    /// The members each bind named, in order.
    fn binds(fake: &Fake) -> Vec<Vec<String>> {
        fake.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Bind(named) => Some(named),
                _ => None,
            })
            .collect()
    }

    const INTERFACE: &str = "en0:4:192.168.1.4";

    fn plan_of(record: &GroupRecord, epoch: u64) -> GroupPlan {
        GroupPlan {
            epoch,
            local: hex('A'),
            record: record.clone(),
            agreement: record.content_digest(),
        }
    }

    fn share_request(plan: &GroupPlan, peer: char) -> ShareRequest {
        ShareRequest {
            interface_id: INTERFACE.into(),
            plan: plan.clone(),
            peer: fingerprint(peer),
            cancel: RevocationSignal::default(),
            progress: SessionProgress::default(),
        }
    }

    fn link_request(peer: char) -> SetupLinkRequest {
        let (events, _) = mpsc::unbounded_channel();
        let (_, commands) = mpsc::unbounded_channel();
        SetupLinkRequest {
            interface_id: INTERFACE.into(),
            peer: fingerprint(peer),
            cancel: RevocationSignal::default(),
            persist: LinkPersist {
                stage: Arc::new(|_, _| Ok(())),
                commit: Arc::new(|_, _| Ok(())),
                discard: Arc::new(|| {}),
            },
            events,
            commands,
        }
    }

    fn network_on(fake: &Fake) -> SharingNetwork {
        let port = fake.clone();
        SharingNetwork::with_port(move || FakePort(port.clone()))
    }

    /// Waits for `future` on a runtime of the test's own.
    fn wait_for<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async { tokio::time::timeout(PATIENCE, future).await })
            .expect("the answer never came")
    }

    /// Stops the network and waits until it let go of the port.
    #[track_caller]
    fn shut(network: &SharingNetwork) {
        network.request_shutdown();
        eventually(|| network.is_quiet());
    }

    #[test]
    fn three_enabled_computers_keep_three_connections() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = group(
            3,
            &[B, C, ('D', 4, 1920, "top", "bottom")],
            everyone(&['B', 'C', 'D']),
        );
        let path = setup_holding("three", &record);
        let fake = Fake::default();
        for peer in ['B', 'C', 'D'] {
            fake.accept(peer, &record);
        }
        let app = app(&fake, &path);
        supervise_until(&app, || {
            ['B', 'C', 'D'].iter().all(|peer| sharing_with(&app, *peer))
        });
        // One hub for the group, every other member in slot order, one session per computer.
        let starts: Vec<Call> = fake
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::StartHub(..)))
            .collect();
        let members = vec![device(&key('B')), device(&key('C')), device(&key('D'))];
        assert_eq!(starts, [Call::StartHub(members, topology(&record))]);
        for peer in ['B', 'C', 'D'] {
            assert_eq!(fake.added(peer), 1);
        }
        let view = json(app.sharing.status());
        assert_eq!(view["phase"], "sharing");
        assert_eq!(view["sharingActive"], true);
        assert!(view["peerFingerprint"].is_null());
        assert_eq!(
            view["enabled"],
            serde_json::json!([key('B'), key('C'), key('D')])
        );
        assert_eq!(view["members"].as_array().unwrap().len(), 4);
        // One drops: it alone reconnects, and the other two never notice.
        fake.accept('C', &record);
        fake.end('C', Err(SessionFailure::Wire), LinkClose::Transport);
        supervise_until(&app, || fake.added('C') == 2 && sharing_with(&app, 'C'));
        assert_eq!((fake.added('B'), fake.added('D')), (1, 1));
        assert!(['B', 'C', 'D'].iter().all(|peer| sharing_with(&app, *peer)));
        finish(&app, &path);
    }

    #[test]
    fn one_peer_failing_backs_off_only_that_peer() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = trio(3, true);
        let path = setup_holding("backoff", &record);
        let fake = Fake::default();
        fake.accept('B', &record);
        fake.answer('C', Answer::Refuse(SetupFailure::PeerIdentityChanged));
        let app = app(&fake, &path);
        supervise_until(&app, || {
            sharing_with(&app, 'B') && peer_json(&app, 'C')["phase"] == "error"
        });
        let backoff = Duration::from_secs(10);
        assert!(
            app.sharing
                .within_failure_backoff(fingerprint('C'), backoff)
        );
        assert!(
            !app.sharing
                .within_failure_backoff(fingerprint('B'), backoff)
        );
        // Further passes leave the failed computer alone and the other one sharing.
        supervise_idle(&app);
        assert_eq!(fake.connects('C'), 1);
        assert_eq!(fake.added('B'), 1);
        assert!(sharing_with(&app, 'B'));
        let view = json(app.sharing.status());
        assert_eq!(view["phase"], "sharing");
        assert_eq!(view["peerFingerprint"], key('B'));
        finish(&app, &path);
    }

    #[test]
    fn a_display_change_on_one_member_links_only_that_member_and_recycles_on_commit() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = trio(3, true);
        let path = setup_holding("display-change", &record);
        let fake = Fake::default();
        fake.accept('B', &record);
        fake.accept('C', &record);
        let app = app(&fake, &path);
        let links = expect_fake_links();
        supervise_until(&app, || sharing_with(&app, 'B') && sharing_with(&app, 'C'));
        // C's displays change: its session ends, and only C is linked.
        fake.end(
            'C',
            Err(SessionFailure::Wire),
            LinkClose::PeerDisplaysChanged,
        );
        supervise_until(&app, || {
            app.sharing.worker_alive(fingerprint('C')) && !fake.live('C')
        });
        let link = links.recv_timeout(PATIENCE).expect("C gets a setup link");
        assert!(sharing_with(&app, 'B'));
        assert_eq!((fake.added('B'), fake.connects('B')), (1, 1));
        assert_eq!(fake.count(|call| *call == Call::StopHub), 0);
        // C sends its refit record over the link, and this computer commits it.
        let refit = group(
            4,
            &[B, ('C', 3, 2560, "left", "right")],
            everyone(&['B', 'C']),
        )
        .restamped(4, &hex('C'))
        .unwrap();
        let seen = link_of(&refit, &hex('A'), &hex('C'));
        link.events
            .send(LinkEvent::Connected {
                inspection: seen.clone(),
            })
            .unwrap();
        eventually(|| {
            app.sharing
                .inspection_for_switch_with(fingerprint('C'))
                .is_some()
        });
        let bytes = shared_group_bytes(&seen, &refit, false).unwrap();
        (link.persist.stage)(&seen, &bytes).unwrap();
        (link.persist.commit)(&seen, &bytes).unwrap();
        assert_eq!(SetupFile::load(&path).unwrap().active_group(), Some(&refit));
        link.events
            .send(LinkEvent::SyncCompleted {
                inspection: seen,
                bytes,
                sending: false,
            })
            .unwrap();
        link.events
            .send(LinkEvent::Disconnected {
                reason: monhop_transport::session_link::LinkDisconnect::PeerClosed,
            })
            .unwrap();
        // The commit recycles B's session and restarts the hub under the new record.
        fake.accept('B', &refit);
        fake.accept('C', &refit);
        supervise_until(&app, || {
            fake.added('B') == 2
                && fake.added('C') == 2
                && sharing_with(&app, 'B')
                && sharing_with(&app, 'C')
        });
        assert!(fake.count(|call| *call == Call::StopHub) >= 1);
        assert!(fake.calls().iter().any(
            |call| matches!(call, Call::StartHub(_, started) if *started == topology(&refit))
        ));
        assert_eq!(connects_under(&fake, &refit), 2);
        finish(&app, &path);
    }

    #[test]
    fn forgetting_one_member_keeps_the_others_sharing() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = trio(3, true);
        let path = setup_holding("forget", &record);
        let fake = Fake::default();
        fake.accept('B', &record);
        fake.accept('C', &record);
        let app = app(&fake, &path);
        supervise_until(&app, || sharing_with(&app, 'B') && sharing_with(&app, 'C'));
        // What forgetting C does before its trust record goes: only its own session ends.
        let file = SetupFile::load(&path).unwrap();
        app.release_forgotten(&file, fingerprint('C')).unwrap();
        assert!(!app.sharing.worker_alive(fingerprint('C')));
        assert!(sharing_with(&app, 'B'));
        assert_eq!(fake.added('B'), 1);
        eventually(|| fake.calls().contains(&Call::Forget(key('C'))));
        app.sharing
            .update_setup_file(&path, |file| file.forget(&hex('C')))
            .unwrap();
        let pair = SetupFile::load(&path).unwrap();
        assert_eq!(pair.enabled(), [key('B')]);
        // B goes on under the pair's record; C is never dialed again.
        fake.accept('B', pair.active_group().unwrap());
        supervise_until(&app, || fake.added('B') == 2 && sharing_with(&app, 'B'));
        supervise_idle(&app);
        assert_eq!(fake.connects('C'), 1);
        assert!(!app.sharing.worker_alive(fingerprint('C')));
        assert!(sharing_with(&app, 'B'));
        finish(&app, &path);
    }

    #[test]
    fn pausing_stops_every_peer_and_keeps_the_group() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = trio(3, true);
        let path = setup_holding("pause", &record);
        let fake = Fake::default();
        fake.accept('B', &record);
        fake.accept('C', &record);
        let app = app(&fake, &path);
        supervise_until(&app, || sharing_with(&app, 'B') && sharing_with(&app, 'C'));
        let paused = app.set_active(None, None).unwrap();
        assert_eq!(paused.phase, "stopping");
        eventually(|| app.sharing.live_peers().is_empty());
        supervise_idle(&app);
        let file = SetupFile::load(&path).unwrap();
        assert!(file.paused());
        assert_eq!(file.enabled(), [key('B'), key('C')]);
        assert_eq!(file.active_group(), Some(&record));
        assert!(app.sharing.live_peers().is_empty());
        assert_eq!((fake.connects('B'), fake.connects('C')), (1, 1));
        // With nothing left to serve, the network lets go of the port.
        eventually(|| app.sharing.shutdown_ready());
        assert!(fake.calls().contains(&Call::Retire));
        let view = json(app.sharing.status());
        assert_eq!(view["phase"], "off");
        assert_eq!(view["message"], PAUSED);
        assert_eq!(view["paused"], true);
        for peer in view["peers"].as_array().unwrap() {
            assert_eq!(peer["message"], PAUSED);
        }
        finish(&app, &path);
    }

    #[test]
    fn enabling_a_computer_on_another_network_stops_every_worker_first() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = trio(3, true);
        let path = setup_holding("new-network", &record);
        let fake = Fake::default();
        fake.accept('B', &record);
        fake.accept('C', &record);
        let app = app(&fake, &path);
        supervise_until(&app, || sharing_with(&app, 'B') && sharing_with(&app, 'C'));
        // The network the file already records ends nothing.
        app.set_enabled(&key('B'), true, Some("en0:4:192.168.1.4"))
            .unwrap();
        supervise_idle(&app);
        assert!(sharing_with(&app, 'B') && sharing_with(&app, 'C'));
        assert_eq!((fake.added('B'), fake.added('C')), (1, 1));
        assert_eq!(fake.count(|call| *call == Call::Retire), 0);

        // Another network ends both sessions and lets the endpoint go before it is recorded,
        // so no connection is left to bind the old network again.
        app.set_enabled(&key('B'), true, Some("en1:7:192.168.1.5"))
            .unwrap();
        assert!(!fake.live('B') && !fake.live('C'));
        let retired = position(&fake, &Call::Retire);
        let file = SetupFile::load(&path).unwrap();
        assert_eq!(file.interface_id(), Some("en1:7:192.168.1.5"));
        assert_eq!(file.enabled(), [key('B'), key('C')]);
        // Both come back on the new network, over an endpoint bound afresh.
        fake.accept('B', &record);
        fake.accept('C', &record);
        supervise_until(&app, || sharing_with(&app, 'B') && sharing_with(&app, 'C'));
        let binds: Vec<usize> = fake
            .calls()
            .iter()
            .enumerate()
            .filter(|(_, call)| matches!(call, Call::Bind(_)))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(binds.len(), 2);
        assert!(binds[0] < retired && retired < binds[1]);
        finish(&app, &path);
    }

    #[test]
    fn a_commit_restarts_the_hub_with_the_new_topology_and_agreement() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = trio(3, true);
        let path = setup_holding("commit", &record);
        let fake = Fake::default();
        fake.accept('B', &record);
        fake.accept('C', &record);
        let app = app(&fake, &path);
        supervise_until(&app, || sharing_with(&app, 'B') && sharing_with(&app, 'C'));
        assert_eq!(connects_under(&fake, &record), 2);
        // A newer record moves C's crossing to the top of this computer's display.
        let moved = group(
            4,
            &[B, ('C', 3, 1920, "top", "bottom")],
            everyone(&['B', 'C']),
        );
        app.sharing
            .update_setup_file(&path, |file| {
                file.adopt(&hex('A'), moved.clone()).unwrap();
            })
            .unwrap();
        fake.accept('B', &moved);
        fake.accept('C', &moved);
        supervise_until(&app, || {
            fake.added('B') == 2
                && fake.added('C') == 2
                && sharing_with(&app, 'B')
                && sharing_with(&app, 'C')
        });
        let starts: Vec<(usize, String)> = fake
            .calls()
            .into_iter()
            .enumerate()
            .filter_map(|(at, call)| match call {
                Call::StartHub(_, started) => Some((at, started)),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0].1, topology(&record));
        assert_eq!(starts[1].1, topology(&moved));
        assert_ne!(starts[0].1, starts[1].1);
        assert!(position(&fake, &Call::StopHub) < starts[1].0);
        assert_ne!(record.content_digest(), moved.content_digest());
        assert_eq!(connects_under(&fake, &moved), 2);
        finish(&app, &path);
    }

    #[test]
    fn a_pair_where_neither_may_control_is_not_connected() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        // B may control this computer; neither this computer nor C may control anyone.
        let record = trio(3, false)
            .with_control([(key('A'), false), (key('B'), true), (key('C'), false)].into())
            .unwrap();
        let path = setup_holding("no-control", &record);
        let fake = Fake::default();
        fake.accept('B', &record);
        fake.accept('C', &record);
        let app = app(&fake, &path);
        supervise_until(&app, || sharing_with(&app, 'B'));
        supervise_idle(&app);
        assert_eq!(fake.connects('C'), 0);
        assert!(!app.sharing.worker_alive(fingerprint('C')));
        // The endpoint never offers C a Share either.
        assert!(fake.calls().contains(&Call::Bind(vec![key('B')])));
        assert_eq!(peer_json(&app, 'C')["phase"], "off");
        finish(&app, &path);
    }

    #[test]
    fn a_record_naming_an_unpaired_member_leaves_it_unconnected() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = trio(3, true);
        let path = setup_holding("unpaired", &record);
        let fake = Fake::default();
        fake.accept('B', &record);
        fake.answer('C', Answer::Refuse(SetupFailure::PairingRequired));
        let app = app(&fake, &path);
        supervise_until(&app, || {
            sharing_with(&app, 'B') && peer_json(&app, 'C')["phase"] == "notPaired"
        });
        supervise_idle(&app);
        assert_eq!(fake.connects('C'), 1);
        assert!(!app.sharing.worker_alive(fingerprint('C')));
        assert!(sharing_with(&app, 'B'));
        assert_eq!(json(app.sharing.status())["phase"], "sharing");
        finish(&app, &path);
    }

    #[test]
    fn a_clipboard_attachment_is_made_before_add_peer_and_dropped_off_the_network_thread() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let saved = preferences();
        let record = crate::sharing_preferences::tests::group(&saved);
        let path = setup_holding("clipboard", &record);
        let fake = Fake::default();
        fake.answer('B', Answer::Accept(Box::new(inspection(&saved))));
        let app = app(&fake, &path);
        supervise_until(&app, || sharing_with(&app, 'B'));
        assert!(
            position(&fake, &Call::Attach(key('B'))) < position(&fake, &Call::AddPeer(key('B')))
        );
        assert_eq!(fake.count(|call| matches!(call, Call::Detached(..))), 0);
        // The other computer's user pauses: the session ends, and its attachment with it.
        fake.end('B', Ok(()), LinkClose::PeerEnded);
        eventually(|| fake.count(|call| matches!(call, Call::Detached(..))) == 1);
        let dropped_on = fake
            .calls()
            .into_iter()
            .find_map(|call| match call {
                Call::Detached(peer, thread) if peer == key('B') => Some(thread),
                _ => None,
            })
            .unwrap();
        assert_ne!(dropped_on.as_deref(), Some(NETWORK_THREAD));
        assert_eq!(dropped_on.as_deref(), Some("monhop-sharing"));
        finish(&app, &path);
    }

    /// The fields Home read before groups existed.
    fn legacy(view: &serde_json::Value) -> serde_json::Value {
        let mut legacy = serde_json::Map::new();
        for field in [
            "phase",
            "localPlatform",
            "peerPlatform",
            "localDisplays",
            "peerDisplays",
            "message",
            "busy",
            "sharingActive",
            "diagnostics",
            "link",
            "sync",
            "synchronizedLayout",
            "peerFingerprint",
            "active",
            "editing",
            "displayNotice",
            "control",
            "lastFailure",
            "held",
        ] {
            legacy.insert(field.into(), view[field].clone());
        }
        legacy.into()
    }

    /// What a computer's own entry repeats of those fields, read from `from`: the entry itself,
    /// or the legacy view.
    fn own_fields(from: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "phase": from["phase"],
            "message": from["message"],
            "busy": from["busy"],
            "sharingActive": from["sharingActive"],
            "held": from["held"],
            "link": from["link"],
            "sync": from["sync"],
            "diagnostics": from["diagnostics"],
            "lastFailure": from["lastFailure"],
        })
    }

    #[test]
    fn a_single_enabled_computer_view_is_identical_to_today() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let saved = preferences();
        let record = crate::sharing_preferences::tests::group(&saved);
        let path = setup_holding("single", &record);
        let fake = Fake::default();
        fake.answer('B', Answer::Accept(Box::new(inspection(&saved))));
        let app = app(&fake, &path);
        supervise_until(&app, || sharing_with(&app, 'B'));
        let this_platform = if cfg!(target_os = "macos") {
            "macos"
        } else {
            "windows"
        };
        // What today's single view showed for each step, field by field: `live` while a worker
        // runs, `inspected` while its session is up with the displays it met.
        let today = |phase: &str,
                     message: &str,
                     (busy, sharing): (bool, bool),
                     attempt: u32,
                     (live, inspected): (bool, bool),
                     last_failure: &str| {
            serde_json::json!({
                "phase": phase,
                "localPlatform": if inspected { "macos" } else { this_platform },
                "peerPlatform": if inspected { serde_json::json!("windows") } else { serde_json::Value::Null },
                "localDisplays": [],
                "peerDisplays": [],
                "message": message,
                "busy": busy,
                "sharingActive": sharing,
                "diagnostics": {
                    "sentEvents": "0",
                    "receivedEvents": "0",
                    "roundTripMs": 0.0,
                    "activeDisplay": null,
                    "activeIsLocal": null,
                },
                "link": { "attempt": attempt, "since": "" },
                "sync": { "state": "idle", "message": "" },
                "synchronizedLayout": null,
                "peerFingerprint": if live { serde_json::json!(key('B')) } else { serde_json::Value::Null },
                "active": key('B'),
                "editing": false,
                "displayNotice": null,
                "control": { "localToPeer": true, "peerToLocal": true, "syncing": false },
                "lastFailure": last_failure,
                "held": false,
            })
        };
        let sharing = json(app.sharing.status());
        assert_eq!(
            legacy(&sharing),
            today(
                "sharing",
                "Sharing enabled. Hold both Control keys and Escape for two seconds to stop.",
                (true, true),
                1,
                (true, true),
                ""
            )
        );
        assert_eq!(sharing["enabled"], serde_json::json!([key('B')]));
        assert_eq!(sharing["paused"], false);
        assert_eq!(own_fields(&sharing["peers"][0]), own_fields(&sharing));
        // A drop: the reconnect wait reads exactly as it did.
        fake.end('B', Err(SessionFailure::Wire), LinkClose::Transport);
        let dropped = status_when(&app, |view| {
            view["message"] == "Sharing dropped. Reconnecting."
        });
        let reason = dropped["lastFailure"].as_str().unwrap().to_owned();
        assert!(
            reason.starts_with("Last drop just now. Attempt 1: "),
            "{reason}"
        );
        assert_eq!(
            legacy(&dropped),
            today(
                "starting",
                "Sharing dropped. Reconnecting.",
                (true, false),
                1,
                (true, false),
                &reason
            )
        );
        assert_eq!(own_fields(&dropped["peers"][0]), own_fields(&dropped));
        // Stopped: input is local, and nothing of the session is left but its drop line.
        app.sharing.stop_with(NOT_CONNECTED);
        eventually(|| app.sharing.live_peers().is_empty());
        let stopped = json(app.sharing.status());
        assert_eq!(
            legacy(&stopped),
            today(
                "off",
                NOT_CONNECTED,
                (false, false),
                0,
                (false, false),
                &reason
            )
        );
        assert_eq!(own_fields(&stopped["peers"][0]), own_fields(&stopped));
        finish(&app, &path);
    }

    #[test]
    fn an_off_computer_being_retried_starts_no_hub_and_no_polling() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let saved = preferences();
        let record = crate::sharing_preferences::tests::group(&saved);
        let path = setup_holding("off", &record);
        let fake = Fake::default();
        fake.answer('B', Answer::Refuse(SetupFailure::Connection));
        fake.answer('B', Answer::Accept(Box::new(inspection(&saved))));
        for _ in 0..8 {
            fake.answer('B', Answer::Refuse(SetupFailure::Connection));
        }
        let app = app(&fake, &path);
        supervise_until(&app, || fake.connects('B') == 1);
        assert_eq!(hub_starts(&fake), 0);
        // Between two dials to a computer that is off, the network thread sleeps.
        let before = app.sharing.network().wakes();
        eventually(|| fake.connects('B') == 2);
        let woken = app.sharing.network().wakes() - before;
        assert!(
            woken < 20,
            "the network thread woke {woken} times between two dials"
        );
        // It comes on, then goes off again: the hub ends with its last session.
        supervise_until(&app, || sharing_with(&app, 'B'));
        assert_eq!(hub_starts(&fake), 1);
        fake.end('B', Err(SessionFailure::Wire), LinkClose::Transport);
        eventually(|| fake.calls().contains(&Call::HubEnded(1)));
        eventually(|| fake.connects('B') == 3);
        assert_eq!(hub_starts(&fake), 1);
        assert!(app.sharing.worker_alive(fingerprint('B')));
        finish(&app, &path);
    }

    #[test]
    fn a_hub_start_failure_backs_off_as_a_failure() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let saved = preferences();
        let record = crate::sharing_preferences::tests::group(&saved);
        let path = setup_holding("hub-start", &record);
        let fake = Fake::default();
        fake.state().hub_failure = Some(SessionFailure::NativeStartup);
        fake.answer('B', Answer::Accept(Box::new(inspection(&saved))));
        let app = app(&fake, &path);
        supervise_until(&app, || peer_json(&app, 'B')["phase"] == "error");
        assert!(
            app.sharing
                .within_failure_backoff(fingerprint('B'), Duration::from_secs(10))
        );
        assert_eq!(
            peer_json(&app, 'B')["message"],
            "Input setup could not finish. Turn sharing off and on."
        );
        // The session it met is closed, and nothing takes it for a layout to arrange.
        assert!(fake.calls().contains(&Call::CloseMember(key('B'))));
        supervise_idle(&app);
        assert_eq!(fake.connects('B'), 1);
        assert!(!app.sharing.worker_alive(fingerprint('B')));
        assert_eq!(peer_json(&app, 'B')["phase"], "error");
        finish(&app, &path);
    }

    #[test]
    fn a_replaced_sessions_end_report_leaves_the_new_session_alone() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = group(3, &[B, C], everyone(&['B', 'C']));
        let plan = plan_of(&record, 1);
        let fake = Fake::default();
        let network = network_on(&fake);
        fake.accept('B', &record);
        let first = wait_for(network.connect_share(share_request(&plan, 'B'))).expect("a session");
        // A session that outlived its worker is replaced by the next one with that computer.
        fake.accept('B', &record);
        let second = wait_for(network.connect_share(share_request(&plan, 'B'))).expect("a session");
        let replaced = wait_for(first.ended).expect("the first session's end");
        assert!(matches!(replaced.result, Err(SessionFailure::Revoked)));
        // The hub reports the replaced session's end only now.
        fake.end('B', Err(SessionFailure::Wire), LinkClose::Transport);
        eventually(|| {
            let state = fake.state();
            state
                .sessions
                .iter()
                .filter(|live| live.key == key('B'))
                .count()
                == 1
        });
        std::thread::sleep(Duration::from_millis(100));
        let mut ended = second.ended;
        assert!(matches!(
            ended.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(fake.live('B'));
        assert_eq!(fake.count(|call| *call == Call::CloseMember(key('B'))), 0);
        // The new session's own end still reaches it.
        fake.end('B', Err(SessionFailure::Wire), LinkClose::Transport);
        let end = wait_for(ended).expect("the second session's end");
        assert!(matches!(end.result, Err(SessionFailure::Wire)));
        network.release_share(fingerprint('B'));
        shut(&network);
    }

    #[test]
    fn an_end_report_lets_the_network_go_idle() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = group(3, &[B, C], everyone(&['B', 'C']));
        let plan = plan_of(&record, 1);
        let fake = Fake::default();
        let network = network_on(&fake);
        fake.accept('B', &record);
        let up = wait_for(network.connect_share(share_request(&plan, 'B'))).expect("a session");
        // The worker lets go while its session still runs: only the session's end is left.
        network.release_share(fingerprint('B'));
        let end = wait_for(up.ended).expect("the session's end");
        assert!(matches!(end.result, Err(SessionFailure::Revoked)));
        eventually(|| network.is_quiet());
        assert!(fake.calls().contains(&Call::Retire));
        network.request_shutdown();
    }

    #[test]
    fn a_dying_threads_flags_do_not_clobber_a_new_thread() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = group(3, &[B, C], everyone(&['B', 'C']));
        let plan = plan_of(&record, 1);
        let fake = Fake::default();
        let network = network_on(&fake);
        // B does not answer, so its connect keeps the first thread's endpoint bound.
        let first = network.connect_share(share_request(&plan, 'B'));
        eventually(|| fake.connects('B') == 1);
        let (open, gate) = std::sync::mpsc::channel();
        {
            let mut state = fake.state();
            state.panic_on_forget = true;
            state.drop_gate = Some(gate);
        }
        // The first thread fails; while it is still ending, a second one starts and binds.
        network.forget(fingerprint('D'));
        eventually(|| fake.state().dropping);
        let second = network.connect_share(share_request(&plan, 'C'));
        eventually(|| fake.connects('C') == 1);
        assert_eq!(binds(&fake).len(), 2);
        assert!(!network.is_quiet());
        open.send(()).unwrap();
        assert!(matches!(
            wait_for(first),
            Err(ShareFailure::Setup(SetupFailure::Cancelled))
        ));
        // The first thread's last writes land on its own state: the second is still bound.
        let watched = Instant::now() + Duration::from_millis(300);
        while Instant::now() < watched {
            assert!(
                !network.is_quiet(),
                "an ended thread overwrote the running one's state"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        network.release_share(fingerprint('C'));
        assert!(matches!(
            wait_for(second),
            Err(ShareFailure::Setup(SetupFailure::Cancelled))
        ));
        shut(&network);
    }

    #[test]
    fn a_revoked_share_request_binds_nothing() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = group(3, &[B, C], everyone(&['B', 'C']));
        let plan = plan_of(&record, 1);
        let fake = Fake::default();
        let network = network_on(&fake);
        let revoked = share_request(&plan, 'B');
        revoked.cancel.revoke();
        assert!(matches!(
            wait_for(network.connect_share(revoked)),
            Err(ShareFailure::Setup(SetupFailure::Cancelled))
        ));
        shut(&network);
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());

        // A request still queued when the app quits binds nothing either.
        let fake = Fake::default();
        let (open, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let port = fake.clone();
        let network = SharingNetwork::with_port(move || {
            let _ = lock(&gate).recv();
            FakePort(port.clone())
        });
        let queued = network.connect_share(share_request(&plan, 'B'));
        network.request_shutdown();
        drop(open);
        assert!(matches!(
            wait_for(queued),
            Err(ShareFailure::Setup(SetupFailure::Cancelled))
        ));
        eventually(|| network.is_quiet());
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[test]
    fn a_dying_hub_is_not_reused() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let record = group(3, &[B, C], everyone(&['B', 'C']));
        let plan = plan_of(&record, 1);
        let fake = Fake::default();
        let network = network_on(&fake);
        fake.accept('B', &record);
        let b = wait_for(network.connect_share(share_request(&plan, 'B'))).expect("a session");
        // The hub fails: its loop takes no more commands while its native cleanup still runs.
        fake.state().closed.insert(1);
        fake.accept('C', &record);
        let c = network.connect_share(share_request(&plan, 'C'));
        let c = std::thread::spawn(move || wait_for(c));
        eventually(|| fake.connects('C') == 1);
        std::thread::sleep(Duration::from_millis(100));
        // C's session waits for that hub to finish instead of being handed to it and lost.
        assert_eq!(fake.added('C'), 0);
        assert_eq!(fake.count(|call| *call == Call::CloseMember(key('C'))), 0);
        assert_eq!(hub_starts(&fake), 1);
        fake.state().released.insert(1);
        let c = c.join().unwrap().expect("C's session");
        assert!(wait_for(b.ended).expect("B's end").result.is_err());
        let restarted = fake
            .calls()
            .iter()
            .rposition(|call| matches!(call, Call::StartHub(..)))
            .unwrap();
        assert!(position(&fake, &Call::HubEnded(1)) < restarted);
        assert_eq!((hub_starts(&fake), fake.added('C')), (2, 1));
        assert!(fake.live('C'));
        drop(c);
        network.release_share(fingerprint('B'));
        network.release_share(fingerprint('C'));
        shut(&network);
    }

    #[test]
    fn a_setup_link_bind_uses_the_enabled_group_for_address_ties() {
        let _test = lock(&crate::NATIVE_LIFECYCLE_TEST_LOCK);
        let trio = group(4, &[B, C], everyone(&['B', 'C']));
        let fake = Fake::default();
        // C's recorded address is shared with an older pairing record: C wins it only named.
        fake.state().tied.insert(key('C'));
        let network = network_on(&fake);
        // The group is known before the thread runs, and the first request is a setup link.
        network.set_group(plan_of(&trio, 1));
        let link = network.open_link(link_request('B'));
        eventually(|| binds(&fake).len() == 1);
        assert_eq!(binds(&fake), [vec![key('B'), key('C')]]);
        drop(link);
        shut(&network);

        // An endpoint bound before the group named C left C out: the first request under a
        // group naming it binds again.
        let fake = Fake::default();
        fake.state().tied.insert(key('C'));
        fake.state().off_subnet.insert(key('D'));
        let network = network_on(&fake);
        network.set_group(plan_of(&group(3, &[B], everyone(&['B'])), 1));
        let link = network.open_link(link_request('B'));
        eventually(|| binds(&fake).len() == 1);
        fake.accept('C', &trio);
        let c = wait_for(network.connect_share(share_request(&plan_of(&trio, 2), 'C')))
            .expect("C's session");
        assert_eq!(binds(&fake), [vec![key('B')], vec![key('B'), key('C')]]);
        let dialed = Call::Connect(key('C'), trio.content_digest());
        assert!(position(&fake, &Call::Retire) < position(&fake, &dialed));
        // A member named at bind and still left out cannot be admitted by binding again.
        let quad = plan_of(
            &group(
                5,
                &[B, C, ('D', 4, 1920, "top", "bottom")],
                everyone(&['B', 'C', 'D']),
            ),
            3,
        );
        for _ in 0..2 {
            assert!(matches!(
                wait_for(network.connect_share(share_request(&quad, 'D'))),
                Err(ShareFailure::Setup(SetupFailure::NetworkRoute))
            ));
        }
        assert_eq!(binds(&fake).len(), 3);
        assert_eq!(binds(&fake)[2], [key('B'), key('C'), key('D')]);
        drop((link, c));
        shut(&network);
    }
}
