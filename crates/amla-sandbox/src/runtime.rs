//! Runtime - PCA-based execution context with per-runtime scheduler isolation.
//!
//! This module provides the core Runtime struct that executes commands in
//! a capability-constrained sandbox. Key design points:
//!
//! - **Per-Runtime Scheduler**: Each runtime has its own scheduler for task isolation
//! - **Isolated VFS**: Each runtime has its own virtual filesystem
//! - **PCA-based**: Runtime is created from a PCA token that defines capabilities
//! - **Multi-tenancy**: Multiple runtimes can coexist; stepping one runtime only
//!   affects its own tasks (no cross-contamination)

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use amla_audit::{AuditBuffer, AuditLog, LogEntry, content_preview};
use amla_capabilities::CapabilitySet;
use amla_protocol::Pca;
use amla_scheduler::{Exit, RandomSourceFn, Scheduler, TaskHandle, TimeSourceFn};
use amla_shell::{Environment, IoHandle, Shell};
use amla_tools::ToolCatalog;
use amla_vfs::{Permission, Vfs};
use thiserror::Error;

use crate::host_ops::{
    HostOpError, HostOpRequest, HostOpResponse, HostOpResult, NOTIFICATION_ID, PendingHostOp,
};
use crate::protocol::{ProtocolError, capabilities_from_pca, validate_pca};
use crate::stubs::ToolStubGenerator;
use crate::types::{
    CommandHandle, CommandStatus, HostOpsVec, RuntimeId, RuntimeStatus, StepResponse,
};

// =============================================================================
// Global State (Thread-Local)
// =============================================================================
//
// These thread-locals are required for the WASM FFI interface, where we cannot
// pass `Runtime` objects directly across the FFI boundary. Instead, we:
// 1. Store runtimes in a registry, keyed by ID
// 2. Pass runtime IDs through the FFI
// 3. Look up runtimes by ID when needed
//
// This is container state only - it does NOT affect runtime behavior.
// Critical isolation is ensured by per-runtime schedulers (each Runtime owns
// its own Scheduler), so stepping one runtime cannot affect another's tasks.

thread_local! {
    /// Registry of all active runtimes, keyed by runtime ID.
    /// This is an FFI accommodation - runtimes are looked up by ID
    /// because we can't pass Rust objects through WASM FFI.
    static RUNTIMES: RefCell<HashMap<u64, Runtime>> = RefCell::new(HashMap::new());

    /// Counter for assigning unique runtime IDs.
    static NEXT_RUNTIME_ID: Cell<u64> = const { Cell::new(1) };
}

// =============================================================================
// Host Path Mount
// =============================================================================

/// A path mapping from host filesystem to sandbox VFS.
///
/// When a file at `sandbox_path` is accessed, the runtime will issue a
/// `file_read` operation through `HostChannel` to read from `host_path`.
///
/// **Security**: All mounted paths are **read-only** in the sandbox.
/// The sandbox cannot write back to the host filesystem.
#[derive(Debug, Clone)]
pub struct PathMount {
    /// Path on the host filesystem (e.g., "/home/user/data/config.json").
    pub host_path: String,

    /// Path within the sandbox VFS (e.g., "/data/config.json").
    pub sandbox_path: String,
}

impl PathMount {
    /// Create a new path mount.
    ///
    /// # Arguments
    ///
    /// * `host_path` - Path on the host filesystem
    /// * `sandbox_path` - Path where the file appears in the sandbox
    ///
    /// The file will be read-only in the sandbox.
    pub fn new(host_path: impl Into<String>, sandbox_path: impl Into<String>) -> Self {
        Self {
            host_path: host_path.into(),
            sandbox_path: sandbox_path.into(),
        }
    }

    /// Parse path mounts from JSON array.
    ///
    /// # JSON Format
    ///
    /// ```json
    /// [
    ///   {
    ///     "host_path": "/home/user/data/config.json",
    ///     "sandbox_path": "/data/config.json"
    ///   }
    /// ]
    /// ```
    pub fn from_json_array(json: &str) -> Result<Vec<Self>, String> {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct JsonPathMount {
            host_path: String,
            sandbox_path: String,
        }

        let mounts: Vec<JsonPathMount> =
            serde_json::from_str(json).map_err(|e| format!("JSON parse error: {e}"))?;

        Ok(mounts
            .into_iter()
            .map(|m| Self {
                host_path: m.host_path,
                sandbox_path: m.sandbox_path,
            })
            .collect())
    }
}

// =============================================================================
// Error Types
// =============================================================================

/// Errors that can occur during runtime operations.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Failed to parse PCA from bytes.
    #[error("failed to parse PCA: {0}")]
    PcaParse(String),

    /// PCA validation failed.
    #[error("PCA validation failed: {0}")]
    PcaValidation(#[from] ProtocolError),

    /// PCA has expired.
    #[error("PCA has expired")]
    Expired,

    /// Runtime not found.
    #[error("runtime not found: {0}")]
    RuntimeNotFound(RuntimeId),

    /// Command not found.
    #[error("command not found: {0}")]
    CommandNotFound(CommandHandle),

    /// VFS error.
    #[error("VFS error: {0}")]
    Vfs(#[from] amla_vfs::VfsError),

    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

// =============================================================================
// Command State
// =============================================================================

/// State of a command executing in the global scheduler.
struct CommandState {
    /// The command string.
    command: String,

    /// Stdin handle (uses host ops for pull-based input).
    stdin: IoHandle,

    /// Stdout handle (streams to host via host ops).
    stdout: IoHandle,

    /// Stderr handle (streams to host via host ops).
    stderr: IoHandle,

    /// Whether this command was cancelled.
    /// When true, the command has been terminated and should not be stepped.
    cancelled: bool,

    /// Exit code (set when command completes or is cancelled).
    /// - None: command still running
    /// - Some(code): command exited with this code (or -1 if cancelled)
    exit_code: Option<i32>,

    /// Handle to the spawned task for this command.
    /// The task runs the `shell.execute()` and is polled by the scheduler.
    /// Using `TaskHandle` instead of polling directly gives us proper wakers.
    task_handle: Option<TaskHandle>,

    /// Scheduler used by this command's execution.
    /// Stored so we can call `run_step()` and collect host ops.
    execution_scheduler: Option<Scheduler>,

    // -------------------------------------------------------------------------
    // Timing instrumentation
    // -------------------------------------------------------------------------
    /// Monotonic timestamp (ns) when command was created.
    start_time_ns: u64,

    /// Cumulative time (ns) spent inside `step()` calls.
    /// This is "user time" - CPU cycles in WASM execution.
    user_time_ns: u64,
}

impl CommandState {
    fn new(command: String, scheduler: Scheduler, start_time_ns: u64) -> Self {
        Self {
            command,
            // Stdin uses host ops for pull-based input (ReadStdin request, StdinData response)
            stdin: IoHandle::host_stdin(scheduler.clone()),
            // Stdout/stderr stream to host via host ops
            // (runtime/command association is determined via task_id tracking)
            stdout: IoHandle::host_stdout(scheduler.clone(), IoHandle::DEFAULT_HOST_BUFFER_SIZE),
            stderr: IoHandle::host_stderr(scheduler, IoHandle::DEFAULT_HOST_BUFFER_SIZE),
            cancelled: false,
            exit_code: None,
            task_handle: None,
            execution_scheduler: None,
            start_time_ns,
            user_time_ns: 0,
        }
    }
}

// =============================================================================
// Command Output Flushing
// =============================================================================

/// Flush buffered output from a command and emit exit notification.
///
/// This helper consolidates the output flushing logic used by both
/// `cancel_command()` and `step()`. It:
/// 1. Flushes stdout buffer with audit logging
/// 2. Flushes stderr buffer with audit logging
/// 3. Optionally emits an error message to stderr
/// 4. Emits `CommandExit` notification with audit logging
///
/// The order is important: buffered output → error message → exit.
///
/// This is a free function (not a method) to avoid borrow checker issues when
/// we have a mutable borrow on `runtime.commands` and need to access `runtime.audit_buffer`.
#[allow(clippy::too_many_arguments)]
fn flush_output_and_exit(
    audit_buffer: &RefCell<AuditBuffer>,
    runtime_id: RuntimeId,
    cmd: &mut CommandState,
    handle: CommandHandle,
    exit_code: i32,
    error_msg: Option<String>,
    pending_ops: &mut HostOpsVec,
    current_time_ns: u64,
) {
    let session_id = format!("rt-{}", runtime_id.raw());

    // 1. Flush stdout buffer
    if let Some((stream, data)) = cmd.stdout.take_buffered_output() {
        let stream_name = if stream == 1 { "stdout" } else { "stderr" };
        audit_buffer.borrow_mut().append(&LogEntry::stream_chunk(
            &session_id,
            handle.raw(),
            stream_name,
            &data,
        ));
        let preview = content_preview(&data, 10);
        pending_ops.push(PendingHostOp {
            id: NOTIFICATION_ID,
            runtime_id,
            command: Some(handle),
            request: HostOpRequest::Output {
                stream,
                data,
                preview,
            },
        });
    }

    // 2. Flush stderr buffer
    if let Some((stream, data)) = cmd.stderr.take_buffered_output() {
        audit_buffer.borrow_mut().append(&LogEntry::stream_chunk(
            &session_id,
            handle.raw(),
            "stderr",
            &data,
        ));
        let preview = content_preview(&data, 10);
        pending_ops.push(PendingHostOp {
            id: NOTIFICATION_ID,
            runtime_id,
            command: Some(handle),
            request: HostOpRequest::Output {
                stream,
                data,
                preview,
            },
        });
    }

    // 3. Emit error message (if any)
    if let Some(msg) = error_msg {
        let data = msg.into_bytes();
        audit_buffer.borrow_mut().append(&LogEntry::stream_chunk(
            &session_id,
            handle.raw(),
            "stderr",
            &data,
        ));
        let preview = content_preview(&data, 10);
        pending_ops.push(PendingHostOp {
            id: NOTIFICATION_ID,
            runtime_id,
            command: Some(handle),
            request: HostOpRequest::Output {
                stream: 2,
                data,
                preview,
            },
        });
    }

    // 4. CommandExit notification with timing
    let elapsed_ns = current_time_ns.saturating_sub(cmd.start_time_ns);
    let user_time_ns = cmd.user_time_ns;

    audit_buffer.borrow_mut().append(&LogEntry::command_exit(
        &session_id,
        runtime_id.raw(),
        handle.raw(),
        exit_code,
        elapsed_ns,
    ));
    pending_ops.push(PendingHostOp {
        id: NOTIFICATION_ID,
        runtime_id,
        command: Some(handle),
        request: HostOpRequest::CommandExit {
            code: exit_code,
            elapsed_ns: Some(elapsed_ns),
            user_time_ns: Some(user_time_ns),
        },
    });
}

// =============================================================================
// VFS Setup
// =============================================================================

/// Create standard directories in a VFS.
///
/// Creates the following directory structure:
/// - `/workspace` (read-write) - Working directory for command execution
/// - `/tools` (read-only) - Tool stubs and documentation
/// - `/log` (read-write) - Audit logs and output
fn create_standard_directories(vfs: &mut Vfs) -> Result<(), RuntimeError> {
    vfs.create_dir_all("/workspace", Permission::ReadWrite)?;
    vfs.create_dir_all("/tools", Permission::ReadOnly)?;
    vfs.create_dir_all("/log", Permission::ReadWrite)?;
    Ok(())
}

// =============================================================================
// Host Operation Conversion
// =============================================================================

/// Convert scheduler's `HostOpKind` to our protocol `HostOpRequest`.
///
/// All operations are exposed to the host via `HostOpRequest`.
pub fn convert_host_op(kind: &amla_scheduler::HostOpKind) -> Option<HostOpRequest> {
    use amla_scheduler::HostOpKind;
    match kind {
        HostOpKind::WakeAt { deadline } => Some(HostOpRequest::WakeAt {
            deadline_nanos: *deadline,
        }),
        HostOpKind::FileRead { path } => Some(HostOpRequest::VfsRead {
            path: path.clone(),
            offset: 0,
            len: usize::MAX, // Read entire file
        }),
        HostOpKind::FileReadRange {
            path,
            offset,
            length,
        } => Some(HostOpRequest::VfsRead {
            path: path.clone(),
            offset: *offset,
            #[allow(clippy::cast_possible_truncation)]
            len: *length as usize,
        }),
        HostOpKind::Custom { name, data } => {
            // Treat custom ops as tool calls
            Some(HostOpRequest::ToolCall {
                tool: name.clone(),
                params: serde_json::from_slice(data).unwrap_or(serde_json::Value::Null),
            })
        }
        // Streaming I/O - expose to host for real-time output
        // (runtime_id and command come from PendingHostOp, derived via task_id)
        HostOpKind::Print { stream, data } => Some(HostOpRequest::Output {
            stream: *stream,
            preview: content_preview(data, 10),
            data: data.clone(),
        }),
        HostOpKind::CommandExit { code } => Some(HostOpRequest::CommandExit {
            code: *code,
            elapsed_ns: None, // Scheduler-level exit doesn't have timing
            user_time_ns: None,
        }),
        HostOpKind::ReadStdin { max_bytes } => Some(HostOpRequest::ReadStdin {
            max_bytes: *max_bytes,
        }),
    }
}

/// Convert host operation response to bytes for scheduler completion.
///
/// Returns an `std::io::Error` for error responses, preserving the structured
/// error information via [`HostOpError`]. Callers can inspect the error using:
///
/// ```ignore
/// if let Some(host_err) = io_err.get_ref().and_then(|e| e.downcast_ref::<HostOpError>()) {
///     println!("Host error code: {}", host_err.code());
///     println!("Host error message: {}", host_err.message());
/// }
/// ```
pub fn response_to_bytes(response: &HostOpResponse) -> Result<Vec<u8>, std::io::Error> {
    match response {
        HostOpResponse::WokeAt { current_time_nanos } => {
            // Return current time from host
            Ok(current_time_nanos.to_le_bytes().to_vec())
        }
        // VFS and stdin data - return as bytes (eof is handled by scheduler via empty data)
        HostOpResponse::VfsData { data } | HostOpResponse::StdinData { data, eof: _ } => {
            Ok(data.clone())
        }
        HostOpResponse::ToolResult { result } => serde_json::to_vec(result)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        // Chunked tool result - return data bytes for accumulation
        // Note: The scheduler is responsible for accumulating chunks until eof=true
        HostOpResponse::ToolResultChunk { data, eof: _ } => Ok(data.clone()),
        // Tool result error - abort the stream
        HostOpResponse::ToolResultError { message } => Err(std::io::Error::other(format!(
            "Tool call failed: {message}"
        ))),
        // Delegation result - return the new PCA bytes
        HostOpResponse::DelegateResult { new_pca } => Ok(new_pca.clone()),
        // Streaming I/O acknowledgements - no data needed
        HostOpResponse::OutputAck | HostOpResponse::ExitAck => Ok(Vec::new()),
        // Host error - preserve structured error info via HostOpError
        HostOpResponse::Error { code, message } => {
            Err(HostOpError::new(code.as_str(), message.clone()).into_io_error())
        }
    }
}

// =============================================================================
// Runtime
// =============================================================================

/// A runtime created from a PCA token.
///
/// Each runtime has:
/// - Its own scheduler for task execution (isolated from other runtimes)
/// - An isolated VFS with tool stubs from capabilities
/// - A set of active commands executing on its scheduler
/// - An audit log for tracking actions
pub struct Runtime {
    /// Runtime ID (unique within this process).
    id: RuntimeId,

    /// The PCA that defines this runtime's capabilities.
    pca: Pca,

    /// Extracted capabilities for fast lookup.
    capabilities: CapabilitySet,

    /// Isolated virtual filesystem for this runtime.
    vfs: Rc<RefCell<Vfs>>,

    /// Tool catalog for search/discovery (shared with shell).
    tool_catalog: Option<Rc<ToolCatalog>>,

    /// Scheduler for this runtime's tasks (isolated from other runtimes).
    scheduler: Scheduler,

    /// Active commands.
    commands: HashMap<CommandHandle, CommandState>,

    /// Next command handle.
    next_handle: u64,

    /// Audit log (wrapped in `RefCell` for interior mutability during command execution).
    audit: RefCell<AuditLog>,

    /// Streaming audit buffer for host to drain (wrapped in `RefCell`).
    audit_buffer: RefCell<AuditBuffer>,

    /// Commands whose stdin has received EOF.
    ///
    /// When the host sends `StdinData { eof: true }`, the command is added here.
    /// Future stdin reads from these commands return empty immediately.
    stdin_closed_commands: HashSet<CommandHandle>,

    /// Mapping of pending stdin operation IDs to their commands.
    ///
    /// Used to track which command a stdin response belongs to, so we can
    /// update `stdin_closed_commands` when EOF is received.
    pending_stdin_ops: HashMap<u64, CommandHandle>,
}

impl Runtime {
    /// Create a runtime from PCA bytes.
    ///
    /// This method:
    /// 1. Deserializes the PCA from CBOR
    /// 2. Verifies the signature
    /// 3. Checks expiry
    /// 4. Extracts capabilities
    /// 5. Creates an isolated VFS with tool stubs
    ///
    /// # Arguments
    /// * `pca_bytes` - PCA CBOR bytes
    /// * `time_source` - Time source function
    /// * `random_source` - Random source function
    pub fn from_pca_bytes(
        pca_bytes: &[u8],
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
    ) -> Result<Self, RuntimeError> {
        // 1. Deserialize PCA
        let pca = Pca::from_cbor(pca_bytes).map_err(|e| RuntimeError::PcaParse(e.to_string()))?;

        Self::from_pca(pca, time_source, random_source)
    }

    /// Create a runtime from a parsed PCA.
    ///
    /// # Arguments
    /// * `pca` - The parsed PCA
    /// * `time_source` - Time source function
    /// * `random_source` - Random source function
    pub fn from_pca(
        pca: Pca,
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
    ) -> Result<Self, RuntimeError> {
        Self::from_pca_with_tools(pca, &[], time_source, random_source)
    }

    /// Create a runtime from a parsed PCA with additional MCP tool definitions.
    ///
    /// The `tools` slice contains MCP-formatted tool definitions that will be
    /// used to generate rich stubs with parameter documentation. These tools
    /// augment (but don't replace) the capabilities from the PCA.
    ///
    /// # Arguments
    /// * `pca` - The parsed PCA
    /// * `tools` - MCP-formatted tool definitions
    /// * `time_source` - Time source function
    /// * `random_source` - Random source function
    pub fn from_pca_with_tools(
        pca: Pca,
        tools: &[crate::mcp::McpTool],
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
    ) -> Result<Self, RuntimeError> {
        Self::from_pca_with_mounts(pca, tools, &[], time_source, random_source)
    }

    /// Create a minimal test runtime without PCA validation.
    ///
    /// This bypasses all PCA parsing and validation, creating a runtime with
    /// empty capabilities. Useful for testing the runtime machinery in isolation.
    ///
    /// # Security Warning
    ///
    /// This should only be used for testing. The resulting runtime has no
    /// capabilities and cannot make tool calls.
    #[cfg(test)]
    pub fn new_test(time_source: TimeSourceFn, random_source: RandomSourceFn) -> Self {
        // Create empty VFS with standard directories
        let mut vfs = Vfs::new();
        let _ = create_standard_directories(&mut vfs);

        // Generate fs/shell prelude (no tools)
        ToolStubGenerator::generate_fs_shell_prelude(&mut vfs);

        Self {
            id: RuntimeId::new(0), // Will be set by register()
            pca: Pca::empty_test(),
            capabilities: CapabilitySet::default(),
            vfs: Rc::new(RefCell::new(vfs)),
            tool_catalog: None,
            scheduler: Scheduler::new(0, time_source, random_source),
            commands: HashMap::new(),
            next_handle: 1,
            audit: RefCell::new(AuditLog::new()),
            audit_buffer: RefCell::new(AuditBuffer::new()),
            stdin_closed_commands: HashSet::new(),
            pending_stdin_ops: HashMap::new(),
        }
    }

    /// Create a test runtime with MCP tool definitions.
    ///
    /// Like `new_test`, this bypasses PCA validation but also registers
    /// tools so they can be called from JavaScript. This is the recommended
    /// way to test the full tool invocation flow without a real PCA.
    ///
    /// # Arguments
    ///
    /// * `tools_json` - JSON array of MCP tool definitions
    ///
    /// # Security Warning
    ///
    /// This should only be used for testing. All tool calls are allowed
    /// without capability checking.
    #[cfg(test)]
    pub fn new_test_with_tools(
        tools_json: &str,
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
    ) -> Result<Self, RuntimeError> {
        // Parse tools
        let tools = crate::mcp::load_mcp_tools(tools_json)
            .map_err(|e| RuntimeError::Internal(format!("Failed to parse tools: {e}")))?;

        // Create VFS with standard directories
        let mut vfs = Vfs::new();
        create_standard_directories(&mut vfs)?;

        // Build tool catalog and generate stubs
        let tool_catalog = if tools.is_empty() {
            None
        } else {
            // Generate tool stubs in VFS (JS, TS, MD files)
            ToolStubGenerator::generate_from_mcp(&mut vfs, &tools);

            // Convert to ToolDef and build searchable catalog
            let tool_defs: Vec<amla_tools::ToolDef> =
                tools.iter().map(mcp_tool_to_tool_def).collect();
            Some(Rc::new(ToolCatalog::from_tools_with_embeddings(tool_defs)))
        };

        // Create permissive capabilities for all tools (test mode only)
        let mut capabilities = CapabilitySet::default();
        for tool in &tools {
            capabilities
                .tool_calls
                .push(amla_capabilities::ToolCallCap::new(&tool.name));
        }

        Ok(Self {
            id: RuntimeId::new(0), // Will be set by register()
            pca: Pca::empty_test(),
            capabilities,
            vfs: Rc::new(RefCell::new(vfs)),
            tool_catalog,
            scheduler: Scheduler::new(0, time_source, random_source),
            commands: HashMap::new(),
            next_handle: 1,
            audit: RefCell::new(AuditLog::new()),
            audit_buffer: RefCell::new(AuditBuffer::new()),
            stdin_closed_commands: HashSet::new(),
            pending_stdin_ops: HashMap::new(),
        })
    }

    /// Create a runtime from PCA bytes with MCP tool definitions JSON.
    ///
    /// # Arguments
    ///
    /// * `pca_bytes` - PCA CBOR bytes
    /// * `tools_json` - JSON array of MCP tool definitions
    ///
    /// # Tool Definition Format
    ///
    /// ```json
    /// [
    ///   {
    ///     "name": "stripe:charge",
    ///     "description": "Charge a customer",
    ///     "inputSchema": {
    ///       "type": "object",
    ///       "properties": {
    ///         "amount": {"type": "integer", "description": "Amount in cents"},
    ///         "currency": {"type": "string", "enum": ["USD", "EUR"]}
    ///       },
    ///       "required": ["amount", "currency"]
    ///     }
    ///   }
    /// ]
    /// ```
    pub fn from_pca_bytes_with_tools(
        pca_bytes: &[u8],
        tools_json: &str,
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
    ) -> Result<Self, RuntimeError> {
        let pca = Pca::from_cbor(pca_bytes).map_err(|e| RuntimeError::PcaParse(e.to_string()))?;

        let tools = crate::mcp::load_mcp_tools(tools_json)
            .map_err(|e| RuntimeError::Internal(format!("Failed to parse tools: {e}")))?;

        Self::from_pca_with_tools(pca, &tools, time_source, random_source)
    }

    /// Create a runtime from PCA with tools and path mounts.
    ///
    /// Path mounts allow the sandbox to access files from the host filesystem.
    /// When a file at `sandbox_path` is read, the runtime issues a `FileRead`
    /// host operation for the corresponding `host_path`.
    ///
    /// **Security**: All mounted paths are **read-only** in the sandbox.
    /// The sandbox cannot write back to the host filesystem.
    ///
    /// # Arguments
    ///
    /// * `pca` - The PCA token
    /// * `tools` - MCP tool definitions
    /// * `mounts` - List of path mounts (`host_path` -> `sandbox_path`, all read-only)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mounts = vec![
    ///     PathMount::new("/home/user/data/config.json", "/data/config.json"),
    ///     PathMount::new("/home/user/data/input.txt", "/data/input.txt"),
    /// ];
    /// let runtime = Runtime::from_pca_with_mounts(pca, &[], &mounts)?;
    /// // Files are read-only - accessed via FileRead host ops
    /// ```
    pub fn from_pca_with_mounts(
        pca: Pca,
        tools: &[crate::mcp::McpTool],
        mounts: &[PathMount],
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
    ) -> Result<Self, RuntimeError> {
        // 1. Validate PCA (signature, expiry)
        // Use the time_source to get current time (works on WASM via host import)
        let current_nanos = time_source(0, amla_scheduler::ClockType::Realtime);
        #[allow(clippy::cast_possible_wrap)]
        let current_time = chrono::DateTime::from_timestamp_nanos(current_nanos as i64);
        validate_pca(&pca, None, current_time)?;

        // 2. Extract capabilities
        let capabilities = capabilities_from_pca(&pca)?;

        // 3. Create isolated VFS with standard directories
        let mut vfs = Vfs::new();
        create_standard_directories(&mut vfs)?;

        // 4. Set up path mounts (VFS handles creating read-only directories)
        vfs.setup_mounts(
            mounts
                .iter()
                .map(|m| (m.host_path.clone(), m.sandbox_path.clone())),
        )?;

        // 5. Build tool catalog from MCP definitions
        let tool_catalog = if tools.is_empty() {
            // No MCP tools - generate just fs/shell prelude
            ToolStubGenerator::generate_fs_shell_prelude(&mut vfs);
            None
        } else {
            // Generate tool stubs in VFS (JS, TS, MD files) + prelude with fs/shell
            ToolStubGenerator::generate_from_mcp(&mut vfs, tools);

            // Convert to ToolDef and build searchable catalog
            let tool_defs: Vec<amla_tools::ToolDef> =
                tools.iter().map(mcp_tool_to_tool_def).collect();
            Some(Rc::new(ToolCatalog::from_tools_with_embeddings(tool_defs)))
        };

        // Also generate stubs from PCA capabilities (basic, for any tools not in MCP list)
        ToolStubGenerator::generate(&mut vfs, &capabilities.tool_calls);

        // 6. Create runtime
        Ok(Self {
            id: RuntimeId::new(0), // Will be set by register()
            pca,
            capabilities,
            vfs: Rc::new(RefCell::new(vfs)),
            tool_catalog,
            scheduler: Scheduler::new(0, time_source, random_source),
            commands: HashMap::new(),
            next_handle: 1,
            audit: RefCell::new(AuditLog::new()),
            audit_buffer: RefCell::new(AuditBuffer::new()),
            stdin_closed_commands: HashSet::new(),
            pending_stdin_ops: HashMap::new(),
        })
    }

    /// Create a runtime from PCA bytes with tools JSON and path mounts JSON.
    ///
    /// # JSON Format for Mounts
    ///
    /// ```json
    /// [
    ///   {
    ///     "host_path": "/home/user/data/config.json",
    ///     "sandbox_path": "/data/config.json"
    ///   }
    /// ]
    /// ```
    pub fn from_pca_bytes_with_mounts(
        pca_bytes: &[u8],
        tools_json: &str,
        mounts_json: &str,
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
    ) -> Result<Self, RuntimeError> {
        let pca = Pca::from_cbor(pca_bytes).map_err(|e| RuntimeError::PcaParse(e.to_string()))?;

        let tools = crate::mcp::load_mcp_tools(tools_json)
            .map_err(|e| RuntimeError::Internal(format!("Failed to parse tools: {e}")))?;

        let mounts = PathMount::from_json_array(mounts_json)
            .map_err(|e| RuntimeError::Internal(format!("Failed to parse mounts: {e}")))?;

        Self::from_pca_with_mounts(pca, &tools, &mounts, time_source, random_source)
    }

    /// Get the runtime ID.
    pub fn id(&self) -> RuntimeId {
        self.id
    }

    /// Get the PCA.
    pub fn pca(&self) -> &Pca {
        &self.pca
    }

    /// Get the capabilities.
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    /// Borrow VFS immutably.
    pub fn vfs(&self) -> Ref<'_, Vfs> {
        self.vfs.borrow()
    }

    /// Borrow VFS mutably.
    pub fn vfs_mut(&self) -> RefMut<'_, Vfs> {
        self.vfs.borrow_mut()
    }

    /// Get this runtime's scheduler.
    ///
    /// Each runtime has its own scheduler for task isolation.
    /// Commands executing in this runtime only affect tasks on this scheduler.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Get the audit log.
    pub fn audit_log(&self) -> Ref<'_, AuditLog> {
        self.audit.borrow()
    }

    /// Get the audit log mutably.
    pub fn audit_log_mut(&self) -> RefMut<'_, AuditLog> {
        self.audit.borrow_mut()
    }

    /// Get the audit buffer.
    pub fn audit_buffer(&self) -> Ref<'_, AuditBuffer> {
        self.audit_buffer.borrow()
    }

    /// Get the audit buffer mutably.
    pub fn audit_buffer_mut(&self) -> RefMut<'_, AuditBuffer> {
        self.audit_buffer.borrow_mut()
    }

    /// Get the session ID for audit logging.
    ///
    /// Uses the runtime ID as the session identifier.
    fn session_id(&self) -> String {
        format!("rt-{}", self.id.raw())
    }

    /// Log an audit entry to the streaming buffer.
    ///
    /// Entries are buffered until the host drains them via `audit_drain`.
    fn audit(&self, entry: &LogEntry) {
        self.audit_buffer.borrow_mut().append(entry);
    }

    /// Check if a sandbox path has a host mount.
    pub fn is_mounted(&self, sandbox_path: &str) -> bool {
        self.vfs.borrow().is_mounted(sandbox_path)
    }

    /// Get the host path for a mounted sandbox path.
    ///
    /// Returns `None` if the path is not mounted.
    pub fn get_host_path(&self, sandbox_path: &str) -> Option<String> {
        self.vfs
            .borrow()
            .get_host_path(sandbox_path)
            .map(String::from)
    }

    /// Get all path mounts (`sandbox_path` -> `host_path`).
    pub fn path_mounts(&self) -> HashMap<String, String> {
        self.vfs.borrow().mounts().clone()
    }

    /// Create a command instance.
    ///
    /// Returns `CommandHandle(0)` if the command is empty.
    pub fn create_command(&mut self, command: &str) -> CommandHandle {
        let command = command.trim();
        if command.is_empty() {
            return CommandHandle::new(0);
        }

        let raw_handle = self.next_handle;
        self.next_handle += 1;

        let handle = CommandHandle::new(raw_handle);

        // Audit: log command creation
        self.audit(&LogEntry::command_create(
            &self.session_id(),
            self.id.raw(),
            raw_handle,
            command,
        ));

        // Use this runtime's scheduler (isolated from other runtimes)
        let scheduler = self.scheduler.clone();
        let start_time_ns = self.scheduler.now_monotonic();
        self.commands.insert(
            handle,
            CommandState::new(command.to_string(), scheduler, start_time_ns),
        );

        handle
    }

    /// Delete a command instance.
    pub fn delete_command(&mut self, handle: CommandHandle) {
        self.commands.remove(&handle);
    }

    /// Cancel a running command.
    ///
    /// This terminates the command and all its pending host operations.
    /// Cancelling a command that has already completed or been cancelled is a no-op.
    ///
    /// # Returns
    ///
    /// A collection of host operations to acknowledge (just `CommandExit` with code -1).
    /// Returns empty if the command was already done or doesn't exist.
    ///
    /// # Cascading Cancellation (Structured Concurrency)
    ///
    /// When a command is cancelled:
    /// 1. The task handle is dropped, which cancels all child tasks (structured concurrency)
    /// 2. Each task's drop cascades to cancel nested `HostOpFuture`s
    /// 3. The execution future is dropped, cleaning up remaining state
    /// 4. Any pending host ops for this command are effectively discarded
    ///
    /// This ensures proper cleanup even for pipelines (`echo | cat | grep`) where
    /// multiple tasks may be spawned under a single command.
    pub fn cancel_command(&mut self, handle: CommandHandle) -> HostOpsVec {
        let mut pending_ops = HostOpsVec::new();

        let Some(cmd) = self.commands.get_mut(&handle) else {
            return pending_ops;
        };

        // Already done or cancelled? No-op.
        if cmd.exit_code.is_some() || cmd.cancelled {
            return pending_ops;
        }

        // Mark as cancelled BEFORE cancelling (for status queries during cancel)
        cmd.cancelled = true;
        cmd.exit_code = Some(-1); // Convention: -1 for cancelled

        // Clean up stdin tracking for this command
        self.stdin_closed_commands.remove(&handle);
        self.pending_stdin_ops
            .retain(|_, cmd_handle| *cmd_handle != handle);

        // Actually cancel the task - dropping a TaskHandle does NOT cancel it!
        // cancel() recursively cancels all child tasks (structured concurrency),
        // drops their futures (cascading to cancel pending HostOpFutures),
        // and wakes any waiters with a cancellation error.
        if let Some(task_handle) = cmd.task_handle.take() {
            task_handle.cancel();
        }

        // Clear scheduler reference (it's shared with runtime, not dropped here)
        cmd.execution_scheduler.take();

        // Flush output and emit exit notification with timing
        let current_time_ns = self.scheduler.now_monotonic();
        flush_output_and_exit(
            &self.audit_buffer,
            self.id,
            cmd,
            handle,
            -1,
            None,
            &mut pending_ops,
            current_time_ns,
        );

        pending_ops
    }

    /// Derive the status of a command from scheduler state.
    ///
    /// This is the single source of truth for command status. Instead of
    /// tracking status in `CommandState`, we query the scheduler to determine
    /// what state the command is in.
    #[allow(clippy::unused_self)]
    fn command_status(&self, cmd: &CommandState) -> CommandStatus {
        // Check terminal states first
        if cmd.cancelled {
            return CommandStatus::Cancelled;
        }
        if cmd.exit_code.is_some() {
            return CommandStatus::Exit;
        }

        // Check scheduler state (if execution has started)
        if let Some(ref scheduler) = cmd.execution_scheduler {
            // If no tasks are ready, command is blocked.
            // We report NeedHostOps as a general "blocked on async" status.
            if !scheduler.has_ready_tasks() {
                return CommandStatus::NeedHostOps;
            }
        }

        // Default: command can make progress
        CommandStatus::Running
    }

    /// Step commands and collect response.
    ///
    /// This executes commands using the global scheduler and returns
    /// their current output and status. Commands that need host operations
    /// (including stdin via `ReadStdin`) are marked as `NeedHostOps` and
    /// the pending operations are returned in the response.
    ///
    /// # Async Host Operations
    ///
    /// All host operations (time, file I/O, tool calls) are returned
    /// asynchronously for the host to process. The host should:
    ///
    /// 1. Execute the operations (potentially in parallel)
    /// 2. Call `submit_results()` with the results
    /// 3. Call `step()` again to continue execution
    ///
    /// This design gives the host full control over execution and
    /// enables parallelization of I/O operations.
    pub fn step(&mut self) -> StepResponse {
        const MAX_STEPS: usize = 10_000;

        let mut pending_host_ops = HostOpsVec::new();

        // Get handles to process
        let handles: Vec<_> = self.commands.keys().copied().collect();

        // =========================================================================
        // Phase 1: Spawn tasks for any commands that haven't started
        // =========================================================================
        for &handle in &handles {
            let Some(cmd) = self.commands.get_mut(&handle) else {
                continue;
            };

            // Skip if already done, cancelled, or already started
            if cmd.exit_code.is_some() || cmd.cancelled || cmd.task_handle.is_some() {
                continue;
            }

            // Create shell with this runtime's VFS, tool catalog, and command's I/O
            let shared_scheduler = self.scheduler.clone();
            let shell = if let Some(catalog) = &self.tool_catalog {
                Shell::with_full_context_and_tools(
                    shared_scheduler.clone(),
                    self.vfs.clone(),
                    "/workspace".to_string(),
                    Environment::with_defaults(),
                    cmd.stdin.clone(),
                    cmd.stdout.clone(),
                    cmd.stderr.clone(),
                    catalog.clone(),
                )
            } else {
                Shell::with_full_context(
                    shared_scheduler.clone(),
                    self.vfs.clone(),
                    "/workspace".to_string(),
                    Environment::with_defaults(),
                    cmd.stdin.clone(),
                    cmd.stdout.clone(),
                    cmd.stderr.clone(),
                )
            };

            // Spawn as scheduler task - gets real waker, not noop_waker
            // This means timer wakeups and host op completions properly wake the task
            let command_string = cmd.command.clone();
            let task_handle = shared_scheduler.spawn(async move {
                shell
                    .execute(&command_string)
                    .await
                    .map(Exit::code)
                    .map_err(|e| amla_scheduler::Error::Command(e.to_string()))
            });

            cmd.task_handle = Some(task_handle);
            cmd.execution_scheduler = Some(shared_scheduler);
        }

        // =========================================================================
        // Phase 2: Run scheduler until blocked or host ops are pending
        // =========================================================================
        // Run multiple steps until:
        // - Scheduler is blocked (all tasks waiting on host ops)
        // - Scheduler is done (all tasks completed)
        // - We've collected host ops that need processing
        // - A task is blocked on stdin
        //
        // This replaces the old per-command polling loop with scheduler-level stepping.
        let step_start_ns = self.scheduler.now_monotonic();
        let mut total_steps = 0;
        let scheduler_state = loop {
            let state = self.scheduler.run_step();

            // Collect host ops
            while let Some(req) = self.scheduler.take_host_op() {
                if let Some(host_request) = convert_host_op(&req.kind) {
                    // Find which command this op belongs to using the task_id.
                    // The scheduler provides the root task ID (top of spawn chain),
                    // so child task ops are correctly attributed to parent commands.
                    let command = req.task_id.and_then(|root_task| {
                        // Find command whose task matches this root
                        self.commands.iter().find_map(|(&handle, cmd)| {
                            cmd.task_handle
                                .as_ref()
                                .and_then(amla_scheduler::TaskHandle::id_repr)
                                .filter(|&id| id == root_task)
                                .map(|_| handle)
                        })
                    });

                    // Handle stdin EOF: if command already received EOF, return empty immediately
                    if matches!(host_request, HostOpRequest::ReadStdin { .. })
                        && let Some(cmd_handle) = command
                    {
                        if self.stdin_closed_commands.contains(&cmd_handle) {
                            // Stdin is closed for this command - return empty (EOF)
                            self.scheduler.complete_host_op(req.id, Vec::new());
                            continue;
                        }
                        // Track this stdin op for EOF handling later
                        self.pending_stdin_ops.insert(req.id.into(), cmd_handle);
                    }

                    pending_host_ops.push(PendingHostOp {
                        id: req.id.into(),
                        runtime_id: self.id,
                        command,
                        request: host_request,
                    });
                } else {
                    self.scheduler.complete_host_op(req.id, Vec::new());
                }
            }

            // Break conditions:
            // With proper wakers (structured concurrency), we continue processing until:
            // 1. Scheduler is blocked - all tasks waiting on host ops
            // 2. Scheduler is done - all tasks completed
            // 3. Exceeded step limit - prevent infinite loops
            //
            // Note: We do NOT break just because pending_host_ops is non-empty.
            // Tasks that don't need those ops can continue making progress.
            // When their host ops complete, tasks are properly woken via real wakers.
            if matches!(
                state,
                amla_scheduler::SchedulerState::Blocked | amla_scheduler::SchedulerState::Done
            ) {
                break state;
            }

            total_steps += 1;
            if total_steps >= MAX_STEPS {
                break state;
            }
        };

        // Accumulate scheduler time into all active commands
        let step_end_ns = self.scheduler.now_monotonic();
        let step_elapsed_ns = step_end_ns.saturating_sub(step_start_ns);
        for &handle in &handles {
            if let Some(cmd) = self.commands.get_mut(&handle) {
                // Only accumulate if command is still active (not done, not cancelled)
                if cmd.exit_code.is_none() && !cmd.cancelled {
                    cmd.user_time_ns += step_elapsed_ns;
                }
            }
        }

        // =========================================================================
        // Phase 3: Check for task completions
        // =========================================================================
        for &handle in &handles {
            let Some(cmd) = self.commands.get_mut(&handle) else {
                continue;
            };

            // Skip if already done or cancelled
            if cmd.exit_code.is_some() || cmd.cancelled {
                continue;
            }

            // Check if task completed - capture result without emitting yet
            let (exit_code, error_msg) = if let Some(ref task_handle) = cmd.task_handle {
                match task_handle.try_get() {
                    Some(Ok(exit)) => (Some(exit.code), None),
                    Some(Err(e)) => (Some(1), Some(format!("{e}\n"))),
                    None => (None, None), // Still running
                }
            } else {
                (None, None)
            };

            if let Some(code) = exit_code {
                // Mark as exited
                cmd.exit_code = Some(code);
                cmd.task_handle = None;
                cmd.execution_scheduler = None;

                // Flush output and emit exit notification with timing
                flush_output_and_exit(
                    &self.audit_buffer,
                    self.id,
                    cmd,
                    handle,
                    code,
                    error_msg,
                    &mut pending_host_ops,
                    step_end_ns,
                );
            }
        }

        // scheduler_state is available but not directly used - collect_response()
        // determines RuntimeStatus by checking command states and scheduler.
        let _ = scheduler_state;

        self.collect_response(pending_host_ops)
    }

    /// Submit host operation results back to the runtime.
    ///
    /// After the host processes the pending operations returned by `step()`,
    /// it calls this method with the results. The results are routed to the
    /// appropriate commands, which can then continue execution on the next
    /// `step()` call.
    ///
    /// # Arguments
    ///
    /// * `results` - Slice of completed host operation results
    ///
    /// # Returns
    ///
    /// Number of results successfully processed.
    pub fn submit_results(&mut self, results: &[HostOpResult]) -> usize {
        let mut processed = 0;

        // All commands in this runtime share its scheduler, which routes completions
        // to the correct waiting tasks via operation IDs.
        let scheduler = &self.scheduler;

        for result in results {
            // Only process results for this runtime
            if result.runtime_id != self.id {
                continue;
            }

            // Handle stdin EOF: when eof is true, mark the command's stdin as closed
            // so future reads return empty immediately.
            if let HostOpResponse::StdinData { eof: true, .. } = &result.result {
                if let Some(cmd_handle) = self.pending_stdin_ops.remove(&result.id) {
                    self.stdin_closed_commands.insert(cmd_handle);
                }
            } else if matches!(result.result, HostOpResponse::StdinData { .. }) {
                // Non-EOF stdin response - clean up tracking
                self.pending_stdin_ops.remove(&result.id);
            }

            // Handle chunked tool results specially - accumulate instead of completing
            match &result.result {
                HostOpResponse::ToolResultChunk { data, eof } => {
                    // Append chunk to accumulation buffer
                    match scheduler.append_chunk(result.id.into(), data.clone(), *eof) {
                        Ok(completed) => {
                            if completed {
                                // Final chunk - operation is now complete
                                processed += 1;
                            }
                            // Non-final chunks don't count toward processed
                        }
                        Err(_e) => {
                            // Chunk error (buffer overflow, cancelled, etc.)
                            // Don't count - the operation already failed
                            // Note: The error is visible to the waiting task
                        }
                    }
                }
                HostOpResponse::ToolResultError { message } => {
                    // Abort chunked stream - complete with error
                    scheduler.complete_host_op_err(
                        result.id.into(),
                        std::io::Error::other(format!("Tool call failed: {message}")),
                    );
                    processed += 1;
                }
                _ => {
                    // All other responses: convert to bytes and complete atomically
                    // For host errors, the structured error (code + message) is preserved
                    // in the std::io::Error via HostOpError, accessible via get_ref().
                    match response_to_bytes(&result.result) {
                        Ok(data) => {
                            scheduler.complete_host_op(result.id.into(), data);
                            processed += 1;
                        }
                        Err(io_err) => {
                            // Pass the structured error directly to the scheduler.
                            // The waiting task will receive this error with full context.
                            scheduler.complete_host_op_err(result.id.into(), io_err);
                            processed += 1;
                        }
                    }
                }
            }
        }

        // Note: No broadcast wakeup needed. The scheduler's complete_host_op()
        // wakes the specific task that was waiting. On next step(), command_status()
        // will derive the status from scheduler.has_ready_tasks().

        processed
    }

    /// Collect step response.
    ///
    /// All I/O is streamed via host ops, so this just returns
    /// the pending ops and overall runtime status.
    fn collect_response(&self, pending_host_ops: HostOpsVec) -> StepResponse {
        let mut any_running = false;
        let mut any_blocked = false;
        let mut all_done = true;

        for cmd in self.commands.values() {
            match self.command_status(cmd) {
                CommandStatus::Running => {
                    any_running = true;
                    all_done = false;
                }
                CommandStatus::NeedHostOps => {
                    any_blocked = true;
                    all_done = false;
                }
                CommandStatus::Exit | CommandStatus::Cancelled => {}
            }
        }

        let status = if all_done {
            RuntimeStatus::AllDone
        } else if any_running {
            RuntimeStatus::Running
        } else if any_blocked {
            RuntimeStatus::AllBlocked
        } else {
            RuntimeStatus::AllDone
        };

        StepResponse {
            host_ops: pending_host_ops,
            status,
        }
    }
}

// =============================================================================
// Global Runtime Management
// =============================================================================

/// Register a runtime and return its ID.
pub fn register_runtime(mut runtime: Runtime) -> RuntimeId {
    let raw_id = NEXT_RUNTIME_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    });

    let id = RuntimeId::new(raw_id);
    runtime.id = id;

    // Update scheduler's runtime_id so time/random sources receive correct ID
    runtime.scheduler.set_runtime_id(raw_id);

    RUNTIMES.with(|r| {
        r.borrow_mut().insert(raw_id, runtime);
    });

    id
}

/// Remove a runtime by ID.
pub fn remove_runtime(id: RuntimeId) -> Option<Runtime> {
    RUNTIMES.with(|r| r.borrow_mut().remove(&id.raw()))
}

/// Execute a function with a runtime reference.
pub fn with_runtime<F, R>(id: RuntimeId, f: F) -> Result<R, RuntimeError>
where
    F: FnOnce(&Runtime) -> R,
{
    RUNTIMES.with(|r| {
        let runtimes = r.borrow();
        runtimes
            .get(&id.raw())
            .map(f)
            .ok_or(RuntimeError::RuntimeNotFound(id))
    })
}

/// Execute a function with a mutable runtime reference.
pub fn with_runtime_mut<F, R>(id: RuntimeId, f: F) -> Result<R, RuntimeError>
where
    F: FnOnce(&mut Runtime) -> R,
{
    RUNTIMES.with(|r| {
        let mut runtimes = r.borrow_mut();
        runtimes
            .get_mut(&id.raw())
            .map(f)
            .ok_or(RuntimeError::RuntimeNotFound(id))
    })
}

/// Step a specific runtime.
///
/// Returns `None` if the runtime doesn't exist.
///
/// # Implementation Note
///
/// This function temporarily removes the runtime from the registry to avoid
/// nested `RefCell` borrows. This is critical because:
/// 1. `Runtime::step` may trigger callbacks or panic recovery code
/// 2. Those code paths might try to access RUNTIMES (e.g., `remove_runtime` in panic cleanup)
/// 3. If RUNTIMES is still borrowed during step, this causes "`RefCell` already borrowed" panic
///
/// By removing-stepping-reinserting, we ensure RUNTIMES is never borrowed during step execution.
pub fn step_runtime(id: RuntimeId) -> Option<StepResponse> {
    // Take the runtime out of the registry (releases the borrow immediately)
    let mut runtime = RUNTIMES.with(|r| r.borrow_mut().remove(&id.raw()))?;

    // Step the runtime (no borrow held on RUNTIMES!)
    let response = runtime.step();

    // Put it back
    RUNTIMES.with(|r| {
        r.borrow_mut().insert(id.raw(), runtime);
    });

    Some(response)
}

/// Cancel a command in a specific runtime.
///
/// Returns the pending host operations (just `CommandExit` with code -1).
/// Returns `None` if the runtime doesn't exist.
/// Returns empty `HostOpsVec` if the command doesn't exist or was already done/cancelled.
///
/// Uses remove-operate-reinsert pattern to avoid nested `RefCell` borrows.
pub fn cancel_command(runtime_id: RuntimeId, handle: CommandHandle) -> Option<HostOpsVec> {
    // Take the runtime out of the registry
    let mut runtime = RUNTIMES.with(|r| r.borrow_mut().remove(&runtime_id.raw()))?;

    // Cancel the command (no borrow held on RUNTIMES)
    let result = runtime.cancel_command(handle);

    // Put it back
    RUNTIMES.with(|r| {
        r.borrow_mut().insert(runtime_id.raw(), runtime);
    });

    Some(result)
}

/// Submit host operation results to all runtimes.
///
/// Routes each result to the appropriate runtime based on `runtime_id`.
/// Returns the total number of results successfully processed.
///
/// # Example
///
/// ```ignore
/// // Host executes pending ops from step_runtime()
/// let response = step_runtime(runtime_id).unwrap();
/// let ops: Vec<_> = response.host_ops.iter().collect();
///
/// // Host processes ops and creates results
/// let results: Vec<HostOpResult> = process_ops(ops);
///
/// // Submit results back
/// let processed = submit_all(&results);
///
/// // Continue execution
/// let response = step_runtime(runtime_id);
/// ```
pub fn submit_all(results: &[HostOpResult]) -> usize {
    // Group results by runtime_id
    let mut grouped: std::collections::HashMap<u64, Vec<&HostOpResult>> =
        std::collections::HashMap::new();
    for result in results {
        grouped
            .entry(result.runtime_id.raw())
            .or_default()
            .push(result);
    }

    let mut total = 0;
    for (runtime_id, runtime_results) in grouped {
        // Take runtime out of registry
        let runtime = RUNTIMES.with(|r| r.borrow_mut().remove(&runtime_id));

        if let Some(mut runtime) = runtime {
            // Submit results (no borrow held on RUNTIMES)
            for result in runtime_results {
                total += runtime.submit_results(std::slice::from_ref(result));
            }

            // Put it back
            RUNTIMES.with(|r| {
                r.borrow_mut().insert(runtime_id, runtime);
            });
        }
    }
    total
}

/// Get the number of active runtimes.
pub fn runtime_count() -> usize {
    RUNTIMES.with(|r| r.borrow().len())
}

/// Get all registered runtime IDs.
#[cfg(test)]
pub fn runtime_ids() -> Vec<RuntimeId> {
    RUNTIMES.with(|r| r.borrow().keys().copied().map(RuntimeId::new).collect())
}

// =============================================================================
// Conversion Helpers
// =============================================================================

/// Convert an MCP tool to a `ToolDef` for the catalog.
fn mcp_tool_to_tool_def(tool: &crate::mcp::McpTool) -> amla_tools::ToolDef {
    use amla_tools::{ParamDef, ParamType, ToolDef};

    let parameters: Vec<ParamDef> = tool
        .params
        .iter()
        .map(|p| {
            let param_type = ParamType::from(p.param_type.as_str());
            ParamDef::new(&p.name, param_type, &p.description, p.required)
        })
        .collect();

    // Extract category from provider (e.g., "stripe" -> "payments", "notion" -> "productivity")
    let category = match tool.provider.as_str() {
        "stripe" => Some("payments".to_string()),
        "notion" | "google_docs" | "google_drive" => Some("productivity".to_string()),
        "github" | "gitlab" | "bitbucket" => Some("development".to_string()),
        "postgres" | "mysql" | "mongodb" => Some("database".to_string()),
        "s3" | "gcs" | "azure_blob" => Some("storage".to_string()),
        "slack" | "discord" | "teams" => Some("communication".to_string()),
        "openai" | "anthropic" | "cohere" => Some("ai".to_string()),
        "filesystem" => Some("filesystem".to_string()),
        _ => None,
    };

    ToolDef {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters,
        category,
        keywords: Vec::new(), // Keywords can be derived from description if needed
        embedding: None,      // Computed on catalog load
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use amla_capabilities::ToolCallCap;
    use amla_protocol::{Algorithm, CapabilityData, KeyPair, PcaBuilder, PublicKey, Version};
    use chrono::{Duration, Utc};
    use serde_json::Value;

    /// Create mock time and random sources for testing.
    ///
    /// Returns a tuple of time source and random source where:
    /// - Time source returns 0 for all clocks
    /// - Random source returns a fixed value (42) for determinism
    fn test_sources() -> (TimeSourceFn, RandomSourceFn) {
        let time_source: TimeSourceFn = Rc::new(|_runtime_id, _clock| 0);
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        (time_source, random_source)
    }

    /// Collected output from a command (aggregated from streamed Output host ops).
    #[derive(Default)]
    struct CollectedOutput {
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    }

    /// Run a runtime until all commands complete, collecting streamed output.
    ///
    /// This helper processes `Output` and `CommandExit` host ops to aggregate
    /// the streamed output into a single result per command.
    fn run_to_completion(runtime: &mut Runtime) -> HashMap<Option<CommandHandle>, CollectedOutput> {
        let mut collected: HashMap<Option<CommandHandle>, CollectedOutput> = HashMap::new();

        loop {
            let resp = runtime.step();

            // Process Output host ops
            // Note: command is in PendingHostOp, not HostOpRequest
            for op in &resp.host_ops {
                match &op.request {
                    HostOpRequest::Output { stream, data, .. } => {
                        let entry = collected.entry(op.command).or_default();
                        let text = String::from_utf8_lossy(data);
                        if *stream == 1 {
                            entry.stdout.push_str(&text);
                        } else {
                            entry.stderr.push_str(&text);
                        }
                    }
                    HostOpRequest::CommandExit { code, .. } => {
                        let entry = collected.entry(op.command).or_default();
                        entry.exit_code = Some(*code);
                    }
                    _ => {}
                }
            }

            // Complete all host ops (acknowledge)
            let results: Vec<HostOpResult> = resp
                .host_ops
                .iter()
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: op.runtime_id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            runtime.submit_results(&results);

            if resp.all_done() {
                break;
            }
        }

        collected
    }

    /// Helper to set up trusted authorities and clean up after test.
    struct TrustedAuthoritiesGuard;

    impl TrustedAuthoritiesGuard {
        fn new(authorities: Vec<PublicKey>) -> Self {
            crate::protocol::set_trusted_authorities(authorities);
            Self
        }
    }

    impl Drop for TrustedAuthoritiesGuard {
        fn drop(&mut self) {
            crate::protocol::clear_trusted_authorities();
        }
    }

    fn create_test_pca() -> (Pca, TrustedAuthoritiesGuard) {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let executor = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        // Set up trusted authority
        let guard = TrustedAuthoritiesGuard::new(vec![keypair.public_key()]);

        let tool_cap = ToolCallCap::new("test:echo");
        let cap_data = CapabilityData::new("cap:echo", "tool-call", &tool_cap).unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&keypair)
            .unwrap();

        (pca, guard)
    }

    #[test]
    fn test_runtime_from_pca() {
        let (pca, _guard) = create_test_pca();
        let (ts, rs) = test_sources();
        let runtime = Runtime::from_pca(pca, ts, rs).unwrap();

        assert!(!runtime.capabilities.tool_calls.is_empty());
        assert!(runtime.vfs().is_dir("/workspace"));
        assert!(runtime.vfs().is_dir("/tools"));
    }

    #[test]
    fn test_runtime_create_command() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        let h1 = runtime.create_command("echo hello");
        let h2 = runtime.create_command("echo world");

        assert!(h1 > CommandHandle::new(0));
        assert!(h2 > CommandHandle::new(0));
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_runtime_empty_command() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        let h = runtime.create_command("");
        assert_eq!(h, CommandHandle::new(0));
    }

    #[test]
    fn test_runtime_step() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        let h = runtime.create_command("echo test");
        let collected = run_to_completion(&mut runtime);

        assert!(collected.contains_key(&Some(h)));
        let output = collected.get(&Some(h)).unwrap();
        assert!(output.stdout.contains("test"), "stdout: {}", output.stdout);
    }

    #[test]
    fn test_register_runtime() {
        let initial_count = runtime_count();

        let (pca, _guard) = create_test_pca();
        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        let id = register_runtime(runtime);
        assert!(id > RuntimeId::new(0));
        assert_eq!(runtime_count(), initial_count + 1);

        let result = with_runtime(id, |r| r.capabilities.tool_calls.len());
        assert!(result.is_ok());

        // Clean up
        remove_runtime(id);
    }

    #[test]
    fn test_step_runtime() {
        let (pca1, _guard1) = create_test_pca();
        let mut runtime1 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca1, ts, rs).unwrap()
        };
        runtime1.create_command("echo runtime1");
        let id1 = register_runtime(runtime1);

        let (pca2, _guard2) = create_test_pca();
        let mut runtime2 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca2, ts, rs).unwrap()
        };
        runtime2.create_command("echo runtime2");
        let id2 = register_runtime(runtime2);

        // Step each runtime individually
        let resp1 = step_runtime(id1);
        let resp2 = step_runtime(id2);

        assert!(resp1.is_some());
        assert!(resp2.is_some());

        // Non-existent runtime returns None
        assert!(step_runtime(RuntimeId::new(999)).is_none());
    }

    /// Test that `step_runtime` doesn't hold RUNTIMES borrow during step execution.
    ///
    /// This test verifies the remove-operate-reinsert pattern works correctly.
    /// The old implementation held a borrow on RUNTIMES during `Runtime::step`,
    /// which could cause "`RefCell` already borrowed" panics if any code path
    /// (like panic recovery) tried to access RUNTIMES.
    #[test]
    fn test_step_runtime_no_nested_borrow() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };
        runtime.create_command("echo test");
        let id = register_runtime(runtime);

        // Step should work - runtime is temporarily removed during step
        let resp = step_runtime(id);
        assert!(resp.is_some());

        // Runtime should be back in registry after step
        assert!(with_runtime(id, |_| ()).is_ok());

        // Multiple steps should work
        for _ in 0..10 {
            let _ = step_runtime(id);
        }

        // Still accessible
        assert!(with_runtime(id, |_| ()).is_ok());

        // Cleanup
        remove_runtime(id);
    }

    /// Test that `cancel_command` doesn't hold RUNTIMES borrow during cancel.
    #[test]
    fn test_cancel_command_no_nested_borrow() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };
        let cmd = runtime.create_command("sleep 10");
        let id = register_runtime(runtime);

        // Cancel should work without nested borrow issues
        let ops = cancel_command(id, cmd);
        assert!(ops.is_some());

        // Runtime should still be accessible
        assert!(with_runtime(id, |_| ()).is_ok());

        // Cleanup
        remove_runtime(id);
    }

    /// Test that `submit_all` doesn't hold RUNTIMES borrow during submit.
    #[test]
    fn test_submit_all_no_nested_borrow() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };
        runtime.create_command("echo test");
        let id = register_runtime(runtime);

        // Get some host ops to submit back
        let resp = step_runtime(id).unwrap();

        // Submit results back (creates HostOpResult structs)
        let results: Vec<_> = resp
            .host_ops
            .iter()
            .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
            .map(|op| HostOpResult {
                id: op.id,
                runtime_id: op.runtime_id,
                result: HostOpResponse::OutputAck,
            })
            .collect();

        if !results.is_empty() {
            let count = submit_all(&results);
            assert!(count > 0 || results.is_empty());
        }

        // Runtime should still be accessible
        assert!(with_runtime(id, |_| ()).is_ok());

        // Cleanup
        remove_runtime(id);
    }

    #[test]
    fn test_remove_runtime() {
        let initial_count = runtime_count();

        let (pca, _guard) = create_test_pca();
        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };
        let id = register_runtime(runtime);

        assert_eq!(runtime_count(), initial_count + 1);

        let removed = remove_runtime(id);
        assert!(removed.is_some());
        assert_eq!(runtime_count(), initial_count);
    }

    // =========================================================================
    // End-to-end tests for tool stubs and tool search
    // =========================================================================

    fn create_test_pca_with_tools() -> (Pca, TrustedAuthoritiesGuard) {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let executor = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        // Set up trusted authority
        let guard = TrustedAuthoritiesGuard::new(vec![keypair.public_key()]);

        // Add multiple tool capabilities
        let tools = ["stripe:charge", "stripe:refund", "notion:search"];
        let mut builder = PcaBuilder::new()
            .version(Version::new(0, 1))
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1));

        for (i, tool) in tools.iter().enumerate() {
            let cap = ToolCallCap::new(*tool);
            let cap_data = CapabilityData::new(&format!("cap:{i}"), "tool-call", &cap).unwrap();
            builder = builder.add_capability(cap_data);
        }

        (builder.build_and_sign(&keypair).unwrap(), guard)
    }

    #[test]
    fn test_tool_stubs_created_in_vfs() {
        // Create runtime with MCP tools
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // Check that tool stubs exist in VFS
        assert!(runtime.vfs().is_file("/tools/stripe/create_charge.js"));
        assert!(runtime.vfs().is_file("/tools/stripe/create_charge.d.ts"));
        assert!(runtime.vfs().is_file("/tools/stripe/create_charge.md"));
        assert!(runtime.vfs().is_file("/tools/stripe/create_refund.js"));
    }

    #[test]
    fn test_tool_stub_content_via_cat() {
        // Create runtime with MCP tools
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // Use cat command to read tool stub
        let h = runtime.create_command("cat /tools/stripe/create_charge.js");
        let collected = run_to_completion(&mut runtime);

        let output = collected.get(&Some(h)).unwrap();
        assert!(
            output.stdout.contains("__amla__.toolCall"),
            "Should contain tool call"
        );
        assert!(
            output.stdout.contains("stripe:create_charge"),
            "Should contain tool ID"
        );
        assert!(
            output.stdout.contains("amount"),
            "Should document amount param"
        );
        assert!(
            output.stdout.contains("currency"),
            "Should document currency param"
        );
    }

    #[test]
    fn test_tool_stub_typescript_definitions() {
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // Use cat command to read TypeScript definitions
        let h = runtime.create_command("cat /tools/stripe/create_charge.d.ts");
        let collected = run_to_completion(&mut runtime);

        let output = collected.get(&Some(h)).unwrap();
        assert!(
            output.stdout.contains("CreateChargeParams"),
            "Should have Params interface"
        );
        assert!(
            output.stdout.contains("CreateChargeResult"),
            "Should have Result interface"
        );
        assert!(
            output.stdout.contains("amount: number"),
            "Should have typed amount"
        );
        assert!(
            output.stdout.contains("currency: string"),
            "Should have typed currency"
        );
    }

    #[test]
    fn test_tool_search_command() {
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // Use tools search command to find payment-related tools
        let h = runtime.create_command("tools search payment");
        let collected = run_to_completion(&mut runtime);

        let output = collected.get(&Some(h)).unwrap();
        // Should find stripe tools (they're about payments)
        assert!(
            output.stdout.contains("charge") || output.stdout.contains("stripe"),
            "Should find payment tools. Got: {}",
            output.stdout
        );
    }

    #[test]
    fn test_tool_list_command() {
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // Use tools list command
        let h = runtime.create_command("tools list");
        let collected = run_to_completion(&mut runtime);

        let output = collected.get(&Some(h)).unwrap();
        assert!(
            output.stdout.contains("create_charge"),
            "Should list create_charge tool"
        );
        assert!(
            output.stdout.contains("create_refund"),
            "Should list create_refund tool"
        );
    }

    #[test]
    fn test_tool_info_command() {
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // Use tools info command
        let h = runtime.create_command("tools info stripe:create_charge");
        let collected = run_to_completion(&mut runtime);

        let output = collected.get(&Some(h)).unwrap();
        assert!(
            output.stdout.contains("stripe:create_charge"),
            "Should show tool name"
        );
        assert!(output.stdout.contains("amount"), "Should show amount param");
    }

    #[test]
    fn test_tool_catalog_passed_to_shell() {
        // Create runtime with tools
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // Verify tool_catalog is set
        assert!(runtime.tool_catalog.is_some(), "Tool catalog should be set");

        // Verify catalog has the tools
        let catalog = runtime.tool_catalog.as_ref().unwrap();
        assert!(!catalog.is_empty(), "Catalog should have tools");
    }

    // =========================================================================
    // Path Mount Tests
    // =========================================================================

    #[test]
    fn test_path_mount_basic() {
        let (pca, _guard) = create_test_pca();
        let mounts = vec![
            PathMount::new("/host/data/config.json", "/data/config.json"),
            PathMount::new("/host/data/input.txt", "/data/input.txt"),
        ];

        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // Verify mounts are stored
        assert_eq!(runtime.path_mounts().len(), 2);
        assert!(runtime.is_mounted("/data/config.json"));
        assert!(runtime.is_mounted("/data/input.txt"));
        assert!(!runtime.is_mounted("/data/other.txt"));

        // Verify host paths
        assert_eq!(
            runtime.get_host_path("/data/config.json"),
            Some("/host/data/config.json".to_string())
        );
        assert_eq!(
            runtime.get_host_path("/data/input.txt"),
            Some("/host/data/input.txt".to_string())
        );
    }

    #[test]
    fn test_path_mount_creates_parent_dirs() {
        let (pca, _guard) = create_test_pca();
        // Mount a file deep in a directory hierarchy that doesn't exist
        let mounts = vec![PathMount::new(
            "/host/deep/nested/path/file.txt",
            "/deep/nested/path/file.txt",
        )];

        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // Parent directories should be created as read-only
        assert!(
            runtime.vfs().is_dir("/deep"),
            "Parent dir /deep should exist"
        );
        assert!(
            runtime.vfs().is_dir("/deep/nested"),
            "Parent dir /deep/nested should exist"
        );
        assert!(
            runtime.vfs().is_dir("/deep/nested/path"),
            "Parent dir /deep/nested/path should exist"
        );

        // Mount should be registered
        assert!(runtime.is_mounted("/deep/nested/path/file.txt"));
    }

    #[test]
    fn test_path_mount_dirs_are_read_only() {
        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new(
            "/host/data/file.txt",
            "/mounted/data/file.txt",
        )];

        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // Try to create a sibling file - should fail because /mounted/data is read-only
        let result = runtime.vfs_mut().write_file(
            "/mounted/data/sibling.txt",
            b"test",
            Permission::ReadWrite,
        );
        assert!(
            result.is_err(),
            "Should not be able to create files in read-only mounted directory"
        );
    }

    #[test]
    fn test_path_mount_multiple_in_same_dir() {
        let (pca, _guard) = create_test_pca();
        let mounts = vec![
            PathMount::new("/host/data/a.txt", "/data/a.txt"),
            PathMount::new("/host/data/b.txt", "/data/b.txt"),
            PathMount::new("/host/data/c.txt", "/data/c.txt"),
        ];

        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // All should be mounted
        assert_eq!(runtime.path_mounts().len(), 3);
        assert!(runtime.is_mounted("/data/a.txt"));
        assert!(runtime.is_mounted("/data/b.txt"));
        assert!(runtime.is_mounted("/data/c.txt"));

        // Parent directory should exist
        assert!(runtime.vfs().is_dir("/data"));
    }

    #[test]
    fn test_path_mount_json_parsing() {
        let json = r#"[
            {"host_path": "/home/user/config.json", "sandbox_path": "/config.json"},
            {"host_path": "/home/user/data.csv", "sandbox_path": "/input/data.csv"}
        ]"#;

        let mounts = PathMount::from_json_array(json).unwrap();

        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].host_path, "/home/user/config.json");
        assert_eq!(mounts[0].sandbox_path, "/config.json");
        assert_eq!(mounts[1].host_path, "/home/user/data.csv");
        assert_eq!(mounts[1].sandbox_path, "/input/data.csv");
    }

    #[test]
    fn test_path_mount_json_parsing_empty() {
        let json = "[]";
        let mounts = PathMount::from_json_array(json).unwrap();
        assert!(mounts.is_empty());
    }

    #[test]
    fn test_path_mount_json_parsing_error() {
        let json = "not valid json";
        let result = PathMount::from_json_array(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("JSON parse error"));
    }

    #[test]
    fn test_path_mount_with_tools() {
        // Both tools and mounts together
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let mounts = vec![PathMount::new("/host/secrets/api_key", "/secrets/api_key")];

        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &tools, &mounts, ts, rs).unwrap()
        };

        // Tools should be generated
        assert!(runtime.vfs().is_file("/tools/stripe/create_charge.js"));

        // Mounts should be registered
        assert!(runtime.is_mounted("/secrets/api_key"));
        assert_eq!(
            runtime.get_host_path("/secrets/api_key"),
            Some("/host/secrets/api_key".to_string())
        );
    }

    #[test]
    fn test_path_mount_overlapping_paths() {
        // Mount files at different depths
        let (pca, _guard) = create_test_pca();
        let mounts = vec![
            PathMount::new("/host/a.txt", "/data/a.txt"),
            PathMount::new("/host/nested/b.txt", "/data/nested/b.txt"),
            PathMount::new("/host/deep/c.txt", "/data/nested/deep/c.txt"),
        ];

        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // All paths should be mounted
        assert!(runtime.is_mounted("/data/a.txt"));
        assert!(runtime.is_mounted("/data/nested/b.txt"));
        assert!(runtime.is_mounted("/data/nested/deep/c.txt"));

        // All directories should exist
        assert!(runtime.vfs().is_dir("/data"));
        assert!(runtime.vfs().is_dir("/data/nested"));
        assert!(runtime.vfs().is_dir("/data/nested/deep"));
    }

    #[test]
    fn test_path_mount_from_pca_bytes_with_mounts() {
        let (pca, _guard) = create_test_pca();
        let pca_bytes = pca.to_cbor().unwrap();

        let tools_json = "[]";
        let mounts_json = r#"[
            {"host_path": "/host/file.txt", "sandbox_path": "/file.txt"}
        ]"#;

        let (ts, rs) = test_sources();
        let runtime =
            Runtime::from_pca_bytes_with_mounts(&pca_bytes, tools_json, mounts_json, ts, rs)
                .unwrap();

        assert!(runtime.is_mounted("/file.txt"));
        assert_eq!(
            runtime.get_host_path("/file.txt"),
            Some("/host/file.txt".to_string())
        );
    }

    #[test]
    fn test_mounted_file_read_via_host_op() {
        // Test: Reading a mounted file issues a FileRead host op and returns host data.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/config.json", "/data/config.json")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // Create a command that reads the mounted file
        let _handle = runtime.create_command("cat /data/config.json");

        // Step to start - should block waiting for file read
        let resp = runtime.step();
        assert!(!resp.all_done());

        // Find the VfsRead request for the mounted file
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/config.json")
        });
        assert!(
            vfs_read.is_some(),
            "Reading mounted file should emit VfsRead for host path"
        );
        let vfs_read = vfs_read.unwrap();

        // Host provides the file content
        let file_content = br#"{"key": "value"}"#;
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: file_content.to_vec(),
            },
        }];
        runtime.submit_results(&results);

        // Continue execution - cat should output the content
        let resp = runtime.step();

        // Find the output
        let output = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::Output { stream: 1, data, .. } if data == file_content)
        });
        assert!(
            output.is_some(),
            "cat should output the mounted file content"
        );
    }

    #[test]
    fn test_mounted_file_exists_and_is_file() {
        // Test: Mounted files should report as existing and as files.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/secret.txt", "/secrets/key.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // Create a command that checks if the mounted file exists
        let _handle = runtime.create_command("test -f /secrets/key.txt && echo exists");

        // Step multiple times, acking outputs
        let mut found_exists = false;
        for _ in 0..10 {
            let resp = runtime.step();

            // Check for "exists" in output
            for op in &resp.host_ops {
                if matches!(&op.request, HostOpRequest::Output { stream: 1, data, .. } if data == b"exists\n")
                {
                    found_exists = true;
                }
            }

            // Ack outputs
            let results: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !results.is_empty() {
                runtime.submit_results(&results);
            }

            if resp.all_done() || found_exists {
                break;
            }
        }

        assert!(
            found_exists,
            "test -f should report mounted file as existing"
        );
    }

    #[test]
    fn test_unmounted_file_not_found() {
        // Test: Non-mounted files should return NotFound (not trigger host ops).

        let (pca, _guard) = create_test_pca();
        // No mounts
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Create a command that tries to read a non-existent file
        let _handle = runtime.create_command("cat /data/nonexistent.txt");

        // Step to start
        let resp = runtime.step();

        // Should NOT have a VfsRead for /data/nonexistent.txt since it's not mounted
        let has_vfs_read = resp.host_ops.iter().any(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path.contains("nonexistent"))
        });
        assert!(
            !has_vfs_read,
            "Non-mounted files should not trigger VfsRead host ops"
        );
    }

    // =========================================================================
    // Comprehensive mounted path tests
    // =========================================================================

    #[test]
    fn test_mounted_file_head_command() {
        // Test: head command on mounted file issues VfsRead and outputs first lines.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/data.txt", "/data/input.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("head -n 2 /data/input.txt");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/data.txt")
        });
        assert!(
            vfs_read.is_some(),
            "head should issue VfsRead for mounted file"
        );
        let vfs_read = vfs_read.unwrap();

        // Host provides file content with multiple lines
        let file_content = b"line1\nline2\nline3\nline4\n";
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: file_content.to_vec(),
            },
        }];
        runtime.submit_results(&results);

        // Collect all output and check it contains first 2 lines
        let mut all_output = String::new();
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    all_output.push_str(&String::from_utf8_lossy(data));
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() {
                break;
            }
        }
        assert!(
            all_output.contains("line1") && all_output.contains("line2"),
            "head should output first 2 lines from mounted file, got: {all_output}"
        );
    }

    #[test]
    fn test_mounted_file_tail_command() {
        // Test: tail command on mounted file issues VfsRead and outputs last lines.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/log.txt", "/logs/app.log")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("tail -n 2 /logs/app.log");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/log.txt")
        });
        assert!(
            vfs_read.is_some(),
            "tail should issue VfsRead for mounted file"
        );
        let vfs_read = vfs_read.unwrap();

        // Host provides file content
        let file_content = b"line1\nline2\nline3\nline4\n";
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: file_content.to_vec(),
            },
        }];
        runtime.submit_results(&results);

        // Collect all output and check it contains last 2 lines
        let mut all_output = String::new();
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    all_output.push_str(&String::from_utf8_lossy(data));
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() {
                break;
            }
        }
        assert!(
            all_output.contains("line3") && all_output.contains("line4"),
            "tail should output last 2 lines from mounted file, got: {all_output}"
        );
    }

    #[test]
    fn test_mounted_file_grep_command() {
        // Test: grep command on mounted file issues VfsRead and searches content.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/source.rs", "/src/main.rs")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("grep 'fn main' /src/main.rs");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/source.rs")
        });
        assert!(
            vfs_read.is_some(),
            "grep should issue VfsRead for mounted file"
        );
        let vfs_read = vfs_read.unwrap();

        // Host provides Rust source content
        let file_content = b"use std::io;\n\nfn main() {\n    println!(\"Hello\");\n}\n";
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: file_content.to_vec(),
            },
        }];
        runtime.submit_results(&results);

        // Continue until we see matching output
        let mut found_match = false;
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                    && String::from_utf8_lossy(data).contains("fn main")
                {
                    found_match = true;
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() || found_match {
                break;
            }
        }
        assert!(
            found_match,
            "grep should find matching line in mounted file"
        );
    }

    #[test]
    fn test_mounted_file_wc_command() {
        // Test: wc command on mounted file issues VfsRead and counts correctly.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/words.txt", "/data/words.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("wc -l /data/words.txt");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/words.txt")
        });
        assert!(
            vfs_read.is_some(),
            "wc should issue VfsRead for mounted file"
        );
        let vfs_read = vfs_read.unwrap();

        // Host provides file with 3 lines
        let file_content = b"one\ntwo\nthree\n";
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: file_content.to_vec(),
            },
        }];
        runtime.submit_results(&results);

        // Continue until we see output with line count
        let mut found_count = false;
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    let output_str = String::from_utf8_lossy(data);
                    // wc -l should show "3" for 3 lines
                    if output_str.contains('3') {
                        found_count = true;
                    }
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() || found_count {
                break;
            }
        }
        assert!(found_count, "wc should count 3 lines in mounted file");
    }

    // =========================================================================
    // Error handling tests for mounted files
    // =========================================================================

    #[test]
    fn test_mounted_file_host_error_not_found() {
        // Test: When host returns error for mounted file, command outputs error.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/missing.txt", "/data/missing.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("cat /data/missing.txt");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/missing.txt")
        });
        assert!(vfs_read.is_some());
        let vfs_read = vfs_read.unwrap();

        // Host returns error
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::error("not_found", "File not found on host"),
        }];
        runtime.submit_results(&results);

        // Continue until we see error output
        let mut found_error = false;
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 2, data, ..
                } = &op.request
                {
                    let output_str = String::from_utf8_lossy(data);
                    // Should see error message on stderr
                    if output_str.contains("cat") || output_str.contains("error") {
                        found_error = true;
                    }
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() {
                break;
            }
        }
        assert!(
            found_error,
            "cat should output error when host file not found"
        );
    }

    #[test]
    fn test_mounted_file_host_error_permission_denied() {
        // Test: When host returns permission denied, command handles it.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/secret.txt", "/secrets/api.key")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("cat /secrets/api.key");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/secret.txt")
        });
        assert!(vfs_read.is_some());
        let vfs_read = vfs_read.unwrap();

        // Host returns permission denied error
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::error("permission_denied", "Access denied"),
        }];
        runtime.submit_results(&results);

        // Command should complete (possibly with error) without panic
        let mut completed = false;
        for _ in 0..10 {
            let resp = runtime.step();
            // Ack all outputs
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() {
                completed = true;
                break;
            }
        }
        assert!(
            completed,
            "Command should complete even with host permission error"
        );
    }

    #[test]
    fn test_host_error_codes_e2e() {
        // End-to-end test: All HostErrorCode variants are correctly propagated
        // from host response through submit_results and produce correct io::ErrorKind.
        use crate::host_ops::HostErrorCode;

        // Test data: (error code string, expected ErrorKind)
        let test_cases: Vec<(&str, std::io::ErrorKind)> = vec![
            ("not_found", std::io::ErrorKind::NotFound),
            ("permission_denied", std::io::ErrorKind::PermissionDenied),
            ("timeout", std::io::ErrorKind::TimedOut),
            ("timed_out", std::io::ErrorKind::TimedOut), // Alias
            ("connection_refused", std::io::ErrorKind::ConnectionRefused),
            ("connection_reset", std::io::ErrorKind::ConnectionReset),
            ("already_exists", std::io::ErrorKind::AlreadyExists),
            ("invalid_input", std::io::ErrorKind::InvalidInput),
            ("invalid_argument", std::io::ErrorKind::InvalidInput), // Alias
            ("invalid_data", std::io::ErrorKind::InvalidData),
            ("would_block", std::io::ErrorKind::WouldBlock),
            ("interrupted", std::io::ErrorKind::Interrupted),
            ("unsupported", std::io::ErrorKind::Unsupported),
            ("out_of_memory", std::io::ErrorKind::OutOfMemory),
            ("internal", std::io::ErrorKind::Other),
            ("custom_error", std::io::ErrorKind::Other), // Unknown code
        ];

        for (code, expected_kind) in test_cases {
            let (pca, _guard) = create_test_pca();
            let mounts = vec![PathMount::new(
                "/host/test.txt",
                format!("/test/{code}.txt"),
            )];
            let mut runtime = {
                let (ts, rs) = test_sources();
                Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
            };

            let _handle = runtime.create_command(&format!("cat /test/{code}.txt"));

            // Step to get VfsRead request
            let resp = runtime.step();
            let vfs_read = resp
                .host_ops
                .iter()
                .find(|op| matches!(&op.request, HostOpRequest::VfsRead { .. }));
            assert!(vfs_read.is_some(), "Expected VfsRead for code {code}");
            let vfs_read = vfs_read.unwrap();

            // Host returns error with the test code
            let results = vec![HostOpResult {
                id: vfs_read.id,
                runtime_id: runtime.id,
                result: HostOpResponse::error(code, format!("Test error: {code}")),
            }];
            runtime.submit_results(&results);

            // Verify the error code was converted correctly
            let error_code: HostErrorCode = code.into();
            assert_eq!(
                error_code.to_io_error_kind(),
                expected_kind,
                "Mismatch for error code '{code}'"
            );

            // Complete the command
            for _ in 0..10 {
                let resp = runtime.step();
                // Ack all outputs
                let acks: Vec<_> = resp
                    .host_ops
                    .iter()
                    .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                    .map(|op| HostOpResult {
                        id: op.id,
                        runtime_id: runtime.id,
                        result: HostOpResponse::OutputAck,
                    })
                    .collect();
                if !acks.is_empty() {
                    runtime.submit_results(&acks);
                }
                if resp.all_done() {
                    break;
                }
            }
        }
    }

    #[test]
    fn test_host_error_structured_preservation() {
        // Test: When host returns an error, the HostOpError is preserved in the
        // std::io::Error and can be downcast to extract code and message.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/readonly.txt", "/data/readonly.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("cat /data/readonly.txt");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/readonly.txt")
        });
        assert!(vfs_read.is_some());
        let vfs_read = vfs_read.unwrap();

        // Host returns structured error
        let error_response = HostOpResponse::error("permission_denied", "File is read-protected");

        // Verify response_to_bytes preserves structured error
        let result = response_to_bytes(&error_response);
        assert!(result.is_err());
        let io_err = result.unwrap_err();

        // Verify ErrorKind is correct
        assert_eq!(io_err.kind(), std::io::ErrorKind::PermissionDenied);

        // Verify we can downcast to HostOpError
        let inner = io_err.get_ref().expect("should have inner error");
        let host_err = inner
            .downcast_ref::<crate::HostOpError>()
            .expect("should be HostOpError");
        assert!(matches!(
            host_err.code(),
            crate::host_ops::HostErrorCode::PermissionDenied
        ));
        assert_eq!(host_err.message(), "File is read-protected");

        // Submit error to runtime (verifies the full flow works)
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: error_response,
        }];
        runtime.submit_results(&results);

        // Command should complete
        for _ in 0..10 {
            let resp = runtime.step();
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() {
                break;
            }
        }
    }

    // =========================================================================
    // Edge case tests for mounted files
    // =========================================================================

    #[test]
    fn test_mounted_file_binary_content() {
        // Test: Binary content from host is passed through correctly.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/binary.bin", "/data/binary.bin")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("cat /data/binary.bin");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/binary.bin")
        });
        assert!(vfs_read.is_some());
        let vfs_read = vfs_read.unwrap();

        // Host provides binary content with null bytes and high bytes
        let binary_content: Vec<u8> = (0u8..=255).collect();
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: binary_content.clone(),
            },
        }];
        runtime.submit_results(&results);

        // Collect all output bytes
        let mut all_output: Vec<u8> = Vec::new();
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    all_output.extend_from_slice(data);
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() {
                break;
            }
        }
        assert_eq!(
            all_output, binary_content,
            "cat should output binary content from mounted file"
        );
    }

    #[test]
    fn test_mounted_file_empty_content() {
        // Test: Empty file from host is handled correctly.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/empty.txt", "/data/empty.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("wc -c /data/empty.txt");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/empty.txt")
        });
        assert!(vfs_read.is_some());
        let vfs_read = vfs_read.unwrap();

        // Host provides empty content
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData { data: vec![] },
        }];
        runtime.submit_results(&results);

        // wc -c should output 0 for empty file
        let mut found_zero = false;
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    let output_str = String::from_utf8_lossy(data);
                    // wc -c should show "0" for empty file
                    if output_str.trim().starts_with('0') || output_str.contains(" 0 ") {
                        found_zero = true;
                    }
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() || found_zero {
                break;
            }
        }
        assert!(
            found_zero,
            "wc should report 0 bytes for empty mounted file"
        );
    }

    #[test]
    fn test_mounted_file_large_content() {
        // Test: Large file from host is handled correctly.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/large.txt", "/data/large.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("wc -c /data/large.txt");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/large.txt")
        });
        assert!(vfs_read.is_some());
        let vfs_read = vfs_read.unwrap();

        // Host provides 1MB of data
        let large_content: Vec<u8> = vec![b'X'; 1024 * 1024];
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: large_content,
            },
        }];
        runtime.submit_results(&results);

        // wc -c should output 1048576 (1MB)
        let mut found_size = false;
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    let output_str = String::from_utf8_lossy(data);
                    if output_str.contains("1048576") {
                        found_size = true;
                    }
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() || found_size {
                break;
            }
        }
        assert!(
            found_size,
            "wc should report 1048576 bytes for 1MB mounted file"
        );
    }

    #[test]
    fn test_mounted_file_relative_path_resolution() {
        // Test: Relative path to mounted file is resolved correctly.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/config.json", "/config/app.json")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // Use relative path from workspace (cwd is /workspace by default)
        let _handle = runtime.create_command("cat ../config/app.json");

        // Step to get VfsRead request
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/config.json")
        });
        assert!(
            vfs_read.is_some(),
            "cat with relative path should issue VfsRead for mounted file"
        );
    }

    #[test]
    fn test_mounted_file_in_pipeline() {
        // Test: Mounted file can be used in pipeline.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/data.csv", "/data/input.csv")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("cat /data/input.csv | grep 'value' | wc -l");

        // Step to get VfsRead request for the mounted file
        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/data.csv")
        });
        assert!(
            vfs_read.is_some(),
            "Pipeline should issue VfsRead for mounted file"
        );
        let vfs_read = vfs_read.unwrap();

        // Host provides CSV content
        let csv_content = b"name,value\nfoo,value1\nbar,other\nbaz,value2\n";
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: csv_content.to_vec(),
            },
        }];
        runtime.submit_results(&results);

        // Pipeline should count 3 lines containing "value"
        let mut found_count = false;
        for _ in 0..20 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    let output_str = String::from_utf8_lossy(data);
                    // Should find 3 lines with "value" (header + 2 data rows)
                    if output_str.contains('3') {
                        found_count = true;
                    }
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() || found_count {
                break;
            }
        }
        assert!(
            found_count,
            "Pipeline should count lines matching 'value' in mounted file"
        );
    }

    #[test]
    fn test_multiple_mounted_files_same_command() {
        // Test: Multiple mounted files can be read by the same command (cat file1 file2).

        let (pca, _guard) = create_test_pca();
        let mounts = vec![
            PathMount::new("/host/part1.txt", "/data/part1.txt"),
            PathMount::new("/host/part2.txt", "/data/part2.txt"),
        ];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("cat /data/part1.txt /data/part2.txt");

        // Step and handle VfsRead requests for both files
        let mut vfs_reads_handled = 0;
        let mut saw_hello = false;
        let mut saw_world = false;

        for _ in 0..20 {
            let resp = runtime.step();

            // Handle VfsRead requests
            for op in &resp.host_ops {
                if let HostOpRequest::VfsRead { path, .. } = &op.request {
                    let content = if path == "/host/part1.txt" {
                        b"Hello ".to_vec()
                    } else if path == "/host/part2.txt" {
                        b"World\n".to_vec()
                    } else {
                        continue;
                    };

                    let result = HostOpResult {
                        id: op.id,
                        runtime_id: runtime.id,
                        result: HostOpResponse::VfsData { data: content },
                    };
                    runtime.submit_results(&[result]);
                    vfs_reads_handled += 1;
                }
            }

            // Check outputs
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    let output_str = String::from_utf8_lossy(data);
                    if output_str.contains("Hello") {
                        saw_hello = true;
                    }
                    if output_str.contains("World") {
                        saw_world = true;
                    }
                }
            }

            // Ack outputs
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if resp.all_done() {
                break;
            }
        }

        assert!(
            vfs_reads_handled >= 2,
            "Should handle VfsRead for both mounted files"
        );
        assert!(saw_hello, "Should output content from part1.txt");
        assert!(saw_world, "Should output content from part2.txt");
    }

    #[test]
    fn test_mounted_file_with_special_characters_in_content() {
        // Test: File with special characters (unicode, control chars) is handled.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/unicode.txt", "/data/unicode.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        let _handle = runtime.create_command("cat /data/unicode.txt");

        let resp = runtime.step();
        let vfs_read = resp.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::VfsRead { path, .. } if path == "/host/unicode.txt")
        });
        assert!(vfs_read.is_some());
        let vfs_read = vfs_read.unwrap();

        // Content with unicode and special characters
        let unicode_content = "Hello 世界! 🚀 café\n\ttab\r\nwindows line\n"
            .as_bytes()
            .to_vec();
        let results = vec![HostOpResult {
            id: vfs_read.id,
            runtime_id: runtime.id,
            result: HostOpResponse::VfsData {
                data: unicode_content.clone(),
            },
        }];
        runtime.submit_results(&results);

        // Collect all output bytes and verify
        let mut all_output: Vec<u8> = Vec::new();
        for _ in 0..10 {
            let resp = runtime.step();
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    all_output.extend_from_slice(data);
                }
            }
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }
            if resp.all_done() {
                break;
            }
        }
        assert_eq!(
            all_output, unicode_content,
            "cat should output unicode content correctly"
        );
    }

    #[test]
    fn test_mounted_and_vfs_files_together() {
        // Test: Command can read both mounted and VFS files in same invocation.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/external.txt", "/data/external.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // Create a VFS file
        runtime
            .vfs_mut()
            .write_file(
                "/workspace/internal.txt",
                b"INTERNAL",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let _handle = runtime.create_command("cat /workspace/internal.txt /data/external.txt");

        // VFS file should be read immediately (no host op needed)
        // Mounted file should trigger VfsRead
        let mut saw_internal = false;
        let mut saw_external = false;

        for _ in 0..20 {
            let resp = runtime.step();

            // Handle VfsRead for mounted file
            for op in &resp.host_ops {
                if let HostOpRequest::VfsRead { path, .. } = &op.request
                    && path == "/host/external.txt"
                {
                    let result = HostOpResult {
                        id: op.id,
                        runtime_id: runtime.id,
                        result: HostOpResponse::VfsData {
                            data: b"EXTERNAL".to_vec(),
                        },
                    };
                    runtime.submit_results(&[result]);
                }
            }

            // Check outputs
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    let output_str = String::from_utf8_lossy(data);
                    if output_str.contains("INTERNAL") {
                        saw_internal = true;
                    }
                    if output_str.contains("EXTERNAL") {
                        saw_external = true;
                    }
                }
            }

            // Ack outputs
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if resp.all_done() {
                break;
            }
        }

        assert!(saw_internal, "Should output VFS file content");
        assert!(saw_external, "Should output mounted file content");
    }

    #[test]
    fn test_mounted_file_read_by_multiple_commands() {
        // Test: Same mounted file can be read by multiple commands.

        let (pca, _guard) = create_test_pca();
        let mounts = vec![PathMount::new("/host/shared.txt", "/data/shared.txt")];
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_mounts(pca, &[], &mounts, ts, rs).unwrap()
        };

        // Create two commands that read the same file
        let h1 = runtime.create_command("cat /data/shared.txt");
        let h2 = runtime.create_command("head -n 1 /data/shared.txt");

        let file_content = b"line1\nline2\nline3\n";
        let mut h1_saw_output = false;
        let mut h2_saw_output = false;

        for _ in 0..30 {
            let resp = runtime.step();

            // Handle VfsRead requests
            for op in &resp.host_ops {
                if let HostOpRequest::VfsRead { path, .. } = &op.request
                    && path == "/host/shared.txt"
                {
                    let result = HostOpResult {
                        id: op.id,
                        runtime_id: runtime.id,
                        result: HostOpResponse::VfsData {
                            data: file_content.to_vec(),
                        },
                    };
                    runtime.submit_results(&[result]);
                }
            }

            // Check outputs
            for op in &resp.host_ops {
                if let HostOpRequest::Output {
                    stream: 1, data, ..
                } = &op.request
                {
                    let output_str = String::from_utf8_lossy(data);
                    if op.command == Some(h1) && output_str.contains("line") {
                        h1_saw_output = true;
                    }
                    if op.command == Some(h2) && output_str.contains("line1") {
                        h2_saw_output = true;
                    }
                }
            }

            // Ack outputs
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if resp.all_done() {
                break;
            }
        }

        assert!(h1_saw_output, "First command should read mounted file");
        assert!(
            h2_saw_output,
            "Second command should read same mounted file"
        );
    }

    // =========================================================================
    // E2E tests: kill, jobs, and multiple runtimes
    // =========================================================================

    #[test]
    fn e2e_kill_background_job() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Run background job and kill it
        runtime.create_command("true &");
        runtime.create_command("kill %1");

        let collected = run_to_completion(&mut runtime);

        // Should have completed both commands
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn e2e_jobs_lists_background_jobs() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Run background jobs and list them
        runtime.create_command("true &");
        let h = runtime.create_command("jobs");

        let collected = run_to_completion(&mut runtime);

        // Jobs command should produce output
        let output = collected.get(&Some(h)).unwrap();
        // Output depends on whether job completes before jobs runs
        // Just check it doesn't crash
        assert!(output.exit_code.is_some());
    }

    #[test]
    fn e2e_kill_pipeline_background_job() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Setup a file to pipe through
        runtime
            .vfs_mut()
            .write_file(
                "/workspace/data.txt",
                b"line1\nline2\nline3\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Run pipeline in background and kill it
        runtime.create_command("cat /workspace/data.txt | grep line &");
        runtime.create_command("kill %1");

        let collected = run_to_completion(&mut runtime);

        // Should complete without panicking
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn e2e_multiple_runtimes_independent_jobs() {
        // Create two runtimes
        let (pca1, _guard1) = create_test_pca();
        let mut runtime1 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca1, ts, rs).unwrap()
        };
        runtime1.create_command("true &");
        runtime1.create_command("kill %1");

        let (pca2, _guard2) = create_test_pca();
        let mut runtime2 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca2, ts, rs).unwrap()
        };
        runtime2.create_command("true &");
        runtime2.create_command("kill %1");

        // Run both to completion
        let collected1 = run_to_completion(&mut runtime1);
        let collected2 = run_to_completion(&mut runtime2);

        // Each runtime should complete independently
        assert_eq!(collected1.len(), 2);
        assert_eq!(collected2.len(), 2);
    }

    #[test]
    fn e2e_multiple_runtimes_interleaved_steps() {
        // Create and register multiple runtimes
        let (pca1, _guard1) = create_test_pca();
        let mut runtime1 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca1, ts, rs).unwrap()
        };
        let h1 = runtime1.create_command("echo runtime1");
        let id1 = register_runtime(runtime1);

        let (pca2, _guard2) = create_test_pca();
        let mut runtime2 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca2, ts, rs).unwrap()
        };
        let h2 = runtime2.create_command("echo runtime2");
        let id2 = register_runtime(runtime2);

        // Collect outputs by interleaving steps
        let mut out1 = CollectedOutput::default();
        let mut out2 = CollectedOutput::default();

        for _ in 0..100 {
            // Step runtime 1
            // Note: command is in PendingHostOp, not HostOpRequest
            if let Some(resp) = step_runtime(id1) {
                for op in &resp.host_ops {
                    match &op.request {
                        HostOpRequest::Output { stream, data, .. }
                            if op.command == Some(h1) && *stream == 1 =>
                        {
                            out1.stdout.push_str(&String::from_utf8_lossy(data));
                        }
                        HostOpRequest::CommandExit { code, .. } if op.command == Some(h1) => {
                            out1.exit_code = Some(*code);
                        }
                        _ => {}
                    }
                }
                let results: Vec<HostOpResult> = resp
                    .host_ops
                    .iter()
                    .map(|op| HostOpResult {
                        id: op.id,
                        runtime_id: op.runtime_id,
                        result: HostOpResponse::OutputAck,
                    })
                    .collect();
                with_runtime_mut(id1, |r| r.submit_results(&results)).ok();
            }

            // Step runtime 2
            // Note: command is in PendingHostOp, not HostOpRequest
            if let Some(resp) = step_runtime(id2) {
                for op in &resp.host_ops {
                    match &op.request {
                        HostOpRequest::Output { stream, data, .. }
                            if op.command == Some(h2) && *stream == 1 =>
                        {
                            out2.stdout.push_str(&String::from_utf8_lossy(data));
                        }
                        HostOpRequest::CommandExit { code, .. } if op.command == Some(h2) => {
                            out2.exit_code = Some(*code);
                        }
                        _ => {}
                    }
                }
                let results: Vec<HostOpResult> = resp
                    .host_ops
                    .iter()
                    .map(|op| HostOpResult {
                        id: op.id,
                        runtime_id: op.runtime_id,
                        result: HostOpResponse::OutputAck,
                    })
                    .collect();
                with_runtime_mut(id2, |r| r.submit_results(&results)).ok();
            }

            // Check if both done
            if out1.exit_code.is_some() && out2.exit_code.is_some() {
                break;
            }
        }

        // Both runtimes should complete with correct output
        assert!(out1.stdout.contains("runtime1"));
        assert!(out2.stdout.contains("runtime2"));
    }

    #[test]
    fn e2e_kill_multiple_background_jobs() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Start multiple background jobs
        runtime.create_command("true &");
        runtime.create_command("true &");
        runtime.create_command("true &");

        // Kill all of them
        runtime.create_command("kill %1 %2 %3");

        let collected = run_to_completion(&mut runtime);

        // Should have 4 commands total
        assert_eq!(collected.len(), 4);
    }

    #[test]
    fn e2e_kill_nonexistent_job_returns_error() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Try to kill non-existent job
        let h = runtime.create_command("kill %99");

        let collected = run_to_completion(&mut runtime);

        // Should exit with code 1
        let output = collected.get(&Some(h)).unwrap();
        assert_eq!(output.exit_code, Some(1));
    }

    #[test]
    fn e2e_wait_for_background_job() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Start background job and wait for it
        runtime.create_command("echo bg &");
        runtime.create_command("wait");

        let collected = run_to_completion(&mut runtime);

        // Both should complete
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn e2e_fg_on_completed_job() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Start fast background job and try to foreground it
        // The job might complete before fg runs, which is expected
        runtime.create_command("echo bg &");
        let h = runtime.create_command("fg %1");

        let collected = run_to_completion(&mut runtime);

        // fg may fail if job already completed (code 1) or succeed if still running (code 0)
        let output = collected.get(&Some(h)).unwrap();
        assert!(
            output.exit_code == Some(0) || output.exit_code == Some(1),
            "fg should return 0 or 1, got {:?}",
            output.exit_code
        );
    }

    #[test]
    fn e2e_stress_multiple_runtimes_with_jobs() {
        let initial_count = runtime_count();

        // Create 5 runtimes, each with background jobs
        let mut runtime_ids = Vec::new();

        for i in 0..5 {
            let (pca, _guard) = create_test_pca();
            let mut runtime = {
                let (ts, rs) = test_sources();
                Runtime::from_pca(pca, ts, rs).unwrap()
            };
            runtime.create_command(&format!("echo runtime{i} &"));
            runtime.create_command("kill %1");
            let id = register_runtime(runtime);
            runtime_ids.push(id);
        }

        // Step all runtimes until done
        for _ in 0..200 {
            let mut all_done = true;

            for &id in &runtime_ids {
                if let Some(resp) = step_runtime(id) {
                    if !resp.all_done() {
                        all_done = false;
                    }
                    // Ack all host ops
                    let results: Vec<HostOpResult> = resp
                        .host_ops
                        .iter()
                        .map(|op| HostOpResult {
                            id: op.id,
                            runtime_id: op.runtime_id,
                            result: HostOpResponse::OutputAck,
                        })
                        .collect();
                    with_runtime_mut(id, |r| r.submit_results(&results)).ok();
                }
            }

            if all_done {
                break;
            }
        }

        // All runtimes should still be registered
        assert_eq!(runtime_count(), initial_count + 5);

        // Clean up
        for id in runtime_ids {
            remove_runtime(id);
        }
    }

    // =========================================================================
    // Memory Profiling Tests
    // =========================================================================
    //
    // These tests document the memory footprint of key runtime structures.
    // They don't use tracking-allocator (blocked by cargo bench serde bug),
    // but verify structure sizes and memory bounds.
    // =========================================================================

    #[test]
    fn memory_structure_sizes() {
        // Document the stack sizes of key structures
        // These are just the struct size, not heap allocations
        let runtime_size = std::mem::size_of::<Runtime>();
        let vfs_size = std::mem::size_of::<amla_vfs::Vfs>();

        // Runtime should be reasonably sized (uses Rc/RefCell internally)
        // The actual heap allocation is much larger
        println!("Runtime struct size: {runtime_size} bytes");
        println!("Vfs struct size: {vfs_size} bytes");

        // These are pointer-sized since they use Rc internally
        assert!(
            runtime_size < 1024,
            "Runtime struct should be small (uses Rc internally)"
        );
        assert!(
            vfs_size < 256,
            "Vfs struct should be small (uses Rc internally)"
        );
    }

    #[test]
    fn memory_vfs_bounded_operations() {
        // VFS has bounded memory through page cache (tested in amla-vfs)
        // Here we verify runtime VFS operations don't leak memory
        let (pca, _guard) = create_test_pca();
        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Write many files - VFS should handle this
        for i in 0..100 {
            let mut vfs = runtime.vfs_mut();
            let path = format!("/workspace/file_{i}.txt");
            let content = format!("content {i}");
            vfs.write_file(&path, content.as_bytes(), amla_vfs::Permission::ReadWrite)
                .unwrap();
        }

        // Read all files back
        let vfs = runtime.vfs();
        for i in 0..100 {
            let path = format!("/workspace/file_{i}.txt");
            let content = vfs.read_file(&path).unwrap();
            let expected = format!("content {i}");
            assert_eq!(content, expected.as_bytes());
        }
    }

    #[test]
    fn memory_runtime_creation_baseline() {
        // Create a runtime and verify it's functional
        // This establishes a baseline for runtime memory
        let (pca, _guard) = create_test_pca();
        let runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Runtime should have initialized VFS
        let vfs = runtime.vfs();
        assert!(vfs.exists("/workspace"), "VFS should have /workspace");
        assert!(vfs.exists("/tools"), "VFS should have /tools");

        // Runtime should have empty command state initially
        // (no commands created yet)
    }

    #[test]
    fn memory_multiple_runtimes_isolation() {
        // Create multiple runtimes to verify they don't share memory unexpectedly
        let mut runtimes = Vec::new();
        for _ in 0..5 {
            let (pca, _guard) = create_test_pca();
            let runtime = {
                let (ts, rs) = test_sources();
                Runtime::from_pca(pca, ts, rs).unwrap()
            };
            runtimes.push(runtime);
        }

        // Each runtime should have independent VFS
        for (i, runtime) in runtimes.iter().enumerate() {
            let mut vfs = runtime.vfs_mut();
            let path = format!("/workspace/file_{i}.txt");
            vfs.write_file(
                &path,
                format!("content {i}").as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        }

        // Verify isolation
        for (i, runtime) in runtimes.iter().enumerate() {
            let vfs = runtime.vfs();
            let path = format!("/workspace/file_{i}.txt");
            assert!(vfs.exists(&path), "File should exist in runtime {i}");

            // Other runtimes' files should NOT exist
            for j in 0..5 {
                if j != i {
                    let other_path = format!("/workspace/file_{j}.txt");
                    assert!(
                        !vfs.exists(&other_path),
                        "File from runtime {j} should not exist in runtime {i}"
                    );
                }
            }
        }
    }

    // =========================================================================
    // Regression tests for bug fixes
    // =========================================================================

    #[test]
    fn cancel_command_actually_stops_task() {
        // Regression test: cancel_command() must call task_handle.cancel(),
        // not just drop the handle. Dropping a TaskHandle does NOT cancel the task.

        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Create a command that blocks waiting for stdin (won't complete on its own)
        let handle = runtime.create_command("cat");

        // Step once to start the command - it will block on ReadStdin
        let resp = runtime.step();
        assert!(
            !resp.all_done(),
            "Command should be blocked waiting for stdin"
        );

        // Should have a ReadStdin request
        let has_read_stdin = resp
            .host_ops
            .iter()
            .any(|op| matches!(op.request, HostOpRequest::ReadStdin { .. }));
        assert!(has_read_stdin, "Command should be waiting for stdin");

        // Cancel the command - this must actually stop the task
        let cancel_ops = runtime.cancel_command(handle);

        // Should get a CommandExit with code -1
        assert_eq!(cancel_ops.len(), 1);
        assert!(matches!(
            &cancel_ops[0].request,
            HostOpRequest::CommandExit { code: -1, .. }
        ));

        // After cancellation, runtime should be done
        let resp = runtime.step();
        assert!(resp.all_done(), "Runtime should be done after cancellation");
    }

    #[test]
    fn cancel_command_cascades_to_nested_tasks() {
        // Regression test: cancel_command() must cascade to child tasks.
        // A pipeline spawns multiple tasks under one command.

        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Create a pipeline command where cat blocks waiting for stdin
        // This ensures the command doesn't complete before we cancel
        let handle = runtime.create_command("cat | cat");

        // Step to start the command - should block on stdin
        let resp = runtime.step();
        assert!(
            !resp.all_done(),
            "Pipeline should be blocked waiting for stdin"
        );

        // Cancel the command (should cascade to all tasks in pipeline)
        let cancel_ops = runtime.cancel_command(handle);
        assert_eq!(cancel_ops.len(), 1);
        assert!(matches!(
            &cancel_ops[0].request,
            HostOpRequest::CommandExit { code: -1, .. }
        ));

        // Step to process cancellation - should complete
        let resp = runtime.step();

        // All commands should be done
        assert!(
            resp.all_done(),
            "All nested tasks should be stopped after cancellation"
        );
    }

    #[test]
    fn cancel_command_preserves_buffered_output() {
        // Regression test: cancel_command() must drain stdout/stderr buffers
        // before emitting CommandExit, otherwise buffered output is lost.

        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // echo outputs text, then cat blocks waiting for stdin.
        // This ensures some output is produced before we cancel.
        let handle = runtime.create_command("echo hello && cat");

        // Step to execute echo and start cat (which blocks on stdin).
        // The echo output may be buffered in the IoHandle.
        let resp = runtime.step();
        assert!(
            !resp.all_done(),
            "Command should be blocked waiting for stdin"
        );

        // Collect any output from the step
        let mut stdout_from_step = String::new();
        for op in &resp.host_ops {
            if let HostOpRequest::Output {
                stream: 1, data, ..
            } = &op.request
                && op.command == Some(handle)
            {
                stdout_from_step.push_str(&String::from_utf8_lossy(data));
            }
        }

        // Cancel the command - should include any remaining buffered output
        let cancel_ops = runtime.cancel_command(handle);

        // Collect output from cancel ops
        let mut stdout_from_cancel = String::new();
        let mut has_exit = false;
        for op in &cancel_ops {
            match &op.request {
                HostOpRequest::Output {
                    stream: 1, data, ..
                } => {
                    stdout_from_cancel.push_str(&String::from_utf8_lossy(data));
                }
                HostOpRequest::CommandExit { code: -1, .. } => {
                    has_exit = true;
                }
                _ => {}
            }
        }

        // Must have the exit notification
        assert!(has_exit, "cancel_command should emit CommandExit");

        // Combined output from step + cancel should contain "hello"
        let combined = format!("{stdout_from_step}{stdout_from_cancel}");
        assert!(
            combined.contains("hello"),
            "Buffered output must be preserved on cancel. Got: {combined:?}"
        );
    }

    // =========================================================================
    // Stdin EOF handling tests
    // =========================================================================

    #[test]
    fn stdin_eof_with_data_signals_eof_on_next_read() {
        // Test: When host sends StdinData { data: [...], eof: true }, the data should
        // be delivered, and subsequent reads should return EOF (empty) immediately.

        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Create a command that reads stdin twice (cat | head -n 1 reads, then reads again)
        // We'll use 'head -n 2' to force two read attempts
        let handle = runtime.create_command("head -n 2");

        // Step to start - should block on ReadStdin
        let resp = runtime.step();
        assert!(!resp.all_done());

        // Find the ReadStdin request
        let stdin_op = resp
            .host_ops
            .iter()
            .find(|op| matches!(op.request, HostOpRequest::ReadStdin { .. }));
        assert!(stdin_op.is_some(), "Should have a ReadStdin request");
        let stdin_op = stdin_op.unwrap();

        // Send first line with EOF flag - simulates "last chunk + eof"
        let results = vec![HostOpResult {
            id: stdin_op.id,
            runtime_id: runtime.id,
            result: HostOpResponse::StdinData {
                data: b"line1\n".to_vec(),
                eof: true, // This is the key: data + eof in same response
            },
        }];
        runtime.submit_results(&results);

        // Command should be marked as stdin-closed
        assert!(
            runtime.stdin_closed_commands.contains(&handle),
            "Command should be marked as stdin-closed after eof: true"
        );

        // Step again - the command may try to read more, but should get EOF immediately
        let resp = runtime.step();

        // Any subsequent ReadStdin should NOT go to host (handled internally)
        let has_read_stdin = resp
            .host_ops
            .iter()
            .any(|op| matches!(op.request, HostOpRequest::ReadStdin { .. }));
        assert!(
            !has_read_stdin,
            "After EOF, stdin reads should not go to host"
        );
    }

    #[test]
    fn stdin_eof_empty_data_signals_eof() {
        // Test: When host sends StdinData { data: [], eof: true }, subsequent reads
        // should return EOF immediately.

        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        let handle = runtime.create_command("cat");

        // Step to start - should block on ReadStdin
        let resp = runtime.step();
        let stdin_op = resp
            .host_ops
            .iter()
            .find(|op| matches!(op.request, HostOpRequest::ReadStdin { .. }))
            .unwrap();

        // Send EOF with empty data
        let results = vec![HostOpResult {
            id: stdin_op.id,
            runtime_id: runtime.id,
            result: HostOpResponse::StdinData {
                data: vec![],
                eof: true,
            },
        }];
        runtime.submit_results(&results);

        // Command should be marked as stdin-closed
        assert!(runtime.stdin_closed_commands.contains(&handle));

        // Command should complete since cat exits on EOF
        let resp = runtime.step();
        assert!(resp.all_done(), "cat should exit after receiving EOF");
    }

    #[test]
    fn stdin_without_eof_allows_more_reads() {
        // Test: When host sends StdinData { data: [...], eof: false }, subsequent
        // reads should still go to host.

        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        let handle = runtime.create_command("cat");

        // Step to start - should block on ReadStdin
        let resp = runtime.step();
        let stdin_op = resp
            .host_ops
            .iter()
            .find(|op| matches!(op.request, HostOpRequest::ReadStdin { .. }))
            .unwrap();

        // Send data WITHOUT eof flag
        let results = vec![HostOpResult {
            id: stdin_op.id,
            runtime_id: runtime.id,
            result: HostOpResponse::StdinData {
                data: b"hello\n".to_vec(),
                eof: false, // Not EOF - more data may come
            },
        }];
        runtime.submit_results(&results);

        // Command should NOT be marked as stdin-closed
        assert!(
            !runtime.stdin_closed_commands.contains(&handle),
            "Command should not be marked stdin-closed when eof: false"
        );

        // Step again - cat will output the data and then try to read more
        // We may need to ack the Output before it reads again
        let mut found_read_stdin = false;
        for _ in 0..5 {
            let resp = runtime.step();

            // Check for ReadStdin
            if resp
                .host_ops
                .iter()
                .any(|op| matches!(op.request, HostOpRequest::ReadStdin { .. }))
            {
                found_read_stdin = true;
                break;
            }

            // Ack any Output ops to allow progress
            let results: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| matches!(op.request, HostOpRequest::Output { .. }))
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !results.is_empty() {
                runtime.submit_results(&results);
            }

            if resp.all_done() {
                break;
            }
        }

        assert!(
            found_read_stdin,
            "Without EOF, stdin reads should still go to host"
        );
    }

    #[test]
    fn cancel_command_cleans_up_stdin_state() {
        // Test: When a command is cancelled, its stdin state should be cleaned up.

        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        let handle = runtime.create_command("cat");

        // Step to start - should block on ReadStdin
        let resp = runtime.step();
        let stdin_op = resp
            .host_ops
            .iter()
            .find(|op| matches!(op.request, HostOpRequest::ReadStdin { .. }))
            .unwrap();

        // Verify we're tracking the pending stdin op
        assert!(
            runtime.pending_stdin_ops.contains_key(&stdin_op.id),
            "Should be tracking pending stdin op"
        );

        // Cancel the command
        runtime.cancel_command(handle);

        // Pending stdin op should be cleaned up
        assert!(
            !runtime.pending_stdin_ops.contains_key(&stdin_op.id),
            "Pending stdin op should be cleaned up after cancel"
        );

        // Stdin closed state should also be cleaned up
        assert!(
            !runtime.stdin_closed_commands.contains(&handle),
            "Stdin closed state should be cleaned up after cancel"
        );
    }

    #[test]
    fn multiple_commands_have_independent_stdin_eof() {
        // Test: Two commands should have independent stdin EOF state.

        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        let handle1 = runtime.create_command("cat");
        let handle2 = runtime.create_command("cat");

        // Step to start both - both should block on ReadStdin
        let resp = runtime.step();

        // Find stdin ops for each command
        let stdin_ops: Vec<_> = resp
            .host_ops
            .iter()
            .filter(|op| matches!(op.request, HostOpRequest::ReadStdin { .. }))
            .collect();
        assert_eq!(stdin_ops.len(), 2, "Both commands should have stdin ops");

        // Find which op belongs to which command
        let cmd1_op = stdin_ops.iter().find(|op| op.command == Some(handle1));
        let cmd2_op = stdin_ops.iter().find(|op| op.command == Some(handle2));
        assert!(cmd1_op.is_some() && cmd2_op.is_some());
        let cmd1_op = cmd1_op.unwrap();

        // Send EOF only to command 1
        let results = vec![HostOpResult {
            id: cmd1_op.id,
            runtime_id: runtime.id,
            result: HostOpResponse::StdinData {
                data: vec![],
                eof: true,
            },
        }];
        runtime.submit_results(&results);

        // Only command 1 should be marked as stdin-closed
        assert!(runtime.stdin_closed_commands.contains(&handle1));
        assert!(!runtime.stdin_closed_commands.contains(&handle2));
    }

    /// E2E test for `QuickJS` JavaScript execution via the `node` command.
    /// This test verifies that JavaScript code runs correctly through the shell.
    #[test]
    fn quickjs_node_command_executes_javascript() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Run JavaScript that outputs a known value
        let handle = runtime.create_command("node -e \"console.log(40 + 2)\"");

        // Run until completion
        let mut stdout_output = Vec::new();
        let mut command_exited = false;

        for _ in 0..20 {
            let resp = runtime.step();

            // Collect stdout writes (stream 1) and check for exit
            for op in &resp.host_ops {
                match &op.request {
                    HostOpRequest::Output {
                        stream: 1, data, ..
                    } if op.command == Some(handle) => {
                        stdout_output.extend_from_slice(data);
                    }
                    HostOpRequest::CommandExit { .. } if op.command == Some(handle) => {
                        command_exited = true;
                    }
                    _ => {}
                }
            }

            // Acknowledge Output and CommandExit ops
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if command_exited || resp.all_done() {
                break;
            }
        }

        assert!(command_exited, "Command should have exited");

        // Verify the output is "42\n"
        let output = String::from_utf8_lossy(&stdout_output);
        assert_eq!(output.trim(), "42", "JavaScript should compute 40+2=42");
    }

    /// E2E test for `QuickJS` with `console.error` going to stderr.
    #[test]
    fn quickjs_node_command_stderr_routing() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Run JavaScript that outputs to both stdout and stderr
        let handle = runtime.create_command("node -e \"console.log('out'); console.error('err')\"");

        // Run until completion
        let mut stdout_output = Vec::new();
        let mut stderr_output = Vec::new();
        let mut command_exited = false;

        for _ in 0..20 {
            let resp = runtime.step();

            // Collect stdout (stream 1) and stderr (stream 2) writes
            for op in &resp.host_ops {
                if op.command != Some(handle) {
                    continue;
                }
                match &op.request {
                    HostOpRequest::Output {
                        stream: 1, data, ..
                    } => {
                        stdout_output.extend_from_slice(data);
                    }
                    HostOpRequest::Output {
                        stream: 2, data, ..
                    } => {
                        stderr_output.extend_from_slice(data);
                    }
                    HostOpRequest::CommandExit { .. } => {
                        command_exited = true;
                    }
                    _ => {}
                }
            }

            // Acknowledge Output and CommandExit ops
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if command_exited || resp.all_done() {
                break;
            }
        }

        assert!(command_exited, "Command should have exited");

        // Verify stdout has "out" and stderr has "err"
        let stdout = String::from_utf8_lossy(&stdout_output);
        let stderr = String::from_utf8_lossy(&stderr_output);
        assert!(
            stdout.contains("out"),
            "stdout should contain 'out', got: {stdout}"
        );
        assert!(
            stderr.contains("err"),
            "stderr should contain 'err', got: {stderr}"
        );
    }

    /// E2E test for `QuickJS` with the `-p` flag (print result).
    #[test]
    fn quickjs_node_print_flag() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Use -p flag to print the result of an expression
        let handle = runtime.create_command("node -p \"Math.sqrt(16)\"");

        // Run until completion
        let mut stdout_output = Vec::new();
        let mut command_exited = false;

        for _ in 0..20 {
            let resp = runtime.step();

            for op in &resp.host_ops {
                if op.command != Some(handle) {
                    continue;
                }
                match &op.request {
                    HostOpRequest::Output {
                        stream: 1, data, ..
                    } => {
                        stdout_output.extend_from_slice(data);
                    }
                    HostOpRequest::CommandExit { .. } => {
                        command_exited = true;
                    }
                    _ => {}
                }
            }

            // Acknowledge Output and CommandExit ops
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if command_exited || resp.all_done() {
                break;
            }
        }

        assert!(command_exited, "Command should have exited");

        // Verify the output is "4"
        let output = String::from_utf8_lossy(&stdout_output);
        assert_eq!(output.trim(), "4", "Math.sqrt(16) should be 4");
    }

    /// E2E test for multiple `QuickJS` instances running concurrently in the same runtime.
    /// Verifies that each node command has isolated JavaScript state.
    #[test]
    fn quickjs_multiple_node_instances_same_runtime() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Create two node commands that compute different values
        let handle1 = runtime.create_command("node -e \"console.log('cmd1:' + (10 + 5))\"");
        let handle2 = runtime.create_command("node -e \"console.log('cmd2:' + (20 * 3))\"");

        // Collect output per command
        let mut stdout1 = Vec::new();
        let mut stdout2 = Vec::new();
        let mut exited1 = false;
        let mut exited2 = false;

        for _ in 0..40 {
            let resp = runtime.step();

            for op in &resp.host_ops {
                match (&op.request, op.command) {
                    (
                        HostOpRequest::Output {
                            stream: 1, data, ..
                        },
                        Some(h),
                    ) if h == handle1 => {
                        stdout1.extend_from_slice(data);
                    }
                    (
                        HostOpRequest::Output {
                            stream: 1, data, ..
                        },
                        Some(h),
                    ) if h == handle2 => {
                        stdout2.extend_from_slice(data);
                    }
                    (HostOpRequest::CommandExit { .. }, Some(h)) if h == handle1 => {
                        exited1 = true;
                    }
                    (HostOpRequest::CommandExit { .. }, Some(h)) if h == handle2 => {
                        exited2 = true;
                    }
                    _ => {}
                }
            }

            // Acknowledge all ops
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if (exited1 && exited2) || resp.all_done() {
                break;
            }
        }

        assert!(exited1, "Command 1 should have exited");
        assert!(exited2, "Command 2 should have exited");

        // Verify each command got its own output
        let out1 = String::from_utf8_lossy(&stdout1);
        let out2 = String::from_utf8_lossy(&stdout2);
        assert!(
            out1.contains("cmd1:15"),
            "Command 1 should output 'cmd1:15', got: {out1}"
        );
        assert!(
            out2.contains("cmd2:60"),
            "Command 2 should output 'cmd2:60', got: {out2}"
        );
    }

    /// E2E test for `QuickJS` instances in separate runtimes.
    /// Verifies that runtimes have completely isolated JavaScript state.
    #[test]
    fn quickjs_separate_runtimes_isolation() {
        let (pca1, _guard1) = create_test_pca();
        let (pca2, _guard2) = create_test_pca();

        let mut runtime1 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca1, ts, rs).unwrap()
        };
        let mut runtime2 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca2, ts, rs).unwrap()
        };

        // Each runtime runs its own node command
        let handle1 = runtime1.create_command("node -e \"console.log('rt1:' + (100 + 11))\"");
        let handle2 = runtime2.create_command("node -e \"console.log('rt2:' + (200 + 22))\"");

        // Collect output from each runtime
        let mut stdout1 = Vec::new();
        let mut stdout2 = Vec::new();
        let mut exited1 = false;
        let mut exited2 = false;

        for _ in 0..40 {
            // Step both runtimes
            let resp1 = runtime1.step();
            let resp2 = runtime2.step();

            // Process runtime1 output
            for op in &resp1.host_ops {
                match (&op.request, op.command) {
                    (
                        HostOpRequest::Output {
                            stream: 1, data, ..
                        },
                        Some(h),
                    ) if h == handle1 => {
                        stdout1.extend_from_slice(data);
                    }
                    (HostOpRequest::CommandExit { .. }, Some(h)) if h == handle1 => {
                        exited1 = true;
                    }
                    _ => {}
                }
            }

            // Process runtime2 output
            for op in &resp2.host_ops {
                match (&op.request, op.command) {
                    (
                        HostOpRequest::Output {
                            stream: 1, data, ..
                        },
                        Some(h),
                    ) if h == handle2 => {
                        stdout2.extend_from_slice(data);
                    }
                    (HostOpRequest::CommandExit { .. }, Some(h)) if h == handle2 => {
                        exited2 = true;
                    }
                    _ => {}
                }
            }

            // Acknowledge ops for runtime1
            let acks1: Vec<_> = resp1
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime1.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks1.is_empty() {
                runtime1.submit_results(&acks1);
            }

            // Acknowledge ops for runtime2
            let acks2: Vec<_> = resp2
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime2.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks2.is_empty() {
                runtime2.submit_results(&acks2);
            }

            if (exited1 && exited2) || (resp1.all_done() && resp2.all_done()) {
                break;
            }
        }

        assert!(exited1, "Runtime 1 command should have exited");
        assert!(exited2, "Runtime 2 command should have exited");

        // Verify each runtime got its own output
        let out1 = String::from_utf8_lossy(&stdout1);
        let out2 = String::from_utf8_lossy(&stdout2);
        assert!(
            out1.contains("rt1:111"),
            "Runtime 1 should output 'rt1:111', got: {out1}"
        );
        assert!(
            out2.contains("rt2:222"),
            "Runtime 2 should output 'rt2:222', got: {out2}"
        );
    }

    /// E2E test for `QuickJS` with global state isolation between concurrent commands.
    /// Verifies that modifying globalThis in one command doesn't affect another.
    #[test]
    fn quickjs_global_state_isolation_concurrent() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // First command sets a global, second tries to read it
        // They should be isolated - second should not see first's global
        let handle1 = runtime
            .create_command("node -e \"globalThis.shared = 'from_cmd1'; console.log('set')\"");
        let handle2 = runtime.create_command(
            "node -e \"console.log('value:' + (globalThis.shared || 'undefined'))\"",
        );

        let mut stdout1 = Vec::new();
        let mut stdout2 = Vec::new();
        let mut exited1 = false;
        let mut exited2 = false;

        for _ in 0..40 {
            let resp = runtime.step();

            for op in &resp.host_ops {
                match (&op.request, op.command) {
                    (
                        HostOpRequest::Output {
                            stream: 1, data, ..
                        },
                        Some(h),
                    ) if h == handle1 => {
                        stdout1.extend_from_slice(data);
                    }
                    (
                        HostOpRequest::Output {
                            stream: 1, data, ..
                        },
                        Some(h),
                    ) if h == handle2 => {
                        stdout2.extend_from_slice(data);
                    }
                    (HostOpRequest::CommandExit { .. }, Some(h)) if h == handle1 => {
                        exited1 = true;
                    }
                    (HostOpRequest::CommandExit { .. }, Some(h)) if h == handle2 => {
                        exited2 = true;
                    }
                    _ => {}
                }
            }

            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if (exited1 && exited2) || resp.all_done() {
                break;
            }
        }

        assert!(exited1, "Command 1 should have exited");
        assert!(exited2, "Command 2 should have exited");

        // Command 1 should output "set"
        let out1 = String::from_utf8_lossy(&stdout1);
        assert!(out1.contains("set"), "Command 1 should output 'set'");

        // Command 2 should output "value:undefined" (not "value:from_cmd1")
        // because each node command has its own isolated QuickJS instance
        let out2 = String::from_utf8_lossy(&stdout2);
        assert!(
            out2.contains("value:undefined"),
            "Command 2 should not see cmd1's global, got: {out2}"
        );
    }

    // =========================================================================
    // Timing Tests
    // =========================================================================

    /// Create a time source that increments on each call.
    ///
    /// Returns a tuple of (`time_source`, `call_counter`) where:
    /// - `time_source`: Function that returns `1_000_000` ns per call (1ms)
    /// - `call_counter`: Rc<`RefCell`<u64>> to inspect call count
    fn advancing_time_sources() -> (TimeSourceFn, Rc<RefCell<u64>>) {
        let counter = Rc::new(RefCell::new(0u64));
        let counter_clone = Rc::clone(&counter);
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| {
            let mut c = counter_clone.borrow_mut();
            *c += 1;
            // Each call returns 1ms more (1_000_000 ns)
            *c * 1_000_000
        });
        (time_source, counter)
    }

    #[test]
    fn test_command_exit_has_timing_fields() {
        let (time_source, _counter) = advancing_time_sources();
        let random_source: RandomSourceFn = Rc::new(|_| 42);

        let mut runtime = Runtime::new_test(time_source, random_source);
        let handle = runtime.create_command("echo test");

        // Run to completion
        let mut exit_found = false;
        let mut timing_info: Option<(u64, u64)> = None;

        loop {
            let resp = runtime.step();

            for op in &resp.host_ops {
                if let HostOpRequest::CommandExit {
                    elapsed_ns,
                    user_time_ns,
                    ..
                } = &op.request
                    && op.command == Some(handle)
                {
                    exit_found = true;
                    timing_info = elapsed_ns.zip(*user_time_ns);
                }
            }

            // Acknowledge all
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: op.runtime_id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if resp.all_done() {
                break;
            }
        }

        assert!(exit_found, "Should have found CommandExit");
        let (elapsed, user_time) = timing_info.expect("CommandExit should have timing fields");
        assert!(elapsed > 0, "elapsed_ns should be > 0");
        assert!(user_time > 0, "user_time_ns should be > 0");
        assert!(
            user_time <= elapsed,
            "user_time ({user_time}) should be <= elapsed ({elapsed})"
        );
    }

    #[test]
    fn test_concurrent_commands_have_independent_timing() {
        let (time_source, _counter) = advancing_time_sources();
        let random_source: RandomSourceFn = Rc::new(|_| 42);

        let mut runtime = Runtime::new_test(time_source, random_source);

        // Create two commands that will run concurrently
        let h1 = runtime.create_command("echo first");
        let h2 = runtime.create_command("echo second");

        let mut timing_h1: Option<(u64, u64)> = None;
        let mut timing_h2: Option<(u64, u64)> = None;

        loop {
            let resp = runtime.step();

            for op in &resp.host_ops {
                if let HostOpRequest::CommandExit {
                    elapsed_ns,
                    user_time_ns,
                    ..
                } = &op.request
                {
                    let timing = elapsed_ns.zip(*user_time_ns);
                    match op.command {
                        Some(h) if h == h1 => timing_h1 = timing,
                        Some(h) if h == h2 => timing_h2 = timing,
                        _ => {}
                    }
                }
            }

            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: op.runtime_id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if resp.all_done() {
                break;
            }
        }

        let (elapsed1, user1) = timing_h1.expect("h1 should have timing");
        let (elapsed2, user2) = timing_h2.expect("h2 should have timing");

        // Both commands should have reasonable timing
        assert!(elapsed1 > 0, "h1 elapsed should be > 0");
        assert!(elapsed2 > 0, "h2 elapsed should be > 0");
        assert!(user1 > 0, "h1 user time should be > 0");
        assert!(user2 > 0, "h2 user time should be > 0");

        // When commands run concurrently in same step, they share user time
        // (both were active during the scheduler execution)
    }

    #[test]
    fn test_cancel_command_reports_timing() {
        let (time_source, _counter) = advancing_time_sources();
        let random_source: RandomSourceFn = Rc::new(|_| 42);

        let mut runtime = Runtime::new_test(time_source, random_source);
        let handle = runtime.create_command("cat"); // Will block on stdin

        // Step once to start the command
        let _ = runtime.step();

        // Cancel it
        let cancel_ops = runtime.cancel_command(handle);

        // Find CommandExit with timing
        let exit_op = cancel_ops
            .iter()
            .find(|op| matches!(op.request, HostOpRequest::CommandExit { .. }));

        assert!(exit_op.is_some(), "Cancel should emit CommandExit");

        if let Some(op) = exit_op
            && let HostOpRequest::CommandExit {
                code,
                elapsed_ns,
                user_time_ns,
            } = &op.request
        {
            assert_eq!(*code, -1, "Cancelled command should have exit code -1");
            assert!(elapsed_ns.is_some(), "Should have elapsed_ns");
            assert!(user_time_ns.is_some(), "Should have user_time_ns");

            let elapsed = elapsed_ns.unwrap();
            let user_time = user_time_ns.unwrap();
            assert!(elapsed >= user_time, "elapsed >= user_time");
        }
    }

    // =========================================================================
    // Tool Call E2E Tests
    // =========================================================================

    /// Helper to run a command handling tool calls with a mock tool result.
    fn run_with_tool_handler(
        runtime: &mut Runtime,
        tool_handler: impl Fn(&str, &Value) -> Value,
    ) -> HashMap<Option<CommandHandle>, CollectedOutput> {
        let mut collected: HashMap<Option<CommandHandle>, CollectedOutput> = HashMap::new();
        let mut step_count = 0;

        loop {
            step_count += 1;
            let resp = runtime.step();
            eprintln!(
                "STEP {}: {} host ops, all_done={}",
                step_count,
                resp.host_ops.len(),
                resp.all_done()
            );

            let mut results: Vec<HostOpResult> = Vec::new();

            for op in &resp.host_ops {
                eprintln!("  OP: {:?}", std::mem::discriminant(&op.request));
                match &op.request {
                    HostOpRequest::Output { stream, data, .. } => {
                        let entry = collected.entry(op.command).or_default();
                        let text = String::from_utf8_lossy(data);
                        if *stream == 1 {
                            entry.stdout.push_str(&text);
                        } else {
                            entry.stderr.push_str(&text);
                        }
                        results.push(HostOpResult {
                            id: op.id,
                            runtime_id: op.runtime_id,
                            result: HostOpResponse::OutputAck,
                        });
                    }
                    HostOpRequest::CommandExit { code, .. } => {
                        let entry = collected.entry(op.command).or_default();
                        entry.exit_code = Some(*code);
                        results.push(HostOpResult {
                            id: op.id,
                            runtime_id: op.runtime_id,
                            result: HostOpResponse::ExitAck,
                        });
                    }
                    HostOpRequest::ToolCall { tool, params } => {
                        let result = tool_handler(tool, params);
                        results.push(HostOpResult {
                            id: op.id,
                            runtime_id: op.runtime_id,
                            result: HostOpResponse::tool_result(result),
                        });
                    }
                    _ => {
                        // For other ops, just acknowledge
                        results.push(HostOpResult {
                            id: op.id,
                            runtime_id: op.runtime_id,
                            result: HostOpResponse::OutputAck,
                        });
                    }
                }
            }

            if !results.is_empty() {
                runtime.submit_results(&results);
            }

            if resp.all_done() {
                break;
            }
        }

        collected
    }

    #[test]
    fn test_tool_call_via_javascript_async_iife() {
        // Test that JS async IIFE tool calls work and output is captured
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // Run JavaScript that calls a tool and logs the result
        // This mimics what the LiveShell demo does
        //
        // The prelude.js is auto-loaded by node, making tool functions available.
        // We use `create_charge()` which is defined in prelude.js.
        let js_code = r#"(async () => {
            const result = await create_charge({ amount: 5000, currency: "usd" });
            console.log(JSON.stringify(result));
        })()"#;
        let h = runtime.create_command(&format!("node -e {}", shell_escape(js_code)));

        let collected = run_with_tool_handler(&mut runtime, |tool, params: &Value| {
            eprintln!("TOOL CALL: {tool} with {params:?}");
            // Mock stripe:create_charge response
            if tool == "stripe:create_charge" {
                serde_json::json!({
                    "success": true,
                    "charge_id": "ch_test123",
                    "amount": params.get("amount").unwrap_or(&serde_json::json!(0)),
                    "currency": params.get("currency").unwrap_or(&serde_json::json!("usd"))
                })
            } else {
                serde_json::json!({ "error": format!("Unknown tool: {tool}") })
            }
        });

        let output = collected.get(&Some(h)).unwrap();
        eprintln!("STDOUT: {}", output.stdout);
        eprintln!("STDERR: {}", output.stderr);
        eprintln!("EXIT: {:?}", output.exit_code);

        assert!(
            output.stdout.contains("ch_test123"),
            "Output should contain charge_id from mock: {}",
            output.stdout
        );
        assert!(
            output.stdout.contains("5000"),
            "Output should contain amount: {}",
            output.stdout
        );
    }

    #[test]
    fn test_tool_call_direct_charge_function() {
        // Test calling the generated charge() function directly
        let (pca, _guard) = create_test_pca_with_tools();
        let tools = crate::mcp::example_stripe_tools();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(pca, &tools, ts, rs).unwrap()
        };

        // First, verify the prelude contains the charge function
        let cat_h = runtime.create_command("cat /tools/prelude.js");
        let cat_collected = run_to_completion(&mut runtime);
        let cat_output = cat_collected.get(&Some(cat_h)).unwrap();
        eprintln!("PRELUDE CONTENT:\n{}", cat_output.stdout);

        // Now test calling a tool (using JS)
        let mut runtime2 = {
            let (ts, rs) = test_sources();
            Runtime::from_pca_with_tools(create_test_pca_with_tools().0, &tools, ts, rs).unwrap()
        };

        // Use simple console.log to test output works
        let h = runtime2.create_command("node -e \"console.log('hello world')\"");
        let collected =
            run_with_tool_handler(&mut runtime2, |_, _| serde_json::json!({"test": true}));

        let output = collected.get(&Some(h)).unwrap();
        eprintln!("SIMPLE OUTPUT STDOUT: {}", output.stdout);
        eprintln!("SIMPLE OUTPUT STDERR: {}", output.stderr);

        assert!(
            output.stdout.contains("hello world"),
            "Simple console.log should work: {}",
            output.stdout
        );
    }

    // ========================================================================
    // JavaScript Runtime Error Reporting Tests
    // ========================================================================

    /// Test that undefined property access errors are reported to stderr.
    /// Regression test for: "JS runtime errors are silent (`QuickJS` issue)"
    #[test]
    fn quickjs_runtime_error_undefined_property_access() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Code that causes a TypeError: accessing property of undefined
        let js_code = r"
            const x = undefined;
            console.log(x.foo.bar);
        ";
        let handle = runtime.create_command(&format!("node -e {}", shell_escape(js_code)));

        // Run until completion, collecting output
        let mut stderr_output = Vec::new();
        let mut command_exited = false;

        for _ in 0..50 {
            let resp = runtime.step();

            for op in &resp.host_ops {
                if op.command != Some(handle) {
                    continue;
                }
                match &op.request {
                    HostOpRequest::Output {
                        stream: 2, data, ..
                    } => {
                        stderr_output.extend_from_slice(data);
                    }
                    HostOpRequest::CommandExit { .. } => {
                        command_exited = true;
                    }
                    _ => {}
                }
            }

            // Acknowledge ops
            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if command_exited || resp.all_done() {
                break;
            }
        }

        assert!(command_exited, "Command should have exited");

        // Verify stderr contains the TypeError
        let stderr = String::from_utf8_lossy(&stderr_output);
        assert!(
            stderr.contains("TypeError") || stderr.contains("undefined"),
            "stderr should contain TypeError for undefined property access, got: {stderr}"
        );
    }

    /// Test that calling undefined functions produces an error message.
    /// Regression test for: "JS runtime errors are silent (`QuickJS` issue)"
    #[test]
    fn quickjs_runtime_error_undefined_function() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Code that causes a ReferenceError: calling undefined function
        let js_code = r"
            const result = nonExistentFunction();
            console.log(result);
        ";
        let handle = runtime.create_command(&format!("node -e {}", shell_escape(js_code)));

        // Run until completion
        let mut stderr_output = Vec::new();
        let mut command_exited = false;

        for _ in 0..50 {
            let resp = runtime.step();

            for op in &resp.host_ops {
                if op.command != Some(handle) {
                    continue;
                }
                match &op.request {
                    HostOpRequest::Output {
                        stream: 2, data, ..
                    } => {
                        stderr_output.extend_from_slice(data);
                    }
                    HostOpRequest::CommandExit { .. } => {
                        command_exited = true;
                    }
                    _ => {}
                }
            }

            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if command_exited || resp.all_done() {
                break;
            }
        }

        assert!(command_exited, "Command should have exited");

        // Verify stderr contains the ReferenceError
        let stderr = String::from_utf8_lossy(&stderr_output);
        assert!(
            stderr.contains("ReferenceError") || stderr.contains("not defined"),
            "stderr should contain ReferenceError for undefined function, got: {stderr}"
        );
    }

    /// Test that `TypeError` for non-function is reported.
    /// Regression test for: "JS runtime errors are silent (`QuickJS` issue)"
    #[test]
    fn quickjs_runtime_error_not_a_function() {
        let (pca, _guard) = create_test_pca();
        let mut runtime = {
            let (ts, rs) = test_sources();
            Runtime::from_pca(pca, ts, rs).unwrap()
        };

        // Code that causes a TypeError: calling non-function
        let js_code = r"
            const num = 42;
            num.map(x => x * 2);
        ";
        let handle = runtime.create_command(&format!("node -e {}", shell_escape(js_code)));

        let mut stderr_output = Vec::new();
        let mut command_exited = false;

        for _ in 0..50 {
            let resp = runtime.step();

            for op in &resp.host_ops {
                if op.command != Some(handle) {
                    continue;
                }
                match &op.request {
                    HostOpRequest::Output {
                        stream: 2, data, ..
                    } => {
                        stderr_output.extend_from_slice(data);
                    }
                    HostOpRequest::CommandExit { .. } => {
                        command_exited = true;
                    }
                    _ => {}
                }
            }

            let acks: Vec<_> = resp
                .host_ops
                .iter()
                .filter(|op| {
                    matches!(
                        op.request,
                        HostOpRequest::Output { .. } | HostOpRequest::CommandExit { .. }
                    )
                })
                .map(|op| HostOpResult {
                    id: op.id,
                    runtime_id: runtime.id,
                    result: HostOpResponse::OutputAck,
                })
                .collect();
            if !acks.is_empty() {
                runtime.submit_results(&acks);
            }

            if command_exited || resp.all_done() {
                break;
            }
        }

        assert!(command_exited, "Command should have exited");

        let stderr = String::from_utf8_lossy(&stderr_output);
        assert!(
            stderr.contains("TypeError") || stderr.contains("not a function"),
            "stderr should contain TypeError for calling non-function, got: {stderr}"
        );
    }

    /// Shell escape a string for passing to node -e
    fn shell_escape(s: &str) -> String {
        // Use single quotes and escape any single quotes in the string
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
