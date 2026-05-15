//! Host operation types for sandboxed command execution.
//!
//! Commands running in WASM cannot directly access system resources.
//! They request operations via [`HostOpRequest`] and receive results
//! via [`HostOpResponse`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{CommandHandle, RuntimeId};

/// Unique identifier for a host operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostOpId(pub u64);

impl From<u64> for HostOpId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<HostOpId> for u64 {
    fn from(id: HostOpId) -> u64 {
        id.0
    }
}

/// A request from a command to the host.
///
/// The host executes these operations and returns results via [`HostOpResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostOpRequest {
    // =========================================================================
    // Time
    // =========================================================================
    /// Request wakeup at or after a specific deadline.
    ///
    /// The host should complete this operation when system time reaches
    /// or exceeds the deadline. Response must include current time.
    WakeAt {
        /// Deadline in nanoseconds since Unix epoch.
        deadline_nanos: u64,
    },

    // =========================================================================
    // Tool Calls
    // =========================================================================
    /// Call an external tool (MCP tool, API endpoint, etc).
    ///
    /// The host validates this against session capabilities before execution.
    ToolCall {
        /// Tool identifier (e.g., "stripe:charge", "notion:search").
        tool: String,
        /// Tool parameters as JSON.
        params: Value,
    },

    // =========================================================================
    // VFS (for mapped/lazy-loaded files)
    // =========================================================================
    /// Read from a mapped file.
    ///
    /// Used for lazy-loading file content from the host. The host provides
    /// the actual file data when requested. In-memory VFS files are read
    /// synchronously; this is only for mapped/external sources.
    VfsRead {
        /// File path.
        path: String,
        /// Byte offset to start reading.
        offset: u64,
        /// Maximum bytes to read.
        len: usize,
    },

    // =========================================================================
    // Streaming I/O
    // =========================================================================
    /// Streamed output from a command (stdout/stderr).
    ///
    /// This is emitted as output is produced, enabling real-time streaming.
    /// Stream values: 1 = stdout, 2 = stderr.
    ///
    /// Note: The `runtime_id` and command are provided by the containing `PendingHostOp`,
    /// determined automatically via `task_id` tracking.
    Output {
        /// Stream type (1 = stdout, 2 = stderr).
        stream: u8,
        /// Output data.
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        /// Human-readable preview of the data for display.
        /// Format: "first10"..."last10" (N bytes) for text, or [hex...hex] (N bytes) for binary.
        preview: String,
    },

    /// Command has exited.
    ///
    /// Notifies the host that a command has completed with an exit code.
    ///
    /// Note: The `runtime_id` and command are provided by the containing `PendingHostOp`,
    /// determined automatically via `task_id` tracking.
    CommandExit {
        /// Exit code (0 = success).
        code: i32,
        /// Wall clock elapsed time (ns) from command creation to exit.
        #[serde(skip_serializing_if = "Option::is_none")]
        elapsed_ns: Option<u64>,
        /// Time (ns) spent in WASM execution (inside `step()` calls).
        /// Host can calculate `sys_time` = `elapsed_ns` - `user_time_ns`.
        #[serde(skip_serializing_if = "Option::is_none")]
        user_time_ns: Option<u64>,
    },

    /// Read from stdin.
    ///
    /// Requests stdin data from the host. The host should respond with
    /// `StdinData` containing the input, or empty data for EOF.
    ///
    /// Note: The `runtime_id` and command are provided by the containing `PendingHostOp`,
    /// determined automatically via `task_id` tracking.
    ReadStdin {
        /// Maximum bytes to read.
        max_bytes: usize,
    },

    // =========================================================================
    // Delegation (PIC Protocol)
    // =========================================================================
    /// Request delegation to a new executor.
    ///
    /// This is an opaque request per PIC spec. The host layer is responsible for:
    /// 1. Routing this to a CTA (local or remote)
    /// 2. Returning the new PCA or an error
    ///
    /// The runtime treats all delegation data as opaque bytes - it doesn't
    /// parse or validate the contents. This keeps the host layer simple.
    ///
    /// # Data Format (per PIC spec)
    ///
    /// The `submission` contains a CBOR-encoded `CtaSubmission`:
    /// - `parent_pca`: The current PCA being continued
    /// - `proof_of_continuity`: Proof the sender is the designated executor
    /// - `continuation_request`: New capabilities, executor, and expiry
    ///
    /// # Response
    ///
    /// Host responds with `DelegateResult` containing either:
    /// - `new_pca`: CBOR bytes of the new PCA (on success)
    /// - Error via `HostOpError` (on failure)
    Delegate {
        /// CTA submission as opaque CBOR bytes.
        #[serde(with = "base64_bytes")]
        submission: Vec<u8>,
    },
}

/// Response from the host to a command's operation request.
///
/// # Host Implementation Notes
///
/// **CRITICAL**: Field names in JSON responses MUST exactly match the Rust field names.
/// The serde deserializer will silently ignore unknown fields and use defaults,
/// which can cause the runtime to hang indefinitely waiting for a valid response.
///
/// All responses use `snake_case` for the `type` tag (e.g., `"woke_at"`, `"tool_result"`).
/// See individual variant docs for exact JSON format examples.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostOpResponse {
    // =========================================================================
    // Time
    // =========================================================================
    /// Wakeup completed.
    ///
    /// Returns the current time so WASM can update its internal clock.
    ///
    /// **Host implementation note**: The JSON response MUST use the exact field name
    /// `current_time_nanos` (not `time_ns` or other variations). Example:
    /// ```json
    /// {"type": "woke_at", "current_time_nanos": 1704067200000000000}
    /// ```
    /// Using incorrect field names will cause the runtime to stay blocked forever
    /// as the deserializer silently fails to parse the response.
    WokeAt {
        /// Current time in nanoseconds since Unix epoch.
        current_time_nanos: u64,
    },

    // =========================================================================
    // Tool Calls
    // =========================================================================
    /// Tool call result (atomic, for small results).
    ///
    /// Use this for tool results that fit within the submit buffer (~8KB).
    /// For larger results, use [`HostOpResponse::ToolResultChunk`] to stream the data.
    ToolResult {
        /// Result value from the tool.
        result: Value,
    },

    /// Chunked tool call result (for large results).
    ///
    /// Use this when tool results exceed the submit buffer size. The host
    /// sends multiple chunks, each with `eof: false`, until the final chunk
    /// which has `eof: true`. The runtime accumulates chunks and resolves
    /// the Promise only when the final chunk is received.
    ///
    /// # Protocol
    ///
    /// 1. Host executes tool, result is large (e.g., 50KB JSON)
    /// 2. Host serializes result to bytes, splits into chunks (e.g., 2KB each)
    /// 3. Host submits each chunk with same operation ID:
    ///    - `ToolResultChunk { data: chunk1, eof: false }`
    ///    - `ToolResultChunk { data: chunk2, eof: false }`
    ///    - ...
    ///    - `ToolResultChunk { data: chunkN, eof: true }`
    /// 4. Runtime accumulates chunks in buffer
    /// 5. On `eof: true`, runtime completes the operation with accumulated bytes
    ///
    /// # Ordering
    ///
    /// **Host MUST send chunks in order.** Out-of-order delivery will corrupt
    /// the result. The runtime does not reorder chunks.
    ///
    /// # Buffer Limits
    ///
    /// The runtime enforces a maximum accumulated size (default 10MB).
    /// If exceeded, the operation fails with an error.
    ///
    /// # Example JSON
    ///
    /// ```json
    /// {"type": "tool_result_chunk", "data": "eyJrZXkiOiAidmFsdWUi...", "eof": false}
    /// {"type": "tool_result_chunk", "data": "Li4ufQ==", "eof": true}
    /// ```
    ToolResultChunk {
        /// Chunk data (partial result bytes, base64-encoded).
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        /// True if this is the final chunk.
        #[serde(default)]
        eof: bool,
    },

    /// Abort a chunked tool result stream.
    ///
    /// Use this to signal that a tool call failed after some chunks were
    /// already sent. The runtime will discard any accumulated chunks and
    /// complete the operation with an error.
    ///
    /// # Example JSON
    ///
    /// ```json
    /// {"type": "tool_result_error", "message": "Connection reset while streaming"}
    /// ```
    ToolResultError {
        /// Error message describing why the tool call failed.
        message: String,
    },

    // =========================================================================
    // VFS
    // =========================================================================
    /// Data read from a mapped file.
    VfsData {
        /// Bytes read.
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },

    // =========================================================================
    // Streaming I/O
    // =========================================================================
    /// Output was received by host.
    OutputAck,

    /// Command exit was acknowledged.
    ExitAck,

    /// Stdin data from the host.
    ///
    /// Response to `ReadStdin` request.
    StdinData {
        /// Input data.
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        /// True if this is the last data and stdin is now closed (EOF).
        /// Can be true with non-empty data (last chunk) or with empty data (just EOF).
        #[serde(default)]
        eof: bool,
    },

    // =========================================================================
    // Delegation (PIC Protocol)
    // =========================================================================
    /// Delegation result.
    ///
    /// Response to `Delegate` request. Contains the new PCA on success.
    DelegateResult {
        /// New PCA as opaque CBOR bytes (on success).
        #[serde(with = "base64_bytes")]
        new_pca: Vec<u8>,
    },

    // =========================================================================
    // Errors
    // =========================================================================
    /// Operation failed.
    Error {
        /// Error code (e.g., `not_found`, `permission_denied`, `timeout`).
        code: String,
        /// Human-readable error message.
        message: String,
    },
}

impl HostOpResponse {
    /// Create an error response.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Create a `woke_at` response.
    pub fn woke_at(current_time_nanos: u64) -> Self {
        Self::WokeAt { current_time_nanos }
    }

    /// Create a tool result response.
    pub fn tool_result(result: Value) -> Self {
        Self::ToolResult { result }
    }

    /// Create a chunked tool result response.
    ///
    /// Use this for streaming large tool results. Send multiple chunks with
    /// `eof: false`, then a final chunk with `eof: true`.
    pub fn tool_result_chunk(data: Vec<u8>, eof: bool) -> Self {
        Self::ToolResultChunk { data, eof }
    }

    /// Create a tool result error response.
    ///
    /// Use this to abort a chunked tool result stream when an error occurs
    /// after some chunks have already been sent.
    pub fn tool_result_error(message: impl Into<String>) -> Self {
        Self::ToolResultError {
            message: message.into(),
        }
    }

    /// Check if this is an error response.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
}

// =============================================================================
// Structured Host Error
// =============================================================================

/// Standard error codes for host operations.
///
/// These codes provide semantic meaning for error handling and map directly
/// to `std::io::ErrorKind`. Using this enum instead of raw strings ensures:
/// - Type safety: compiler catches typos
/// - Consistency: all code uses the same codes
/// - Documentation: codes are self-documenting
///
/// Serializes as a lowercase `snake_case` string for JSON compatibility.
///
/// # Example
///
/// ```
/// use amla_sandbox::HostErrorCode;
///
/// // Create error responses with type-safe codes
/// let code = HostErrorCode::NotFound;
/// assert_eq!(code.as_str(), "not_found");
/// assert_eq!(code.to_io_error_kind(), std::io::ErrorKind::NotFound);
///
/// // Parse from string (for JSON deserialization)
/// let parsed: HostErrorCode = "permission_denied".into();
/// assert!(matches!(parsed, HostErrorCode::PermissionDenied));
///
/// // Unknown codes are preserved
/// let custom: HostErrorCode = "my_custom_error".into();
/// assert!(matches!(custom, HostErrorCode::Other(_)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostErrorCode {
    /// Resource not found (file, endpoint, etc.).
    NotFound,
    /// Permission denied (authorization failure).
    PermissionDenied,
    /// Operation timed out.
    Timeout,
    /// Connection was refused.
    ConnectionRefused,
    /// Connection was reset.
    ConnectionReset,
    /// Resource already exists.
    AlreadyExists,
    /// Invalid input provided.
    InvalidInput,
    /// Data format is invalid.
    InvalidData,
    /// Operation would block.
    WouldBlock,
    /// Operation was interrupted.
    Interrupted,
    /// Operation is not supported.
    Unsupported,
    /// Out of memory.
    OutOfMemory,
    /// Internal error.
    Internal,
    /// Unknown or custom error code.
    ///
    /// Used for error codes not in the standard set.
    /// The original code string is preserved.
    #[serde(untagged)]
    Other(String),
}

impl HostErrorCode {
    /// Get the string representation of this error code.
    pub fn as_str(&self) -> &str {
        match self {
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::Timeout => "timeout",
            Self::ConnectionRefused => "connection_refused",
            Self::ConnectionReset => "connection_reset",
            Self::AlreadyExists => "already_exists",
            Self::InvalidInput => "invalid_input",
            Self::InvalidData => "invalid_data",
            Self::WouldBlock => "would_block",
            Self::Interrupted => "interrupted",
            Self::Unsupported => "unsupported",
            Self::OutOfMemory => "out_of_memory",
            Self::Internal => "internal",
            Self::Other(s) => s,
        }
    }

    /// Map to the corresponding `std::io::ErrorKind`.
    pub fn to_io_error_kind(&self) -> std::io::ErrorKind {
        match self {
            Self::NotFound => std::io::ErrorKind::NotFound,
            Self::PermissionDenied => std::io::ErrorKind::PermissionDenied,
            Self::Timeout => std::io::ErrorKind::TimedOut,
            Self::ConnectionRefused => std::io::ErrorKind::ConnectionRefused,
            Self::ConnectionReset => std::io::ErrorKind::ConnectionReset,
            Self::AlreadyExists => std::io::ErrorKind::AlreadyExists,
            Self::InvalidInput => std::io::ErrorKind::InvalidInput,
            Self::InvalidData => std::io::ErrorKind::InvalidData,
            Self::WouldBlock => std::io::ErrorKind::WouldBlock,
            Self::Interrupted => std::io::ErrorKind::Interrupted,
            Self::Unsupported => std::io::ErrorKind::Unsupported,
            Self::OutOfMemory => std::io::ErrorKind::OutOfMemory,
            Self::Internal | Self::Other(_) => std::io::ErrorKind::Other,
        }
    }
}

impl std::fmt::Display for HostErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl HostErrorCode {
    /// Try to parse a known error code from a string.
    ///
    /// Returns `None` for unknown codes.
    fn try_parse(s: &str) -> Option<Self> {
        match s {
            "not_found" => Some(Self::NotFound),
            "permission_denied" => Some(Self::PermissionDenied),
            "timeout" | "timed_out" => Some(Self::Timeout),
            "connection_refused" => Some(Self::ConnectionRefused),
            "connection_reset" => Some(Self::ConnectionReset),
            "already_exists" => Some(Self::AlreadyExists),
            "invalid_input" | "invalid_argument" => Some(Self::InvalidInput),
            "invalid_data" => Some(Self::InvalidData),
            "would_block" => Some(Self::WouldBlock),
            "interrupted" => Some(Self::Interrupted),
            "unsupported" => Some(Self::Unsupported),
            "out_of_memory" => Some(Self::OutOfMemory),
            "internal" => Some(Self::Internal),
            _ => None,
        }
    }
}

impl From<&str> for HostErrorCode {
    fn from(s: &str) -> Self {
        Self::try_parse(s).unwrap_or_else(|| Self::Other(s.to_string()))
    }
}

impl From<String> for HostErrorCode {
    fn from(s: String) -> Self {
        Self::try_parse(&s).unwrap_or(Self::Other(s))
    }
}

/// Error returned by a host operation.
///
/// This error type preserves the structured error information from the host,
/// including the error code and message. It implements `std::error::Error`
/// so it can be wrapped in `std::io::Error` while remaining accessible
/// via `error.get_ref()` or `error.into_inner()`.
///
/// # Example
///
/// ```ignore
/// // When a host operation fails, the error can be inspected:
/// match result {
///     Err(io_err) => {
///         if let Some(host_err) = io_err.get_ref().and_then(|e| e.downcast_ref::<HostOpError>()) {
///             // Type-safe error code matching
///             match host_err.code() {
///                 HostErrorCode::NotFound => println!("File not found"),
///                 HostErrorCode::PermissionDenied => println!("Access denied"),
///                 _ => println!("Error: {}", host_err.message()),
///             }
///         }
///     }
///     Ok(_) => {}
/// }
/// ```
#[derive(Debug, Clone)]
pub struct HostOpError {
    /// Structured error code.
    code: HostErrorCode,
    /// Human-readable error message.
    message: String,
}

impl HostOpError {
    /// Create a new host operation error with a type-safe code.
    pub fn new(code: impl Into<HostErrorCode>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Create a "not found" error.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(HostErrorCode::NotFound, message)
    }

    /// Create a "permission denied" error.
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(HostErrorCode::PermissionDenied, message)
    }

    /// Create a "timeout" error.
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(HostErrorCode::Timeout, message)
    }

    /// Create an "invalid input" error.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(HostErrorCode::InvalidInput, message)
    }

    /// Create an "unsupported" error.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(HostErrorCode::Unsupported, message)
    }

    /// Create an "internal" error.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(HostErrorCode::Internal, message)
    }

    /// Get the error code.
    pub fn code(&self) -> &HostErrorCode {
        &self.code
    }

    /// Get the error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Map the error code to an appropriate `std::io::ErrorKind`.
    pub fn to_io_error_kind(&self) -> std::io::ErrorKind {
        self.code.to_io_error_kind()
    }

    /// Convert to an `std::io::Error` with appropriate error kind.
    pub fn into_io_error(self) -> std::io::Error {
        let kind = self.to_io_error_kind();
        std::io::Error::new(kind, self)
    }
}

impl std::fmt::Display for HostOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HostOpError {}

/// ID for one-way notifications that don't need response correlation.
///
/// Used for synthetic operations created directly by the runtime (not from
/// the scheduler), such as:
/// - `CommandExit` during cancellation or normal exit
/// - Error messages to stderr
///
/// These differ from scheduler-originated operations (like Print) which have
/// proper IDs and wait for host acknowledgement before the task continues.
///
/// The host will still receive these and may acknowledge them, but the
/// runtime doesn't correlate the response back to the original operation.
pub const NOTIFICATION_ID: u64 = 0;

/// A pending host operation with its ID.
///
/// Contains all information needed to route the result back to the
/// correct runtime and command after the host completes the operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingHostOp {
    /// Unique operation ID for response correlation.
    ///
    /// For one-way notifications (synthetic `CommandExit`, error Output),
    /// use [`NOTIFICATION_ID`] (0). The runtime doesn't correlate responses for these.
    ///
    /// For scheduler-originated operations (Print, `WakeAt`, `VfsRead`, `ToolCall`, etc.),
    /// this ID comes from the scheduler's `allocate_id()` and must match `HostOpResult.id`.
    /// These operations wait for the host acknowledgement before the task continues.
    pub id: u64,
    /// Which runtime this operation belongs to.
    pub runtime_id: RuntimeId,
    /// Which command within the runtime requested this operation.
    ///
    /// `None` if the operation couldn't be attributed to a specific command
    /// (e.g., internal scheduler operations or attribution lookup failed).
    pub command: Option<CommandHandle>,
    /// The operation request.
    pub request: HostOpRequest,
}

/// A completed host operation result.
///
/// The host constructs this from the `PendingHostOp` after completing
/// the operation, including the `runtime_id` for routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostOpResult {
    /// Operation ID (must match a pending operation).
    pub id: u64,
    /// Which runtime this result belongs to.
    pub runtime_id: RuntimeId,
    /// The result.
    pub result: HostOpResponse,
}

/// Serde helper for base64-encoded bytes.
mod base64_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = STANDARD.encode(bytes);
        encoded.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        STANDARD.decode(encoded).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Request Serialization Tests
    // =========================================================================

    #[test]
    fn serialize_wake_at() {
        let req = HostOpRequest::WakeAt {
            deadline_nanos: 1_000_000_000,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"type":"wake_at","deadline_nanos":1000000000}"#);
    }

    #[test]
    fn serialize_wake_at_zero() {
        let req = HostOpRequest::WakeAt { deadline_nanos: 0 };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"type":"wake_at","deadline_nanos":0}"#);
    }

    #[test]
    fn serialize_wake_at_max() {
        let req = HostOpRequest::WakeAt {
            deadline_nanos: u64::MAX,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(&u64::MAX.to_string()));
    }

    #[test]
    fn serialize_tool_call() {
        let req = HostOpRequest::ToolCall {
            tool: "stripe:charge".to_string(),
            params: serde_json::json!({"amount": 500}),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"tool_call""#));
        assert!(json.contains(r#""tool":"stripe:charge""#));
    }

    #[test]
    fn serialize_tool_call_empty_params() {
        let req = HostOpRequest::ToolCall {
            tool: "test".to_string(),
            params: serde_json::json!({}),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""params":{}"#));
    }

    #[test]
    fn serialize_tool_call_null_params() {
        let req = HostOpRequest::ToolCall {
            tool: "test".to_string(),
            params: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""params":null"#));
    }

    #[test]
    fn serialize_tool_call_complex_params() {
        let req = HostOpRequest::ToolCall {
            tool: "api:call".to_string(),
            params: serde_json::json!({
                "nested": {
                    "array": [1, 2, 3],
                    "string": "value"
                },
                "null_field": null,
                "bool": true
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: HostOpRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpRequest::ToolCall { tool, params } => {
                assert_eq!(tool, "api:call");
                assert!(params["nested"]["array"].is_array());
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn serialize_vfs_read() {
        let req = HostOpRequest::VfsRead {
            path: "/data/file.txt".to_string(),
            offset: 100,
            len: 1024,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"vfs_read""#));
        assert!(json.contains(r#""path":"/data/file.txt""#));
        assert!(json.contains(r#""offset":100"#));
        assert!(json.contains(r#""len":1024"#));
    }

    #[test]
    fn serialize_vfs_read_zero_offset() {
        let req = HostOpRequest::VfsRead {
            path: "/file".to_string(),
            offset: 0,
            len: 0,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: HostOpRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpRequest::VfsRead { offset, len, .. } => {
                assert_eq!(offset, 0);
                assert_eq!(len, 0);
            }
            _ => panic!("Expected VfsRead"),
        }
    }

    // =========================================================================
    // Response Serialization Tests
    // =========================================================================

    #[test]
    fn serialize_woke_at_response() {
        let resp = HostOpResponse::woke_at(1_000_000_000);
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(
            json,
            r#"{"type":"woke_at","current_time_nanos":1000000000}"#
        );
    }

    #[test]
    fn serialize_tool_result() {
        let resp = HostOpResponse::tool_result(serde_json::json!({
            "success": true,
            "data": [1, 2, 3]
        }));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"tool_result""#));
        assert!(json.contains(r#""success":true"#));
    }

    #[test]
    fn serialize_tool_result_null() {
        let resp = HostOpResponse::tool_result(serde_json::Value::Null);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""result":null"#));
    }

    #[test]
    fn serialize_vfs_data() {
        let resp = HostOpResponse::VfsData {
            data: b"file content".to_vec(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"vfs_data""#));
        // Should be base64 encoded
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::VfsData { data } => {
                assert_eq!(data, b"file content");
            }
            _ => panic!("Expected VfsData"),
        }
    }

    #[test]
    fn serialize_error_response() {
        let resp = HostOpResponse::error("not_found", "File not found");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"error""#));
        assert!(json.contains(r#""code":"not_found""#));
    }

    #[test]
    fn serialize_error_various_codes() {
        let codes = [
            "not_found",
            "permission_denied",
            "timeout",
            "rate_limited",
            "invalid_params",
            "internal_error",
        ];
        for code in codes {
            let resp = HostOpResponse::error(code, "Test message");
            let json = serde_json::to_string(&resp).unwrap();
            let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
            match parsed {
                HostOpResponse::Error {
                    code: parsed_code, ..
                } => {
                    assert_eq!(parsed_code, code);
                }
                _ => panic!("Expected Error"),
            }
        }
    }

    #[test]
    fn serialize_error_empty_message() {
        let resp = HostOpResponse::error("test", "");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""message":"""#));
    }

    #[test]
    fn serialize_error_unicode_message() {
        let resp = HostOpResponse::error("test", "Error: 文件未找到 🔍");
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::Error { message, .. } => {
                assert!(message.contains("文件未找到"));
                assert!(message.contains("🔍"));
            }
            _ => panic!("Expected Error"),
        }
    }

    // =========================================================================
    // HostOpResponse Helper Methods
    // =========================================================================

    #[test]
    fn error_helper() {
        let resp = HostOpResponse::error("code", "message");
        assert!(resp.is_error());
    }

    #[test]
    fn tool_result_helper() {
        let resp = HostOpResponse::tool_result(serde_json::json!({"key": "value"}));
        assert!(!resp.is_error());
    }

    #[test]
    fn is_error_false_for_success() {
        let responses = [
            HostOpResponse::woke_at(0),
            HostOpResponse::tool_result(serde_json::Value::Null),
            HostOpResponse::VfsData { data: vec![] },
        ];
        for resp in &responses {
            assert!(!resp.is_error(), "Expected is_error() = false for {resp:?}");
        }
    }

    // =========================================================================
    // HostOpId Tests
    // =========================================================================

    #[test]
    fn host_op_id_from_u64() {
        let id: HostOpId = 42u64.into();
        assert_eq!(id.0, 42);
    }

    #[test]
    fn host_op_id_into_u64() {
        let id = HostOpId(42);
        let value: u64 = id.into();
        assert_eq!(value, 42);
    }

    #[test]
    fn host_op_id_equality() {
        let id1 = HostOpId(1);
        let id2 = HostOpId(1);
        let id3 = HostOpId(2);
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn host_op_id_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(HostOpId(1));
        set.insert(HostOpId(2));
        set.insert(HostOpId(1)); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn host_op_id_serialize() {
        let id = HostOpId(999);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "999");
        let parsed: HostOpId = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.0, 999);
    }

    // =========================================================================
    // PendingHostOp Tests
    // =========================================================================

    #[test]
    fn roundtrip_pending_op() {
        let op = PendingHostOp {
            id: 42,
            runtime_id: RuntimeId::new(1),
            command: Some(CommandHandle::new(1)),
            request: HostOpRequest::WakeAt {
                deadline_nanos: 1_000_000_000,
            },
        };
        let json = serde_json::to_string(&op).unwrap();
        let parsed: PendingHostOp = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.runtime_id, RuntimeId::new(1));
        assert_eq!(parsed.command, Some(CommandHandle::new(1)));
    }

    #[test]
    fn pending_op_with_all_request_types() {
        let requests = [
            HostOpRequest::WakeAt {
                deadline_nanos: 1_000_000_000,
            },
            HostOpRequest::ToolCall {
                tool: "test".to_string(),
                params: serde_json::json!(null),
            },
            HostOpRequest::VfsRead {
                path: "/".to_string(),
                offset: 0,
                len: 10,
            },
        ];

        for (i, req) in requests.into_iter().enumerate() {
            let op = PendingHostOp {
                id: i as u64,
                runtime_id: RuntimeId::new(99),
                command: Some(CommandHandle::new(1)),
                request: req,
            };
            let json = serde_json::to_string(&op).unwrap();
            let parsed: PendingHostOp = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.id, i as u64);
            assert_eq!(parsed.runtime_id, RuntimeId::new(99));
        }
    }

    // =========================================================================
    // HostOpResult Tests
    // =========================================================================

    #[test]
    fn roundtrip_result() {
        let result = HostOpResult {
            id: 42,
            runtime_id: RuntimeId::new(1),
            result: HostOpResponse::woke_at(1_000_000_000),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: HostOpResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.runtime_id, RuntimeId::new(1));
    }

    #[test]
    fn result_with_all_response_types() {
        let responses = [
            HostOpResponse::woke_at(0),
            HostOpResponse::woke_at(12345),
            HostOpResponse::tool_result(serde_json::json!({"ok": true})),
            HostOpResponse::VfsData {
                data: b"test".to_vec(),
            },
            HostOpResponse::error("err", "msg"),
        ];

        for (i, resp) in responses.into_iter().enumerate() {
            let result = HostOpResult {
                id: i as u64,
                runtime_id: RuntimeId::new(99),
                result: resp,
            };
            let json = serde_json::to_string(&result).unwrap();
            let parsed: HostOpResult = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.id, i as u64);
            assert_eq!(parsed.runtime_id, RuntimeId::new(99));
        }
    }

    #[test]
    fn result_batch_serialization() {
        let results = vec![
            HostOpResult {
                id: 1,
                runtime_id: RuntimeId::new(10),
                result: HostOpResponse::woke_at(1_000_000_000),
            },
            HostOpResult {
                id: 2,
                runtime_id: RuntimeId::new(10),
                result: HostOpResponse::woke_at(2_000_000_000),
            },
            HostOpResult {
                id: 3,
                runtime_id: RuntimeId::new(20),
                result: HostOpResponse::error("timeout", "Request timed out"),
            },
        ];
        let json = serde_json::to_string(&results).unwrap();
        let parsed: Vec<HostOpResult> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].id, 1);
        assert_eq!(parsed[0].runtime_id, RuntimeId::new(10));
        assert_eq!(parsed[1].id, 2);
        assert_eq!(parsed[1].runtime_id, RuntimeId::new(10));
        assert_eq!(parsed[2].id, 3);
        assert_eq!(parsed[2].runtime_id, RuntimeId::new(20));
    }

    // =========================================================================
    // Edge Cases and Error Handling
    // =========================================================================

    #[test]
    fn deserialize_invalid_type() {
        let json = r#"{"type":"unknown_type"}"#;
        let result: Result<HostOpRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_missing_type() {
        let json = r#"{"millis":1000}"#;
        let result: Result<HostOpRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_missing_required_field() {
        let json = r#"{"type":"wake_at"}"#; // missing deadline_nanos
        let result: Result<HostOpRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_wrong_field_type() {
        let json = r#"{"type":"wake_at","deadline_nanos":"not a number"}"#;
        let result: Result<HostOpRequest, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn special_characters_in_paths() {
        let req = HostOpRequest::VfsRead {
            path: "/path/with spaces/and\"quotes\"/file.txt".to_string(),
            offset: 0,
            len: 100,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: HostOpRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpRequest::VfsRead { path, .. } => {
                assert!(path.contains("spaces"));
                assert!(path.contains("quotes"));
            }
            _ => panic!("Expected VfsRead"),
        }
    }

    #[test]
    fn unicode_in_tool_name() {
        let req = HostOpRequest::ToolCall {
            tool: "工具:测试".to_string(),
            params: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: HostOpRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpRequest::ToolCall { tool, .. } => {
                assert_eq!(tool, "工具:测试");
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn large_vfs_data() {
        let large_data = vec![0x42u8; 1024 * 1024]; // 1MB
        let resp = HostOpResponse::VfsData {
            data: large_data.clone(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::VfsData { data } => {
                assert_eq!(data.len(), 1024 * 1024);
                assert!(data.iter().all(|&b| b == 0x42));
            }
            _ => panic!("Expected VfsData"),
        }
    }

    // =========================================================================
    // Delegation Tests
    // =========================================================================

    #[test]
    fn serialize_delegate_request() {
        // Sample CBOR-like submission data
        let submission = vec![0xA2, 0x01, 0x02, 0x03, 0x04]; // Example bytes
        let req = HostOpRequest::Delegate {
            submission: submission.clone(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"delegate""#));
        assert!(json.contains(r#""submission":"#)); // Base64 encoded

        // Roundtrip
        let parsed: HostOpRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpRequest::Delegate {
                submission: parsed_sub,
            } => {
                assert_eq!(parsed_sub, submission);
            }
            _ => panic!("Expected Delegate"),
        }
    }

    #[test]
    fn serialize_delegate_result() {
        // Sample PCA bytes
        let new_pca = vec![0xD9, 0xD9, 0xF7, 0xA3, 0x01, 0x02]; // Example CBOR
        let resp = HostOpResponse::DelegateResult {
            new_pca: new_pca.clone(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"delegate_result""#));
        assert!(json.contains(r#""new_pca":"#)); // Base64 encoded

        // Roundtrip
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::DelegateResult {
                new_pca: parsed_pca,
            } => {
                assert_eq!(parsed_pca, new_pca);
            }
            _ => panic!("Expected DelegateResult"),
        }
    }

    #[test]
    fn delegate_empty_submission() {
        let req = HostOpRequest::Delegate { submission: vec![] };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: HostOpRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpRequest::Delegate { submission } => {
                assert!(submission.is_empty());
            }
            _ => panic!("Expected Delegate"),
        }
    }

    #[test]
    fn delegate_large_submission() {
        // Test with larger data (simulating real CBOR PCA)
        let submission = vec![0x42u8; 4096]; // 4KB
        let req = HostOpRequest::Delegate {
            submission: submission.clone(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: HostOpRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpRequest::Delegate {
                submission: parsed_sub,
            } => {
                assert_eq!(parsed_sub.len(), 4096);
                assert!(parsed_sub.iter().all(|&b| b == 0x42));
            }
            _ => panic!("Expected Delegate"),
        }
    }

    #[test]
    fn pending_op_with_delegate() {
        let op = PendingHostOp {
            id: 99,
            runtime_id: RuntimeId::new(5),
            command: Some(CommandHandle::new(3)),
            request: HostOpRequest::Delegate {
                submission: vec![0x01, 0x02, 0x03],
            },
        };
        let json = serde_json::to_string(&op).unwrap();
        let parsed: PendingHostOp = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 99);
        assert_eq!(parsed.runtime_id, RuntimeId::new(5));
        assert_eq!(parsed.command, Some(CommandHandle::new(3)));
        match parsed.request {
            HostOpRequest::Delegate { submission } => {
                assert_eq!(submission, vec![0x01, 0x02, 0x03]);
            }
            _ => panic!("Expected Delegate"),
        }
    }

    #[test]
    fn result_with_delegate_result() {
        let result = HostOpResult {
            id: 42,
            runtime_id: RuntimeId::new(7),
            result: HostOpResponse::DelegateResult {
                new_pca: vec![0xAB, 0xCD, 0xEF],
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: HostOpResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.runtime_id, RuntimeId::new(7));
        match parsed.result {
            HostOpResponse::DelegateResult { new_pca } => {
                assert_eq!(new_pca, vec![0xAB, 0xCD, 0xEF]);
            }
            _ => panic!("Expected DelegateResult"),
        }
    }

    // =========================================================================
    // HostErrorCode Tests
    // =========================================================================

    #[test]
    fn host_error_code_from_str() {
        // Known codes
        assert!(matches!(
            HostErrorCode::from("not_found"),
            HostErrorCode::NotFound
        ));
        assert!(matches!(
            HostErrorCode::from("permission_denied"),
            HostErrorCode::PermissionDenied
        ));
        assert!(matches!(
            HostErrorCode::from("timeout"),
            HostErrorCode::Timeout
        ));
        assert!(matches!(
            HostErrorCode::from("timed_out"),
            HostErrorCode::Timeout
        )); // Alias
        assert!(matches!(
            HostErrorCode::from("invalid_argument"),
            HostErrorCode::InvalidInput
        )); // Alias

        // Unknown code
        let unknown = HostErrorCode::from("custom_error");
        assert!(matches!(unknown, HostErrorCode::Other(ref s) if s == "custom_error"));
    }

    #[test]
    fn host_error_code_to_io_error_kind() {
        assert_eq!(
            HostErrorCode::NotFound.to_io_error_kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            HostErrorCode::PermissionDenied.to_io_error_kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            HostErrorCode::Timeout.to_io_error_kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(
            HostErrorCode::AlreadyExists.to_io_error_kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            HostErrorCode::Internal.to_io_error_kind(),
            std::io::ErrorKind::Other
        );
        assert_eq!(
            HostErrorCode::Other("custom".to_string()).to_io_error_kind(),
            std::io::ErrorKind::Other
        );
    }

    #[test]
    fn host_error_code_display() {
        assert_eq!(HostErrorCode::NotFound.to_string(), "not_found");
        assert_eq!(
            HostErrorCode::PermissionDenied.to_string(),
            "permission_denied"
        );
        assert_eq!(
            HostErrorCode::Other("custom".to_string()).to_string(),
            "custom"
        );
    }

    // =========================================================================
    // HostOpError Tests
    // =========================================================================

    #[test]
    fn host_op_error_with_code_enum() {
        let err = HostOpError::new(HostErrorCode::NotFound, "File not found");
        assert!(matches!(err.code(), HostErrorCode::NotFound));
        assert_eq!(err.message(), "File not found");
        assert_eq!(err.to_io_error_kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn host_op_error_with_str() {
        // Using &str - gets converted via From<&str>
        let err = HostOpError::new("permission_denied", "Access denied");
        assert!(matches!(err.code(), HostErrorCode::PermissionDenied));
        assert_eq!(err.to_io_error_kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn host_op_error_convenience_constructors() {
        let err = HostOpError::not_found("File not found");
        assert!(matches!(err.code(), HostErrorCode::NotFound));

        let err = HostOpError::permission_denied("Access denied");
        assert!(matches!(err.code(), HostErrorCode::PermissionDenied));

        let err = HostOpError::timeout("Request timed out");
        assert!(matches!(err.code(), HostErrorCode::Timeout));
    }

    #[test]
    fn host_op_error_into_io_error() {
        let err = HostOpError::not_found("File not found");
        let io_err = err.into_io_error();

        assert_eq!(io_err.kind(), std::io::ErrorKind::NotFound);
        assert!(io_err.to_string().contains("not_found"));
        assert!(io_err.to_string().contains("File not found"));

        // Verify we can downcast
        let inner = io_err.get_ref().unwrap();
        let host_err = inner.downcast_ref::<HostOpError>().unwrap();
        assert!(matches!(host_err.code(), HostErrorCode::NotFound));
    }

    #[test]
    fn host_op_error_display() {
        let err = HostOpError::new(HostErrorCode::NotFound, "File not found");
        assert_eq!(err.to_string(), "not_found: File not found");
    }

    // =========================================================================
    // ToolResultChunk Tests
    // =========================================================================

    #[test]
    fn serialize_tool_result_chunk() {
        let resp = HostOpResponse::tool_result_chunk(b"partial data".to_vec(), false);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"tool_result_chunk""#));
        assert!(json.contains(r#""eof":false"#));
        // Data should be base64 encoded
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::ToolResultChunk { data, eof } => {
                assert_eq!(data, b"partial data");
                assert!(!eof);
            }
            _ => panic!("Expected ToolResultChunk"),
        }
    }

    #[test]
    fn serialize_tool_result_chunk_final() {
        let resp = HostOpResponse::tool_result_chunk(b"final chunk".to_vec(), true);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""eof":true"#));
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::ToolResultChunk { data, eof } => {
                assert_eq!(data, b"final chunk");
                assert!(eof);
            }
            _ => panic!("Expected ToolResultChunk"),
        }
    }

    #[test]
    fn serialize_tool_result_chunk_empty() {
        // Empty final chunk (just EOF signal)
        let resp = HostOpResponse::tool_result_chunk(vec![], true);
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::ToolResultChunk { data, eof } => {
                assert!(data.is_empty());
                assert!(eof);
            }
            _ => panic!("Expected ToolResultChunk"),
        }
    }

    #[test]
    fn serialize_tool_result_chunk_large_data() {
        // Simulate a 4KB chunk
        let large_data = vec![0x42u8; 4096];
        let resp = HostOpResponse::tool_result_chunk(large_data.clone(), false);
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::ToolResultChunk { data, eof } => {
                assert_eq!(data.len(), 4096);
                assert_eq!(data, large_data);
                assert!(!eof);
            }
            _ => panic!("Expected ToolResultChunk"),
        }
    }

    #[test]
    fn deserialize_tool_result_chunk_default_eof() {
        // eof should default to false if not present
        let json = r#"{"type":"tool_result_chunk","data":"dGVzdA=="}"#;
        let parsed: HostOpResponse = serde_json::from_str(json).unwrap();
        match parsed {
            HostOpResponse::ToolResultChunk { data, eof } => {
                assert_eq!(data, b"test");
                assert!(!eof); // Default is false
            }
            _ => panic!("Expected ToolResultChunk"),
        }
    }

    // =========================================================================
    // ToolResultError Tests
    // =========================================================================

    #[test]
    fn serialize_tool_result_error() {
        let resp = HostOpResponse::tool_result_error("Connection reset");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"tool_result_error""#));
        assert!(json.contains(r#""message":"Connection reset""#));
    }

    #[test]
    fn serialize_tool_result_error_empty_message() {
        let resp = HostOpResponse::tool_result_error("");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""message":"""#));
    }

    #[test]
    fn serialize_tool_result_error_unicode() {
        let resp = HostOpResponse::tool_result_error("Error: 连接失败 🔌");
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HostOpResponse = serde_json::from_str(&json).unwrap();
        match parsed {
            HostOpResponse::ToolResultError { message } => {
                assert!(message.contains("连接失败"));
                assert!(message.contains("🔌"));
            }
            _ => panic!("Expected ToolResultError"),
        }
    }

    #[test]
    fn deserialize_tool_result_error() {
        let json = r#"{"type":"tool_result_error","message":"Streaming failed mid-transfer"}"#;
        let parsed: HostOpResponse = serde_json::from_str(json).unwrap();
        match parsed {
            HostOpResponse::ToolResultError { message } => {
                assert_eq!(message, "Streaming failed mid-transfer");
            }
            _ => panic!("Expected ToolResultError"),
        }
    }

    // =========================================================================
    // Chunked Result Integration Tests
    // =========================================================================

    #[test]
    fn tool_result_chunk_helper_methods() {
        // Verify helper constructors work correctly
        let chunk = HostOpResponse::tool_result_chunk(b"data".to_vec(), false);
        assert!(!matches!(chunk, HostOpResponse::ToolResult { .. }));

        let error = HostOpResponse::tool_result_error("failed");
        assert!(!matches!(error, HostOpResponse::Error { .. }));
    }

    #[test]
    fn chunked_result_in_host_op_result() {
        // Verify chunks work in HostOpResult
        let result = HostOpResult {
            id: 42,
            runtime_id: RuntimeId::new(1),
            result: HostOpResponse::tool_result_chunk(b"chunk".to_vec(), false),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: HostOpResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
        match parsed.result {
            HostOpResponse::ToolResultChunk { data, eof } => {
                assert_eq!(data, b"chunk");
                assert!(!eof);
            }
            _ => panic!("Expected ToolResultChunk"),
        }
    }

    #[test]
    fn chunked_result_batch() {
        // Simulate a batch of chunks for the same operation
        let results = vec![
            HostOpResult {
                id: 100,
                runtime_id: RuntimeId::new(1),
                result: HostOpResponse::tool_result_chunk(b"chunk1".to_vec(), false),
            },
            HostOpResult {
                id: 100, // Same operation ID
                runtime_id: RuntimeId::new(1),
                result: HostOpResponse::tool_result_chunk(b"chunk2".to_vec(), false),
            },
            HostOpResult {
                id: 100, // Same operation ID
                runtime_id: RuntimeId::new(1),
                result: HostOpResponse::tool_result_chunk(b"chunk3".to_vec(), true), // Final
            },
        ];
        let json = serde_json::to_string(&results).unwrap();
        let parsed: Vec<HostOpResult> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 3);

        // Verify all have same ID
        for result in &parsed {
            assert_eq!(result.id, 100);
        }

        // Verify only last has eof=true
        if let HostOpResponse::ToolResultChunk { eof, .. } = &parsed[0].result {
            assert!(!eof);
        }
        if let HostOpResponse::ToolResultChunk { eof, .. } = &parsed[2].result {
            assert!(*eof);
        }
    }

    #[test]
    fn tool_result_error_aborts_stream() {
        // Simulate an abort after partial chunks
        let results = vec![
            HostOpResult {
                id: 200,
                runtime_id: RuntimeId::new(1),
                result: HostOpResponse::tool_result_chunk(b"partial".to_vec(), false),
            },
            HostOpResult {
                id: 200, // Same operation ID
                runtime_id: RuntimeId::new(1),
                result: HostOpResponse::tool_result_error("Connection lost"),
            },
        ];
        let json = serde_json::to_string(&results).unwrap();
        let parsed: Vec<HostOpResult> = serde_json::from_str(&json).unwrap();

        // First is chunk, second is error
        assert!(matches!(
            parsed[0].result,
            HostOpResponse::ToolResultChunk { .. }
        ));
        assert!(matches!(
            parsed[1].result,
            HostOpResponse::ToolResultError { .. }
        ));
    }

    #[test]
    fn is_error_for_chunked_types() {
        // ToolResultChunk is not an error
        let chunk = HostOpResponse::tool_result_chunk(b"data".to_vec(), false);
        assert!(!chunk.is_error());

        // ToolResultError is also not the Error variant (it's a separate abort mechanism)
        let error = HostOpResponse::tool_result_error("abort");
        assert!(!error.is_error()); // is_error() only matches Error variant
    }
}
