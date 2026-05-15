//! Protocol types for the WASM runtime interface.
//!
//! These types define the JSON protocol between the host and WASM runtime.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::host_ops::PendingHostOp;

/// Type alias for host operations collection.
///
/// Uses `SmallVec` to avoid heap allocation for the common case of 0-4 pending ops.
/// Most commands have at most 1-2 host operations pending at a time.
pub type HostOpsVec = SmallVec<[PendingHostOp; 4]>;

// =============================================================================
// ID Types (Newtypes for type safety)
// =============================================================================

/// Unique identifier for a runtime instance.
///
/// Assigned by `register_runtime()` and used to route operations to the correct runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuntimeId(pub u64);

impl RuntimeId {
    /// Represents an invalid or unassigned runtime ID.
    pub const NONE: Self = Self(0);

    /// Create a new runtime ID.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Get the raw ID value.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Check if this is a valid (non-zero) runtime ID.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl From<u64> for RuntimeId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<RuntimeId> for u64 {
    fn from(id: RuntimeId) -> Self {
        id.0
    }
}

impl std::fmt::Display for RuntimeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Handle to a command instance within a runtime.
///
/// Assigned by `create_command()` and used to identify commands in host operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandHandle(pub u64);

impl CommandHandle {
    /// Represents an invalid or unassigned command handle.
    pub const NONE: Self = Self(0);

    /// Create a new command handle.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Get the raw handle value.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Check if this is a valid (non-zero) command handle.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl From<u64> for CommandHandle {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<CommandHandle> for u64 {
    fn from(handle: CommandHandle) -> Self {
        handle.0
    }
}

impl std::fmt::Display for CommandHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// =============================================================================
// Step Response
// =============================================================================

/// Response from stepping a runtime.
///
/// Contains pending host operations and overall status. All command I/O
/// (stdout, stderr, exit codes, stdin requests) is streamed via host ops.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepResponse {
    /// All pending host operations.
    ///
    /// The host should execute these (preferably in parallel) and
    /// provide results via `submit()`. Host ops include:
    /// - `Output` - stdout/stderr chunks
    /// - `CommandExit` - command completed with exit code
    /// - `ReadStdin` - command needs stdin input
    /// - `WakeAt`, `VfsRead`, `VfsWrite`, `ToolCall` - async operations
    ///
    /// Uses `SmallVec<[_; 4]>` internally to avoid heap allocation for
    /// the common case of 0-4 pending operations.
    #[serde(default, skip_serializing_if = "HostOpsVec::is_empty")]
    pub host_ops: HostOpsVec,

    /// Overall runtime status.
    pub status: RuntimeStatus,
}

impl StepResponse {
    /// Create an error response (e.g., runtime not found).
    pub fn error(message: &str) -> Self {
        Self {
            host_ops: HostOpsVec::new(),
            status: RuntimeStatus::Error {
                message: message.to_string(),
            },
        }
    }

    /// Check if all commands have exited.
    pub fn all_done(&self) -> bool {
        matches!(self.status, RuntimeStatus::AllDone)
    }

    /// Check if any commands need host operations.
    pub fn needs_host_ops(&self) -> bool {
        !self.host_ops.is_empty()
    }
}

/// Status of a single command (internal use).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Command can make progress (not blocked).
    #[default]
    Running,

    /// Command is blocked waiting for host operation results.
    ///
    /// This includes waiting for stdin (`ReadStdin`), file reads, timers, etc.
    /// Hosts can check pending ops for `ReadStdin` if stdin-specific UX is needed.
    NeedHostOps,

    /// Command has exited normally.
    Exit,

    /// Command was cancelled before completion.
    Cancelled,
}

impl CommandStatus {
    /// Check if command is blocked (cannot make progress without host action).
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::NeedHostOps)
    }

    /// Check if command has completed (either normally or cancelled).
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Exit | Self::Cancelled)
    }
}

/// Overall runtime status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    /// At least one command can make progress.
    #[default]
    Running,

    /// All commands are blocked (need input or host ops).
    AllBlocked,

    /// All commands have exited.
    AllDone,

    /// Runtime error.
    Error {
        /// Error message.
        message: String,
    },

    /// Runtime panicked and was killed.
    ///
    /// The runtime has been destroyed and cannot be used again.
    /// Other runtimes are unaffected.
    Panic {
        /// Panic message (if available).
        message: String,
    },
}

impl RuntimeStatus {
    /// Check if runtime is in error state.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    /// Check if runtime panicked.
    pub fn is_panic(&self) -> bool {
        matches!(self, Self::Panic { .. })
    }

    /// Create a panic status with message.
    pub fn panic(message: impl Into<String>) -> Self {
        Self::Panic {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_response_error() {
        let resp = StepResponse::error("not initialized");
        assert!(resp.status.is_error());
    }

    #[test]
    fn command_status_serialize() {
        let status = CommandStatus::NeedHostOps;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""need_host_ops""#);

        let parsed: CommandStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, CommandStatus::NeedHostOps);
    }

    #[test]
    fn runtime_status_serialize() {
        let status = RuntimeStatus::AllDone;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""all_done""#);
    }

    #[test]
    fn step_response_all_done() {
        let resp = StepResponse {
            host_ops: HostOpsVec::new(),
            status: RuntimeStatus::AllDone,
        };
        assert!(resp.all_done());
        assert!(!resp.needs_host_ops());
    }

    #[test]
    fn step_response_with_host_ops() {
        let mut resp = StepResponse::default();
        resp.host_ops.push(PendingHostOp {
            id: 1,
            runtime_id: RuntimeId::new(1),
            command: Some(CommandHandle::new(1)),
            request: crate::host_ops::HostOpRequest::WakeAt {
                deadline_nanos: 1_000_000_000,
            },
        });
        assert!(resp.needs_host_ops());
    }

    #[test]
    fn command_status_is_blocked() {
        assert!(!CommandStatus::Running.is_blocked());
        assert!(CommandStatus::NeedHostOps.is_blocked());
        assert!(!CommandStatus::Exit.is_blocked());
        assert!(!CommandStatus::Cancelled.is_blocked());
    }

    #[test]
    fn command_status_is_done() {
        assert!(!CommandStatus::Running.is_done());
        assert!(!CommandStatus::NeedHostOps.is_done());
        assert!(CommandStatus::Exit.is_done());
        assert!(CommandStatus::Cancelled.is_done());
    }

    #[test]
    fn runtime_status_is_error() {
        assert!(!RuntimeStatus::Running.is_error());
        assert!(!RuntimeStatus::AllBlocked.is_error());
        assert!(!RuntimeStatus::AllDone.is_error());
        assert!(
            RuntimeStatus::Error {
                message: "test".to_string()
            }
            .is_error()
        );
        assert!(!RuntimeStatus::panic("test").is_error());
    }

    #[test]
    fn runtime_status_is_panic() {
        assert!(!RuntimeStatus::Running.is_panic());
        assert!(!RuntimeStatus::AllBlocked.is_panic());
        assert!(!RuntimeStatus::AllDone.is_panic());
        assert!(
            !RuntimeStatus::Error {
                message: "test".to_string()
            }
            .is_panic()
        );
        assert!(RuntimeStatus::panic("test").is_panic());
    }

    #[test]
    fn runtime_status_panic_serialization() {
        let status = RuntimeStatus::panic("something panicked");
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("panic"));
        assert!(json.contains("something panicked"));
    }

    #[test]
    fn runtime_status_error_serialization() {
        let status = RuntimeStatus::Error {
            message: "something broke".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("error"));
        assert!(json.contains("something broke"));
    }
}
