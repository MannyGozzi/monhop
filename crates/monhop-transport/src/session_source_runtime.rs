//! Native capture conversion and bounded route command submission.
use crate::{
    session::{
        LinkIo, NativeCaptureStartupFailure, SESSION_POLL_INTERVAL, SessionFailure, SessionIo,
    },
    session_clock::SessionClock,
    session_native::{MAC_POINTS_PER_DETENT, WINDOWS_UNITS_PER_DETENT},
    session_source::{
        MotionTarget, NormalizedInput, SourceController, SourceEffect, SourceEffects,
        SourceOutcome, TaggedInput,
    },
};
use monhop_core::{
    Platform, Point, PointerGesture,
    capture::{CaptureEvent, CapturedEvent, MAX_SUPPRESSION_TTL, StopReason},
    capture_physical::HeldInput,
};
#[cfg(target_os = "macos")]
pub(crate) use monhop_platform_macos::native_capture::{NativeCapture, NativeCaptureError};
#[cfg(windows)]
pub(crate) use monhop_platform_windows::native_capture::{NativeCapture, NativeCaptureError};
use monhop_protocol::Frame;
use std::time::Duration;

/// The two platform error enums are distinct types, so the arms they share are written once here
/// and each caller appends only its platform-only tail.
macro_rules! shared_capture_start_arms {
    ($error:expr, $($tail:tt)*) => {
        match $error {
            NativeCaptureError::AlreadyActive => NativeCaptureStartupFailure::AlreadyActive,
            NativeCaptureError::InvalidDuration => NativeCaptureStartupFailure::InvalidDuration,
            NativeCaptureError::StartFailed => NativeCaptureStartupFailure::StartFailed,
            NativeCaptureError::StartupTimeout => NativeCaptureStartupFailure::StartupTimeout,
            NativeCaptureError::CleanupPending => NativeCaptureStartupFailure::CleanupPending,
            NativeCaptureError::ControlPending => NativeCaptureStartupFailure::ControlPending,
            NativeCaptureError::ControlFailed => NativeCaptureStartupFailure::ControlFailed,
            NativeCaptureError::WorkerPanicked => NativeCaptureStartupFailure::WorkerPanicked,
            NativeCaptureError::Stopped(reason) => NativeCaptureStartupFailure::Stopped(reason),
            $($tail)*
        }
    };
}

#[cfg(windows)]
pub(crate) fn native_capture_start_failure(error: NativeCaptureError) -> SessionFailure {
    use monhop_platform_windows::native_capture::NativeCaptureError;

    if matches!(
        error,
        NativeCaptureError::Stopped(StopReason::DisplaysChanged)
    ) {
        return SessionFailure::LocalDisplaysChanged;
    }
    let failure = shared_capture_start_arms!(
        error,
        NativeCaptureError::DesktopUnavailable => NativeCaptureStartupFailure::DesktopUnavailable,
        NativeCaptureError::Windows { operation, code } => {
            NativeCaptureStartupFailure::WindowsOperation { operation, code }
        }
    );
    SessionFailure::NativeCaptureStartup(failure)
}

#[cfg(target_os = "macos")]
pub(crate) fn native_capture_start_failure(error: NativeCaptureError) -> SessionFailure {
    use monhop_platform_macos::native_capture::NativeCaptureError;

    if matches!(
        error,
        NativeCaptureError::Stopped(StopReason::DisplaysChanged)
    ) {
        return SessionFailure::LocalDisplaysChanged;
    }
    let failure = shared_capture_start_arms!(
        error,
        NativeCaptureError::Mac(_)
        | NativeCaptureError::Clock(_)
        | NativeCaptureError::Power(_) => NativeCaptureStartupFailure::Platform,
    );
    SessionFailure::NativeCaptureStartup(failure)
}

/// Why native capture refused a route command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureRefusal {
    /// An earlier command is still being applied: submit this one again later.
    Pending,
    /// Only from `activate_remote`: input held since capture started keeps suppression refused
    /// while capture itself runs on.
    NotReady,
    /// Capture stopped or its control failed.
    Failed,
}

/// The one native capture that every source controller on this computer routes through.
pub(crate) trait CaptureControl {
    fn activate_remote(&mut self, generation: u64, ttl: Duration) -> Result<u64, CaptureRefusal>;
    fn restore_local_at(&mut self, generation: u64, position: Point)
    -> Result<u64, CaptureRefusal>;
    fn renew_suppression(&mut self, generation: u64, ttl: Duration) -> Result<u64, CaptureRefusal>;
    /// `None` once native control failed.
    fn completed_control_revision(&self) -> Option<u64>;
    fn is_ready_for_suppression(&self) -> bool;
    /// The presses held since capture started that keep suppression refused.
    fn blocking_presses(&self) -> Vec<HeldInput>;
    fn stop_reason(&self) -> Option<StopReason>;
    fn request_stop(&self);
}

/// Asks the capture only while suppression is refused, so a ready capture's tick allocates nothing.
pub(crate) fn blocking_presses(capture: &impl CaptureControl) -> Vec<HeldInput> {
    if capture.is_ready_for_suppression() {
        Vec::new()
    } else {
        capture.blocking_presses()
    }
}

/// The authenticated link to one controller's peer.
pub(crate) trait OutboundFrames {
    fn send_outbound(&self, frame: Frame) -> Result<(), SessionFailure>;
}

impl CaptureControl for NativeCapture {
    fn activate_remote(&mut self, generation: u64, ttl: Duration) -> Result<u64, CaptureRefusal> {
        NativeCapture::activate_remote(self, generation, ttl)
            .map_err(|error| activation_refusal(error, NativeCapture::stop_reason(self)))
    }
    fn restore_local_at(
        &mut self,
        generation: u64,
        position: Point,
    ) -> Result<u64, CaptureRefusal> {
        NativeCapture::restore_local_at(self, generation, position).map_err(refusal)
    }
    fn renew_suppression(&mut self, generation: u64, ttl: Duration) -> Result<u64, CaptureRefusal> {
        NativeCapture::renew_suppression(self, generation, ttl).map_err(refusal)
    }
    fn completed_control_revision(&self) -> Option<u64> {
        NativeCapture::completed_control_revision(self).ok()
    }
    fn is_ready_for_suppression(&self) -> bool {
        NativeCapture::is_ready_for_suppression(self)
    }
    fn blocking_presses(&self) -> Vec<HeldInput> {
        NativeCapture::blocking_presses(self)
    }
    fn stop_reason(&self) -> Option<StopReason> {
        NativeCapture::stop_reason(self)
    }
    fn request_stop(&self) {
        NativeCapture::request_stop(self);
    }
}

impl OutboundFrames for SessionIo {
    fn send_outbound(&self, frame: Frame) -> Result<(), SessionFailure> {
        SessionIo::send_outbound(self, frame)
    }
}

impl OutboundFrames for LinkIo {
    fn send_outbound(&self, frame: Frame) -> Result<(), SessionFailure> {
        LinkIo::send_outbound(self, frame)
    }
}

fn refusal(error: NativeCaptureError) -> CaptureRefusal {
    if matches!(error, NativeCaptureError::ControlPending) {
        CaptureRefusal::Pending
    } else {
        CaptureRefusal::Failed
    }
}

/// The readiness guard refuses with `Stopped(InvalidInput)` without stopping capture; the same
/// error once capture has stopped is final.
fn activation_refusal(error: NativeCaptureError, stopped: Option<StopReason>) -> CaptureRefusal {
    match error {
        NativeCaptureError::Stopped(StopReason::InvalidInput) if stopped.is_none() => {
            CaptureRefusal::NotReady
        }
        error => refusal(error),
    }
}

/// The pairwise runtime's outcome: a controller failure stops capture at once, and a handover,
/// which two computers never make, is a broken controller.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_outcome<C: CaptureControl, O: OutboundFrames>(
    outcome: SourceOutcome,
    controller: &mut SourceController,
    capture: &mut C,
    out: &O,
    generation: u64,
    origin: &SessionClock,
    pending: &mut Option<(SourceEffect, Duration)>,
    submitted: &mut Option<(u64, Duration)>,
) -> Result<(), SessionFailure> {
    if let Some(failure) = outcome.failure {
        capture.request_stop();
        return Err(SessionFailure::SourceController(failure));
    }
    if outcome.handover.is_some() {
        return Err(SessionFailure::Source);
    }
    apply_effects(
        &outcome.effects,
        controller,
        capture,
        out,
        generation,
        origin,
        pending,
        submitted,
    )
}

/// Frames go to the controller's peer and route changes to native capture. A route change that
/// native control is still busy for waits in `pending`; a second one meanwhile is a broken
/// controller.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_effects<C: CaptureControl, O: OutboundFrames>(
    effects: &SourceEffects,
    controller: &mut SourceController,
    capture: &mut C,
    out: &O,
    generation: u64,
    origin: &SessionClock,
    pending: &mut Option<(SourceEffect, Duration)>,
    submitted: &mut Option<(u64, Duration)>,
) -> Result<(), SessionFailure> {
    for effect in effects.iter() {
        match effect {
            SourceEffect::RemoteFrame(frame) => out.send_outbound(frame.clone())?,
            _ => {
                if pending.is_some() {
                    return Err(SessionFailure::Source);
                }
                if !apply_route(
                    effect, controller, capture, out, generation, origin, submitted,
                )? {
                    *pending = Some((effect.clone(), origin.elapsed()));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn lease_budget(
    controller: &mut SourceController,
    now: Duration,
) -> Result<Duration, SessionFailure> {
    let ttl = controller
        .suppression_budget(now)
        .map_err(SessionFailure::SourceController)?;
    ttl.checked_sub(SESSION_POLL_INTERVAL)
        .filter(|ttl| !ttl.is_zero())
        .ok_or(SessionFailure::Source)
}

/// True once the effect is settled: submitted, or refused and unwound. False means retry it.
pub(crate) fn apply_route<C: CaptureControl, O: OutboundFrames>(
    effect: &SourceEffect,
    controller: &mut SourceController,
    capture: &mut C,
    out: &O,
    generation: u64,
    origin: &SessionClock,
    submitted: &mut Option<(u64, Duration)>,
) -> Result<bool, SessionFailure> {
    let (request, result) = match effect {
        SourceEffect::ActivateRemote { request } => (
            *request,
            capture.activate_remote(generation, lease_budget(controller, origin.elapsed())?),
        ),
        SourceEffect::RestoreLocalAt {
            request, position, ..
        } => (*request, capture.restore_local_at(generation, *position)),
        SourceEffect::RemoteFrame(_) => return Err(SessionFailure::Source),
    };
    match result {
        Ok(ticket) => {
            *submitted = Some((ticket, origin.elapsed()));
            let bound = controller.bind_capture_route(request, ticket, origin.elapsed());
            if let Some(failure) = bound.failure {
                return Err(SessionFailure::SourceController(failure));
            }
            if !bound.effects.is_empty() || bound.handover.is_some() {
                return Err(SessionFailure::Source);
            }
            Ok(true)
        }
        Err(CaptureRefusal::Pending) => Ok(false),
        // Input held since capture started keeps suppression refused: the seam stays a wall.
        Err(CaptureRefusal::NotReady) if matches!(effect, SourceEffect::ActivateRemote { .. }) => {
            let unwound = controller.refuse_capture_activation(request, origin.elapsed());
            if let Some(failure) = unwound.failure {
                return Err(SessionFailure::SourceController(failure));
            }
            if unwound.handover.is_some() {
                return Err(SessionFailure::Source);
            }
            for effect in unwound.effects.iter() {
                let SourceEffect::RemoteFrame(frame) = effect else {
                    return Err(SessionFailure::Source);
                };
                out.send_outbound(frame.clone())?;
            }
            Ok(true)
        }
        Err(_) => {
            controller.fail_capture_route_submission(request, origin.elapsed());
            Err(SessionFailure::Native)
        }
    }
}

/// Resubmits the route change native control was busy for; one still waiting after a whole lease
/// means native control stalled.
pub(crate) fn retry_pending<C: CaptureControl, O: OutboundFrames>(
    pending: &mut Option<(SourceEffect, Duration)>,
    controller: &mut SourceController,
    capture: &mut C,
    out: &O,
    generation: u64,
    origin: &SessionClock,
    submitted: &mut Option<(u64, Duration)>,
) -> Result<(), SessionFailure> {
    if let Some((effect, since)) = pending.take() {
        if origin.elapsed().saturating_sub(since) >= MAX_SUPPRESSION_TTL {
            return Err(SessionFailure::Native);
        }
        if !apply_route(
            &effect, controller, capture, out, generation, origin, submitted,
        )? {
            *pending = Some((effect, since));
        }
    }
    Ok(())
}

/// Clears the submitted command once native control completed it; one outstanding for a whole
/// lease means native control stalled.
pub(crate) fn settle_submitted<C: CaptureControl>(
    capture: &C,
    submitted: &mut Option<(u64, Duration)>,
    now: Duration,
) -> Result<(), SessionFailure> {
    if let Some((ticket, issued)) = *submitted {
        match capture.completed_control_revision() {
            Some(completed) if completed >= ticket => *submitted = None,
            Some(_) if now.saturating_sub(issued) < MAX_SUPPRESSION_TTL => {}
            _ => return Err(SessionFailure::Native),
        }
    }
    Ok(())
}

/// Extends remote suppression by the controller's lease budget. True once the renewal is
/// submitted, false while native control is busy.
pub(crate) fn renew_lease<C: CaptureControl>(
    controller: &mut SourceController,
    capture: &mut C,
    generation: u64,
    origin: &SessionClock,
    submitted: &mut Option<(u64, Duration)>,
) -> Result<bool, SessionFailure> {
    let ttl = lease_budget(controller, origin.elapsed())?;
    match capture.renew_suppression(generation, ttl) {
        Ok(ticket) => {
            *submitted = Some((ticket, origin.elapsed()));
            Ok(true)
        }
        Err(CaptureRefusal::Pending) => Ok(false),
        Err(_) => Err(SessionFailure::Native),
    }
}

pub(crate) fn normalize(
    record: CapturedEvent,
    target: Option<MotionTarget>,
) -> Result<Option<TaggedInput>, SessionFailure> {
    let event = match record.event {
        CaptureEvent::Key {
            usage,
            pressed,
            repeat,
            modifiers,
        } => NormalizedInput::Key {
            usage,
            pressed,
            repeat,
            modifiers,
        },
        CaptureEvent::Button {
            button,
            pressed,
            at,
        } => NormalizedInput::Button {
            button,
            pressed,
            at,
        },
        CaptureEvent::RouteChanged { remote, revision } => {
            NormalizedInput::RouteChanged { remote, revision }
        }
        CaptureEvent::AbsoluteMotion { x, y } => {
            if record.remote {
                return Ok(None);
            }
            NormalizedInput::AbsoluteMotion(Point::new(f64::from(x), f64::from(y)))
        }
        CaptureEvent::LogicalAbsoluteMotion { x, y } => {
            if record.remote {
                return Ok(None);
            }
            NormalizedInput::AbsoluteMotion(Point::new(x, y))
        }
        CaptureEvent::RelativeMotion { dx, dy } => {
            relative(Platform::Windows, f64::from(dx), f64::from(dy), target)?
        }
        CaptureEvent::LogicalRelativeMotion { dx, dy } => {
            relative(Platform::MacOs, dx, dy, target)?
        }
        CaptureEvent::Scroll {
            horizontal,
            vertical,
        } => NormalizedInput::Scroll {
            horizontal: f64::from(horizontal) / WINDOWS_UNITS_PER_DETENT,
            vertical: f64::from(vertical) / WINDOWS_UNITS_PER_DETENT,
        },
        CaptureEvent::LogicalScroll {
            horizontal,
            vertical,
        } => NormalizedInput::Scroll {
            horizontal: -horizontal / MAC_POINTS_PER_DETENT,
            vertical: vertical / MAC_POINTS_PER_DETENT,
        },
        // The capture queue admits only valid parts, so a refusal here is a broken slot.
        CaptureEvent::Gesture { kind, phase, value } => NormalizedInput::Gesture(
            PointerGesture::from_parts(kind, phase, value).map_err(|_| SessionFailure::Source)?,
        ),
        CaptureEvent::SystemGesture(gesture) => NormalizedInput::SystemGesture(gesture),
    };
    Ok(Some(TaggedInput {
        event,
        routing_revision: record.routing_revision,
        remote: record.remote,
        floor_generation: record.floor_generation,
    }))
}

/// Relative motion a `source` computer captured, onto `target`'s display, or in the mouse's own
/// units while no route is settled so the source can carry it into the one it settles on.
fn relative(
    source: Platform,
    dx: f64,
    dy: f64,
    target: Option<MotionTarget>,
) -> Result<NormalizedInput, SessionFailure> {
    let per_unit = target.map_or(1.0, |target| target.per_captured_unit(source));
    if !per_unit.is_finite() || per_unit <= 0.0 {
        return Err(SessionFailure::InvalidLayout);
    }
    Ok(NormalizedInput::RelativeMotion(Point::new(
        dx * per_unit,
        dy * per_unit,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalization_preserves_floor_generation_and_redacted_routing() {
        let record = normalize(
            CapturedEvent {
                event: CaptureEvent::LogicalRelativeMotion { dx: 1.0, dy: 2.0 },
                routing_revision: 7,
                remote: true,
                floor_generation: 42,
            },
            Some(MotionTarget {
                target: monhop_core::PointerTarget::new(
                    monhop_core::DeviceId([1; 16]),
                    monhop_core::DisplayId(1),
                ),
                platform: Platform::MacOs,
                scale_factor: 2.0,
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(record.floor_generation, 42);
        assert_eq!(record.routing_revision, 7);
        assert!(record.remote);
        assert_eq!(
            record.event,
            NormalizedInput::RelativeMotion(Point::new(1.0, 2.0))
        );
    }
    #[test]
    fn relative_motion_converts_onto_its_target_or_keeps_the_mouses_units_without_one() {
        let counts = |target: Option<MotionTarget>| {
            normalize(
                CapturedEvent {
                    event: CaptureEvent::RelativeMotion { dx: 6, dy: -4 },
                    routing_revision: 1,
                    remote: false,
                    floor_generation: 1,
                },
                target,
            )
            .unwrap()
            .unwrap()
            .event
        };
        let retina_mac = MotionTarget {
            target: monhop_core::PointerTarget::new(
                monhop_core::DeviceId([2; 16]),
                monhop_core::DisplayId(2),
            ),
            platform: Platform::MacOs,
            scale_factor: 2.0,
        };
        assert_eq!(
            counts(Some(retina_mac)),
            NormalizedInput::RelativeMotion(Point::new(3.0, -2.0))
        );
        assert_eq!(
            counts(None),
            NormalizedInput::RelativeMotion(Point::new(6.0, -4.0))
        );
    }
    #[test]
    fn high_resolution_scroll_preserves_fraction_and_direction() {
        let record = normalize(
            CapturedEvent {
                event: CaptureEvent::Scroll {
                    horizontal: 30,
                    vertical: -1,
                },
                routing_revision: 0,
                remote: false,
                floor_generation: 1,
            },
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            record.event,
            NormalizedInput::Scroll {
                horizontal: 0.25,
                vertical: -1.0 / 120.0
            }
        );
        let record = normalize(
            CapturedEvent {
                event: CaptureEvent::LogicalScroll {
                    horizontal: 10.0,
                    vertical: -0.5,
                },
                routing_revision: 0,
                remote: false,
                floor_generation: 1,
            },
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            record.event,
            NormalizedInput::Scroll {
                horizontal: -0.25,
                vertical: -0.0125
            }
        );
    }
    #[test]
    fn captured_gestures_normalize_to_the_semantic_they_carry() {
        let normalized = |event| {
            normalize(
                CapturedEvent {
                    event,
                    routing_revision: 2,
                    remote: true,
                    floor_generation: 1,
                },
                None,
            )
            .unwrap()
            .unwrap()
            .event
        };
        let pinch = PointerGesture::Magnify {
            phase: monhop_core::GesturePhase::Changed,
            delta: -0.25,
        };
        assert_eq!(
            normalized(CaptureEvent::gesture(pinch)),
            NormalizedInput::Gesture(pinch)
        );
        let system = monhop_core::SystemGesture::ShowDesktop;
        assert_eq!(
            normalized(CaptureEvent::SystemGesture(system)),
            NormalizedInput::SystemGesture(system)
        );
    }

    #[test]
    fn remote_absolute_samples_cannot_duplicate_relative_motion() {
        assert!(
            normalize(
                CapturedEvent {
                    event: CaptureEvent::AbsoluteMotion { x: 10, y: 20 },
                    routing_revision: 3,
                    remote: true,
                    floor_generation: 3
                },
                None
            )
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn only_an_activation_refused_while_capture_runs_is_not_ready() {
        use NativeCaptureError::{ControlFailed, ControlPending, Stopped};
        let held = Stopped(StopReason::InvalidInput);
        assert_eq!(activation_refusal(held, None), CaptureRefusal::NotReady);
        assert_eq!(
            activation_refusal(held, Some(StopReason::InvalidInput)),
            CaptureRefusal::Failed
        );
        assert_eq!(
            activation_refusal(Stopped(StopReason::NativeFailure), None),
            CaptureRefusal::Failed
        );
        assert_eq!(
            activation_refusal(ControlPending, None),
            CaptureRefusal::Pending
        );
        assert_eq!(refusal(held), CaptureRefusal::Failed);
        assert_eq!(refusal(ControlPending), CaptureRefusal::Pending);
        assert_eq!(refusal(ControlFailed), CaptureRefusal::Failed);
    }

    const HOME: u8 = 1;
    const PEER: u8 = 2;
    const THIRD: u8 = 3;

    fn device(machine: u8) -> monhop_core::DeviceId {
        monhop_core::DeviceId([machine; 16])
    }

    /// Display 1 has the peer's display 2 on its right, and display 2 has a third computer's
    /// display 3 on its right: a topology in which a controller can hand over.
    fn group_topology() -> monhop_core::Topology {
        use monhop_core::{
            Display, DisplayId, Edge, EdgeLink, LogicalSize, Machine, NativeSize, NormalizedSpan,
        };
        let full = NormalizedSpan::new(0.0, 1.0).unwrap();
        let link = |from: u8, from_edge, to: u8, to_edge| {
            EdgeLink::new(
                DisplayId(u64::from(from)),
                from_edge,
                full,
                DisplayId(u64::from(to)),
                to_edge,
                full,
                1.0,
            )
            .unwrap()
        };
        let display = |machine: u8| {
            Display::new(
                DisplayId(u64::from(machine)),
                device(machine),
                format!("display-{machine}"),
                NativeSize::new(100, 100),
                LogicalSize::new(100.0, 100.0),
                Point::new(0.0, 0.0),
                1.0,
                None,
                true,
            )
        };
        monhop_core::Topology::new(
            vec![
                Machine::new(device(HOME), Platform::Windows),
                Machine::new(device(PEER), Platform::MacOs),
                Machine::new(device(THIRD), Platform::Windows),
            ],
            vec![display(HOME), display(PEER), display(THIRD)],
            vec![
                link(HOME, Edge::Right, PEER, Edge::Left),
                link(PEER, Edge::Left, HOME, Edge::Right),
                link(PEER, Edge::Right, THIRD, Edge::Left),
                link(THIRD, Edge::Left, PEER, Edge::Right),
            ],
        )
        .unwrap()
    }

    /// Grants every route change at once and queues its barrier for the next capture read.
    #[derive(Default)]
    struct FakeCapture {
        route: (bool, u64),
        barrier: Option<(bool, u64)>,
        stopped: std::cell::Cell<bool>,
    }

    impl FakeCapture {
        fn reroute(&mut self, remote: bool) -> Result<u64, CaptureRefusal> {
            self.route = (remote, self.route.1 + 1);
            self.barrier = Some(self.route);
            Ok(self.route.1)
        }
    }

    impl CaptureControl for FakeCapture {
        fn activate_remote(&mut self, _: u64, _: Duration) -> Result<u64, CaptureRefusal> {
            self.reroute(true)
        }
        fn restore_local_at(&mut self, _: u64, _: Point) -> Result<u64, CaptureRefusal> {
            self.reroute(false)
        }
        fn renew_suppression(&mut self, _: u64, _: Duration) -> Result<u64, CaptureRefusal> {
            Ok(self.route.1)
        }
        fn completed_control_revision(&self) -> Option<u64> {
            Some(self.route.1)
        }
        fn is_ready_for_suppression(&self) -> bool {
            true
        }
        fn blocking_presses(&self) -> Vec<HeldInput> {
            Vec::new()
        }
        fn stop_reason(&self) -> Option<StopReason> {
            self.stopped.get().then_some(StopReason::Requested)
        }
        fn request_stop(&self) {
            self.stopped.set(true);
        }
    }

    #[derive(Default)]
    struct Outbox(std::cell::RefCell<std::collections::VecDeque<Frame>>);

    impl OutboundFrames for Outbox {
        fn send_outbound(&self, frame: Frame) -> Result<(), SessionFailure> {
            self.0.borrow_mut().push_back(frame);
            Ok(())
        }
    }

    struct Ignored;

    impl crate::session_receiver::InputDestination for Ignored {
        fn apply(
            &mut self,
            _: crate::session_receiver::DestinationAction,
        ) -> Result<(), crate::session_receiver::DestinationFailure> {
            Ok(())
        }
    }

    /// The outbound half of a pairwise session, driven through `apply_outcome` at a fixed time,
    /// with its peer's receiver answering every frame.
    struct Pairwise {
        floor: monhop_core::SharedFloor,
        source: SourceController,
        native: FakeCapture,
        outbox: Outbox,
        peer: crate::session_receiver::InputReceiver,
        reply_epoch: monhop_protocol::SessionEpoch,
        reply_sequence: u64,
        heartbeat_sequence: u64,
        origin: SessionClock,
        pending: Option<(SourceEffect, Duration)>,
        submitted: Option<(u64, Duration)>,
    }

    impl Pairwise {
        fn new() -> Self {
            use monhop_core::{DisplayId, FloorPeer, SharedFloor, TakeBackGate};
            use monhop_protocol::{DisplayDescription, DisplayTopology, SessionEpoch};
            let epoch = SessionEpoch::new(3).unwrap();
            let floor = SharedFloor::new();
            let mut source = SourceController::new(
                group_topology(),
                device(HOME),
                DisplayId(u64::from(HOME)),
                epoch,
                3,
                Duration::ZERO,
            )
            .unwrap()
            .with_floor(floor.clone(), true)
            .with_peer(device(PEER), FloorPeer::slot(1).unwrap())
            .unwrap();
            source.set_reachable(&[device(THIRD)]);
            let displays = DisplayTopology::new(vec![DisplayDescription {
                id: DisplayId(u64::from(PEER)),
                name: format!("display-{PEER}"),
                native_width: 100,
                native_height: 100,
                logical_origin: Point::new(0.0, 0.0),
                logical_size: Point::new(100.0, 100.0),
                scale_factor: 1.0,
                is_primary: true,
                monitor: None,
            }])
            .unwrap();
            let peer = crate::session_receiver::InputReceiver::new(displays, epoch, Duration::ZERO)
                .with_floor(TakeBackGate::new(SharedFloor::new()), false, true);
            Self {
                floor,
                source,
                native: FakeCapture::default(),
                outbox: Outbox::default(),
                peer,
                reply_epoch: epoch,
                reply_sequence: 0,
                heartbeat_sequence: 3,
                origin: SessionClock::with_test_reader(|| Duration::ZERO),
                pending: None,
                submitted: None,
            }
        }

        fn apply(&mut self, outcome: SourceOutcome) -> Result<(), SessionFailure> {
            apply_outcome(
                outcome,
                &mut self.source,
                &mut self.native,
                &self.outbox,
                1,
                &self.origin,
                &mut self.pending,
                &mut self.submitted,
            )?;
            if let Some((remote, revision)) = self.native.barrier.take() {
                self.capture(NormalizedInput::RouteChanged { remote, revision })?;
            }
            Ok(())
        }

        fn capture(&mut self, event: NormalizedInput) -> Result<(), SessionFailure> {
            let record = TaggedInput {
                event,
                routing_revision: self.native.route.1,
                remote: self.native.route.0,
                floor_generation: self.floor.snapshot().generation,
            };
            let outcome = self.source.on_captured(record, Duration::ZERO);
            self.apply(outcome)
        }

        fn respond(&mut self, frame: &Frame) -> Option<Frame> {
            use monhop_protocol::Message;
            let reply = self
                .peer
                .receive(frame, Duration::ZERO, &mut Ignored)
                .expect("the peer accepts the frame");
            self.peer.flush(&mut Ignored).expect("the peer delivers it");
            let message = reply?;
            if matches!(message, Message::Ping(_) | Message::Pong(_)) {
                let sequence = self.heartbeat_sequence;
                self.heartbeat_sequence += 1;
                return Some(Frame::new(self.peer.control_epoch(), sequence, message));
            }
            let epoch = self.peer.epoch();
            if epoch != self.reply_epoch {
                self.reply_epoch = epoch;
                self.reply_sequence = 0;
            }
            let sequence = self.reply_sequence;
            self.reply_sequence += 1;
            Some(Frame::new(epoch, sequence, message))
        }

        /// Delivers queued frames and applies the replies, up to the first reply whose outcome
        /// hands the pointer over, which comes back unapplied.
        fn deliver(&mut self) -> Result<Option<SourceOutcome>, SessionFailure> {
            loop {
                let next = self.outbox.0.borrow_mut().pop_front();
                let Some(frame) = next else {
                    return Ok(None);
                };
                if let Some(reply) = self.respond(&frame) {
                    let outcome = self.source.on_remote_frame(&reply, Duration::ZERO);
                    if outcome.handover.is_some() {
                        return Ok(Some(outcome));
                    }
                    self.apply(outcome)?;
                }
            }
        }
    }

    fn moved(dx: f64) -> NormalizedInput {
        NormalizedInput::RelativeMotion(Point::new(dx, 0.0))
    }

    #[test]
    fn a_handover_is_a_broken_controller_in_the_pairwise_runtime() {
        use crate::session_source::{PUSH_THROUGH_DISTANCE, SourceMode};
        let mut pair = Pairwise::new();
        pair.capture(NormalizedInput::AbsoluteMotion(Point::new(99.0, 50.0)))
            .unwrap();
        pair.capture(moved(PUSH_THROUGH_DISTANCE)).unwrap();
        assert!(pair.deliver().unwrap().is_none());
        assert_eq!(pair.source.mode(), SourceMode::Remote);
        assert_eq!(pair.native.route, (true, 1));

        pair.capture(moved(98.0)).unwrap();
        pair.capture(moved(PUSH_THROUGH_DISTANCE)).unwrap();
        assert_eq!(
            pair.source.mode(),
            SourceMode::AwaitRemoteReleaseAcknowledgement
        );
        let handed = pair
            .deliver()
            .unwrap()
            .expect("the release acknowledgement hands the pointer over");
        assert_eq!(handed.failure, None);
        let queued = pair.outbox.0.borrow().len();
        assert_eq!(pair.apply(handed), Err(SessionFailure::Source));
        assert_eq!(pair.outbox.0.borrow().len(), queued);
    }

    #[cfg(windows)]
    #[test]
    fn windows_startup_failures_retain_the_operation_and_code() {
        assert_eq!(
            native_capture_start_failure(NativeCaptureError::Windows {
                operation: "capture startup test",
                code: 5
            }),
            SessionFailure::NativeCaptureStartup(NativeCaptureStartupFailure::WindowsOperation {
                operation: "capture startup test",
                code: 5
            })
        );
    }
}
