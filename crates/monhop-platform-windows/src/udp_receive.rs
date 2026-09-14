//! Bounded IPv4 UDP receive metadata for an owned, interface-pinned socket.

use std::{
    io,
    mem::size_of,
    net::{Ipv4Addr, SocketAddrV4},
};

const PACKET_INFO_BYTES: usize = 8;
const CMSG_ALIGNMENT: usize = size_of::<usize>();
const CMSG_HEADER_BYTES: usize = size_of::<usize>() + size_of::<i32>() * 2;
#[cfg(test)]
const PACKET_INFO_RECORD_BYTES: usize =
    cmsg_align(CMSG_HEADER_BYTES) + cmsg_align(PACKET_INFO_BYTES);
const CONTROL_BYTES: usize = 256;
const MAX_BUFFER_BYTES: usize = 65_535;
const MAX_IPV4_UDP_PAYLOAD_BYTES: usize = 65_507;

#[derive(Clone, Copy, PartialEq, Eq)]
struct PacketInfo {
    destination: Ipv4Addr,
    interface_index: u32,
}

/// Metadata from one complete IPv4 UDP datagram.
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ReceivedDatagram {
    pub length: usize,
    pub source: SocketAddrV4,
    pub destination: Ipv4Addr,
    pub interface_index: u32,
}

const fn cmsg_align(length: usize) -> usize {
    (length + CMSG_ALIGNMENT - 1) & !(CMSG_ALIGNMENT - 1)
}

fn invalid_data() -> io::Error {
    io::Error::from(io::ErrorKind::InvalidData)
}

fn validate_buffer_length(length: usize) -> io::Result<()> {
    if length == 0 || length > MAX_BUFFER_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok(())
}

fn validate_received_length(received: usize, buffer_length: usize) -> io::Result<()> {
    if received > buffer_length || received > MAX_IPV4_UDP_PAYLOAD_BYTES {
        return Err(invalid_data());
    }
    Ok(())
}

fn parse_ipv4_source(address: &[u8], ipv4_family: u16) -> io::Result<SocketAddrV4> {
    if address.len() < 16 {
        return Err(invalid_data());
    }
    let family = u16::from_ne_bytes([address[0], address[1]]);
    if family != ipv4_family {
        return Err(invalid_data());
    }
    Ok(SocketAddrV4::new(
        Ipv4Addr::new(address[4], address[5], address[6], address[7]),
        u16::from_be_bytes([address[2], address[3]]),
    ))
}

fn parse_packet_info(
    control: &[u8],
    packet_level: i32,
    packet_type: i32,
) -> io::Result<PacketInfo> {
    let mut offset = 0;
    let mut packet_info = None;

    while offset < control.len() {
        let remaining = &control[offset..];
        if remaining.iter().all(|byte| *byte == 0) {
            break;
        }
        if remaining.len() < CMSG_HEADER_BYTES {
            return Err(invalid_data());
        }

        let message_len =
            read_native_usize(&remaining[..size_of::<usize>()]).ok_or_else(invalid_data)?;
        if message_len < CMSG_HEADER_BYTES || message_len > remaining.len() {
            return Err(invalid_data());
        }
        let level_offset = size_of::<usize>();
        let level = read_i32(&remaining[level_offset..level_offset + size_of::<i32>()])
            .ok_or_else(invalid_data)?;
        let kind_offset = level_offset + size_of::<i32>();
        let kind = read_i32(&remaining[kind_offset..kind_offset + size_of::<i32>()])
            .ok_or_else(invalid_data)?;

        if level == packet_level && kind == packet_type {
            if message_len != CMSG_HEADER_BYTES + PACKET_INFO_BYTES || packet_info.is_some() {
                return Err(invalid_data());
            }
            let data = &remaining[CMSG_HEADER_BYTES..message_len];
            let interface_index = read_u32(&data[4..]).ok_or_else(invalid_data)?;
            if interface_index == 0 {
                return Err(invalid_data());
            }
            let destination = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
            if destination.is_unspecified() {
                return Err(invalid_data());
            }
            packet_info = Some(PacketInfo {
                destination,
                interface_index,
            });
        }

        let next = cmsg_align(message_len);
        if next > remaining.len() {
            return Err(invalid_data());
        }
        offset += next;
    }

    packet_info.ok_or_else(invalid_data)
}

fn read_native_usize(bytes: &[u8]) -> Option<usize> {
    if size_of::<usize>() == size_of::<u64>() {
        let value = u64::from_ne_bytes(bytes.get(..8)?.try_into().ok()?);
        usize::try_from(value).ok()
    } else if size_of::<usize>() == size_of::<u32>() {
        let value = u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
        usize::try_from(value).ok()
    } else {
        None
    }
}

fn read_i32(bytes: &[u8]) -> Option<i32> {
    Some(i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?))
}

#[cfg(windows)]
mod windows {
    #[cfg(test)]
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::{
        io,
        mem::size_of,
        net::{SocketAddr, SocketAddrV4, UdpSocket},
        os::windows::io::AsRawSocket,
        ptr,
        task::{Context, Poll},
    };

    use tokio::{io::Interest, net::UdpSocket as TokioUdpSocket};
    use windows_sys::{
        Win32::{
            Networking::WinSock::{
                AF_INET, IP_PKTINFO, IPPROTO_IP, LPFN_WSARECVMSG,
                LPWSAOVERLAPPED_COMPLETION_ROUTINE, MSG_CTRUNC, MSG_TRUNC,
                SIO_GET_EXTENSION_FUNCTION_POINTER, SIO_UDP_CONNRESET, SIO_UDP_NETRESET, SOCKADDR,
                SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR, WSABUF, WSAEMSGSIZE, WSAEWOULDBLOCK,
                WSAGetLastError, WSAID_WSARECVMSG, WSAIoctl, WSAMSG, setsockopt,
            },
            System::IO::OVERLAPPED,
        },
        core::BOOL,
    };

    use super::{
        CONTROL_BYTES, PacketInfo, ReceivedDatagram, invalid_data, parse_ipv4_source,
        parse_packet_info, validate_buffer_length, validate_received_length,
    };

    type RecvMsg = unsafe extern "system" fn(
        SOCKET,
        *mut WSAMSG,
        *mut u32,
        *mut OVERLAPPED,
        LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;

    #[repr(C, align(8))]
    struct ControlBuffer([u8; CONTROL_BYTES]);

    /// A resolved WSARecvMsg callback tied to its retained UDP socket.
    pub struct UdpReceiver {
        socket: UdpSocket,
        recv_msg: RecvMsg,
    }

    /// A Tokio-registered receiver retaining the socket that resolved WSARecvMsg.
    pub struct AsyncUdpReceiver {
        socket: TokioUdpSocket,
        recv_msg: RecvMsg,
        #[cfg(test)]
        native_would_blocks: AtomicU32,
    }

    impl UdpReceiver {
        /// Takes an IPv4-bound socket, enables metadata, and resolves its provider callback.
        pub fn configure(socket: UdpSocket) -> io::Result<Self> {
            if !matches!(socket.local_addr()?, SocketAddr::V4(address) if !address.ip().is_unspecified())
            {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            let raw_socket = socket.as_raw_socket() as SOCKET;
            disable_udp_reset_reports(raw_socket)?;
            socket.set_nonblocking(true)?;
            set_packet_info(raw_socket)?;
            let recv_msg = resolve_recv_msg(raw_socket)?;
            Ok(Self { socket, recv_msg })
        }

        /// Borrows the retained socket for readiness registration without transferring ownership.
        pub fn socket(&self) -> &UdpSocket {
            &self.socket
        }

        /// Transfers the retained socket into Tokio and panics outside an active Tokio runtime.
        pub fn into_async(self) -> io::Result<AsyncUdpReceiver> {
            let Self { socket, recv_msg } = self;
            let socket = TokioUdpSocket::from_std(socket)?;
            Ok(AsyncUdpReceiver {
                socket,
                recv_msg,
                #[cfg(test)]
                native_would_blocks: AtomicU32::new(0),
            })
        }

        /// Receives once without waiting.
        pub fn receive(&self, buffer: &mut [u8]) -> io::Result<ReceivedDatagram> {
            validate_buffer_length(buffer.len())?;
            receive_message(self.socket.as_raw_socket() as SOCKET, self.recv_msg, buffer)
        }
    }

    impl AsyncUdpReceiver {
        /// Exactly one task may call this at a time. Multiple writers are supported.
        /// `&self` follows Quinn's one-endpoint receive driver without blocking send readiness.
        pub fn poll_receive(
            &self,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<io::Result<ReceivedDatagram>> {
            if let Err(error) = validate_buffer_length(buffer.len()) {
                return Poll::Ready(Err(error));
            }
            loop {
                match self.socket.poll_recv_ready(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) => {
                        let raw_socket = self.socket.as_raw_socket() as SOCKET;
                        match self.socket.try_io(Interest::READABLE, || {
                            let result = receive_message(raw_socket, self.recv_msg, buffer);
                            #[cfg(test)]
                            if matches!(
                                result.as_ref(),
                                Err(error) if error.kind() == io::ErrorKind::WouldBlock
                            ) {
                                self.record_native_would_block();
                            }
                            result
                        }) {
                            Ok(received) => return Poll::Ready(Ok(received)),
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                            Err(error) => return Poll::Ready(Err(error)),
                        }
                    }
                }
            }
        }

        /// Waits until Tokio reports that the retained socket may be writable.
        pub async fn writable(&self) -> io::Result<()> {
            self.socket.writable().await
        }

        /// Sends once without waiting to the supplied IPv4 peer.
        pub fn try_send_to(&self, buffer: &[u8], peer: SocketAddrV4) -> io::Result<usize> {
            self.socket.try_send_to(buffer, SocketAddr::V4(peer))
        }

        /// Returns the exact IPv4 address retained by this receiver.
        pub fn local_addr(&self) -> io::Result<SocketAddrV4> {
            match self.socket.local_addr()? {
                SocketAddr::V4(address) => Ok(address),
                SocketAddr::V6(_) => Err(invalid_data()),
            }
        }

        #[cfg(test)]
        pub(crate) fn native_would_block_count(&self) -> u32 {
            self.native_would_blocks.load(Ordering::Relaxed)
        }

        #[cfg(test)]
        fn record_native_would_block(&self) {
            self.native_would_blocks.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn receive_message(
        socket: SOCKET,
        recv_msg: RecvMsg,
        buffer: &mut [u8],
    ) -> io::Result<ReceivedDatagram> {
        let mut source = SOCKADDR_STORAGE::default();
        let mut control = ControlBuffer([0; CONTROL_BYTES]);
        let mut data = WSABUF {
            len: buffer.len() as u32,
            buf: buffer.as_mut_ptr(),
        };
        let mut message = WSAMSG {
            name: (&mut source as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            namelen: size_of::<SOCKADDR_STORAGE>() as i32,
            lpBuffers: &mut data,
            dwBufferCount: 1,
            Control: WSABUF {
                len: CONTROL_BYTES as u32,
                buf: control.0.as_mut_ptr(),
            },
            dwFlags: 0,
        };
        let mut received = 0;

        // SAFETY: all message buffers are stack-owned and the non-overlapped call completes before return.
        let result =
            unsafe { recv_msg(socket, &mut message, &mut received, ptr::null_mut(), None) };
        if result == SOCKET_ERROR {
            // SAFETY: WSAGetLastError reads this thread's last socket error.
            return Err(match unsafe { WSAGetLastError() } {
                WSAEWOULDBLOCK => io::Error::from(io::ErrorKind::WouldBlock),
                WSAEMSGSIZE => invalid_data(),
                code => io::Error::from_raw_os_error(code),
            });
        }
        if message.dwFlags & (MSG_TRUNC | MSG_CTRUNC) != 0 {
            return Err(invalid_data());
        }
        let received = received as usize;
        validate_received_length(received, buffer.len())?;
        let source_len = usize::try_from(message.namelen).map_err(|_| invalid_data())?;
        if source_len > size_of::<SOCKADDR_STORAGE>() {
            return Err(invalid_data());
        }
        // SAFETY: source_len is bounded by the live stack allocation and source remains initialized for the call.
        let source_bytes = unsafe {
            std::slice::from_raw_parts(
                (&source as *const SOCKADDR_STORAGE).cast::<u8>(),
                source_len,
            )
        };
        let source = parse_ipv4_source(source_bytes, AF_INET)?;
        let control_len = message.Control.len as usize;
        if control_len > CONTROL_BYTES {
            return Err(invalid_data());
        }
        let PacketInfo {
            destination,
            interface_index,
        } = parse_packet_info(&control.0[..control_len], IPPROTO_IP, IP_PKTINFO)?;

        Ok(ReceivedDatagram {
            length: received,
            source,
            destination,
            interface_index,
        })
    }

    fn set_packet_info(socket: SOCKET) -> io::Result<()> {
        let enabled = 1_u32;
        // SAFETY: the socket is live and enabled is a valid DWORD-sized option value.
        if unsafe {
            setsockopt(
                socket,
                IPPROTO_IP,
                IP_PKTINFO,
                (&enabled as *const u32).cast(),
                size_of::<u32>() as i32,
            )
        } == SOCKET_ERROR
        {
            // SAFETY: WSAGetLastError reads this thread's last socket error.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(())
    }

    fn disable_udp_reset_reports(socket: SOCKET) -> io::Result<()> {
        configure_udp_reset_reports(socket, set_udp_reset_report)
    }

    fn configure_udp_reset_reports(
        socket: SOCKET,
        mut set: impl FnMut(SOCKET, u32) -> io::Result<()>,
    ) -> io::Result<()> {
        for control in [SIO_UDP_CONNRESET, SIO_UDP_NETRESET] {
            set(socket, control)?;
        }
        Ok(())
    }

    fn set_udp_reset_report(socket: SOCKET, control: u32) -> io::Result<()> {
        let disabled: BOOL = 0;
        let mut returned = 0;
        // SAFETY: the owned datagram socket receives one documented BOOL input with no output.
        if unsafe {
            WSAIoctl(
                socket,
                control,
                (&raw const disabled).cast(),
                size_of::<BOOL>() as u32,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
                None,
            )
        } == SOCKET_ERROR
        {
            // SAFETY: WSAGetLastError reads this thread's last socket error.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(())
    }

    fn resolve_recv_msg(socket: SOCKET) -> io::Result<RecvMsg> {
        let guid = WSAID_WSARECVMSG;
        let mut callback: LPFN_WSARECVMSG = None;
        let mut returned = 0;
        // SAFETY: WSAIoctl synchronously writes one function pointer into the correctly sized output buffer.
        if unsafe {
            WSAIoctl(
                socket,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                (&guid as *const windows_sys::core::GUID).cast(),
                size_of::<windows_sys::core::GUID>() as u32,
                (&mut callback as *mut LPFN_WSARECVMSG).cast(),
                size_of::<LPFN_WSARECVMSG>() as u32,
                &mut returned,
                ptr::null_mut(),
                None,
            )
        } == SOCKET_ERROR
        {
            // SAFETY: WSAGetLastError reads this thread's last socket error.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        if returned as usize != size_of::<LPFN_WSARECVMSG>() {
            return Err(invalid_data());
        }
        callback.ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reset_reporting_disables_both_controls_in_order() {
            let mut controls = Vec::new();
            configure_udp_reset_reports(7, |socket, control| {
                assert_eq!(socket, 7);
                controls.push(control);
                Ok(())
            })
            .unwrap();
            assert_eq!(controls, [SIO_UDP_CONNRESET, SIO_UDP_NETRESET]);
        }

        #[test]
        fn reset_reporting_stops_when_the_first_control_fails() {
            let mut controls = Vec::new();
            let error = configure_udp_reset_reports(7, |_, control| {
                controls.push(control);
                Err(io::Error::from(io::ErrorKind::Unsupported))
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert_eq!(controls, [SIO_UDP_CONNRESET]);
        }
    }
}

#[cfg(windows)]
pub use windows::{AsyncUdpReceiver, UdpReceiver};

#[cfg(test)]
mod tests {
    use super::*;

    const IPPROTO_IP: i32 = 0;
    const IP_PKTINFO: i32 = 19;

    fn packet_info_control(
        destination: [u8; 4],
        interface_index: u32,
    ) -> [u8; PACKET_INFO_RECORD_BYTES] {
        let mut control = [0; PACKET_INFO_RECORD_BYTES];
        let length = CMSG_HEADER_BYTES + PACKET_INFO_BYTES;
        control[..size_of::<usize>()].copy_from_slice(&length.to_ne_bytes());
        let level_offset = size_of::<usize>();
        control[level_offset..level_offset + 4].copy_from_slice(&IPPROTO_IP.to_ne_bytes());
        control[level_offset + 4..level_offset + 8].copy_from_slice(&IP_PKTINFO.to_ne_bytes());
        control[CMSG_HEADER_BYTES..CMSG_HEADER_BYTES + 4].copy_from_slice(&destination);
        control[CMSG_HEADER_BYTES + 4..CMSG_HEADER_BYTES + 8]
            .copy_from_slice(&interface_index.to_ne_bytes());
        control
    }

    #[test]
    fn parses_exact_ipv4_packet_info() {
        let control = packet_info_control([192, 0, 2, 1], 7);
        let packet_info = parse_packet_info(&control, IPPROTO_IP, IP_PKTINFO).unwrap();
        assert_eq!(packet_info.destination, Ipv4Addr::new(192, 0, 2, 1));
        assert_eq!(packet_info.interface_index, 7);

        let mut source = [0; 16];
        source[..2].copy_from_slice(&2_u16.to_ne_bytes());
        source[2..4].copy_from_slice(&4_444_u16.to_be_bytes());
        source[4..8].copy_from_slice(&[198, 51, 100, 2]);
        assert_eq!(
            parse_ipv4_source(&source, 2).unwrap(),
            SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 2), 4_444)
        );
    }

    #[test]
    fn rejects_missing_duplicate_and_malformed_packet_info() {
        assert!(parse_packet_info(&[0; CONTROL_BYTES], IPPROTO_IP, IP_PKTINFO).is_err());

        let mut duplicate = [0; PACKET_INFO_RECORD_BYTES * 2];
        duplicate[..PACKET_INFO_RECORD_BYTES]
            .copy_from_slice(&packet_info_control([192, 0, 2, 1], 7));
        duplicate[PACKET_INFO_RECORD_BYTES..]
            .copy_from_slice(&packet_info_control([192, 0, 2, 2], 8));
        assert!(parse_packet_info(&duplicate, IPPROTO_IP, IP_PKTINFO).is_err());

        let mut malformed = packet_info_control([192, 0, 2, 1], 7);
        malformed[..size_of::<usize>()].copy_from_slice(&CMSG_HEADER_BYTES.to_ne_bytes());
        assert!(parse_packet_info(&malformed, IPPROTO_IP, IP_PKTINFO).is_err());
    }

    #[test]
    fn rejects_zero_interface_and_non_ipv4_or_truncated_source() {
        assert!(
            parse_packet_info(
                &packet_info_control([192, 0, 2, 1], 0),
                IPPROTO_IP,
                IP_PKTINFO,
            )
            .is_err()
        );
        assert!(
            parse_packet_info(
                &packet_info_control([0, 0, 0, 0], 7),
                IPPROTO_IP,
                IP_PKTINFO,
            )
            .is_err()
        );
        assert!(parse_ipv4_source(&[0; 15], 2).is_err());

        let mut source = [0; 16];
        source[..2].copy_from_slice(&23_u16.to_ne_bytes());
        assert!(parse_ipv4_source(&source, 2).is_err());
    }

    #[test]
    fn validates_receive_buffer_and_ipv4_payload_bounds() {
        assert!(validate_buffer_length(0).is_err());
        assert!(validate_buffer_length(MAX_BUFFER_BYTES).is_ok());
        assert!(validate_buffer_length(MAX_BUFFER_BYTES + 1).is_err());

        assert!(validate_received_length(0, MAX_BUFFER_BYTES).is_ok());
        assert!(validate_received_length(MAX_IPV4_UDP_PAYLOAD_BYTES, MAX_BUFFER_BYTES).is_ok());
        assert!(
            validate_received_length(MAX_IPV4_UDP_PAYLOAD_BYTES + 1, MAX_BUFFER_BYTES).is_err()
        );
        assert!(validate_received_length(MAX_BUFFER_BYTES, MAX_IPV4_UDP_PAYLOAD_BYTES).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn packet_info_constant_matches_windows_sdk() {
        assert_eq!(
            IP_PKTINFO,
            windows_sys::Win32::Networking::WinSock::IP_PKTINFO
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "explicit localhost-only native UDP metadata probe, not physical-network proof"]
    fn native_loopback_metadata_matches_bound_addresses() {
        use std::{
            net::UdpSocket,
            time::{Duration, Instant},
        };
        use windows_sys::Win32::NetworkManagement::IpHelper::GetBestInterface;
        let mut loopback_index = 0;
        assert_eq!(
            // SAFETY: the output is writable and the destination contains only literal localhost.
            unsafe {
                GetBestInterface(
                    u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets()),
                    &mut loopback_index,
                )
            },
            0
        );
        assert_ne!(loopback_index, 0);
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        crate::network::restrict_udp_interface(&socket, loopback_index).unwrap();
        let receiver_address = socket.local_addr().unwrap();
        let receiver = UdpReceiver::configure(socket).unwrap();
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        crate::network::restrict_udp_interface(&sender, loopback_index).unwrap();
        let payload = b"MonHop metadata probe";
        assert_eq!(
            sender.send_to(payload, receiver_address).unwrap(),
            payload.len()
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut buffer = [0; 256];
        let received = loop {
            match receiver.receive(&mut buffer) {
                Ok(received) => break received,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("native UDP metadata probe failed: {error}"),
            }
        };
        assert_eq!(received.length, payload.len());
        assert_eq!(&buffer[..received.length], payload);
        assert_eq!(
            std::net::SocketAddr::V4(received.source),
            sender.local_addr().unwrap()
        );
        assert_eq!(received.destination, Ipv4Addr::LOCALHOST);
        assert_eq!(received.interface_index, loopback_index);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "explicit localhost-only native async UDP metadata probe, not physical-network proof"]
    fn native_async_loopback_metadata_wakes_and_rejects_truncation() {
        use std::{future::poll_fn, net::UdpSocket, task::Poll, time::Duration};

        use tokio::sync::oneshot;
        use windows_sys::Win32::NetworkManagement::IpHelper::GetBestInterface;

        const OUTBOUND: &[u8] = b"MonHop async writable probe";
        const INBOUND: &[u8] = b"MonHop async metadata probe";
        const SECOND: &[u8] = b"MonHop async stale-readiness probe";
        const TRUNCATED: &[u8] = b"MonHop async truncation probe";

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut loopback_index = 0;
            assert_eq!(
                // SAFETY: the output is writable and the destination contains only literal localhost.
                unsafe {
                    GetBestInterface(
                        u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets()),
                        &mut loopback_index,
                    )
                },
                0
            );
            assert_ne!(loopback_index, 0);

            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            crate::network::restrict_udp_interface(&socket, loopback_index).unwrap();
            let receiver_address = match socket.local_addr().unwrap() {
                std::net::SocketAddr::V4(address) => address,
                std::net::SocketAddr::V6(_) => panic!("loopback listener was not IPv4"),
            };
            let receiver = UdpReceiver::configure(socket)
                .unwrap()
                .into_async()
                .unwrap();
            assert_eq!(receiver.local_addr().unwrap(), receiver_address);

            let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            crate::network::restrict_udp_interface(&sender, loopback_index).unwrap();
            sender.set_nonblocking(true).unwrap();
            let sender_address = match sender.local_addr().unwrap() {
                std::net::SocketAddr::V4(address) => address,
                std::net::SocketAddr::V6(_) => panic!("loopback sender was not IPv4"),
            };

            let outbound = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    receiver.writable().await?;
                    match receiver.try_send_to(OUTBOUND, sender_address) {
                        Ok(sent) => return Ok(sent),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                        Err(error) => return Err(error),
                    }
                }
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(outbound, OUTBOUND.len());

            let (first_send, first_sent) = oneshot::channel();
            let first_sender = tokio::spawn(async move {
                tokio::time::timeout(Duration::from_secs(1), first_sent)
                    .await
                    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
                    .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
                let sent = sender.send_to(INBOUND, receiver_address)?;
                Ok::<(UdpSocket, usize), io::Error>((sender, sent))
            });
            let mut buffer = [0; 256];
            let mut first_send = Some(first_send);
            let first_receive = tokio::time::timeout(
                Duration::from_secs(1),
                poll_fn(|cx| match receiver.poll_receive(cx, &mut buffer) {
                    Poll::Pending => {
                        if let Some(first_send) = first_send.take() {
                            let _ = first_send.send(());
                        }
                        Poll::Pending
                    }
                    Poll::Ready(result) => Poll::Ready(result),
                }),
            )
            .await;
            let first_sender = first_sender.await;
            let received = first_receive.unwrap().unwrap();
            let (sender, sent) = first_sender.unwrap().unwrap();
            assert_eq!(sent, INBOUND.len());
            assert_eq!(received.length, INBOUND.len());
            assert_eq!(&buffer[..received.length], INBOUND);
            assert_eq!(received.source, sender_address);
            assert_eq!(received.destination, Ipv4Addr::LOCALHOST);
            assert_eq!(received.interface_index, loopback_index);

            let first_would_blocks = receiver.native_would_block_count();
            let (second_pending, second_pending_wait) = oneshot::channel();
            let mut waiting_receive = tokio::spawn(async move {
                let mut waiting_buffer = [0; 256];
                let mut second_pending = Some(second_pending);
                let received = poll_fn(|cx| match receiver.poll_receive(cx, &mut waiting_buffer) {
                    Poll::Pending => {
                        if let Some(second_pending) = second_pending.take() {
                            let _ = second_pending.send(());
                        }
                        Poll::Pending
                    }
                    Poll::Ready(result) => Poll::Ready(result),
                })
                .await;
                let native_would_blocks = receiver.native_would_block_count();
                (receiver, received, native_would_blocks)
            });
            let second_pending =
                tokio::time::timeout(Duration::from_secs(1), second_pending_wait).await;
            let second_sent = sender.send_to(SECOND, receiver_address);
            let waiting_result =
                tokio::time::timeout(Duration::from_secs(1), &mut waiting_receive).await;
            let waiting_result = match waiting_result {
                Ok(result) => result,
                Err(_) => {
                    waiting_receive.abort();
                    let _ = waiting_receive.await;
                    panic!("waiting receive did not wake");
                }
            };

            second_pending.unwrap().unwrap();
            assert_eq!(second_sent.unwrap(), SECOND.len());
            let (receiver, waiting, native_would_blocks) = waiting_result.unwrap();
            assert!(native_would_blocks > first_would_blocks);
            let waiting = waiting.unwrap();
            assert_eq!(waiting.length, SECOND.len());
            assert_eq!(waiting.source, sender_address);
            assert_eq!(waiting.destination, Ipv4Addr::LOCALHOST);
            assert_eq!(waiting.interface_index, loopback_index);

            assert_eq!(
                sender.send_to(TRUNCATED, receiver_address).unwrap(),
                TRUNCATED.len()
            );
            let mut small_buffer = [0; 1];
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                poll_fn(|cx| receiver.poll_receive(cx, &mut small_buffer)),
            )
            .await
            .unwrap();
            let error = match result {
                Ok(_) => panic!("truncated datagram was accepted"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        });
    }
}
