//! Single-datagram IPv4 receive metadata validation for a caller-pinned socket.

use std::{
    ffi::{c_int, c_void},
    io,
    mem::size_of,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    os::fd::AsRawFd,
    task::{Context, Poll},
};

use tokio::io::Interest;

#[cfg(test)]
use std::sync::atomic::{AtomicU32, Ordering};

use crate::network::{IPPROTO_IP, set_ip_option};

const AF_INET: u8 = 2;
const AF_LINK: u8 = 18;
const IP_RECVDSTADDR: c_int = 7;
const IP_RECVIF: c_int = 20;
const IP_PKTINFO: c_int = 26;
const MSG_TRUNC: c_int = 0x10;
const MSG_CTRUNC: c_int = 0x20;
const MSG_DONTWAIT: c_int = 0x80;
const MAX_BUFFER_BYTES: usize = 65_535;
const MAX_IPV4_UDP_PAYLOAD_BYTES: usize = 65_507;
const SOCKET_ADDRESS_STORAGE_BYTES: usize = 128;
const CMSG_ALIGNMENT: usize = 4;
const CMSG_HEADER_BYTES: usize = size_of::<Cmsghdr>();
const IN_PKTINFO_BYTES: usize = size_of::<InPktInfo>();
const IN_ADDR_BYTES: usize = size_of::<InAddr>();
const MAX_CONTROL_BYTES: usize = 256;

const _: () = assert!(size_of::<Cmsghdr>() == 12);
const _: () = assert!(size_of::<InAddr>() == 4);
const _: () = assert!(size_of::<InPktInfo>() == 12);
const _: () = assert!(size_of::<SockaddrDl>() == 20);
const _: () = assert!(size_of::<SockaddrIn>() == 16);
const _: () = assert!(size_of::<Msghdr>() == 48);

#[repr(C)]
struct Iovec {
    base: *mut c_void,
    length: usize,
}

#[repr(C)]
struct Msghdr {
    name: *mut c_void,
    name_length: u32,
    iov: *mut Iovec,
    iov_length: c_int,
    control: *mut c_void,
    control_length: u32,
    flags: c_int,
}

#[repr(C)]
struct Cmsghdr {
    length: u32,
    level: c_int,
    message_type: c_int,
}

#[repr(C)]
struct InAddr {
    address: u32,
}

#[repr(C)]
struct InPktInfo {
    interface_index: u32,
    specified_destination: InAddr,
    destination: InAddr,
}

#[repr(C)]
struct SockaddrDl {
    length: u8,
    family: u8,
    interface_index: u16,
    interface_type: u8,
    name_length: u8,
    address_length: u8,
    selector_length: u8,
    data: [u8; 12],
}

#[repr(C)]
struct SockaddrIn {
    length: u8,
    family: u8,
    port: u16,
    address: InAddr,
    padding: [u8; 8],
}

#[repr(align(8))]
struct SocketAddressStorage([u8; SOCKET_ADDRESS_STORAGE_BYTES]);

#[repr(align(4))]
struct ControlBuffer([u8; MAX_CONTROL_BYTES]);

// SAFETY: Declarations match the installed macOS SDK's BSD socket ABI.
unsafe extern "C" {
    fn recvmsg(socket: c_int, message: *mut Msghdr, flags: c_int) -> isize;
}

/// A received IPv4 UDP datagram with kernel-supplied arrival metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceivedDatagram {
    pub length: usize,
    pub source: SocketAddrV4,
    pub destination: Ipv4Addr,
    pub interface_index: u32,
}

/// Metadata receiver that owns the configured socket.
pub struct UdpReceiver {
    socket: UdpSocket,
}

/// Tokio-ready receiver that retains exclusive ownership of the configured socket.
pub struct AsyncUdpReceiver {
    socket: tokio::net::UdpSocket,
    #[cfg(test)]
    native_would_block_count: AtomicU32,
}

impl UdpReceiver {
    /// Caller must bind an exact non-unspecified IPv4 address and pin it before calling.
    /// `IP_PKTINFO` is canonical; prior optional receive metadata is only cross-checked.
    pub fn configure(socket: UdpSocket) -> io::Result<Self> {
        if !matches!(socket.local_addr()?, SocketAddr::V4(address) if !address.ip().is_unspecified())
        {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }

        let fd = socket.as_raw_fd();
        set_ip_option(fd, IP_PKTINFO, 1)?;

        Ok(Self { socket })
    }

    /// Borrows the configured socket for future readiness registration.
    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }

    /// Transfers this socket into Tokio without duplicating its descriptor.
    /// Tokio requires an active runtime here and panics when no runtime is entered.
    pub fn into_async(self) -> io::Result<AsyncUdpReceiver> {
        self.socket.set_nonblocking(true)?;
        let socket = tokio::net::UdpSocket::from_std(self.socket)?;
        Ok(AsyncUdpReceiver {
            socket,
            #[cfg(test)]
            native_would_block_count: AtomicU32::new(0),
        })
    }

    /// Receives exactly one datagram without allocating or retrying the socket operation.
    pub fn receive(&self, buffer: &mut [u8]) -> io::Result<ReceivedDatagram> {
        receive_one(&self.socket, buffer)
    }
}

impl AsyncUdpReceiver {
    /// Only one receive task may be active, including while Pending.
    /// Writers can run concurrently; Tokio retains a single reader waker.
    pub fn poll_receive(
        &self,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<ReceivedDatagram>> {
        if let Err(error) = validate_buffer(buffer.len()) {
            return Poll::Ready(Err(error));
        }

        loop {
            match self.socket.poll_recv_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {
                    match self.socket.try_io(Interest::READABLE, || {
                        let result = receive_one(&self.socket, buffer);
                        #[cfg(test)]
                        if matches!(result.as_ref(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
                        {
                            self.native_would_block_count.fetch_add(1, Ordering::Relaxed);
                        }
                        result
                    }) {
                        Ok(datagram) => return Poll::Ready(Ok(datagram)),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) => return Poll::Ready(Err(error)),
                    }
                }
            }
        }
    }

    /// Waits until Tokio reports this owned socket writable.
    pub async fn writable(&self) -> io::Result<()> {
        self.socket.writable().await
    }

    /// Sends one datagram without applying peer policy or altering socket configuration.
    pub fn try_send_to(&self, buffer: &[u8], peer: SocketAddrV4) -> io::Result<usize> {
        self.socket.try_send_to(buffer, SocketAddr::V4(peer))
    }

    /// Returns the exact IPv4 address retained from caller setup.
    pub fn local_addr(&self) -> io::Result<SocketAddrV4> {
        match self.socket.local_addr()? {
            SocketAddr::V4(address) => Ok(address),
            SocketAddr::V6(_) => Err(invalid_data()),
        }
    }

    #[cfg(test)]
    fn native_would_block_count(&self) -> u32 {
        self.native_would_block_count.load(Ordering::Relaxed)
    }
}

fn receive_one(socket: &impl AsRawFd, buffer: &mut [u8]) -> io::Result<ReceivedDatagram> {
    validate_buffer(buffer.len())?;
    let fd = socket.as_raw_fd();
    let mut source = SocketAddressStorage([0; SOCKET_ADDRESS_STORAGE_BYTES]);
    let mut control = ControlBuffer([0; MAX_CONTROL_BYTES]);
    let mut iov = Iovec {
        base: buffer.as_mut_ptr().cast(),
        length: buffer.len(),
    };
    let mut message = Msghdr {
        name: source.0.as_mut_ptr().cast(),
        name_length: SOCKET_ADDRESS_STORAGE_BYTES as u32,
        iov: &mut iov,
        iov_length: 1,
        control: control.0.as_mut_ptr().cast(),
        control_length: MAX_CONTROL_BYTES as u32,
        flags: 0,
    };

    // SAFETY: The live owned descriptor, writable iovec, sockaddr storage, and aligned control
    // buffer match the Darwin recvmsg ABI for exactly one nonblocking receive.
    let received = unsafe { recvmsg(fd, &mut message, MSG_DONTWAIT) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    validate_message_flags(message.flags)?;
    let received = usize::try_from(received).map_err(|_| invalid_data())?;
    validate_received_length(received, buffer.len())?;
    let name_length = usize::try_from(message.name_length).map_err(|_| invalid_data())?;
    let source = parse_source(&source.0, name_length)?;
    let control_length = usize::try_from(message.control_length).map_err(|_| invalid_data())?;
    if control_length > MAX_CONTROL_BYTES {
        return Err(invalid_data());
    }
    let metadata = parse_control_messages(&control.0[..control_length])?;

    Ok(ReceivedDatagram {
        length: received,
        source,
        destination: metadata.destination,
        interface_index: metadata.interface_index,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PacketMetadata {
    destination: Ipv4Addr,
    interface_index: u32,
}

fn validate_buffer(length: usize) -> io::Result<()> {
    if length == 0 || length > MAX_BUFFER_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok(())
}

fn validate_received_length(length: usize, buffer_length: usize) -> io::Result<()> {
    if length > buffer_length || length > MAX_IPV4_UDP_PAYLOAD_BYTES {
        return Err(invalid_data());
    }
    Ok(())
}

fn validate_message_flags(flags: c_int) -> io::Result<()> {
    if flags & (MSG_TRUNC | MSG_CTRUNC) != 0 {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(())
}

fn parse_source(bytes: &[u8], reported_length: usize) -> io::Result<SocketAddrV4> {
    if reported_length != size_of::<SockaddrIn>() || bytes.len() < reported_length {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let source = &bytes[..reported_length];
    if source[0] as usize != size_of::<SockaddrIn>() || source[1] != AF_INET {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }

    Ok(SocketAddrV4::new(
        Ipv4Addr::new(source[4], source[5], source[6], source[7]),
        u16::from_be_bytes([source[2], source[3]]),
    ))
}

fn parse_control_messages(control: &[u8]) -> io::Result<PacketMetadata> {
    let mut packet_info = None;
    let mut received_destination = None;
    let mut received_interface = None;
    let mut offset = 0;

    while offset < control.len() {
        if control.len() - offset < CMSG_HEADER_BYTES {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }

        let header = &control[offset..offset + CMSG_HEADER_BYTES];
        let message_length =
            usize::try_from(read_native_u32(&header[..4])?).map_err(|_| invalid_data())?;
        if message_length < CMSG_HEADER_BYTES || message_length > control.len() - offset {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let level = read_native_i32(&header[4..8])?;
        let message_type = read_native_i32(&header[8..12])?;
        let data = &control[offset + CMSG_HEADER_BYTES..offset + message_length];

        if level == IPPROTO_IP {
            match message_type {
                IP_PKTINFO => {
                    if packet_info.is_some() || data.len() != IN_PKTINFO_BYTES {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    let interface_index = read_native_u32(&data[..4])?;
                    if interface_index == 0 {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    let destination = ipv4(&data[8..12]);
                    if destination.is_unspecified() {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    packet_info = Some(PacketMetadata {
                        interface_index,
                        destination,
                    });
                }
                IP_RECVDSTADDR => {
                    if received_destination.is_some() || data.len() != IN_ADDR_BYTES {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    received_destination = Some(ipv4(data));
                }
                IP_RECVIF => {
                    if received_interface.is_some() {
                        return Err(io::Error::from(io::ErrorKind::InvalidData));
                    }
                    received_interface = Some(parse_received_interface(data)?);
                }
                _ => {}
            }
        }

        offset = offset
            .checked_add(cmsg_align(message_length))
            .filter(|next| *next <= control.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    }

    let packet_info = packet_info.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    if received_destination.is_some_and(|destination| destination != packet_info.destination)
        || received_interface.is_some_and(|index| index != packet_info.interface_index)
    {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(packet_info)
}

fn parse_received_interface(data: &[u8]) -> io::Result<u32> {
    const SOCKADDR_DL_HEADER_BYTES: usize = 8;

    if data.len() < SOCKADDR_DL_HEADER_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let length = data[0] as usize;
    if length < SOCKADDR_DL_HEADER_BYTES || length > data.len() || data[1] != AF_LINK {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let interface_index = u16::from_ne_bytes([data[2], data[3]]) as u32;
    if interface_index == 0 {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(interface_index)
}

fn read_native_u32(bytes: &[u8]) -> io::Result<u32> {
    let bytes: [u8; 4] = bytes.try_into().map_err(|_| invalid_data())?;
    Ok(u32::from_ne_bytes(bytes))
}

fn read_native_i32(bytes: &[u8]) -> io::Result<i32> {
    let bytes: [u8; 4] = bytes.try_into().map_err(|_| invalid_data())?;
    Ok(i32::from_ne_bytes(bytes))
}

fn ipv4(bytes: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
}

const fn cmsg_align(length: usize) -> usize {
    (length + CMSG_ALIGNMENT - 1) & !(CMSG_ALIGNMENT - 1)
}

fn invalid_data() -> io::Error {
    io::Error::from(io::ErrorKind::InvalidData)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmsg(message_type: c_int, data: &[u8]) -> Vec<u8> {
        let length = CMSG_HEADER_BYTES + data.len();
        let mut bytes = vec![0; cmsg_align(length)];
        bytes[..4].copy_from_slice(&(length as u32).to_ne_bytes());
        bytes[4..8].copy_from_slice(&IPPROTO_IP.to_ne_bytes());
        bytes[8..12].copy_from_slice(&message_type.to_ne_bytes());
        bytes[CMSG_HEADER_BYTES..length].copy_from_slice(data);
        bytes
    }

    fn packet_info(index: u32, destination: Ipv4Addr) -> Vec<u8> {
        let mut data = vec![0; IN_PKTINFO_BYTES];
        data[..4].copy_from_slice(&index.to_ne_bytes());
        data[8..12].copy_from_slice(&destination.octets());
        cmsg(IP_PKTINFO, &data)
    }

    fn received_interface(index: u16) -> Vec<u8> {
        let mut data = vec![0; 8];
        data[0] = 8;
        data[1] = AF_LINK;
        data[2..4].copy_from_slice(&index.to_ne_bytes());
        cmsg(IP_RECVIF, &data)
    }

    fn valid_control() -> Vec<u8> {
        let destination = Ipv4Addr::new(192, 0, 2, 8);
        let mut control = packet_info(11, destination);
        control.extend(received_interface(11));
        control.extend(cmsg(IP_RECVDSTADDR, &destination.octets()));
        control
    }

    fn assert_invalid<T: std::fmt::Debug>(result: io::Result<T>) {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parses_required_and_consistent_optional_metadata() {
        assert_eq!(
            parse_control_messages(&valid_control()).unwrap(),
            PacketMetadata {
                destination: Ipv4Addr::new(192, 0, 2, 8),
                interface_index: 11,
            }
        );
    }

    #[test]
    fn parses_packet_info_without_optional_metadata() {
        assert_eq!(
            parse_control_messages(&packet_info(11, Ipv4Addr::new(192, 0, 2, 8))).unwrap(),
            PacketMetadata {
                destination: Ipv4Addr::new(192, 0, 2, 8),
                interface_index: 11,
            }
        );
    }

    #[test]
    fn rejects_missing_required_packet_info() {
        assert_invalid(parse_control_messages(&cmsg(
            IP_RECVDSTADDR,
            &Ipv4Addr::new(192, 0, 2, 8).octets(),
        )));
    }

    #[test]
    fn rejects_duplicate_metadata() {
        let mut duplicate_packet_info = packet_info(11, Ipv4Addr::new(192, 0, 2, 8));
        duplicate_packet_info.extend(packet_info(11, Ipv4Addr::new(192, 0, 2, 8)));
        assert_invalid(parse_control_messages(&duplicate_packet_info));

        let mut duplicate_destination = packet_info(11, Ipv4Addr::new(192, 0, 2, 8));
        duplicate_destination.extend(cmsg(IP_RECVDSTADDR, &[192, 0, 2, 8]));
        duplicate_destination.extend(cmsg(IP_RECVDSTADDR, &[192, 0, 2, 8]));
        assert_invalid(parse_control_messages(&duplicate_destination));

        let mut duplicate_interface = packet_info(11, Ipv4Addr::new(192, 0, 2, 8));
        duplicate_interface.extend(received_interface(11));
        duplicate_interface.extend(received_interface(11));
        assert_invalid(parse_control_messages(&duplicate_interface));
    }

    #[test]
    fn rejects_malformed_control_messages() {
        assert_invalid(parse_control_messages(&[0; CMSG_HEADER_BYTES - 1]));

        let mut too_short = packet_info(11, Ipv4Addr::new(192, 0, 2, 8));
        too_short[..4].copy_from_slice(&((CMSG_HEADER_BYTES - 1) as u32).to_ne_bytes());
        assert_invalid(parse_control_messages(&too_short));

        let mut too_long = packet_info(11, Ipv4Addr::new(192, 0, 2, 8));
        let declared_length = (too_long.len() + 4) as u32;
        too_long[..4].copy_from_slice(&declared_length.to_ne_bytes());
        assert_invalid(parse_control_messages(&too_long));

        assert_invalid(parse_control_messages(&cmsg(IP_PKTINFO, &[0; 4])));
        assert_invalid(parse_control_messages(&cmsg(IP_RECVDSTADDR, &[192, 0, 2])));
        assert_invalid(parse_control_messages(&cmsg(IP_RECVIF, &[8, AF_LINK, 1])));
        assert_invalid(parse_control_messages(&cmsg(
            IP_RECVIF,
            &[9, AF_LINK, 1, 0, 0, 0, 0, 0],
        )));
        assert_invalid(parse_control_messages(&cmsg(
            IP_RECVIF,
            &[8, AF_INET, 1, 0, 0, 0, 0, 0],
        )));
    }

    #[test]
    fn rejects_inconsistent_or_zero_interface_metadata() {
        let mut conflicting_destination = packet_info(11, Ipv4Addr::new(192, 0, 2, 8));
        conflicting_destination.extend(cmsg(IP_RECVDSTADDR, &[192, 0, 2, 9]));
        assert_invalid(parse_control_messages(&conflicting_destination));

        let mut conflicting_interface = packet_info(11, Ipv4Addr::new(192, 0, 2, 8));
        conflicting_interface.extend(received_interface(12));
        assert_invalid(parse_control_messages(&conflicting_interface));

        assert_invalid(parse_control_messages(&packet_info(
            0,
            Ipv4Addr::new(192, 0, 2, 8),
        )));
        assert_invalid(parse_control_messages(&packet_info(
            11,
            Ipv4Addr::UNSPECIFIED,
        )));

        let mut zero_received_interface = packet_info(11, Ipv4Addr::new(192, 0, 2, 8));
        zero_received_interface.extend(received_interface(0));
        assert_invalid(parse_control_messages(&zero_received_interface));
    }

    #[test]
    fn rejects_truncated_payload_or_ancillary_data() {
        assert_invalid(validate_message_flags(MSG_TRUNC));
        assert_invalid(validate_message_flags(MSG_CTRUNC));
        assert_invalid(validate_message_flags(MSG_TRUNC | MSG_CTRUNC));
    }

    #[test]
    fn rejects_non_ipv4_or_truncated_sources() {
        let mut source = [0; SOCKET_ADDRESS_STORAGE_BYTES];
        source[0] = size_of::<SockaddrIn>() as u8;
        source[1] = AF_INET;
        source[2..4].copy_from_slice(&443u16.to_be_bytes());
        source[4..8].copy_from_slice(&[192, 0, 2, 99]);
        assert_eq!(
            parse_source(&source, size_of::<SockaddrIn>()).unwrap(),
            SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 99), 443)
        );

        assert_invalid(parse_source(&source, size_of::<SockaddrIn>() - 1));
        source[1] = AF_LINK;
        assert_invalid(parse_source(&source, size_of::<SockaddrIn>()));
    }

    #[test]
    fn accepts_larger_backing_buffers_and_rejects_invalid_lengths() {
        assert_eq!(
            validate_buffer(0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(validate_buffer(MAX_BUFFER_BYTES).is_ok());
        assert_eq!(
            validate_buffer(MAX_BUFFER_BYTES + 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(validate_received_length(MAX_IPV4_UDP_PAYLOAD_BYTES, MAX_BUFFER_BYTES).is_ok());
        assert!(validate_received_length(0, 1).is_ok());
        assert_invalid(validate_received_length(
            MAX_IPV4_UDP_PAYLOAD_BYTES + 1,
            MAX_BUFFER_BYTES,
        ));
        assert_invalid(validate_received_length(
            MAX_BUFFER_BYTES,
            MAX_BUFFER_BYTES - 1,
        ));
    }

    #[test]
    #[ignore = "explicit localhost-only native UDP metadata probe, not physical-network proof"]
    fn native_loopback_metadata_matches_bound_addresses() {
        use std::{
            net::UdpSocket,
            time::{Duration, Instant},
        };
        // SAFETY: the declaration matches the installed net/if.h interface-index lookup ABI.
        unsafe extern "C" {
            fn if_nametoindex(name: *const std::ffi::c_char) -> u32;
        }
        // SAFETY: the static NUL-terminated name identifies only the local loopback interface.
        let loopback_index = unsafe { if_nametoindex(c"lo0".as_ptr()) };
        assert_ne!(loopback_index, 0);
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        crate::network::restrict_udp_interface(&socket, loopback_index).unwrap();
        let receiver = UdpReceiver::configure(socket).unwrap();
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        crate::network::restrict_udp_interface(&sender, loopback_index).unwrap();
        let payload = b"MonHop metadata probe";
        assert_eq!(
            sender
                .send_to(payload, receiver.socket().local_addr().unwrap())
                .unwrap(),
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

    #[test]
    #[ignore = "explicit localhost-only async UDP metadata probe, not physical-network proof"]
    fn native_async_loopback_metadata_wakes_and_rejects_truncation() {
        use std::{future::poll_fn, net::UdpSocket, time::Duration};

        // SAFETY: The declaration matches the installed net/if.h interface-index lookup ABI.
        unsafe extern "C" {
            fn if_nametoindex(name: *const std::ffi::c_char) -> u32;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            // SAFETY: The static NUL-terminated name identifies only the local loopback interface.
            let loopback_index = unsafe { if_nametoindex(c"lo0".as_ptr()) };
            assert_ne!(loopback_index, 0);
            let loopback = Ipv4Addr::new(127, 0, 0, 1);
            let receiver_socket = UdpSocket::bind((loopback, 0)).unwrap();
            crate::network::restrict_udp_interface(&receiver_socket, loopback_index).unwrap();
            let receiver = UdpReceiver::configure(receiver_socket)
                .unwrap()
                .into_async()
                .unwrap();
            let receiver_address = receiver.local_addr().unwrap();
            let sender = UdpSocket::bind((loopback, 0)).unwrap();
            crate::network::restrict_udp_interface(&sender, loopback_index).unwrap();
            sender.set_nonblocking(true).unwrap();
            let sender_address = sender.local_addr().unwrap();
            let sender_ipv4 = match sender_address {
                SocketAddr::V4(address) => address,
                SocketAddr::V6(_) => panic!("loopback sender was not IPv4"),
            };
            let outbound = b"MonHop async writable probe";
            let sent = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    receiver.writable().await?;
                    match receiver.try_send_to(outbound, sender_ipv4) {
                        Ok(sent) => return Ok(sent),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                        Err(error) => return Err(error),
                    }
                }
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(sent, outbound.len());

            let payload = b"MonHop async metadata probe";
            let mut buffer = [0; 256];

            let (pending_signal, pending_wait) = tokio::sync::oneshot::channel();
            let sender_task = tokio::spawn(async move {
                match tokio::time::timeout(Duration::from_secs(1), pending_wait).await {
                    Ok(Ok(())) => sender.send_to(payload, receiver_address),
                    Ok(Err(_)) => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                    Err(_) => Err(io::Error::from(io::ErrorKind::TimedOut)),
                }
            });
            let mut saw_pending = false;
            let mut pending_signal = Some(pending_signal);
            let receive_result = tokio::time::timeout(
                Duration::from_secs(1),
                poll_fn(|cx| match receiver.poll_receive(cx, &mut buffer) {
                    Poll::Pending => {
                        saw_pending = true;
                        if let Some(signal) = pending_signal.take() {
                            let _ = signal.send(());
                        }
                        Poll::Pending
                    }
                    ready => ready,
                }),
            )
            .await;
            let send_result = sender_task.await.unwrap();
            let received = receive_result.unwrap().unwrap();
            let sent = send_result.unwrap();

            assert!(saw_pending);
            assert_eq!(sent, payload.len());
            assert_eq!(received.length, payload.len());
            assert_eq!(&buffer[..received.length], payload);
            assert_eq!(std::net::SocketAddr::V4(received.source), sender_address);
            assert_eq!(received.destination, loopback);
            assert_eq!(received.interface_index, loopback_index);

            let native_would_blocks_before_stale = receiver.native_would_block_count();
            let stale_sender = UdpSocket::bind((loopback, 0)).unwrap();
            crate::network::restrict_udp_interface(&stale_sender, loopback_index).unwrap();
            stale_sender.set_nonblocking(true).unwrap();
            let stale_sender_address = stale_sender.local_addr().unwrap();
            let (stale_signal, stale_wait) = tokio::sync::oneshot::channel();
            let stale_sender_task = tokio::spawn(async move {
                match tokio::time::timeout(Duration::from_secs(1), stale_wait).await {
                    Ok(Ok(())) => stale_sender.send_to(payload, receiver_address),
                    Ok(Err(_)) => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                    Err(_) => Err(io::Error::from(io::ErrorKind::TimedOut)),
                }
            });
            let mut stale_receiver_task = tokio::spawn(async move {
                let mut stale_buffer = [0; 256];
                let mut saw_pending = false;
                let mut stale_signal = Some(stale_signal);
                let receive_result =
                    poll_fn(|cx| match receiver.poll_receive(cx, &mut stale_buffer) {
                        Poll::Pending => {
                            saw_pending = true;
                            if let Some(signal) = stale_signal.take() {
                                let _ = signal.send(());
                            }
                            Poll::Pending
                        }
                        ready => ready,
                    })
                    .await;
                (receiver, receive_result, stale_buffer, saw_pending)
            });
            let stale_receiver_join = match tokio::time::timeout(
                Duration::from_secs(1),
                &mut stale_receiver_task,
            )
            .await
            {
                Ok(join) => join,
                Err(_) => {
                    stale_receiver_task.abort();
                    let _ = stale_receiver_task.await;
                    let _ = stale_sender_task.await;
                    panic!("stale async UDP reader timed out");
                }
            };
            let stale_sender_join = stale_sender_task.await;
            let (receiver, stale_receive_result, stale_buffer, stale_saw_pending) =
                stale_receiver_join.unwrap();
            let stale_send_result = stale_sender_join.unwrap();
            let stale_received = stale_receive_result.unwrap();
            let stale_sent = stale_send_result.unwrap();

            assert!(stale_saw_pending);
            assert_eq!(stale_sent, payload.len());
            assert_eq!(stale_received.length, payload.len());
            assert_eq!(&stale_buffer[..stale_received.length], payload);
            assert_eq!(
                std::net::SocketAddr::V4(stale_received.source),
                stale_sender_address
            );
            assert_eq!(stale_received.destination, loopback);
            assert_eq!(stale_received.interface_index, loopback_index);
            assert!(receiver.native_would_block_count() > native_would_blocks_before_stale);

            let small_sender = UdpSocket::bind((loopback, 0)).unwrap();
            crate::network::restrict_udp_interface(&small_sender, loopback_index).unwrap();
            small_sender.set_nonblocking(true).unwrap();
            assert_eq!(
                small_sender.send_to(payload, receiver_address).unwrap(),
                payload.len()
            );
            let mut too_small = [0; 1];
            let error = tokio::time::timeout(
                Duration::from_secs(1),
                poll_fn(|cx| receiver.poll_receive(cx, &mut too_small)),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        });
    }
}
