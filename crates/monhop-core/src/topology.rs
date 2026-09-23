//! Validated display geometry and directed pointer-edge transitions.

use std::collections::BTreeMap;

use crate::{DeviceId, DisplayId, Platform, Point};

/// The maximum number of displays in one configured topology.
pub const MAX_DISPLAYS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Machine {
    pub id: DeviceId,
    pub platform: Platform,
}

impl Machine {
    pub const fn new(id: DeviceId, platform: Platform) -> Self {
        Self { id, platform }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSize {
    pub width: u32,
    pub height: u32,
}

impl NativeSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalSize {
    pub width: f64,
    pub height: f64,
}

impl LogicalSize {
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalRect {
    pub origin: Point,
    pub size: LogicalSize,
}

impl LogicalRect {
    pub fn max_x(self) -> f64 {
        self.origin.x + self.size.width
    }

    pub fn max_y(self) -> f64 {
        self.origin.y + self.size.height
    }

    pub fn contains(self, point: Point) -> bool {
        point.is_finite()
            && point.x >= self.origin.x
            && point.x <= self.max_x()
            && point.y >= self.origin.y
            && point.y <= self.max_y()
    }

    /// Excludes the max edges so abutting displays never both claim the same point.
    pub fn contains_half_open(self, point: Point) -> bool {
        point.is_finite()
            && point.x >= self.origin.x
            && point.x < self.max_x()
            && point.y >= self.origin.y
            && point.y < self.max_y()
    }

    /// The nearest point [`Self::contains_half_open`] admits; NaN stays NaN. The rect must not be
    /// empty.
    pub fn clamp_half_open(self, point: Point) -> Point {
        Point::new(
            point.x.clamp(self.origin.x, self.max_x().next_down()),
            point.y.clamp(self.origin.y, self.max_y().next_down()),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub id: DisplayId,
    pub machine: DeviceId,
    pub name: String,
    pub native_size: NativeSize,
    pub logical_size: LogicalSize,
    pub origin: Point,
    pub scale_factor: f64,
    pub refresh_rate_hz: Option<f64>,
    pub primary: bool,
    pub monitor: Option<crate::MonitorIdentity>,
    /// False for a display the layout leaves out: it is never the pointer's display.
    pub in_use: bool,
}

impl Display {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DisplayId,
        machine: DeviceId,
        name: String,
        native_size: NativeSize,
        logical_size: LogicalSize,
        origin: Point,
        scale_factor: f64,
        refresh_rate_hz: Option<f64>,
        primary: bool,
    ) -> Self {
        Self {
            id,
            machine,
            name,
            native_size,
            logical_size,
            origin,
            scale_factor,
            refresh_rate_hz,
            primary,
            monitor: None,
            in_use: true,
        }
    }

    pub fn with_monitor(mut self, monitor: Option<crate::MonitorIdentity>) -> Self {
        self.monitor = monitor;
        self
    }

    pub fn with_in_use(mut self, in_use: bool) -> Self {
        self.in_use = in_use;
        self
    }

    pub const fn bounds(&self) -> LogicalRect {
        LogicalRect {
            origin: self.origin,
            size: self.logical_size,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn normal_extent(self, bounds: LogicalRect) -> f64 {
        match self {
            Self::Left | Self::Right => bounds.size.width,
            Self::Top | Self::Bottom => bounds.size.height,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NormalizedSpan {
    start: f64,
    end: f64,
}

impl NormalizedSpan {
    pub fn new(start: f64, end: f64) -> Result<Self, TopologyError> {
        if !start.is_finite()
            || !end.is_finite()
            || !(0.0..=1.0).contains(&start)
            || !(0.0..=1.0).contains(&end)
            || start >= end
        {
            return Err(TopologyError::InvalidNormalizedSpan { start, end });
        }

        Ok(Self { start, end })
    }

    pub const fn start(self) -> f64 {
        self.start
    }

    pub const fn end(self) -> f64 {
        self.end
    }

    fn contains(self, position: f64) -> bool {
        position >= self.start && (position < self.end || self.end == 1.0 && position == 1.0)
    }

    fn map_to(self, destination: Self, position: f64) -> f64 {
        let progress = (position - self.start) / (self.end - self.start);
        destination.start + progress * (destination.end - destination.start)
    }

    fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeLink {
    pub from_display: DisplayId,
    pub from_edge: Edge,
    pub from_span: NormalizedSpan,
    pub to_display: DisplayId,
    pub to_edge: Edge,
    pub to_span: NormalizedSpan,
    pub hysteresis: f64,
}

impl EdgeLink {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        from_display: DisplayId,
        from_edge: Edge,
        from_span: NormalizedSpan,
        to_display: DisplayId,
        to_edge: Edge,
        to_span: NormalizedSpan,
        hysteresis: f64,
    ) -> Result<Self, TopologyError> {
        validate_link_shape(from_display, to_display, hysteresis)?;

        Ok(Self {
            from_display,
            from_edge,
            from_span,
            to_display,
            to_edge,
            to_span,
            hysteresis,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeTransition {
    pub from_display: DisplayId,
    pub to_display: DisplayId,
    pub from_edge: Edge,
    pub to_edge: Edge,
    pub crossing_point: Point,
    pub entry_point: Point,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TopologyError {
    EmptyTopology,
    TooManyDisplays {
        count: usize,
    },
    DuplicateMachine {
        machine: DeviceId,
    },
    DuplicateDisplay {
        display: DisplayId,
    },
    UnknownMachine {
        machine: DeviceId,
    },
    UnknownDisplay {
        display: DisplayId,
    },
    InvalidNativeSize {
        display: DisplayId,
    },
    InvalidLogicalSize {
        display: DisplayId,
    },
    InvalidOrigin {
        display: DisplayId,
    },
    InvalidScaleFactor {
        display: DisplayId,
    },
    InvalidRefreshRate {
        display: DisplayId,
    },
    InvalidNormalizedSpan {
        start: f64,
        end: f64,
    },
    SameDisplayLink {
        from_display: DisplayId,
    },
    InvalidHysteresis {
        hysteresis: f64,
    },
    HysteresisExceedsDisplay {
        from_display: DisplayId,
        to_display: DisplayId,
    },
    OverlappingSourceLinks {
        display: DisplayId,
        edge: Edge,
    },
    NonFiniteMotionPoint,
}

#[derive(Clone, Debug)]
pub struct Topology {
    machines: BTreeMap<DeviceId, Machine>,
    displays: BTreeMap<DisplayId, Display>,
    links: Vec<EdgeLink>,
    /// Every motion sample resolves against one source display, so its links are indexed here in
    /// the order `links` holds them: first match still wins.
    links_by_source: BTreeMap<DisplayId, Vec<EdgeLink>>,
}

impl Topology {
    pub fn new(
        machines: Vec<Machine>,
        displays: Vec<Display>,
        links: Vec<EdgeLink>,
    ) -> Result<Self, TopologyError> {
        if displays.is_empty() {
            return Err(TopologyError::EmptyTopology);
        }
        if displays.len() > MAX_DISPLAYS {
            return Err(TopologyError::TooManyDisplays {
                count: displays.len(),
            });
        }

        let mut machine_map = BTreeMap::new();
        for machine in machines {
            if machine_map.insert(machine.id, machine).is_some() {
                return Err(TopologyError::DuplicateMachine {
                    machine: machine.id,
                });
            }
        }

        let mut display_map = BTreeMap::new();
        for display in displays {
            validate_display(&display)?;
            if !machine_map.contains_key(&display.machine) {
                return Err(TopologyError::UnknownMachine {
                    machine: display.machine,
                });
            }
            let display_id = display.id;
            if display_map.insert(display_id, display).is_some() {
                return Err(TopologyError::DuplicateDisplay {
                    display: display_id,
                });
            }
        }

        validate_links(&display_map, &links)?;

        let mut links_by_source: BTreeMap<DisplayId, Vec<EdgeLink>> = BTreeMap::new();
        for link in &links {
            links_by_source
                .entry(link.from_display)
                .or_default()
                .push(*link);
        }

        Ok(Self {
            machines: machine_map,
            displays: display_map,
            links,
            links_by_source,
        })
    }

    pub fn machine_for_display(&self, id: DisplayId) -> Result<&Machine, TopologyError> {
        let display = self.display(id)?;
        self.machines
            .get(&display.machine)
            .ok_or(TopologyError::UnknownMachine {
                machine: display.machine,
            })
    }

    pub fn display(&self, id: DisplayId) -> Result<&Display, TopologyError> {
        self.displays
            .get(&id)
            .ok_or(TopologyError::UnknownDisplay { display: id })
    }

    pub fn displays(&self) -> impl ExactSizeIterator<Item = &Display> {
        self.displays.values()
    }

    pub fn links(&self) -> &[EdgeLink] {
        &self.links
    }

    pub fn transition_for_motion(
        &self,
        source_display: DisplayId,
        previous: Point,
        current: Point,
    ) -> Result<Option<EdgeTransition>, TopologyError> {
        if !previous.is_finite() || !current.is_finite() {
            return Err(TopologyError::NonFiniteMotionPoint);
        }

        let source = self.display(source_display)?;
        let bounds = source.bounds();

        let Some(links) = self.links_by_source.get(&source_display) else {
            return Ok(None);
        };
        for link in links {
            let Some(crossing_point) = crossing_point(bounds, link, previous, current) else {
                continue;
            };
            let source_position = normalized_position(bounds, link.from_edge, crossing_point);
            if !link.from_span.contains(source_position) {
                continue;
            }

            let destination = self.display(link.to_display)?;
            let destination_position = link.from_span.map_to(link.to_span, source_position);
            let entry_point = destination_entry_point(
                destination.bounds(),
                link.to_edge,
                destination_position,
                link.hysteresis,
            );

            return Ok(Some(EdgeTransition {
                from_display: source_display,
                to_display: link.to_display,
                from_edge: link.from_edge,
                to_edge: link.to_edge,
                crossing_point,
                entry_point,
            }));
        }

        Ok(None)
    }
}

fn validate_display(display: &Display) -> Result<(), TopologyError> {
    if display.native_size.width == 0 || display.native_size.height == 0 {
        return Err(TopologyError::InvalidNativeSize {
            display: display.id,
        });
    }
    if !display.logical_size.width.is_finite()
        || !display.logical_size.height.is_finite()
        || display.logical_size.width <= 0.0
        || display.logical_size.height <= 0.0
    {
        return Err(TopologyError::InvalidLogicalSize {
            display: display.id,
        });
    }
    if !display.origin.is_finite() {
        return Err(TopologyError::InvalidOrigin {
            display: display.id,
        });
    }
    if !display.bounds().max_x().is_finite()
        || !display.bounds().max_y().is_finite()
        || display.bounds().max_x() <= display.origin.x
        || display.bounds().max_y() <= display.origin.y
    {
        return Err(TopologyError::InvalidLogicalSize {
            display: display.id,
        });
    }
    if !display.scale_factor.is_finite() || display.scale_factor <= 0.0 {
        return Err(TopologyError::InvalidScaleFactor {
            display: display.id,
        });
    }
    if display
        .refresh_rate_hz
        .is_some_and(|refresh_rate_hz| !refresh_rate_hz.is_finite() || refresh_rate_hz <= 0.0)
    {
        return Err(TopologyError::InvalidRefreshRate {
            display: display.id,
        });
    }

    Ok(())
}

fn validate_link_shape(
    from_display: DisplayId,
    to_display: DisplayId,
    hysteresis: f64,
) -> Result<(), TopologyError> {
    if from_display == to_display {
        return Err(TopologyError::SameDisplayLink { from_display });
    }
    if !hysteresis.is_finite() || hysteresis <= 0.0 {
        return Err(TopologyError::InvalidHysteresis { hysteresis });
    }
    Ok(())
}

fn validate_links(
    displays: &BTreeMap<DisplayId, Display>,
    links: &[EdgeLink],
) -> Result<(), TopologyError> {
    for link in links {
        validate_link_shape(link.from_display, link.to_display, link.hysteresis)?;
        let from_display =
            displays
                .get(&link.from_display)
                .ok_or(TopologyError::UnknownDisplay {
                    display: link.from_display,
                })?;
        let to_display = displays
            .get(&link.to_display)
            .ok_or(TopologyError::UnknownDisplay {
                display: link.to_display,
            })?;
        if link.hysteresis >= link.from_edge.normal_extent(from_display.bounds())
            || link.hysteresis >= link.to_edge.normal_extent(to_display.bounds())
        {
            return Err(TopologyError::HysteresisExceedsDisplay {
                from_display: link.from_display,
                to_display: link.to_display,
            });
        }
    }

    for (index, link) in links.iter().enumerate() {
        for other in &links[index + 1..] {
            if link.from_display == other.from_display
                && link.from_edge == other.from_edge
                && link.from_span.overlaps(other.from_span)
            {
                return Err(TopologyError::OverlappingSourceLinks {
                    display: link.from_display,
                    edge: link.from_edge,
                });
            }
        }
    }

    Ok(())
}

fn crossing_point(
    bounds: LogicalRect,
    link: &EdgeLink,
    previous: Point,
    current: Point,
) -> Option<Point> {
    // The actual edge may be behind both samples. Interpolate at the crossed dead-zone
    // boundary, then project onto the edge instead of extrapolating outside the segment.
    match link.from_edge {
        Edge::Left
            if previous.x <= bounds.max_x()
                && previous.x >= bounds.origin.x - link.hysteresis
                && current.x < bounds.origin.x - link.hysteresis
                && current.x < previous.x =>
        {
            let crossed = interpolate_at_x(previous, current, bounds.origin.x - link.hysteresis);
            Some(Point::new(bounds.origin.x, crossed.y))
        }
        Edge::Right
            if previous.x >= bounds.origin.x
                && previous.x <= bounds.max_x() + link.hysteresis
                && current.x > bounds.max_x() + link.hysteresis
                && current.x > previous.x =>
        {
            let crossed = interpolate_at_x(previous, current, bounds.max_x() + link.hysteresis);
            Some(Point::new(bounds.max_x(), crossed.y))
        }
        Edge::Top
            if previous.y <= bounds.max_y()
                && previous.y >= bounds.origin.y - link.hysteresis
                && current.y < bounds.origin.y - link.hysteresis
                && current.y < previous.y =>
        {
            let crossed = interpolate_at_y(previous, current, bounds.origin.y - link.hysteresis);
            Some(Point::new(crossed.x, bounds.origin.y))
        }
        Edge::Bottom
            if previous.y >= bounds.origin.y
                && previous.y <= bounds.max_y() + link.hysteresis
                && current.y > bounds.max_y() + link.hysteresis
                && current.y > previous.y =>
        {
            let crossed = interpolate_at_y(previous, current, bounds.max_y() + link.hysteresis);
            Some(Point::new(crossed.x, bounds.max_y()))
        }
        _ => None,
    }
}

fn interpolate_at_x(previous: Point, current: Point, x: f64) -> Point {
    let progress = (x - previous.x) / (current.x - previous.x);
    Point::new(x, previous.y + (current.y - previous.y) * progress)
}

fn interpolate_at_y(previous: Point, current: Point, y: f64) -> Point {
    let progress = (y - previous.y) / (current.y - previous.y);
    Point::new(previous.x + (current.x - previous.x) * progress, y)
}

fn normalized_position(bounds: LogicalRect, edge: Edge, point: Point) -> f64 {
    match edge {
        Edge::Left | Edge::Right => (point.y - bounds.origin.y) / bounds.size.height,
        Edge::Top | Edge::Bottom => (point.x - bounds.origin.x) / bounds.size.width,
    }
}

fn destination_entry_point(
    bounds: LogicalRect,
    edge: Edge,
    normalized_position: f64,
    inset: f64,
) -> Point {
    match edge {
        Edge::Left => Point::new(
            bounds.origin.x + inset,
            clamp_tangential(
                bounds.origin.y + bounds.size.height * normalized_position,
                bounds.origin.y,
                bounds.max_y(),
            ),
        ),
        Edge::Right => Point::new(
            bounds.max_x() - inset,
            clamp_tangential(
                bounds.origin.y + bounds.size.height * normalized_position,
                bounds.origin.y,
                bounds.max_y(),
            ),
        ),
        Edge::Top => Point::new(
            clamp_tangential(
                bounds.origin.x + bounds.size.width * normalized_position,
                bounds.origin.x,
                bounds.max_x(),
            ),
            bounds.origin.y + inset,
        ),
        Edge::Bottom => Point::new(
            clamp_tangential(
                bounds.origin.x + bounds.size.width * normalized_position,
                bounds.origin.x,
                bounds.max_x(),
            ),
            bounds.max_y() - inset,
        ),
    }
}

fn clamp_tangential(value: f64, min: f64, max: f64) -> f64 {
    value.clamp(min, max.next_down())
}

#[cfg(test)]
mod split_edge_tests {
    use super::NormalizedSpan;

    #[test]
    fn touching_spans_have_one_owner_at_the_shared_endpoint() {
        let first = NormalizedSpan::new(0.0, 0.5).unwrap();
        let second = NormalizedSpan::new(0.5, 1.0).unwrap();
        assert!(!first.overlaps(second));
        assert!(first.contains(0.0));
        assert!(!first.contains(0.5));
        assert!(second.contains(0.5));
        assert!(second.contains(1.0));
        assert!(first.overlaps(NormalizedSpan::new(0.49, 0.8).unwrap()));
    }
}
