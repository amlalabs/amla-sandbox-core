//! # amla-audit
//!
//! Structured audit logging for AI agent observability.

#![forbid(unsafe_code)]
//!
//! This crate provides a lightweight, structured logging system designed for
//! observability of AI agent operations. All log entries are serializable to
//! JSONL format for easy ingestion by log aggregation systems.
//!
//! ## Features
//!
//! - **Structured entries**: Typed log entries with consistent schema
//! - **JSONL output**: One JSON object per line for streaming
//! - **Session tracking**: All entries tied to a session ID
//! - **Hashing for privacy**: Sensitive data can be hashed before logging
//!
//! ## Example
//!
//! ```rust
//! use amla_audit::{AuditLog, LogEntry};
//!
//! let mut log = AuditLog::new();
//!
//! // Log session start
//! log.log(LogEntry::session_start("sess_123", vec!["tool-call".to_string()]));
//!
//! // Log a tool call
//! log.log(LogEntry::tool_call(
//!     "sess_123",
//!     "stripe.charge",
//!     &serde_json::json!({"amount": 5000}),
//!     true,
//!     None,
//! ));
//!
//! // Get JSONL output
//! let jsonl = log.to_jsonl();
//! ```
//!
//! ## Log Entry Types
//!
//! | Type | Description |
//! |------|-------------|
//! | `SessionStart` | Session created with initial capabilities |
//! | `SessionEnd` | Session terminated |
//! | `Shell` | Shell command executed |
//! | `JsStart` | JavaScript execution started |
//! | `JsEnd` | JavaScript execution completed |
//! | `ToolCall` | Tool invocation (with params hash) |
//! | `MemoryRead` | Memory/state read operation |
//! | `MemoryWrite` | Memory/state write operation |
//! | `MemoryDelete` | Memory/state delete operation |
//! | `Spawn` | Child session spawned |
//! | `ConstraintViolation` | Authorization constraint violated |
//! | `FileRead` | VFS file read |
//! | `FileWrite` | VFS file write |
//! | `HostOpRequest` | Host operation request (metadata only) |
//! | `HostOpResponse` | Host operation response (metadata only) |
//! | `StreamChunk` | I/O stream data (stdout/stderr/stdin) |
//! | `CommandCreate` | Command created in runtime |
//! | `CommandExit` | Command exited |

// missing_docs lint inherited from workspace
#![deny(rustdoc::broken_intra_doc_links)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Log level for filtering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Debug-level entries (file reads, memory ops)
    Debug = 0,
    /// Info-level entries (session lifecycle, commands)
    #[default]
    Info = 1,
    /// Warning-level entries (constraint violations)
    Warn = 2,
    /// Error-level entries (failures)
    Error = 3,
}

/// Structured log entry.
///
/// All entries include a session ID and timestamp. The entry type
/// is encoded in the `type` field for easy filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogEntry {
    /// Session created.
    SessionStart {
        /// Unique session identifier.
        session_id: String,
        /// When the session was created.
        timestamp: DateTime<Utc>,
        /// List of capability names granted to the session.
        capabilities: Vec<String>,
    },

    /// Session ended.
    SessionEnd {
        /// Unique session identifier.
        session_id: String,
        /// When the session ended.
        timestamp: DateTime<Utc>,
    },

    /// Shell command executed.
    Shell {
        /// Session that executed the command.
        session_id: String,
        /// When the command was executed.
        timestamp: DateTime<Utc>,
        /// Command name (e.g., "ls", "cat").
        command: String,
        /// Command arguments.
        args: Vec<String>,
        /// Exit code (0 = success).
        exit_code: i32,
    },

    /// JavaScript execution started.
    JsStart {
        /// Session that started JS execution.
        session_id: String,
        /// When execution started.
        timestamp: DateTime<Utc>,
        /// Hash of the code being executed (for privacy).
        code_hash: String,
    },

    /// JavaScript execution completed.
    JsEnd {
        /// Session that completed JS execution.
        session_id: String,
        /// When execution completed.
        timestamp: DateTime<Utc>,
        /// Whether execution succeeded.
        success: bool,
        /// Error message if execution failed.
        error: Option<String>,
    },

    /// Tool call.
    ToolCall {
        /// Session that made the tool call.
        session_id: String,
        /// When the tool was called.
        timestamp: DateTime<Utc>,
        /// Tool name (e.g., "stripe.charge").
        tool: String,
        /// Hash of parameters (for privacy).
        params_hash: String,
        /// Whether the tool call succeeded.
        success: bool,
        /// Error message if the call failed.
        error: Option<String>,
    },

    /// Memory read operation.
    MemoryRead {
        /// Session that performed the read.
        session_id: String,
        /// When the read occurred.
        timestamp: DateTime<Utc>,
        /// Key that was read.
        key: String,
        /// Whether the key was found.
        found: bool,
    },

    /// Memory write operation.
    MemoryWrite {
        /// Session that performed the write.
        session_id: String,
        /// When the write occurred.
        timestamp: DateTime<Utc>,
        /// Key that was written.
        key: String,
        /// Size of the value in bytes.
        size_bytes: usize,
    },

    /// Memory delete operation.
    MemoryDelete {
        /// Session that performed the delete.
        session_id: String,
        /// When the delete occurred.
        timestamp: DateTime<Utc>,
        /// Key that was deleted.
        key: String,
    },

    /// Spawn child session.
    Spawn {
        /// Parent session that spawned the child.
        session_id: String,
        /// When the spawn occurred.
        timestamp: DateTime<Utc>,
        /// Identifier of the spawned child session.
        child_session_id: String,
        /// Attenuations applied to the child session.
        attenuations: Vec<String>,
    },

    /// Constraint violation.
    ConstraintViolation {
        /// Session that violated the constraint.
        session_id: String,
        /// When the violation occurred.
        timestamp: DateTime<Utc>,
        /// Operation that was attempted.
        operation: String,
        /// Constraint that was violated.
        constraint: String,
        /// Actual value that caused the violation.
        actual: String,
    },

    /// File read (VFS).
    FileRead {
        /// Session that read the file.
        session_id: String,
        /// When the read occurred.
        timestamp: DateTime<Utc>,
        /// VFS path that was read.
        path: String,
    },

    /// File write (VFS).
    FileWrite {
        /// Session that wrote the file.
        session_id: String,
        /// When the write occurred.
        timestamp: DateTime<Utc>,
        /// VFS path that was written.
        path: String,
        /// Size of the written data in bytes.
        size_bytes: usize,
    },

    /// Custom event for extensibility.
    Custom {
        /// Session that logged the event.
        session_id: String,
        /// When the event occurred.
        timestamp: DateTime<Utc>,
        /// Custom event type name.
        event_type: String,
        /// Arbitrary JSON data for the event.
        data: serde_json::Value,
    },

    // =========================================================================
    // Host Operation Events (metadata only - no binary payloads)
    // =========================================================================
    /// Host operation request from WASM runtime to host.
    ///
    /// Logged when the runtime emits a pending host operation.
    HostOpRequest {
        /// Session identifier.
        session_id: String,
        /// When the request was emitted.
        timestamp: DateTime<Utc>,
        /// Unique operation ID for correlation with response.
        op_id: u64,
        /// Runtime that emitted this operation.
        runtime_id: u64,
        /// Command handle if attributable to a command.
        command_handle: Option<u64>,
        /// Operation type (e.g., `tool_call`, `output`, `wake_at`).
        op_type: String,
        /// Tool name (for `tool_call` operations).
        tool: Option<String>,
        /// Hash of parameters (for `tool_call` operations).
        params_hash: Option<String>,
        /// Size in bytes (for output/stdin operations).
        size_bytes: Option<usize>,
        /// Content hash (for output/stdin operations).
        content_hash: Option<String>,
    },

    /// Host operation response from host to WASM runtime.
    ///
    /// Logged when the host submits a result back to the runtime.
    HostOpResponse {
        /// Session identifier.
        session_id: String,
        /// When the response was submitted.
        timestamp: DateTime<Utc>,
        /// Operation ID matching the request.
        op_id: u64,
        /// Response type.
        response_type: String,
        /// Whether the operation succeeded.
        success: bool,
        /// Error message if the operation failed.
        error: Option<String>,
        /// Latency in nanoseconds from request to response.
        latency_nanos: u64,
        /// Hash of result data (for tool results).
        result_hash: Option<String>,
    },

    /// I/O stream chunk (stdout, stderr, or stdin).
    ///
    /// Logged for each chunk of data flowing through streams.
    StreamChunk {
        /// Session identifier.
        session_id: String,
        /// When the chunk was processed.
        timestamp: DateTime<Utc>,
        /// Command that produced/consumed this chunk.
        command_handle: u64,
        /// Stream type: "stdout", "stderr", or "stdin".
        stream: String,
        /// Size of the chunk in bytes.
        size_bytes: usize,
        /// BLAKE3 hash of the content.
        content_hash: String,
        /// Whether the content is likely text (vs binary).
        is_text: bool,
        /// Preview of text content (first 128 chars if text).
        preview: Option<String>,
    },

    /// Command created in runtime.
    CommandCreate {
        /// Session identifier.
        session_id: String,
        /// When the command was created.
        timestamp: DateTime<Utc>,
        /// Runtime that created the command.
        runtime_id: u64,
        /// Unique command handle.
        command_handle: u64,
        /// Preview of the command (first 256 chars).
        command_preview: String,
        /// BLAKE3 hash of the full command string.
        command_hash: String,
    },

    /// Command exited.
    CommandExit {
        /// Session identifier.
        session_id: String,
        /// When the command exited.
        timestamp: DateTime<Utc>,
        /// Runtime that ran the command.
        runtime_id: u64,
        /// Command handle.
        command_handle: u64,
        /// Exit code (0 = success).
        exit_code: i32,
        /// Duration from creation to exit in nanoseconds.
        duration_nanos: u64,
    },
}

impl LogEntry {
    /// Create a session start entry.
    pub fn session_start(session_id: &str, capabilities: Vec<String>) -> Self {
        Self::SessionStart {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            capabilities,
        }
    }

    /// Create a session end entry.
    pub fn session_end(session_id: &str) -> Self {
        Self::SessionEnd {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
        }
    }

    /// Create a shell command entry.
    pub fn shell(session_id: &str, command: &str, args: Vec<String>, exit_code: i32) -> Self {
        Self::Shell {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            command: command.to_string(),
            args,
            exit_code,
        }
    }

    /// Create a JavaScript start entry.
    pub fn js_start(session_id: &str, code: &str) -> Self {
        Self::JsStart {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            code_hash: hash_string(code),
        }
    }

    /// Create a JavaScript end entry.
    pub fn js_end(session_id: &str, success: bool, error: Option<String>) -> Self {
        Self::JsEnd {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            success,
            error,
        }
    }

    /// Create a tool call entry.
    pub fn tool_call(
        session_id: &str,
        tool: &str,
        params: &serde_json::Value,
        success: bool,
        error: Option<String>,
    ) -> Self {
        Self::ToolCall {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            tool: tool.to_string(),
            params_hash: hash_string(&params.to_string()),
            success,
            error,
        }
    }

    /// Create a memory read entry.
    pub fn memory_read(session_id: &str, key: &str, found: bool) -> Self {
        Self::MemoryRead {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            key: key.to_string(),
            found,
        }
    }

    /// Create a memory write entry.
    pub fn memory_write(session_id: &str, key: &str, size_bytes: usize) -> Self {
        Self::MemoryWrite {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            key: key.to_string(),
            size_bytes,
        }
    }

    /// Create a memory delete entry.
    pub fn memory_delete(session_id: &str, key: &str) -> Self {
        Self::MemoryDelete {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            key: key.to_string(),
        }
    }

    /// Create a spawn entry.
    pub fn spawn(session_id: &str, child_session_id: &str, attenuations: Vec<String>) -> Self {
        Self::Spawn {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            child_session_id: child_session_id.to_string(),
            attenuations,
        }
    }

    /// Create a constraint violation entry.
    pub fn constraint_violation(
        session_id: &str,
        operation: &str,
        constraint: &str,
        actual: &str,
    ) -> Self {
        Self::ConstraintViolation {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            operation: operation.to_string(),
            constraint: constraint.to_string(),
            actual: actual.to_string(),
        }
    }

    /// Create a file read entry.
    pub fn file_read(session_id: &str, path: &str) -> Self {
        Self::FileRead {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            path: path.to_string(),
        }
    }

    /// Create a file write entry.
    pub fn file_write(session_id: &str, path: &str, size_bytes: usize) -> Self {
        Self::FileWrite {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            path: path.to_string(),
            size_bytes,
        }
    }

    /// Create a custom event entry.
    pub fn custom(session_id: &str, event_type: &str, data: serde_json::Value) -> Self {
        Self::Custom {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            event_type: event_type.to_string(),
            data,
        }
    }

    /// Create a host operation request entry.
    #[allow(clippy::too_many_arguments)]
    pub fn host_op_request(
        session_id: &str,
        op_id: u64,
        runtime_id: u64,
        command_handle: Option<u64>,
        op_type: &str,
        tool: Option<&str>,
        params_hash: Option<String>,
        size_bytes: Option<usize>,
        content_hash: Option<String>,
    ) -> Self {
        Self::HostOpRequest {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            op_id,
            runtime_id,
            command_handle,
            op_type: op_type.to_string(),
            tool: tool.map(ToString::to_string),
            params_hash,
            size_bytes,
            content_hash,
        }
    }

    /// Create a host operation response entry.
    #[allow(clippy::too_many_arguments)]
    pub fn host_op_response(
        session_id: &str,
        op_id: u64,
        response_type: &str,
        success: bool,
        error: Option<String>,
        latency_nanos: u64,
        result_hash: Option<String>,
    ) -> Self {
        Self::HostOpResponse {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            op_id,
            response_type: response_type.to_string(),
            success,
            error,
            latency_nanos,
            result_hash,
        }
    }

    /// Create a stream chunk entry.
    pub fn stream_chunk(session_id: &str, command_handle: u64, stream: &str, data: &[u8]) -> Self {
        let is_text = is_likely_text(data);
        Self::StreamChunk {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            command_handle,
            stream: stream.to_string(),
            size_bytes: data.len(),
            content_hash: hash_bytes(data),
            is_text,
            preview: Some(content_preview(data, 10)),
        }
    }

    /// Create a command create entry.
    pub fn command_create(
        session_id: &str,
        runtime_id: u64,
        command_handle: u64,
        command: &str,
    ) -> Self {
        Self::CommandCreate {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            runtime_id,
            command_handle,
            command_preview: command.chars().take(256).collect(),
            command_hash: hash_string(command),
        }
    }

    /// Create a command exit entry.
    pub fn command_exit(
        session_id: &str,
        runtime_id: u64,
        command_handle: u64,
        exit_code: i32,
        duration_nanos: u64,
    ) -> Self {
        Self::CommandExit {
            session_id: session_id.to_string(),
            timestamp: Utc::now(),
            runtime_id,
            command_handle,
            exit_code,
            duration_nanos,
        }
    }

    /// Get the session ID for this entry.
    pub fn session_id(&self) -> &str {
        match self {
            Self::SessionStart { session_id, .. }
            | Self::SessionEnd { session_id, .. }
            | Self::Shell { session_id, .. }
            | Self::JsStart { session_id, .. }
            | Self::JsEnd { session_id, .. }
            | Self::ToolCall { session_id, .. }
            | Self::MemoryRead { session_id, .. }
            | Self::MemoryWrite { session_id, .. }
            | Self::MemoryDelete { session_id, .. }
            | Self::Spawn { session_id, .. }
            | Self::ConstraintViolation { session_id, .. }
            | Self::FileRead { session_id, .. }
            | Self::FileWrite { session_id, .. }
            | Self::Custom { session_id, .. }
            | Self::HostOpRequest { session_id, .. }
            | Self::HostOpResponse { session_id, .. }
            | Self::StreamChunk { session_id, .. }
            | Self::CommandCreate { session_id, .. }
            | Self::CommandExit { session_id, .. } => session_id,
        }
    }

    /// Get the log level for this entry.
    ///
    /// - Debug: File/memory reads, stream chunks (high volume, low importance)
    /// - Info: Session lifecycle, commands, tool calls, host operations
    /// - Warn: Constraint violations, failures that were handled
    /// - Error: JS/tool execution failures
    pub fn level(&self) -> LogLevel {
        match self {
            // Debug: high-volume, low-priority events
            Self::FileRead { .. } | Self::MemoryRead { .. } | Self::StreamChunk { .. } => {
                LogLevel::Debug
            }

            // Info: normal operations
            Self::SessionStart { .. }
            | Self::SessionEnd { .. }
            | Self::Shell { .. }
            | Self::JsStart { .. }
            | Self::MemoryWrite { .. }
            | Self::MemoryDelete { .. }
            | Self::FileWrite { .. }
            | Self::Spawn { .. }
            | Self::Custom { .. }
            | Self::HostOpRequest { .. }
            | Self::CommandCreate { .. }
            | Self::CommandExit { .. } => LogLevel::Info,

            // JsEnd/ToolCall/HostOpResponse: Info if success, Error if failure
            Self::JsEnd { success, .. }
            | Self::ToolCall { success, .. }
            | Self::HostOpResponse { success, .. } => {
                if *success {
                    LogLevel::Info
                } else {
                    LogLevel::Error
                }
            }

            // Warn: security-relevant events that were blocked
            Self::ConstraintViolation { .. } => LogLevel::Warn,
        }
    }

    /// Serialize to JSONL format (one line).
    pub fn to_jsonl(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Audit log collector.
///
/// Collects log entries in memory and provides methods to
/// serialize them to JSONL format.
#[derive(Debug, Default)]
pub struct AuditLog {
    /// Minimum log level to record.
    pub level: LogLevel,
    entries: Vec<LogEntry>,
}

impl AuditLog {
    /// Create a new empty audit log.
    pub fn new() -> Self {
        Self {
            level: LogLevel::Info,
            entries: Vec::new(),
        }
    }

    /// Create an audit log with a specific log level.
    pub fn with_level(level: LogLevel) -> Self {
        Self {
            level,
            entries: Vec::new(),
        }
    }

    /// Log an entry if it meets the minimum log level.
    ///
    /// Entries below the configured level are silently dropped.
    pub fn log(&mut self, entry: LogEntry) {
        if entry.level() >= self.level {
            self.entries.push(entry);
        }
    }

    /// Log an entry unconditionally, bypassing level filtering.
    ///
    /// Use this for critical entries that must always be recorded.
    pub fn log_always(&mut self, entry: LogEntry) {
        self.entries.push(entry);
    }

    /// Get all entries as JSONL string.
    pub fn to_jsonl(&self) -> String {
        self.entries
            .iter()
            .map(LogEntry::to_jsonl)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get all entries.
    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// Get entry count.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Take all entries (returns and clears).
    pub fn take(&mut self) -> Vec<LogEntry> {
        std::mem::take(&mut self.entries)
    }

    /// Filter entries by session ID.
    pub fn filter_by_session(&self, session_id: &str) -> Vec<&LogEntry> {
        self.entries
            .iter()
            .filter(|e| e.session_id() == session_id)
            .collect()
    }
}

/// Cryptographic hash function for privacy-preserving logging.
///
/// Uses BLAKE3 to provide:
/// - Collision resistance: Different inputs produce different hashes
/// - Pre-image resistance: Cannot recover input from hash
/// - Constant-time comparison safety
///
/// Returns a 16-character hex string (64 bits of the hash) for compact logging.
/// This matches the output format of the previous non-cryptographic hash
/// for backwards compatibility.
pub fn hash_string(s: &str) -> String {
    hash_bytes(s.as_bytes())
}

/// Hash arbitrary bytes using BLAKE3.
///
/// Returns a 16-character hex string (64 bits of the hash).
pub fn hash_bytes(data: &[u8]) -> String {
    let hash = blake3::hash(data);
    // Take first 8 bytes (64 bits) and format as 16 hex chars
    let bytes = hash.as_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
    )
}

/// Check if data is likely text (vs binary).
///
/// Uses simple heuristics:
/// - No null bytes in sample
/// - Valid UTF-8 in sample
/// - Low ratio of control characters
pub fn is_likely_text(data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }

    // Sample first 512 bytes
    let sample = &data[..data.len().min(512)];

    // Check for null bytes (strong binary indicator)
    if sample.contains(&0) {
        return false;
    }

    // Check for valid UTF-8
    if std::str::from_utf8(sample).is_err() {
        return false;
    }

    // Check for high ratio of control characters (excluding common whitespace)
    let control_count = sample
        .iter()
        .filter(|&&b| b < 32 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();
    // Less than 10% control chars (use multiply to avoid integer division edge case)
    control_count * 10 < sample.len() || control_count == 0
}

/// Get a text preview of the first N characters.
///
/// Returns None if data is not text or is empty.
pub fn text_preview(data: &[u8], max_chars: usize) -> Option<String> {
    if data.is_empty() || !is_likely_text(data) {
        return None;
    }

    let s = std::str::from_utf8(data).ok()?;
    let preview: String = s.chars().take(max_chars).collect();
    if preview.is_empty() {
        None
    } else {
        Some(preview)
    }
}

/// Get a content preview showing first/last characters or bytes with total size.
///
/// For text data: `"Hello worl...end chars" (156 bytes)`
/// For binary data: `[89 50 4e 47...0a 1a 0a] (1024 bytes)`
///
/// Returns a string showing the first and last `edge_chars` (or bytes for binary),
/// separated by ellipsis, plus the total byte count.
pub fn content_preview(data: &[u8], edge_chars: usize) -> String {
    if data.is_empty() {
        return "(0 bytes)".to_string();
    }

    let len = data.len();

    if is_likely_text(data)
        && let Ok(s) = std::str::from_utf8(data)
    {
        let chars: Vec<char> = s.chars().collect();
        let char_count = chars.len();

        if char_count <= edge_chars * 2 {
            // Short enough to show entirely
            return format!("{s:?} ({len} bytes)");
        }

        let first: String = chars[..edge_chars].iter().collect();
        let last: String = chars[char_count - edge_chars..].iter().collect();
        return format!("{first:?}...{last:?} ({len} bytes)");
    }

    // Binary data - show hex
    if len <= edge_chars * 2 {
        // Short enough to show entirely
        let hex: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
        return format!("[{}] ({} bytes)", hex.join(" "), len);
    }

    let first_hex: Vec<String> = data[..edge_chars]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let last_hex: Vec<String> = data[len - edge_chars..]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "[{}...{}] ({} bytes)",
        first_hex.join(" "),
        last_hex.join(" "),
        len
    )
}

// =============================================================================
// Audit Buffer
// =============================================================================

/// Default buffer size (64KB - enough for ~500 metadata entries).
pub const AUDIT_BUFFER_SIZE: usize = 65536;

/// Ring buffer for streaming audit log entries.
///
/// Appends JSONL entries to an internal buffer. The host can drain
/// entries periodically via the `drain()` method.
///
/// When the buffer is full, oldest entries are overwritten (ring buffer behavior).
#[derive(Debug)]
pub struct AuditBuffer {
    buffer: Vec<u8>,
    write_pos: usize,
    read_pos: usize,
    /// Sequence number for entries.
    seq: u64,
    /// Minimum log level.
    pub level: LogLevel,
    /// Maximum characters for text preview.
    pub preview_chars: usize,
}

impl Default for AuditBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditBuffer {
    /// Create a new audit buffer with default size.
    pub fn new() -> Self {
        Self::with_capacity(AUDIT_BUFFER_SIZE)
    }

    /// Create an audit buffer with specific capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: vec![0u8; capacity],
            write_pos: 0,
            read_pos: 0,
            seq: 0,
            level: LogLevel::Info,
            preview_chars: 128,
        }
    }

    /// Append a log entry to the buffer.
    ///
    /// Returns true if the entry was appended, false if it was filtered by level
    /// or too large.
    pub fn append(&mut self, entry: &LogEntry) -> bool {
        // Check log level
        if entry.level() < self.level {
            return false;
        }

        // Serialize to JSON
        let Ok(mut json) = serde_json::to_vec(entry) else {
            return false;
        };
        json.push(b'\n'); // JSONL format

        // Check if entry fits
        let needed = json.len();
        let capacity = self.buffer.len();

        // Don't accept entries larger than 1/4 of buffer
        if needed > capacity / 4 {
            return false;
        }

        // Write with wraparound
        self.write_bytes(&json);
        self.seq += 1;
        true
    }

    /// Write bytes to the buffer, handling wraparound.
    fn write_bytes(&mut self, data: &[u8]) {
        let capacity = self.buffer.len();
        let mut remaining = data;
        let mut pos = self.write_pos;

        while !remaining.is_empty() {
            let space = capacity - pos;
            let to_write = remaining.len().min(space);
            self.buffer[pos..pos + to_write].copy_from_slice(&remaining[..to_write]);
            remaining = &remaining[to_write..];
            pos = (pos + to_write) % capacity;
        }

        // Update write position
        let new_write_pos = pos;

        // If we've caught up to read position, advance read position
        // to skip overwritten data (find next newline)
        if Self::would_overwrite(self.write_pos, new_write_pos, self.read_pos) {
            self.read_pos = self.find_next_entry(new_write_pos);
        }

        self.write_pos = new_write_pos;
    }

    /// Check if writing from `old_write` to `new_write` would overwrite `read_pos`.
    fn would_overwrite(old_write: usize, new_write: usize, read: usize) -> bool {
        if old_write <= new_write {
            // Normal case: no wraparound in this write
            read > old_write && read <= new_write
        } else {
            // Wraparound case
            read > old_write || read <= new_write
        }
    }

    /// Find the next entry start after a position (next byte after newline).
    fn find_next_entry(&self, start: usize) -> usize {
        let capacity = self.buffer.len();
        let mut pos = start;
        let mut checked = 0;

        while checked < capacity {
            if self.buffer[pos] == b'\n' {
                return (pos + 1) % capacity;
            }
            pos = (pos + 1) % capacity;
            checked += 1;
        }

        // No newline found, reset to write position
        self.write_pos
    }

    /// Get number of bytes available for reading.
    pub fn available(&self) -> usize {
        let capacity = self.buffer.len();
        if self.write_pos >= self.read_pos {
            self.write_pos - self.read_pos
        } else {
            capacity - self.read_pos + self.write_pos
        }
    }

    /// Drain available bytes into the output buffer.
    ///
    /// Returns number of bytes written.
    pub fn drain(&mut self, out: &mut [u8]) -> usize {
        let available = self.available();
        if available == 0 || out.is_empty() {
            return 0;
        }

        let to_read = available.min(out.len());
        let capacity = self.buffer.len();
        let mut written = 0;

        // Read up to end of buffer or to_read bytes
        let first_chunk = (capacity - self.read_pos).min(to_read);
        out[..first_chunk]
            .copy_from_slice(&self.buffer[self.read_pos..self.read_pos + first_chunk]);
        written += first_chunk;
        self.read_pos = (self.read_pos + first_chunk) % capacity;

        // If we wrapped and need more
        if written < to_read && self.read_pos == 0 {
            let second_chunk = to_read - written;
            out[written..written + second_chunk].copy_from_slice(&self.buffer[..second_chunk]);
            written += second_chunk;
            self.read_pos = second_chunk;
        }

        written
    }

    /// Check if buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.available() == 0
    }

    /// Get the sequence number of the next entry.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.read_pos = 0;
        self.write_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_entry_serialization() {
        let entry = LogEntry::session_start("sess_123", vec!["tool-call".to_string()]);
        let json = entry.to_jsonl();
        assert!(json.contains("session_start"));
        assert!(json.contains("sess_123"));
        assert!(json.contains("tool-call"));
    }

    #[test]
    fn test_audit_log() {
        let mut log = AuditLog::new();
        log.log(LogEntry::session_start("sess_1", vec![]));
        log.log(LogEntry::shell(
            "sess_1",
            "ls",
            vec!["/tools".to_string()],
            0,
        ));

        assert_eq!(log.len(), 2);

        let jsonl = log.to_jsonl();
        assert!(jsonl.contains("session_start"));
        assert!(jsonl.contains("shell"));
    }

    #[test]
    fn test_tool_call_hashing() {
        let entry = LogEntry::tool_call(
            "sess_1",
            "stripe.charge",
            &serde_json::json!({"amount": 5000, "currency": "USD"}),
            true,
            None,
        );

        let json = entry.to_jsonl();
        // Should contain hash, not raw params
        assert!(json.contains("params_hash"));
        assert!(!json.contains("5000"));
    }

    #[test]
    fn test_filter_by_session() {
        let mut log = AuditLog::new();
        log.log(LogEntry::session_start("sess_1", vec![]));
        log.log(LogEntry::session_start("sess_2", vec![]));
        log.log(LogEntry::shell("sess_1", "ls", vec![], 0));

        let sess_1_entries = log.filter_by_session("sess_1");
        assert_eq!(sess_1_entries.len(), 2);

        let sess_2_entries = log.filter_by_session("sess_2");
        assert_eq!(sess_2_entries.len(), 1);
    }

    #[test]
    fn test_take_entries() {
        let mut log = AuditLog::new();
        log.log(LogEntry::session_start("sess_1", vec![]));
        log.log(LogEntry::session_end("sess_1"));

        assert_eq!(log.len(), 2);

        let entries = log.take();
        assert_eq!(entries.len(), 2);
        assert!(log.is_empty());
    }

    #[test]
    fn test_custom_event() {
        let entry = LogEntry::custom(
            "sess_1",
            "user_action",
            serde_json::json!({"button": "submit", "form": "checkout"}),
        );

        let json = entry.to_jsonl();
        assert!(json.contains("custom"));
        assert!(json.contains("user_action"));
        assert!(json.contains("checkout"));
    }

    #[test]
    fn test_constraint_violation() {
        let entry = LogEntry::constraint_violation(
            "sess_1",
            "stripe.charge",
            "amount <= 10000",
            "amount = 50000",
        );

        let json = entry.to_jsonl();
        assert!(json.contains("constraint_violation"));
        assert!(json.contains("amount <= 10000"));
    }

    #[test]
    fn test_hash_deterministic() {
        let hash1 = hash_string("test value");
        let hash2 = hash_string("test value");
        assert_eq!(hash1, hash2);

        let hash3 = hash_string("different value");
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_log_level_filtering() {
        // Info level should filter out Debug entries
        let mut log = AuditLog::with_level(LogLevel::Info);

        // Debug entry (FileRead) - should be filtered
        log.log(LogEntry::file_read("sess_1", "/test"));
        assert_eq!(log.len(), 0, "Debug entry should be filtered at Info level");

        // Info entry (Shell) - should be kept
        log.log(LogEntry::shell("sess_1", "ls", vec![], 0));
        assert_eq!(log.len(), 1, "Info entry should be kept at Info level");

        // Warn entry (ConstraintViolation) - should be kept
        log.log(LogEntry::constraint_violation(
            "sess_1", "test", "x <= 100", "x = 200",
        ));
        assert_eq!(log.len(), 2, "Warn entry should be kept at Info level");
    }

    #[test]
    fn test_log_level_debug() {
        // Debug level should keep everything
        let mut log = AuditLog::with_level(LogLevel::Debug);

        log.log(LogEntry::file_read("sess_1", "/test"));
        log.log(LogEntry::shell("sess_1", "ls", vec![], 0));

        assert_eq!(log.len(), 2, "Debug level should keep all entries");
    }

    #[test]
    fn test_log_level_warn() {
        // Warn level should filter out Debug and Info
        let mut log = AuditLog::with_level(LogLevel::Warn);

        log.log(LogEntry::file_read("sess_1", "/test")); // Debug - filtered
        log.log(LogEntry::shell("sess_1", "ls", vec![], 0)); // Info - filtered
        log.log(LogEntry::constraint_violation(
            "sess_1", "test", "x <= 100", "x = 200",
        )); // Warn - kept

        assert_eq!(log.len(), 1, "Only Warn+ entries should be kept");
    }

    #[test]
    fn test_log_always_bypasses_filter() {
        let mut log = AuditLog::with_level(LogLevel::Error);

        // Debug entry should be filtered normally
        log.log(LogEntry::file_read("sess_1", "/test"));
        assert_eq!(log.len(), 0);

        // log_always bypasses filter
        log.log_always(LogEntry::file_read("sess_1", "/critical"));
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn test_entry_levels() {
        // Verify correct level assignment for each entry type
        assert_eq!(
            LogEntry::file_read("s", "/p").level(),
            LogLevel::Debug,
            "FileRead should be Debug"
        );
        assert_eq!(
            LogEntry::memory_read("s", "k", true).level(),
            LogLevel::Debug,
            "MemoryRead should be Debug"
        );
        assert_eq!(
            LogEntry::session_start("s", vec![]).level(),
            LogLevel::Info,
            "SessionStart should be Info"
        );
        assert_eq!(
            LogEntry::shell("s", "ls", vec![], 0).level(),
            LogLevel::Info,
            "Shell should be Info"
        );
        assert_eq!(
            LogEntry::constraint_violation("s", "o", "c", "a").level(),
            LogLevel::Warn,
            "ConstraintViolation should be Warn"
        );
        assert_eq!(
            LogEntry::js_end("s", false, Some("err".to_string())).level(),
            LogLevel::Error,
            "Failed JsEnd should be Error"
        );
        assert_eq!(
            LogEntry::js_end("s", true, None).level(),
            LogLevel::Info,
            "Successful JsEnd should be Info"
        );
        assert_eq!(
            LogEntry::tool_call(
                "s",
                "t",
                &serde_json::json!({}),
                false,
                Some("err".to_string())
            )
            .level(),
            LogLevel::Error,
            "Failed ToolCall should be Error"
        );
        assert_eq!(
            LogEntry::tool_call("s", "t", &serde_json::json!({}), true, None).level(),
            LogLevel::Info,
            "Successful ToolCall should be Info"
        );
    }

    #[test]
    fn test_js_start_entry() {
        let entry = LogEntry::js_start("sess_1", "console.log('hello');");
        let json = entry.to_jsonl();

        assert!(json.contains("js_start"));
        assert!(json.contains("sess_1"));
        assert!(json.contains("code_hash"));
        // Code should be hashed, not stored raw
        assert!(!json.contains("console.log"));

        // Verify session_id accessor
        assert_eq!(entry.session_id(), "sess_1");

        // JsStart should be Info level
        assert_eq!(entry.level(), LogLevel::Info);
    }

    #[test]
    fn test_js_end_entry() {
        // Test successful completion
        let success_entry = LogEntry::js_end("sess_1", true, None);
        let json = success_entry.to_jsonl();
        assert!(json.contains("js_end"));
        assert!(json.contains("\"success\":true"));
        assert_eq!(success_entry.session_id(), "sess_1");

        // Test failed completion with error
        let error_entry = LogEntry::js_end("sess_2", false, Some("ReferenceError".to_string()));
        let json = error_entry.to_jsonl();
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("ReferenceError"));
    }

    #[test]
    fn test_memory_write_entry() {
        let entry = LogEntry::memory_write("sess_1", "user_prefs", 1024);
        let json = entry.to_jsonl();

        assert!(json.contains("memory_write"));
        assert!(json.contains("sess_1"));
        assert!(json.contains("user_prefs"));
        assert!(json.contains("1024"));

        assert_eq!(entry.session_id(), "sess_1");
        assert_eq!(entry.level(), LogLevel::Info);
    }

    #[test]
    fn test_memory_delete_entry() {
        let entry = LogEntry::memory_delete("sess_1", "temp_cache");
        let json = entry.to_jsonl();

        assert!(json.contains("memory_delete"));
        assert!(json.contains("sess_1"));
        assert!(json.contains("temp_cache"));

        assert_eq!(entry.session_id(), "sess_1");
        assert_eq!(entry.level(), LogLevel::Info);
    }

    #[test]
    fn test_spawn_entry() {
        let entry = LogEntry::spawn(
            "parent_sess",
            "child_sess_001",
            vec!["amount <= 1000".to_string(), "read_only".to_string()],
        );
        let json = entry.to_jsonl();

        assert!(json.contains("spawn"));
        assert!(json.contains("parent_sess"));
        assert!(json.contains("child_sess_001"));
        assert!(json.contains("amount <= 1000"));
        assert!(json.contains("read_only"));

        assert_eq!(entry.session_id(), "parent_sess");
        assert_eq!(entry.level(), LogLevel::Info);
    }

    #[test]
    fn test_file_write_entry() {
        let entry = LogEntry::file_write("sess_1", "/data/output.json", 4096);
        let json = entry.to_jsonl();

        assert!(json.contains("file_write"));
        assert!(json.contains("sess_1"));
        assert!(json.contains("/data/output.json"));
        assert!(json.contains("4096"));

        assert_eq!(entry.session_id(), "sess_1");
        assert_eq!(entry.level(), LogLevel::Info);
    }

    #[test]
    fn test_audit_log_entries_accessor() {
        let mut log = AuditLog::new();
        log.log(LogEntry::session_start("sess_1", vec!["cap1".to_string()]));
        log.log(LogEntry::shell(
            "sess_1",
            "echo",
            vec!["hello".to_string()],
            0,
        ));

        let entries = log.entries();
        assert_eq!(entries.len(), 2);

        // First entry should be SessionStart
        match &entries[0] {
            LogEntry::SessionStart { session_id, .. } => {
                assert_eq!(session_id, "sess_1");
            }
            _ => panic!("Expected SessionStart"),
        }

        // Second entry should be Shell
        match &entries[1] {
            LogEntry::Shell { command, .. } => {
                assert_eq!(command, "echo");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_audit_log_clear() {
        let mut log = AuditLog::new();
        log.log(LogEntry::session_start("sess_1", vec![]));
        log.log(LogEntry::session_end("sess_1"));

        assert_eq!(log.len(), 2);
        assert!(!log.is_empty());

        log.clear();

        assert_eq!(log.len(), 0);
        assert!(log.is_empty());
        assert!(log.entries().is_empty());
    }

    #[test]
    fn test_all_entry_types_session_id() {
        // Ensure session_id() works for every variant
        let entries = vec![
            LogEntry::session_start("s1", vec![]),
            LogEntry::session_end("s2"),
            LogEntry::shell("s3", "ls", vec![], 0),
            LogEntry::js_start("s4", "code"),
            LogEntry::js_end("s5", true, None),
            LogEntry::tool_call("s6", "tool", &serde_json::json!({}), true, None),
            LogEntry::memory_read("s7", "key", true),
            LogEntry::memory_write("s8", "key", 100),
            LogEntry::memory_delete("s9", "key"),
            LogEntry::spawn("s10", "child", vec![]),
            LogEntry::constraint_violation("s11", "op", "c", "a"),
            LogEntry::file_read("s12", "/path"),
            LogEntry::file_write("s13", "/path", 50),
            LogEntry::custom("s14", "evt", serde_json::json!({})),
        ];

        for (i, entry) in entries.iter().enumerate() {
            let expected = format!("s{}", i + 1);
            assert_eq!(
                entry.session_id(),
                expected,
                "session_id mismatch for entry type"
            );
        }
    }

    #[test]
    fn test_hash_string_length_and_format() {
        let hash = hash_string("test input");

        // Hash should be exactly 16 hex characters
        assert_eq!(hash.len(), 16);

        // All characters should be valid hex
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_log_level_ordering() {
        // Verify PartialOrd works correctly for filtering
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);

        // Test >= used in log filtering
        assert!(LogLevel::Info >= LogLevel::Info);
        assert!(LogLevel::Warn >= LogLevel::Info);
        assert!(LogLevel::Debug < LogLevel::Info);
    }

    #[test]
    fn test_log_level_default() {
        let level: LogLevel = LogLevel::default();
        assert_eq!(level, LogLevel::Info);
    }

    #[test]
    fn test_audit_log_default() {
        let log: AuditLog = AuditLog::default();
        assert_eq!(log.level, LogLevel::Info);
        assert!(log.is_empty());
    }

    #[test]
    fn test_log_entry_deserialization() {
        // Test round-trip serialization
        let original = LogEntry::tool_call(
            "sess_1",
            "api.call",
            &serde_json::json!({"key": "value"}),
            true,
            None,
        );

        let json = serde_json::to_string(&original).unwrap();
        let restored: LogEntry = serde_json::from_str(&json).unwrap();

        assert_eq!(original.session_id(), restored.session_id());
        match (&original, &restored) {
            (
                LogEntry::ToolCall {
                    tool: t1,
                    success: s1,
                    ..
                },
                LogEntry::ToolCall {
                    tool: t2,
                    success: s2,
                    ..
                },
            ) => {
                assert_eq!(t1, t2);
                assert_eq!(s1, s2);
            }
            _ => panic!("Type mismatch after deserialization"),
        }
    }

    #[test]
    fn test_log_level_serialization() {
        // Test LogLevel serialization
        let level = LogLevel::Warn;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, "\"warn\"");

        let restored: LogLevel = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, LogLevel::Warn);
    }

    #[test]
    fn test_memory_read_found_variants() {
        // Test both found=true and found=false
        let found_entry = LogEntry::memory_read("sess_1", "existing_key", true);
        let json = found_entry.to_jsonl();
        assert!(json.contains("\"found\":true"));

        let not_found_entry = LogEntry::memory_read("sess_1", "missing_key", false);
        let json = not_found_entry.to_jsonl();
        assert!(json.contains("\"found\":false"));
    }

    #[test]
    fn test_spawn_empty_attenuations() {
        // Spawn with no attenuations
        let entry = LogEntry::spawn("parent", "child", vec![]);
        let json = entry.to_jsonl();
        assert!(json.contains("\"attenuations\":[]"));
    }

    #[test]
    fn test_entry_levels_complete() {
        // Test remaining entry types for level assignment
        assert_eq!(
            LogEntry::session_end("s").level(),
            LogLevel::Info,
            "SessionEnd should be Info"
        );
        assert_eq!(
            LogEntry::js_start("s", "code").level(),
            LogLevel::Info,
            "JsStart should be Info"
        );
        assert_eq!(
            LogEntry::memory_write("s", "k", 100).level(),
            LogLevel::Info,
            "MemoryWrite should be Info"
        );
        assert_eq!(
            LogEntry::memory_delete("s", "k").level(),
            LogLevel::Info,
            "MemoryDelete should be Info"
        );
        assert_eq!(
            LogEntry::file_write("s", "/p", 100).level(),
            LogLevel::Info,
            "FileWrite should be Info"
        );
        assert_eq!(
            LogEntry::spawn("s", "c", vec![]).level(),
            LogLevel::Info,
            "Spawn should be Info"
        );
        assert_eq!(
            LogEntry::custom("s", "e", serde_json::json!({})).level(),
            LogLevel::Info,
            "Custom should be Info"
        );
    }

    #[test]
    fn test_content_preview_empty() {
        assert_eq!(content_preview(&[], 10), "(0 bytes)");
    }

    #[test]
    fn test_content_preview_short_text() {
        let data = b"Hello";
        let preview = content_preview(data, 10);
        // Debug format includes quotes: "Hello"
        assert!(preview.contains("\"Hello\""));
        assert!(preview.contains("5 bytes"));
    }

    #[test]
    fn test_content_preview_long_text() {
        let data = b"Hello, this is a longer message that should be truncated!";
        let preview = content_preview(data, 10);
        // Should show first 10 and last 10 chars with ellipsis
        assert!(preview.contains("..."));
        assert!(preview.contains(&format!("{} bytes", data.len())));
    }

    #[test]
    fn test_content_preview_binary() {
        // PNG header bytes (binary data)
        let data: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        let preview = content_preview(data, 10);
        // Should show hex bytes
        assert!(preview.contains('['));
        assert!(preview.contains("89"));
        assert!(preview.contains("8 bytes"));
    }

    #[test]
    fn test_content_preview_long_binary() {
        // Create binary data longer than 20 bytes
        let data: Vec<u8> = (0..50).collect();
        let preview = content_preview(&data, 10);
        // Should show first 10 and last 10 hex bytes with ellipsis
        assert!(preview.contains("..."));
        assert!(preview.contains("00 01 02")); // First bytes
        assert!(preview.contains("31")); // Last byte (49 = 0x31)
        assert!(preview.contains("50 bytes"));
    }

    #[test]
    fn test_stream_chunk_uses_content_preview() {
        let data = b"Hello, world! This is a test message for the stream chunk.";
        let entry = LogEntry::stream_chunk("sess_1", 42, "stdout", data);

        if let LogEntry::StreamChunk {
            preview,
            size_bytes,
            ..
        } = entry
        {
            assert!(preview.is_some());
            let p = preview.unwrap();
            assert!(p.contains(&format!("{} bytes", data.len())));
            assert_eq!(size_bytes, data.len());
        } else {
            panic!("Expected StreamChunk");
        }
    }
}
