//! Preserve each OS's monitor adjacency; caller-provided links join computers only.
//! A hidden display (one marked not in use) keeps no adjacency: nothing routes the pointer onto it.

use monhop_core::{
    DeviceId, Display, DisplayId, Edge, EdgeLink, LogicalSize, Machine, NativeSize, NormalizedSpan,
    Platform, Point, Topology,
};
use monhop_protocol::DisplayTopology;

pub(crate) fn inherit_display_edges(
    displays: &[Display],
    mut seams: Vec<EdgeLink>,
    hidden: &[DisplayId],
) -> Result<Vec<EdgeLink>, ()> {
    for seam in &seams {
        let from = displays
            .iter()
            .find(|d| d.id == seam.from_display)
            .ok_or(())?;
        let to = displays
            .iter()
            .find(|d| d.id == seam.to_display)
            .ok_or(())?;
        if from.machine == to.machine {
            return Err(());
        }
    }
    let placed_at = |d: &Display| Placed {
        id: d.id,
        origin: d.origin,
        size: d.logical_size,
    };
    for (index, a) in displays.iter().enumerate() {
        for b in &displays[index + 1..] {
            if a.machine != b.machine || hidden.contains(&a.id) || hidden.contains(&b.id) {
                continue;
            }
            let (a, b) = (placed_at(a), placed_at(b));
            let ax = a.origin.x + a.size.width;
            let ay = a.origin.y + a.size.height;
            let bx = b.origin.x + b.size.width;
            let by = b.origin.y + b.size.height;
            let horizontal = (a.origin.x.max(b.origin.x), ax.min(bx));
            let vertical = (a.origin.y.max(b.origin.y), ay.min(by));
            if horizontal.0 < horizontal.1 && vertical.0 < vertical.1 {
                return Err(());
            }
            if vertical.0 < vertical.1 {
                if ax == b.origin.x {
                    add_pair(&mut seams, &a, &b, Edge::Right, Edge::Left, vertical)?;
                } else if bx == a.origin.x {
                    add_pair(&mut seams, &a, &b, Edge::Left, Edge::Right, vertical)?;
                }
            }
            if horizontal.0 < horizontal.1 {
                if ay == b.origin.y {
                    add_pair(&mut seams, &a, &b, Edge::Bottom, Edge::Top, horizontal)?;
                } else if by == a.origin.y {
                    add_pair(&mut seams, &a, &b, Edge::Top, Edge::Bottom, horizontal)?;
                }
            }
        }
    }
    Ok(seams)
}

/// One member's displays and where its block sits in the shared coordinate space. The caller's
/// own block conventionally carries `offset` zero; every other member's is translated to it.
pub struct MemberBlock<'a> {
    pub device: DeviceId,
    pub platform: Platform,
    pub displays: &'a DisplayTopology,
    pub offset: Point,
}

/// Why a group of member blocks could not become one whole-group topology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupTopologyError {
    /// The caller's explicit links already exceed the per-topology bound.
    TooManyLinks,
    /// A block's offset carries a non-finite coordinate.
    NonFiniteOffset,
    /// A hidden display id names no member's display.
    UnknownHiddenDisplay,
    /// Seam inheritance, an explicit crossing on a member's own seam, span overlap, or final
    /// topology construction failed.
    Layout,
}

/// Builds the whole-group pointer topology: each member's displays translated by its own offset,
/// each member's own OS-reported seams inherited independently (an explicit link crossing one is
/// rejected), and the caller's explicit links joining members only.
pub fn group_topology(
    blocks: &[MemberBlock],
    links: Vec<EdgeLink>,
    hidden: &[DisplayId],
) -> Result<Topology, GroupTopologyError> {
    if links.len() > 64 {
        return Err(GroupTopologyError::TooManyLinks);
    }
    if blocks.iter().any(|block| !block.offset.is_finite()) {
        return Err(GroupTopologyError::NonFiniteOffset);
    }
    let displays: Vec<_> = blocks
        .iter()
        .flat_map(|block| {
            block.displays.displays().iter().map(move |display| {
                Display::new(
                    display.id,
                    block.device,
                    display.name.clone(),
                    NativeSize::new(display.native_width, display.native_height),
                    LogicalSize::new(display.logical_size.x, display.logical_size.y),
                    Point::new(
                        display.logical_origin.x + block.offset.x,
                        display.logical_origin.y + block.offset.y,
                    ),
                    f64::from(display.scale_factor),
                    None,
                    display.is_primary,
                )
                .with_in_use(!hidden.contains(&display.id))
            })
        })
        .collect();
    if hidden
        .iter()
        .any(|id| !displays.iter().any(|display| display.id == *id))
    {
        return Err(GroupTopologyError::UnknownHiddenDisplay);
    }
    let links =
        inherit_display_edges(&displays, links, hidden).map_err(|()| GroupTopologyError::Layout)?;
    if links.len() > 64 {
        return Err(GroupTopologyError::TooManyLinks);
    }
    let machines = blocks
        .iter()
        .map(|block| Machine::new(block.device, block.platform))
        .collect();
    Topology::new(machines, displays, links).map_err(|_| GroupTopologyError::Layout)
}

/// A display's rectangle for adjacency: its OS origin, or the picture's when the picture rules.
struct Placed {
    id: DisplayId,
    origin: Point,
    size: monhop_core::LogicalSize,
}

fn add_pair(
    links: &mut Vec<EdgeLink>,
    a: &Placed,
    b: &Placed,
    from_edge: Edge,
    to_edge: Edge,
    overlap: (f64, f64),
) -> Result<(), ()> {
    let span = |d: &Placed| {
        let (origin, size) = match from_edge {
            Edge::Left | Edge::Right => (d.origin.y, d.size.height),
            Edge::Top | Edge::Bottom => (d.origin.x, d.size.width),
        };
        NormalizedSpan::new((overlap.0 - origin) / size, (overlap.1 - origin) / size)
            .map_err(|_| ())
    };
    let from_span = span(a)?;
    let to_span = span(b)?;
    links.push(
        EdgeLink::new(a.id, from_edge, from_span, b.id, to_edge, to_span, 1.0).map_err(|_| ())?,
    );
    links.push(
        EdgeLink::new(b.id, to_edge, to_span, a.id, from_edge, from_span, 1.0).map_err(|_| ())?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_core::{DeviceId, DisplayId, LogicalSize, NativeSize, Point};

    fn display(id: u64, computer: u8, x: f64, y: f64, width: f64, height: f64) -> Display {
        Display::new(
            DisplayId(id),
            DeviceId([computer; 16]),
            "Monitor".into(),
            NativeSize::new(width as u32, height as u32),
            LogicalSize::new(width, height),
            Point::new(x, y),
            1.0,
            None,
            false,
        )
    }

    #[test]
    fn inherits_stacked_and_offset_monitors_without_changing_geometry() {
        let monitors = vec![
            display(1, 1, -100.0, -100.0, 200.0, 100.0),
            display(2, 1, 0.0, 0.0, 100.0, 100.0),
            display(3, 2, 0.0, 0.0, 100.0, 100.0),
        ];
        let original = monitors.clone();
        let links = inherit_display_edges(&monitors, vec![], &[]).unwrap();
        assert_eq!(monitors, original);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].from_display, DisplayId(1));
        assert_eq!(links[0].from_edge, Edge::Bottom);
        assert_eq!(links[0].from_span, NormalizedSpan::new(0.5, 1.0).unwrap());
        assert_eq!(links[0].to_span, NormalizedSpan::new(0.0, 1.0).unwrap());
        assert_eq!(links[1].to_display, DisplayId(1));
    }

    #[test]
    fn gaps_corners_and_cross_computer_overlap_do_not_invent_routes() {
        let monitors = vec![
            display(1, 1, 0.0, 0.0, 100.0, 100.0),
            display(2, 1, 100.0, 100.0, 100.0, 100.0),
            display(3, 2, 0.0, 0.0, 100.0, 100.0),
        ];
        assert!(
            inherit_display_edges(&monitors, vec![], &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_hidden_display_keeps_no_adjacency() {
        let monitors = vec![
            display(1, 1, 0.0, 0.0, 100.0, 100.0),
            display(2, 1, 100.0, 0.0, 100.0, 100.0),
            display(3, 1, 200.0, 0.0, 100.0, 100.0),
        ];
        let links = inherit_display_edges(&monitors, vec![], &[DisplayId(2)]).unwrap();
        assert!(links.is_empty());
        let links = inherit_display_edges(&monitors, vec![], &[DisplayId(3)]).unwrap();
        assert_eq!(links.len(), 2);
        assert!(links.iter().all(|link| link.to_display != DisplayId(3)));
    }

    #[test]
    fn refuses_user_overrides_of_inherited_routes_and_overlapping_local_monitors() {
        let monitors = vec![
            display(1, 1, 0.0, 0.0, 100.0, 100.0),
            display(2, 1, 100.0, 0.0, 100.0, 100.0),
        ];
        let mut links = inherit_display_edges(&monitors, vec![], &[]).unwrap();
        assert!(inherit_display_edges(&monitors, vec![links.remove(0)], &[]).is_err());
        let overlap = vec![monitors[0].clone(), display(2, 1, 50.0, 0.0, 100.0, 100.0)];
        assert!(inherit_display_edges(&overlap, vec![], &[]).is_err());
    }
}

#[cfg(test)]
mod group_tests {
    use super::*;
    use crate::{crypto::CertificateFingerprint, session_setup::InspectedPeer};
    use monhop_protocol::DisplayDescription;

    fn description(
        id: u64,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        name: &str,
    ) -> DisplayDescription {
        DisplayDescription {
            id: DisplayId(id),
            name: name.into(),
            native_width: width as u32,
            native_height: height as u32,
            logical_origin: Point::new(x, y),
            logical_size: Point::new(width, height),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }
    }

    /// Exactly one primary is required; the fixture always makes it the first display.
    fn topology_of(mut displays: Vec<DisplayDescription>) -> DisplayTopology {
        for (index, display) in displays.iter_mut().enumerate() {
            display.is_primary = index == 0;
        }
        DisplayTopology::new(displays).unwrap()
    }

    fn block<'a>(
        device: DeviceId,
        platform: Platform,
        displays: &'a DisplayTopology,
        offset: Point,
    ) -> MemberBlock<'a> {
        MemberBlock {
            device,
            platform,
            displays,
            offset,
        }
    }

    fn straight_link(from: DisplayId, from_edge: Edge, to: DisplayId, to_edge: Edge) -> EdgeLink {
        let span = NormalizedSpan::new(0.0, 1.0).unwrap();
        EdgeLink::new(from, from_edge, span, to, to_edge, span, 1.0).unwrap()
    }

    /// Every display, its machine, and every link, ignoring only cosmetic display order.
    fn assert_topologies_match(a: &Topology, b: &Topology) {
        let a_displays: Vec<_> = a.displays().cloned().collect();
        let b_displays: Vec<_> = b.displays().cloned().collect();
        assert_eq!(a_displays, b_displays);
        assert_eq!(a.links(), b.links());
        for display in &a_displays {
            assert_eq!(
                a.machine_for_display(display.id).unwrap(),
                b.machine_for_display(display.id).unwrap()
            );
        }
    }

    #[test]
    fn three_machines_inherit_each_own_seams_and_take_explicit_crossings() {
        let a = topology_of(vec![description(1, 0.0, 0.0, 100.0, 100.0, "A1")]);
        let b = topology_of(vec![
            description(2, 0.0, 0.0, 100.0, 100.0, "B1"),
            description(3, 100.0, 0.0, 100.0, 100.0, "B2"),
        ]);
        let c = topology_of(vec![description(4, 0.0, 0.0, 100.0, 100.0, "C1")]);
        let blocks = vec![
            block(DeviceId([1; 16]), Platform::Windows, &a, Point::default()),
            block(
                DeviceId([2; 16]),
                Platform::MacOs,
                &b,
                Point::new(500.0, 0.0),
            ),
            block(
                DeviceId([3; 16]),
                Platform::Windows,
                &c,
                Point::new(1_000.0, 0.0),
            ),
        ];
        let crossing = straight_link(DisplayId(3), Edge::Right, DisplayId(4), Edge::Left);
        let topology = group_topology(&blocks, vec![crossing], &[]).unwrap();
        // B's own seam (B1 <-> B2) is inherited without being named explicitly.
        assert!(topology.links().iter().any(|link| {
            (link.from_display, link.to_display) == (DisplayId(2), DisplayId(3))
                || (link.from_display, link.to_display) == (DisplayId(3), DisplayId(2))
        }));
        // The explicit B->C crossing survives alongside it.
        assert!(
            topology.links().iter().any(|link| {
                (link.from_display, link.to_display) == (DisplayId(3), DisplayId(4))
            })
        );
        // A has no seam to inherit and took no explicit crossing: it stays isolated.
        assert!(
            topology
                .links()
                .iter()
                .all(|link| link.from_display != DisplayId(1) && link.to_display != DisplayId(1))
        );
    }

    #[test]
    fn an_explicit_crossing_on_any_members_own_seam_is_rejected() {
        let a = topology_of(vec![
            description(1, 0.0, 0.0, 100.0, 100.0, "A1"),
            description(2, 100.0, 0.0, 100.0, 100.0, "A2"),
        ]);
        let b = topology_of(vec![description(3, 0.0, 0.0, 100.0, 100.0, "B1")]);
        let c = topology_of(vec![description(4, 0.0, 0.0, 100.0, 100.0, "C1")]);
        let blocks = vec![
            block(DeviceId([1; 16]), Platform::Windows, &a, Point::default()),
            block(
                DeviceId([2; 16]),
                Platform::MacOs,
                &b,
                Point::new(500.0, 0.0),
            ),
            block(
                DeviceId([3; 16]),
                Platform::Windows,
                &c,
                Point::new(1_000.0, 0.0),
            ),
        ];
        // A1 <-> A2 is A's own seam: an explicit crossing naming it is rejected, whichever
        // member proposed it.
        let own_seam = straight_link(DisplayId(1), Edge::Right, DisplayId(2), Edge::Left);
        assert_eq!(
            group_topology(&blocks, vec![own_seam], &[]).unwrap_err(),
            GroupTopologyError::Layout
        );
        let cross_member = straight_link(DisplayId(3), Edge::Right, DisplayId(4), Edge::Left);
        assert!(group_topology(&blocks, vec![cross_member], &[]).is_ok());
    }

    #[test]
    fn each_block_is_translated_by_its_own_offset() {
        let a = topology_of(vec![description(1, 0.0, 0.0, 100.0, 100.0, "A1")]);
        let b = topology_of(vec![description(2, 0.0, 0.0, 100.0, 100.0, "B1")]);
        let blocks = vec![
            block(
                DeviceId([1; 16]),
                Platform::Windows,
                &a,
                Point::new(10.0, 20.0),
            ),
            block(
                DeviceId([2; 16]),
                Platform::MacOs,
                &b,
                Point::new(-30.0, 40.0),
            ),
        ];
        let topology = group_topology(&blocks, vec![], &[]).unwrap();
        assert_eq!(
            topology.display(DisplayId(1)).unwrap().origin,
            Point::new(10.0, 20.0)
        );
        assert_eq!(
            topology.display(DisplayId(2)).unwrap().origin,
            Point::new(-30.0, 40.0)
        );
    }

    #[test]
    fn two_blocks_match_outbound_topology() {
        let local = topology_of(vec![
            description(1, 0.0, 0.0, 100.0, 100.0, "L1"),
            description(2, 100.0, 0.0, 100.0, 100.0, "L2"),
        ]);
        let peer = topology_of(vec![description(3, 0.0, 0.0, 100.0, 100.0, "P1")]);
        let inspected = InspectedPeer {
            local_device: DeviceId([1; 16]),
            peer_device: DeviceId([2; 16]),
            local_fingerprint: CertificateFingerprint::from_certificate_der(b"fixture-local"),
            peer_fingerprint: CertificateFingerprint::from_certificate_der(b"fixture-peer"),
            local_platform: Platform::Windows,
            peer_platform: Platform::MacOs,
            local_displays: local.clone(),
            peer_displays: peer.clone(),
            interface_id: "fixture".into(),
        };
        let peer_offset = Point::new(500.0, 0.0);
        let hidden = [DisplayId(2)];
        let crossing = straight_link(DisplayId(1), Edge::Right, DisplayId(3), Edge::Left);
        let via_peer = inspected
            .outbound_topology(vec![crossing], &hidden, peer_offset)
            .unwrap();
        let blocks = vec![
            block(
                inspected.local_device,
                inspected.local_platform,
                &local,
                Point::default(),
            ),
            block(
                inspected.peer_device,
                inspected.peer_platform,
                &peer,
                peer_offset,
            ),
        ];
        let via_group = group_topology(&blocks, vec![crossing], &hidden).unwrap();
        assert_topologies_match(&via_peer, &via_group);
    }

    #[test]
    fn more_than_sixteen_displays_or_links_past_64_are_rejected() {
        let members: Vec<DisplayTopology> = (0..17u64)
            .map(|id| {
                topology_of(vec![description(
                    id,
                    id as f64 * 200.0,
                    0.0,
                    100.0,
                    100.0,
                    "D",
                )])
            })
            .collect();
        let blocks: Vec<MemberBlock> = members
            .iter()
            .enumerate()
            .map(|(index, displays)| {
                block(
                    DeviceId([index as u8 + 1; 16]),
                    Platform::Windows,
                    displays,
                    Point::new(index as f64 * 200.0, 0.0),
                )
            })
            .collect();
        assert_eq!(
            group_topology(&blocks, vec![], &[]).unwrap_err(),
            GroupTopologyError::Layout
        );

        let a = topology_of(vec![description(100, 0.0, 0.0, 100.0, 100.0, "A")]);
        let b = topology_of(vec![description(101, 500.0, 0.0, 100.0, 100.0, "B")]);
        let two_blocks = vec![
            block(DeviceId([1; 16]), Platform::Windows, &a, Point::default()),
            block(
                DeviceId([2; 16]),
                Platform::MacOs,
                &b,
                Point::new(500.0, 0.0),
            ),
        ];
        let one_link = straight_link(DisplayId(100), Edge::Right, DisplayId(101), Edge::Left);
        let too_many_links = vec![one_link; 65];
        assert_eq!(
            group_topology(&two_blocks, too_many_links, &[]).unwrap_err(),
            GroupTopologyError::TooManyLinks
        );
    }
}
