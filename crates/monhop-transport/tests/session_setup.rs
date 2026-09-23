use monhop_core::{DisplayId, Platform, Point};
use monhop_protocol::{DisplayDescription, DisplayTopology};
use monhop_transport::{
    crypto::DeviceIdentity, session_handshake::device_id_from_fingerprint,
    session_setup::InspectedPeer,
};

fn topology(display_id: u64) -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(display_id),
        name: "loopback display".to_owned(),
        native_width: 1_920,
        native_height: 1_080,
        logical_origin: Point::new(0.0, 0.0),
        logical_size: Point::new(1_920.0, 1_080.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }])
    .expect("valid generated topology")
}

#[test]
fn inspection_recheck_requires_the_same_route() {
    let local_identity = DeviceIdentity::generate().expect("local identity");
    let peer_identity = DeviceIdentity::generate().expect("peer identity");
    let local_device = device_id_from_fingerprint(local_identity.fingerprint());
    let peer_device = device_id_from_fingerprint(peer_identity.fingerprint());
    let inspection = InspectedPeer {
        local_device,
        peer_device,
        local_fingerprint: local_identity.fingerprint(),
        peer_fingerprint: peer_identity.fingerprint(),
        local_platform: Platform::Windows,
        peer_platform: Platform::MacOs,
        local_displays: topology(1),
        peer_displays: topology(2),
        interface_id: "loopback-route".to_owned(),
    };
    let mut changed_route = inspection.clone();
    changed_route.interface_id = "changed-route".into();

    assert!(inspection.matches(&inspection));
    assert!(!inspection.matches(&changed_route));
}

#[test]
fn outbound_topology_preserves_local_geometry_and_translates_the_peer_block() {
    use monhop_core::{Edge, EdgeLink, NormalizedSpan};
    let a = DeviceIdentity::generate().unwrap();
    let b = DeviceIdentity::generate().unwrap();
    let inspection = InspectedPeer {
        local_device: device_id_from_fingerprint(a.fingerprint()),
        peer_device: device_id_from_fingerprint(b.fingerprint()),
        local_fingerprint: a.fingerprint(),
        peer_fingerprint: b.fingerprint(),
        local_platform: Platform::MacOs,
        peer_platform: Platform::Windows,
        local_displays: topology(1),
        peer_displays: topology(2),
        interface_id: "fixture".into(),
    };
    let span = NormalizedSpan::new(0.0, 1.0).unwrap();
    let links = vec![
        EdgeLink::new(
            DisplayId(1),
            Edge::Right,
            span,
            DisplayId(2),
            Edge::Left,
            span,
            1.0,
        )
        .unwrap(),
        EdgeLink::new(
            DisplayId(2),
            Edge::Left,
            span,
            DisplayId(1),
            Edge::Right,
            span,
            1.0,
        )
        .unwrap(),
    ];
    let placed = inspection
        .outbound_topology(links.clone(), &[], Point::new(1920.0, -10.0))
        .unwrap();
    assert_eq!(
        placed.display(DisplayId(1)).unwrap().origin,
        Point::default()
    );
    assert_eq!(
        placed.display(DisplayId(2)).unwrap().origin,
        Point::new(1920.0, -10.0)
    );
    assert!(
        inspection
            .outbound_topology(links, &[], Point::new(f64::NAN, 0.0))
            .is_err()
    );
    assert!(
        inspection
            .outbound_topology(vec![], &[DisplayId(99)], Point::default())
            .is_err()
    );
}
