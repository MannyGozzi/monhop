//! Control-only startup. Native readiness cannot be inferred from a responsive network peer.

use crate::{
    session::{SessionFailure, SessionStartupFailure, StartupPeerHealthFailure},
    session_clock::millis_u64,
    session_health::{HealthError, PeerHealth},
    session_wire::is_heartbeat,
};
use monhop_protocol::{DeliveryClass, Frame, Message, RateLimiter, SequenceGate, SessionEpoch};
use std::time::Duration;

pub(crate) const STARTUP_DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartupError {
    Health(HealthError),
    Deadline,
    Sequence,
    UnexpectedMessage,
    RateLimited,
    NotReady,
}

impl From<StartupError> for SessionStartupFailure {
    fn from(error: StartupError) -> Self {
        match error {
            StartupError::Health(HealthError::ClockRegression) => {
                Self::PeerHealth(StartupPeerHealthFailure::ClockRegression)
            }
            StartupError::Health(HealthError::DeadlineExpired) => {
                Self::PeerHealth(StartupPeerHealthFailure::DeadlineExpired)
            }
            StartupError::Health(HealthError::UnexpectedResponse) => {
                Self::PeerHealth(StartupPeerHealthFailure::UnexpectedResponse)
            }
            StartupError::Health(HealthError::TokenExhausted) => {
                Self::PeerHealth(StartupPeerHealthFailure::TokenExhausted)
            }
            StartupError::Deadline => Self::Deadline,
            StartupError::Sequence => Self::Sequence,
            StartupError::UnexpectedMessage => Self::UnexpectedMessage,
            StartupError::RateLimited => Self::RateLimited,
            StartupError::NotReady => Self::NotReady,
        }
    }
}

pub(crate) fn startup_failure(error: StartupError) -> SessionFailure {
    SessionFailure::Startup(error.into())
}

pub(crate) struct ReadyControl {
    pub epoch: SessionEpoch,
    pub next_incoming: u64,
    pub next_outgoing: u64,
    /// Heartbeats use their own sequence space because they travel as datagrams.
    pub next_heartbeat_out: u64,
    pub last_heartbeat_in: Option<u64>,
    pub health: PeerHealth,
    pub limiter: RateLimiter,
    pub now: Duration,
}

pub(crate) struct StartupControl {
    control: ReadyControl,
    heartbeats: SequenceGate,
    started_at: Duration,
    local_is_source: bool,
    local_ready: bool,
    peer_ready: bool,
    failure: Option<StartupError>,
}

impl StartupControl {
    pub fn new(
        epoch: SessionEpoch,
        next_sequence: u64,
        local_is_source: bool,
        now: Duration,
    ) -> Self {
        let mut heartbeats = SequenceGate::default();
        heartbeats
            .activate_epoch(epoch)
            .expect("fresh heartbeat gate");
        Self {
            control: ReadyControl {
                epoch,
                next_incoming: next_sequence,
                next_outgoing: next_sequence,
                next_heartbeat_out: 0,
                last_heartbeat_in: None,
                health: PeerHealth::new(now),
                limiter: RateLimiter::new(20_000).expect("bounded startup rate"),
                now,
            },
            heartbeats,
            started_at: now,
            local_is_source,
            local_ready: false,
            peer_ready: false,
            failure: None,
        }
    }

    pub fn poll(&mut self, now: Duration) -> Result<Option<Frame>, StartupError> {
        self.check(now)?;
        let token = self
            .control
            .health
            .poll(now)
            .map_err(|error| self.fail(StartupError::Health(error)))?;
        token
            .map(|token| self.outgoing(Message::Ping(token)))
            .transpose()
    }

    pub fn receive(&mut self, frame: &Frame, now: Duration) -> Result<Option<Frame>, StartupError> {
        self.check(now)?;
        self.control
            .limiter
            .allow_at(millis_u64(now))
            .map_err(|_| self.fail(StartupError::RateLimited))?;
        if is_heartbeat(&frame.message) {
            // Datagrams may arrive out of order: a stale heartbeat is dropped, never answered or trusted.
            if self.heartbeats.accept_heartbeat(frame).is_err() {
                return Ok(None);
            }
        } else {
            if frame.epoch != self.control.epoch
                || frame.sequence != self.control.next_incoming
                || frame.delivery() != DeliveryClass::Reliable
            {
                return Err(self.fail(StartupError::Sequence));
            }
            self.control.next_incoming = self
                .control
                .next_incoming
                .checked_add(1)
                .ok_or_else(|| self.fail(StartupError::Sequence))?;
        }
        match frame.message {
            Message::Ping(token) => self.outgoing(Message::Pong(token)).map(Some),
            Message::Pong(token) => {
                self.control
                    .health
                    .receive_pong(token, now)
                    .map_err(|error| self.fail(StartupError::Health(error)))?;
                Ok(None)
            }
            Message::SessionReady if !self.peer_ready => {
                // A source may only advertise readiness after the receiver advertised its own.
                if !self.local_is_source && !self.local_ready {
                    return Err(self.fail(StartupError::NotReady));
                }
                self.peer_ready = true;
                Ok(None)
            }
            _ => Err(self.fail(StartupError::UnexpectedMessage)),
        }
    }

    pub fn can_prepare_source(&mut self, now: Duration) -> Result<bool, StartupError> {
        self.check(now)?;
        Ok(self.local_is_source && self.peer_ready && self.control.health.is_confirmed())
    }

    pub fn can_announce_ready(&mut self, now: Duration) -> Result<bool, StartupError> {
        self.check(now)?;
        Ok(!self.local_ready
            && self.control.health.is_confirmed()
            && (!self.local_is_source || self.peer_ready))
    }

    pub fn announce_ready(&mut self, now: Duration) -> Result<Frame, StartupError> {
        if !self.can_announce_ready(now)? {
            return Err(self.fail(StartupError::NotReady));
        }
        self.local_ready = true;
        self.outgoing(Message::SessionReady)
    }

    pub fn is_ready(&mut self, now: Duration) -> Result<bool, StartupError> {
        self.check(now)?;
        Ok(self.local_ready && self.peer_ready && self.control.health.is_confirmed())
    }

    pub fn into_ready(mut self, now: Duration) -> Result<ReadyControl, StartupError> {
        if !self.is_ready(now)? {
            return Err(self.fail(StartupError::NotReady));
        }
        self.control.last_heartbeat_in = self.heartbeats.last_heartbeat();
        Ok(self.control)
    }

    fn outgoing(&mut self, message: Message) -> Result<Frame, StartupError> {
        let counter = if is_heartbeat(&message) {
            &mut self.control.next_heartbeat_out
        } else {
            &mut self.control.next_outgoing
        };
        let sequence = *counter;
        match sequence.checked_add(1) {
            Some(next) => *counter = next,
            None => return Err(self.fail(StartupError::Sequence)),
        }
        Ok(Frame::new(self.control.epoch, sequence, message))
    }

    fn check(&mut self, now: Duration) -> Result<(), StartupError> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        self.control
            .health
            .check(now)
            .map_err(|error| self.fail(StartupError::Health(error)))?;
        if now.saturating_sub(self.started_at) >= STARTUP_DEADLINE {
            return Err(self.fail(StartupError::Deadline));
        }
        self.control.now = now;
        Ok(())
    }

    fn fail(&mut self, failure: StartupError) -> StartupError {
        *self.failure.get_or_insert(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionStartupFailure, StartupPeerHealthFailure};
    use crate::session_health::PEER_LIVENESS;
    use monhop_core::DisplayId;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }
    fn pair() -> (StartupControl, StartupControl) {
        let epoch = SessionEpoch::new(10).unwrap();
        (
            StartupControl::new(epoch, 3, true, ms(0)),
            StartupControl::new(epoch, 3, false, ms(0)),
        )
    }
    fn heartbeat(a: &mut StartupControl, b: &mut StartupControl, now: Duration) {
        if let Some(ping) = a.poll(now).unwrap() {
            let pong = b.receive(&ping, now).unwrap().unwrap();
            a.receive(&pong, now).unwrap();
        }
        if let Some(ping) = b.poll(now).unwrap() {
            let pong = a.receive(&ping, now).unwrap().unwrap();
            b.receive(&pong, now).unwrap();
        }
    }

    #[test]
    fn receiver_readiness_precedes_capture_and_source_readiness() {
        let (mut source, mut receiver) = pair();
        heartbeat(&mut source, &mut receiver, ms(0));
        assert!(!source.can_prepare_source(ms(0)).unwrap());
        assert!(!source.can_announce_ready(ms(0)).unwrap());
        assert!(!receiver.is_ready(ms(0)).unwrap());
        let ready = receiver.announce_ready(ms(1)).unwrap();
        source.receive(&ready, ms(1)).unwrap();
        assert!(source.can_prepare_source(ms(1)).unwrap());
        receiver
            .receive(&source.announce_ready(ms(2)).unwrap(), ms(2))
            .unwrap();
        assert!(source.into_ready(ms(2)).is_ok());
        assert!(receiver.into_ready(ms(2)).is_ok());
    }

    #[test]
    fn responsive_peer_without_ready_cannot_extend_startup_deadline() {
        let (mut source, mut receiver) = pair();
        for time in (0..3_000).step_by(30) {
            heartbeat(&mut source, &mut receiver, ms(time));
            assert!(!source.can_prepare_source(ms(time)).unwrap());
        }
        assert_eq!(
            source.poll(ms(3_000)).map_err(startup_failure),
            Err(SessionFailure::Startup(SessionStartupFailure::Deadline)),
        );
        assert_eq!(source.poll(ms(3_001)).err(), Some(StartupError::Deadline));
    }

    #[test]
    fn startup_errors_retain_redacted_non_native_categories() {
        for (error, expected) in [
            (
                StartupError::Health(HealthError::ClockRegression),
                SessionStartupFailure::PeerHealth(StartupPeerHealthFailure::ClockRegression),
            ),
            (
                StartupError::Health(HealthError::DeadlineExpired),
                SessionStartupFailure::PeerHealth(StartupPeerHealthFailure::DeadlineExpired),
            ),
            (
                StartupError::Health(HealthError::UnexpectedResponse),
                SessionStartupFailure::PeerHealth(StartupPeerHealthFailure::UnexpectedResponse),
            ),
            (
                StartupError::Health(HealthError::TokenExhausted),
                SessionStartupFailure::PeerHealth(StartupPeerHealthFailure::TokenExhausted),
            ),
            (StartupError::Deadline, SessionStartupFailure::Deadline),
            (StartupError::Sequence, SessionStartupFailure::Sequence),
            (
                StartupError::UnexpectedMessage,
                SessionStartupFailure::UnexpectedMessage,
            ),
            (
                StartupError::RateLimited,
                SessionStartupFailure::RateLimited,
            ),
            (StartupError::NotReady, SessionStartupFailure::NotReady),
        ] {
            assert_eq!(startup_failure(error), SessionFailure::Startup(expected));
        }
    }

    #[test]
    fn premature_input_duplicate_ready_and_skipped_sequence_fail_closed() {
        let (mut source, _) = pair();
        let epoch = source.control.epoch;
        assert_eq!(
            source.receive(
                &Frame::new(epoch, 3, Message::ActivateDisplay(DisplayId(1))),
                ms(0)
            ),
            Err(StartupError::UnexpectedMessage)
        );
        let (mut source, _) = pair();
        assert_eq!(
            source.receive(&Frame::new(epoch, 4, Message::SessionReady), ms(0)),
            Err(StartupError::Sequence)
        );
        let (mut source, mut receiver) = pair();
        heartbeat(&mut source, &mut receiver, ms(0));
        source
            .receive(&receiver.announce_ready(ms(0)).unwrap(), ms(0))
            .unwrap();
        assert_eq!(
            source.receive(
                &Frame::new(epoch, source.control.next_incoming, Message::SessionReady),
                ms(1)
            ),
            Err(StartupError::UnexpectedMessage)
        );
    }

    #[test]
    fn reordered_heartbeat_datagrams_are_dropped_without_ending_startup() {
        let (mut source, mut receiver) = pair();
        let ping = source.poll(ms(0)).unwrap().unwrap();
        let pong = receiver.receive(&ping, ms(0)).unwrap().unwrap();
        let receiver_ping = receiver.poll(ms(0)).unwrap().unwrap();
        // The receiver's second datagram overtakes its first: the late one is dropped unanswered.
        assert!(source.receive(&receiver_ping, ms(1)).unwrap().is_some());
        assert_eq!(source.receive(&pong, ms(1)), Ok(None));
        assert!(!source.control.health.is_confirmed());
        heartbeat(&mut source, &mut receiver, ms(30));
        assert!(source.control.health.is_confirmed());
    }

    #[test]
    fn wrong_epoch_and_lost_health_permanently_prevent_readiness() {
        let (mut source, _) = pair();
        assert_eq!(
            source.receive(
                &Frame::new(SessionEpoch::new(11).unwrap(), 3, Message::SessionReady),
                ms(0)
            ),
            Err(StartupError::Sequence)
        );
        assert_eq!(
            source.can_prepare_source(ms(1)),
            Err(StartupError::Sequence)
        );
        let (mut source, mut receiver) = pair();
        heartbeat(&mut source, &mut receiver, ms(0));
        source
            .receive(&receiver.announce_ready(ms(0)).unwrap(), ms(0))
            .unwrap();
        assert_eq!(
            source.can_prepare_source(PEER_LIVENESS),
            Err(StartupError::Health(HealthError::DeadlineExpired))
        );
        assert_eq!(
            source.announce_ready(PEER_LIVENESS + ms(1)),
            Err(StartupError::Health(HealthError::DeadlineExpired))
        );
    }

    #[test]
    fn clock_regression_and_sequence_exhaustion_are_terminal() {
        let (mut source, _) = pair();
        source.poll(ms(10)).unwrap();
        assert_eq!(
            source.poll(ms(9)),
            Err(StartupError::Health(HealthError::ClockRegression))
        );
        assert_eq!(
            source.poll(ms(11)),
            Err(StartupError::Health(HealthError::ClockRegression))
        );
        let epoch = SessionEpoch::new(1).unwrap();
        let mut source = StartupControl::new(epoch, u64::MAX, true, ms(0));
        let mut receiver = StartupControl::new(epoch, u64::MAX, false, ms(0));
        heartbeat(&mut source, &mut receiver, ms(0));
        assert_eq!(receiver.announce_ready(ms(0)), Err(StartupError::Sequence));
        assert_eq!(receiver.poll(ms(1)), Err(StartupError::Sequence));
        assert_eq!(
            source.receive(&Frame::new(epoch, u64::MAX, Message::SessionReady), ms(0)),
            Err(StartupError::Sequence)
        );
        assert_eq!(source.poll(ms(1)), Err(StartupError::Sequence));
    }

    #[test]
    fn ready_handoff_does_not_refill_rate_credit() {
        let (mut source, mut receiver) = pair();
        heartbeat(&mut source, &mut receiver, ms(0));
        source
            .receive(&receiver.announce_ready(ms(0)).unwrap(), ms(0))
            .unwrap();
        receiver
            .receive(&source.announce_ready(ms(0)).unwrap(), ms(0))
            .unwrap();
        // Both sides have consumed two heartbeat frames and one Ready frame.
        let mut control = source.into_ready(ms(0)).unwrap();
        for _ in 0..19_997 {
            control.limiter.allow_at(0).unwrap();
        }
        assert!(control.limiter.allow_at(0).is_err());
    }

    #[test]
    fn handoff_preserves_heartbeat_positions_and_pending_health_challenge() {
        let (mut source, mut receiver) = pair();
        heartbeat(&mut source, &mut receiver, ms(0));
        source
            .receive(&receiver.announce_ready(ms(0)).unwrap(), ms(0))
            .unwrap();
        let ping = source.poll(ms(30)).unwrap().unwrap();
        let pong = receiver.receive(&ping, ms(30)).unwrap().unwrap();
        receiver
            .receive(&source.announce_ready(ms(30)).unwrap(), ms(30))
            .unwrap();
        let mut control = source.into_ready(ms(30)).unwrap();
        // Both sides sent one Ready on the reliable stream; heartbeats live in their own space.
        assert_eq!((control.next_incoming, control.next_outgoing), (4, 4));
        assert_eq!((ping.sequence, control.next_heartbeat_out), (2, 3));
        // The reply to that last ping is still in flight, so the handoff carries the ping before it.
        assert_eq!((pong.sequence, control.last_heartbeat_in), (2, Some(1)));
        let Message::Pong(token) = pong.message else {
            panic!("expected a heartbeat response");
        };
        assert!(control.health.receive_pong(token, ms(31)).is_ok());
    }
}
