//! One network coordinator, one capture worker and one independent injection watchdog.
use crate::{
    session::{
        DISPLAY_CHECK_INTERVAL, SESSION_POLL_INTERVAL, SESSION_QUEUE_CAPACITY, SessionFailure,
        SessionHalf, SessionIo, SessionProgress, SessionStartupFailure, TickGap,
        check_read_displays, destination_failure, session_millis, settle_end, worker_end,
    },
    session_actor::DestinationActor,
    session_clock::{SessionClock, millis_u64},
    session_handshake::NegotiatedSession,
    session_native::{
        NativeDestination, current_displays, current_pointer_position, double_click_interval,
    },
    session_source::{SourceController, SourceEffect, SourceMode},
    session_source_runtime::{
        NativeCapture, NativeCaptureError, apply_outcome, apply_route, lease_budget,
        native_capture_start_failure, normalize,
    },
    session_startup::{StartupControl, startup_failure},
    session_wire::is_heartbeat,
};
use monhop_core::{
    FloorOwner, FloorState, NativeSessionClaim, Point, RevocationSignal, SharedFloor, TakeBackGate,
    Topology, capture::MAX_SUPPRESSION_TTL,
};
use monhop_protocol::{Frame, FrameScope, Message, SessionPurpose};
use std::{collections::VecDeque, sync::Arc, time::Duration};
const POINTER_POLL_INTERVAL: Duration = Duration::from_millis(16);
const CLEANUP_WAIT: Duration = Duration::from_millis(250);
const DRAIN_LIMIT: usize = 256;

struct NativeWorkers {
    gate: TakeBackGate,
    cancel: RevocationSignal,
    capture: Option<NativeCapture>,
    starting: Option<tokio::task::JoinHandle<Result<NativeCapture, SessionFailure>>>,
    destination: Option<DestinationActor>,
}
/// What each native worker was doing when `CLEANUP_WAIT` ran out.
struct CleanupPending {
    startup_running: bool,
    /// `CleanupPending` while the worker runs, otherwise how it ended.
    capture: Option<NativeCaptureError>,
    injection_running: bool,
    injected_release_failed: bool,
}
impl NativeWorkers {
    fn stop(&self) {
        self.gate.open_injection(0);
        if let Some(capture) = &self.capture {
            capture.request_stop();
        }
        if let Some(destination) = &self.destination {
            destination.request_stop();
        }
    }
    async fn finish(&mut self) -> Result<(), CleanupPending> {
        self.stop();
        let deadline = tokio::time::Instant::now() + CLEANUP_WAIT;
        loop {
            if self
                .starting
                .as_ref()
                .is_some_and(|task| task.is_finished())
                && let Ok(Ok(capture)) = self.starting.take().expect("completed startup").await
            {
                capture.request_stop();
                self.capture = Some(capture);
            }
            let capture = self.capture.as_mut().and_then(|capture| {
                if capture.is_finished() {
                    capture.finish().err()
                } else {
                    Some(NativeCaptureError::CleanupPending)
                }
            });
            let injection_running = self
                .destination
                .as_mut()
                .is_some_and(|actor| !actor.finish());
            let injected_release_failed = !injection_running
                && self
                    .destination
                    .as_ref()
                    .is_some_and(DestinationActor::cleanup_pending);
            if self.starting.is_none()
                && capture.is_none()
                && !injection_running
                && !injected_release_failed
            {
                self.gate.floor().reset();
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CleanupPending {
                    startup_running: self.starting.is_some(),
                    capture,
                    injection_running,
                    injected_release_failed,
                });
            }
            tokio::time::sleep(SESSION_POLL_INTERVAL).await;
        }
    }
}
impl Drop for NativeWorkers {
    fn drop(&mut self) {
        self.stop();
        // A detached blocking constructor must never admit input after its coordinator disappears.
        if self.starting.is_some() {
            self.cancel.request_stop();
        }
    }
}

pub async fn run_session(
    session: NegotiatedSession,
    topology: Topology,
    generation: u64,
    cancel: RevocationSignal,
    progress: SessionProgress,
) -> Result<(), SessionFailure> {
    if session.purpose() != SessionPurpose::Share {
        return Err(SessionFailure::Destination);
    }
    let local = session.local.device_id;
    let peer = session.peer.device_id;
    let expected_displays = session.local.topology.clone();
    let permissions = session.permissions;
    let outbound_scope = session.outbound_scope();
    let inbound_scope = session.inbound_scope();
    let initial_display = topology
        .displays()
        .find(|d| d.machine == local && d.in_use && d.primary)
        .or_else(|| topology.displays().find(|d| d.machine == local && d.in_use))
        .ok_or(SessionFailure::InvalidLayout)?
        .id;
    let peer_offset = validate_topology(&topology, &session)?;
    let epoch = session.initial_epoch;
    let sequence = session.control.next_sequence();
    let floor = SharedFloor::new();
    let gate = TakeBackGate::new(floor.clone());
    let origin = SessionClock::try_now()?;
    let double_click = double_click_interval();
    let mut source = SourceController::new(
        topology.clone(),
        local,
        initial_display,
        epoch,
        sequence,
        origin.elapsed(),
    )
    .map_err(|_| SessionFailure::InvalidLayout)?
    .with_floor(floor.clone(), permissions.allows(outbound_scope));
    source.set_double_click_interval(double_click);
    let mut outbound = Some(StartupControl::new(epoch, sequence, true, origin.elapsed()));
    let mut inbound = Some(StartupControl::new(
        epoch,
        sequence,
        false,
        origin.elapsed(),
    ));
    let mut io = SessionIo::new(session, progress.clone());
    let mut workers = NativeWorkers {
        gate: gate.clone(),
        cancel: cancel.clone(),
        capture: None,
        starting: None,
        destination: None,
    };
    let mut injection_permit = None;
    let captured = Arc::new(tokio::sync::Notify::new());
    let mut early_input = VecDeque::<Frame>::new();
    let mut pending: Option<(SourceEffect, Duration)> = None;
    let mut submitted: Option<(u64, Duration)> = None;
    let mut last_renewed = origin.elapsed();
    let mut last_pointer_poll = origin.elapsed();
    let mut last_control = None;
    let mut tick = tokio::time::interval(SESSION_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut gaps = TickGap::new(origin.elapsed());
    let mut started_at = None;
    let result = async {
        loop {
            enum Event {
                Tick,
                Capture,
                Frame(Frame),
            }
            let event = tokio::select! {
                _ = tick.tick() => Event::Tick,
                _ = captured.notified() => Event::Capture,
                frame = io.next_frame() => Event::Frame(frame?),
            };
            let now = origin.elapsed();
            if let Some(failure) = worker_end(
                cancel.is_stopping(),
                progress.ended_deliberately(),
                workers
                    .capture
                    .as_ref()
                    .and_then(NativeCapture::stop_reason),
                workers
                    .capture
                    .as_ref()
                    .is_some_and(NativeCapture::is_finished),
                workers
                    .destination
                    .as_ref()
                    .and_then(DestinationActor::failure),
            ) {
                return Err(failure);
            }
            if workers
                .starting
                .as_ref()
                .is_some_and(|task| task.is_finished())
            {
                let capture = workers
                    .starting
                    .take()
                    .expect("completed startup")
                    .await
                    .map_err(|_| {
                        SessionFailure::Startup(SessionStartupFailure::CaptureWorkerUnavailable)
                    })??;
                let notify = Arc::clone(&captured);
                capture.set_waker(Arc::new(move || notify.notify_one()));
                workers.capture = Some(capture);
                let permit = injection_permit.take().ok_or(SessionFailure::Destination)?;
                let native_gate = gate.clone();
                let native_cancel = cancel.clone();
                let displays = expected_displays.clone();
                let actor = DestinationActor::start_after_local_enable(
                    expected_displays.clone(),
                    cancel.clone(),
                    permit,
                    gate.clone(),
                    local < peer,
                    permissions.allows(inbound_scope),
                    move |permit| {
                        NativeDestination::new_after_local_enable(
                            local,
                            displays,
                            permit,
                            native_cancel,
                            native_gate,
                        )
                    },
                )
                .map_err(destination_failure)?;
                let wake = actor.waker();
                let notify = Arc::clone(&captured);
                gate.set_waker(Arc::new(move || {
                    wake();
                    notify.notify_one();
                }));
                let notify = Arc::clone(&captured);
                actor.set_response_waker(Arc::new(move || notify.notify_one()));
                workers.destination = Some(actor);
            }
            let housekeeping = matches!(event, Event::Tick);
            if let Event::Frame(frame) = event {
                if frame.scope == FrameScope::Connection {
                    return if matches!(frame.message, Message::Disconnect(_)) {
                        Ok(())
                    } else {
                        Err(SessionFailure::Wire)
                    };
                }
                progress.received(&frame);
                if let Some(control) = if frame.scope == outbound_scope {
                    outbound.as_mut()
                } else {
                    inbound.as_mut()
                } {
                    if frame.scope == inbound_scope
                        && !is_heartbeat(&frame.message)
                        && !matches!(frame.message, Message::SessionReady)
                        && control.is_ready(now).map_err(startup_failure)?
                    {
                        if early_input.len() == SESSION_QUEUE_CAPACITY {
                            return Err(SessionFailure::QueueFull);
                        }
                        early_input.push_back(frame);
                    } else if let Some(response) =
                        control.receive(&frame, now).map_err(startup_failure)?
                    {
                        if frame.scope == outbound_scope {
                            io.send_outbound(response)?;
                        } else {
                            io.send_inbound(response)?;
                        }
                    }
                } else if frame.scope == outbound_scope {
                    let outcome = source.on_remote_frame(&frame, now);
                    apply_outcome(
                        outcome,
                        &mut source,
                        workers.capture.as_mut().ok_or(SessionFailure::Native)?,
                        &io,
                        generation,
                        &origin,
                        &mut pending,
                        &mut submitted,
                    )?;
                } else {
                    workers
                        .destination
                        .as_ref()
                        .ok_or(SessionFailure::Destination)?
                        .try_submit(frame)
                        .map_err(destination_failure)?;
                }
            }
            if let (Some(out), Some(input)) = (&mut outbound, &mut inbound) {
                if let Some(frame) = out.poll(now).map_err(startup_failure)? {
                    io.send_outbound(frame)?;
                }
                if let Some(frame) = input.poll(now).map_err(startup_failure)? {
                    io.send_inbound(frame)?;
                }
                if workers.capture.is_none()
                    && workers.starting.is_none()
                    && out.health_confirmed(now).map_err(startup_failure)?
                    && input.health_confirmed(now).map_err(startup_failure)?
                {
                    let (capture_permit, inject) = NativeSessionClaim::claim()
                        .ok_or(SessionFailure::Startup(
                            SessionStartupFailure::NativeOwnershipUnavailable,
                        ))?
                        .split();
                    injection_permit = Some(inject);
                    let cancel = cancel.clone();
                    let gate = gate.clone();
                    let displays = expected_displays.clone();
                    workers.starting = Some(tokio::task::spawn_blocking(move || {
                        if cancel.is_stopping() {
                            return Err(SessionFailure::Revoked);
                        }
                        check_read_displays(&displays, current_displays(local))?;
                        NativeCapture::start_for_session(generation, cancel, capture_permit, gate)
                            .map_err(native_capture_start_failure)
                    }));
                }
                if workers.capture.is_some()
                    && workers
                        .destination
                        .as_ref()
                        .is_some_and(DestinationActor::is_native_ready)
                {
                    if input.can_announce_ready(now).map_err(startup_failure)? {
                        io.send_inbound(input.announce_ready(now).map_err(startup_failure)?)?;
                    }
                    if out.can_announce_ready(now).map_err(startup_failure)? {
                        io.send_outbound(out.announce_ready(now).map_err(startup_failure)?)?;
                    }
                }
            }
            let mut capture_drained = false;
            if let Some(capture) = &mut workers.capture {
                source.set_capture_ready(capture.is_ready_for_suppression());
                for _ in 0..DRAIN_LIMIT {
                    let Some(record) = capture
                        .try_next_event()
                        .map_err(|_| SessionFailure::Native)?
                    else {
                        capture_drained = true;
                        break;
                    };
                    let Some(record) = normalize(record, source.motion_target())? else {
                        continue;
                    };
                    if outbound.is_some() {
                        if let Some(failure) = source.bookkeeping(record).failure {
                            return Err(SessionFailure::SourceController(failure));
                        }
                    } else {
                        let outcome = source.on_captured(record, origin.elapsed());
                        apply_outcome(
                            outcome,
                            &mut source,
                            capture,
                            &io,
                            generation,
                            &origin,
                            &mut pending,
                            &mut submitted,
                        )?;
                    }
                }
            }
            if let (Some(out), Some(input)) = (&mut outbound, &mut inbound)
                && capture_drained
                && out.is_ready(origin.elapsed()).map_err(startup_failure)?
                && input.is_ready(origin.elapsed()).map_err(startup_failure)?
            {
                let control = outbound
                    .take()
                    .expect("ready outbound")
                    .into_ready(origin.elapsed())
                    .map_err(startup_failure)?;
                let mut ready_source = SourceController::after_startup(
                    topology.clone(),
                    local,
                    initial_display,
                    control,
                )
                .map_err(SessionFailure::SourceController)?
                .with_floor(floor.clone(), permissions.allows(outbound_scope));
                ready_source.inherit_bookkeeping(&source);
                ready_source.set_peer_offset(peer_offset);
                ready_source.set_double_click_interval(double_click);
                source = ready_source;
                let control = inbound
                    .take()
                    .expect("ready inbound")
                    .into_ready(origin.elapsed())
                    .map_err(startup_failure)?;
                let actor = workers
                    .destination
                    .as_ref()
                    .ok_or(SessionFailure::Destination)?;
                actor
                    .handoff_ready(control, origin.clone())
                    .map_err(destination_failure)?;
                while let Some(frame) = early_input.pop_front() {
                    actor.try_submit(frame).map_err(destination_failure)?;
                }
                started_at = Some(origin.elapsed());
            }
            if let Some(actor) = &mut workers.destination {
                for _ in 0..SESSION_QUEUE_CAPACITY {
                    let Some(frame) = actor.try_response().map_err(destination_failure)? else {
                        break;
                    };
                    io.send_inbound(frame)?;
                }
                if outbound.is_none() && actor.is_started() {
                    progress.mark_started();
                }
            }
            if housekeeping {
                gaps.observe(origin.elapsed());
                // Two quinn lock round trips: the tick, never every captured event, pays for them.
                io.check()?;
                let control = (
                    floor.snapshot().state,
                    source.mode(),
                    source.capture_route().0,
                );
                if last_control != Some(control) {
                    last_control = Some(control);
                    log_control_change(control);
                }
                if outbound.is_none() {
                    let capture = workers.capture.as_mut().ok_or(SessionFailure::Native)?;
                    if let Some((ticket, issued)) = submitted {
                        match capture.completed_control_revision() {
                            Ok(completed) if completed >= ticket => submitted = None,
                            Ok(_) if now.saturating_sub(issued) < MAX_SUPPRESSION_TTL => {}
                            _ => return Err(SessionFailure::Native),
                        }
                    }
                    let outcome = source.tick(origin.elapsed());
                    apply_outcome(
                        outcome,
                        &mut source,
                        capture,
                        &io,
                        generation,
                        &origin,
                        &mut pending,
                        &mut submitted,
                    )?;
                    if let Some((effect, since)) = pending.take() {
                        if origin.elapsed().saturating_sub(since) >= MAX_SUPPRESSION_TTL {
                            return Err(SessionFailure::Native);
                        }
                        if !apply_route(
                            &effect,
                            &mut source,
                            capture,
                            &io,
                            generation,
                            &origin,
                            &mut submitted,
                        )? {
                            pending = Some((effect, since));
                        }
                    }
                    if source.mode() == SourceMode::Local
                        && floor.snapshot().state == FloorState::Free
                        && !source.capture_route().0
                        && pending.is_none()
                        && now.saturating_sub(last_pointer_poll) >= POINTER_POLL_INTERVAL
                    {
                        last_pointer_poll = now;
                        if let Some(point) = current_pointer_position() {
                            let outcome = source.observe_pointer(point, origin.elapsed());
                            apply_outcome(
                                outcome,
                                &mut source,
                                capture,
                                &io,
                                generation,
                                &origin,
                                &mut pending,
                                &mut submitted,
                            )?;
                        }
                    }
                    if source.held_since().is_none()
                        && source.capture_route().0
                        && pending.is_none()
                        && submitted.is_none()
                        && now.saturating_sub(last_renewed) >= DISPLAY_CHECK_INTERVAL
                    {
                        let ttl = lease_budget(&mut source, origin.elapsed())?;
                        match capture.renew_suppression(generation, ttl) {
                            Ok(ticket) => {
                                submitted = Some((ticket, origin.elapsed()));
                                last_renewed = origin.elapsed();
                            }
                            Err(NativeCaptureError::ControlPending) => {}
                            Err(_) => return Err(SessionFailure::Native),
                        }
                    }
                    let inbound_active =
                        floor.snapshot().state.owner() == Some(FloorOwner::Inbound);
                    let target = source.motion_target().map(|motion| motion.target);
                    progress.route(
                        if inbound_active {
                            workers
                                .destination
                                .as_ref()
                                .and_then(DestinationActor::active_display)
                        } else {
                            target.map(|t| t.display)
                        },
                        inbound_active || target.is_some_and(|t| t.machine == local),
                    );
                    progress.active_half(match floor.snapshot().state.owner() {
                        Some(FloorOwner::Inbound) => Some(SessionHalf::Inbound),
                        Some(FloorOwner::Outbound) => Some(SessionHalf::Outbound),
                        None => None,
                    });
                    progress.hold(
                        source.held_since().is_some()
                            || workers
                                .destination
                                .as_ref()
                                .is_some_and(DestinationActor::is_held),
                    );
                }
            }
        }
    }
    .await;
    let result = settle_end(
        result,
        progress.ended_deliberately(),
        workers
            .capture
            .as_ref()
            .and_then(NativeCapture::stop_reason),
        workers
            .destination
            .as_ref()
            .and_then(DestinationActor::failure),
    );
    workers.stop();
    if workers.starting.is_some() {
        cancel.request_stop();
    }
    let (holds, held_max) = source.hold_stats(origin.elapsed());
    let receiver = workers
        .destination
        .as_ref()
        .map(DestinationActor::stats)
        .unwrap_or_default();
    progress.record_receiver(receiver);
    progress.record_link(io.link_stats(
        session_millis(started_at, origin.elapsed()),
        gaps.max_micros(),
        (
            u64::from(holds) + receiver.holds,
            millis_u64(held_max).max(receiver.held_max_millis),
        ),
        &cancel,
    ));
    io.close_after(
        &result,
        progress.ended_deliberately(),
        cancel.is_stopping_for_control_change(),
    );
    drop(io);
    if let Err(pending) = workers.finish().await {
        // NativeCleanup replaces the session's own end, so that end is only visible here.
        log::warn!(
            "session: native cleanup outlasted {CLEANUP_WAIT:?} (startup running {}, capture {}, injection running {}, injected release failed {}) after the session ended with {result:?}",
            pending.startup_running,
            pending
                .capture
                .map_or_else(|| "done".to_owned(), |error| format!("{error:?}")),
            pending.injection_running,
            pending.injected_release_failed,
        );
        return Err(SessionFailure::NativeCleanup);
    }
    result
}

/// One line per control handoff: who holds the floor and where the local pointer sits.
fn log_control_change((floor, mode, capture_remote): (FloorState, SourceMode, bool)) {
    let pointer = current_pointer_position().map_or_else(
        || "unknown".to_owned(),
        |p| format!("{:.0},{:.0}", p.x, p.y),
    );
    log::info!(
        "control: floor {floor:?}, source {mode:?}, capture remote {capture_remote}, pointer {pointer}"
    );
}

/// Only a translation of the peer block is accepted. Wire coordinates stay native on each peer.
fn validate_topology(
    topology: &Topology,
    session: &NegotiatedSession,
) -> Result<Point, SessionFailure> {
    let mut offset = None;
    for (native, device, is_local) in [
        (&session.local.topology, session.local.device_id, true),
        (&session.peer.topology, session.peer.device_id, false),
    ] {
        for d in native.displays() {
            let actual = topology
                .display(d.id)
                .map_err(|_| SessionFailure::InvalidLayout)?;
            if actual.machine != device
                || actual.logical_size.width != d.logical_size.x
                || actual.logical_size.height != d.logical_size.y
                || actual.native_size.width != d.native_width
                || actual.native_size.height != d.native_height
                || actual.scale_factor != f64::from(d.scale_factor)
            {
                return Err(SessionFailure::InvalidLayout);
            }
            let delta = Point::new(
                actual.origin.x - d.logical_origin.x,
                actual.origin.y - d.logical_origin.y,
            );
            if is_local && delta != Point::default() {
                return Err(SessionFailure::InvalidLayout);
            }
            if !is_local {
                if offset.is_some_and(|previous| previous != delta) {
                    return Err(SessionFailure::InvalidLayout);
                }
                offset = Some(delta);
            }
        }
    }
    if topology.displays().count()
        != session.local.topology.displays().len() + session.peer.topology.displays().len()
    {
        return Err(SessionFailure::InvalidLayout);
    }
    offset.ok_or(SessionFailure::InvalidLayout)
}
