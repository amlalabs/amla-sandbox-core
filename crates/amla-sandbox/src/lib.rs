//! Unified runtime for AI agent sandboxing.
//!
//! This crate provides the runtime infrastructure for executing AI agent code
//! in a sandboxed environment with capability-based security.
//!
//! ## Architecture
//!
//! Each runtime is fully isolated with its own scheduler. Multiple runtimes can
//! coexist without interference. Each runtime:
//! - Is created from a PCA (PIC Causal Authority) token
//! - Has an isolated VFS (Virtual File System)
//! - Has its own scheduler for task execution
//! - Can spawn multiple commands that execute as tasks
//!
//! ## WASM Interface
//!
//! The [`wasm`] module provides a multi-tenant WASM interface:
//! - `runtime_new(pca)` - Create a runtime from PCA, returns a runtime ID
//! - `runtime_destroy(id)` - Destroy a runtime
//! - `cmd_create(id, cmd)` - Create command in a runtime
//! - `cmd_delete(id, handle)` - Delete a command
//! - `cmd_stdin(id, handle, data)` - Provide stdin data
//! - `cmd_stdin_close(id, handle)` - Close stdin
//! - `runtime_step(id)` - Step a specific runtime, returns pending host ops
//! - `submit()` - Routes host op results to appropriate runtime
//!
//! ## Protocol Integration
//!
//! This crate integrates with `amla-protocol` to support:
//! - Creating runtimes from PCA tokens
//! - Cryptographic signature verification
//! - Capability mapping from protocol types to runtime types
//!
//! See the [`protocol`] module for details on capability type mappings.

#![deny(rustdoc::broken_intra_doc_links)]

// =============================================================================
// Modules
// =============================================================================

pub mod host_ops;
pub mod keys;
pub mod mcp;
pub mod protocol;
pub mod runtime;
pub mod types;

pub mod wasm;

mod stubs;

// =============================================================================
// Re-exports
// =============================================================================

pub use host_ops::{
    HostErrorCode, HostOpError, HostOpId, HostOpRequest, HostOpResponse, HostOpResult,
    PendingHostOp,
};
pub use keys::{
    KeyError, key_count, key_delete, key_exists, key_generate_from_seed, key_get_public,
    key_get_public_key, key_set, key_sign, key_verify,
};
pub use mcp::{McpError, McpTool, load_mcp_tool, load_mcp_tools};
pub use protocol::{
    CAP_TYPE_MEMORY_DELETE, CAP_TYPE_MEMORY_READ, CAP_TYPE_MEMORY_WRITE, CAP_TYPE_SPAWN,
    CAP_TYPE_TOOL_CALL, ProtocolError, capabilities_from_pca, clear_trusted_authorities,
    get_trusted_authorities, set_trusted_authorities, validate_pca,
};
pub use runtime::{
    PathMount, Runtime, RuntimeError, register_runtime, remove_runtime, runtime_count,
    step_runtime, with_runtime, with_runtime_mut,
};
pub use stubs::{ParamMetadata, ToolMetadata, ToolStubGenerator};
pub use types::{CommandHandle, CommandStatus, RuntimeStatus, StepResponse};
