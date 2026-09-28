//! Keeps every Quinn driver on the runtime the guarded socket registered with.

use std::{fmt, future::Future, io, pin::Pin, sync::Arc, time::Instant};

use quinn::{AsyncTimer, AsyncUdpSocket, Runtime};
use tokio::runtime::Handle;

/// Spawns the endpoint and connection drivers, and arms their timers, on one network runtime,
/// whichever thread dials. That runtime must never block.
pub(super) struct HandleRuntime(Handle);

impl HandleRuntime {
    pub(super) fn current() -> io::Result<Self> {
        Handle::try_current()
            .map(Self)
            .map_err(|_| io::Error::other("an active Tokio I/O runtime is required"))
    }

    pub(super) fn handle(&self) -> &Handle {
        &self.0
    }
}

impl fmt::Debug for HandleRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HandleRuntime")
    }
}

impl Runtime for HandleRuntime {
    fn new_timer(&self, deadline: Instant) -> Pin<Box<dyn AsyncTimer>> {
        let _network = self.0.enter();
        Box::pin(tokio::time::sleep_until(deadline.into()))
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        drop(self.0.spawn(future));
    }

    /// Only the guarded socket may carry QUIC; a plain socket would bypass its filters.
    fn wrap_udp_socket(&self, _: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "only the guarded socket may carry QUIC",
        ))
    }

    fn now(&self) -> Instant {
        let _network = self.0.enter();
        tokio::time::Instant::now().into_std()
    }
}
