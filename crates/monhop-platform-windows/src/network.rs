//! Read-only adapter/route diagnostics and explicit UDP interface restrictions.

use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::windows::io::AsRawSocket,
    ptr,
};
use windows_sys::{
    Win32::{
        Foundation::HANDLE,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub interface_index: u32,
    pub source: Ipv4Addr,
    pub next_hop: Ipv4Addr,
}

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
        if address.SkipAsSource || address.DadState != IpDadStatePreferred {
            continue;
        }
        let mut interface = MIB_IF_ROW2 {
            InterfaceIndex: address.InterfaceIndex,
            ..Default::default()
        };
        // SAFETY: InterfaceIndex selects the row; the remaining fields are initialized by Windows.
        win_result(unsafe { GetIfEntry2(&mut interface) })?;
        let ethernet = interface.Type == IF_TYPE_ETHERNET_CSMACD;
        let wifi = interface.Type == IF_TYPE_IEEE80211;
        let flags = interface.InterfaceAndOperStatusFlags._bitfield;
        let physical = flags & 1 != 0 && flags & 2 == 0 && interface.TunnelType == 0;
        let up = interface.OperStatus == IfOperStatusUp && flags & (8 | 16 | 32 | 64) == 0;
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
        adapters.push(Adapter {
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
        });
    }
    adapters.sort_by_key(|adapter| (adapter.index, adapter.address));
    Ok(adapters)
}

/// Reads the actual best route, without forcing Windows to pretend the selected NIC is best.
pub fn best_route(source: Ipv4Addr, peer: Ipv4Addr) -> io::Result<Route> {
    let source = sockaddr(source);
    let destination = sockaddr(peer);
    let mut best_source = SOCKADDR_INET::default();
    let mut route = MIB_IPFORWARD_ROW2::default();
    // SAFETY: all pointers refer to initialized in/out structures for this synchronous call.
    win_result(unsafe {
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
        return Err(io::Error::other(
            "Loopback route is not a physical peer route",
        ));
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

/// Keeps a socket's datagrams to one peer in the voice class, which Windows tags so Wi-Fi queues
/// them ahead of bulk traffic; dropping it ends the marking.
pub struct InteractiveTraffic(HANDLE);

// SAFETY: the qWAVE handle is used only when created and when closed on drop, never shared.
unsafe impl Send for InteractiveTraffic {}
// SAFETY: no method reads or writes the handle through a shared reference.
unsafe impl Sync for InteractiveTraffic {}

/// An unconnected socket names its one peer. A non-adaptive flow sends no probes, and a traffic
/// type needs no administrator rights.
pub fn mark_interactive_traffic(
    socket: &UdpSocket,
    peer: SocketAddrV4,
) -> io::Result<InteractiveTraffic> {
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
    let destination = sockaddr_in(peer);
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
