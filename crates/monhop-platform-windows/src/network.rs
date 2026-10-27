//! Read-only adapter/route diagnostics and explicit UDP interface restrictions.

use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::windows::io::AsRawSocket,
    ptr,
};
use windows_sys::{
    Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_HOST_UNREACHABLE, ERROR_NETWORK_UNREACHABLE,
            ERROR_NOT_FOUND, HANDLE, NO_ERROR,
        },
        NetworkManagement::{
            IpHelper::*,
            Ndis::IfOperStatusUp,
            QoS::{
                QOS_NON_ADAPTIVE_FLOW, QOS_VERSION, QOSAddSocketToFlow, QOSCloseHandle,
                QOSCreateHandle, QOSTrafficTypeVoice,
            },
            WiFi::*,
        },
        Networking::WinSock::*,
    },
    core::GUID,
};

const WIFI_NETWORK_GUID_LEN: usize = 16;
const MAX_WIFI_SSID_LEN: usize = 32;
// MIB_IF_ROW2 InterfaceAndOperStatusFlags bits.
const IF_HARDWARE: u8 = 1;
const IF_FILTER: u8 = 2;
const IF_NOT_AUTHENTICATED: u8 = 8;
const IF_NOT_MEDIA_CONNECTED: u8 = 16;
const IF_PAUSED: u8 = 32;
const IF_LOW_POWER: u8 = 64;
/// The link itself is down: a move to a same-named network passes through this.
const IF_LINK_DOWN: u8 = IF_NOT_AUTHENTICATED | IF_NOT_MEDIA_CONNECTED;
/// The adapter is held without losing its link, and passes traffic again once resumed.
const IF_SUSPENDED: u8 = IF_PAUSED | IF_LOW_POWER;

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
    /// Attachment metadata only. Never display the SSID in automatic logs.
    pub attachment: Option<Vec<u8>>,
}

impl Adapter {
    /// Returns the SSID only when this is a live physical Wi-Fi adapter with a valid attachment.
    pub fn wifi_network_name(&self) -> Option<&str> {
        if !self.physical || !self.wifi || !self.up {
            return None;
        }
        ssid_from_attachment(self.attachment.as_deref()?)
    }
}

/// One read of the selected adapter, for a recheck that must tell a change from a transition.
pub enum AdapterReading {
    /// Read back as `enumerate_adapters` lists it; the attachment is `None` while unidentified.
    Present(Adapter),
    /// The interface, its link, or the selected address on it is gone, or the address is refused.
    Lost,
    /// Present with its link up, but held: the address is not yet or no longer preferred, or the
    /// adapter is paused or in low power.
    Settling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub interface_index: u32,
    pub source: Ipv4Addr,
    pub next_hop: Ipv4Addr,
}

/// A route lookup that answered, with a route the selected-network policy refuses for good.
#[derive(Debug)]
pub struct RouteRefused(pub &'static str);

impl std::fmt::Display for RouteRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for RouteRefused {}

struct AddressTable(*mut MIB_UNICASTIPADDRESS_TABLE);
impl Drop for AddressTable {
    fn drop(&mut self) {
        // SAFETY: this table was allocated by GetUnicastIpAddressTable and is freed once.
        unsafe { FreeMibTable(self.0.cast()) };
    }
}

pub fn enumerate_adapters() -> io::Result<Vec<Adapter>> {
    let mut table = ptr::null_mut();
    // SAFETY: the API initializes the output pointer on success.
    win_result(unsafe { GetUnicastIpAddressTable(AF_INET, &mut table) })?;
    if table.is_null() {
        return Err(io::Error::other("Missing interface table"));
    }
    let table = AddressTable(table);
    // SAFETY: the system-owned table stays alive for the iteration; its count describes the tail array.
    let rows = unsafe {
        let count = (*table.0).NumEntries as usize;
        if count > 4096 {
            return Err(io::Error::other("Interface table exceeds bound"));
        }
        std::slice::from_raw_parts(
            ptr::addr_of!((*table.0).Table).cast::<MIB_UNICASTIPADDRESS_ROW>(),
            count,
        )
    };
    let mut adapters = Vec::new();
    for address in rows {
        if address_readiness(address.DadState, address.SkipAsSource) != Readiness::Ready {
            continue;
        }
        // An interface removed since the table was read is no longer listed.
        let Some(interface) = interface_row(address.InterfaceIndex)? else {
            continue;
        };
        adapters.push(adapter_from_rows(address, &interface)?);
    }
    adapters.sort_by_key(|adapter| (adapter.index, adapter.address));
    Ok(adapters)
}

/// Reads only the selected interface and its selected address, so another adapter coming or
/// going mid-read cannot fail it.
pub fn read_selected_adapter(selected: &Adapter) -> io::Result<AdapterReading> {
    let mut address = MIB_UNICASTIPADDRESS_ROW {
        Address: sockaddr(selected.address),
        InterfaceIndex: selected.index,
        ..Default::default()
    };
    // SAFETY: the row names one address on one interface; Windows fills the rest synchronously.
    match unsafe { GetUnicastIpAddressEntry(&mut address) } {
        NO_ERROR => {}
        ERROR_NOT_FOUND | ERROR_FILE_NOT_FOUND => return Ok(AdapterReading::Lost),
        code => return Err(io::Error::from_raw_os_error(code as i32)),
    }
    let Some(interface) = interface_row(selected.index)? else {
        return Ok(AdapterReading::Lost);
    };
    let address_ready = address_readiness(address.DadState, address.SkipAsSource);
    match address_ready.and(link_readiness(&interface)) {
        Readiness::Ready => Ok(AdapterReading::Present(adapter_from_rows(
            &address, &interface,
        )?)),
        Readiness::Settling => Ok(AdapterReading::Settling),
        Readiness::Lost => Ok(AdapterReading::Lost),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Readiness {
    Ready,
    Settling,
    Lost,
}

impl Readiness {
    /// Lost outranks settling, which outranks ready.
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Lost, _) | (_, Self::Lost) => Self::Lost,
            (Self::Settling, _) | (_, Self::Settling) => Self::Settling,
            _ => Self::Ready,
        }
    }
}

/// A duplicate or invalid address is refused for good; one still being confirmed, past its
/// preferred lifetime, or kept from use as a source is between states.
fn address_readiness(dad: NL_DAD_STATE, skip_as_source: bool) -> Readiness {
    if dad == IpDadStateDuplicate || dad == IpDadStateInvalid {
        Readiness::Lost
    } else if dad == IpDadStatePreferred && !skip_as_source {
        Readiness::Ready
    } else {
        Readiness::Settling
    }
}

/// A link that is down, unplugged or unauthenticated is lost at once, since a move to a
/// same-named network goes through one; a paused or low-power adapter keeps its link.
fn link_readiness(interface: &MIB_IF_ROW2) -> Readiness {
    let flags = interface.InterfaceAndOperStatusFlags._bitfield;
    if interface.OperStatus != IfOperStatusUp || flags & IF_LINK_DOWN != 0 {
        Readiness::Lost
    } else if flags & IF_SUSPENDED != 0 {
        Readiness::Settling
    } else {
        Readiness::Ready
    }
}

/// `None` once the interface is gone.
fn interface_row(index: u32) -> io::Result<Option<MIB_IF_ROW2>> {
    let mut interface = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    // SAFETY: InterfaceIndex selects the row; the remaining fields are initialized by Windows.
    match unsafe { GetIfEntry2(&mut interface) } {
        NO_ERROR => Ok(Some(interface)),
        ERROR_FILE_NOT_FOUND => Ok(None),
        code => Err(io::Error::from_raw_os_error(code as i32)),
    }
}

/// The adapter one address row and its interface row describe.
fn adapter_from_rows(
    address: &MIB_UNICASTIPADDRESS_ROW,
    interface: &MIB_IF_ROW2,
) -> io::Result<Adapter> {
    let ethernet = interface.Type == IF_TYPE_ETHERNET_CSMACD;
    let wifi = interface.Type == IF_TYPE_IEEE80211;
    let flags = interface.InterfaceAndOperStatusFlags._bitfield;
    let physical = flags & IF_HARDWARE != 0 && flags & IF_FILTER == 0 && interface.TunnelType == 0;
    let up = link_readiness(interface) == Readiness::Ready;
    let guid = guid_bytes(&interface.NetworkGuid);
    let attachment = if wifi && up {
        wifi_attachment(&interface.InterfaceGuid)
            .ok()
            .map(|mut connection| {
                connection.extend_from_slice(&guid);
                connection
            })
    } else if ethernet && up && guid != [0; 16] {
        Some(guid.to_vec())
    } else {
        None
    };
    Ok(Adapter {
        stable_id: guid_bytes(&interface.InterfaceGuid)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        name: String::from_utf16_lossy(
            &interface.Alias[..interface
                .Alias
                .iter()
                .position(|v| *v == 0)
                .unwrap_or(interface.Alias.len())],
        ),
        index: address.InterfaceIndex,
        address: ipv4_from_sockaddr(&address.Address)?,
        prefix_len: address.OnLinkPrefixLength,
        physical,
        up,
        ethernet,
        wifi,
        attachment,
    })
}

/// Reads the actual best route, without forcing Windows to pretend the selected NIC is best.
/// No route to the peer is `HostUnreachable` or `NetworkUnreachable`, and a route the policy
/// refuses carries `RouteRefused`; every other failure keeps its own kind.
pub fn best_route(source: Ipv4Addr, peer: Ipv4Addr) -> io::Result<Route> {
    let source = sockaddr(source);
    let destination = sockaddr(peer);
    let mut best_source = SOCKADDR_INET::default();
    let mut route = MIB_IPFORWARD_ROW2::default();
    // SAFETY: all pointers refer to initialized in/out structures for this synchronous call.
    route_lookup_result(unsafe {
        GetBestRoute2(
            ptr::null(),
            0,
            &source,
            &destination,
            0,
            &mut route,
            &mut best_source,
        )
    })?;
    if route.Loopback {
        return Err(io::Error::other(RouteRefused(
            "Loopback route is not a physical peer route",
        )));
    }
    Ok(Route {
        interface_index: route.InterfaceIndex,
        source: ipv4_from_sockaddr(&best_source)?,
        next_hop: ipv4_from_sockaddr(&route.NextHop)?,
    })
}

/// Restricts an already address-bound socket. This is not peer authorization or a network-change watcher.
/// Callers must validate NetworkLock first and close the endpoint on attachment/route changes.
pub fn restrict_udp_interface(socket: &UdpSocket, index: u32) -> io::Result<()> {
    if index == 0
        || !matches!(socket.local_addr()?, std::net::SocketAddr::V4(address) if !address.ip().is_unspecified())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "An exact IPv4 address and interface are required",
        ));
    }
    socket.set_broadcast(false)?;
    // Receive filtering starts with an empty allowlist. Any unsupported option fails closed.
    set_ip_option(socket, IP_IFLIST, 1)?;
    set_ip_option(socket, IP_ADD_IFLIST, index)?;
    set_ip_option(socket, IP_UNICAST_IF, index.to_be())?;
    set_ip_option(socket, IP_RECEIVE_BROADCAST, 0)?;
    Ok(())
}

/// Keeps a socket's datagrams to its peers in the voice class, which Windows tags so Wi-Fi queues
/// them ahead of bulk traffic; dropping it ends the marking.
pub struct InteractiveTraffic(HANDLE);

// SAFETY: the qWAVE handle is used only when created and when closed on drop, never shared.
unsafe impl Send for InteractiveTraffic {}
// SAFETY: no method reads or writes the handle through a shared reference.
unsafe impl Sync for InteractiveTraffic {}

/// An unconnected socket names each peer, one flow apiece on one handle; any refused flow drops
/// them all. A non-adaptive flow sends no probes, and a traffic type needs no administrator rights.
pub fn mark_interactive_traffic(
    socket: &UdpSocket,
    peers: &[SocketAddrV4],
) -> io::Result<InteractiveTraffic> {
    if peers.is_empty() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let version = QOS_VERSION {
        MajorVersion: 1,
        MinorVersion: 0,
    };
    let mut handle = ptr::null_mut();
    // SAFETY: the version and the handle output are valid for this synchronous call.
    if unsafe { QOSCreateHandle(&version, &mut handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let traffic = InteractiveTraffic(handle);
    for peer in peers {
        let destination = sockaddr_in(*peer);
        let mut flow = 0;
        // SAFETY: the handle and socket are live, and the destination is a complete SOCKADDR_IN.
        let added = unsafe {
            QOSAddSocketToFlow(
                traffic.0,
                socket.as_raw_socket() as SOCKET,
                (&raw const destination).cast(),
                QOSTrafficTypeVoice,
                QOS_NON_ADAPTIVE_FLOW,
                &mut flow,
            )
        };
        if added == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(traffic)
}

impl Drop for InteractiveTraffic {
    fn drop(&mut self) {
        // SAFETY: the handle came from QOSCreateHandle and is closed once, which removes its flow.
        unsafe { QOSCloseHandle(self.0) };
    }
}

fn set_ip_option(socket: &UdpSocket, option: i32, value: u32) -> io::Result<()> {
    // SAFETY: a live socket handle and a correctly sized DWORD are passed synchronously.
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            IPPROTO_IP,
            option,
            (&value as *const u32).cast(),
            size_of::<u32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        // SAFETY: WSAGetLastError reads this thread's last socket error.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}

fn wifi_attachment(guid: &GUID) -> io::Result<Vec<u8>> {
    let mut version = 0;
    let mut handle = ptr::null_mut();
    // SAFETY: output variables are writable; reserved parameter is null.
    win_result(unsafe { WlanOpenHandle(2, ptr::null(), &mut version, &mut handle) })?;
    let mut data = ptr::null_mut();
    let mut len = 0;
    // SAFETY: the live WLAN handle and GUID are valid during this synchronous query.
    let result = unsafe {
        WlanQueryInterface(
            handle,
            guid,
            wlan_intf_opcode_current_connection,
            ptr::null(),
            &mut len,
            &mut data,
            ptr::null_mut(),
        )
    };
    // SAFETY: query completed and the handle is no longer needed.
    unsafe {
        WlanCloseHandle(handle, ptr::null());
    }
    win_result(result)?;
    let answer = if !data.is_null() && len as usize >= size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
        // SAFETY: the returned allocation contains a connection structure of the checked size.
        let connection = unsafe { &*data.cast::<WLAN_CONNECTION_ATTRIBUTES>() };
        let association = &connection.wlanAssociationAttributes;
        let ssid_len = association.dot11Ssid.uSSIDLength as usize;
        let ssid = association.dot11Ssid.ucSSID.get(..ssid_len);
        match ssid.and_then(wifi_signature) {
            Some(signature) if connection.isState == wlan_interface_state_connected => {
                Ok(signature)
            }
            _ => Err(io::Error::other("Wi-Fi attachment unavailable")),
        }
    } else {
        Err(io::Error::other("Missing Wi-Fi connection data"))
    };
    if !data.is_null() {
        // SAFETY: this allocation came from WlanQueryInterface and is no longer borrowed.
        unsafe {
            WlanFreeMemory(data);
        }
    }
    answer
}

/// The network, not the access point, so roaming within one network keeps it.
fn wifi_signature(ssid: &[u8]) -> Option<Vec<u8>> {
    if ssid.is_empty() || ssid.len() > MAX_WIFI_SSID_LEN {
        return None;
    }
    let mut signature = Vec::with_capacity(1 + ssid.len());
    signature.push(ssid.len() as u8);
    signature.extend_from_slice(ssid);
    Some(signature)
}

fn ssid_from_attachment(attachment: &[u8]) -> Option<&str> {
    let ssid_len = *attachment.first()? as usize;
    if !(1..=MAX_WIFI_SSID_LEN).contains(&ssid_len) {
        return None;
    }
    let ssid_start = 1;
    let ssid_end = ssid_start + ssid_len;
    if attachment.len() != ssid_end.checked_add(WIFI_NETWORK_GUID_LEN)? {
        return None;
    }
    std::str::from_utf8(&attachment[ssid_start..ssid_end]).ok()
}

fn sockaddr(address: Ipv4Addr) -> SOCKADDR_INET {
    SOCKADDR_INET {
        Ipv4: sockaddr_in(SocketAddrV4::new(address, 0)),
    }
}

fn sockaddr_in(address: SocketAddrV4) -> SOCKADDR_IN {
    SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: address.port().to_be(),
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes(address.ip().octets()),
            },
        },
        ..Default::default()
    }
}
pub(crate) fn ipv4_from_sockaddr(address: &SOCKADDR_INET) -> io::Result<Ipv4Addr> {
    // SAFETY: the family tag is inspected before reading its corresponding union member.
    unsafe {
        if address.si_family != AF_INET {
            return Err(io::Error::other("IPv4 required"));
        }
        Ok(Ipv4Addr::from(
            address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes(),
        ))
    }
}
fn guid_bytes(guid: &GUID) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&guid.data1.to_le_bytes());
    bytes[4..6].copy_from_slice(&guid.data2.to_le_bytes());
    bytes[6..8].copy_from_slice(&guid.data3.to_le_bytes());
    bytes[8..].copy_from_slice(&guid.data4);
    bytes
}
fn win_result(code: u32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// Callers set aside only the one peer a route lookup cannot reach, so those failures carry the
/// unreachable kinds whatever std maps the codes to.
fn route_lookup_result(code: u32) -> io::Result<()> {
    let kind = match code {
        ERROR_HOST_UNREACHABLE => io::ErrorKind::HostUnreachable,
        ERROR_NETWORK_UNREACHABLE => io::ErrorKind::NetworkUnreachable,
        _ => return win_result(code),
    };
    Err(io::Error::new(
        kind,
        io::Error::from_raw_os_error(code as i32),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wifi_attachment_record(ssid: &[u8]) -> Vec<u8> {
        let mut attachment = wifi_signature(ssid).unwrap();
        attachment.extend_from_slice(&[0; WIFI_NETWORK_GUID_LEN]);
        attachment
    }

    fn wifi_adapter(attachment: Option<Vec<u8>>) -> Adapter {
        Adapter {
            stable_id: "stable".into(),
            name: "Wi-Fi".into(),
            index: 1,
            address: Ipv4Addr::new(192, 168, 1, 10),
            prefix_len: 24,
            physical: true,
            up: true,
            ethernet: false,
            wifi: true,
            attachment,
        }
    }

    #[test]
    fn an_address_being_confirmed_is_settling_and_a_refused_one_is_lost() {
        assert_eq!(
            address_readiness(IpDadStatePreferred, false),
            Readiness::Ready
        );
        for (dad, skip_as_source) in [
            (IpDadStateTentative, false),
            (IpDadStateDeprecated, false),
            (IpDadStatePreferred, true),
        ] {
            assert_eq!(
                address_readiness(dad, skip_as_source),
                Readiness::Settling,
                "{dad} {skip_as_source}"
            );
        }
        for dad in [IpDadStateDuplicate, IpDadStateInvalid] {
            assert_eq!(address_readiness(dad, false), Readiness::Lost, "{dad}");
        }
    }

    #[test]
    fn a_link_that_goes_down_is_lost_and_a_held_adapter_is_settling() {
        use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusDown;
        let (_, interface) = ethernet_rows();
        assert_eq!(link_readiness(&interface), Readiness::Ready);
        for flag in [IF_NOT_AUTHENTICATED, IF_NOT_MEDIA_CONNECTED] {
            let mut down = interface;
            down.InterfaceAndOperStatusFlags._bitfield |= flag;
            assert_eq!(link_readiness(&down), Readiness::Lost, "{flag}");
        }
        let mut not_operational = interface;
        not_operational.OperStatus = IfOperStatusDown;
        assert_eq!(link_readiness(&not_operational), Readiness::Lost);
        for flag in [IF_PAUSED, IF_LOW_POWER] {
            let mut held = interface;
            held.InterfaceAndOperStatusFlags._bitfield |= flag;
            assert_eq!(link_readiness(&held), Readiness::Settling, "{flag}");
            // A link that also went down is lost, whatever else holds the adapter.
            held.InterfaceAndOperStatusFlags._bitfield |= IF_NOT_MEDIA_CONNECTED;
            assert_eq!(link_readiness(&held), Readiness::Lost, "{flag}");
        }
        assert_eq!(Readiness::Settling.and(Readiness::Lost), Readiness::Lost);
        assert_eq!(
            Readiness::Ready.and(Readiness::Settling),
            Readiness::Settling
        );
    }

    fn ethernet_rows() -> (MIB_UNICASTIPADDRESS_ROW, MIB_IF_ROW2) {
        let address = MIB_UNICASTIPADDRESS_ROW {
            Address: sockaddr(Ipv4Addr::new(192, 168, 1, 10)),
            InterfaceIndex: 7,
            OnLinkPrefixLength: 24,
            DadState: IpDadStatePreferred,
            ..Default::default()
        };
        let mut interface = MIB_IF_ROW2 {
            InterfaceIndex: 7,
            Type: IF_TYPE_ETHERNET_CSMACD,
            OperStatus: IfOperStatusUp,
            NetworkGuid: GUID::from_u128(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff),
            ..Default::default()
        };
        interface.InterfaceAndOperStatusFlags._bitfield = IF_HARDWARE;
        (address, interface)
    }

    #[test]
    fn adapter_listing_shows_only_a_ready_link_as_up() {
        let (address, interface) = ethernet_rows();
        let adapter = adapter_from_rows(&address, &interface).unwrap();
        assert!(adapter.up && adapter.physical && adapter.ethernet);
        assert_eq!(adapter.address, Ipv4Addr::new(192, 168, 1, 10));
        assert!(adapter.attachment.is_some());

        for flag in [
            IF_NOT_AUTHENTICATED,
            IF_NOT_MEDIA_CONNECTED,
            IF_PAUSED,
            IF_LOW_POWER,
        ] {
            let mut flagged = interface;
            flagged.InterfaceAndOperStatusFlags._bitfield |= flag;
            let adapter = adapter_from_rows(&address, &flagged).unwrap();
            assert!(!adapter.up, "{flag}");
            assert!(adapter.attachment.is_none(), "{flag}");
        }
        let mut filter = interface;
        filter.InterfaceAndOperStatusFlags._bitfield |= IF_FILTER;
        assert!(!adapter_from_rows(&address, &filter).unwrap().physical);
    }

    #[test]
    #[ignore = "set MONHOP_TEST_INTERFACE_INDEX to an up physical adapter's index"]
    fn native_targeted_read_matches_the_listed_adapter() {
        let index = std::env::var("MONHOP_TEST_INTERFACE_INDEX")
            .expect("explicit selected interface index required")
            .parse::<u32>()
            .expect("numeric interface index required");
        let listed = enumerate_adapters()
            .unwrap()
            .into_iter()
            .find(|adapter| adapter.index == index && adapter.physical && adapter.up)
            .expect("selected physical interface must be present and up");
        match read_selected_adapter(&listed).unwrap() {
            AdapterReading::Present(read) => assert!(read == listed),
            AdapterReading::Lost => panic!("the listed adapter read back lost"),
            AdapterReading::Settling => panic!("the listed adapter read back settling"),
        }
        let mut elsewhere = listed;
        elsewhere.address = Ipv4Addr::new(192, 0, 2, 1);
        assert!(matches!(
            read_selected_adapter(&elsewhere).unwrap(),
            AdapterReading::Lost
        ));
    }

    #[test]
    fn only_a_missing_route_reads_as_unreachable() {
        use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, ERROR_NOT_FOUND};
        assert!(route_lookup_result(0).is_ok());
        for (code, kind) in [
            (ERROR_HOST_UNREACHABLE, io::ErrorKind::HostUnreachable),
            (ERROR_NETWORK_UNREACHABLE, io::ErrorKind::NetworkUnreachable),
        ] {
            assert_eq!(route_lookup_result(code).unwrap_err().kind(), kind);
        }
        for code in [ERROR_NOT_FOUND, ERROR_INVALID_PARAMETER] {
            let error = route_lookup_result(code).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code as i32));
            assert!(!matches!(
                error.kind(),
                io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable
            ));
        }
    }

    #[test]
    #[ignore = "explicit qWAVE probe on a live adapter; binds one local socket and sends nothing"]
    fn one_handle_marks_a_flow_for_each_peer_on_one_socket() {
        let adapter = enumerate_adapters()
            .unwrap()
            .into_iter()
            .find(|adapter| {
                adapter.physical
                    && adapter.up
                    && adapter.address.is_private()
                    && (8..=29).contains(&adapter.prefix_len)
            })
            .expect("an up physical adapter with a private address");
        let socket = UdpSocket::bind((adapter.address, 0)).unwrap();
        let network = u32::from(adapter.address) & (u32::MAX << (32 - adapter.prefix_len));
        let peers: Vec<_> = (1..)
            .map(|host| Ipv4Addr::from(network + host))
            .filter(|address| *address != adapter.address)
            .take(3)
            .map(|address| SocketAddrV4::new(address, 24872))
            .collect();
        assert_eq!(
            mark_interactive_traffic(&socket, &[])
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::InvalidInput)
        );
        let traffic = mark_interactive_traffic(&socket, &peers).unwrap();
        drop(traffic);
        drop(mark_interactive_traffic(&socket, &peers[..1]).unwrap());
    }

    #[test]
    fn sockaddr_preserves_network_byte_order() {
        for address in [
            Ipv4Addr::new(192, 168, 8, 7),
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(169, 254, 10, 20),
        ] {
            assert_eq!(ipv4_from_sockaddr(&sockaddr(address)).unwrap(), address);
        }
        let peer = sockaddr_in(SocketAddrV4::new(Ipv4Addr::new(192, 168, 8, 7), 0x1f90));
        assert_eq!(peer.sin_port.to_ne_bytes(), [0x1f, 0x90]);
    }

    #[test]
    fn extracts_only_exact_utf8_bounded_ssids() {
        let short = wifi_attachment_record(b"studio");
        assert_eq!(ssid_from_attachment(&short), Some("studio"));

        let max = wifi_attachment_record(&[b'x'; MAX_WIFI_SSID_LEN]);
        assert_eq!(
            ssid_from_attachment(&max),
            Some("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
        );

        let mut truncated = short.clone();
        truncated.pop();
        assert!(ssid_from_attachment(&truncated).is_none());

        let mut wrong_length = short;
        wrong_length[0] = 5;
        assert!(ssid_from_attachment(&wrong_length).is_none());
        wrong_length[0] = 0;
        assert!(ssid_from_attachment(&wrong_length).is_none());

        let mut overlong = wifi_attachment_record(&[b'x'; MAX_WIFI_SSID_LEN]);
        overlong[0] = (MAX_WIFI_SSID_LEN + 1) as u8;
        assert!(ssid_from_attachment(&overlong).is_none());

        let non_utf8 = wifi_attachment_record(&[0xff]);
        assert!(ssid_from_attachment(&non_utf8).is_none());
    }

    #[test]
    fn the_signature_names_the_network_not_the_access_point() {
        assert_eq!(wifi_signature(b"studio"), Some(b"\x06studio".to_vec()));
        assert_eq!(
            wifi_signature(&[b'x'; MAX_WIFI_SSID_LEN]).map(|signature| signature.len()),
            Some(1 + MAX_WIFI_SSID_LEN)
        );
        assert_eq!(wifi_signature(b""), None);
        assert_eq!(wifi_signature(&[b'x'; MAX_WIFI_SSID_LEN + 1]), None);
    }

    #[test]
    fn wifi_network_name_requires_live_physical_wifi_with_an_attachment() {
        let adapter = wifi_adapter(Some(wifi_attachment_record(b"studio")));
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
            wifi_adapter(None),
        ] {
            assert_eq!(ineligible.wifi_network_name(), None);
        }
    }
}
