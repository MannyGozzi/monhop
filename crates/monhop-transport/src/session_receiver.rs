//! Input admission and native completion precede acknowledgement to the paired source.

use crate::session_clock::millis_u64;
use crate::session_health::{HOLD_LIMIT, HealthError, PEER_LIVENESS, PeerHealth, hold_stats};
use crate::session_startup::ReadyControl;
use monhop_core::{DisplayId, HidUsage, ModifierState, MouseButton, Point};
use monhop_protocol::{
    DeliveryClass, DisplayTopology, Frame, Message, Motion, RateLimiter, SequenceGate,
    SequenceGateError, SessionEpoch,
};
use std::time::Duration;

#[derive(Clone, Copy)]
pub enum DestinationAction {
    MoveTo(Point),
    Key { usage: HidUsage, pressed: bool },
    Button { button: MouseButton, pressed: bool },
    Scroll { horizontal: f64, vertical: f64 },
    ReleaseAll,
}

/// Implementations own their injected pressed state and retry incomplete release operations.
pub trait InputDestination {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestinationFailure;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiverFailure {
    Health(HealthError),
    InvalidEpoch,
    InvalidSequence,
    RateLimited,
    WrongDisplay,
    InvalidPoint,
    NotActive,
    InvalidPressedState,
    UnexpectedMessage,
    NativeDelivery,
    PeerStopped,
}

/// Construct only after matching SessionSetup proposals, full peer authentication and local enable.
pub struct InputReceiver {
    displays: DisplayTopology,
    epoch: SessionEpoch,
    sequences: SequenceGate,
    control_epoch: SessionEpoch,
    control_sequences: SequenceGate,
    limiter: RateLimiter,
    health: PeerHealth,
    active: Option<(DisplayId, Point)>,
    /// Validated pointer motion waits here so a burst injects once; anything else flushes it first.
    pending_move: Option<Point>,
    keys: [bool; 256],
    buttons: [bool; 5],
    failure: Option<ReceiverFailure>,
    cleanup_pending: bool,
    /// Set while the source has been silent past the deadline and its barrier has not arrived.
    held_since: Option<Duration>,
    holds: u32,
    held_max: Duration,
}

impl InputReceiver {
    pub fn new(displays: DisplayTopology, epoch: SessionEpoch, now: Duration) -> Self {
        let mut sequences = SequenceGate::default();
        sequences
            .activate_epoch(epoch)
            .expect("fresh sequence gate");
        let mut control_sequences = SequenceGate::default();
        control_sequences
            .activate_epoch(epoch)
            .expect("fresh control gate");
        Self {
            displays,
            epoch,
            sequences,
            control_epoch: epoch,
            control_sequences,
            limiter: RateLimiter::new(20_000).expect("bounded input rate"),
            health: PeerHealth::new(now),
            active: None,
            pending_move: None,
            keys: [false; 256],
            buttons: [false; 5],
            failure: None,
            cleanup_pending: false,
            held_since: None,
            holds: 0,
            held_max: Duration::ZERO,
        }
    }

    pub(crate) fn after_startup(
        displays: DisplayTopology,
        mut control: ReadyControl,
        now: Duration,
    ) -> Result<Self, ReceiverFailure> {
        control.health.check(now).map_err(ReceiverFailure::Health)?;
        let mut receiver = Self::new(displays, control.epoch, now);
        receiver
            .control_sequences
            .seed_heartbeat(control.last_heartbeat_in);
        receiver.health = control.health;
        receiver.limiter = control.limiter;
        Ok(receiver)
    }

    pub fn epoch(&self) -> SessionEpoch {
        self.epoch
    }
    pub fn control_epoch(&self) -> SessionEpoch {
        self.control_epoch
    }
    pub fn is_active(&self) -> bool {
        self.active.is_some() && self.failure.is_none()
    }
    pub fn active_display(&self) -> Option<DisplayId> {
        if self.failure.is_some() {
            None
        } else {
            self.active.map(|(display, _)| display)
        }
    }
    pub fn cleanup_pending(&self) -> bool {
        self.cleanup_pending
    }
    pub fn held_since(&self) -> Option<Duration> {
        self.held_since
    }
    /// How many holds this session survived and the longest one, including one still running.
    pub fn hold_stats(&self, now: Duration) -> (u32, Duration) {
        hold_stats(self.holds, self.held_max, self.held_since, now)
    }
    pub fn failure(&self) -> Option<ReceiverFailure> {
        self.failure
    }

    /// Time since the peer's last fresh reply, and the age of the unanswered challenge if any.
    pub fn reply_ages(&self, now: Duration) -> (Duration, Option<Duration>) {
        (
            now.saturating_sub(self.health.last_response()),
            self.health
                .pending_since()
                .map(|sent| now.saturating_sub(sent)),
        )
    }

    /// Injects the last validated pointer position of a motion burst.
    pub fn flush(
        &mut self,
        destination: &mut impl InputDestination,
    ) -> Result<(), ReceiverFailure> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        match self.apply_pending_move(destination) {
            Ok(()) => Ok(()),
            Err(failure) => Err(self.stop(failure, destination)),
        }
    }

    fn apply_pending_move(
        &mut self,
        destination: &mut impl InputDestination,
    ) -> Result<(), ReceiverFailure> {
        if let Some(point) = self.pending_move {
            destination
                .apply(DestinationAction::MoveTo(point))
                .map_err(|_| ReceiverFailure::NativeDelivery)?;
            self.pending_move = None;
        }
        Ok(())
    }

    pub fn tick(
        &mut self,
        now: Duration,
        destination: &mut impl InputDestination,
    ) -> Result<Option<Message>, ReceiverFailure> {
        if let Some(failure) = self.failure {
            self.retry_cleanup(destination);
            return Err(failure);
        }
        if let Some(since) = self.held_since {
            if now.saturating_sub(since) >= HOLD_LIMIT {
                let failure = ReceiverFailure::Health(HealthError::DeadlineExpired);
                return Err(self.stop(failure, destination));
            }
            self.retry_cleanup(destination);
            return match self.health.poll_while_held(now) {
                Ok(token) => Ok(token.map(Message::Ping)),
                Err(error) => Err(self.stop(ReceiverFailure::Health(error), destination)),
            };
        }
        match self.health.poll(now) {
            Ok(token) => Ok(token.map(Message::Ping)),
            Err(HealthError::DeadlineExpired) => {
                self.hold(now, destination)?;
                Ok(None)
            }
            Err(error) => Err(self.stop(ReceiverFailure::Health(error), destination)),
        }
    }

    /// No fresh reply for the whole deadline: release everything this side injected and wait
    /// for the source's barrier instead of ending the session.
    fn hold(
        &mut self,
        now: Duration,
        destination: &mut impl InputDestination,
    ) -> Result<(), ReceiverFailure> {
        log::warn!("receiver held: no fresh reply for {PEER_LIVENESS:?}; injected input released");
        self.active = None;
        self.pending_move = None;
        self.cleanup_pending = true;
        self.retry_cleanup(destination);
        if let Err(error) = self.health.hold(now) {
            return Err(self.stop(ReceiverFailure::Health(error), destination));
        }
        self.held_since = Some(now);
        self.holds = self.holds.saturating_add(1);
        Ok(())
    }

    fn resume(&mut self, now: Duration) -> Result<(), ReceiverFailure> {
        if let Some(since) = self.held_since.take() {
            let held = now.saturating_sub(since);
            self.held_max = self.held_max.max(held);
            log::info!("receiver resumed after {} ms", held.as_millis());
        }
        self.health.resume(now).map_err(ReceiverFailure::Health)
    }

    pub fn receive(
        &mut self,
        frame: &Frame,
        now: Duration,
        destination: &mut impl InputDestination,
    ) -> Result<Option<Message>, ReceiverFailure> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        let result = self.receive_active(frame, now, destination);
        match result {
            Ok(response) => Ok(response),
            Err(failure) => Err(self.stop(failure, destination)),
        }
    }

    /// `held` means the link has stalled: nothing is injected and no challenge is answered, but
    /// frames are still validated and tracked so the source's ReleaseAll, the barrier that ends
    /// the hold, still validates when it arrives.
    fn receive_active(
        &mut self,
        frame: &Frame,
        now: Duration,
        destination: &mut impl InputDestination,
    ) -> Result<Option<Message>, ReceiverFailure> {
        let mut held = self.held_since.is_some();
        if !held && let Err(error) = self.health.check(now) {
            if error != HealthError::DeadlineExpired {
                return Err(ReceiverFailure::Health(error));
            }
            // A frame arriving just past the deadline is the first sign of the stall.
            self.hold(now, destination)?;
            held = true;
        }
        self.limiter
            .allow_at(millis_u64(now))
            .map_err(|_| ReceiverFailure::RateLimited)?;
        if matches!(frame.message, Message::Ping(_) | Message::Pong(_)) {
            // Datagrams may arrive out of order: a stale heartbeat is dropped, never answered or trusted.
            if self.control_sequences.accept_heartbeat(frame).is_err() {
                return Ok(None);
            }
            return match frame.message {
                Message::Ping(token) => Ok((!held).then_some(Message::Pong(token))),
                Message::Pong(token) => {
                    if held {
                        self.health
                            .receive_pong_while_held(token, now)
                            .map_err(ReceiverFailure::Health)?;
                    } else {
                        self.health
                            .receive_pong(token, now)
                            .map_err(ReceiverFailure::Health)?;
                    }
                    Ok(None)
                }
                _ => unreachable!(),
            };
        }
        if let Message::ActivateDisplayAt {
            display_id,
            position,
        } = frame.message
        {
            if self.epoch.get().checked_add(1) != Some(frame.epoch.get()) {
                return Err(ReceiverFailure::InvalidEpoch);
            }
            self.check_point(display_id, position)?;
            self.sequences
                .activate_epoch(frame.epoch)
                .map_err(|_| ReceiverFailure::InvalidEpoch)?;
            self.sequences
                .accept(frame)
                .map_err(|_| ReceiverFailure::InvalidSequence)?;
            if held {
                self.epoch = frame.epoch;
                return Ok(None);
            }
            // Moving between displays on this destination preserves an in-progress drag.
            self.pending_move = None;
            destination
                .apply(DestinationAction::MoveTo(position))
                .map_err(|_| ReceiverFailure::NativeDelivery)?;
            self.epoch = frame.epoch;
            self.active = Some((display_id, position));
            return Ok(Some(Message::ActivationAck(display_id)));
        }
        let datagram = frame.delivery() == DeliveryClass::MotionDatagram;
        if frame.epoch != self.epoch {
            // A datagram from the epoch just left is late, not hostile; anything else is fatal.
            if datagram && frame.epoch < self.epoch {
                return Ok(None);
            }
            return Err(ReceiverFailure::InvalidEpoch);
        }
        if let Err(error) = self.sequences.accept(frame) {
            if datagram && error == SequenceGateError::StaleOrReplay {
                return Ok(None);
            }
            return Err(ReceiverFailure::InvalidSequence);
        }
        if !held && !matches!(frame.message, Message::Motion(_)) {
            self.apply_pending_move(destination)?;
        }
        match &frame.message {
            Message::ReleaseAll => {
                destination
                    .apply(DestinationAction::ReleaseAll)
                    .map_err(|_| ReceiverFailure::NativeDelivery)?;
                self.keys.fill(false);
                self.buttons.fill(false);
                self.active = None;
                if held {
                    self.cleanup_pending = false;
                    self.resume(now)?;
                }
                Ok(Some(Message::ReleaseAck))
            }
            Message::Disconnect(_) | Message::Error(_) => Err(ReceiverFailure::PeerStopped),
            Message::Key(_) | Message::Button(_) | Message::Scroll(_) | Message::Motion(_)
                if held =>
            {
                Ok(None)
            }
            Message::Key(key) => {
                self.require_active()?;
                if !key.usage.is_valid() {
                    return Err(ReceiverFailure::InvalidPressedState);
                }
                let index = usize::from(key.usage.0);
                if (key.is_down && key.repeat != self.keys[index])
                    || (!key.is_down && (!self.keys[index] || key.repeat))
                {
                    return Err(ReceiverFailure::InvalidPressedState);
                }
                self.keys[index] = key.is_down;
                if self.modifiers() != key.modifiers {
                    return Err(ReceiverFailure::InvalidPressedState);
                }
                destination
                    .apply(DestinationAction::Key {
                        usage: key.usage,
                        pressed: key.is_down,
                    })
                    .map_err(|_| ReceiverFailure::NativeDelivery)?;
                Ok(None)
            }
            Message::Button(button) => {
                self.require_active()?;
                let index = button.button.index();
                if self.buttons[index] == button.is_down {
                    return Err(ReceiverFailure::InvalidPressedState);
                }
                destination
                    .apply(DestinationAction::Button {
                        button: button.button,
                        pressed: button.is_down,
                    })
                    .map_err(|_| ReceiverFailure::NativeDelivery)?;
                self.buttons[index] = button.is_down;
                Ok(None)
            }
            Message::Scroll(scroll) => {
                self.require_active()?;
                if !scroll.horizontal.is_finite()
                    || !scroll.vertical.is_finite()
                    || scroll.horizontal.abs() > 1_000_000.0
                    || scroll.vertical.abs() > 1_000_000.0
                {
                    return Err(ReceiverFailure::InvalidPoint);
                }
                destination
                    .apply(DestinationAction::Scroll {
                        horizontal: scroll.horizontal,
                        vertical: scroll.vertical,
                    })
                    .map_err(|_| ReceiverFailure::NativeDelivery)?;
                Ok(None)
            }
            Message::Motion(motion) => {
                let (display_id, previous) = self.require_active()?;
                let next = match motion {
                    Motion::Absolute(position) => *position,
                    Motion::Relative(delta) => {
                        Point::new(previous.x + delta.x, previous.y + delta.y)
                    }
                };
                self.check_point(display_id, next)?;
                self.pending_move = Some(next);
                self.active = Some((display_id, next));
                Ok(None)
            }
            _ => Err(ReceiverFailure::UnexpectedMessage),
        }
    }

    pub fn stop(
        &mut self,
        failure: ReceiverFailure,
        destination: &mut impl InputDestination,
    ) -> ReceiverFailure {
        if self.failure.is_none() {
            log::warn!("receiver stopped: {failure:?}");
        }
        let failure = *self.failure.get_or_insert(failure);
        self.active = None;
        self.pending_move = None;
        self.cleanup_pending = true;
        self.retry_cleanup(destination);
        failure
    }

    pub fn retry_cleanup(&mut self, destination: &mut impl InputDestination) {
        if self.cleanup_pending && destination.apply(DestinationAction::ReleaseAll).is_ok() {
            self.keys.fill(false);
            self.buttons.fill(false);
            self.cleanup_pending = false;
        }
    }

    fn require_active(&self) -> Result<(DisplayId, Point), ReceiverFailure> {
        self.active.ok_or(ReceiverFailure::NotActive)
    }

    fn modifiers(&self) -> ModifierState {
        let mut mask = 0;
        for index in 0..8 {
            if self.keys[0xe0 + index] {
                mask |= 1 << index;
            }
        }
        ModifierState(mask)
    }

    fn check_point(&self, id: DisplayId, point: Point) -> Result<(), ReceiverFailure> {
        let display = self
            .displays
            .displays()
            .iter()
            .find(|d| d.id == id)
            .ok_or(ReceiverFailure::WrongDisplay)?;
        if !point.is_finite()
            || point.x < display.logical_origin.x
            || point.y < display.logical_origin.y
            || point.x >= display.logical_origin.x + display.logical_size.x
            || point.y >= display.logical_origin.y + display.logical_size.y
        {
            return Err(ReceiverFailure::InvalidPoint);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_protocol::{DisplayDescription, Key};

    #[derive(Default)]
    struct Destination {
        calls: Vec<DestinationAction>,
        fail_move: bool,
        fail_release: bool,
    }
    impl InputDestination for Destination {
        fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
            self.calls.push(action);
            if (matches!(action, DestinationAction::MoveTo(_)) && self.fail_move)
                || (matches!(action, DestinationAction::ReleaseAll) && self.fail_release)
            {
                return Err(DestinationFailure);
            }
            Ok(())
        }
    }
    fn receiver() -> InputReceiver {
        let displays = DisplayTopology::new(vec![DisplayDescription {
            id: DisplayId(1),
            name: "test".into(),
            native_width: 100,
            native_height: 100,
            logical_origin: Point::new(0.0, 0.0),
            logical_size: Point::new(100.0, 100.0),
            scale_factor: 1.0,
            is_primary: true,
            monitor: None,
        }])
        .unwrap();
        InputReceiver::new(displays, SessionEpoch::new(1).unwrap(), Duration::ZERO)
    }
    fn frame(epoch: u64, sequence: u64, message: Message) -> Frame {
        Frame::new(SessionEpoch::new(epoch).unwrap(), sequence, message)
    }
    fn activate() -> Frame {
        frame(
            2,
            0,
            Message::ActivateDisplayAt {
                display_id: DisplayId(1),
                position: Point::new(10.0, 10.0),
            },
        )
    }
    fn key(down: bool) -> Message {
        Message::Key(Key {
            usage: HidUsage(4),
            is_down: down,
            repeat: false,
            modifiers: ModifierState(0),
        })
    }

    #[test]
    fn activation_ack_requires_successful_native_positioning() {
        let mut receiver = receiver();
        let mut target = Destination {
            fail_move: true,
            ..Default::default()
        };
        assert_eq!(
            receiver.receive(&activate(), Duration::ZERO, &mut target),
            Err(ReceiverFailure::NativeDelivery)
        );
        assert!(!receiver.is_active());
        assert!(matches!(
            target.calls.last(),
            Some(DestinationAction::ReleaseAll)
        ));
    }
    fn inherited_control() -> ReadyControl {
        let mut health = PeerHealth::new(Duration::ZERO);
        let token = health.poll(Duration::ZERO).unwrap().unwrap();
        health.receive_pong(token, Duration::ZERO).unwrap();
        assert_eq!(health.poll(Duration::from_millis(30)), Ok(Some(2)));
        ReadyControl {
            epoch: SessionEpoch::new(1).unwrap(),
            next_incoming: 7,
            next_outgoing: 9,
            next_heartbeat_out: 9,
            last_heartbeat_in: Some(6),
            health,
            limiter: RateLimiter::new(1).unwrap(),
            now: Duration::from_millis(30),
        }
    }

    #[test]
    fn startup_handoff_preserves_challenge_sequence_rate_and_health_deadline() {
        let mut resumed = InputReceiver::after_startup(
            receiver().displays,
            inherited_control(),
            Duration::from_millis(31),
        )
        .unwrap();
        let mut target = Destination::default();
        assert_eq!(
            resumed.receive(
                &frame(1, 7, Message::Pong(2)),
                Duration::from_millis(31),
                &mut target
            ),
            Ok(None)
        );
        assert_eq!(
            resumed.receive(
                &frame(1, 8, Message::Ping(3)),
                Duration::from_millis(31),
                &mut target
            ),
            Err(ReceiverFailure::RateLimited)
        );
        assert!(
            target
                .calls
                .iter()
                .all(|action| matches!(action, DestinationAction::ReleaseAll))
        );
        assert!(matches!(
            InputReceiver::after_startup(
                receiver().displays,
                inherited_control(),
                crate::session_health::PEER_LIVENESS
            ),
            Err(ReceiverFailure::Health(HealthError::DeadlineExpired))
        ));
    }

    #[test]
    fn startup_handoff_drops_a_replayed_heartbeat_and_tolerates_a_lost_one() {
        for (sequence, expected) in [(6, Ok(None)), (8, Ok(Some(Message::Pong(2))))] {
            let mut receiver = InputReceiver::after_startup(
                receiver().displays,
                inherited_control(),
                Duration::from_millis(31),
            )
            .unwrap();
            assert_eq!(
                receiver.receive(
                    &frame(1, sequence, Message::Ping(2)),
                    Duration::from_millis(31),
                    &mut Destination::default()
                ),
                expected
            );
        }
    }

    #[test]
    fn late_motion_datagrams_are_dropped_while_the_ordered_stream_stays_strict() {
        let mut active = receiver();
        let mut target = Destination::default();
        active
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        let motion = |epoch, sequence| {
            frame(
                epoch,
                sequence,
                Message::Motion(Motion::Absolute(Point::new(20.0, 20.0))),
            )
        };
        assert_eq!(
            active.receive(&motion(2, 5), Duration::ZERO, &mut target),
            Ok(None)
        );
        assert_eq!(
            active.receive(&motion(2, 4), Duration::ZERO, &mut target),
            Ok(None)
        );
        assert_eq!(
            active.receive(&motion(1, 6), Duration::ZERO, &mut target),
            Ok(None)
        );
        assert_eq!(
            active.receive(&frame(1, 1, key(true)), Duration::ZERO, &mut target),
            Err(ReceiverFailure::InvalidEpoch)
        );
        let mut fresh = receiver();
        fresh
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        assert_eq!(
            fresh.receive(&frame(2, 0, key(true)), Duration::ZERO, &mut target),
            Err(ReceiverFailure::InvalidSequence)
        );
    }

    #[test]
    fn input_before_activation_is_rejected_and_released() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        assert_eq!(
            receiver.receive(&frame(1, 0, key(true)), Duration::ZERO, &mut target),
            Err(ReceiverFailure::NotActive)
        );
        assert!(
            target
                .calls
                .iter()
                .all(|a| matches!(a, DestinationAction::ReleaseAll))
        );
    }

    #[test]
    fn heartbeat_reply_crossing_an_activation_remains_valid() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        let Some(Message::Ping(token)) = receiver.tick(Duration::ZERO, &mut target).unwrap() else {
            panic!("initial health challenge");
        };
        receiver
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        assert_eq!(
            receiver.receive(
                &frame(1, 0, Message::Pong(token)),
                Duration::from_millis(20),
                &mut target
            ),
            Ok(None)
        );
        assert!(receiver.is_active());
    }

    #[test]
    fn release_ack_is_emitted_only_after_native_release_succeeds() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        receiver
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        target.fail_release = true;
        assert_eq!(
            receiver.receive(
                &frame(2, 1, Message::ReleaseAll),
                Duration::ZERO,
                &mut target
            ),
            Err(ReceiverFailure::NativeDelivery)
        );
        assert!(receiver.cleanup_pending());
    }
    #[test]
    fn stuck_key_is_released_at_the_heartbeat_deadline_without_more_input() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        assert_eq!(
            receiver.receive(&activate(), Duration::ZERO, &mut target),
            Ok(Some(Message::ActivationAck(DisplayId(1))))
        );
        receiver
            .receive(&frame(2, 1, key(true)), Duration::ZERO, &mut target)
            .unwrap();
        assert_eq!(receiver.tick(PEER_LIVENESS, &mut target), Ok(None));
        assert!(matches!(
            target.calls.last(),
            Some(DestinationAction::ReleaseAll)
        ));
        assert!(receiver.held_since().is_some());
        assert!(!receiver.is_active());
        // The release that follows is stale: tracked for its sequence, never injected.
        let injected = target.calls.len();
        assert_eq!(
            receiver.receive(&frame(2, 2, key(false)), PEER_LIVENESS, &mut target),
            Ok(None)
        );
        assert_eq!(target.calls.len(), injected);
    }

    #[test]
    fn a_held_receiver_answers_no_challenge_and_only_the_barrier_ends_the_hold() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        assert_eq!(
            receiver.tick(Duration::ZERO, &mut target),
            Ok(Some(Message::Ping(1)))
        );
        receiver
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        receiver
            .receive(&frame(2, 1, key(true)), Duration::ZERO, &mut target)
            .unwrap();
        let held_at = PEER_LIVENESS;
        assert_eq!(receiver.tick(held_at, &mut target), Ok(None));
        assert_eq!(receiver.hold_stats(held_at).0, 1);
        // Stale motion and a stale activation are tracked without a reply or an injection.
        let injected = target.calls.len();
        let motion = Message::Motion(Motion::Relative(Point::new(1.0, 1.0)));
        assert_eq!(
            receiver.receive(&frame(2, 2, motion), held_at, &mut target),
            Ok(None)
        );
        let reactivate = frame(
            3,
            0,
            Message::ActivateDisplayAt {
                display_id: DisplayId(1),
                position: Point::new(5.0, 5.0),
            },
        );
        assert_eq!(
            receiver.receive(&reactivate, held_at, &mut target),
            Ok(None)
        );
        assert_eq!(receiver.epoch().get(), 3);
        assert!(!receiver.is_active());
        assert_eq!(target.calls.len(), injected);
        // A challenge from the source goes unanswered while held; a reply to a challenge sent
        // before the hold earns nothing.
        assert_eq!(
            receiver.receive(&frame(1, 0, Message::Ping(7)), held_at, &mut target),
            Ok(None)
        );
        assert_eq!(
            receiver.receive(&frame(1, 1, Message::Pong(1)), held_at, &mut target),
            Ok(None)
        );
        assert_eq!(
            receiver.tick(held_at + Duration::from_millis(30), &mut target),
            Ok(Some(Message::Ping(2)))
        );
        // The barrier itself ends the hold, is applied, and is acknowledged.
        let resumed_at = held_at + Duration::from_millis(900);
        assert_eq!(
            receiver.receive(&frame(3, 1, Message::ReleaseAll), resumed_at, &mut target),
            Ok(Some(Message::ReleaseAck))
        );
        assert!(receiver.held_since().is_none());
        assert!(matches!(
            target.calls.last(),
            Some(DestinationAction::ReleaseAll)
        ));
        assert_eq!(
            receiver.hold_stats(resumed_at),
            (1, Duration::from_millis(900))
        );
        // Fresh input after the barrier applies normally once the source activates again.
        assert_eq!(
            receiver.receive(&frame(1, 2, Message::Ping(8)), resumed_at, &mut target),
            Ok(Some(Message::Pong(8)))
        );
        let activate_again = frame(
            4,
            0,
            Message::ActivateDisplayAt {
                display_id: DisplayId(1),
                position: Point::new(1.0, 1.0),
            },
        );
        assert_eq!(
            receiver.receive(&activate_again, resumed_at, &mut target),
            Ok(Some(Message::ActivationAck(DisplayId(1))))
        );
        assert_eq!(
            receiver.receive(&frame(4, 1, key(true)), resumed_at, &mut target),
            Ok(None)
        );
        assert!(matches!(
            target.calls.last(),
            Some(DestinationAction::Key { pressed: true, .. })
        ));
        assert_eq!(
            receiver.tick(
                resumed_at + PEER_LIVENESS - Duration::from_millis(1),
                &mut target
            ),
            Ok(Some(Message::Ping(3)))
        );
    }

    /// Builds the same activated receiver twice, so the hold is the only difference between them.
    fn activated(hold: bool, target: &mut Destination) -> InputReceiver {
        let mut receiver = receiver();
        receiver
            .receive(&activate(), Duration::ZERO, target)
            .unwrap();
        receiver
            .receive(&frame(2, 1, key(true)), Duration::ZERO, target)
            .unwrap();
        if hold {
            assert_eq!(receiver.tick(PEER_LIVENESS, target), Ok(None));
            assert!(receiver.held_since().is_some());
        }
        receiver
    }

    #[test]
    fn a_hold_admits_and_rejects_exactly_the_frames_an_active_receiver_does() {
        let activation = |epoch, display, x| {
            frame(
                epoch,
                0,
                Message::ActivateDisplayAt {
                    display_id: DisplayId(display),
                    position: Point::new(x, 10.0),
                },
            )
        };
        let motion = |epoch, sequence| {
            frame(
                epoch,
                sequence,
                Message::Motion(Motion::Absolute(Point::new(20.0, 20.0))),
            )
        };
        let cases = [
            (
                "unexpected message",
                frame(2, 2, Message::ActivationAck(DisplayId(1))),
                Err(ReceiverFailure::UnexpectedMessage),
            ),
            (
                "stale epoch",
                frame(1, 2, key(true)),
                Err(ReceiverFailure::InvalidEpoch),
            ),
            (
                "future epoch",
                frame(3, 2, key(true)),
                Err(ReceiverFailure::InvalidEpoch),
            ),
            (
                "replayed sequence",
                frame(2, 1, key(false)),
                Err(ReceiverFailure::InvalidSequence),
            ),
            ("late datagram", motion(1, 2), Ok(None)),
            ("replayed datagram", motion(2, 1), Ok(None)),
            (
                "activation out of bounds",
                activation(3, 1, 100.0),
                Err(ReceiverFailure::InvalidPoint),
            ),
            (
                "activation on an unknown display",
                activation(3, 9, 10.0),
                Err(ReceiverFailure::WrongDisplay),
            ),
            (
                "activation skipping an epoch",
                activation(4, 1, 10.0),
                Err(ReceiverFailure::InvalidEpoch),
            ),
        ];
        for (label, probe, expected) in cases {
            let mut target = Destination::default();
            let mut active = activated(false, &mut target);
            let mut held = activated(true, &mut target);
            assert_eq!(
                active.receive(&probe, Duration::ZERO, &mut target),
                expected,
                "{label}, active"
            );
            assert_eq!(
                held.receive(&probe, PEER_LIVENESS, &mut target),
                expected,
                "{label}, held"
            );
        }
    }

    #[test]
    fn a_hold_without_a_barrier_ends_at_the_limit() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        assert_eq!(receiver.tick(PEER_LIVENESS, &mut target), Ok(None));
        let last_tick = PEER_LIVENESS + HOLD_LIMIT - Duration::from_millis(1);
        assert!(receiver.tick(last_tick, &mut target).is_ok());
        assert_eq!(
            receiver.tick(PEER_LIVENESS + HOLD_LIMIT, &mut target),
            Err(ReceiverFailure::Health(HealthError::DeadlineExpired))
        );
        assert_eq!(receiver.hold_stats(PEER_LIVENESS + HOLD_LIMIT).0, 1);
    }

    #[test]
    fn a_failed_release_at_hold_entry_is_retried_on_every_tick() {
        let mut receiver = receiver();
        let mut target = Destination {
            fail_release: true,
            ..Default::default()
        };
        assert_eq!(receiver.tick(PEER_LIVENESS, &mut target), Ok(None));
        assert!(receiver.cleanup_pending());
        target.fail_release = false;
        assert!(
            receiver
                .tick(PEER_LIVENESS + Duration::from_millis(30), &mut target)
                .is_ok()
        );
        assert!(!receiver.cleanup_pending());
    }
    #[test]
    fn failed_release_remains_pending_and_can_be_retried_locally() {
        let mut receiver = receiver();
        let mut target = Destination {
            fail_release: true,
            ..Default::default()
        };
        receiver.stop(ReceiverFailure::PeerStopped, &mut target);
        assert!(receiver.cleanup_pending());
        target.fail_release = false;
        receiver.retry_cleanup(&mut target);
        assert!(!receiver.cleanup_pending());
    }
    #[test]
    fn out_of_bounds_activation_never_reaches_the_injector() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        let activation = frame(
            2,
            0,
            Message::ActivateDisplayAt {
                display_id: DisplayId(1),
                position: Point::new(100.0, 10.0),
            },
        );
        assert_eq!(
            receiver.receive(&activation, Duration::ZERO, &mut target),
            Err(ReceiverFailure::InvalidPoint)
        );
        assert!(
            target
                .calls
                .iter()
                .all(|a| matches!(a, DestinationAction::ReleaseAll))
        );
    }

    fn motion(sequence: u64, dx: f64, dy: f64) -> Frame {
        frame(
            2,
            sequence,
            Message::Motion(Motion::Relative(Point::new(dx, dy))),
        )
    }

    #[test]
    fn a_motion_burst_injects_its_final_position_once_on_flush() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        receiver
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        for (sequence, delta) in [(1, 5.0), (2, 5.0), (3, -2.0)] {
            receiver
                .receive(&motion(sequence, delta, 1.0), Duration::ZERO, &mut target)
                .unwrap();
        }
        assert_eq!(target.calls.len(), 1, "the burst waits for the flush");
        receiver.flush(&mut target).unwrap();
        assert_eq!(target.calls.len(), 2);
        assert!(matches!(
            target.calls[1],
            DestinationAction::MoveTo(point) if point == Point::new(18.0, 13.0)
        ));
        receiver.flush(&mut target).unwrap();
        assert_eq!(
            target.calls.len(),
            2,
            "a flush without motion injects nothing"
        );
    }

    #[test]
    fn keys_and_buttons_land_after_the_pending_motion() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        receiver
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        receiver
            .receive(&motion(1, 5.0, 0.0), Duration::ZERO, &mut target)
            .unwrap();
        receiver
            .receive(&frame(2, 2, key(true)), Duration::ZERO, &mut target)
            .unwrap();
        assert!(matches!(
            target.calls[1],
            DestinationAction::MoveTo(point) if point == Point::new(15.0, 10.0)
        ));
        assert!(matches!(target.calls[2], DestinationAction::Key { .. }));
    }

    #[test]
    fn every_motion_in_a_burst_is_still_bounds_checked() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        receiver
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        receiver
            .receive(&motion(1, 80.0, 0.0), Duration::ZERO, &mut target)
            .unwrap();
        assert_eq!(
            receiver.receive(&motion(2, 20.0, 0.0), Duration::ZERO, &mut target),
            Err(ReceiverFailure::InvalidPoint)
        );
        assert!(
            !target
                .calls
                .iter()
                .any(|a| matches!(a, DestinationAction::MoveTo(point) if point.x >= 90.0))
        );
    }

    #[test]
    fn a_failed_flush_stops_the_receiver_and_releases() {
        let mut receiver = receiver();
        let mut target = Destination::default();
        receiver
            .receive(&activate(), Duration::ZERO, &mut target)
            .unwrap();
        receiver
            .receive(&motion(1, 5.0, 0.0), Duration::ZERO, &mut target)
            .unwrap();
        target.fail_move = true;
        assert_eq!(
            receiver.flush(&mut target),
            Err(ReceiverFailure::NativeDelivery)
        );
        assert!(!receiver.is_active());
        assert!(matches!(
            target.calls.last(),
            Some(DestinationAction::ReleaseAll)
        ));
    }
}
