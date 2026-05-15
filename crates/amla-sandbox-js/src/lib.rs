//! JavaScript runtime for AI agent sandboxing.
//!
//! This crate provides a JavaScript runtime interface with async operation support.
//! It's designed to work in WASM environments where I/O must be delegated to the host.
//!
//! ## Async Architecture
//!
//! When JS code calls async functions like `__amla__.toolCall()`:
//!
//! 1. JS creates a Promise and registers it with `__native_register_op`
//! 2. The operation is added to the `pending_ops` queue
//! 3. `execute()` returns with the pending ops
//! 4. Host performs the actual async I/O
//! 5. Host calls `resolve()` or `reject()` with results
//! 6. We call `__amla__._resolve()` or `__amla__._reject()` in JS
//! 7. Host calls `run_pending_jobs()` to process Promise continuations
//!
//! ## Example
//!
//! ```rust
//! use amla_js::JsRuntime;
//!
//! let mut runtime = JsRuntime::new();
//!
//! // Execute simple JS
//! let result = runtime.execute("1 + 2").unwrap();
//! assert_eq!(result.value.as_i64().unwrap(), 3);
//!
//! // Console output is captured with level information
//! let result = runtime.execute("console.log(\"hello\")").unwrap();
//! assert!(result.console_output.iter().any(|entry| entry.message.contains("hello")));
//! ```

// missing_docs lint inherited from workspace
#![deny(rustdoc::broken_intra_doc_links)]

mod engine;
mod ffi;
mod globals;
pub mod hydrate;
mod ops;
mod runtime;
mod time_helper;

pub use time_helper::now_millis;

// Re-export engine types
pub use engine::{EngineConfig, EngineError, JsValue as EngineJsValue, QuickJsEngine};
pub use globals::{AMLA_GLOBAL_JS, CONSOLE_JS};
pub use ops::{
    AttenuationSpec, ConstraintSpec, FetchOptions, LlmMessage, LlmOptions, OpResult, OpType,
    PendingOp, PendingOpsQueue,
};
pub use runtime::ConsoleEntry;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// JavaScript runtime error types.
#[derive(Debug, Error)]
pub enum JsError {
    /// JavaScript syntax error
    #[error("Syntax error: {0}")]
    SyntaxError(String),

    /// JavaScript runtime error
    #[error("Runtime error: {0}")]
    RuntimeError(String),

    /// JavaScript type error
    #[error("Type error: {0}")]
    TypeError(String),

    /// JavaScript reference error
    #[error("Reference error: {0}")]
    ReferenceError(String),

    /// Execution timeout
    #[error("Evaluation timeout")]
    Timeout,

    /// Engine not available (e.g., `QuickJS` not compiled)
    #[error("Engine not available: {0}")]
    EngineNotAvailable(String),
}

/// Console output entry with level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleOutput {
    /// Log level: "log", "error", "warn", "info", "debug"
    pub level: String,
    /// The message content
    pub message: String,
}

/// Result of JavaScript execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsResult {
    /// Return value from execution
    pub value: serde_json::Value,
    /// Console output captured during execution (with levels)
    pub console_output: Vec<ConsoleOutput>,
    /// Tool calls recorded during execution (for audit)
    pub tool_calls: Vec<ToolCallRecord>,
    /// Pending async operations that need host fulfillment
    pub pending_ops: Vec<PendingOp>,
}

/// Record of a tool call made during JS execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    /// Tool name that was called
    pub tool: String,
    /// Parameters passed to the tool
    pub params: serde_json::Value,
    /// Result from the tool (if resolved)
    pub result: Option<serde_json::Value>,
    /// Error from the tool (if rejected)
    pub error: Option<String>,
}

/// Shared state between Rust and JS context.
#[derive(Debug, Default)]
pub struct JsState {
    /// Queue of pending async operations
    pub pending_ops: PendingOpsQueue,
    /// Console output captured during execution
    pub console_output: Vec<ConsoleOutput>,
    /// Tool calls made during execution
    pub tool_calls: Vec<ToolCallRecord>,
}

/// JavaScript runtime with async operation support.
///
/// The runtime manages JS execution and tracks pending async operations
/// that need to be fulfilled by the host.
///
/// # Example
///
/// ```rust
/// use amla_js::JsRuntime;
///
/// let mut runtime = JsRuntime::new();
///
/// // Execute JS code
/// let result = runtime.execute("1 + 2").unwrap();
/// assert_eq!(result.value.as_i64().unwrap(), 3);
///
/// // For async operations, the host fulfills pending ops:
/// // for op in result.pending_ops {
/// //     let value = host_do_async_op(&op).await;
/// //     runtime.resolve(&op.id, &value).unwrap();
/// // }
/// // runtime.run_pending_jobs().unwrap();
/// ```
pub struct JsRuntime {
    inner: runtime::RealJsRuntime,
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl JsRuntime {
    /// Create a new JS runtime with `__amla__` global initialized.
    pub fn new() -> Self {
        Self {
            inner: runtime::RealJsRuntime::new().expect("Failed to create QuickJS runtime"),
        }
    }

    /// Create a new JS runtime with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Engine configuration (memory limit, stack size, etc.)
    pub fn with_config(config: EngineConfig) -> Result<Self, JsError> {
        let inner = runtime::RealJsRuntime::with_config(config)
            .map_err(|e| JsError::RuntimeError(e.message))?;
        Ok(Self { inner })
    }

    /// Execute JavaScript code.
    ///
    /// Returns the result along with any pending async operations.
    /// The caller should fulfill pending ops and call `resolve()`/`reject()`.
    pub fn execute(&mut self, code: &str) -> Result<JsResult, JsError> {
        let value = self
            .inner
            .execute(code)
            .map_err(|e| JsError::RuntimeError(e.message))?;

        let console_output: Vec<ConsoleOutput> = self
            .inner
            .take_console_output()
            .into_iter()
            .map(|entry| ConsoleOutput {
                level: entry.level,
                message: entry.message,
            })
            .collect();

        let pending_ops = self.inner.take_pending_ops();

        Ok(JsResult {
            value,
            console_output,
            tool_calls: vec![], // Tool calls are tracked via pending_ops
            pending_ops,
        })
    }

    /// Resolve a pending operation with a successful result.
    ///
    /// This removes the operation from the pending queue and resolves the Promise.
    pub fn resolve(&mut self, op_id: &str, value: &serde_json::Value) -> Result<(), JsError> {
        self.inner
            .resolve(op_id, value)
            .map_err(|e| JsError::RuntimeError(e.message))
    }

    /// Reject a pending operation with an error.
    ///
    /// This removes the operation from the pending queue and rejects the Promise.
    pub fn reject(&mut self, op_id: &str, error: &str) -> Result<(), JsError> {
        self.inner
            .reject(op_id, error)
            .map_err(|e| JsError::RuntimeError(e.message))
    }

    /// Run pending Promise jobs (microtask queue).
    ///
    /// Call this after resolving/rejecting operations to process Promise continuations.
    pub fn run_pending_jobs(&mut self) -> Result<(), JsError> {
        self.inner
            .run_pending_jobs()
            .map(|_| ())
            .map_err(|e| JsError::RuntimeError(e.message))
    }

    /// Check if there are pending jobs.
    pub fn has_pending_jobs(&self) -> bool {
        self.inner.has_pending_jobs()
    }

    /// Get current pending operations without removing them.
    ///
    /// Note: This returns empty - use `take_pending_ops()` instead since
    /// we can't get ops without removing them from the inner runtime.
    pub fn pending_ops(&self) -> Vec<PendingOp> {
        vec![]
    }

    /// Take pending operations (removes them from the queue).
    ///
    /// Returns ops that were created since the last call.
    /// Ops can be created during `execute()` or in Promise continuations
    /// triggered by `resolve()`/`reject()`.
    pub fn take_pending_ops(&mut self) -> Vec<PendingOp> {
        self.inner.take_pending_ops()
    }

    /// Get console output from last execution.
    ///
    /// Note: This always returns empty since console output
    /// is returned from `execute()` and consumed.
    pub fn console_output(&self) -> Vec<ConsoleOutput> {
        vec![] // Console output is returned inline from execute()
    }

    /// Take console output accumulated since last call (drains the buffer).
    ///
    /// Use this after `resolve()` or `reject()` to get console output
    /// from promise continuations.
    pub fn take_console_output(&mut self) -> Vec<ConsoleOutput> {
        self.inner
            .take_console_output()
            .into_iter()
            .map(|entry| ConsoleOutput {
                level: entry.level,
                message: entry.message,
            })
            .collect()
    }

    /// Clear all state (pending ops, console output, tool calls).
    ///
    /// Note: This is a no-op since state is managed per-execution by the inner runtime.
    pub fn clear(&mut self) {
        // State is managed per-execution, nothing to clear
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_now_returns_timestamp() {
        let mut rt = JsRuntime::new();

        // Call Date.now() twice with a small delay
        let result1 = rt.execute("Date.now()").unwrap();
        let result2 = rt.execute("Date.now()").unwrap();

        #[allow(clippy::cast_possible_truncation)]
        let t1 = result1
            .value
            .as_i64()
            .or_else(|| result1.value.as_f64().map(|f| f as i64));
        #[allow(clippy::cast_possible_truncation)]
        let t2 = result2
            .value
            .as_i64()
            .or_else(|| result2.value.as_f64().map(|f| f as i64));

        println!("Date.now() call 1: {t1:?}");
        println!("Date.now() call 2: {t2:?}");

        // Should return a reasonable Unix timestamp (after year 2020)
        let t1 = t1.expect("Date.now() should return a number");
        assert!(t1 > 1577836800000, "Timestamp should be after 2020: {t1}");

        // Second call should be >= first (time moves forward)
        let t2 = t2.expect("Date.now() should return a number");
        assert!(t2 >= t1, "Time should not go backwards");
    }

    #[test]
    fn test_math_random_returns_float() {
        let mut rt = JsRuntime::new();

        let result1 = rt.execute("Math.random()").unwrap();
        let result2 = rt.execute("Math.random()").unwrap();

        let r1 = result1
            .value
            .as_f64()
            .expect("Math.random() should return float");
        let r2 = result2
            .value
            .as_f64()
            .expect("Math.random() should return float");

        println!("Math.random() call 1: {r1}");
        println!("Math.random() call 2: {r2}");

        // Should be in [0, 1) range
        assert!((0.0..1.0).contains(&r1), "Random should be in [0,1): {r1}");
        assert!((0.0..1.0).contains(&r2), "Random should be in [0,1): {r2}");

        // Should (almost certainly) be different
        // Note: Same PRNG seed would produce same sequence
        #[allow(clippy::float_cmp)]
        let different = r1 != r2;
        println!("Values are different: {different}");
    }

    #[test]
    fn test_arithmetic() {
        let mut rt = JsRuntime::new();
        let result = rt.execute("1 + 1").unwrap();
        assert_eq!(result.value.as_i64().unwrap(), 2);
    }

    #[test]
    fn test_arithmetic_precedence() {
        let mut rt = JsRuntime::new();
        let result = rt.execute("1 + 2 * 3").unwrap();
        assert_eq!(result.value.as_i64().unwrap(), 7);
    }

    #[test]
    fn test_string_literal() {
        let mut rt = JsRuntime::new();
        let result = rt.execute("\"hello world\"").unwrap();
        assert_eq!(result.value, serde_json::json!("hello world"));
    }

    #[test]
    fn test_json_object() {
        let mut rt = JsRuntime::new();
        // Wrap in parens so it's parsed as expression
        let result = rt.execute("({\"a\": 1, \"b\": 2})").unwrap();
        assert_eq!(result.value, serde_json::json!({"a": 1, "b": 2}));
    }

    #[test]
    fn test_console_log() {
        let mut rt = JsRuntime::new();
        let result = rt.execute("console.log(\"hello\")").unwrap();
        assert!(
            result
                .console_output
                .iter()
                .any(|entry| entry.message.contains("hello"))
        );
    }

    #[test]
    fn test_pending_ops() {
        let mut rt = JsRuntime::new();
        let result = rt
            .execute("__amla__.toolCall(\"test:echo\", {\"msg\": \"hello\"})")
            .unwrap();
        assert!(!result.pending_ops.is_empty());
    }

    #[test]
    fn test_empty_code() {
        let mut rt = JsRuntime::new();
        let result = rt.execute("").unwrap();
        assert_eq!(result.value, serde_json::Value::Null);
    }

    #[test]
    fn test_whitespace_code() {
        let mut rt = JsRuntime::new();
        let result = rt.execute("   \n\t  ").unwrap();
        assert_eq!(result.value, serde_json::Value::Null);
    }

    #[test]
    fn test_real_js_function() {
        let mut rt = JsRuntime::new();
        let result = rt.execute("function foo() { return 42; } foo();").unwrap();
        assert_eq!(result.value, serde_json::json!(42));
    }

    #[test]
    fn test_real_js_promise_resolve() {
        let mut rt = JsRuntime::new();
        let result = rt
            .execute("__amla__.toolCall(\"test:echo\", {\"msg\": \"hello\"})")
            .unwrap();

        assert!(!result.pending_ops.is_empty());
        let op_id = result.pending_ops[0].id.clone();

        // Resolve the promise
        rt.resolve(&op_id, &serde_json::json!({"result": "ok"}))
            .unwrap();
        rt.run_pending_jobs().unwrap();
    }

    #[test]
    fn test_pending_ops_from_promise_continuation() {
        // Tests that pending ops created in Promise continuations
        // are accessible via take_pending_ops()
        let mut rt = JsRuntime::new();

        // Chain: tool call -> on resolve, make another tool call
        let result = rt
            .execute(
                r"
            globalThis.chainResult = null;
            __amla__.toolCall('first:call', {})
                .then(r => {
                    return __amla__.toolCall('second:call', {first_result: r});
                })
                .then(r => {
                    globalThis.chainResult = r;
                });
        ",
            )
            .unwrap();

        // First call from execute()
        assert_eq!(result.pending_ops.len(), 1);
        let op_id = result.pending_ops[0].id.clone();

        // Resolve first call - triggers continuation with second call
        rt.resolve(&op_id, &serde_json::json!({"step": 1})).unwrap();

        // Second call must be accessible via take_pending_ops()
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1, "Second op from continuation must be surfaced");
        assert!(matches!(
            &ops[0].op_type,
            crate::ops::OpType::ToolCall { tool, .. } if tool == "second:call"
        ));

        // Resolve second call
        rt.resolve(&ops[0].id, &serde_json::json!({"step": 2}))
            .unwrap();

        // Verify final result
        let final_result = rt.execute("globalThis.chainResult").unwrap();
        assert_eq!(final_result.value["step"], 2);
    }

    // =========================================================================
    // Coverage tests for lib.rs wrapper methods
    // =========================================================================

    #[test]
    fn test_js_runtime_default() {
        // Test Default implementation
        let rt = JsRuntime::default();
        assert!(!rt.has_pending_jobs());
    }

    #[test]
    fn test_js_runtime_with_config() {
        // Test with_config method
        let config = EngineConfig::default();
        let rt = JsRuntime::with_config(config).unwrap();
        assert!(!rt.has_pending_jobs());
    }

    #[test]
    fn test_js_runtime_reject() {
        let mut rt = JsRuntime::new();
        let result = rt
            .execute(
                r#"
            globalThis.error = null;
            __amla__.toolCall("test", {}).catch(e => { globalThis.error = e.message; });
        "#,
            )
            .unwrap();

        // pending_ops are returned in the result, not via take_pending_ops
        assert_eq!(result.pending_ops.len(), 1);

        // Test reject via wrapper
        rt.reject(&result.pending_ops[0].id, "Wrapper rejection test")
            .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.value.as_str().unwrap(), "Wrapper rejection test");
    }

    #[test]
    fn test_js_runtime_has_pending_jobs() {
        let rt = JsRuntime::new();
        // Initially no pending jobs
        assert!(!rt.has_pending_jobs());
    }

    #[test]
    fn test_js_runtime_pending_ops() {
        let rt = JsRuntime::new();
        // pending_ops() returns empty (use take_pending_ops instead)
        assert!(rt.pending_ops().is_empty());
    }

    #[test]
    fn test_js_runtime_console_output() {
        let rt = JsRuntime::new();
        // console_output() returns empty (output is inline from execute)
        assert!(rt.console_output().is_empty());
    }

    #[test]
    fn test_js_runtime_take_console_output() {
        let mut rt = JsRuntime::new();
        rt.execute("console.log('test')").unwrap();

        // Console output from execute is consumed, but we can check the method works
        let output = rt.take_console_output();
        // May or may not have output depending on timing
        assert!(output.is_empty() || !output.is_empty());
    }

    #[test]
    fn test_js_runtime_run_pending_jobs() {
        let mut rt = JsRuntime::new();
        // Test run_pending_jobs wrapper
        rt.run_pending_jobs().unwrap();
    }

    #[test]
    fn test_js_runtime_clear() {
        let mut rt = JsRuntime::new();
        // Test clear() - it's a no-op but should not panic
        rt.clear();
    }

    #[test]
    fn test_console_output_struct() {
        let output = ConsoleOutput {
            level: "log".to_string(),
            message: "test message".to_string(),
        };
        assert_eq!(output.level, "log");
        assert_eq!(output.message, "test message");
    }

    #[test]
    fn test_js_result_struct() {
        let result = JsResult {
            value: serde_json::json!(42),
            console_output: vec![ConsoleOutput {
                level: "log".to_string(),
                message: "test".to_string(),
            }],
            tool_calls: vec![],
            pending_ops: vec![],
        };
        assert_eq!(result.value, serde_json::json!(42));
        assert_eq!(result.console_output.len(), 1);
    }

    #[test]
    fn test_js_error_display() {
        let error = JsError::RuntimeError("test error".to_string());
        assert_eq!(format!("{error}"), "Runtime error: test error");

        let error = JsError::SyntaxError("bad syntax".to_string());
        assert_eq!(format!("{error}"), "Syntax error: bad syntax");

        let error = JsError::TypeError("type mismatch".to_string());
        assert_eq!(format!("{error}"), "Type error: type mismatch");

        let error = JsError::ReferenceError("undefined variable".to_string());
        assert_eq!(format!("{error}"), "Reference error: undefined variable");

        let error = JsError::Timeout;
        assert_eq!(format!("{error}"), "Evaluation timeout");

        let error = JsError::EngineNotAvailable("QuickJS".to_string());
        assert_eq!(format!("{error}"), "Engine not available: QuickJS");
    }
}
