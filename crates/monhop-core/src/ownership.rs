//! Explicit pointer destination ownership with epoch-guarded handoffs.

use crate::{DeviceId, DisplayId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerTarget {
    pub machine: DeviceId,
    pub display: DisplayId,
}

impl PointerTarget {
    pub const fn new(machine: DeviceId, display: DisplayId) -> Self {
        Self { machine, display }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnershipState {
    LocalActive {
        target: PointerTarget,
    },
    Transitioning {
        from: PointerTarget,
        to: PointerTarget,
        epoch: u64,
    },
    RemoteActive {
        target: PointerTarget,
        epoch: u64,
    },
    Disconnected {
        local: PointerTarget,
        epoch: u64,
    },
    Recovering {
        local: PointerTarget,
        epoch: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionAcknowledgement {
    pub target: PointerTarget,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnershipCommand {
    BeginTransition {
        from: PointerTarget,
        to: PointerTarget,
        epoch: u64,
    },
    Activate {
        target: PointerTarget,
        epoch: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureReason {
    Disconnected,
    Timeout,
    InputOverflow,
    EmergencyEscape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryPlan {
    pub reason: FailureReason,
    pub local: PointerTarget,
    pub remote_to_release: Option<PointerTarget>,
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnershipError {
    LocalTargetMustBelongToLocalMachine,
    TransitionAlreadyInProgress,
    NotReady,
    SameTarget,
    UnsolicitedAcknowledgement,
    StaleAcknowledgement,
    StaleRecovery,
    EpochExhausted,
}

#[derive(Clone, Debug)]
pub struct PointerOwnership {
    local: PointerTarget,
    state: OwnershipState,
    epoch: u64,
}

impl PointerOwnership {
    pub fn new(local_machine: DeviceId, local: PointerTarget) -> Result<Self, OwnershipError> {
        if local.machine != local_machine {
            return Err(OwnershipError::LocalTargetMustBelongToLocalMachine);
        }

        Ok(Self {
            local,
            state: OwnershipState::LocalActive { target: local },
            epoch: 0,
        })
    }

    pub const fn state(&self) -> OwnershipState {
        self.state
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the one destination that owns input, including while handoff recovery is in progress.
    pub const fn active_destination(&self) -> PointerTarget {
        match self.state {
            OwnershipState::LocalActive { target }
            | OwnershipState::RemoteActive { target, .. } => target,
            OwnershipState::Transitioning { from, .. } => from,
            OwnershipState::Disconnected { local, .. }
            | OwnershipState::Recovering { local, .. } => local,
        }
    }

    pub fn begin_transition(
        &mut self,
        to: PointerTarget,
    ) -> Result<OwnershipCommand, OwnershipError> {
        let from = match self.state {
            OwnershipState::LocalActive { target }
            | OwnershipState::RemoteActive { target, .. } => target,
            OwnershipState::Transitioning { .. } => {
                return Err(OwnershipError::TransitionAlreadyInProgress);
            }
            OwnershipState::Disconnected { .. } | OwnershipState::Recovering { .. } => {
                return Err(OwnershipError::NotReady);
            }
        };
        if from == to {
            return Err(OwnershipError::SameTarget);
        }

        let epoch = self.next_epoch()?;
        self.state = OwnershipState::Transitioning { from, to, epoch };
        Ok(OwnershipCommand::BeginTransition { from, to, epoch })
    }

    pub fn acknowledge_transition(
        &mut self,
        acknowledgement: TransitionAcknowledgement,
    ) -> Result<OwnershipCommand, OwnershipError> {
        let OwnershipState::Transitioning { to, epoch, .. } = self.state else {
            return Err(OwnershipError::UnsolicitedAcknowledgement);
        };
        if acknowledgement.epoch != epoch || acknowledgement.target != to {
            return Err(OwnershipError::StaleAcknowledgement);
        }

        self.state = if to.machine == self.local.machine {
            OwnershipState::LocalActive { target: to }
        } else {
            OwnershipState::RemoteActive { target: to, epoch }
        };
        Ok(OwnershipCommand::Activate { target: to, epoch })
    }

    pub fn on_disconnect(&mut self) -> Result<RecoveryPlan, OwnershipError> {
        self.enter_failure(FailureReason::Disconnected, true)
    }

    pub fn on_timeout(&mut self) -> Result<RecoveryPlan, OwnershipError> {
        self.enter_failure(FailureReason::Timeout, false)
    }

    pub fn on_input_overflow(&mut self) -> Result<RecoveryPlan, OwnershipError> {
        self.enter_failure(FailureReason::InputOverflow, false)
    }

    pub fn on_emergency_escape(&mut self) -> Result<RecoveryPlan, OwnershipError> {
        self.enter_failure(FailureReason::EmergencyEscape, false)
    }

    pub fn begin_recovery(&mut self) -> Result<u64, OwnershipError> {
        let OwnershipState::Disconnected { local, epoch } = self.state else {
            return Err(OwnershipError::NotReady);
        };
        self.state = OwnershipState::Recovering { local, epoch };
        Ok(epoch)
    }

    pub fn complete_recovery(&mut self, epoch: u64) -> Result<(), OwnershipError> {
        let OwnershipState::Recovering {
            local,
            epoch: expected_epoch,
        } = self.state
        else {
            return Err(OwnershipError::NotReady);
        };
        if epoch != expected_epoch {
            return Err(OwnershipError::StaleRecovery);
        }

        self.state = OwnershipState::LocalActive { target: local };
        Ok(())
    }

    fn enter_failure(
        &mut self,
        reason: FailureReason,
        disconnected: bool,
    ) -> Result<RecoveryPlan, OwnershipError> {
        let remote_to_release = match self.state {
            OwnershipState::RemoteActive { target, .. } if target.machine != self.local.machine => {
                Some(target)
            }
            OwnershipState::Transitioning { from, to, .. } => {
                if to.machine != self.local.machine {
                    Some(to)
                } else if from.machine != self.local.machine {
                    Some(from)
                } else {
                    None
                }
            }
            _ => None,
        };
        let epoch = self.next_epoch()?;
        self.state = if disconnected {
            OwnershipState::Disconnected {
                local: self.local,
                epoch,
            }
        } else {
            OwnershipState::Recovering {
                local: self.local,
                epoch,
            }
        };

        Ok(RecoveryPlan {
            reason,
            local: self.local,
            remote_to_release,
            epoch,
        })
    }

    fn next_epoch(&mut self) -> Result<u64, OwnershipError> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or(OwnershipError::EpochExhausted)?;
        Ok(self.epoch)
    }
}
