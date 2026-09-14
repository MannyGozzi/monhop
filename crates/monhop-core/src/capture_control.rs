//! One controller publishes at most one command until the capture thread completes it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::Point;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CaptureCommand {
    /// Begins a remote route after the native owner has released locally delivered held input.
    ActivateRemote { deadline: Duration },
    /// Extends an already-active remote route without another held-input transfer.
    RemoteUntil(Duration),
    /// Returns to local routing after the native owner places the cursor and restores held input.
    LocalAt(Point),
    /// Emergency local restore without a held-input transfer.
    Local,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlError {
    Pending,
    Failed,
    Exhausted,
    InvalidDeadline,
    InvalidPoint,
}

/// The capture thread's terminal outcome for one command revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlCompletion {
    Applied,
    Failed,
}

struct Mailbox {
    requested: AtomicU64,
    completed: AtomicU64,
    completion: AtomicU8,
    kind: AtomicU8,
    deadline_ns: AtomicU64,
    position_x_bits: AtomicU64,
    position_y_bits: AtomicU64,
}

pub struct ControlWriter {
    mailbox: Arc<Mailbox>,
}
pub struct ControlReader {
    mailbox: Arc<Mailbox>,
}

pub fn control_channel() -> (ControlWriter, ControlReader) {
    let mailbox = Arc::new(Mailbox {
        requested: AtomicU64::new(0),
        completed: AtomicU64::new(0),
        completion: AtomicU8::new(ControlCompletion::Applied.encoded()),
        kind: AtomicU8::new(0),
        deadline_ns: AtomicU64::new(0),
        position_x_bits: AtomicU64::new(0),
        position_y_bits: AtomicU64::new(0),
    });
    (
        ControlWriter {
            mailbox: mailbox.clone(),
        },
        ControlReader { mailbox },
    )
}

impl ControlWriter {
    /// `&mut self` prevents two writers from publishing overlapping payloads.
    pub fn submit(&mut self, command: CaptureCommand) -> Result<u64, ControlError> {
        let previous = self.mailbox.requested.load(Ordering::Relaxed);
        if self.mailbox.completed.load(Ordering::Acquire) != previous {
            return Err(ControlError::Pending);
        }
        if ControlCompletion::from_encoded(self.mailbox.completion.load(Ordering::Acquire))
            == Some(ControlCompletion::Failed)
        {
            return Err(ControlError::Failed);
        }
        let revision = previous.checked_add(1).ok_or(ControlError::Exhausted)?;
        let (kind, deadline, position_x_bits, position_y_bits) = match command {
            CaptureCommand::ActivateRemote { deadline } => (
                1,
                u64::try_from(deadline.as_nanos()).map_err(|_| ControlError::InvalidDeadline)?,
                0,
                0,
            ),
            CaptureCommand::RemoteUntil(deadline) => (
                2,
                u64::try_from(deadline.as_nanos()).map_err(|_| ControlError::InvalidDeadline)?,
                0,
                0,
            ),
            CaptureCommand::LocalAt(position) => {
                if !position.is_finite() {
                    return Err(ControlError::InvalidPoint);
                }
                (3, 0, position.x.to_bits(), position.y.to_bits())
            }
            CaptureCommand::Local => (4, 0, 0, 0),
        };
        self.mailbox.kind.store(kind, Ordering::Relaxed);
        self.mailbox.deadline_ns.store(deadline, Ordering::Relaxed);
        self.mailbox
            .position_x_bits
            .store(position_x_bits, Ordering::Relaxed);
        self.mailbox
            .position_y_bits
            .store(position_y_bits, Ordering::Relaxed);
        self.mailbox.requested.store(revision, Ordering::Release);
        Ok(revision)
    }

    /// Returns the most recent terminal revision only when it was applied successfully.
    pub fn completed_revision(&self) -> Result<u64, ControlError> {
        let revision = self.mailbox.completed.load(Ordering::Acquire);
        match ControlCompletion::from_encoded(self.mailbox.completion.load(Ordering::Acquire)) {
            Some(ControlCompletion::Applied) => Ok(revision),
            Some(ControlCompletion::Failed) => Err(ControlError::Failed),
            None => unreachable!("completion status is always published by this module"),
        }
    }
}

impl ControlReader {
    pub fn pending(&mut self) -> Option<(u64, CaptureCommand)> {
        let revision = self.mailbox.requested.load(Ordering::Acquire);
        if revision == self.mailbox.completed.load(Ordering::Relaxed) {
            return None;
        }
        let command = match self.mailbox.kind.load(Ordering::Relaxed) {
            1 => CaptureCommand::ActivateRemote {
                deadline: Duration::from_nanos(self.mailbox.deadline_ns.load(Ordering::Relaxed)),
            },
            2 => CaptureCommand::RemoteUntil(Duration::from_nanos(
                self.mailbox.deadline_ns.load(Ordering::Relaxed),
            )),
            3 => CaptureCommand::LocalAt(Point::new(
                f64::from_bits(self.mailbox.position_x_bits.load(Ordering::Relaxed)),
                f64::from_bits(self.mailbox.position_y_bits.load(Ordering::Relaxed)),
            )),
            4 => CaptureCommand::Local,
            _ => unreachable!("single writer publishes a valid command before its revision"),
        };
        Some((revision, command))
    }

    /// Publishes the terminal result after applying the command and enqueuing any route barrier.
    pub fn complete(&mut self, revision: u64, completion: ControlCompletion) {
        self.mailbox
            .completion
            .store(completion.encoded(), Ordering::Relaxed);
        self.mailbox.completed.store(revision, Ordering::Release);
    }
}

impl ControlCompletion {
    const fn encoded(self) -> u8 {
        match self {
            Self::Applied => 1,
            Self::Failed => 2,
        }
    }

    const fn from_encoded(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Applied),
            2 => Some(Self::Failed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_cannot_be_overwritten_until_the_worker_completes_it() {
        let (mut writer, mut reader) = control_channel();
        let command = CaptureCommand::RemoteUntil(Duration::from_millis(120));
        assert_eq!(writer.submit(command), Ok(1));
        assert_eq!(
            writer.submit(CaptureCommand::Local),
            Err(ControlError::Pending)
        );
        assert_eq!(reader.pending(), Some((1, command)));
        assert_eq!(writer.completed_revision(), Ok(0));
        reader.complete(1, ControlCompletion::Applied);
        assert_eq!(writer.completed_revision(), Ok(1));
        assert_eq!(writer.submit(CaptureCommand::Local), Ok(2));
        assert_eq!(reader.pending(), Some((2, CaptureCommand::Local)));
        reader.complete(2, ControlCompletion::Applied);
        assert_eq!(reader.pending(), None);
    }

    #[test]
    fn oversized_deadline_never_publishes_a_partial_command() {
        let (mut writer, mut reader) = control_channel();
        assert_eq!(
            writer.submit(CaptureCommand::RemoteUntil(Duration::MAX)),
            Err(ControlError::InvalidDeadline)
        );
        assert_eq!(reader.pending(), None);
        assert_eq!(writer.submit(CaptureCommand::Local), Ok(1));
    }

    #[test]
    fn local_cursor_position_round_trips_exact_f64_bits() {
        let (mut writer, mut reader) = control_channel();
        let position = Point::new(-0.0, 1.5);

        assert_eq!(writer.submit(CaptureCommand::LocalAt(position)), Ok(1));
        let Some((revision, CaptureCommand::LocalAt(actual))) = reader.pending() else {
            panic!("local cursor command must be pending");
        };
        assert_eq!(actual.x.to_bits(), position.x.to_bits());
        assert_eq!(actual.y.to_bits(), position.y.to_bits());
        reader.complete(revision, ControlCompletion::Applied);

        assert_eq!(
            writer.submit(CaptureCommand::LocalAt(Point::new(f64::NAN, 0.0))),
            Err(ControlError::InvalidPoint)
        );
        assert_eq!(writer.completed_revision(), Ok(1));
    }

    #[test]
    fn activation_and_renewal_remain_distinct_terminal_commands() {
        let (mut writer, mut reader) = control_channel();
        let deadline = Duration::from_millis(120);

        assert_eq!(
            writer.submit(CaptureCommand::ActivateRemote { deadline }),
            Ok(1)
        );
        assert_eq!(
            reader.pending(),
            Some((1, CaptureCommand::ActivateRemote { deadline }))
        );
        reader.complete(1, ControlCompletion::Applied);

        assert_eq!(writer.submit(CaptureCommand::RemoteUntil(deadline)), Ok(2));
        assert_eq!(
            reader.pending(),
            Some((2, CaptureCommand::RemoteUntil(deadline)))
        );
    }

    #[test]
    fn failed_completion_is_terminal_instead_of_success_or_permanent_pending() {
        let (mut writer, mut reader) = control_channel();
        assert_eq!(writer.submit(CaptureCommand::Local), Ok(1));
        assert_eq!(reader.pending(), Some((1, CaptureCommand::Local)));
        assert_eq!(
            writer.submit(CaptureCommand::Local),
            Err(ControlError::Pending)
        );

        reader.complete(1, ControlCompletion::Failed);

        assert_eq!(reader.pending(), None);
        assert_eq!(writer.completed_revision(), Err(ControlError::Failed));
        assert_eq!(
            writer.submit(CaptureCommand::Local),
            Err(ControlError::Failed)
        );
    }
}
