use serde::{Deserialize, Serialize};

use crate::{ErrorKind, FabricError};

/// Repeating the current state is an idempotent no-op, including terminal states.
/// These pure transitions do not replace transactional persistence or capture checks.
/// Interrupted work can enter NEEDS_ATTENTION from any nonterminal state;
/// recovery must not fabricate a process launch, exit or successful capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunState {
    Queued,
    Preparing,
    Running,
    Finalizing,
    Succeeded,
    Failed,
    Cancelled,
    NeedsAttention,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::NeedsAttention
        )
    }

    pub fn transition(&mut self, next: Self) -> Result<(), FabricError> {
        use RunState::*;
        if *self == next
            || matches!(
                (*self, next),
                (Queued, Preparing | NeedsAttention)
                    | (Preparing, Running | Cancelled | NeedsAttention)
                    | (Running, Finalizing | NeedsAttention)
                    | (Finalizing, Succeeded | Failed | Cancelled | NeedsAttention)
            )
        {
            *self = next;
            Ok(())
        } else {
            Err(invalid_transition("run", self, next))
        }
    }
}

/// READY and FAILED cannot be changed or reused for another preparation attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PreparedStateStatus {
    Preparing,
    Ready,
    Failed,
}

impl PreparedStateStatus {
    pub fn transition(&mut self, next: Self) -> Result<(), FabricError> {
        if *self == next || matches!((*self, next), (Self::Preparing, Self::Ready | Self::Failed)) {
            *self = next;
            Ok(())
        } else {
            Err(invalid_transition("prepared state", self, next))
        }
    }
}

/// A lifecycle graph, not permission to delete: entering CLEANUP_PENDING still
/// requires verified artifacts, no active lease, and validated managed ownership.
/// RECOVERY_REQUIRED must pass those checks before cleanup; expiry alone is insufficient.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkspaceState {
    Active,
    CleanupPending,
    Reclaimed,
    Retained,
    RecoveryRequired,
}

impl WorkspaceState {
    pub fn transition(&mut self, next: Self) -> Result<(), FabricError> {
        use WorkspaceState::*;
        if *self == next
            || matches!(
                (*self, next),
                (Active, CleanupPending | Retained | RecoveryRequired)
                    | (CleanupPending, Reclaimed | Retained | RecoveryRequired)
                    | (Retained, CleanupPending | RecoveryRequired)
                    | (RecoveryRequired, CleanupPending | Retained)
            )
        {
            *self = next;
            Ok(())
        } else {
            Err(invalid_transition("workspace", self, next))
        }
    }
}

fn invalid_transition(
    machine: &str,
    from: impl std::fmt::Debug,
    to: impl std::fmt::Debug,
) -> FabricError {
    FabricError {
        kind: ErrorKind::InvalidTransition,
        message: format!("illegal {machine} transition: {from:?} -> {to:?}"),
    }
}
