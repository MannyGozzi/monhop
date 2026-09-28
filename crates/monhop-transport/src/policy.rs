//! Validation is required before socket creation and again whenever the interface changes.
use std::net::Ipv4Addr;

/// The most paired computers one pinned socket admits.
pub const MAX_PINNED_PEERS: usize = 7;

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
    InvalidPeerSet,
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
            Self::InvalidPeerSet => {
                "the paired peers must be 1 to 7 distinct addresses, each with its own route"
            }
        })
    }
}

impl std::error::Error for PolicyError {}

/// One selected interface and the fixed set of on-link peers it admits. Any peer's route failing
/// revokes the whole lock.
#[derive(Clone, Debug)]
pub struct NetworkLock {
    selected: InterfaceSnapshot,
    peers: Vec<Ipv4Addr>,
    revoked: bool,
}

impl NetworkLock {
    pub fn new(
        selected: InterfaceSnapshot,
        peer: Ipv4Addr,
        route: RouteSnapshot,
        allow_discovery: bool,
    ) -> Result<Self, PolicyError> {
        Self::new_group(selected, &[(peer, route)], allow_discovery)
    }

    /// `routes` names each peer with its current route; the peers keep this order.
    pub fn new_group(
        selected: InterfaceSnapshot,
        routes: &[(Ipv4Addr, RouteSnapshot)],
        allow_discovery: bool,
    ) -> Result<Self, PolicyError> {
        if allow_discovery {
            return Err(PolicyError::DiscoveryDisabled);
        }
        validate_interface(&selected)?;
        if routes.is_empty() || routes.len() > MAX_PINNED_PEERS {
            return Err(PolicyError::InvalidPeerSet);
        }
        for (index, (peer, route)) in routes.iter().enumerate() {
            if routes[..index].iter().any(|(earlier, _)| earlier == peer) {
                return Err(PolicyError::InvalidPeerSet);
            }
            validate_peer(&selected, *peer)?;
            validate_route(&selected, *route)?;
        }
        Ok(Self {
            selected,
            peers: routes.iter().map(|(peer, _)| *peer).collect(),
            revoked: false,
        })
    }

    pub fn selected(&self) -> &InterfaceSnapshot {
        &self.selected
    }

    pub fn peers(&self) -> &[Ipv4Addr] {
        &self.peers
    }

    pub fn is_revoked(&self) -> bool {
        self.revoked
    }

    /// Revocation is sticky: restoring the old address must not silently resume input forwarding.
    /// `routes` must name every peer, in order, with its current route.
    pub fn revalidate(
        &mut self,
        current: Option<&InterfaceSnapshot>,
        routes: &[(Ipv4Addr, RouteSnapshot)],
    ) -> Result<(), PolicyError> {
        if self.revoked {
            return Err(PolicyError::SessionClosed);
        }
        let result = match current {
            Some(current) if current == &self.selected => self.validate_routes(current, routes),
            _ => Err(PolicyError::InterfaceChanged),
        };
        if result.is_err() {
            self.revoked = true;
        }
        result
    }

    fn validate_routes(
        &self,
        current: &InterfaceSnapshot,
        routes: &[(Ipv4Addr, RouteSnapshot)],
    ) -> Result<(), PolicyError> {
        if routes.len() != self.peers.len()
            || routes
                .iter()
                .zip(&self.peers)
                .any(|((peer, _), pinned)| peer != pinned)
        {
            return Err(PolicyError::InvalidPeerSet);
        }
        routes
            .iter()
            .try_for_each(|(_, route)| validate_route(current, *route))
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
        if !self.peers.contains(&source) {
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

    const PEER: Ipv4Addr = Ipv4Addr::new(192, 168, 50, 12);

    fn lock() -> NetworkLock {
        NetworkLock::new(selected(), PEER, route(), false).unwrap()
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
        assert!(lock.authorize_packet(selected().address, PEER, 7).is_ok());
        assert_eq!(
            lock.authorize_packet(selected().address, PEER, 8),
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
            NetworkLock::new(selected(), PEER, route, false).unwrap_err(),
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
            lock.revalidate(Some(&changed), &[(PEER, route())]),
            Err(PolicyError::InterfaceChanged)
        );
        assert_eq!(
            lock.revalidate(Some(&selected()), &[(PEER, route())]),
            Err(PolicyError::SessionClosed)
        );
        assert_eq!(
            lock.authorize_packet(selected().address, PEER, 7),
            Err(PolicyError::SessionClosed)
        );
    }

    #[test]
    fn wifi_network_change_is_detected_even_with_same_ip() {
        let mut changed = selected();
        changed.network_signature[0] = 2;
        assert_eq!(
            lock().revalidate(Some(&changed), &[(PEER, route())]),
            Err(PolicyError::InterfaceChanged)
        );
        assert_eq!(
            lock().revalidate(None, &[(PEER, route())]),
            Err(PolicyError::InterfaceChanged)
        );
    }

    #[test]
    fn changed_gateway_or_egress_revokes_session() {
        let mut lock = lock();
        assert_eq!(
            lock.revalidate(
                Some(&selected()),
                &[(
                    PEER,
                    RouteSnapshot {
                        interface_index: 88,
                        ..route()
                    }
                )]
            ),
            Err(PolicyError::WrongInterface)
        );
        assert!(lock.is_revoked());
    }

    const MEMBERS: [Ipv4Addr; 3] = [
        Ipv4Addr::new(192, 168, 50, 11),
        Ipv4Addr::new(192, 168, 50, 12),
        Ipv4Addr::new(192, 168, 50, 13),
    ];

    fn member_routes() -> Vec<(Ipv4Addr, RouteSnapshot)> {
        MEMBERS.iter().map(|member| (*member, route())).collect()
    }

    #[test]
    fn every_member_route_is_validated_and_any_change_revokes() {
        let group = NetworkLock::new_group(selected(), &member_routes(), false).unwrap();
        assert_eq!(group.peers(), MEMBERS);
        for member in MEMBERS {
            assert!(
                group
                    .authorize_packet(selected().address, member, 7)
                    .is_ok()
            );
            assert_eq!(
                group.authorize_packet(selected().address, member, 8),
                Err(PolicyError::WrongInterface)
            );
        }
        for stranger in ["192.168.50.14", "192.168.50.10", "10.0.0.2"] {
            assert_eq!(
                group.authorize_packet(selected().address, stranger.parse().unwrap(), 7),
                Err(PolicyError::OffLinkPeer)
            );
        }
        let mut unchanged = group.clone();
        assert!(
            unchanged
                .revalidate(Some(&selected()), &member_routes())
                .is_ok()
        );
        assert!(!unchanged.is_revoked());

        let gateway = RouteSnapshot {
            next_hop: Ipv4Addr::new(192, 168, 50, 1),
            ..route()
        };
        let other_interface = RouteSnapshot {
            interface_index: 9,
            ..route()
        };
        for (member, bad, error) in [
            (0, gateway, PolicyError::RoutedPeer),
            (2, gateway, PolicyError::RoutedPeer),
            (1, other_interface, PolicyError::WrongInterface),
        ] {
            let mut lock = group.clone();
            let mut routes = member_routes();
            routes[member].1 = bad;
            assert_eq!(lock.revalidate(Some(&selected()), &routes), Err(error));
            assert!(lock.is_revoked());
            assert_eq!(
                lock.authorize_packet(selected().address, MEMBERS[0], 7),
                Err(PolicyError::SessionClosed)
            );
            assert_eq!(
                lock.revalidate(Some(&selected()), &member_routes()),
                Err(PolicyError::SessionClosed)
            );
        }

        let mut reordered = member_routes();
        reordered.swap(0, 2);
        let mut substituted = member_routes();
        substituted[1].0 = Ipv4Addr::new(192, 168, 50, 14);
        for routes in [
            member_routes()[..2].to_vec(),
            [
                member_routes(),
                vec![(Ipv4Addr::new(192, 168, 50, 14), route())],
            ]
            .concat(),
            reordered,
            substituted,
            Vec::new(),
        ] {
            let mut lock = group.clone();
            assert_eq!(
                lock.revalidate(Some(&selected()), &routes),
                Err(PolicyError::InvalidPeerSet)
            );
            assert!(lock.is_revoked());
        }

        let mut moved = group.clone();
        let mut other_network = selected();
        other_network.network_signature = vec![2; 32];
        assert_eq!(
            moved.revalidate(Some(&other_network), &member_routes()),
            Err(PolicyError::InterfaceChanged)
        );
        assert!(moved.is_revoked());
    }

    #[test]
    fn a_member_set_is_one_to_seven_distinct_on_link_private_peers_with_direct_routes() {
        assert_eq!(
            NetworkLock::new_group(selected(), &[], false).unwrap_err(),
            PolicyError::InvalidPeerSet
        );
        let seven: Vec<_> = (11..18)
            .map(|host| (Ipv4Addr::new(192, 168, 50, host), route()))
            .collect();
        assert!(NetworkLock::new_group(selected(), &seven, false).is_ok());
        let eight: Vec<_> = (11..19)
            .map(|host| (Ipv4Addr::new(192, 168, 50, host), route()))
            .collect();
        assert_eq!(
            NetworkLock::new_group(selected(), &eight, false).unwrap_err(),
            PolicyError::InvalidPeerSet
        );
        let mut duplicate = member_routes();
        duplicate[2].0 = MEMBERS[0];
        assert_eq!(
            NetworkLock::new_group(selected(), &duplicate, false).unwrap_err(),
            PolicyError::InvalidPeerSet
        );
        for (stranger, error) in [
            ("192.168.51.12", PolicyError::OffLinkPeer),
            ("8.8.8.8", PolicyError::NonPrivateAddress),
            ("192.168.50.10", PolicyError::PeerIsLocal),
            ("192.168.50.255", PolicyError::NetworkOrBroadcastAddress),
        ] {
            let mut routes = member_routes();
            routes[1].0 = stranger.parse().unwrap();
            assert_eq!(
                NetworkLock::new_group(selected(), &routes, false).unwrap_err(),
                error
            );
        }
        let mut routed = member_routes();
        routed[2].1.next_hop = Ipv4Addr::new(192, 168, 50, 1);
        assert_eq!(
            NetworkLock::new_group(selected(), &routed, false).unwrap_err(),
            PolicyError::RoutedPeer
        );
        assert_eq!(
            NetworkLock::new_group(selected(), &member_routes(), true).unwrap_err(),
            PolicyError::DiscoveryDisabled
        );
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
            NetworkLock::new(selected(), PEER, route(), true).unwrap_err(),
            PolicyError::DiscoveryDisabled
        );
    }
}
