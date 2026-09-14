//! Couples the pure source controller to the explicitly enabled native capture lifetime.

use crate::{
    session::{
        NativeCaptureStartupFailure, SESSION_POLL_INTERVAL, SessionFailure, SessionIo,
        SessionProgress, SessionStartupFailure, TickGap, session_millis,
    },
    session_clock::{SessionClock, millis_u64},
    session_handshake::NegotiatedSession,
    session_native::{MAC_POINTS_PER_DETENT, WINDOWS_UNITS_PER_DETENT},
    session_source::{
        MotionTarget, NormalizedInput, SourceController, SourceEffect, SourceMode, SourceOutcome,
        TaggedInput,
    },
    session_startup::{StartupControl, startup_failure},
    session_trial::TrialInputGuard,
};
use monhop_core::{
    DisplayId, Platform, Point, RevocationSignal, Topology,
    capture::{CaptureEvent, CapturedEvent},
};
use monhop_protocol::DisplayTopology;
use std::time::Duration;

/// A pointer resting on a display the layout leaves out, or moved by the OS without a hook
/// sample, still crosses: the OS pointer is read this often while input is local.
const POINTER_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[cfg(any(windows, target_os = "macos"))]
use crate::session_trial::TrialAuthorization;

#[cfg(target_os = "macos")]
use monhop_platform_macos::native_capture::{NativeCapture, NativeCaptureError};
#[cfg(windows)]
use monhop_platform_windows::native_capture::{NativeCapture, NativeCaptureError};

pub async fn run_source(
    session: NegotiatedSession,
    topology: Topology,
    initial_display: DisplayId,
    generation: u64,
    cancel: RevocationSignal,
    progress: SessionProgress,
) -> Result<(), SessionFailure> {
    run_source_with(
        session,
        topology,
        initial_display,
        generation,
        cancel,
        progress,
        SourceStart {
            purpose: crate::session_handshake::SessionPurpose::Share,
            trial_guard: None,
            capture: move |revocation| {
                NativeCapture::start_after_local_enable(generation, revocation)
                    .map_err(native_capture_start_failure)
            },
        },
    )
    .await
}

/// Runs the source half of a separately authenticated controlled trial. The trial window is the
/// capture's confinement: losing the foreground revokes before the next event can be suppressed.
#[cfg(any(windows, target_os = "macos"))]
pub async fn run_trial_source(
    session: NegotiatedSession,
    topology: Topology,
    initial_display: DisplayId,
    generation: u64,
    cancel: RevocationSignal,
    progress: SessionProgress,
    authorization: TrialAuthorization,
) -> Result<(), SessionFailure> {
    authorization.require_active()?;
    let _external_revocation = authorization.link_external_revocation(cancel)?;
    let trial_revocation = authorization.revocation();
    let trial_guard = authorization.input_guard();
    let window = authorization.source_window();
    let result = run_source_with(
        session,
        topology,
        initial_display,
        generation,
        trial_revocation,
        progress,
        SourceStart {
            purpose: crate::session_handshake::SessionPurpose::ControlledTrial,
            trial_guard: Some(trial_guard),
            capture: move |revocation| {
                NativeCapture::start_controlled_trial(generation, revocation, window)
                    .map_err(native_capture_start_failure)
            },
        },
    )
    .await;
    authorization.revocation().revoke();
    result
}

struct SourceStart<F> {
    purpose: crate::session_handshake::SessionPurpose,
    trial_guard: Option<TrialInputGuard>,
    capture: F,
}

// A capture that stopped revokes the session signal itself; say why before reporting "Revoked".
fn note_capture_stop(capture: &NativeCapture) {
    if let Some(reason) = capture.stop_reason() {
        log::warn!("source capture stopped: {reason:?}; the session ends as revoked");
    }
}

async fn run_source_with<F>(
    session: NegotiatedSession,
    topology: Topology,
    initial_display: DisplayId,
    generation: u64,
    cancel: RevocationSignal,
    progress: SessionProgress,
    start: SourceStart<F>,
) -> Result<(), SessionFailure>
where
    F: FnOnce(RevocationSignal) -> Result<NativeCapture, SessionFailure> + Send + 'static,
{
    if !session.local_is_source || session.purpose() != start.purpose {
        return Err(SessionFailure::Source);
    }
    let trial_guard = start.trial_guard;
    require_trial_active(trial_guard.as_ref())?;
    let source_device = session.source;
    let epoch = session.initial_epoch;
    let sequence = session.control.next_sequence();
    let expected_displays = session.local.topology.clone();
    let mut lifetime = RevokeOnDrop(Some(cancel.clone()));
    let mut io = SessionIo::new(session, progress.clone());
    let origin = SessionClock::try_now()?;
    let mut startup = StartupControl::new(epoch, sequence, true, Duration::ZERO);
    let mut capture_startup = None;
    let mut start_capture = Some(start.capture);
    let mut tick = tokio::time::interval(SESSION_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Start capture only after the receiver is ready, so it never queues input waiting for a peer.
    let mut capture = loop {
        tokio::select! {
            biased;
            result=poll_capture_startup(&mut capture_startup, &mut tick, &cancel)=>{
                if let Some(capture)=result? {break capture;}
                require_trial_active(trial_guard.as_ref())?;
                io.check()?;
                if let Some(frame) = startup.poll(origin.elapsed()).map_err(startup_failure)? {
                    io.send(frame)?;
                }
                if capture_startup.is_none() && startup.can_prepare_source(origin.elapsed()).map_err(startup_failure)? && trial_permits_start(trial_guard.as_ref()) {
                    let capture_cancel = cancel.clone();
                    let startup_displays = expected_displays.clone();
                    let startup_guard = trial_guard.clone();
                    let start_capture = start_capture.take().ok_or(SessionFailure::Startup(
                        SessionStartupFailure::CaptureTaskUnavailable,
                    ))?;
                    capture_startup = Some(tokio::task::spawn_blocking(move || {
                        require_trial_active(startup_guard.as_ref())?;
                        start_after_display_check(
                            &startup_displays,
                            || crate::session_native::current_displays(source_device).map_err(|_| SessionFailure::Startup(SessionStartupFailure::DisplayUnavailable)),
                            || start_capture(capture_cancel),
                        )
                    }));
                }
            }
            frame=io.next_frame()=>{
                let frame=frame?;
                if let Some(response)=startup.receive(&frame,origin.elapsed()).map_err(startup_failure)? {
                    io.send(response)?;
                }
            }
        }
    };
    if !crate::session_native::current_displays(source_device)
        .map_err(|_| SessionFailure::Startup(SessionStartupFailure::DisplayUnavailable))?
        .same_geometry(&expected_displays)
    {
        return Err(SessionFailure::InvalidLayout);
    }
    if let Some(reason) = capture.stop_reason() {
        return Err(SessionFailure::Startup(
            SessionStartupFailure::CaptureStopped(reason),
        ));
    }
    if capture.is_finished() {
        return Err(SessionFailure::Startup(SessionStartupFailure::CaptureEnded));
    }
    if cancel.is_stopping() {
        return Err(SessionFailure::Revoked);
    }
    require_trial_active(trial_guard.as_ref())?;
    io.check()?;
    io.send(
        startup
            .announce_ready(origin.elapsed())
            .map_err(startup_failure)?,
    )?;
    let control = startup
        .into_ready(origin.elapsed())
        .map_err(startup_failure)?;
    // Both sides advertised readiness over a live capture: the trial's active phase starts here.
    if let Some(guard) = trial_guard.as_ref() {
        guard.begin_active();
    }
    let mut controller =
        SourceController::after_startup(topology, source_device, initial_display, control)
            .map_err(|_| SessionFailure::InvalidLayout)?;
    progress.mark_started();
    let mut pending: Option<(SourceEffect, Duration)> = None;
    let mut submitted: Option<(u64, Duration)> = None;
    let mut last_renewed = origin.elapsed();
    let mut last_display_check = origin.elapsed();
    let mut last_pointer_poll = origin.elapsed();
    let mut last_polled_pointer = None;
    let started_at = origin.elapsed();
    let mut gaps = TickGap::new(started_at);
    // Captured input wakes the loop the moment it is queued; the tick only carries housekeeping.
    let captured = std::sync::Arc::new(tokio::sync::Notify::new());
    {
        let notify = std::sync::Arc::clone(&captured);
        capture.set_waker(std::sync::Arc::new(move || notify.notify_one()));
    }
    macro_rules! drain_captured {
        () => {
            for _ in 0..256 {
                let Some(record) = capture
                    .try_next_event()
                    .map_err(|_| SessionFailure::Native)?
                else {
                    break;
                };
                let Some(record) = normalize(record, controller.motion_target())? else {
                    continue;
                };
                let outcome = controller.on_captured(record, origin.elapsed());
                apply_outcome(
                    outcome,
                    &mut controller,
                    &mut capture,
                    &io,
                    generation,
                    &origin,
                    &mut pending,
                    &mut submitted,
                    trial_guard.as_ref(),
                )?;
            }
        };
    }
    let result=async {
        loop {
            tokio::select! {
                biased;
                _=captured.notified()=>{
                    if cancel.is_stopping(){note_capture_stop(&capture);return Err(SessionFailure::Revoked);}
                    drain_captured!();
                }
                _=tick.tick()=>{
                    gaps.observe(origin.elapsed());
                    if cancel.is_stopping(){note_capture_stop(&capture);return Err(SessionFailure::Revoked);}
                    require_trial_active(trial_guard.as_ref())?;
                    io.check()?;
                    if capture.is_finished()||capture.stop_reason().is_some(){log::warn!("source capture ended on its own: finished={}, stop reason {:?}",capture.is_finished(),capture.stop_reason());return Err(SessionFailure::Native);}
                    if origin.elapsed().saturating_sub(last_display_check)>=Duration::from_millis(30) {
                        if !crate::session_native::current_displays(source_device).map_err(|_|SessionFailure::Native)?.same_geometry(&expected_displays){log::warn!("source display geometry changed during the session");return Err(SessionFailure::InvalidLayout);}
                        last_display_check=origin.elapsed();
                    }
                    let target=controller.motion_target().map(|motion|motion.target);
                    progress.route(target.map(|target|target.display),target.is_some_and(|target|target.machine==source_device));
                    progress.hold(controller.held_since().is_some());
                    if let Some((ticket,issued))=submitted {
                        match capture.completed_control_revision() {
                            Ok(completed) if completed>=ticket=>submitted=None,
                            Ok(completed)=>if origin.elapsed().saturating_sub(issued)>=Duration::from_millis(120){log::warn!("capture control revision {ticket} not applied within 120 ms (completed {completed})");return Err(SessionFailure::Native);},
                            Err(error)=>{log::warn!("capture control revision unavailable: {error:?}");return Err(SessionFailure::Native);}
                        }
                    }
                    apply_outcome(controller.tick(origin.elapsed()),&mut controller,&mut capture,&io,generation,&origin,&mut pending,&mut submitted,trial_guard.as_ref())?;
                    if let Some((effect,since))=pending.take() {
                        if origin.elapsed().saturating_sub(since)>=Duration::from_millis(120){log::warn!("route change not accepted by capture within 120 ms");return Err(SessionFailure::Native);}
                        if !apply_route(&effect,&mut controller,&mut capture,generation,&origin,&mut submitted,trial_guard.as_ref())?{pending=Some((effect,since));}
                    }
                    drain_captured!();
                    if controller.mode()==SourceMode::Local && !controller.capture_route().0 && pending.is_none() && origin.elapsed().saturating_sub(last_pointer_poll)>=POINTER_POLL_INTERVAL {
                        last_pointer_poll=origin.elapsed();
                        if let Some(point)=crate::session_native::current_pointer_position().filter(|point|last_polled_pointer!=Some(*point)) {
                            last_polled_pointer=Some(point);
                            apply_outcome(controller.observe_pointer(point,origin.elapsed()),&mut controller,&mut capture,&io,generation,&origin,&mut pending,&mut submitted,trial_guard.as_ref())?;
                        }
                    }
                    if controller.held_since().is_none() && controller.capture_route().0 && pending.is_none() && submitted.is_none() && origin.elapsed().saturating_sub(last_renewed)>=Duration::from_millis(30) {
                        require_trial_active(trial_guard.as_ref())?;
                        let ttl=lease_budget(&mut controller,origin.elapsed())?;
                        match capture.renew_suppression(generation,ttl) {
                            Ok(ticket)=>{submitted=Some((ticket,origin.elapsed()));last_renewed=origin.elapsed();}
                            Err(NativeCaptureError::ControlPending)=>{},
                            Err(error)=>{log::warn!("capture suppression renewal failed: {error:?}");return Err(SessionFailure::Native);}
                        }
                    }
                }
                frame=io.next_frame()=>{
                    if cancel.is_stopping(){note_capture_stop(&capture);return Err(SessionFailure::Revoked);}
                    require_trial_active(trial_guard.as_ref())?;
                    let frame=frame?;
                    progress.received(&frame);
                    let outcome=controller.on_remote_frame(&frame,origin.elapsed());
                    apply_outcome(outcome,&mut controller,&mut capture,&io,generation,&origin,&mut pending,&mut submitted,trial_guard.as_ref())?;
                }
            }
        }
    }.await;
    capture.request_stop();
    let (holds, held_max) = controller.hold_stats(origin.elapsed());
    progress.record_link(io.link_stats(
        session_millis(Some(started_at), origin.elapsed()),
        gaps.max_micros(),
        (u64::from(holds), millis_u64(held_max)),
        &cancel,
    ));
    io.close_after(&result, progress.ended_deliberately());
    drop(io);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    while !capture.is_finished() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(SESSION_POLL_INTERVAL).await;
    }
    if capture.finish().is_err() {
        log::error!("source session ended with native cleanup pending");
        return Err(SessionFailure::NativeCleanup);
    }
    lifetime.disarm();
    match &result {
        Ok(()) => log::info!("source session ended by the peer"),
        Err(error) => log::warn!("source session ended: {error:?}"),
    }
    result
}

/// A paused startup phase is not a failure, so only a revoked trial ends the session.
fn require_trial_active(trial_guard: Option<&TrialInputGuard>) -> Result<(), SessionFailure> {
    if trial_guard.is_none_or(TrialInputGuard::is_live) {
        Ok(())
    } else {
        Err(SessionFailure::Revoked)
    }
}

/// Capture never starts behind a window the user cannot see, but waiting costs nothing yet.
fn trial_permits_start(trial_guard: Option<&TrialInputGuard>) -> bool {
    trial_guard.is_none_or(TrialInputGuard::allows_new_input)
}

async fn poll_capture_startup<T>(
    task: &mut Option<tokio::task::JoinHandle<Result<T, SessionFailure>>>,
    tick: &mut tokio::time::Interval,
    cancel: &RevocationSignal,
) -> Result<Option<T>, SessionFailure> {
    // Native failure revokes before returning. Preserve a completed error without waiting for
    // a pending worker or admitting successful capture after revocation.
    tokio::select! {
        biased;
        result = async {
            match task.as_mut() {
                Some(task) => task.await,
                None => std::future::pending().await,
            }
        } => {
            let capture = result.map_err(|_| SessionFailure::Startup(
                SessionStartupFailure::CaptureWorkerUnavailable,
            ))??;
            if cancel.is_stopping() { Err(SessionFailure::Revoked) } else { Ok(Some(capture)) }
        }
        _ = tick.tick() => {
            if cancel.is_stopping() { Err(SessionFailure::Revoked) } else { Ok(None) }
        }
    }
}

fn start_after_display_check<T>(
    expected: &DisplayTopology,
    read_current: impl FnOnce() -> Result<DisplayTopology, SessionFailure>,
    start: impl FnOnce() -> Result<T, SessionFailure>,
) -> Result<T, SessionFailure> {
    if !read_current()?.same_geometry(expected) {
        return Err(SessionFailure::InvalidLayout);
    }
    start()
}

/// The two platform error enums are distinct types, so the arms they share are written once here
/// and each caller appends only its platform-only tail.
macro_rules! shared_capture_start_arms {
    ($error:expr, $($tail:tt)*) => {
        match $error {
            NativeCaptureError::AlreadyActive => NativeCaptureStartupFailure::AlreadyActive,
            NativeCaptureError::TrialWindowNotForeground => {
                NativeCaptureStartupFailure::TrialWindowNotForeground
            }
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
fn native_capture_start_failure(error: NativeCaptureError) -> SessionFailure {
    use monhop_platform_windows::native_capture::NativeCaptureError;

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
fn native_capture_start_failure(error: NativeCaptureError) -> SessionFailure {
    use monhop_platform_macos::native_capture::NativeCaptureError;

    let failure = shared_capture_start_arms!(
        error,
        NativeCaptureError::Mac(_)
        | NativeCaptureError::Clock(_)
        | NativeCaptureError::Power(_) => NativeCaptureStartupFailure::Platform,
    );
    SessionFailure::NativeCaptureStartup(failure)
}

// The blocking startup worker also observes this signal if its awaiting future is cancelled.
// A normal return disarms it, so the endpoint outlives the session for its close to leave.
struct RevokeOnDrop(Option<RevocationSignal>);
impl RevokeOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for RevokeOnDrop {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            signal.revoke();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_outcome(
    outcome: SourceOutcome,
    controller: &mut SourceController,
    capture: &mut NativeCapture,
    io: &SessionIo,
    generation: u64,
    origin: &SessionClock,
    pending: &mut Option<(SourceEffect, Duration)>,
    submitted: &mut Option<(u64, Duration)>,
    trial_guard: Option<&TrialInputGuard>,
) -> Result<(), SessionFailure> {
    require_trial_active(trial_guard)?;
    if let Some(failure) = outcome.failure {
        capture.request_stop();
        return Err(SessionFailure::SourceController(failure));
    }
    for effect in outcome.effects.iter() {
        match effect {
            SourceEffect::RemoteFrame(frame) => io.send(frame.clone())?,
            _ => {
                if pending.is_some() {
                    return Err(SessionFailure::Source);
                }
                if !apply_route(
                    effect,
                    controller,
                    capture,
                    generation,
                    origin,
                    submitted,
                    trial_guard,
                )? {
                    *pending = Some((effect.clone(), origin.elapsed()));
                }
            }
        }
    }
    Ok(())
}

fn lease_budget(
    controller: &mut SourceController,
    now: Duration,
) -> Result<Duration, SessionFailure> {
    let ttl = controller
        .suppression_budget(now)
        .map_err(SessionFailure::SourceController)?;
    ttl.checked_sub(Duration::from_millis(1))
        .filter(|ttl| !ttl.is_zero())
        .ok_or(SessionFailure::Source)
}

fn apply_route(
    effect: &SourceEffect,
    controller: &mut SourceController,
    capture: &mut NativeCapture,
    generation: u64,
    origin: &SessionClock,
    submitted: &mut Option<(u64, Duration)>,
    trial_guard: Option<&TrialInputGuard>,
) -> Result<bool, SessionFailure> {
    require_trial_active(trial_guard)?;
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
        Err(_) => {
            controller.fail_capture_route_submission(request, origin.elapsed());
            Err(SessionFailure::Native)
        }
    }
}

fn normalize(
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
        CaptureEvent::Button { button, pressed } => NormalizedInput::Button { button, pressed },
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
            let Some(target) = target else {
                return Ok(None);
            };
            let divisor = if target.platform == Platform::MacOs {
                target.scale_factor
            } else {
                1.0
            };
            if !divisor.is_finite() || divisor <= 0.0 {
                return Err(SessionFailure::InvalidLayout);
            }
            NormalizedInput::RelativeMotion(Point::new(
                f64::from(dx) / divisor,
                f64::from(dy) / divisor,
            ))
        }
        CaptureEvent::LogicalRelativeMotion { dx, dy } => {
            let Some(target) = target else {
                return Ok(None);
            };
            let multiplier = if target.platform == Platform::Windows {
                target.scale_factor
            } else {
                1.0
            };
            if !multiplier.is_finite() || multiplier <= 0.0 {
                return Err(SessionFailure::InvalidLayout);
            }
            NormalizedInput::RelativeMotion(Point::new(dx * multiplier, dy * multiplier))
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
    };
    Ok(Some(TaggedInput {
        event,
        routing_revision: record.routing_revision,
        remote: record.remote,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_protocol::DisplayDescription;

    #[tokio::test]
    async fn completed_capture_failure_survives_a_simultaneous_revoked_tick() {
        let failure =
            SessionFailure::NativeCaptureStartup(NativeCaptureStartupFailure::DesktopUnavailable);
        let mut task = Some(tokio::spawn(async move { Err::<(), _>(failure) }));
        while !task.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        let cancel = RevocationSignal::default();
        cancel.revoke();
        let mut tick = tokio::time::interval(SESSION_POLL_INTERVAL);
        assert_eq!(
            poll_capture_startup(&mut task, &mut tick, &cancel).await,
            Err(failure)
        );
    }

    #[tokio::test]
    async fn cancellation_does_not_wait_for_a_pending_capture_worker() {
        let mut task = Some(tokio::spawn(std::future::pending::<
            Result<(), SessionFailure>,
        >()));
        let cancel = RevocationSignal::default();
        cancel.revoke();
        let mut tick = tokio::time::interval(SESSION_POLL_INTERVAL);
        assert_eq!(
            poll_capture_startup(&mut task, &mut tick, &cancel).await,
            Err(SessionFailure::Revoked),
        );
        assert!(!task.as_ref().unwrap().is_finished());
        task.unwrap().abort();
    }

    #[tokio::test]
    async fn completed_capture_success_is_not_admitted_after_revocation() {
        let mut task = Some(tokio::spawn(async { Ok::<(), SessionFailure>(()) }));
        while !task.as_ref().unwrap().is_finished() {
            tokio::task::yield_now().await;
        }
        let cancel = RevocationSignal::default();
        cancel.revoke();
        let mut tick = tokio::time::interval(SESSION_POLL_INTERVAL);
        assert_eq!(
            poll_capture_startup(&mut task, &mut tick, &cancel).await,
            Err(SessionFailure::Revoked),
        );
    }

    fn display_topology(width: u32) -> DisplayTopology {
        DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(7),
            name: "Display".to_owned(),
            native_width: width,
            native_height: 1080,
            logical_origin: Point::new(0.0, 0.0),
            logical_size: Point::new(f64::from(width), 1080.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap()
    }

    #[test]
    fn changed_or_unreadable_displays_prevent_native_startup() {
        let expected = display_topology(1920);
        for (observed, failure) in [
            (Ok(display_topology(2560)), SessionFailure::InvalidLayout),
            (
                Err(SessionFailure::Startup(
                    SessionStartupFailure::DisplayUnavailable,
                )),
                SessionFailure::Startup(SessionStartupFailure::DisplayUnavailable),
            ),
        ] {
            assert_eq!(
                start_after_display_check::<()>(
                    &expected,
                    || observed,
                    || panic!("native startup must not run")
                ),
                Err(failure)
            );
        }
    }

    #[test]
    fn display_read_completes_before_native_startup() {
        let expected = display_topology(1920);
        let checked = std::cell::Cell::new(false);
        let result = start_after_display_check(
            &expected,
            || {
                checked.set(true);
                Ok(expected.clone())
            },
            || {
                assert!(checked.get());
                Err::<(), _>(SessionFailure::NativeStartup)
            },
        );
        assert_eq!(result, Err(SessionFailure::NativeStartup));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn capture_start_error_retains_its_bounded_category() {
        assert_eq!(
            native_capture_start_failure(NativeCaptureError::StartFailed),
            SessionFailure::NativeCaptureStartup(NativeCaptureStartupFailure::StartFailed)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_trial_window_behind_others_reports_focus_rather_than_a_platform_fault() {
        assert_eq!(
            native_capture_start_failure(NativeCaptureError::TrialWindowNotForeground),
            SessionFailure::NativeCaptureStartup(
                NativeCaptureStartupFailure::TrialWindowNotForeground,
            )
        );
    }

    #[cfg(windows)]
    #[test]
    fn trial_focus_and_windows_operation_stay_redacted_and_distinct() {
        assert_eq!(
            native_capture_start_failure(NativeCaptureError::TrialWindowNotForeground),
            SessionFailure::NativeCaptureStartup(
                NativeCaptureStartupFailure::TrialWindowNotForeground,
            )
        );
        assert_eq!(
            native_capture_start_failure(NativeCaptureError::Windows {
                operation: "capture startup test",
                code: 5,
            }),
            SessionFailure::NativeCaptureStartup(NativeCaptureStartupFailure::WindowsOperation {
                operation: "capture startup test",
                code: 5,
            },)
        );
    }

    #[test]
    fn remote_hook_absolute_motion_cannot_duplicate_raw_motion() {
        assert!(
            normalize(
                CapturedEvent {
                    event: CaptureEvent::AbsoluteMotion { x: 10, y: 20 },
                    routing_revision: 3,
                    remote: true
                },
                None
            )
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn high_resolution_wheel_values_preserve_fraction_and_direction() {
        let record = normalize(
            CapturedEvent {
                event: CaptureEvent::Scroll {
                    horizontal: 30,
                    vertical: -1,
                },
                routing_revision: 0,
                remote: false,
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
}
