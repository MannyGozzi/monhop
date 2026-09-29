use std::{
    fmt,
    future::Future,
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    pin::Pin,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use monhop_core::revocation::RevocationSignal;
use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};
use tokio::{
    sync::{Notify, watch},
    time::Instant,
};

use super::{TokenBucket, native::NativeSocket};
use crate::{
    crypto::MAX_PENDING_INCOMING,
    policy::{MAX_PINNED_PEERS, NetworkLock},
};

const MAX_DATAGRAM_BYTES: usize = 65_507;
const RECEIVE_BUDGET: usize = 32;
/// QUIC first packets one member may send back to back; each can queue a handshake in Quinn.
const FIRST_PACKET_BURST: u32 = 8;
/// How often a member's first-packet budget regains one packet.
const FIRST_PACKET_REFILL: Duration = Duration::from_millis(100);
// Every member's whole burst fits Quinn's incoming queue at once, so no member's flood fills it.
const _: () = assert!(MAX_PINNED_PEERS * FIRST_PACKET_BURST as usize <= MAX_PENDING_INCOMING);
type WritableFuture = Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync>>;

/// Raw send errors, beyond the unreachable kinds, that say one destination cannot take a datagram
/// now: its host is down or has no route, or the send buffers are momentarily full.
#[cfg(windows)]
const UNREACHABLE_SEND_ERRORS: [i32; 4] = [
    10_064, // WSAEHOSTDOWN
    10_065, // WSAEHOSTUNREACH
    10_055, // WSAENOBUFS
    10_051, // WSAENETUNREACH
];
#[cfg(target_os = "macos")]
const UNREACHABLE_SEND_ERRORS: [i32; 4] = [
    64, // EHOSTDOWN
    65, // EHOSTUNREACH
    55, // ENOBUFS
    51, // ENETUNREACH
];
#[cfg(not(any(windows, target_os = "macos")))]
const UNREACHABLE_SEND_ERRORS: [i32; 0] = [];

/// A send that failed only for this datagram's destination: QUIC loss recovery resends it, and
/// one member that cannot be reached never ends the others' sessions.
fn destination_unreachable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable
    ) || error
        .raw_os_error()
        .is_some_and(|code| UNREACHABLE_SEND_ERRORS.contains(&code))
}

/// A QUIC long-header Initial, the only packet that makes Quinn queue a new incoming handshake.
fn is_first_packet(datagram: &[u8]) -> bool {
    datagram.first().is_some_and(|first| first & 0xB0 == 0x80)
}

/// Which members the last route check could reach. A member without a usable route is absent:
/// nothing is sent to it, nothing from it reaches Quinn and dials to it fail, until a later check
/// finds its route. Shared by the route checks, the socket and the dialer.
pub(super) struct Reachability {
    unreachable: Box<[AtomicBool]>,
    /// Woken by every verdict that leaves a member set aside.
    set_aside: Notify,
}

impl Reachability {
    /// Every member starts reachable.
    pub(super) fn new(members: usize) -> Arc<Self> {
        Arc::new(Self {
            unreachable: (0..members).map(|_| AtomicBool::new(false)).collect(),
            set_aside: Notify::new(),
        })
    }

    pub(super) fn len(&self) -> usize {
        self.unreachable.len()
    }

    pub(super) fn reaches(&self, member: usize) -> bool {
        self.unreachable
            .get(member)
            .is_some_and(|unreachable| !unreachable.load(Ordering::Acquire))
    }

    pub(super) fn reaches_all(&self) -> bool {
        self.unreachable
            .iter()
            .all(|unreachable| !unreachable.load(Ordering::Acquire))
    }

    /// Resolves once a verdict leaves some member set aside.
    pub(super) async fn some_set_aside(&self) {
        while self.reaches_all() {
            self.set_aside.notified().await;
        }
    }

    /// Takes one route check's verdict, in member order.
    pub(super) fn set(&self, reachable: &[bool]) {
        for (member, (flag, &reachable)) in self.unreachable.iter().zip(reachable).enumerate() {
            let was_unreachable = flag.swap(!reachable, Ordering::AcqRel);
            if was_unreachable == reachable {
                if reachable {
                    log::info!("guarded socket: member slot {member} is reachable again");
                } else {
                    log::warn!(
                        "guarded socket: member slot {member} has no usable route; its datagrams \
                         are dropped until a check finds one"
                    );
                }
            }
        }
        if !self.reaches_all() {
            self.set_aside.notify_one();
        }
    }
}

/// Datagrams one member lost at this socket without a revocation, for diagnostics.
#[derive(Default)]
struct Dropped {
    sends: AtomicU64,
    first_packets: AtomicU64,
}

/// Counts one drop, logging the count at powers of two so a lasting outage stays a few lines.
fn count_drop(counter: &AtomicU64, member: usize, what: &str, cause: &dyn fmt::Display) {
    let dropped = counter.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    if dropped.is_power_of_two() {
        log::debug!("guarded socket: {dropped} {what} for member slot {member} dropped: {cause}");
    }
}

#[derive(Clone, Copy)]
pub(super) struct Arrival {
    length: usize,
    source: SocketAddrV4,
    destination: Ipv4Addr,
    interface_index: u32,
}

pub(super) trait DatagramIo: Send + Sync + 'static {
    fn poll_receive(&self, cx: &mut Context<'_>, buffer: &mut [u8]) -> Poll<io::Result<Arrival>>;
    fn try_send_to(&self, buffer: &[u8], peer: SocketAddrV4) -> io::Result<usize>;
    fn writable(self: Arc<Self>) -> WritableFuture;
    fn local_addr(&self) -> io::Result<SocketAddrV4>;
}

impl DatagramIo for NativeSocket {
    fn poll_receive(&self, cx: &mut Context<'_>, buffer: &mut [u8]) -> Poll<io::Result<Arrival>> {
        NativeSocket::poll_receive(self, cx, buffer).map(|result| {
            result.map(|packet| Arrival {
                length: packet.length,
                source: packet.source,
                destination: packet.destination,
                interface_index: packet.interface_index,
            })
        })
    }

    fn try_send_to(&self, buffer: &[u8], peer: SocketAddrV4) -> io::Result<usize> {
        NativeSocket::try_send_to(self, buffer, peer)
    }

    fn writable(self: Arc<Self>) -> WritableFuture {
        Box::pin(async move { NativeSocket::writable(&self).await })
    }

    fn local_addr(&self) -> io::Result<SocketAddrV4> {
        NativeSocket::local_addr(self)
    }
}

pub(super) struct GuardedSocket<I> {
    io: Arc<I>,
    local: SocketAddrV4,
    /// Fixed for the socket's life: exactly the lock's peers, each with its pinned port.
    members: Box<[SocketAddrV4]>,
    /// In member order, like everything else below indexed by member.
    reachability: Arc<Reachability>,
    first_packets: Mutex<Box<[TokenBucket]>>,
    dropped: Box<[Dropped]>,
    interface_index: u32,
    signal: RevocationSignal,
    /// Dropped with the socket, ending every `lifetime()` wait. Declared after `io`, so the OS
    /// socket is closed by the time a wait ends.
    lifetime: watch::Sender<()>,
}

impl<I> fmt::Debug for GuardedSocket<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardedSocket")
            .field("revoked", &self.signal.is_revoked())
            .finish_non_exhaustive()
    }
}

impl<I: DatagramIo> GuardedSocket<I> {
    /// `reachability` is the route checks' verdict for exactly `members`, in the same order.
    pub(super) fn new(
        io: I,
        lock: &NetworkLock,
        local: SocketAddrV4,
        members: &[SocketAddrV4],
        reachability: Arc<Reachability>,
        signal: RevocationSignal,
    ) -> io::Result<Self> {
        if lock.is_revoked() || signal.is_revoked() {
            return Err(revoked_error());
        }
        if local.port() == 0
            || !members_match_lock(members, lock)
            || reachability.len() != members.len()
            || *local.ip() != lock.selected().address
            || io.local_addr()? != local
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket does not match the authorized network",
            ));
        }
        Ok(Self::pinned(
            io,
            local,
            members,
            lock.selected().index,
            reachability,
            signal,
        ))
    }
}

impl<I> GuardedSocket<I> {
    /// Only after the checks `new` makes, or in a test fixture pinning addresses of its own.
    fn pinned(
        io: I,
        local: SocketAddrV4,
        members: &[SocketAddrV4],
        interface_index: u32,
        reachability: Arc<Reachability>,
        signal: RevocationSignal,
    ) -> Self {
        let now = Instant::now();
        Self {
            io: Arc::new(io),
            local,
            members: members.into(),
            reachability,
            first_packets: Mutex::new(
                members
                    .iter()
                    .map(|_| TokenBucket::new(FIRST_PACKET_BURST, FIRST_PACKET_REFILL, now))
                    .collect(),
            ),
            dropped: members.iter().map(|_| Dropped::default()).collect(),
            interface_index,
            signal,
            lifetime: watch::Sender::new(()),
        }
    }
}

/// The same distinct peers as the lock, in its order, each with a nonzero port.
fn members_match_lock(members: &[SocketAddrV4], lock: &NetworkLock) -> bool {
    (1..=MAX_PINNED_PEERS).contains(&members.len())
        && members.len() == lock.peers().len()
        && members
            .iter()
            .zip(lock.peers())
            .all(|(member, peer)| member.port() != 0 && member.ip() == peer)
        && members
            .iter()
            .enumerate()
            .all(|(index, member)| !members[..index].contains(member))
}

impl<I> GuardedSocket<I> {
    /// Never changes; `changed()` errs once the socket is dropped and its OS socket closed.
    pub(super) fn lifetime(&self) -> watch::Receiver<()> {
        self.lifetime.subscribe()
    }

    pub(super) fn revocation(&self) -> RevocationSignal {
        self.signal.clone()
    }

    pub(super) fn reachability(&self) -> Arc<Reachability> {
        self.reachability.clone()
    }
}

impl<I: DatagramIo> GuardedSocket<I> {
    fn check_active(&self) -> io::Result<()> {
        if self.signal.is_revoked() {
            Err(revoked_error())
        } else {
            Ok(())
        }
    }

    // A socket error ends the session as "Revoked"; the log line is the only record of what it was.
    #[track_caller]
    fn revoke_because(&self, reason: &dyn std::fmt::Display) {
        log::warn!("guarded socket: {reason}; revoked");
        self.signal.revoke();
    }

    #[track_caller]
    fn fail(&self, error: io::Error) -> io::Error {
        self.revoke_because(&error);
        // Quinn ignores ConnectionReset. A revoked boundary must terminate its driver instead.
        if error.kind() == io::ErrorKind::ConnectionReset {
            io::Error::new(io::ErrorKind::ConnectionAborted, error)
        } else {
            error
        }
    }

    /// The reachable member a datagram came from, if it arrived on the pinned address and
    /// interface.
    fn sender(&self, packet: Arrival) -> Option<usize> {
        if packet.length == 0
            || packet.destination != *self.local.ip()
            || packet.interface_index != self.interface_index
        {
            return None;
        }
        let member = self
            .members
            .iter()
            .position(|member| *member == packet.source)?;
        self.reachability.reaches(member).then_some(member)
    }

    /// Whether `member`'s first-packet budget admits one more, so a flood from one recorded
    /// address never fills Quinn's incoming queue for the others.
    fn admits_first_packet(&self, member: usize) -> bool {
        let mut budgets = self
            .first_packets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if budgets[member].take(Instant::now()) {
            return true;
        }
        drop(budgets);
        count_drop(
            &self.dropped[member].first_packets,
            member,
            "first packets",
            &"over the rate limit",
        );
        false
    }

    /// The member a transmit is for, if it also keeps the pinned source address and size bounds.
    fn permitted_destination(&self, transmit: &Transmit<'_>) -> Option<(usize, SocketAddrV4)> {
        let SocketAddr::V4(destination) = transmit.destination else {
            return None;
        };
        let member = self
            .members
            .iter()
            .position(|member| *member == destination)?;
        if transmit
            .src_ip
            .is_some_and(|ip| ip != IpAddr::V4(*self.local.ip()))
            || transmit.segment_size.is_some()
            || transmit.contents.is_empty()
            || transmit.contents.len() > MAX_DATAGRAM_BYTES
        {
            return None;
        }
        Some((member, destination))
    }
}

impl<I: DatagramIo> AsyncUdpSocket for GuardedSocket<I> {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(WritePoller {
            socket: self,
            pending: None,
        })
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        // Quinn abandons connection cleanup on a hard send error. Fail through its receive driver.
        if self.signal.is_revoked() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let Some((member, destination)) = self.permitted_destination(transmit) else {
            self.revoke_because(&"a transmit left the pinned peers, address, or size bounds");
            return Err(io::ErrorKind::WouldBlock.into());
        };
        // Reported as sent: QUIC loss recovery owns a datagram that cannot leave.
        if !self.reachability.reaches(member) {
            count_drop(
                &self.dropped[member].sends,
                member,
                "datagrams",
                &"no usable route",
            );
            return Ok(());
        }
        // No outgoing source/ECN ancillary data: IP_PKTINFO could override macOS interface pinning.
        let result = self.io.try_send_to(transmit.contents, destination);
        if self.signal.is_revoked() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        match result {
            Ok(length) if length == transmit.contents.len() => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Err(error),
            Err(error) if destination_unreachable(&error) => {
                count_drop(&self.dropped[member].sends, member, "datagrams", &error);
                Ok(())
            }
            Ok(_) => {
                self.revoke_because(&"a datagram was written short");
                Err(io::ErrorKind::WouldBlock.into())
            }
            Err(error) => {
                self.revoke_because(&error);
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Only Quinn's single endpoint receive driver observes this signal, never writer pollers.
        if self.signal.poll_revoked(cx).is_ready() {
            return Poll::Ready(Err(revoked_error()));
        }
        let (Some(buffer), Some(output)) = (bufs.first_mut(), meta.first_mut()) else {
            return Poll::Ready(Err(self.fail(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing receive buffer or metadata",
            ))));
        };
        let capacity = buffer.len().min(MAX_DATAGRAM_BYTES);
        if capacity == 0 {
            return Poll::Ready(Err(self.fail(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty receive buffer",
            ))));
        }
        for _ in 0..RECEIVE_BUDGET {
            if let Err(error) = self.check_active() {
                return Poll::Ready(Err(error));
            }
            let result = self.io.poll_receive(cx, &mut buffer[..capacity]);
            if let Err(error) = self.check_active() {
                return Poll::Ready(Err(error));
            }
            let packet = match result {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(self.fail(error))),
                Poll::Ready(Ok(packet)) => packet,
            };
            if packet.length > capacity {
                return Poll::Ready(Err(self.fail(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid received datagram length",
                ))));
            }
            let Some(member) = self.sender(packet) else {
                continue;
            };
            if is_first_packet(&buffer[..packet.length]) && !self.admits_first_packet(member) {
                continue;
            }
            *output = RecvMeta {
                addr: packet.source.into(),
                len: packet.length,
                stride: packet.length,
                ecn: None,
                dst_ip: Some(packet.destination.into()),
            };
            return Poll::Ready(Ok(1));
        }
        // Bound unrelated traffic per poll without revoking a valid session or starving other tasks.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.check_active()?;
        match self.io.local_addr() {
            Ok(local) if local == self.local => Ok(local.into()),
            Ok(_) => Err(self.fail(io::Error::new(
                io::ErrorKind::InvalidData,
                "bound address changed",
            ))),
            Err(error) => Err(self.fail(error)),
        }
    }
}

struct WritePoller<I> {
    socket: Arc<GuardedSocket<I>>,
    pending: Option<WritableFuture>,
}

impl<I> fmt::Debug for WritePoller<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WritePoller").finish_non_exhaustive()
    }
}

impl<I: DatagramIo> UdpPoller for WritePoller<I> {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.socket.signal.is_revoked() {
            this.pending = None;
            return Poll::Pending;
        }
        let future = this
            .pending
            .get_or_insert_with(|| this.socket.io.clone().writable());
        let result = future.as_mut().poll(cx);
        if this.socket.signal.is_revoked() {
            this.pending = None;
            return Poll::Pending;
        }
        match result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.pending = None;
                match result {
                    Ok(()) => Poll::Ready(Ok(())),
                    Err(error) => {
                        this.socket.revoke_because(&error);
                        Poll::Pending
                    }
                }
            }
        }
    }
}

pub(super) fn revoked_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "network session revoked; reconnect explicitly",
    )
}

#[cfg(test)]
mod tests;
