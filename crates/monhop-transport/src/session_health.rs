//! Fresh request/response progress, independent of input traffic or queued heartbeats.

use std::time::Duration;

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(30);
/// A peer silent for this long is gone. Sized above Wi-Fi scan stalls so one late packet does
/// not end a session; the local suppression lease stays bounded separately.
pub const PEER_LIVENESS: Duration = Duration::from_millis(500);
/// Native suppression renewed by a controller that then stalls expires within this bound.
pub const SUPPRESSION_LEASE_CAP: Duration = Duration::from_millis(120);
/// A source whose peer has been silent this long brings input home while its native lease is
/// still valid, so a stall never ends in a terminal lease expiry.
pub const RETREAT_AFTER: Duration = PEER_LIVENESS.saturating_sub(SUPPRESSION_LEASE_CAP);
/// How long a held session waits for the link to come back before it ends as a health failure.
/// Sized under the QUIC idle timeout so the transport never closes first.
pub const HOLD_LIMIT: Duration = Duration::from_secs(5);
/// A held source whose barrier was acknowledged but that hears no fresh reply sends another
/// barrier after this long: the receiver consumed the last one just before holding itself.
pub const BARRIER_RESEND_AFTER: Duration = PEER_LIVENESS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthError {
    ClockRegression,
    DeadlineExpired,
    UnexpectedResponse,
    TokenExhausted,
}

/// How many holds a session survived and the longest one, including one still running.
pub(crate) fn hold_stats(
    holds: u32,
    held_max: Duration,
    held_since: Option<Duration>,
    now: Duration,
) -> (u32, Duration) {
    let running = held_since.map_or(Duration::ZERO, |since| now.saturating_sub(since));
    (holds, held_max.max(running))
}

/// Challenges go out on a fixed cadence whether or not earlier ones were answered; any reply to
/// a challenge newer than the last acknowledged one is fresh proof, so a lost packet costs one
/// interval instead of the session.
pub struct PeerHealth {
    last_now: Duration,
    last_response: Duration,
    last_sent: Option<Duration>,
    next_token: u64,
    last_acked: u64,
    confirmed: bool,
    failure: Option<HealthError>,
}

impl PeerHealth {
    pub fn new(now: Duration) -> Self {
        Self {
            last_now: now,
            last_response: now,
            last_sent: None,
            next_token: 1,
            last_acked: 0,
            confirmed: false,
            failure: None,
        }
    }

    pub fn is_confirmed(&self) -> bool {
        self.confirmed && self.failure.is_none()
    }

    pub fn last_response(&self) -> Duration {
        self.last_response
    }

    /// When the newest challenge went out, if any challenge is still unanswered.
    pub fn pending_since(&self) -> Option<Duration> {
        (self.last_acked + 1 < self.next_token)
            .then_some(self.last_sent)
            .flatten()
    }

    pub fn poll(&mut self, now: Duration) -> Result<Option<u64>, HealthError> {
        self.check(now)?;
        if self
            .last_sent
            .is_some_and(|last| now - last < HEARTBEAT_INTERVAL)
        {
            return Ok(None);
        }
        let token = self.next_token;
        self.next_token = token
            .checked_add(1)
            .ok_or_else(|| self.fail(HealthError::TokenExhausted))?;
        self.last_sent = Some(now);
        Ok(Some(token))
    }

    pub fn receive_pong(&mut self, token: u64, now: Duration) -> Result<(), HealthError> {
        self.check(now)?;
        if token >= self.next_token {
            return Err(self.fail(HealthError::UnexpectedResponse));
        }
        // A reply to a challenge already superseded is late, not wrong: it earns no liveness credit.
        if token <= self.last_acked {
            return Ok(());
        }
        self.last_acked = token;
        self.last_response = now;
        self.confirmed = true;
        Ok(())
    }

    pub fn check(&mut self, now: Duration) -> Result<(), HealthError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if now < self.last_now {
            return Err(self.fail(HealthError::ClockRegression));
        }
        self.last_now = now;
        if now - self.last_response >= PEER_LIVENESS {
            return Err(self.fail(HealthError::DeadlineExpired));
        }
        Ok(())
    }

    /// Native suppression cannot outlive the last fresh peer response if its controller stalls.
    pub fn remaining(&mut self, now: Duration) -> Result<Duration, HealthError> {
        self.check(now)?;
        Ok((PEER_LIVENESS - (now - self.last_response)).min(SUPPRESSION_LEASE_CAP))
    }

    /// Enters a hold: an expired deadline stops latching, tokens stay monotonic, and only a reply
    /// to a challenge sent from now on counts, so a reply delayed from before the stall earns
    /// nothing. Any other latched failure stays fatal.
    pub fn hold(&mut self, now: Duration) -> Result<(), HealthError> {
        match self.failure {
            None | Some(HealthError::DeadlineExpired) => {}
            Some(error) => return Err(error),
        }
        self.advance(now)?;
        self.failure = None;
        self.last_acked = self.next_token.saturating_sub(1);
        self.confirmed = false;
        Ok(())
    }

    /// Issues challenges on cadence during a hold without judging the deadline.
    pub fn poll_while_held(&mut self, now: Duration) -> Result<Option<u64>, HealthError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.advance(now)?;
        if self
            .last_sent
            .is_some_and(|last| now - last < HEARTBEAT_INTERVAL)
        {
            return Ok(None);
        }
        let token = self.next_token;
        self.next_token = token
            .checked_add(1)
            .ok_or_else(|| self.fail(HealthError::TokenExhausted))?;
        self.last_sent = Some(now);
        Ok(Some(token))
    }

    /// A reply during a hold: true only when it answers a challenge sent after the hold began.
    pub fn receive_pong_while_held(
        &mut self,
        token: u64,
        now: Duration,
    ) -> Result<bool, HealthError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.advance(now)?;
        if token >= self.next_token {
            return Err(self.fail(HealthError::UnexpectedResponse));
        }
        if token <= self.last_acked {
            return Ok(false);
        }
        self.last_acked = token;
        self.last_response = now;
        self.confirmed = true;
        Ok(true)
    }

    /// Ends a hold on a reliably delivered frame from the peer: the deadline restarts here.
    pub fn resume(&mut self, now: Duration) -> Result<(), HealthError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        self.advance(now)?;
        self.last_response = now;
        self.confirmed = true;
        Ok(())
    }

    fn advance(&mut self, now: Duration) -> Result<(), HealthError> {
        if now < self.last_now {
            return Err(self.fail(HealthError::ClockRegression));
        }
        self.last_now = now;
        Ok(())
    }

    fn fail(&mut self, error: HealthError) -> HealthError {
        *self.failure.get_or_insert(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn incoming_traffic_does_not_replace_a_fresh_response() {
        let mut health = PeerHealth::new(ms(0));
        assert_eq!(health.poll(ms(0)), Ok(Some(1)));
        for tick in 1..500 {
            assert_eq!(health.check(ms(tick)), Ok(()));
        }
        assert_eq!(
            health.receive_pong(1, ms(500)),
            Err(HealthError::DeadlineExpired)
        );
        assert_eq!(health.poll(ms(501)), Err(HealthError::DeadlineExpired));
    }

    #[test]
    fn suppression_budget_is_capped_and_shrinks_until_a_fresh_response_arrives() {
        let mut health = PeerHealth::new(ms(0));
        assert_eq!(health.poll(ms(0)), Ok(Some(1)));
        assert_eq!(health.remaining(ms(90)), Ok(SUPPRESSION_LEASE_CAP));
        assert_eq!(health.remaining(ms(390)), Ok(ms(110)));
        health.receive_pong(1, ms(395)).unwrap();
        assert_eq!(health.remaining(ms(400)), Ok(SUPPRESSION_LEASE_CAP));
    }

    #[test]
    fn challenges_keep_flowing_while_unanswered_and_any_new_reply_counts() {
        let mut health = PeerHealth::new(ms(0));
        assert_eq!(health.poll(ms(0)), Ok(Some(1)));
        assert_eq!(health.poll(ms(29)), Ok(None));
        assert_eq!(health.poll(ms(30)), Ok(Some(2)));
        assert_eq!(health.poll(ms(60)), Ok(Some(3)));
        assert_eq!(health.pending_since(), Some(ms(60)));
        assert!(!health.is_confirmed());
        health.receive_pong(1, ms(65)).unwrap();
        assert!(health.is_confirmed());
        assert_eq!(health.pending_since(), Some(ms(60)));
        health.receive_pong(3, ms(70)).unwrap();
        assert_eq!(health.pending_since(), None);
        assert_eq!(health.last_response(), ms(70));
    }

    #[test]
    fn late_replies_earn_nothing_and_unsent_tokens_are_rejected_permanently() {
        let mut health = PeerHealth::new(ms(0));
        assert_eq!(health.poll(ms(0)), Ok(Some(1)));
        assert_eq!(health.poll(ms(30)), Ok(Some(2)));
        health.receive_pong(2, ms(31)).unwrap();
        assert_eq!(health.receive_pong(1, ms(40)), Ok(()));
        assert_eq!(health.last_response(), ms(31));
        assert!(health.is_confirmed());
        assert_eq!(health.poll(ms(60)), Ok(Some(3)));

        let mut health = PeerHealth::new(ms(0));
        assert_eq!(health.poll(ms(0)), Ok(Some(1)));
        assert_eq!(
            health.receive_pong(2, ms(1)),
            Err(HealthError::UnexpectedResponse)
        );
        assert!(!health.is_confirmed());
        assert_eq!(health.poll(ms(60)), Err(HealthError::UnexpectedResponse));
    }

    #[test]
    fn a_lost_reply_costs_one_interval_not_the_session() {
        let mut health = PeerHealth::new(ms(0));
        assert_eq!(health.poll(ms(0)), Ok(Some(1)));
        health.receive_pong(1, ms(5)).unwrap();
        assert_eq!(health.poll(ms(30)), Ok(Some(2)));
        assert_eq!(health.poll(ms(60)), Ok(Some(3)));
        health.receive_pong(3, ms(65)).unwrap();
        assert_eq!(health.check(ms(500)), Ok(()));
        assert_eq!(health.check(ms(565)), Err(HealthError::DeadlineExpired));
    }

    #[test]
    fn a_hold_keeps_tokens_monotonic_and_credits_only_replies_sent_after_it_began() {
        let mut health = PeerHealth::new(ms(0));
        assert_eq!(health.poll(ms(0)), Ok(Some(1)));
        assert_eq!(health.poll(ms(30)), Ok(Some(2)));
        assert_eq!(health.check(ms(500)), Err(HealthError::DeadlineExpired));
        health.hold(ms(500)).unwrap();
        assert_eq!(health.poll_while_held(ms(500)), Ok(Some(3)));
        assert_eq!(health.poll_while_held(ms(510)), Ok(None));
        // A reply delayed from before the stall proves nothing about the link now.
        assert_eq!(health.receive_pong_while_held(2, ms(520)), Ok(false));
        assert!(!health.is_confirmed());
        assert_eq!(health.receive_pong_while_held(3, ms(530)), Ok(true));
        assert!(health.is_confirmed());
        assert_eq!(
            health.receive_pong_while_held(9, ms(531)),
            Err(HealthError::UnexpectedResponse)
        );
        assert_eq!(
            health.poll_while_held(ms(540)),
            Err(HealthError::UnexpectedResponse)
        );
    }

    #[test]
    fn resume_restarts_the_deadline_and_a_second_stall_holds_again() {
        let mut health = PeerHealth::new(ms(0));
        assert_eq!(health.check(ms(500)), Err(HealthError::DeadlineExpired));
        health.hold(ms(500)).unwrap();
        assert_eq!(health.poll_while_held(ms(2_000)), Ok(Some(1)));
        health.resume(ms(2_100)).unwrap();
        assert_eq!(health.remaining(ms(2_500)), Ok(ms(100)));
        assert_eq!(health.check(ms(2_599)), Ok(()));
        assert_eq!(health.check(ms(2_600)), Err(HealthError::DeadlineExpired));
        health.hold(ms(2_600)).unwrap();
        assert_eq!(health.poll_while_held(ms(2_600)), Ok(Some(2)));
        assert_eq!(health.hold(ms(2_599)), Err(HealthError::ClockRegression));
    }

    #[test]
    fn the_retreat_leaves_the_full_lease_cap_before_the_deadline() {
        assert_eq!(RETREAT_AFTER + SUPPRESSION_LEASE_CAP, PEER_LIVENESS);
        assert!(HOLD_LIMIT < Duration::from_secs(10));
    }

    #[test]
    fn clock_regression_is_permanent_even_after_a_valid_reply() {
        let mut health = PeerHealth::new(ms(100));
        health.poll(ms(100)).unwrap();
        health.receive_pong(1, ms(101)).unwrap();
        assert_eq!(health.poll(ms(100)), Err(HealthError::ClockRegression));
        assert_eq!(health.poll(ms(102)), Err(HealthError::ClockRegression));
    }
}
