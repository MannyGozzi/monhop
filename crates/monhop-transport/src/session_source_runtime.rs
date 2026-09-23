//! Native capture conversion and bounded route command submission.
use crate::{
    session::{NativeCaptureStartupFailure, SESSION_POLL_INTERVAL, SessionFailure, SessionIo},
    session_clock::SessionClock,
    session_native::{MAC_POINTS_PER_DETENT, WINDOWS_UNITS_PER_DETENT},
    session_source::{
        MotionTarget, NormalizedInput, SourceController, SourceEffect, SourceOutcome, TaggedInput,
    },
};
use monhop_core::{
    Platform, Point, PointerGesture,
    capture::{CaptureEvent, CapturedEvent, StopReason},
};
#[cfg(target_os = "macos")]
pub(crate) use monhop_platform_macos::native_capture::{NativeCapture, NativeCaptureError};
#[cfg(windows)]
pub(crate) use monhop_platform_windows::native_capture::{NativeCapture, NativeCaptureError};
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_outcome(
    outcome: SourceOutcome,
    controller: &mut SourceController,
    capture: &mut NativeCapture,
    io: &SessionIo,
    generation: u64,
    origin: &SessionClock,
    pending: &mut Option<(SourceEffect, Duration)>,
    submitted: &mut Option<(u64, Duration)>,
) -> Result<(), SessionFailure> {
    if let Some(failure) = outcome.failure {
        capture.request_stop();
        return Err(SessionFailure::SourceController(failure));
    }
    for effect in outcome.effects.iter() {
        match effect {
            SourceEffect::RemoteFrame(frame) => io.send_outbound(frame.clone())?,
            _ => {
                if pending.is_some() {
                    return Err(SessionFailure::Source);
                }
                if !apply_route(
                    effect, controller, capture, io, generation, origin, submitted,
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
pub(crate) fn apply_route(
    effect: &SourceEffect,
    controller: &mut SourceController,
    capture: &mut NativeCapture,
    io: &SessionIo,
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
            if !bound.effects.is_empty() {
                return Err(SessionFailure::Source);
            }
            Ok(true)
        }
        Err(NativeCaptureError::ControlPending) => Ok(false),
        // Input held since capture started keeps suppression refused: the seam stays a wall.
        Err(NativeCaptureError::Stopped(StopReason::InvalidInput))
            if matches!(effect, SourceEffect::ActivateRemote { .. })
                && capture.stop_reason().is_none() =>
        {
            let unwound = controller.refuse_capture_activation(request, origin.elapsed());
            if let Some(failure) = unwound.failure {
                return Err(SessionFailure::SourceController(failure));
            }
            for effect in unwound.effects.iter() {
                let SourceEffect::RemoteFrame(frame) = effect else {
                    return Err(SessionFailure::Source);
                };
                io.send_outbound(frame.clone())?;
            }
            Ok(true)
        }
        Err(_) => {
            controller.fail_capture_route_submission(request, origin.elapsed());
            Err(SessionFailure::Native)
        }
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
