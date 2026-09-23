//! Preserve each OS's monitor adjacency; caller-provided links join computers only.
//! A hidden display (one marked not in use) keeps no adjacency: nothing routes the pointer onto it.

use monhop_core::{Display, DisplayId, Edge, EdgeLink, NormalizedSpan, Point};

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
