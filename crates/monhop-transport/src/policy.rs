//! Validation is required before socket creation and again whenever the interface changes.
use std::net::Ipv4Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterfaceKind {
    Ethernet,
    WiFi,
    Other,
}

#[derive(Clone, PartialEq, Eq)]
pub struct InterfaceSnapshot {
    pub stable_id: String,
    pub name: String,
    pub index: u32,
    pub address: Ipv4Addr,
    pub prefix_len: u8,
    pub kind: InterfaceKind,
    pub is_hardware: bool,
    pub is_up: bool,
    /// Changes on network attachment/profile changes even when DHCP reuses the same address.
    pub network_signature: Vec<u8>,
}

impl std::fmt::Debug for InterfaceSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InterfaceSnapshot")
            .field("stable_id", &self.stable_id)
            .field("name", &self.name)
            .field("index", &self.index)
            .field("address", &self.address)
            .field("prefix_len", &self.prefix_len)
            .field("kind", &self.kind)
            .field("is_hardware", &self.is_hardware)
            .field("is_up", &self.is_up)
            .field("network_signature", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteSnapshot {
    pub interface_index: u32,
    pub source: Ipv4Addr,
    /// An unspecified next hop denotes a directly connected route.
    pub next_hop: Ipv4Addr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyError {
    InvalidInterface,
    UnknownAttachment,
    InterfaceDown,
    NonPhysicalInterface,
    InvalidSubnet,
    NonPrivateAddress,
    PeerIsLocal,
    NetworkOrBroadcastAddress,
    OffLinkPeer,
    RoutedPeer,
    WrongInterface,
    InterfaceChanged,
    SessionClosed,
    DiscoveryDisabled,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidInterface => "select a physical interface with a valid stable identifier",
            Self::UnknownAttachment => {
                "the current physical network attachment could not be identified"
            }
            Self::InterfaceDown => "the selected interface is down",
            Self::NonPhysicalInterface => "only physical Ethernet and Wi-Fi interfaces are allowed",
            Self::InvalidSubnet => "the selected IPv4 subnet must have a prefix between 8 and 31",
            Self::NonPrivateAddress => {
                "only RFC1918 private or IPv4 link-local addresses are allowed"
            }
            Self::PeerIsLocal => "the peer must be another machine",
            Self::NetworkOrBroadcastAddress => "a subnet or broadcast address cannot be a peer",
            Self::OffLinkPeer => "the peer is outside the selected directly connected subnet",
            Self::RoutedPeer => "the route to the peer uses a gateway",
            Self::WrongInterface => "the route or packet uses an unselected interface",
            Self::InterfaceChanged => {
                "the selected interface or network changed; reconnect explicitly"
            }
            Self::SessionClosed => "this network lock has been revoked",
            Self::DiscoveryDisabled => "discovery is not supported in this version",
        })
    }
}

impl std::error::Error for PolicyError {}

#[derive(Clone, Debug)]
pub struct NetworkLock {
    selected: InterfaceSnapshot,
    peer: Ipv4Addr,
    revoked: bool,
}

impl NetworkLock {
    pub fn new(
        selected: InterfaceSnapshot,
        peer: Ipv4Addr,
        route: RouteSnapshot,
        allow_discovery: bool,
    ) -> Result<Self, PolicyError> {
        if allow_discovery {
            return Err(PolicyError::DiscoveryDisabled);
        }
        validate_interface(&selected)?;
        validate_peer(&selected, peer)?;
        validate_route(&selected, route)?;
        Ok(Self {
            selected,
            peer,
            revoked: false,
        })
    }

    pub fn selected(&self) -> &InterfaceSnapshot {
        &self.selected
    }

    pub fn peer(&self) -> Ipv4Addr {
        self.peer
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked
    }

    /// Revocation is sticky: restoring the old address must not silently resume input forwarding.
    pub fn revalidate(
        &mut self,
        current: Option<&InterfaceSnapshot>,
        route: RouteSnapshot,
    ) -> Result<(), PolicyError> {
        if self.revoked {
            return Err(PolicyError::SessionClosed);
        }
        let result = match current {
            Some(current) if current == &self.selected => validate_route(current, route),
            _ => Err(PolicyError::InterfaceChanged),
        };
        if result.is_err() {
            self.revoked = true;
        }
        result
    }

    pub fn authorize_packet(
        &self,
        local: Ipv4Addr,
        source: Ipv4Addr,
        arrival_interface: u32,
    ) -> Result<(), PolicyError> {
        if self.revoked {
            return Err(PolicyError::SessionClosed);
        }
        if local != self.selected.address || arrival_interface != self.selected.index {
            return Err(PolicyError::WrongInterface);
        }
        if source != self.peer {
            return Err(PolicyError::OffLinkPeer);
        }
        Ok(())
    }
}

pub fn is_private_or_link_local(address: Ipv4Addr) -> bool {
    address.is_private() || address.is_link_local()
}

pub fn validate_interface(interface: &InterfaceSnapshot) -> Result<(), PolicyError> {
    if interface.index == 0 || interface.stable_id.is_empty() || interface.stable_id.len() > 256 {
        return Err(PolicyError::InvalidInterface);
    }
    if interface.network_signature.is_empty()
        || interface.network_signature.len() > 128
        || interface.network_signature.iter().all(|byte| *byte == 0)
    {
        return Err(PolicyError::UnknownAttachment);
    }
    if !interface.is_up {
        return Err(PolicyError::InterfaceDown);
    }
    if !interface.is_hardware || interface.kind == InterfaceKind::Other {
        return Err(PolicyError::NonPhysicalInterface);
    }
    if !(8..=31).contains(&interface.prefix_len) {
        return Err(PolicyError::InvalidSubnet);
    }
    if !is_private_or_link_local(interface.address) {
        return Err(PolicyError::NonPrivateAddress);
    }
    validate_host(interface.address, interface.prefix_len)
}

pub fn validate_peer(interface: &InterfaceSnapshot, peer: Ipv4Addr) -> Result<(), PolicyError> {
    validate_interface(interface)?;
    if !is_private_or_link_local(peer) {
        return Err(PolicyError::NonPrivateAddress);
    }
    if interface.address == peer {
        return Err(PolicyError::PeerIsLocal);
    }
    let mask = u32::MAX << (32 - interface.prefix_len);
    if u32::from(interface.address) & mask != u32::from(peer) & mask {
        return Err(PolicyError::OffLinkPeer);
    }
    validate_host(peer, interface.prefix_len)
}

fn validate_host(address: Ipv4Addr, prefix: u8) -> Result<(), PolicyError> {
    if prefix == 31 {
        return Ok(());
    }
    let host_mask = u32::MAX >> prefix;
    let host = u32::from(address) & host_mask;
    if host == 0 || host == host_mask {
        return Err(PolicyError::NetworkOrBroadcastAddress);
    }
    Ok(())
}

fn validate_route(interface: &InterfaceSnapshot, route: RouteSnapshot) -> Result<(), PolicyError> {
    if route.interface_index != interface.index || route.source != interface.address {
        return Err(PolicyError::WrongInterface);
    }
    if !route.next_hop.is_unspecified() {
        return Err(PolicyError::RoutedPeer);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected() -> InterfaceSnapshot {
        InterfaceSnapshot {
            stable_id: "physical-adapter-id".into(),
            name: "Ethernet".into(),
            index: 7,
            address: Ipv4Addr::new(192, 168, 50, 10),
            prefix_len: 24,
            kind: InterfaceKind::Ethernet,
            is_hardware: true,
            is_up: true,
            network_signature: vec![1; 32],
        }
    }

    fn route() -> RouteSnapshot {
        RouteSnapshot {
            interface_index: 7,
            source: selected().address,
            next_hop: Ipv4Addr::UNSPECIFIED,
        }
    }

    fn lock() -> NetworkLock {
        NetworkLock::new(selected(), Ipv4Addr::new(192, 168, 50, 12), route(), false).unwrap()
    }

    #[test]
    fn debug_output_redacts_network_attachment_bytes() {
        let mut interface = selected();
        interface.network_signature = b"private-wifi-attachment".to_vec();
        let raw_debug = format!("{:?}", interface.network_signature);
        let output = format!("{interface:?}");
        assert!(output.contains("<redacted>"));
        assert!(!output.contains(&raw_debug));
        assert!(!output.contains("private-wifi-attachment"));
    }

    #[test]
    fn only_exact_private_peer_on_selected_interface_is_authorized() {
        let lock = lock();
        assert!(
            lock.authorize_packet(selected().address, lock.peer(), 7)
                .is_ok()
        );
        assert_eq!(
            lock.authorize_packet(selected().address, lock.peer(), 8),
            Err(PolicyError::WrongInterface)
        );
        assert_eq!(
            lock.authorize_packet(selected().address, Ipv4Addr::new(192, 168, 50, 13), 7),
            Err(PolicyError::OffLinkPeer)
        );
    }

    #[test]
    fn unknown_attachment_cannot_authorize_a_connection() {
        let mut interface = selected();
        for signature in [vec![], vec![0; 32], vec![1; 129]] {
            interface.network_signature = signature;
            assert_eq!(
                validate_interface(&interface),
                Err(PolicyError::UnknownAttachment)
            );
        }
    }

    #[test]
    fn off_subnet_and_wan_peers_are_rejected_before_binding() {
        for peer in [
            "8.8.8.8",
            "1.1.1.1",
            "127.0.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "100.64.0.1",
            "192.0.2.2",
        ] {
            assert_eq!(
                validate_peer(&selected(), peer.parse().unwrap()),
                Err(PolicyError::NonPrivateAddress)
            );
        }
        for peer in ["192.168.51.12", "10.0.0.2", "172.16.0.2"] {
            assert_eq!(
                validate_peer(&selected(), peer.parse().unwrap()),
                Err(PolicyError::OffLinkPeer)
            );
        }
        for peer in ["192.168.50.0", "192.168.50.255"] {
            assert_eq!(
                validate_peer(&selected(), peer.parse().unwrap()),
                Err(PolicyError::NetworkOrBroadcastAddress)
            );
        }
    }

    #[test]
    fn on_subnet_peer_with_gateway_is_still_rejected() {
        let route = RouteSnapshot {
            next_hop: Ipv4Addr::new(192, 168, 50, 1),
            ..route()
        };
        assert_eq!(
            NetworkLock::new(selected(), lock().peer(), route, false).unwrap_err(),
            PolicyError::RoutedPeer
        );
    }

    #[test]
    fn vpn_and_virtual_ethernet_adapters_are_not_physical() {
        let mut interface = selected();
        interface.is_hardware = false;
        assert_eq!(
            validate_interface(&interface),
            Err(PolicyError::NonPhysicalInterface)
        );
        interface.is_hardware = true;
        interface.kind = InterfaceKind::Other;
        assert_eq!(
            validate_interface(&interface),
            Err(PolicyError::NonPhysicalInterface)
        );
    }

    #[test]
    fn interface_changes_revoke_permanently_without_fallback() {
        let mut changed = selected();
        changed.index = 8;
        let mut lock = lock();
        assert_eq!(
            lock.revalidate(Some(&changed), route()),
            Err(PolicyError::InterfaceChanged)
        );
        assert_eq!(
            lock.revalidate(Some(&selected()), route()),
            Err(PolicyError::SessionClosed)
        );
        assert_eq!(
            lock.authorize_packet(selected().address, lock.peer(), 7),
            Err(PolicyError::SessionClosed)
        );
    }

    #[test]
    fn wifi_network_change_is_detected_even_with_same_ip() {
        let mut changed = selected();
        changed.network_signature[0] = 2;
        assert_eq!(
            lock().revalidate(Some(&changed), route()),
            Err(PolicyError::InterfaceChanged)
        );
        assert_eq!(
            lock().revalidate(None, route()),
            Err(PolicyError::InterfaceChanged)
        );
    }

    #[test]
    fn changed_gateway_or_egress_revokes_session() {
        let mut lock = lock();
        assert_eq!(
            lock.revalidate(
                Some(&selected()),
                RouteSnapshot {
                    interface_index: 88,
                    ..route()
                }
            ),
            Err(PolicyError::WrongInterface)
        );
        assert!(lock.is_revoked());
    }

    #[test]
    fn direct_cable_slash31_is_valid_but_slash32_is_not() {
        let interface = InterfaceSnapshot {
            prefix_len: 31,
            ..selected()
        };
        assert!(validate_peer(&interface, Ipv4Addr::new(192, 168, 50, 11)).is_ok());
        assert_eq!(
            validate_interface(&InterfaceSnapshot {
                prefix_len: 32,
                ..interface
            }),
            Err(PolicyError::InvalidSubnet)
        );
    }

    #[test]
    fn invalid_masks_never_panic_or_permit_wildcards() {
        for prefix_len in [0, 1, 7, 32, 33, 255] {
            assert_eq!(
                validate_interface(&InterfaceSnapshot {
                    prefix_len,
                    ..selected()
                }),
                Err(PolicyError::InvalidSubnet)
            );
        }
        assert_eq!(
            NetworkLock::new(selected(), lock().peer(), route(), true).unwrap_err(),
            PolicyError::DiscoveryDisabled
        );
    }
}
