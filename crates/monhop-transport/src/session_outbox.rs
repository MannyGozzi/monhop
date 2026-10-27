//! The ordered control stream's send queue.
//!
//! Every frame of a link waits here, heartbeats included, and frames pile up only while the peer's
//! stream credit is used up. Absolute motion then keeps only the newest position, and a newer
//! heartbeat of the same kind and direction replaces an older unsent one, but nothing moves ahead
//! of a frame queued before it: every key, button, scroll and gesture keeps its place behind the
//! position sent before it, and every heartbeat behind all of them. A stall of this stream thus
//! silences the heartbeats both sides judge each other by, so each holds once it heard nothing
//! fresh for `PEER_LIVENESS`; a held receiver discards the input that arrives late, unseen, and the
//! source's hold barrier releases everything. Input is never dropped here: only a position or a
//! heartbeat replaced by a newer one leaves the queue unsent.

use std::{
    collections::VecDeque,
    sync::{Mutex, PoisonError},
};

use monhop_protocol::{Frame, Message, Motion};
use tokio::{sync::Notify, time::Instant};

/// Why the queue took no frame. Sticky: once one frame is refused, no later frame is accepted, so
/// the peer never sees input with a gap in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The bounded queue of frames that cannot merge is full.
    Full,
    /// The writer stopped.
    Closed,
}

/// Frames not yet handed to the stream, in send order, each with when its wait began.
pub(crate) struct Queue {
    frames: VecDeque<(Frame, Instant)>,
    capacity: usize,
    /// When the frame the writer is still writing began to wait.
    in_flight: Option<Instant>,
    refused: Option<Refusal>,
}

impl Queue {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            capacity,
            in_flight: None,
            refused: None,
        }
    }

    pub(crate) fn push(&mut self, frame: Frame, now: Instant) -> Result<(), Refusal> {
        if let Some(refusal) = self.refused {
            return Err(refusal);
        }
        if let Some((tail, _)) = self.frames.back_mut()
            && newer_position(tail, &frame)
        {
            *tail = frame;
            return Ok(());
        }
        let mut waiting = now;
        if let Some(stale) = self
            .frames
            .iter()
            .position(|(queued, _)| superseded(queued, &frame))
        {
            // The replacement keeps the wait already run up, so the stall measure never restarts.
            waiting = self.frames.remove(stale).map_or(now, |(_, since)| since);
        }
        if self.frames.len() >= self.capacity {
            return Err(*self.refused.insert(Refusal::Full));
        }
        self.frames.push_back((frame, waiting));
        Ok(())
    }

    /// The next frame to write; it can no longer change and counts as waiting until `written`.
    pub(crate) fn pop(&mut self) -> Option<Frame> {
        let (frame, waiting) = self.frames.pop_front()?;
        self.in_flight = Some(waiting);
        Some(frame)
    }

    pub(crate) fn written(&mut self) {
        self.in_flight = None;
    }

    pub(crate) fn close(&mut self) {
        self.refused.get_or_insert(Refusal::Closed);
    }

    pub(crate) fn refused(&self) -> Option<Refusal> {
        self.refused
    }

    /// When the oldest wait among frames the local stream has not yet accepted began. A replaced
    /// heartbeat carries its wait to the tail, so the in-flight frame is not always the oldest.
    pub(crate) fn waiting_since(&self) -> Option<Instant> {
        self.in_flight
            .into_iter()
            .chain(self.frames.iter().map(|(_, since)| *since))
            .min()
    }
}

fn same_lane(queued: &Frame, next: &Frame) -> bool {
    queued.epoch == next.epoch && queued.scope == next.scope && queued.sequence < next.sequence
}

/// Absolute motion at the queued tail takes the newer position in place: it lands exactly where
/// the later frame would have. Relative motion never merges, so every step keeps its bounds check.
fn newer_position(queued: &Frame, next: &Frame) -> bool {
    same_lane(queued, next)
        && matches!(queued.message, Message::Motion(Motion::Absolute(_)))
        && matches!(next.message, Message::Motion(Motion::Absolute(_)))
}

/// Only the newest challenge or reply of a direction counts as fresh, so an unsent older one is
/// moot; the newer one still queues behind everything before it.
fn superseded(queued: &Frame, next: &Frame) -> bool {
    same_lane(queued, next)
        && matches!(
            (&queued.message, &next.message),
            (Message::Ping(_), Message::Ping(_)) | (Message::Pong(_), Message::Pong(_))
        )
}

/// The queue shared by a link's senders and its one writer task.
pub(crate) struct Outbox {
    queue: Mutex<Queue>,
    ready: Notify,
}

impl Outbox {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(Queue::new(capacity)),
            ready: Notify::new(),
        }
    }

    // Every operation leaves the queue consistent, so a poisoned lock still holds a valid queue.
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn push(&self, frame: Frame) -> Result<(), Refusal> {
        self.queue().push(frame, Instant::now())?;
        self.ready.notify_one();
        Ok(())
    }

    /// Waits for the next frame to write. `notify_one` keeps a permit, so a push between the
    /// empty check and the wait is never missed.
    pub(crate) async fn next(&self) -> Frame {
        loop {
            if let Some(frame) = self.queue().pop() {
                return frame;
            }
            self.ready.notified().await;
        }
    }

    pub(crate) fn written(&self) {
        self.queue().written();
    }

    pub(crate) fn close(&self) {
        self.queue().close();
    }

    pub(crate) fn refused(&self) -> Option<Refusal> {
        self.queue().refused()
    }

    pub(crate) fn waiting_since(&self) -> Option<Instant> {
        self.queue().waiting_since()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monhop_core::{HidUsage, ModifierState, MouseButton, Point};
    use monhop_protocol::{Button, FrameScope, Key, SessionEpoch};
    use std::time::Duration;

    const SCOPE: FrameScope = FrameScope::LowerControlsHigher;

    fn frame(epoch: u64, sequence: u64, message: Message) -> Frame {
        Frame::new(SessionEpoch::new(epoch).unwrap(), sequence, message).with_scope(SCOPE)
    }

    fn at(x: f64) -> Message {
        Message::Motion(Motion::Absolute(Point::new(x, 1.0)))
    }

    fn by(x: f64) -> Message {
        Message::Motion(Motion::Relative(Point::new(x, 2.0)))
    }

    fn press(is_down: bool) -> Message {
        Message::Button(Button {
            button: MouseButton::Left,
            is_down,
            click_count: 1,
        })
    }

    fn key(is_down: bool) -> Message {
        Message::Key(Key {
            usage: HidUsage(0x04),
            is_down,
            repeat: false,
            modifiers: ModifierState::default(),
        })
    }

    fn drain(queue: &mut Queue) -> Vec<Frame> {
        std::iter::from_fn(|| {
            let frame = queue.pop();
            queue.written();
            frame
        })
        .collect()
    }

    #[test]
    fn backed_up_motion_collapses_and_discrete_input_keeps_its_place_behind_it() {
        let now = Instant::now();
        let mut queue = Queue::new(8);
        for (sequence, message) in [
            (1, at(1.0)),
            (2, at(2.0)),
            (3, press(true)),
            (4, at(3.0)),
            (5, at(4.0)),
            (6, at(5.0)),
            (7, key(true)),
            (8, key(false)),
            (9, press(false)),
            (10, at(6.0)),
            (11, at(7.0)),
        ] {
            queue.push(frame(2, sequence, message), now).unwrap();
        }
        assert_eq!(
            drain(&mut queue),
            [
                frame(2, 2, at(2.0)),
                frame(2, 3, press(true)),
                frame(2, 6, at(5.0)),
                frame(2, 7, key(true)),
                frame(2, 8, key(false)),
                frame(2, 9, press(false)),
                frame(2, 11, at(7.0)),
            ]
        );
    }

    #[test]
    fn relative_motion_never_merges_so_each_step_keeps_its_bounds_check() {
        let now = Instant::now();
        let mut queue = Queue::new(8);
        let steps = [(1, by(1.5)), (2, by(-4.0)), (3, at(9.0)), (4, by(2.0))];
        for (sequence, message) in steps.clone() {
            queue.push(frame(2, sequence, message), now).unwrap();
        }
        let expected: Vec<_> = steps
            .into_iter()
            .map(|(sequence, message)| frame(2, sequence, message))
            .collect();
        assert_eq!(drain(&mut queue), expected);
    }

    #[test]
    fn a_newer_heartbeat_replaces_an_unsent_older_one_but_never_moves_ahead_of_input() {
        let start = Instant::now();
        let later = start + Duration::from_millis(30);
        let inbound = |sequence, message| {
            frame(1, sequence, message).with_scope(FrameScope::HigherControlsLower)
        };
        let mut queue = Queue::new(8);
        queue.push(frame(1, 1, Message::Ping(1)), start).unwrap();
        queue.push(frame(2, 1, key(true)), start).unwrap();
        queue.push(inbound(1, Message::Pong(4)), start).unwrap();
        queue.push(frame(1, 2, Message::Pong(7)), start).unwrap();
        queue.push(frame(2, 2, key(false)), later).unwrap();
        queue.push(frame(1, 3, Message::Ping(2)), later).unwrap();
        queue.push(frame(1, 4, Message::Pong(8)), later).unwrap();
        assert_eq!(queue.waiting_since(), Some(start));
        assert_eq!(
            drain(&mut queue),
            [
                frame(2, 1, key(true)),
                inbound(1, Message::Pong(4)),
                frame(2, 2, key(false)),
                frame(1, 3, Message::Ping(2)),
                frame(1, 4, Message::Pong(8)),
            ]
        );

        // A stream stuck with only heartbeats queued still reports its whole wait.
        queue.push(frame(1, 5, Message::Ping(3)), start).unwrap();
        for sequence in 6..20 {
            queue
                .push(frame(1, sequence, Message::Ping(sequence)), later)
                .unwrap();
        }
        assert_eq!(queue.waiting_since(), Some(start));
        assert_eq!(drain(&mut queue), [frame(1, 19, Message::Ping(19))]);
    }

    #[test]
    fn motion_never_merges_across_an_epoch_a_direction_or_the_frame_being_written() {
        let now = Instant::now();
        let mut queue = Queue::new(8);
        queue.push(frame(2, 1, at(1.0)), now).unwrap();
        queue.push(frame(3, 1, at(2.0)), now).unwrap();
        let inbound = frame(3, 2, at(3.0)).with_scope(FrameScope::HigherControlsLower);
        queue.push(inbound.clone(), now).unwrap();
        assert_eq!(
            drain(&mut queue),
            [frame(2, 1, at(1.0)), frame(3, 1, at(2.0)), inbound]
        );

        queue.push(frame(3, 3, at(4.0)), now).unwrap();
        let writing = queue.pop().unwrap();
        queue.push(frame(3, 4, at(5.0)), now).unwrap();
        queue.written();
        assert_eq!(writing, frame(3, 3, at(4.0)));
        assert_eq!(drain(&mut queue), [frame(3, 4, at(5.0))]);
    }

    #[test]
    fn a_full_queue_refuses_every_later_frame_but_motion_still_merges_until_then() {
        let now = Instant::now();
        let mut queue = Queue::new(2);
        queue.push(frame(2, 1, press(true)), now).unwrap();
        queue.push(frame(2, 2, at(1.0)), now).unwrap();
        queue.push(frame(2, 3, at(2.0)), now).unwrap();
        assert_eq!(
            queue.push(frame(2, 4, press(false)), now),
            Err(Refusal::Full)
        );
        assert_eq!(queue.push(frame(2, 5, at(3.0)), now), Err(Refusal::Full));
        assert_eq!(queue.refused(), Some(Refusal::Full));
        queue.close();
        assert_eq!(queue.refused(), Some(Refusal::Full));

        let mut closed = Queue::new(2);
        closed.close();
        assert_eq!(closed.push(frame(2, 1, at(1.0)), now), Err(Refusal::Closed));
    }

    #[test]
    fn the_wait_runs_from_the_oldest_frame_the_peer_has_not_taken() {
        let start = Instant::now();
        let later = start + Duration::from_millis(40);
        let mut queue = Queue::new(4);
        assert_eq!(queue.waiting_since(), None);
        queue.push(frame(2, 1, at(1.0)), start).unwrap();
        queue.push(frame(2, 2, press(true)), later).unwrap();
        // A merged position keeps the wait of the slot it took over.
        queue
            .push(frame(2, 3, at(2.0)), later + Duration::from_millis(5))
            .unwrap();
        assert_eq!(queue.waiting_since(), Some(start));
        queue.pop();
        assert_eq!(queue.waiting_since(), Some(start), "still being written");
        queue.written();
        assert_eq!(queue.waiting_since(), Some(later));
        drain(&mut queue);
        assert_eq!(queue.waiting_since(), None);

        queue.push(frame(1, 1, Message::Ping(1)), start).unwrap();
        queue.push(frame(2, 4, key(true)), later).unwrap();
        queue.push(frame(1, 2, Message::Ping(2)), later).unwrap();
        queue.pop();
        assert_eq!(
            queue.waiting_since(),
            Some(start),
            "a replaced heartbeat's wait outlives the newer frame being written"
        );
    }

    #[tokio::test]
    async fn the_writer_takes_frames_in_order_and_wakes_for_one_pushed_while_it_waits() {
        let outbox = Outbox::new(4);
        outbox.push(frame(2, 1, key(true))).unwrap();
        outbox.push(frame(2, 2, key(false))).unwrap();
        assert_eq!(outbox.next().await, frame(2, 1, key(true)));
        assert_eq!(outbox.next().await, frame(2, 2, key(false)));
        let pushed = async {
            tokio::task::yield_now().await;
            outbox.push(frame(2, 3, at(1.0))).unwrap();
        };
        let (next, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(outbox.next(), pushed)
        })
        .await
        .expect("the waiting writer wakes");
        assert_eq!(next, frame(2, 3, at(1.0)));
    }
}
