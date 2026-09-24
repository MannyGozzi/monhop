//! Read-only macOS adapter and route diagnostics.
//!
//! This module creates no peer endpoint. Route lookup uses the local routing
//! control plane only, and the UDP helper configures an endpoint created later
//! by its caller.

use std::{
    collections::BTreeMap,
    ffi::{CStr, c_char, c_int, c_void},
    io,
    mem::{offset_of, size_of},
    net::{Ipv4Addr, UdpSocket},
    os::fd::{AsRawFd, RawFd},
    ptr,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use crate::cf_owned::{
    CFEqual, CFRelease, CFStringCreateWithCString, CFStringGetCString, CFStringRef, CFTypeRef,
    CfOwned,
};

const AF_INET: u8 = 2;
const AF_LINK: u8 = 18;
const PF_ROUTE: c_int = 17;
const SOCK_RAW: c_int = 3;
const IFF_UP: u32 = 0x1;
const IFF_RUNNING: u32 = 0x40;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_POINTOPOINT: u32 = 0x10;
const RTM_VERSION: u8 = 5;
const RTM_ADD: u8 = 0x1;
const RTM_DELETE: u8 = 0x2;
const RTM_CHANGE: u8 = 0x3;
const RTM_GET: u8 = 0x4;
const RTA_DST: i32 = 0x1;
const RTA_GATEWAY: i32 = 0x2;
const RTA_NETMASK: i32 = 0x4;
const RTA_IFP: i32 = 0x10;
const RTA_IFA: i32 = 0x20;
const RTF_UP: i32 = 0x1;
const RTF_GATEWAY: i32 = 0x2;
const RTF_REJECT: i32 = 0x8;
const RTF_LLINFO: i32 = 0x400;
const RTF_BLACKHOLE: i32 = 0x1000;
const RTF_LOCAL: i32 = 0x20_0000;
const RTF_BROADCAST: i32 = 0x40_0000;
const RTF_MULTICAST: i32 = 0x80_0000;
const RTF_IFSCOPE: i32 = 0x100_0000;
pub(crate) const IPPROTO_IP: c_int = 0;
const IP_RECVIF: c_int = 20;
const IP_BOUND_IF: c_int = 25;
const SOL_SOCKET: c_int = 0xffff;
const SO_NET_SERVICE_TYPE: c_int = 0x1116;
const NET_SERVICE_TYPE_VO: c_int = 4;
const MAX_INTERFACES: usize = 4096;
const MAX_IOKIT_INTERFACES: usize = 256;
const MAX_IOKIT_PARENTS: usize = 32;
const MAX_ROUTE_REPLY: usize = 4096;
const ROUTE_REPLY_TIMEOUT: Duration = Duration::from_secs(1);
const POLLIN: i16 = 0x0001;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const POLLNVAL: i16 = 0x0020;

static NEXT_ROUTE_SEQUENCE: AtomicU32 = AtomicU32::new(1);

#[derive(Clone, PartialEq, Eq)]
pub struct Adapter {
    pub stable_id: String,
    pub name: String,
    pub index: u32,
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub physical: bool,
    pub up: bool,
    pub ethernet: bool,
    pub wifi: bool,
    /// Opaque attachment metadata. Never display the SSID in automatic logs.
    pub attachment: Option<Vec<u8>>,
}

impl Adapter {
    /// Returns the SSID only when this is a live physical Wi-Fi adapter with a valid attachment.
    pub fn wifi_network_name(&self) -> Option<&str> {
        if !self.physical || !self.wifi || !self.up {
            return None;
        }
        crate::wifi_attachment::ssid_from_attachment(self.attachment.as_deref()?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub interface_index: u32,
    pub source: Ipv4Addr,
    /// An unspecified next hop denotes a directly connected route.
    pub next_hop: Ipv4Addr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterfaceType {
    Ethernet,
    WiFi,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InterfaceMetadata {
    kind: InterfaceType,
    stable_id: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct AddressRecord<'a> {
    name: &'a str,
    index: u32,
    address: Ipv4Addr,
    prefix_len: u8,
    flags: u32,
}

#[repr(C)]
struct Sockaddr {
    length: u8,
    family: u8,
    data: [u8; 14],
}

#[repr(C)]
struct IfAddrs {
    next: *mut IfAddrs,
    name: *const c_char,
    flags: u32,
    address: *const Sockaddr,
    netmask: *const Sockaddr,
    destination: *const Sockaddr,
    data: *mut c_void,
}

struct IfAddrsList(*mut IfAddrs);

impl Drop for IfAddrsList {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: getifaddrs allocated this list and this guard releases it once.
            unsafe { freeifaddrs(self.0) };
        }
    }
}

type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type SCNetworkInterfaceRef = *const c_void;
type IoObject = u32;
type IoIterator = u32;
type KernReturn = i32;

// SAFETY: Declarations below match the installed macOS SDK's Darwin,
// CoreFoundation, SystemConfiguration, and IOKit C interfaces.
unsafe extern "C" {
    fn getifaddrs(addresses: *mut *mut IfAddrs) -> c_int;
    fn freeifaddrs(addresses: *mut IfAddrs);
    fn if_nametoindex(name: *const c_char) -> u32;
    fn getpid() -> c_int;
    fn socket(domain: c_int, socket_type: c_int, protocol: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn write(fd: c_int, buffer: *const c_void, length: usize) -> isize;
    fn read(fd: c_int, buffer: *mut c_void, length: usize) -> isize;
    fn poll(fds: *mut PollFd, count: u32, timeout_ms: c_int) -> c_int;
    fn pipe(fds: *mut c_int) -> c_int;
    fn setsockopt(
        fd: c_int,
        level: c_int,
        option: c_int,
        value: *const c_void,
        length: u32,
    ) -> c_int;
}

// SAFETY: CoreFoundation functions only inspect immutable values or release
// objects whose Create/Copy ownership is held by the caller.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> *const c_void;
    fn CFStringGetLength(value: CFStringRef) -> isize;
}

// SAFETY: These SystemConfiguration calls enumerate existing interface metadata
// and do not modify preferences, services, or network state.
#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    static kSCNetworkInterfaceTypeEthernet: CFStringRef;
    static kSCNetworkInterfaceTypeIEEE80211: CFStringRef;
    fn SCNetworkInterfaceCopyAll() -> CFArrayRef;
    fn SCNetworkInterfaceGetBSDName(interface: SCNetworkInterfaceRef) -> CFStringRef;
    fn SCNetworkInterfaceGetHardwareAddressString(interface: SCNetworkInterfaceRef) -> CFStringRef;
    fn SCNetworkInterfaceGetInterfaceType(interface: SCNetworkInterfaceRef) -> CFStringRef;
}

// SAFETY: These IOKit calls inspect registered controller ancestry. Matching
// dictionaries are consumed by IOServiceGetMatchingServices as documented.
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(class_name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(
        main_port: u32,
        matching: CFDictionaryRef,
        existing: *mut IoIterator,
    ) -> KernReturn;
    fn IOIteratorNext(iterator: IoIterator) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> KernReturn;
    fn IOObjectConformsTo(object: IoObject, class_name: *const c_char) -> i32;
    fn IORegistryEntryGetParentEntry(
        entry: IoObject,
        plane: *const c_char,
        parent: *mut IoObject,
    ) -> KernReturn;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFStringRef,
        allocator: *const c_void,
        options: u32,
    ) -> CFTypeRef;
}

/// Enumerates IPv4 addresses together with read-only SystemConfiguration and
/// IOKit hardware metadata. Unknown or virtual interfaces remain nonphysical.
pub fn enumerate_adapters() -> io::Result<Vec<Adapter>> {
    let metadata = system_configuration_metadata()?;
    let mut list = ptr::null_mut();
    // SAFETY: getifaddrs initializes the output pointer on success.
    if unsafe { getifaddrs(&mut list) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if list.is_null() {
        return Err(io::Error::other("macOS returned an empty interface list"));
    }
    let _list = IfAddrsList(list);
    let mut adapters = Vec::new();
    let mut hardware_backing = BTreeMap::new();
    let mut current = list;

    for _ in 0..MAX_INTERFACES {
        if current.is_null() {
            adapters.sort_by_key(|adapter: &Adapter| (adapter.index, adapter.address));
            return Ok(adapters);
        }
        // SAFETY: current is a node owned by the getifaddrs list guard.
        let record = unsafe { &*current };
        current = record.next;
        if record.address.is_null() || record.netmask.is_null() || record.name.is_null() {
            continue;
        }
        // SAFETY: getifaddrs owns this sockaddr; its fixed family byte is readable here.
        if unsafe { (*record.address).family } != AF_INET {
            continue;
        }
        let address_bytes = sockaddr_bytes(record.address)?;
        // SAFETY: getifaddrs supplies a NUL-terminated interface name for this node.
        let name = unsafe { CStr::from_ptr(record.name) }
            .to_str()
            .map_err(|_| io::Error::other("macOS returned a non-UTF-8 interface name"))?;
        if name.is_empty() || name.len() >= 16 {
            return Err(io::Error::other("macOS returned an invalid interface name"));
        }
        // SAFETY: record.name is a live getifaddrs-owned C string during this iteration.
        let index = unsafe { if_nametoindex(record.name) };
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        let address = parse_ipv4_sockaddr(&address_bytes)?;
        let netmask_bytes = sockaddr_bytes(record.netmask)?;
        let prefix_len = prefix_from_netmask(&netmask_bytes)?;
        let backed = match hardware_backing.get(name) {
            Some(backed) => *backed,
            None => {
                let backed = has_hardware_controller(name)?;
                hardware_backing.insert(name.to_owned(), backed);
                backed
            }
        };
        let adapter = adapter_from_record(
            AddressRecord {
                name,
                index,
                address,
                prefix_len,
                flags: record.flags,
            },
            metadata.get(name),
            backed,
        );
        adapters.push(adapter);
    }
    Err(io::Error::other(
        "macOS interface list exceeds the supported bound",
    ))
}

/// Explicit attachment diagnostic. Ordinary enumeration and route lookup do not read Wi-Fi identifiers.
pub fn enumerate_adapters_with_attachment() -> io::Result<Vec<Adapter>> {
    let mut adapters = enumerate_adapters()?;
    populate_wifi_attachments(&mut adapters, crate::wifi_attachment::read_attachment);
    Ok(adapters)
}

fn populate_wifi_attachments(
    adapters: &mut [Adapter],
    mut read_attachment: impl FnMut(&str) -> io::Result<Option<Vec<u8>>>,
) {
    let mut attachments = BTreeMap::new();
    for adapter in adapters {
        if adapter.physical && adapter.wifi && adapter.up {
            adapter.attachment = attachments
                .entry(adapter.name.clone())
                .or_insert_with(|| read_attachment(&adapter.name).ok().flatten())
                .clone();
        }
    }
}

/// Reads the route an `IP_BOUND_IF` socket on `source`'s interface uses, without binding,
/// connecting, or sending to a peer. Scoping matches the pinned sockets, so a VPN that claims the
/// LAN in the global table does not move this route.
pub fn best_route(source: Ipv4Addr, peer: Ipv4Addr) -> io::Result<Route> {
    if source.is_unspecified() || peer.is_unspecified() || peer.is_multicast() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an exact unicast IPv4 source and peer are required",
        ));
    }
    let scope = enumerate_adapters()?
        .into_iter()
        .find(|adapter| adapter.address == source && adapter.physical && adapter.up)
        .map(|adapter| adapter.index)
        .ok_or_else(|| {
            io::Error::other(
                "the requested source is not a live physical Ethernet or Wi-Fi interface",
            )
        })?;
    let route = kernel_route_lookup(peer, scope)?;
    if route.interface_index != scope || route.source != source {
        return Err(io::Error::other(
            "the kernel-selected route does not use the requested interface and source address",
        ));
    }
    Ok(route)
}

/// Restricts a caller-created, exact-address-bound UDP socket to one interface.
///
/// `IP_BOUND_IF` is set directly and has no fallback. `IP_RECVIF` requests
/// arrival metadata, but a future transport must still inspect every datagram;
/// this snapshot API installs no network-change notifications.
pub fn restrict_udp_interface(socket: &UdpSocket, index: u32) -> io::Result<()> {
    if index == 0
        || !matches!(socket.local_addr()?, std::net::SocketAddr::V4(address) if !address.ip().is_unspecified())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an exact IPv4 address and interface are required",
        ));
    }
    let index = c_int::try_from(index).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index exceeds the macOS socket option range",
        )
    })?;
    socket.set_broadcast(false)?;
    set_ip_option(socket.as_raw_fd(), IP_BOUND_IF, index)?;
    set_ip_option(socket.as_raw_fd(), IP_RECVIF, 1)
}

/// Marks the socket as interactive voice, so Wi-Fi queues its datagrams (WMM AC_VO) ahead of bulk
/// traffic; the system decides whether a DSCP mark follows.
pub fn mark_interactive_traffic(socket: &UdpSocket) -> io::Result<()> {
    set_int_option(
        socket.as_raw_fd(),
        SOL_SOCKET,
        SO_NET_SERVICE_TYPE,
        NET_SERVICE_TYPE_VO,
    )
}

fn system_configuration_metadata() -> io::Result<BTreeMap<String, InterfaceMetadata>> {
    // SAFETY: SCNetworkInterfaceCopyAll returns a retained array or null on failure.
    let interfaces = CfOwned(unsafe { SCNetworkInterfaceCopyAll() });
    if interfaces.0.is_null() {
        return Err(io::Error::other(
            "SystemConfiguration could not enumerate network interfaces",
        ));
    }
    let count = cf_array_count(interfaces.0, MAX_IOKIT_INTERFACES)?;
    let mut metadata = BTreeMap::new();
    for index in 0..count {
        // SAFETY: index is within the checked Core Foundation array bounds.
        let interface = unsafe { CFArrayGetValueAtIndex(interfaces.0, index as isize) };
        if interface.is_null() {
            continue;
        }
        // SAFETY: the array owns a valid SCNetworkInterfaceRef at this index.
        let name = unsafe { SCNetworkInterfaceGetBSDName(interface) };
        // SAFETY: the array owns a valid SCNetworkInterfaceRef at this index.
        let interface_type = unsafe { SCNetworkInterfaceGetInterfaceType(interface) };
        let kind = interface_kind(interface_type);
        if name.is_null() || kind == InterfaceType::Other {
            continue;
        }
        let name = cf_string(name, 15)?;
        if name.is_empty() {
            continue;
        }
        // SAFETY: the array owns a valid SCNetworkInterfaceRef at this index.
        let hardware = unsafe { SCNetworkInterfaceGetHardwareAddressString(interface) };
        let stable_id = if hardware.is_null() {
            None
        } else {
            normalize_hardware_address(&cf_string(hardware, 17)?)
        };
        let candidate = InterfaceMetadata { kind, stable_id };
        match metadata.get(&name) {
            Some(existing) if existing != &candidate => {
                metadata.insert(
                    name,
                    InterfaceMetadata {
                        kind: InterfaceType::Other,
                        stable_id: None,
                    },
                );
            }
            Some(_) => {}
            None => {
                metadata.insert(name, candidate);
            }
        }
    }
    Ok(metadata)
}

fn interface_kind(interface_type: CFStringRef) -> InterfaceType {
    if interface_type.is_null() {
        return InterfaceType::Other;
    }
    // SAFETY: all values are Core Foundation strings owned by SystemConfiguration.
    unsafe {
        if CFEqual(interface_type, kSCNetworkInterfaceTypeEthernet) != 0 {
            InterfaceType::Ethernet
        } else if CFEqual(interface_type, kSCNetworkInterfaceTypeIEEE80211) != 0 {
            InterfaceType::WiFi
        } else {
            InterfaceType::Other
        }
    }
}

fn has_hardware_controller(name: &str) -> io::Result<bool> {
    let name = c_string(name)?;
    let key = c_string("BSD Name")?;
    // SAFETY: the C strings are NUL-terminated and each returned value is owned on success.
    let name = unsafe { CFStringCreateWithCString(ptr::null(), name.as_ptr(), 0x0800_0100) };
    if name.is_null() {
        return Err(io::Error::other(
            "CoreFoundation could not allocate an interface name",
        ));
    }
    // SAFETY: key remains valid for the synchronous Core Foundation conversion.
    let key = unsafe { CFStringCreateWithCString(ptr::null(), key.as_ptr(), 0x0800_0100) };
    if key.is_null() {
        // SAFETY: name is owned by this function after a successful Create call.
        unsafe { CFRelease(name) };
        return Err(io::Error::other(
            "CoreFoundation could not allocate a registry key",
        ));
    }
    // SAFETY: the class name is static and NUL-terminated; the matching dictionary is
    // consumed by IOServiceGetMatchingServices below regardless of its result.
    let matching = unsafe { IOServiceMatching(c"IONetworkInterface".as_ptr()) };
    if matching.is_null() {
        // SAFETY: both values are owned by this function after successful Create calls.
        unsafe {
            CFRelease(key);
            CFRelease(name);
        };
        return Err(io::Error::other(
            "IOKit could not create a network interface match",
        ));
    }
    let mut iterator = 0;
    // SAFETY: IOKit consumes matching and initializes iterator on success.
    let result = unsafe { IOServiceGetMatchingServices(0, matching, &mut iterator) };
    if result != 0 {
        // SAFETY: both values are still owned by this function.
        unsafe {
            CFRelease(key);
            CFRelease(name);
        };
        return Err(io::Error::from_raw_os_error(result));
    }
    if iterator == 0 {
        // SAFETY: both values are still owned by this function.
        unsafe {
            CFRelease(key);
            CFRelease(name);
        };
        return Ok(false);
    }

    let mut found = false;
    let mut exhausted = false;
    for _ in 0..MAX_IOKIT_INTERFACES {
        // SAFETY: iterator is a live IOKit iterator until released below.
        let service = unsafe { IOIteratorNext(iterator) };
        if service == 0 {
            exhausted = true;
            break;
        }
        // SAFETY: the service is valid and key remains alive for the synchronous query.
        let bsd_name = unsafe { IORegistryEntryCreateCFProperty(service, key, ptr::null(), 0) };
        let matches = !bsd_name.is_null()
            // SAFETY: both values are valid Core Foundation strings for this comparison.
            && unsafe { CFEqual(bsd_name, name) != 0 };
        if !bsd_name.is_null() {
            // SAFETY: bsd_name is owned by this function after CreateCFProperty.
            unsafe { CFRelease(bsd_name) };
        }
        if matches {
            found = has_controller_ancestor(service);
        }
        // SAFETY: every object returned by IOIteratorNext must be released once.
        unsafe { IOObjectRelease(service) };
        if matches {
            exhausted = true;
            break;
        }
    }
    // SAFETY: iterator and key are owned resources after successful enumeration setup.
    unsafe {
        IOObjectRelease(iterator);
        CFRelease(key);
        CFRelease(name);
    }
    if !found && !exhausted {
        return Err(io::Error::other(
            "IOKit interface enumeration exceeds the supported bound",
        ));
    }
    Ok(found)
}

fn has_controller_ancestor(entry: IoObject) -> bool {
    let mut current = entry;
    for depth in 0..MAX_IOKIT_PARENTS {
        // SAFETY: current is either the live matching service or an owned parent entry.
        let is_controller =
            unsafe { IOObjectConformsTo(current, c"IONetworkController".as_ptr()) != 0 };
        if is_controller {
            if depth > 0 {
                // SAFETY: parent entries are owned by this traversal after successful lookup.
                unsafe { IOObjectRelease(current) };
            }
            return true;
        }
        let mut parent = 0;
        // SAFETY: the service plane is a static NUL-terminated string and parent is writable.
        let result =
            unsafe { IORegistryEntryGetParentEntry(current, c"IOService".as_ptr(), &mut parent) };
        if depth > 0 {
            // SAFETY: parent entries are owned by this traversal after successful lookup.
            unsafe { IOObjectRelease(current) };
        }
        if result != 0 || parent == 0 {
            return false;
        }
        current = parent;
    }
    // SAFETY: the final parent remains owned after reaching the traversal bound.
    unsafe { IOObjectRelease(current) };
    false
}

fn adapter_from_record(
    record: AddressRecord<'_>,
    metadata: Option<&InterfaceMetadata>,
    hardware_backed: bool,
) -> Adapter {
    let kind = metadata.map_or(InterfaceType::Other, |metadata| metadata.kind);
    let stable_id = metadata.and_then(|metadata| metadata.stable_id.clone());
    let up = record.flags & (IFF_UP | IFF_RUNNING) == (IFF_UP | IFF_RUNNING);
    let physical = hardware_backed
        && stable_id.is_some()
        && matches!(kind, InterfaceType::Ethernet | InterfaceType::WiFi)
        && record.flags & (IFF_LOOPBACK | IFF_POINTOPOINT) == 0;
    Adapter {
        stable_id: stable_id.unwrap_or_default(),
        name: record.name.to_owned(),
        index: record.index,
        address: record.address,
        prefix_len: record.prefix_len,
        physical,
        up,
        ethernet: physical && kind == InterfaceType::Ethernet,
        wifi: physical && kind == InterfaceType::WiFi,
        // DHCP address/router data can be reused on another network. Without an
        // authenticated attachment identifier, returning None is the safe result.
        attachment: None,
    }
}

fn kernel_route_lookup(peer: Ipv4Addr, scope: u32) -> io::Result<Route> {
    // SAFETY: this opens the local routing-control plane, never a peer data endpoint.
    let fd = unsafe { socket(PF_ROUTE, SOCK_RAW, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = RouteSocket(fd);
    let sequence = NEXT_ROUTE_SEQUENCE.fetch_add(1, Ordering::Relaxed) as i32;
    // SAFETY: zero initializes a C route header whose fields are then set explicitly.
    let mut header: RtMsgHdr = unsafe { std::mem::zeroed() };
    header.message_len = u16::try_from(size_of::<RouteRequest>())
        .map_err(|_| io::Error::other("route request exceeds the macOS message bound"))?;
    header.version = RTM_VERSION;
    header.message_type = RTM_GET;
    // RTF_IFSCOPE with the index asks for the route an IP_BOUND_IF socket on that interface uses.
    header.flags = RTF_IFSCOPE;
    header.interface_index = u16::try_from(scope).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface index exceeds the route header",
        )
    })?;
    // XNU emits the selected IPv4 interface address only when RTM_GET asks for interface metadata.
    header.addresses = RTA_DST | RTA_IFP | RTA_IFA;
    // SAFETY: getpid has no input and returns this process identifier synchronously.
    let pid = unsafe { getpid() };
    header.pid = pid;
    header.sequence = sequence;
    let request = RouteRequest {
        header,
        destination: SockaddrIn::for_ipv4(peer),
        interface: SockaddrDl::query_placeholder(),
        interface_address: SockaddrIn::for_ipv4(Ipv4Addr::UNSPECIFIED),
    };
    // SAFETY: request is a fully initialized C-compatible routing message.
    let written = unsafe {
        write(
            socket.0,
            (&raw const request).cast(),
            size_of::<RouteRequest>(),
        )
    };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    if usize::try_from(written).ok() != Some(size_of::<RouteRequest>()) {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "macOS accepted only part of the route lookup request",
        ));
    }

    let deadline = Instant::now() + ROUTE_REPLY_TIMEOUT;
    let mut reply = [0_u8; MAX_ROUTE_REPLY];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "macOS did not return the route lookup before the diagnostic deadline",
            ));
        }
        let timeout = remaining.as_millis().min(c_int::MAX as u128) as c_int;
        let mut pollfd = PollFd {
            fd: socket.0,
            events: POLLIN,
            revents: 0,
        };
        // SAFETY: pollfd points to initialized writable storage for one local descriptor.
        let available = unsafe { poll(&mut pollfd, 1, timeout) };
        if available < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if available == 0 {
            continue;
        }
        if pollfd.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
            return Err(io::Error::other(
                "macOS routing-control socket became unavailable during lookup",
            ));
        }
        if pollfd.revents & POLLIN == 0 {
            return Err(io::Error::other(
                "macOS routing-control socket returned an unsupported poll event",
            ));
        }
        // SAFETY: reply is writable and the routing socket is readable after poll.
        let length = unsafe { read(socket.0, reply.as_mut_ptr().cast(), reply.len()) };
        if length < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let length = usize::try_from(length)
            .map_err(|_| io::Error::other("macOS returned a negative route reply length"))?;
        let Some(route) = parse_route_reply(&reply[..length], pid, sequence, peer)? else {
            continue;
        };
        return Ok(route);
    }
}

/// A kernel route message that can change how the pinned socket reaches its peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteChange {
    Added,
    Deleted,
    Changed,
}

/// Streams IPv4 kernel route changes; the dynamic store never carries them. Read-only.
pub struct RouteObserver {
    socket: RouteSocket,
    wake: [c_int; 2],
}

impl RouteObserver {
    pub fn open() -> io::Result<Self> {
        // SAFETY: this opens the local routing-control plane for IPv4 messages only.
        let fd = unsafe { socket(PF_ROUTE, SOCK_RAW, c_int::from(AF_INET)) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = RouteSocket(fd);
        let mut wake = [-1; 2];
        // SAFETY: wake has room for the two descriptors pipe writes on success.
        if unsafe { pipe(wake.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { socket, wake })
    }

    /// Blocks until a route on `interface_index` is added, deleted, or changed. `None` means
    /// `stop` was called; an error means the socket is no longer trustworthy.
    pub fn next(&self, interface_index: u32) -> io::Result<Option<RouteChange>> {
        let mut message = [0_u8; MAX_ROUTE_REPLY];
        loop {
            let mut fds = [
                PollFd {
                    fd: self.socket.0,
                    events: POLLIN,
                    revents: 0,
                },
                PollFd {
                    fd: self.wake[0],
                    events: POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: fds points to two initialized entries for descriptors this observer owns.
            let ready = unsafe { poll(fds.as_mut_ptr(), 2, -1) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if fds[1].revents != 0 {
                return Ok(None);
            }
            if fds[0].revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
                return Err(io::Error::other(
                    "macOS routing-control socket became unavailable while watching",
                ));
            }
            if fds[0].revents & POLLIN == 0 {
                continue;
            }
            // SAFETY: message is writable and the routing socket is readable after poll.
            let length = unsafe { read(self.socket.0, message.as_mut_ptr().cast(), message.len()) };
            if length < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let length = usize::try_from(length)
                .map_err(|_| io::Error::other("macOS returned a negative route message length"))?;
            if let Some(change) = relevant_route_change(&message[..length], interface_index) {
                return Ok(Some(change));
            }
        }
    }

    /// Wakes a blocked `next` so it returns `None`. Safe from any thread.
    pub fn stop(&self) {
        let byte = 1_u8;
        // SAFETY: the pipe write end stays open until this observer drops.
        unsafe { write(self.wake[1], (&raw const byte).cast(), 1) };
    }
}

impl Drop for RouteObserver {
    fn drop(&mut self) {
        for fd in self.wake {
            // SAFETY: pipe returned both descriptors and each is closed once here.
            unsafe { close(fd) };
        }
    }
}

/// Only routes on the selected interface count, and link-layer (ARP) entries never do.
fn relevant_route_change(message: &[u8], interface_index: u32) -> Option<RouteChange> {
    if *message.get(offset_of!(RtMsgHdr, version))? != RTM_VERSION {
        return None;
    }
    let change = match *message.get(offset_of!(RtMsgHdr, message_type))? {
        RTM_ADD => RouteChange::Added,
        RTM_DELETE => RouteChange::Deleted,
        RTM_CHANGE => RouteChange::Changed,
        _ => return None,
    };
    let index = read_u16(message, offset_of!(RtMsgHdr, interface_index)).ok()?;
    let flags = read_i32(message, offset_of!(RtMsgHdr, flags)).ok()?;
    if u32::from(index) != interface_index || flags & RTF_LLINFO != 0 {
        return None;
    }
    Some(change)
}

struct RouteSocket(c_int);

impl Drop for RouteSocket {
    fn drop(&mut self) {
        // SAFETY: this guard owns the routing socket descriptor and closes it once.
        unsafe { close(self.0) };
    }
}

#[repr(C)]
struct RtMsgHdr {
    message_len: u16,
    version: u8,
    message_type: u8,
    interface_index: u16,
    flags: i32,
    addresses: i32,
    pid: i32,
    sequence: i32,
    error: i32,
    use_count: i32,
    inits: u32,
    metrics: [u32; 14],
}

#[repr(C)]
struct SockaddrIn {
    length: u8,
    family: u8,
    port: u16,
    address: [u8; 4],
    zero: [u8; 8],
}

#[repr(C)]
struct SockaddrDl {
    length: u8,
    family: u8,
    index: u16,
    interface_type: u8,
    name_len: u8,
    address_len: u8,
    selector_len: u8,
}

impl SockaddrDl {
    fn query_placeholder() -> Self {
        Self {
            length: size_of::<Self>() as u8,
            family: AF_LINK,
            index: 0,
            interface_type: 0,
            name_len: 0,
            address_len: 0,
            selector_len: 0,
        }
    }
}

impl SockaddrIn {
    fn for_ipv4(address: Ipv4Addr) -> Self {
        Self {
            length: size_of::<SockaddrIn>() as u8,
            family: AF_INET,
            port: 0,
            address: address.octets(),
            zero: [0; 8],
        }
    }
}

#[repr(C)]
struct RouteRequest {
    header: RtMsgHdr,
    destination: SockaddrIn,
    interface: SockaddrDl,
    interface_address: SockaddrIn,
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

fn parse_route_reply(
    message: &[u8],
    expected_pid: i32,
    expected_sequence: i32,
    peer: Ipv4Addr,
) -> io::Result<Option<Route>> {
    if message.len() < size_of::<RtMsgHdr>() {
        return Err(io::Error::other("route reply is shorter than its header"));
    }
    let message_len = read_u16(message, offset_of!(RtMsgHdr, message_len))? as usize;
    if message_len != message.len() || message_len < size_of::<RtMsgHdr>() {
        return Err(io::Error::other(
            "route reply has an invalid message length",
        ));
    }
    let pid = read_i32(message, offset_of!(RtMsgHdr, pid))?;
    let sequence = read_i32(message, offset_of!(RtMsgHdr, sequence))?;
    if pid != expected_pid || sequence != expected_sequence {
        return Ok(None);
    }
    if message[offset_of!(RtMsgHdr, version)] != RTM_VERSION
        || message[offset_of!(RtMsgHdr, message_type)] != RTM_GET
    {
        return Err(io::Error::other(
            "macOS returned an unexpected route reply type",
        ));
    }
    let error = read_i32(message, offset_of!(RtMsgHdr, error))?;
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    let interface_index = u32::from(read_u16(message, offset_of!(RtMsgHdr, interface_index))?);
    let flags = read_i32(message, offset_of!(RtMsgHdr, flags))?;
    let addresses = read_i32(message, offset_of!(RtMsgHdr, addresses))?;
    if interface_index == 0
        || flags & RTF_UP == 0
        || flags & (RTF_REJECT | RTF_BLACKHOLE | RTF_LOCAL | RTF_BROADCAST | RTF_MULTICAST) != 0
    {
        return Err(io::Error::other("macOS returned an ineligible route"));
    }
    let addrs = parse_route_addresses(message, addresses)?;
    let destination = addrs
        .destination
        .ok_or_else(|| io::Error::other("macOS route reply omitted its IPv4 destination"))?;
    if !match addrs.prefix_len {
        Some(prefix_len) => ipv4_prefix_contains(destination, prefix_len, peer),
        None => destination == peer,
    } {
        return Err(io::Error::other(
            "macOS route reply destination does not cover the requested IPv4 peer",
        ));
    }
    let source = addrs
        .interface_address
        .ok_or_else(|| io::Error::other("macOS route reply omitted its IPv4 source"))?;
    let next_hop = if flags & RTF_GATEWAY == 0 {
        Ipv4Addr::UNSPECIFIED
    } else {
        addrs
            .gateway
            .ok_or_else(|| io::Error::other("macOS route reply omitted its IPv4 gateway"))?
    };
    Ok(Some(Route {
        interface_index,
        source,
        next_hop,
    }))
}

#[derive(Default)]
struct RouteAddresses {
    destination: Option<Ipv4Addr>,
    gateway: Option<Ipv4Addr>,
    prefix_len: Option<u8>,
    interface_address: Option<Ipv4Addr>,
}

fn parse_route_addresses(message: &[u8], addresses: i32) -> io::Result<RouteAddresses> {
    let mut result = RouteAddresses::default();
    let mut offset = size_of::<RtMsgHdr>();
    for slot in 0..8 {
        if addresses & (1 << slot) == 0 {
            continue;
        }
        let length = *message
            .get(offset)
            .ok_or_else(|| io::Error::other("route reply ends inside an address"))?
            as usize;
        if length == 0 && 1 << slot == RTA_NETMASK {
            result.prefix_len = Some(0);
            offset = offset
                .checked_add(route_roundup(length))
                .filter(|next| *next <= message.len())
                .ok_or_else(|| {
                    io::Error::other("route reply netmask alignment exceeds message bounds")
                })?;
            continue;
        }
        if length < 2 {
            return Err(io::Error::other(
                "route reply contains a truncated socket address",
            ));
        }
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= message.len())
            .ok_or_else(|| io::Error::other("route reply address exceeds message bounds"))?;
        let address = &message[offset..end];
        match 1 << slot {
            RTA_DST => result.destination = Some(parse_ipv4_sockaddr(address)?),
            RTA_GATEWAY if address[1] == AF_INET => {
                result.gateway = Some(parse_ipv4_sockaddr(address)?)
            }
            RTA_GATEWAY if address[1] == AF_LINK => {}
            RTA_GATEWAY => {
                return Err(io::Error::other(
                    "route reply has an unsupported gateway family",
                ));
            }
            RTA_NETMASK => result.prefix_len = Some(prefix_from_netmask(address)?),
            RTA_IFA => result.interface_address = Some(parse_ipv4_sockaddr(address)?),
            _ => {}
        }
        offset = offset
            .checked_add(route_roundup(length))
            .filter(|next| *next <= message.len())
            .ok_or_else(|| {
                io::Error::other("route reply address alignment exceeds message bounds")
            })?;
    }
    if offset != message.len() {
        return Err(io::Error::other(
            "route reply has trailing or unparsed bytes",
        ));
    }
    Ok(result)
}

fn sockaddr_bytes(address: *const Sockaddr) -> io::Result<Vec<u8>> {
    if address.is_null() {
        return Err(io::Error::other("macOS returned a missing socket address"));
    }
    // SAFETY: callers pass a live getifaddrs sockaddr; only the ABI length byte is read first.
    let length = unsafe { (*address).length } as usize;
    if !(2..=size_of::<Sockaddr>()).contains(&length) {
        return Err(io::Error::other(
            "macOS returned an invalid socket address length",
        ));
    }
    // SAFETY: getifaddrs owns a sockaddr with the checked length until the list guard drops.
    Ok(unsafe { std::slice::from_raw_parts(address.cast(), length) }.to_vec())
}

fn parse_ipv4_sockaddr(address: &[u8]) -> io::Result<Ipv4Addr> {
    if address.len() < 8 || address[0] as usize != address.len() || address[1] != AF_INET {
        return Err(io::Error::other("an IPv4 socket address is malformed"));
    }
    Ok(Ipv4Addr::new(
        address[4], address[5], address[6], address[7],
    ))
}

fn prefix_from_netmask(address: &[u8]) -> io::Result<u8> {
    if address.len() < 4 || address[0] as usize != address.len() {
        return Err(io::Error::other("an IPv4 netmask is malformed"));
    }
    // Darwin may compact a netmask sockaddr by omitting trailing zero octets.
    let mut octets = [0_u8; 4];
    let available = address.len().saturating_sub(4).min(octets.len());
    octets[..available].copy_from_slice(&address[4..4 + available]);
    let mask = u32::from_be_bytes(octets);
    let prefix = mask.leading_ones();
    let canonical = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    if mask != canonical {
        return Err(io::Error::other(
            "macOS returned a non-contiguous IPv4 netmask",
        ));
    }
    Ok(prefix as u8)
}

fn ipv4_prefix_contains(network: Ipv4Addr, prefix_len: u8, address: Ipv4Addr) -> bool {
    if prefix_len > 32 {
        return false;
    }
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    u32::from(network) & mask == u32::from(address) & mask
}

fn normalize_hardware_address(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 17 {
        return None;
    }
    let mut normalized = String::with_capacity(16);
    normalized.push_str("mac:");
    for pair in 0..6 {
        if pair > 0 && bytes[pair * 3 - 1] != b':' {
            return None;
        }
        let high = hex_value(bytes[pair * 3])?;
        let low = hex_value(bytes[pair * 3 + 1])?;
        normalized.push(char::from(b"0123456789abcdef"[high as usize]));
        normalized.push(char::from(b"0123456789abcdef"[low as usize]));
    }
    Some(normalized)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn c_string(value: &str) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in native API string"))
}

fn cf_array_count(array: CFArrayRef, limit: usize) -> io::Result<usize> {
    // SAFETY: array is a live Core Foundation array returned by CopyAll.
    let count = unsafe { CFArrayGetCount(array) };
    let count = usize::try_from(count)
        .map_err(|_| io::Error::other("Core Foundation returned a negative array count"))?;
    if count > limit {
        return Err(io::Error::other(
            "Core Foundation array exceeds the supported bound",
        ));
    }
    Ok(count)
}

fn cf_string(value: CFStringRef, limit: usize) -> io::Result<String> {
    if value.is_null() {
        return Err(io::Error::other(
            "Core Foundation returned a missing string",
        ));
    }
    // SAFETY: value is a valid Core Foundation string from a documented API.
    let chars = unsafe { CFStringGetLength(value) };
    let chars = usize::try_from(chars)
        .map_err(|_| io::Error::other("Core Foundation returned a negative string length"))?;
    if chars > limit {
        return Err(io::Error::other(
            "Core Foundation string exceeds the supported bound",
        ));
    }
    let capacity = chars
        .checked_mul(4)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| io::Error::other("Core Foundation string capacity overflow"))?;
    let mut bytes = vec![0_u8; capacity];
    // SAFETY: bytes has the checked capacity and CFStringGetCString writes a terminated UTF-8 string.
    let converted = unsafe {
        CFStringGetCString(
            value,
            bytes.as_mut_ptr().cast(),
            bytes.len() as isize,
            0x0800_0100,
        )
    };
    if converted == 0 {
        return Err(io::Error::other(
            "Core Foundation could not encode a UTF-8 string",
        ));
    }
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| io::Error::other("Core Foundation returned an unterminated UTF-8 string"))?;
    String::from_utf8(bytes[..length].to_vec())
        .map_err(|_| io::Error::other("Core Foundation returned invalid UTF-8"))
}

fn read_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    let value: [u8; 2] = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| io::Error::other("route reply ends inside a u16 field"))?
        .try_into()
        .map_err(|_| io::Error::other("route reply has an invalid u16 field"))?;
    Ok(u16::from_ne_bytes(value))
}

fn read_i32(bytes: &[u8], offset: usize) -> io::Result<i32> {
    let value: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| io::Error::other("route reply ends inside an i32 field"))?
        .try_into()
        .map_err(|_| io::Error::other("route reply has an invalid i32 field"))?;
    Ok(i32::from_ne_bytes(value))
}

fn route_roundup(length: usize) -> usize {
    let align = size_of::<u32>();
    if length == 0 {
        align
    } else {
        (length + align - 1) & !(align - 1)
    }
}

pub(crate) fn set_ip_option(fd: RawFd, option: c_int, value: c_int) -> io::Result<()> {
    set_int_option(fd, IPPROTO_IP, option, value)
}

fn set_int_option(fd: RawFd, level: c_int, option: c_int, value: c_int) -> io::Result<()> {
    // SAFETY: fd is a live UDP socket, and the value pointer and length match Darwin's int option ABI.
    let result = unsafe {
        setsockopt(
            fd,
            level,
            option,
            (&raw const value).cast(),
            u32::try_from(size_of::<c_int>()).expect("c_int size fits socklen_t"),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // SAFETY: getsockopt writes at most `length` bytes into the caller's buffer.
    unsafe extern "C" {
        fn getsockopt(
            fd: c_int,
            level: c_int,
            option: c_int,
            value: *mut c_void,
            length: *mut u32,
        ) -> c_int;
    }

    #[test]
    fn a_marked_socket_reads_back_as_interactive_voice() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        mark_interactive_traffic(&socket).unwrap();
        let mut value: c_int = -1;
        let mut length = u32::try_from(size_of::<c_int>()).unwrap();
        // SAFETY: the socket is live and the buffer and its length describe one c_int.
        let read = unsafe {
            getsockopt(
                socket.as_raw_fd(),
                SOL_SOCKET,
                SO_NET_SERVICE_TYPE,
                (&raw mut value).cast(),
                &mut length,
            )
        };
        assert_eq!(read, 0);
        assert_eq!(value, NET_SERVICE_TYPE_VO);
    }

    fn metadata(kind: InterfaceType) -> InterfaceMetadata {
        InterfaceMetadata {
            kind,
            stable_id: Some("mac:001122334455".into()),
        }
    }

    fn record() -> AddressRecord<'static> {
        AddressRecord {
            name: "en0",
            index: 4,
            address: Ipv4Addr::new(192, 168, 1, 10),
            prefix_len: 24,
            flags: IFF_UP | IFF_RUNNING,
        }
    }

    #[test]
    fn rejects_malformed_and_non_contiguous_netmasks() {
        assert_eq!(
            prefix_from_netmask(&[16, AF_INET, 0, 0, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                .unwrap(),
            24
        );
        assert_eq!(prefix_from_netmask(&[5, 0, 0, 0, 255]).unwrap(), 8);
        for value in [vec![3, AF_INET, 0], vec![8, AF_INET, 0, 0, 255, 0, 255, 0]] {
            assert!(prefix_from_netmask(&value).is_err());
        }
    }

    #[test]
    fn virtual_or_ambiguous_metadata_cannot_be_hardware() {
        let virtual_adapter =
            adapter_from_record(record(), Some(&metadata(InterfaceType::Other)), true);
        assert!(!virtual_adapter.physical);
        assert!(!virtual_adapter.ethernet);
        assert!(!virtual_adapter.wifi);

        let mut missing_identity = metadata(InterfaceType::Ethernet);
        missing_identity.stable_id = None;
        assert!(!adapter_from_record(record(), Some(&missing_identity), true).physical);
    }

    #[test]
    fn adapter_records_do_not_invent_attachment_identity() {
        let adapter = adapter_from_record(record(), Some(&metadata(InterfaceType::WiFi)), true);
        assert!(adapter.wifi);
        assert_eq!(adapter.attachment, None);
    }

    #[test]
    fn attachment_query_is_once_per_live_physical_wifi_interface() {
        let wifi = adapter_from_record(record(), Some(&metadata(InterfaceType::WiFi)), true);
        let ethernet =
            adapter_from_record(record(), Some(&metadata(InterfaceType::Ethernet)), true);
        let mut down = wifi.clone();
        down.up = false;
        let mut virtual_wifi = wifi.clone();
        virtual_wifi.physical = false;
        let mut adapters = vec![wifi.clone(), wifi, ethernet, down, virtual_wifi];
        let mut queries = 0;
        populate_wifi_attachments(&mut adapters, |_| {
            queries += 1;
            Ok(Some(vec![7]))
        });
        assert_eq!(queries, 1);
        assert!(
            adapters[..2]
                .iter()
                .all(|adapter| adapter.attachment == Some(vec![7]))
        );
        assert!(
            adapters[2..]
                .iter()
                .all(|adapter| adapter.attachment.is_none())
        );
    }

    #[test]
    fn attachment_query_failure_stays_unknown_without_losing_other_adapters() {
        let mut adapters = vec![
            adapter_from_record(record(), Some(&metadata(InterfaceType::WiFi)), true),
            adapter_from_record(record(), Some(&metadata(InterfaceType::Ethernet)), true),
        ];
        populate_wifi_attachments(&mut adapters, |_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert_eq!(adapters.len(), 2);
        assert!(adapters.iter().all(|adapter| adapter.attachment.is_none()));
    }

    #[test]
    fn wifi_network_name_requires_live_physical_wifi_with_an_attachment() {
        let mut adapter = adapter_from_record(record(), Some(&metadata(InterfaceType::WiFi)), true);
        adapter.attachment = Some(test_wifi_attachment(b"studio"));
        assert_eq!(adapter.wifi_network_name(), Some("studio"));

        for ineligible in [
            {
                let mut value = adapter.clone();
                value.physical = false;
                value
            },
            {
                let mut value = adapter.clone();
                value.wifi = false;
                value
            },
            {
                let mut value = adapter.clone();
                value.up = false;
                value
            },
            {
                let mut value = adapter;
                value.attachment = None;
                value
            },
        ] {
            assert_eq!(ineligible.wifi_network_name(), None);
        }
    }

    #[test]
    fn only_canonical_hardware_addresses_become_stable_ids() {
        assert_eq!(
            normalize_hardware_address("00:11:22:aa:BB:ff"),
            Some("mac:001122aabbff".into())
        );
        for address in ["00-11-22-33-44-55", "00:11:22:33:44", "00:11:22:33:44:gg"] {
            assert_eq!(normalize_hardware_address(address), None);
        }
    }

    #[test]
    fn routing_request_matches_the_darwin_header_alignment() {
        assert_eq!(size_of::<RtMsgHdr>(), 92);
        assert_eq!(offset_of!(RtMsgHdr, flags), 8);
        assert_eq!(offset_of!(RtMsgHdr, pid), 16);
        assert_eq!(size_of::<SockaddrIn>(), 16);
        assert_eq!(size_of::<SockaddrDl>(), 8);
        assert_eq!(size_of::<RouteRequest>(), 132);
    }

    #[test]
    fn route_parser_accepts_kernel_network_destination_and_scoped_direct_route() {
        let peer = Ipv4Addr::new(192, 168, 1, 20);
        let message = route_reply(
            17,
            9,
            Ipv4Addr::new(192, 168, 1, 0),
            Some(24),
            Ipv4Addr::new(192, 168, 1, 10),
            None,
            RTF_UP | RTF_IFSCOPE,
        );
        assert_eq!(
            parse_route_reply(&message, 17, 9, peer).unwrap(),
            Some(Route {
                interface_index: 4,
                source: Ipv4Addr::new(192, 168, 1, 10),
                next_hop: Ipv4Addr::UNSPECIFIED,
            })
        );
    }

    #[test]
    fn route_parser_retains_kernel_gateway_and_rejects_uncovered_peers() {
        let peer = Ipv4Addr::new(192, 168, 1, 20);
        let message = route_reply(
            17,
            9,
            Ipv4Addr::new(192, 168, 1, 0),
            Some(24),
            Ipv4Addr::new(192, 168, 1, 10),
            Some(Ipv4Addr::new(192, 168, 1, 1)),
            RTF_UP | RTF_GATEWAY,
        );
        assert_eq!(
            parse_route_reply(&message, 17, 9, peer).unwrap(),
            Some(Route {
                interface_index: 4,
                source: Ipv4Addr::new(192, 168, 1, 10),
                next_hop: Ipv4Addr::new(192, 168, 1, 1),
            })
        );

        let error = parse_route_reply(&message, 17, 9, Ipv4Addr::new(192, 168, 2, 20)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "macOS route reply destination does not cover the requested IPv4 peer"
        );

        let default_route = route_reply(
            18,
            10,
            Ipv4Addr::UNSPECIFIED,
            None,
            Ipv4Addr::new(192, 168, 1, 10),
            Some(Ipv4Addr::new(192, 168, 1, 1)),
            RTF_UP | RTF_GATEWAY,
        );
        assert_eq!(
            parse_route_reply(&default_route, 18, 10, Ipv4Addr::new(203, 0, 113, 20)).unwrap(),
            Some(Route {
                interface_index: 4,
                source: Ipv4Addr::new(192, 168, 1, 10),
                next_hop: Ipv4Addr::new(192, 168, 1, 1),
            })
        );
    }

    #[test]
    fn route_parser_checks_lengths_alignment_and_required_interface_address() {
        let peer = Ipv4Addr::new(192, 168, 1, 20);
        let message = route_reply(
            17,
            9,
            Ipv4Addr::new(192, 168, 1, 0),
            Some(24),
            Ipv4Addr::new(192, 168, 1, 10),
            None,
            RTF_UP,
        );
        let mut truncated = message.clone();
        truncated[message.len() - 1] = 0;
        truncated.pop();
        assert!(parse_route_reply(&truncated, 17, 9, peer).is_err());

        let mut missing_source = message;
        write_i32(
            &mut missing_source,
            offset_of!(RtMsgHdr, addresses),
            RTA_DST | RTA_GATEWAY | RTA_NETMASK | RTA_IFP,
        );
        missing_source.truncate(missing_source.len() - size_of::<SockaddrIn>());
        let missing_source_len = missing_source.len() as u16;
        write_u16(
            &mut missing_source,
            offset_of!(RtMsgHdr, message_len),
            missing_source_len,
        );
        assert_eq!(
            parse_route_reply(&missing_source, 17, 9, peer)
                .unwrap_err()
                .to_string(),
            "macOS route reply omitted its IPv4 source"
        );
        assert_eq!(route_roundup(0), size_of::<u32>());
        assert_eq!(route_roundup(9), 12);
    }

    #[test]
    #[ignore = "explicit read-only macOS interface diagnostic"]
    fn native_enumeration_returns_bounded_models() {
        let adapters = enumerate_adapters().unwrap();
        assert!(adapters.len() <= MAX_INTERFACES);
        assert!(adapters.iter().all(|adapter| {
            adapter.index != 0
                && !adapter.name.is_empty()
                && adapter.name.len() < 16
                && (!adapter.physical || !adapter.stable_id.is_empty())
                && (!adapter.ethernet || adapter.physical)
                && (!adapter.wifi || adapter.physical)
        }));
    }

    #[test]
    #[ignore = "explicit read-only macOS routing-control diagnostic"]
    fn native_kernel_lookup_receives_and_rejects_loopback() {
        // SAFETY: the name is a static NUL-terminated C string.
        let lo0 = unsafe { if_nametoindex(c"lo0".as_ptr()) };
        let error = kernel_route_lookup(Ipv4Addr::LOCALHOST, lo0).unwrap_err();
        assert_eq!(error.to_string(), "macOS returned an ineligible route");
    }

    #[test]
    #[ignore = "set MONHOP_TEST_SOURCE to this Mac's physical IPv4 address"]
    fn native_scoped_lookup_rejects_a_route_on_another_interface() {
        // XNU answers every 127/8 lookup on lo0 regardless of scope, so the interface check must refuse it.
        let error =
            best_route(test_ipv4("MONHOP_TEST_SOURCE"), Ipv4Addr::new(127, 0, 0, 2)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the kernel-selected route does not use the requested interface and source address"
        );
    }

    #[test]
    #[ignore = "set MONHOP_TEST_SOURCE and MONHOP_TEST_PEER to a direct physical IPv4 route"]
    fn native_kernel_lookup_accepts_explicit_physical_direct_route() {
        let source = test_ipv4("MONHOP_TEST_SOURCE");
        let peer = test_ipv4("MONHOP_TEST_PEER");
        let route = best_route(source, peer).expect("expected an eligible physical route");
        assert_eq!(route.source, source);
        assert_eq!(route.next_hop, Ipv4Addr::UNSPECIFIED);

        let adapter = enumerate_adapters()
            .expect("expected adapter diagnostics")
            .into_iter()
            .find(|adapter| adapter.index == route.interface_index && adapter.address == source)
            .expect("kernel route interface must have the requested source");
        assert!(adapter.physical && adapter.up);
        assert!(ipv4_prefix_contains(
            adapter.address,
            adapter.prefix_len,
            peer
        ));
    }

    fn test_ipv4(variable: &str) -> Ipv4Addr {
        std::env::var(variable)
            .unwrap_or_else(|_| panic!("{variable} must be set for this explicit native test"))
            .parse()
            .unwrap_or_else(|_| panic!("{variable} must contain an IPv4 address"))
    }

    fn test_wifi_attachment(ssid: &[u8]) -> Vec<u8> {
        crate::wifi_attachment::build_signature(ssid).unwrap()
    }

    fn route_reply(
        pid: i32,
        sequence: i32,
        destination: Ipv4Addr,
        prefix_len: Option<u8>,
        source: Ipv4Addr,
        gateway: Option<Ipv4Addr>,
        flags: i32,
    ) -> Vec<u8> {
        let mut message = vec![0_u8; size_of::<RtMsgHdr>()];
        append_route_address(&mut message, &sockaddr_in(destination));
        match gateway {
            Some(gateway) => append_route_address(&mut message, &sockaddr_in(gateway)),
            None => append_route_address(&mut message, &[8, AF_LINK, 0, 0, 0, 0, 0, 0]),
        }
        match prefix_len {
            Some(prefix_len) => append_route_address(&mut message, &compact_netmask(prefix_len)),
            None => append_route_address(&mut message, &[]),
        }
        append_route_address(&mut message, &[8, AF_LINK, 0, 0, 0, 0, 0, 0]);
        append_route_address(&mut message, &sockaddr_in(source));
        let message_len = message.len() as u16;
        write_u16(&mut message, offset_of!(RtMsgHdr, message_len), message_len);
        message[offset_of!(RtMsgHdr, version)] = RTM_VERSION;
        message[offset_of!(RtMsgHdr, message_type)] = RTM_GET;
        write_u16(&mut message, offset_of!(RtMsgHdr, interface_index), 4);
        write_i32(&mut message, offset_of!(RtMsgHdr, flags), flags);
        write_i32(
            &mut message,
            offset_of!(RtMsgHdr, addresses),
            RTA_DST | RTA_GATEWAY | RTA_NETMASK | RTA_IFP | RTA_IFA,
        );
        write_i32(&mut message, offset_of!(RtMsgHdr, pid), pid);
        write_i32(&mut message, offset_of!(RtMsgHdr, sequence), sequence);
        message
    }

    fn sockaddr_in(address: Ipv4Addr) -> [u8; 16] {
        let mut result = [0_u8; 16];
        result[0] = 16;
        result[1] = AF_INET;
        result[4..8].copy_from_slice(&address.octets());
        result
    }

    fn compact_netmask(prefix_len: u8) -> Vec<u8> {
        let mask = if prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - prefix_len)
        }
        .to_be_bytes();
        let trailing_zeroes = mask.iter().rev().take_while(|byte| **byte == 0).count();
        let length = 4 + mask.len() - trailing_zeroes;
        let mut result = vec![0_u8; length];
        result[0] = length as u8;
        result[4..].copy_from_slice(&mask[..mask.len() - trailing_zeroes]);
        result
    }

    fn append_route_address(message: &mut Vec<u8>, address: &[u8]) {
        let start = message.len();
        message.extend_from_slice(address);
        message.resize(start + route_roundup(address.len()), 0);
    }

    fn write_u16(buffer: &mut [u8], offset: usize, value: u16) {
        buffer[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
    }

    fn route_message(kind: u8, index: u16, flags: i32) -> Vec<u8> {
        let mut message = vec![0_u8; size_of::<RtMsgHdr>()];
        message[offset_of!(RtMsgHdr, version)] = RTM_VERSION;
        message[offset_of!(RtMsgHdr, message_type)] = kind;
        write_u16(&mut message, offset_of!(RtMsgHdr, interface_index), index);
        write_i32(&mut message, offset_of!(RtMsgHdr, flags), flags);
        message
    }

    #[test]
    fn only_route_changes_on_the_selected_interface_count() {
        let gateway = RTF_UP | RTF_GATEWAY;
        assert_eq!(
            relevant_route_change(&route_message(RTM_ADD, 15, gateway), 15),
            Some(RouteChange::Added)
        );
        assert_eq!(
            relevant_route_change(&route_message(RTM_DELETE, 15, RTF_UP), 15),
            Some(RouteChange::Deleted)
        );
        assert_eq!(
            relevant_route_change(&route_message(RTM_CHANGE, 15, gateway), 15),
            Some(RouteChange::Changed)
        );
        assert_eq!(
            relevant_route_change(&route_message(RTM_ADD, 9, gateway), 15),
            None
        );
        assert_eq!(
            relevant_route_change(&route_message(RTM_ADD, 15, RTF_UP | RTF_LLINFO), 15),
            None
        );
        assert_eq!(
            relevant_route_change(&route_message(RTM_GET, 15, gateway), 15),
            None
        );
        let mut stale = route_message(RTM_ADD, 15, gateway);
        stale[offset_of!(RtMsgHdr, version)] = 4;
        assert_eq!(relevant_route_change(&stale, 15), None);
        assert_eq!(relevant_route_change(&[RTM_VERSION; 3], 15), None);
    }

    #[test]
    fn a_stopped_route_observer_returns_at_once() {
        let observer = RouteObserver::open().unwrap();
        observer.stop();
        assert_eq!(observer.next(1).unwrap(), None);
    }

    fn write_i32(buffer: &mut [u8], offset: usize, value: i32) {
        buffer[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }
}
