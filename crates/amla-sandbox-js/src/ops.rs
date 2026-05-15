//! Pending operations queue for async JS execution.
//!
//! When JS code calls async functions like `__amla__.toolCall()` or `fetch()`,
//! we can't perform I/O directly from WASM. Instead:
//!
//! 1. We create a Promise and store its resolver
//! 2. We add a `PendingOp` to the queue
//! 3. We return control to the host with the pending ops
//! 4. Host performs async I/O
//! 5. Host calls `resolve(op_id, result)` to deliver results
//! 6. We resolve the Promise and run pending jobs
//!
//! For WASI builds, uuid uses WASI's `random_get` syscall.
//! The host controls randomness by intercepting this WASI import.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// A pending async operation that the host must fulfill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingOp {
    /// Unique identifier for this operation
    pub id: String,
    /// Type of operation
    pub op_type: OpType,
    /// Timestamp when created (milliseconds since epoch)
    pub created_at: i64,
}

impl PendingOp {
    /// Create a new pending operation with a unique ID.
    ///
    /// Uses uuid crate which uses WASI `random_get` on WASM.
    pub fn new(op_type: OpType) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            op_type,
            created_at: crate::now_millis(),
        }
    }

    /// Create a tool call operation.
    pub fn tool_call(tool: String, params: serde_json::Value) -> Self {
        Self::new(OpType::ToolCall { tool, params })
    }

    /// Create a fetch operation.
    pub fn fetch(url: String, options: FetchOptions) -> Self {
        Self::new(OpType::Fetch { url, options })
    }

    /// Create a memory read operation.
    pub fn memory_read(key: String) -> Self {
        Self::new(OpType::MemoryRead { key })
    }

    /// Create a memory write operation.
    pub fn memory_write(key: String, value: serde_json::Value) -> Self {
        Self::new(OpType::MemoryWrite { key, value })
    }

    /// Create a memory delete operation.
    pub fn memory_delete(key: String) -> Self {
        Self::new(OpType::MemoryDelete { key })
    }

    /// Create a spawn operation.
    pub fn spawn(attenuations: Vec<AttenuationSpec>) -> Self {
        Self::new(OpType::Spawn { attenuations })
    }

    /// Create an LLM call operation.
    pub fn llm_call(model: String, messages: Vec<LlmMessage>, options: LlmOptions) -> Self {
        Self::new(OpType::LlmCall {
            model,
            messages,
            options,
        })
    }

    /// Create a shell command operation.
    ///
    /// This runs through the internal sandboxed shell interpreter.
    pub fn shell(command: String) -> Self {
        Self::new(OpType::Shell { command })
    }
}

/// Types of async operations that can be performed by the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpType {
    /// Tool call via capability.
    ToolCall {
        /// The tool name to call.
        tool: String,
        /// The parameters to pass to the tool.
        params: serde_json::Value,
    },

    /// HTTP fetch.
    Fetch {
        /// The URL to fetch.
        url: String,
        /// Fetch options (method, headers, body).
        options: FetchOptions,
    },

    /// Memory read from partitioned storage.
    MemoryRead {
        /// The key to read.
        key: String,
    },

    /// Memory write to partitioned storage.
    MemoryWrite {
        /// The key to write.
        key: String,
        /// The value to write.
        value: serde_json::Value,
    },

    /// Memory delete from partitioned storage.
    MemoryDelete {
        /// The key to delete.
        key: String,
    },

    /// Spawn child session with attenuated capabilities.
    Spawn {
        /// Capability attenuations for the child session.
        attenuations: Vec<AttenuationSpec>,
    },

    /// LLM call.
    LlmCall {
        /// The model to use (e.g., "gpt-4").
        model: String,
        /// The messages to send.
        messages: Vec<LlmMessage>,
        /// LLM options (temperature, `max_tokens`, etc.).
        options: LlmOptions,
    },

    /// Sleep/delay operation.
    Sleep {
        /// Delay in milliseconds.
        delay_ms: u64,
    },

    /// File system read operation.
    FsRead {
        /// Path to read.
        path: String,
        /// Read options.
        options: FsReadOptions,
    },

    /// File system write operation.
    FsWrite {
        /// Path to write.
        path: String,
        /// Data to write.
        data: String,
        /// Write options.
        options: FsWriteOptions,
    },

    /// File system read directory operation.
    FsReadDir {
        /// Path to directory.
        path: String,
    },

    /// File system stat operation.
    FsStat {
        /// Path to stat.
        path: String,
    },

    /// File system exists check.
    FsExists {
        /// Path to check.
        path: String,
    },

    /// File system unlink (delete) operation.
    FsUnlink {
        /// Path to delete.
        path: String,
    },

    /// File system mkdir operation.
    FsMkdir {
        /// Path to create.
        path: String,
        /// Whether to create parent directories.
        recursive: bool,
    },

    /// Internal shell command execution.
    ///
    /// Runs a command through the sandboxed shell interpreter (amla-shell).
    /// This is NOT a host operation - it runs entirely within the sandbox.
    Shell {
        /// The command string to execute (e.g., "echo hello", "cat file.txt").
        command: String,
    },
}

/// Options for fetch operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FetchOptions {
    /// HTTP method (GET, POST, etc.)
    #[serde(default)]
    pub method: String,
    /// HTTP headers
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Request body
    #[serde(default)]
    pub body: Option<String>,
    /// Timeout in milliseconds
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Options for file system read operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsReadOptions {
    /// Encoding (e.g., "utf-8", "base64"). Default is "utf-8".
    #[serde(default)]
    pub encoding: Option<String>,
}

/// Options for file system write operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsWriteOptions {
    /// Encoding (e.g., "utf-8", "base64"). Default is "utf-8".
    #[serde(default)]
    pub encoding: Option<String>,
    /// Whether to append instead of overwrite.
    #[serde(default)]
    pub append: bool,
    /// Whether to create parent directories if they don't exist.
    #[serde(default)]
    pub create_dirs: bool,
}

/// Attenuation specification for spawn operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttenuationSpec {
    /// Capability to attenuate
    pub capability: String,
    /// Constraints to add
    pub constraints: Vec<ConstraintSpec>,
}

/// Constraint specification from JS.
///
/// These are parsed from the `Param(name).op(value)` syntax in JavaScript.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ConstraintSpec {
    /// Less than or equal constraint (`param <= value`).
    Le {
        /// Parameter name to constrain.
        param: String,
        /// Value to compare against.
        value: serde_json::Value,
    },
    /// Greater than or equal constraint (`param >= value`).
    Ge {
        /// Parameter name to constrain.
        param: String,
        /// Value to compare against.
        value: serde_json::Value,
    },
    /// Less than constraint (`param < value`).
    Lt {
        /// Parameter name to constrain.
        param: String,
        /// Value to compare against.
        value: serde_json::Value,
    },
    /// Greater than constraint (`param > value`).
    Gt {
        /// Parameter name to constrain.
        param: String,
        /// Value to compare against.
        value: serde_json::Value,
    },
    /// Equality constraint (`param == value`).
    Eq {
        /// Parameter name to constrain.
        param: String,
        /// Value to compare against.
        value: serde_json::Value,
    },
    /// Not-equal constraint (`param != value`).
    Ne {
        /// Parameter name to constrain.
        param: String,
        /// Value to compare against.
        value: serde_json::Value,
    },
    /// Set membership constraint (`param in values`).
    In {
        /// Parameter name to constrain.
        param: String,
        /// Allowed values.
        values: Vec<serde_json::Value>,
    },
    /// Set exclusion constraint (`param not in values`).
    NotIn {
        /// Parameter name to constrain.
        param: String,
        /// Disallowed values.
        values: Vec<serde_json::Value>,
    },
    /// String prefix constraint.
    StartsWith {
        /// Parameter name to constrain.
        param: String,
        /// Required prefix.
        prefix: String,
    },
    /// String suffix constraint.
    EndsWith {
        /// Parameter name to constrain.
        param: String,
        /// Required suffix.
        suffix: String,
    },
    /// Substring containment constraint.
    Contains {
        /// Parameter name to constrain.
        param: String,
        /// Required substring.
        substring: String,
    },
}

/// LLM message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    /// Role (system, user, assistant)
    pub role: String,
    /// Message content
    pub content: String,
}

/// Options for LLM calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmOptions {
    /// Sampling temperature
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Maximum tokens to generate
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Stop sequences
    #[serde(default)]
    pub stop: Vec<String>,
}

/// Result delivered back from host after fulfilling an operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpResult {
    /// Operation ID this result is for
    pub id: String,
    /// Whether the operation succeeded
    pub success: bool,
    /// Result value (if success)
    pub value: Option<serde_json::Value>,
    /// Error message (if failure)
    pub error: Option<String>,
}

impl OpResult {
    /// Create a successful result.
    pub fn ok(id: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            success: true,
            value: Some(value),
            error: None,
        }
    }

    /// Create an error result.
    pub fn err(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            success: false,
            value: None,
            error: Some(error.into()),
        }
    }
}

/// Queue of pending async operations.
#[derive(Debug, Default)]
pub struct PendingOpsQueue {
    ops: Vec<PendingOp>,
}

impl PendingOpsQueue {
    /// Create a new empty queue.
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    /// Add an operation to the queue, returning its ID.
    pub fn push(&mut self, op: PendingOp) -> String {
        let id = op.id.clone();
        self.ops.push(op);
        id
    }

    /// Take all pending operations from the queue.
    pub fn take(&mut self) -> Vec<PendingOp> {
        std::mem::take(&mut self.ops)
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Get the number of pending operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Get the last operation (useful for testing).
    pub fn last(&self) -> Option<&PendingOp> {
        self.ops.last()
    }

    /// Remove operations matching a predicate.
    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&PendingOp) -> bool,
    {
        self.ops.retain(f);
    }

    /// Clone all pending operations.
    pub fn clone_all(&self) -> Vec<PendingOp> {
        self.ops.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_op_serialization() {
        let op = PendingOp::tool_call(
            "stripe:charge".to_string(),
            serde_json::json!({"amount": 100}),
        );
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("tool_call"));
        assert!(json.contains("stripe:charge"));
    }

    #[test]
    fn test_op_result() {
        let ok = OpResult::ok("abc", serde_json::json!({"result": "success"}));
        assert!(ok.success);
        assert!(ok.value.is_some());
        assert!(ok.error.is_none());

        let err = OpResult::err("xyz", "Something went wrong");
        assert!(!err.success);
        assert!(err.value.is_none());
        assert!(err.error.is_some());
    }

    #[test]
    fn test_queue_operations() {
        let mut queue = PendingOpsQueue::new();
        assert!(queue.is_empty());

        let id = queue.push(PendingOp::memory_read("foo".to_string()));
        assert_eq!(queue.len(), 1);
        assert!(!queue.is_empty());

        let ops = queue.take();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].id, id);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_fetch_options_default() {
        let opts = FetchOptions::default();
        assert!(opts.method.is_empty());
        assert!(opts.headers.is_empty());
        assert!(opts.body.is_none());
        assert!(opts.timeout_ms.is_none());
    }

    #[test]
    fn test_llm_options_default() {
        let opts = LlmOptions::default();
        assert!(opts.temperature.is_none());
        assert!(opts.max_tokens.is_none());
        assert!(opts.stop.is_empty());
    }

    #[test]
    fn test_constraint_spec_serialization() {
        let constraint = ConstraintSpec::Le {
            param: "amount".to_string(),
            value: serde_json::json!(1000),
        };
        let json = serde_json::to_string(&constraint).unwrap();
        assert!(json.contains("le"));
        assert!(json.contains("amount"));
        assert!(json.contains("1000"));
    }

    #[test]
    fn test_attenuation_spec() {
        let spec = AttenuationSpec {
            capability: "stripe:charge".to_string(),
            constraints: vec![
                ConstraintSpec::Le {
                    param: "amount".to_string(),
                    value: serde_json::json!(5000),
                },
                ConstraintSpec::Eq {
                    param: "currency".to_string(),
                    value: serde_json::json!("USD"),
                },
            ],
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("stripe:charge"));
        assert!(json.contains("amount"));
        assert!(json.contains("currency"));
    }

    #[test]
    fn test_pending_op_fetch() {
        let options = FetchOptions {
            method: "POST".to_string(),
            headers: [("Content-Type".to_string(), "application/json".to_string())]
                .into_iter()
                .collect(),
            body: Some(r#"{"key":"value"}"#.to_string()),
            timeout_ms: Some(5000),
        };
        let op = PendingOp::fetch("https://api.example.com".to_string(), options);
        match op.op_type {
            OpType::Fetch { url, options } => {
                assert_eq!(url, "https://api.example.com");
                assert_eq!(options.method, "POST");
                assert_eq!(options.timeout_ms, Some(5000));
            }
            _ => panic!("Expected Fetch op"),
        }
    }

    #[test]
    fn test_pending_op_memory_write() {
        let op =
            PendingOp::memory_write("user:123".to_string(), serde_json::json!({"name": "Alice"}));
        match op.op_type {
            OpType::MemoryWrite { key, value } => {
                assert_eq!(key, "user:123");
                assert_eq!(value["name"], "Alice");
            }
            _ => panic!("Expected MemoryWrite op"),
        }
    }

    #[test]
    fn test_pending_op_memory_delete() {
        let op = PendingOp::memory_delete("old_key".to_string());
        match op.op_type {
            OpType::MemoryDelete { key } => {
                assert_eq!(key, "old_key");
            }
            _ => panic!("Expected MemoryDelete op"),
        }
    }

    #[test]
    fn test_pending_op_spawn() {
        let attenuations = vec![AttenuationSpec {
            capability: "stripe:charge".to_string(),
            constraints: vec![ConstraintSpec::Le {
                param: "amount".to_string(),
                value: serde_json::json!(1000),
            }],
        }];
        let op = PendingOp::spawn(attenuations);
        match op.op_type {
            OpType::Spawn { attenuations } => {
                assert_eq!(attenuations.len(), 1);
                assert_eq!(attenuations[0].capability, "stripe:charge");
            }
            _ => panic!("Expected Spawn op"),
        }
    }

    #[test]
    fn test_pending_op_llm_call() {
        let messages = vec![
            LlmMessage {
                role: "system".to_string(),
                content: "You are helpful".to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
            },
        ];
        let options = LlmOptions {
            temperature: Some(0.7),
            max_tokens: Some(100),
            stop: vec!["\n".to_string()],
        };
        let op = PendingOp::llm_call("gpt-4".to_string(), messages, options);
        match op.op_type {
            OpType::LlmCall {
                model,
                messages,
                options,
            } => {
                assert_eq!(model, "gpt-4");
                assert_eq!(messages.len(), 2);
                assert_eq!(options.temperature, Some(0.7));
                assert_eq!(options.max_tokens, Some(100));
            }
            _ => panic!("Expected LlmCall op"),
        }
    }

    #[test]
    fn test_queue_last() {
        let mut queue = PendingOpsQueue::new();
        assert!(queue.last().is_none());

        queue.push(PendingOp::memory_read("key1".to_string()));
        queue.push(PendingOp::memory_read("key2".to_string()));

        let last = queue.last().unwrap();
        match &last.op_type {
            OpType::MemoryRead { key } => assert_eq!(key, "key2"),
            _ => panic!("Expected MemoryRead"),
        }
    }

    #[test]
    fn test_queue_retain() {
        let mut queue = PendingOpsQueue::new();
        let id1 = queue.push(PendingOp::memory_read("keep".to_string()));
        let _id2 = queue.push(PendingOp::memory_read("remove".to_string()));
        let id3 = queue.push(PendingOp::memory_read("keep2".to_string()));

        assert_eq!(queue.len(), 3);

        queue.retain(
            |op| matches!(&op.op_type, OpType::MemoryRead { key } if key.starts_with("keep")),
        );

        assert_eq!(queue.len(), 2);
        let ops = queue.take();
        assert_eq!(ops[0].id, id1);
        assert_eq!(ops[1].id, id3);
    }

    #[test]
    fn test_queue_clone_all() {
        let mut queue = PendingOpsQueue::new();
        queue.push(PendingOp::memory_read("key1".to_string()));
        queue.push(PendingOp::memory_read("key2".to_string()));

        let cloned = queue.clone_all();
        assert_eq!(cloned.len(), 2);

        // Original queue still has items
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn test_all_constraint_spec_variants() {
        // Test serialization of all ConstraintSpec variants
        let constraints = vec![
            ConstraintSpec::Lt {
                param: "x".to_string(),
                value: serde_json::json!(10),
            },
            ConstraintSpec::Gt {
                param: "y".to_string(),
                value: serde_json::json!(5),
            },
            ConstraintSpec::Ne {
                param: "z".to_string(),
                value: serde_json::json!("bad"),
            },
            ConstraintSpec::In {
                param: "color".to_string(),
                values: vec![serde_json::json!("red"), serde_json::json!("blue")],
            },
            ConstraintSpec::NotIn {
                param: "status".to_string(),
                values: vec![serde_json::json!("deleted")],
            },
            ConstraintSpec::StartsWith {
                param: "path".to_string(),
                prefix: "/api/".to_string(),
            },
            ConstraintSpec::EndsWith {
                param: "file".to_string(),
                suffix: ".json".to_string(),
            },
            ConstraintSpec::Contains {
                param: "query".to_string(),
                substring: "SELECT".to_string(),
            },
        ];

        for constraint in constraints {
            let json = serde_json::to_string(&constraint).unwrap();
            let parsed: ConstraintSpec = serde_json::from_str(&json).unwrap();
            assert_eq!(
                serde_json::to_string(&parsed).unwrap(),
                json,
                "Roundtrip failed"
            );
        }
    }

    #[test]
    fn test_pending_op_shell() {
        let op = PendingOp::shell("echo hello world".to_string());
        match op.op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "echo hello world");
            }
            _ => panic!("Expected Shell op"),
        }
    }

    #[test]
    fn test_shell_op_serialization() {
        let op = PendingOp::shell("ls -la /tmp".to_string());
        let json = serde_json::to_string(&op).unwrap();
        assert!(json.contains("shell"));
        assert!(json.contains("ls -la /tmp"));
    }
}
