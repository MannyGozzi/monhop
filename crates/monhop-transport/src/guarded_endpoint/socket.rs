use std::{
    fmt,
    future::Future,
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use monhop_core::revocation::RevocationSignal;
use quinn::{
    AsyncUdpSocket, UdpPoller,
    udp::{RecvMeta, Transmit},
};

use super::native::NativeSocket;
use crate::policy::NetworkLock;

const MAX_DATAGRAM_BYTES: usize = 65_507;
const RECEIVE_BUDGET: usize = 32;
type WritableFuture = Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync>>;

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
    peer: SocketAddrV4,
    interface_index: u32,
    signal: RevocationSignal,
}

impl<I> fmt::Debug for GuardedSocket<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuardedSocket")
            .field("revoked", &self.signal.is_revoked())
            .finish_non_exhaustive()
    }
}

impl GuardedSocket<NativeSocket> {
    pub(super) fn new(
        io: NativeSocket,
        lock: &NetworkLock,
        local: SocketAddrV4,
        peer: SocketAddrV4,
        signal: RevocationSignal,
    ) -> io::Result<Self> {
        if lock.is_revoked() || signal.is_revoked() {
            return Err(revoked_error());
        }
        if local.port() == 0
            || peer.port() == 0
            || *peer.ip() != lock.peer()
            || *local.ip() != lock.selected().address
            || io.local_addr()? != local
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket does not match the authorized network",
            ));
        }
        Ok(Self {
            io: Arc::new(io),
            local,
            peer,
            interface_index: lock.selected().index,
            signal,
        })
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

    fn permits(&self, packet: Arrival) -> bool {
        packet.source == self.peer
            && packet.destination == *self.local.ip()
            && packet.interface_index == self.interface_index
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
        if transmit.destination != SocketAddr::V4(self.peer)
            || transmit
                .src_ip
                .is_some_and(|ip| ip != IpAddr::V4(*self.local.ip()))
            || transmit.segment_size.is_some()
            || transmit.contents.is_empty()
            || transmit.contents.len() > MAX_DATAGRAM_BYTES
        {
            self.revoke_because(&"a transmit left the pinned peer, address, or size bounds");
            return Err(io::ErrorKind::WouldBlock.into());
        }
        // No outgoing source/ECN ancillary data: IP_PKTINFO could override macOS interface pinning.
        let result = self.io.try_send_to(transmit.contents, self.peer);
        if self.signal.is_revoked() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        match result {
            Ok(length) if length == transmit.contents.len() => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Err(error),
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
            if packet.length == 0 || !self.permits(packet) {
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
