//! Real JavaScript runtime using `QuickJS`.
//!
//! This module integrates the `QuickJS` engine with our async operation model,
//! console interception, and global object setup.

use crate::engine::{EngineConfig, EngineError, QuickJsEngine};
use crate::globals::{AMLA_GLOBAL_JS, CONSOLE_JS};
use crate::ops::{FetchOptions, FsReadOptions, FsWriteOptions, LlmOptions, OpType, PendingOp};

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// Console output entry.
#[derive(Debug, Clone)]
pub struct ConsoleEntry {
    /// Log level: "log", "error", "warn", "info", "debug"
    pub level: String,
    /// The message content
    pub message: String,
}

/// Shared state for the runtime callbacks.
struct RuntimeState {
    /// Pending operations registered by JS
    pending_ops: VecDeque<PendingOp>,
    /// Console output captured during execution
    console_output: Vec<ConsoleEntry>,
}

/// Real JavaScript runtime backed by `QuickJS`.
///
/// This runtime:
/// - Executes JavaScript code via `QuickJS`
/// - Intercepts console.log/error/warn/etc. via `__native_log`
/// - Captures pending async operations via `__native_register_op`
/// - Provides resolve/reject for Promises
pub struct RealJsRuntime {
    engine: QuickJsEngine,
    state: Rc<RefCell<RuntimeState>>,
}

impl RealJsRuntime {
    /// Create a new JS runtime with default configuration.
    pub fn new() -> Result<Self, EngineError> {
        Self::with_config(EngineConfig::default())
    }

    /// Create a new JS runtime with custom configuration.
    pub fn with_config(config: EngineConfig) -> Result<Self, EngineError> {
        let mut engine = QuickJsEngine::new(config)?;
        let state = Rc::new(RefCell::new(RuntimeState {
            pending_ops: VecDeque::new(),
            console_output: Vec::new(),
        }));

        // Register native callbacks
        Self::register_native_functions(&mut engine, &state)?;

        // Inject global objects
        engine.eval(CONSOLE_JS)?;
        engine.eval(AMLA_GLOBAL_JS)?;

        Ok(Self { engine, state })
    }

    #[allow(clippy::too_many_lines)]
    fn register_native_functions(
        engine: &mut QuickJsEngine,
        state: &Rc<RefCell<RuntimeState>>,
    ) -> Result<(), EngineError> {
        // Register __native_log for console output
        let log_state = Rc::clone(state);
        engine.add_function("__native_log", move |args_json| {
            let args: Vec<serde_json::Value> = serde_json::from_str(args_json).ok()?;
            let level = args.first()?.as_str()?.to_string();
            let message = args.get(1)?.as_str()?.to_string();

            let mut s = log_state.borrow_mut();
            s.console_output.push(ConsoleEntry { level, message });
            None // __native_log returns undefined
        })?;

        // Register __native_register_op for pending operations
        // Returns: null on success, or {"error": "message"} on failure
        let op_state = Rc::clone(state);
        engine.add_function("__native_register_op", move |args_json| {
            // Helper to return error JSON
            fn error_result(msg: &str) -> String {
                format!(r#"{{"error":"{}"}}"#, msg.replace('"', "\\\""))
            }

            let Ok(args) = serde_json::from_str::<Vec<serde_json::Value>>(args_json) else {
                return Some(error_result("Invalid arguments JSON"));
            };
            let Some(id) = args.first().and_then(|v| v.as_str()).map(String::from) else {
                return Some(error_result("Missing operation ID"));
            };
            let Some(payload_str) = args.get(1).and_then(|v| v.as_str()) else {
                return Some(error_result("Missing operation payload"));
            };
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(payload_str) else {
                return Some(error_result("Invalid payload JSON"));
            };

            // Parse the operation type
            let Some(op_type) = payload.get("type").and_then(|v| v.as_str()) else {
                return Some(error_result("Missing operation type"));
            };
            // Helper macros to extract required fields with error messages
            macro_rules! get_str {
                ($obj:expr, $field:literal) => {
                    match $obj.get($field).and_then(|v| v.as_str()) {
                        Some(s) => s.to_string(),
                        None => {
                            return Some(error_result(concat!("Missing or invalid field: ", $field)))
                        }
                    }
                };
            }
            macro_rules! get_value {
                ($obj:expr, $field:literal) => {
                    match $obj.get($field) {
                        Some(v) => v.clone(),
                        None => return Some(error_result(concat!("Missing field: ", $field))),
                    }
                };
            }

            let pending_op = match op_type {
                "tool_call" => {
                    let tool = get_str!(payload, "tool");
                    let params = get_value!(payload, "params");
                    PendingOp {
                        id,
                        op_type: OpType::ToolCall { tool, params },
                        created_at: crate::now_millis(),
                    }
                }
                "fetch" => {
                    let url = get_str!(payload, "url");
                    let options: FetchOptions = payload
                        .get("options")
                        .and_then(|o| serde_json::from_value(o.clone()).ok())
                        .unwrap_or_default();
                    PendingOp {
                        id,
                        op_type: OpType::Fetch { url, options },
                        created_at: crate::now_millis(),
                    }
                }
                "memory_read" => {
                    let key = get_str!(payload, "key");
                    PendingOp {
                        id,
                        op_type: OpType::MemoryRead { key },
                        created_at: crate::now_millis(),
                    }
                }
                "memory_write" => {
                    let key = get_str!(payload, "key");
                    let value = get_value!(payload, "value");
                    PendingOp {
                        id,
                        op_type: OpType::MemoryWrite { key, value },
                        created_at: crate::now_millis(),
                    }
                }
                "memory_delete" => {
                    let key = get_str!(payload, "key");
                    PendingOp {
                        id,
                        op_type: OpType::MemoryDelete { key },
                        created_at: crate::now_millis(),
                    }
                }
                "spawn" => {
                    let attenuations = get_value!(payload, "attenuations");
                    let Ok(parsed) = serde_json::from_value(attenuations) else {
                        return Some(error_result("Invalid attenuations format"));
                    };
                    PendingOp {
                        id,
                        op_type: OpType::Spawn {
                            attenuations: parsed,
                        },
                        created_at: crate::now_millis(),
                    }
                }
                "llm_call" => {
                    let model = get_str!(payload, "model");
                    let messages = get_value!(payload, "messages");
                    let Ok(parsed_messages) = serde_json::from_value(messages) else {
                        return Some(error_result("Invalid messages format"));
                    };
                    let options: LlmOptions = payload
                        .get("options")
                        .and_then(|o| serde_json::from_value(o.clone()).ok())
                        .unwrap_or_default();
                    PendingOp {
                        id,
                        op_type: OpType::LlmCall {
                            model,
                            messages: parsed_messages,
                            options,
                        },
                        created_at: crate::now_millis(),
                    }
                }
                "sleep" => {
                    let Some(delay_ms) =
                        payload.get("delay_ms").and_then(serde_json::Value::as_u64)
                    else {
                        return Some(error_result("Missing or invalid delay_ms"));
                    };
                    PendingOp {
                        id,
                        op_type: OpType::Sleep { delay_ms },
                        created_at: crate::now_millis(),
                    }
                }
                "fs_read" => {
                    let path = get_str!(payload, "path");
                    let options: FsReadOptions = payload
                        .get("options")
                        .and_then(|o| serde_json::from_value(o.clone()).ok())
                        .unwrap_or_default();
                    PendingOp {
                        id,
                        op_type: OpType::FsRead { path, options },
                        created_at: crate::now_millis(),
                    }
                }
                "fs_write" => {
                    let path = get_str!(payload, "path");
                    let data = get_str!(payload, "data");
                    let options: FsWriteOptions = payload
                        .get("options")
                        .and_then(|o| serde_json::from_value(o.clone()).ok())
                        .unwrap_or_default();
                    PendingOp {
                        id,
                        op_type: OpType::FsWrite {
                            path,
                            data,
                            options,
                        },
                        created_at: crate::now_millis(),
                    }
                }
                "fs_readdir" => {
                    let path = get_str!(payload, "path");
                    PendingOp {
                        id,
                        op_type: OpType::FsReadDir { path },
                        created_at: crate::now_millis(),
                    }
                }
                "fs_stat" => {
                    let path = get_str!(payload, "path");
                    PendingOp {
                        id,
                        op_type: OpType::FsStat { path },
                        created_at: crate::now_millis(),
                    }
                }
                "fs_exists" => {
                    let path = get_str!(payload, "path");
                    PendingOp {
                        id,
                        op_type: OpType::FsExists { path },
                        created_at: crate::now_millis(),
                    }
                }
                "fs_unlink" => {
                    let path = get_str!(payload, "path");
                    PendingOp {
                        id,
                        op_type: OpType::FsUnlink { path },
                        created_at: crate::now_millis(),
                    }
                }
                "fs_mkdir" => {
                    let path = get_str!(payload, "path");
                    let recursive = payload
                        .get("recursive")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    PendingOp {
                        id,
                        op_type: OpType::FsMkdir { path, recursive },
                        created_at: crate::now_millis(),
                    }
                }
                "shell" => {
                    let command = get_str!(payload, "command");
                    PendingOp {
                        id,
                        op_type: OpType::Shell { command },
                        created_at: crate::now_millis(),
                    }
                }
                unknown => {
                    return Some(error_result(&format!("Unknown operation type: {unknown}")));
                }
            };

            op_state.borrow_mut().pending_ops.push_back(pending_op);
            None // __native_register_op returns undefined
        })?;

        Ok(())
    }

    /// Execute JavaScript code.
    pub fn execute(&mut self, code: &str) -> Result<serde_json::Value, EngineError> {
        let result = self.engine.eval(code)?;
        // Run microtask queue
        self.engine.run_pending_jobs()?;
        Ok(result.to_json())
    }

    /// Take pending operations (removes them from the queue).
    pub fn take_pending_ops(&mut self) -> Vec<PendingOp> {
        self.state.borrow_mut().pending_ops.drain(..).collect()
    }

    /// Take console output (removes from buffer).
    pub fn take_console_output(&mut self) -> Vec<ConsoleEntry> {
        std::mem::take(&mut self.state.borrow_mut().console_output)
    }

    /// Resolve a pending operation with a value.
    ///
    /// This calls `__amla__._resolve(id, result_json)` in JS.
    pub fn resolve(&mut self, op_id: &str, value: &serde_json::Value) -> Result<(), EngineError> {
        let result_json = serde_json::to_string(value).map_err(|e| EngineError {
            message: format!("JSON serialization failed: {e}"),
            stack: None,
        })?;

        let call_code = format!(
            "__amla__._resolve({}, {})",
            serde_json::to_string(op_id).unwrap(),
            serde_json::to_string(&result_json).unwrap()
        );
        self.engine.eval(&call_code)?;
        self.engine.run_pending_jobs()?;
        Ok(())
    }

    /// Reject a pending operation with an error.
    ///
    /// This calls `__amla__._reject(id, error)` in JS.
    pub fn reject(&mut self, op_id: &str, error: &str) -> Result<(), EngineError> {
        let call_code = format!(
            "__amla__._reject({}, {})",
            serde_json::to_string(op_id).unwrap(),
            serde_json::to_string(error).unwrap()
        );
        self.engine.eval(&call_code)?;
        self.engine.run_pending_jobs()?;
        Ok(())
    }

    /// Run pending jobs (microtask queue).
    pub fn run_pending_jobs(&mut self) -> Result<i32, EngineError> {
        self.engine.run_pending_jobs()
    }

    /// Check if there are pending jobs.
    pub fn has_pending_jobs(&self) -> bool {
        self.engine.has_pending_jobs()
    }
}

impl Default for RealJsRuntime {
    fn default() -> Self {
        Self::new().expect("Failed to create default runtime")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_creation() {
        let rt = RealJsRuntime::new();
        assert!(rt.is_ok());
    }

    #[test]
    fn test_simple_eval() {
        let mut rt = RealJsRuntime::new().unwrap();
        let result = rt.execute("1 + 2").unwrap();
        assert_eq!(result, serde_json::json!(3));
    }

    #[test]
    fn test_console_log_captured() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("console.log('hello', 'world')").unwrap();

        let output = rt.take_console_output();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].level, "log");
        assert_eq!(output[0].message, "hello world");
    }

    #[test]
    fn test_console_error_captured() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("console.error('oops')").unwrap();

        let output = rt.take_console_output();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].level, "error");
        assert_eq!(output[0].message, "oops");
    }

    #[test]
    fn test_tool_call_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.toolCall('test:echo', {msg: 'hi'})")
            .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::ToolCall { tool, params } => {
                assert_eq!(tool, "test:echo");
                assert_eq!(params["msg"], "hi");
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_fetch_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fetch('https://example.com')").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Fetch { url, .. } => {
                assert_eq!(url, "https://example.com");
            }
            _ => panic!("Expected Fetch"),
        }
    }

    #[test]
    fn test_resolve_promise() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Create a tool call that returns a promise
        rt.execute(
            r"
            globalThis.result = null;
            __amla__.toolCall('test:echo', {}).then(r => { globalThis.result = r; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        let op_id = ops[0].id.clone();

        // Resolve the promise
        rt.resolve(&op_id, &serde_json::json!({"value": 42}))
            .unwrap();

        // Check that the result was set
        let result = rt.execute("globalThis.result").unwrap();
        assert_eq!(result["value"], 42);
    }

    #[test]
    fn test_runtime_isolation_separate_instances() {
        // Create two separate runtimes
        let mut rt1 = RealJsRuntime::new().unwrap();
        let mut rt2 = RealJsRuntime::new().unwrap();

        // Set different values in each runtime
        rt1.execute("globalThis.testValue = 'runtime1'").unwrap();
        rt2.execute("globalThis.testValue = 'runtime2'").unwrap();

        // Verify each runtime has its own value
        let result1 = rt1.execute("globalThis.testValue").unwrap();
        let result2 = rt2.execute("globalThis.testValue").unwrap();

        assert_eq!(result1, serde_json::json!("runtime1"));
        assert_eq!(result2, serde_json::json!("runtime2"));
    }

    #[test]
    fn test_runtime_isolation_pending_ops() {
        // Create two runtimes with pending ops
        let mut rt1 = RealJsRuntime::new().unwrap();
        let mut rt2 = RealJsRuntime::new().unwrap();

        // Each runtime makes its own tool call
        rt1.execute("__amla__.toolCall('rt1:tool', {id: 1})")
            .unwrap();
        rt2.execute("__amla__.toolCall('rt2:tool', {id: 2})")
            .unwrap();

        // Verify pending ops are independent
        let ops1 = rt1.take_pending_ops();
        let ops2 = rt2.take_pending_ops();

        assert_eq!(ops1.len(), 1);
        assert_eq!(ops2.len(), 1);

        // Verify correct tool names
        match &ops1[0].op_type {
            crate::ops::OpType::ToolCall { tool, params } => {
                assert_eq!(tool, "rt1:tool");
                assert_eq!(params["id"], 1);
            }
            _ => panic!("Expected ToolCall"),
        }

        match &ops2[0].op_type {
            crate::ops::OpType::ToolCall { tool, params } => {
                assert_eq!(tool, "rt2:tool");
                assert_eq!(params["id"], 2);
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_runtime_isolation_console_output() {
        // Create two runtimes with console output
        let mut rt1 = RealJsRuntime::new().unwrap();
        let mut rt2 = RealJsRuntime::new().unwrap();

        // Each logs its own message
        rt1.execute("console.log('from runtime 1')").unwrap();
        rt2.execute("console.log('from runtime 2')").unwrap();

        // Verify console outputs are independent
        let console1 = rt1.take_console_output();
        let console2 = rt2.take_console_output();

        assert_eq!(console1.len(), 1);
        assert_eq!(console2.len(), 1);
        assert_eq!(console1[0].message, "from runtime 1");
        assert_eq!(console2[0].message, "from runtime 2");
    }

    #[test]
    fn test_ops_created_in_promise_continuation() {
        // This tests the FEEDBACK.md issue: pending ops created in Promise
        // continuations must be surfaced after resolve()
        let mut rt = RealJsRuntime::new().unwrap();

        // Set up a chain: tool call -> on resolve, make another tool call
        rt.execute(
            r"
            globalThis.chainResult = null;
            __amla__.toolCall('first:call', {})
                .then(r => {
                    // This second call is made in the Promise continuation
                    return __amla__.toolCall('second:call', {first_result: r});
                })
                .then(r => {
                    globalThis.chainResult = r;
                });
        ",
        )
        .unwrap();

        // First call
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1, "Should have first pending op");
        assert!(matches!(&ops[0].op_type, OpType::ToolCall { tool, .. } if tool == "first:call"));

        // Resolve first call - this should trigger the second call in continuation
        rt.resolve(&ops[0].id, &serde_json::json!({"step": 1}))
            .unwrap();

        // CRITICAL: Second call should now be in pending ops
        let ops2 = rt.take_pending_ops();
        assert_eq!(
            ops2.len(),
            1,
            "Should have second pending op from continuation"
        );
        assert!(matches!(&ops2[0].op_type, OpType::ToolCall { tool, .. } if tool == "second:call"));

        // Resolve second call
        rt.resolve(&ops2[0].id, &serde_json::json!({"step": 2}))
            .unwrap();

        // Verify final result
        let result = rt.execute("globalThis.chainResult").unwrap();
        assert_eq!(result["step"], 2);
    }

    #[test]
    fn test_runtime_isolation_resolve_correct_promise() {
        // Create two runtimes with pending promises
        let mut rt1 = RealJsRuntime::new().unwrap();
        let mut rt2 = RealJsRuntime::new().unwrap();

        // Set up promise handlers
        rt1.execute(
            r"
            globalThis.rt1Result = null;
            __amla__.toolCall('rt1:tool', {}).then(r => { globalThis.rt1Result = r; });
        ",
        )
        .unwrap();

        rt2.execute(
            r"
            globalThis.rt2Result = null;
            __amla__.toolCall('rt2:tool', {}).then(r => { globalThis.rt2Result = r; });
        ",
        )
        .unwrap();

        // Get ops
        let ops1 = rt1.take_pending_ops();
        let ops2 = rt2.take_pending_ops();

        // Resolve with different values
        rt1.resolve(&ops1[0].id, &serde_json::json!({"from": "rt1"}))
            .unwrap();
        rt2.resolve(&ops2[0].id, &serde_json::json!({"from": "rt2"}))
            .unwrap();

        // Verify correct resolution
        let result1 = rt1.execute("globalThis.rt1Result").unwrap();
        let result2 = rt2.execute("globalThis.rt2Result").unwrap();

        assert_eq!(result1["from"], "rt1");
        assert_eq!(result2["from"], "rt2");
    }

    // =========================================================================
    // Memory Operations Tests
    // =========================================================================

    #[test]
    fn test_memory_read_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.memoryRead('user:prefs')").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::MemoryRead { key } => {
                assert_eq!(key, "user:prefs");
            }
            _ => panic!("Expected MemoryRead"),
        }
    }

    #[test]
    fn test_memory_write_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.memoryWrite('user:prefs', {theme: 'dark'})")
            .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::MemoryWrite { key, value } => {
                assert_eq!(key, "user:prefs");
                assert_eq!(value["theme"], "dark");
            }
            _ => panic!("Expected MemoryWrite"),
        }
    }

    #[test]
    fn test_memory_delete_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.memoryDelete('user:prefs')").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::MemoryDelete { key } => {
                assert_eq!(key, "user:prefs");
            }
            _ => panic!("Expected MemoryDelete"),
        }
    }

    #[test]
    fn test_memory_operations_resolve() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Memory read with resolve
        rt.execute(
            r"
            globalThis.memResult = null;
            __amla__.memoryRead('config').then(v => { globalThis.memResult = v; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(&ops[0].id, &serde_json::json!({"setting": "value"}))
            .unwrap();

        let result = rt.execute("globalThis.memResult").unwrap();
        assert_eq!(result["setting"], "value");
    }

    // =========================================================================
    // LLM Call Tests
    // =========================================================================

    #[test]
    fn test_llm_call_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"__amla__.llm('gpt-4', [{role: 'user', content: 'Hello'}], {temperature: 0.7})",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::LlmCall {
                model,
                messages,
                options,
            } => {
                assert_eq!(model, "gpt-4");
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].role, "user");
                assert_eq!(messages[0].content, "Hello");
                assert!((options.temperature.unwrap() - 0.7).abs() < 0.01);
            }
            _ => panic!("Expected LlmCall"),
        }
    }

    #[test]
    fn test_llm_call_resolve() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.llmResult = null;
            __amla__.llm('claude', [{role: 'user', content: 'Hi'}])
                .then(r => { globalThis.llmResult = r; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "content": "Hello! How can I help?",
                "model": "claude-3"
            }),
        )
        .unwrap();

        let result = rt.execute("globalThis.llmResult").unwrap();
        assert_eq!(result["content"], "Hello! How can I help?");
    }

    // =========================================================================
    // Spawn Tests
    // =========================================================================

    #[test]
    fn test_spawn_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            __amla__.spawn([
                {
                    capability: 'stripe:charge',
                    constraints: [
                        { type: 'le', param: 'amount', value: 1000 },
                        { type: 'eq', param: 'currency', value: 'USD' }
                    ]
                }
            ])
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Spawn { attenuations } => {
                assert_eq!(attenuations.len(), 1);
                assert_eq!(attenuations[0].capability, "stripe:charge");
                assert_eq!(attenuations[0].constraints.len(), 2);
            }
            _ => panic!("Expected Spawn"),
        }
    }

    #[test]
    fn test_spawn_with_param_builder() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            const P = __amla__.Param;
            __amla__.spawn([
                {
                    capability: 'payments:process',
                    constraints: [
                        P('amount').le(500),
                        P('region').in_(['US', 'EU'])
                    ]
                }
            ])
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Spawn { attenuations } => {
                assert_eq!(attenuations.len(), 1);
                assert_eq!(attenuations[0].capability, "payments:process");
                assert_eq!(attenuations[0].constraints.len(), 2);
            }
            _ => panic!("Expected Spawn"),
        }
    }

    // =========================================================================
    // Promise Rejection Tests
    // =========================================================================

    #[test]
    fn test_promise_rejection() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.rejected = false;
            globalThis.errorMsg = null;
            __amla__.toolCall('test:fail', {})
                .then(r => { globalThis.rejected = false; })
                .catch(e => {
                    globalThis.rejected = true;
                    globalThis.errorMsg = e.message;
                });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.reject(&ops[0].id, "Tool not found").unwrap();

        let rejected = rt.execute("globalThis.rejected").unwrap();
        let error_msg = rt.execute("globalThis.errorMsg").unwrap();

        assert_eq!(rejected, serde_json::json!(true));
        assert_eq!(error_msg, serde_json::json!("Tool not found"));
    }

    #[test]
    fn test_fetch_rejection() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.fetchError = null;
            __amla__.fetch('https://invalid.example')
                .catch(e => { globalThis.fetchError = e.message; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.reject(&ops[0].id, "Network error: connection refused")
            .unwrap();

        let error = rt.execute("globalThis.fetchError").unwrap();
        assert_eq!(error, "Network error: connection refused");
    }

    // =========================================================================
    // Fetch Tests
    // =========================================================================

    #[test]
    fn test_fetch_with_options() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            __amla__.fetch('https://api.example.com/data', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: '{"key": "value"}'
            })
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Fetch { url, options } => {
                assert_eq!(url, "https://api.example.com/data");
                assert_eq!(options.method, "POST");
                assert!(!options.headers.is_empty());
                assert_eq!(
                    options.headers.get("Content-Type"),
                    Some(&"application/json".to_string())
                );
            }
            _ => panic!("Expected Fetch"),
        }
    }

    #[test]
    fn test_fetch_resolve_with_response() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.fetchResult = null;
            fetch('https://api.example.com/users')
                .then(r => r.json())
                .then(data => { globalThis.fetchResult = data; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "ok": true,
                "status": 200,
                "statusText": "OK",
                "headers": {"content-type": "application/json"},
                "body": {"users": [{"id": 1, "name": "Alice"}]}
            }),
        )
        .unwrap();

        let result = rt.execute("globalThis.fetchResult").unwrap();
        assert_eq!(result["users"][0]["name"], "Alice");
    }

    // =========================================================================
    // Edge Cases and Error Handling
    // =========================================================================

    #[test]
    fn test_unicode_in_console() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r"console.log('Hello 世界 🌍 émojis')").unwrap();

        let output = rt.take_console_output();
        assert_eq!(output.len(), 1);
        assert!(output[0].message.contains("世界"));
        assert!(output[0].message.contains("🌍"));
    }

    #[test]
    fn test_unicode_in_tool_params() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r"__amla__.toolCall('search', {query: '日本語テスト'})")
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::ToolCall { params, .. } => {
                assert_eq!(params["query"], "日本語テスト");
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_nested_objects() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            __amla__.toolCall('complex', {
                level1: {
                    level2: {
                        level3: {
                            value: 'deep'
                        }
                    }
                }
            })
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::ToolCall { params, .. } => {
                assert_eq!(params["level1"]["level2"]["level3"]["value"], "deep");
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_array_params() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r"__amla__.toolCall('batch', {items: [1, 2, 3, 'four', {five: 5}]})")
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::ToolCall { params, .. } => {
                let items = params["items"].as_array().unwrap();
                assert_eq!(items.len(), 5);
                assert_eq!(items[3], "four");
                assert_eq!(items[4]["five"], 5);
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_special_characters_in_strings() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"console.log('Tab:\t Newline:\n Quote:\" Backslash:\\')"#)
            .unwrap();

        let output = rt.take_console_output();
        assert_eq!(output.len(), 1);
        assert!(output[0].message.contains("Tab:\t"));
        assert!(output[0].message.contains("Newline:\n"));
    }

    #[test]
    fn test_multiple_console_levels() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            console.log('info message');
            console.error('error message');
            console.warn('warning');
            console.debug('debug info');
        ",
        )
        .unwrap();

        let output = rt.take_console_output();
        assert_eq!(output.len(), 4);
        assert_eq!(output[0].level, "log");
        assert_eq!(output[1].level, "error");
        assert_eq!(output[2].level, "warn");
        assert_eq!(output[3].level, "debug");
    }

    #[test]
    fn test_js_exception_handling() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Syntax error
        let result = rt.execute("function( { }");
        assert!(result.is_err());

        // Reference error
        let result = rt.execute("undefinedVariable.property");
        assert!(result.is_err());

        // Type error
        let result = rt.execute("null.toString()");
        assert!(result.is_err());
    }

    #[test]
    fn test_multiple_ops_single_execution() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            __amla__.toolCall('tool1', {});
            __amla__.toolCall('tool2', {});
            __amla__.fetch('https://example.com');
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn test_empty_params() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.toolCall('noop', {})").unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::ToolCall { params, .. } => {
                assert!(params.as_object().unwrap().is_empty());
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn test_null_and_undefined_values() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            __amla__.toolCall('test', {
                nullVal: null,
                // undefined values typically get omitted in JSON
            })
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::ToolCall { params, .. } => {
                assert!(params["nullVal"].is_null());
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    // =========================================================================
    // Sleep/Timer Tests
    // =========================================================================

    #[test]
    fn test_sleep_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.sleep(1000)").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Sleep { delay_ms } => {
                assert_eq!(*delay_ms, 1000);
            }
            _ => panic!("Expected Sleep"),
        }
    }

    #[test]
    fn test_sleep_zero_delay() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.sleep(0)").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Sleep { delay_ms } => {
                assert_eq!(*delay_ms, 0);
            }
            _ => panic!("Expected Sleep"),
        }
    }

    #[test]
    fn test_sleep_resolve() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.sleepDone = false;
            __amla__.sleep(100).then(() => { globalThis.sleepDone = true; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);

        // Resolve the sleep
        rt.resolve(&ops[0].id, &serde_json::json!(null)).unwrap();

        let result = rt.execute("globalThis.sleepDone").unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn test_set_timeout_creates_sleep() {
        let mut rt = RealJsRuntime::new().unwrap();

        // setTimeout should create a sleep pending op
        rt.execute(
            r"
            globalThis.timerFired = false;
            setTimeout(() => { globalThis.timerFired = true; }, 500);
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Sleep { delay_ms } => {
                assert_eq!(*delay_ms, 500);
            }
            _ => panic!("Expected Sleep for setTimeout"),
        }
    }

    #[test]
    fn test_set_timeout_callback_fires() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.timerResult = '';
            setTimeout((a, b) => { globalThis.timerResult = a + b; }, 100, 'Hello', 'World');
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);

        // Resolve the sleep - callback should fire
        rt.resolve(&ops[0].id, &serde_json::json!(null)).unwrap();

        let result = rt.execute("globalThis.timerResult").unwrap();
        assert_eq!(result, serde_json::json!("HelloWorld"));
    }

    #[test]
    fn test_clear_timeout() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.timerFired = false;
            const id = setTimeout(() => { globalThis.timerFired = true; }, 100);
            clearTimeout(id);
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);

        // Resolve the sleep - callback should NOT fire due to clearTimeout
        rt.resolve(&ops[0].id, &serde_json::json!(null)).unwrap();

        let result = rt.execute("globalThis.timerFired").unwrap();
        assert_eq!(result, serde_json::json!(false));
    }

    #[test]
    fn test_set_interval_creates_sleep() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.intervalCount = 0;
            setInterval(() => { globalThis.intervalCount++; }, 200);
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Sleep { delay_ms } => {
                assert_eq!(*delay_ms, 200);
            }
            _ => panic!("Expected Sleep for setInterval"),
        }
    }

    #[test]
    fn test_set_interval_repeats() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.intervalCount = 0;
            setInterval(() => { globalThis.intervalCount++; }, 100);
        ",
        )
        .unwrap();

        // First sleep
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        rt.resolve(&ops[0].id, &serde_json::json!(null)).unwrap();

        let count = rt.execute("globalThis.intervalCount").unwrap();
        assert_eq!(count, serde_json::json!(1));

        // Second sleep should be created
        let ops2 = rt.take_pending_ops();
        assert_eq!(ops2.len(), 1);
        rt.resolve(&ops2[0].id, &serde_json::json!(null)).unwrap();

        let count2 = rt.execute("globalThis.intervalCount").unwrap();
        assert_eq!(count2, serde_json::json!(2));
    }

    #[test]
    fn test_clear_interval() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.intervalCount = 0;
            const id = setInterval(() => { globalThis.intervalCount++; }, 100);
            // We'll clear after first execution in the test
            globalThis.intervalId = id;
        ",
        )
        .unwrap();

        // First tick
        let ops = rt.take_pending_ops();
        rt.resolve(&ops[0].id, &serde_json::json!(null)).unwrap();

        // Now clear
        rt.execute("clearInterval(globalThis.intervalId)").unwrap();

        // Second tick should be scheduled but callback won't fire
        let ops2 = rt.take_pending_ops();
        assert_eq!(ops2.len(), 1);
        rt.resolve(&ops2[0].id, &serde_json::json!(null)).unwrap();

        // No third tick should be scheduled
        let ops3 = rt.take_pending_ops();
        assert_eq!(ops3.len(), 0);

        // Count should stay at 1
        let count = rt.execute("globalThis.intervalCount").unwrap();
        assert_eq!(count, serde_json::json!(1));
    }

    // =========================================================================
    // File System Tests
    // =========================================================================

    #[test]
    fn test_fs_read_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.readFile('/path/to/file.txt')")
            .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::FsRead { path, options } => {
                assert_eq!(path, "/path/to/file.txt");
                assert!(options.encoding.is_none());
            }
            _ => panic!("Expected FsRead"),
        }
    }

    #[test]
    fn test_fs_read_with_encoding() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.readFile('/data.bin', { encoding: 'base64' })")
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::FsRead { path, options } => {
                assert_eq!(path, "/data.bin");
                assert_eq!(options.encoding.as_deref(), Some("base64"));
            }
            _ => panic!("Expected FsRead"),
        }
    }

    #[test]
    fn test_fs_read_resolve() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.fileContent = null;
            __amla__.fs.readFile('/test.txt')
                .then(data => { globalThis.fileContent = data; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(&ops[0].id, &serde_json::json!("Hello, World!"))
            .unwrap();

        let result = rt.execute("globalThis.fileContent").unwrap();
        assert_eq!(result, serde_json::json!("Hello, World!"));
    }

    #[test]
    fn test_fs_write_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.writeFile('/output.txt', 'file content')")
            .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::FsWrite {
                path,
                data,
                options,
            } => {
                assert_eq!(path, "/output.txt");
                assert_eq!(data, "file content");
                assert!(!options.append);
                assert!(!options.create_dirs);
            }
            _ => panic!("Expected FsWrite"),
        }
    }

    #[test]
    fn test_fs_write_with_options() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            __amla__.fs.writeFile('/log.txt', 'new line\n', {
                append: true,
                create_dirs: true,
                encoding: 'utf-8'
            })
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::FsWrite { options, .. } => {
                assert!(options.append);
                assert!(options.create_dirs);
                assert_eq!(options.encoding.as_deref(), Some("utf-8"));
            }
            _ => panic!("Expected FsWrite"),
        }
    }

    #[test]
    fn test_fs_readdir_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.readDir('/home/user')").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::FsReadDir { path } => {
                assert_eq!(path, "/home/user");
            }
            _ => panic!("Expected FsReadDir"),
        }
    }

    #[test]
    fn test_fs_readdir_resolve() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.files = null;
            __amla__.fs.readDir('/home')
                .then(entries => { globalThis.files = entries; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(
            &ops[0].id,
            &serde_json::json!(["file1.txt", "file2.txt", "subdir"]),
        )
        .unwrap();

        let result = rt.execute("globalThis.files").unwrap();
        assert_eq!(
            result,
            serde_json::json!(["file1.txt", "file2.txt", "subdir"])
        );
    }

    #[test]
    fn test_fs_stat_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.stat('/some/file.txt')").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::FsStat { path } => {
                assert_eq!(path, "/some/file.txt");
            }
            _ => panic!("Expected FsStat"),
        }
    }

    #[test]
    fn test_fs_stat_resolve() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.statResult = null;
            __amla__.fs.stat('/file.txt')
                .then(s => { globalThis.statResult = s; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "size": 1024,
                "isDirectory": false,
                "isFile": true,
                "modifiedAt": 1234567890000_i64
            }),
        )
        .unwrap();

        let result = rt.execute("globalThis.statResult").unwrap();
        assert_eq!(result["size"], 1024);
        assert_eq!(result["isFile"], true);
    }

    #[test]
    fn test_fs_exists_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.exists('/maybe/exists.txt')")
            .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::FsExists { path } => {
                assert_eq!(path, "/maybe/exists.txt");
            }
            _ => panic!("Expected FsExists"),
        }
    }

    #[test]
    fn test_fs_exists_resolve() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.exists = null;
            __amla__.fs.exists('/test')
                .then(e => { globalThis.exists = e; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(&ops[0].id, &serde_json::json!(true)).unwrap();

        let result = rt.execute("globalThis.exists").unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    #[test]
    fn test_fs_unlink_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.unlink('/delete/me.txt')").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::FsUnlink { path } => {
                assert_eq!(path, "/delete/me.txt");
            }
            _ => panic!("Expected FsUnlink"),
        }
    }

    #[test]
    fn test_fs_mkdir_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.mkdir('/new/directory')").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::FsMkdir { path, recursive } => {
                assert_eq!(path, "/new/directory");
                assert!(!*recursive);
            }
            _ => panic!("Expected FsMkdir"),
        }
    }

    #[test]
    fn test_fs_mkdir_recursive() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.fs.mkdir('/deep/nested/path', { recursive: true })")
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::FsMkdir { path, recursive } => {
                assert_eq!(path, "/deep/nested/path");
                assert!(*recursive);
            }
            _ => panic!("Expected FsMkdir"),
        }
    }

    #[test]
    fn test_fs_error_handling() {
        let mut rt = RealJsRuntime::new().unwrap();

        rt.execute(
            r"
            globalThis.fsError = null;
            __amla__.fs.readFile('/nonexistent')
                .catch(e => { globalThis.fsError = e.message; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.reject(&ops[0].id, "ENOENT: no such file or directory")
            .unwrap();

        let result = rt.execute("globalThis.fsError").unwrap();
        assert_eq!(
            result,
            serde_json::json!("ENOENT: no such file or directory")
        );
    }

    #[test]
    fn test_multiple_fs_ops() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            __amla__.fs.readFile('/file1.txt');
            __amla__.fs.writeFile('/file2.txt', 'data');
            __amla__.fs.exists('/file3.txt');
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 3);
        assert!(matches!(&ops[0].op_type, OpType::FsRead { .. }));
        assert!(matches!(&ops[1].op_type, OpType::FsWrite { .. }));
        assert!(matches!(&ops[2].op_type, OpType::FsExists { .. }));
    }

    #[test]
    fn test_fs_chain_operations() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Chain: read file -> process -> write result
        rt.execute(
            r"
            globalThis.chainComplete = false;
            __amla__.fs.readFile('/input.txt')
                .then(data => {
                    return __amla__.fs.writeFile('/output.txt', data.toUpperCase());
                })
                .then(() => {
                    globalThis.chainComplete = true;
                });
        ",
        )
        .unwrap();

        // First op: read
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        rt.resolve(&ops[0].id, &serde_json::json!("hello world"))
            .unwrap();

        // Second op: write (from continuation)
        let ops2 = rt.take_pending_ops();
        assert_eq!(ops2.len(), 1);
        match &ops2[0].op_type {
            OpType::FsWrite { data, .. } => {
                assert_eq!(data, "HELLO WORLD");
            }
            _ => panic!("Expected FsWrite"),
        }

        rt.resolve(&ops2[0].id, &serde_json::json!(null)).unwrap();

        let result = rt.execute("globalThis.chainComplete").unwrap();
        assert_eq!(result, serde_json::json!(true));
    }

    // =========================================================================
    // Invalid Operation Error Handling Tests
    // =========================================================================

    #[test]
    fn test_invalid_op_type_rejects_promise() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Try to create an invalid op type - should reject immediately
        rt.execute(
            r"
            globalThis.errorMsg = null;
            globalThis.caught = false;
            __amla__._createPendingOp('invalid_operation_type', {})
                .catch(e => {
                    globalThis.caught = true;
                    globalThis.errorMsg = e.message;
                });
        ",
        )
        .unwrap();

        // Run microtask queue to trigger the catch handler
        rt.run_pending_jobs().unwrap();

        // The promise should have been rejected
        let caught = rt.execute("globalThis.caught").unwrap();
        let error_msg = rt.execute("globalThis.errorMsg").unwrap();

        assert_eq!(caught, serde_json::json!(true));
        assert!(
            error_msg
                .as_str()
                .unwrap()
                .contains("Unknown operation type"),
            "Expected 'Unknown operation type' error, got: {error_msg}"
        );

        // No pending ops should have been registered
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 0, "Invalid op should not be registered");
    }

    #[test]
    fn test_missing_required_field_rejects_promise() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Tool call without 'tool' field
        rt.execute(
            r"
            globalThis.errorMsg = null;
            __amla__._createPendingOp('tool_call', { params: {} })
                .catch(e => { globalThis.errorMsg = e.message; });
        ",
        )
        .unwrap();

        // Run microtask queue to trigger the catch handler
        rt.run_pending_jobs().unwrap();

        let error_msg = rt.execute("globalThis.errorMsg").unwrap();
        assert!(
            error_msg.as_str().unwrap().contains("tool"),
            "Expected missing 'tool' error, got: {error_msg}"
        );

        // No pending ops
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 0);
    }

    #[test]
    fn test_invalid_messages_format_rejects() {
        let mut rt = RealJsRuntime::new().unwrap();

        // LLM call with invalid messages format (not an array of message objects)
        rt.execute(
            r"
            globalThis.errorMsg = null;
            __amla__._createPendingOp('llm_call', {
                model: 'test',
                messages: 'not an array'
            }).catch(e => { globalThis.errorMsg = e.message; });
        ",
        )
        .unwrap();

        // Run microtask queue to trigger the catch handler
        rt.run_pending_jobs().unwrap();

        let error_msg = rt.execute("globalThis.errorMsg").unwrap();
        assert!(
            error_msg.as_str().unwrap().contains("messages"),
            "Expected invalid messages error, got: {error_msg}"
        );
    }

    // =========================================================================
    // Shell Tests (internal sandboxed shell)
    // =========================================================================

    #[test]
    fn test_shell_pending_op() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute("__amla__.shell('echo hello world')").unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "echo hello world");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_resolve() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            globalThis.shellResult = null;
            __amla__.shell('echo hello').then(r => { globalThis.shellResult = r; });
        ",
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        let response = serde_json::json!({
            "stdout": "hello\n",
            "stderr": "",
            "exitCode": 0
        });
        rt.resolve(&ops[0].id, &response).unwrap();

        let result = rt.execute("globalThis.shellResult").unwrap();
        assert_eq!(result["stdout"], "hello\n");
        assert_eq!(result["exitCode"], 0);
    }

    #[test]
    fn test_shell_rejects_non_string() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r"
            globalThis.shellError = null;
            __amla__.shell(123).catch(e => { globalThis.shellError = e.message; });
        ",
        )
        .unwrap();

        rt.run_pending_jobs().unwrap();

        let error = rt.execute("globalThis.shellError").unwrap();
        assert!(error.as_str().unwrap().contains("command must be a string"));
    }

    #[test]
    fn test_shell_with_pipes_and_redirects() {
        // Shell metacharacters are passed through - the internal shell interpreter
        // decides how to handle them (it's sandboxed anyway)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell('cat file.txt | grep pattern')"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "cat file.txt | grep pattern");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_output_redirect() {
        // Test output redirection (>)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("echo hello > output.txt")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "echo hello > output.txt");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_append_redirect() {
        // Test append redirection (>>)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("echo line >> output.txt")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "echo line >> output.txt");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_input_redirect() {
        // Test input redirection (<)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("sort < input.txt")"#).unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "sort < input.txt");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_pipe_chain() {
        // Test multi-stage pipeline
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("cat file.txt | grep pattern | sort | uniq -c")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "cat file.txt | grep pattern | sort | uniq -c");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_combined_redirects() {
        // Test combining pipes and redirects
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("cat < input.txt | grep pattern > output.txt")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "cat < input.txt | grep pattern > output.txt");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_stderr_redirect() {
        // Test stderr redirection (2>)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("command 2> error.log")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "command 2> error.log");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_stderr_to_stdout() {
        // Test redirecting stderr to stdout (2>&1)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("command 2>&1 | grep error")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "command 2>&1 | grep error");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_heredoc_style() {
        // Test heredoc-like multiline commands
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("cat << 'EOF'\nline1\nline2\nEOF")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert!(command.contains("EOF"));
                assert!(command.contains("line1"));
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_command_substitution() {
        // Test command substitution $()
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("echo $(date)")"#).unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "echo $(date)");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_variable_expansion() {
        // Test variable expansion
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("echo $HOME")"#).unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "echo $HOME");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_quoted_strings() {
        // Test quoted strings with special characters
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("echo 'hello | world' > file.txt")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "echo 'hello | world' > file.txt");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_semicolon_sequence() {
        // Test command sequence with semicolons
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("echo one; echo two; echo three")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "echo one; echo two; echo three");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_and_operator() {
        // Test && (and) operator
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("mkdir dir && cd dir && touch file.txt")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "mkdir dir && cd dir && touch file.txt");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_or_operator() {
        // Test || (or) operator
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("test -f file.txt || echo 'not found'")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "test -f file.txt || echo 'not found'");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_backgrounding() {
        // Test background operator (&)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("long_running_command &")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "long_running_command &");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_glob_patterns() {
        // Test glob patterns
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("ls *.txt")"#).unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "ls *.txt");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_complex_pipeline() {
        // Test complex real-world pipeline
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("find . -name '*.js' | xargs grep -l 'TODO' | head -10")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(
                    command,
                    "find . -name '*.js' | xargs grep -l 'TODO' | head -10"
                );
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_node_applet() {
        // Running "node -e 'code'" should invoke the sandboxed JS interpreter
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.shell("node -e 'console.log(1+1)'")"#)
            .unwrap();

        let ops = rt.take_pending_ops();
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert_eq!(command, "node -e 'console.log(1+1)'");
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_node_multiple_isolated() {
        // Multiple node invocations should be isolated from each other
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.results = [];

            // First node call sets a global
            __amla__.shell("node -e 'globalThis.x = 42; console.log(x)'")
                .then(r => globalThis.results.push(r));

            // Second node call should NOT see the global from first call
            __amla__.shell("node -e 'console.log(typeof x)'")
                .then(r => globalThis.results.push(r));

            // Third node call with different computation
            __amla__.shell("node -e 'console.log(2 + 2)'")
                .then(r => globalThis.results.push(r));
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 3, "Should have 3 shell operations");

        // Verify all are shell commands for node
        for op in &ops {
            match &op.op_type {
                OpType::Shell { command } => {
                    assert!(command.starts_with("node -e"));
                }
                _ => panic!("Expected Shell"),
            }
        }

        // Simulate isolated responses (as if each ran in separate context)
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({"stdout": "42\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();
        rt.resolve(
            &ops[1].id,
            &serde_json::json!({"stdout": "undefined\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();
        rt.resolve(
            &ops[2].id,
            &serde_json::json!({"stdout": "4\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();

        let results = rt.execute("globalThis.results").unwrap();
        assert_eq!(results[0]["stdout"], "42\n");
        assert_eq!(results[1]["stdout"], "undefined\n"); // x not visible - isolated!
        assert_eq!(results[2]["stdout"], "4\n");
    }

    #[test]
    fn test_shell_node_nested() {
        // node calling shell calling node - turtles all the way down
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"__amla__.shell("node -e 'const r = await shell(\"echo inner\"); console.log(r.stdout)'")"#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert!(command.contains("shell"));
                assert!(command.contains("echo inner"));
            }
            _ => panic!("Expected Shell"),
        }
    }

    #[test]
    fn test_shell_concurrent_node_isolation() {
        // Concurrent node processes should be fully isolated
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            // Launch multiple node processes concurrently
            Promise.all([
                __amla__.shell("node -e 'globalThis.id = 1; console.log(id)'"),
                __amla__.shell("node -e 'globalThis.id = 2; console.log(id)'"),
                __amla__.shell("node -e 'globalThis.id = 3; console.log(id)'"),
            ]).then(results => {
                globalThis.allResults = results;
            });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 3);

        // Each should see its own id, not polluted by others
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({"stdout": "1\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();
        rt.resolve(
            &ops[1].id,
            &serde_json::json!({"stdout": "2\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();
        rt.resolve(
            &ops[2].id,
            &serde_json::json!({"stdout": "3\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();

        let results = rt.execute("globalThis.allResults").unwrap();
        assert_eq!(results[0]["stdout"], "1\n");
        assert_eq!(results[1]["stdout"], "2\n");
        assert_eq!(results[2]["stdout"], "3\n");
    }

    #[test]
    fn test_shell_deep_nesting() {
        // Deep nesting: node → shell → node → shell → node
        // Each level should be isolated and work correctly
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.result = null;
            // Level 1: node calls shell
            __amla__.shell("node -e 'const r = await shell(\"node -e \\\"console.log(42)\\\"\"); console.log(r.stdout.trim())'")
                .then(r => { globalThis.result = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);

        // The command string shows the nesting structure
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert!(command.contains("node -e"));
                assert!(command.contains("shell"));
                assert!(command.contains("console.log(42)"));
            }
            _ => panic!("Expected Shell"),
        }

        // Simulate deep execution result (as if all levels ran)
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({"stdout": "42\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();

        let result = rt.execute("globalThis.result").unwrap();
        assert_eq!(result["stdout"], "42\n");
        assert_eq!(result["exitCode"], 0);
    }

    #[test]
    fn test_shell_five_levels_deep() {
        // 5 levels: node → shell → node → shell → node → shell → echo
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.result = null;
            __amla__.shell(`node -e "
                const r1 = await shell('node -e \\\"const r2 = await shell(\\\\\\\"echo level5\\\\\\\"); console.log(r2.stdout.trim())\\\"');
                console.log('level3:', r1.stdout.trim());
            "`).then(r => { globalThis.result = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);

        // Verify it's a deeply nested shell command
        match &ops[0].op_type {
            OpType::Shell { command } => {
                assert!(command.contains("node -e"));
                assert!(command.contains("shell"));
            }
            _ => panic!("Expected Shell"),
        }

        // Simulate the final result after all levels execute
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({"stdout": "level3: level5\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();

        let result = rt.execute("globalThis.result").unwrap();
        assert_eq!(result["exitCode"], 0);
    }

    #[test]
    fn test_shell_race_with_promise_race() {
        // Promise.race between shell commands - first to complete wins
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.winner = null;
            Promise.race([
                __amla__.shell("sleep 1 && echo slow"),
                __amla__.shell("echo fast"),
                __amla__.shell("sleep 2 && echo slower"),
            ]).then(r => { globalThis.winner = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 3);

        // Complete the fast one first
        rt.resolve(
            &ops[1].id,
            &serde_json::json!({"stdout": "fast\n", "stderr": "", "exitCode": 0}),
        )
        .unwrap();

        // The race should be won
        let winner = rt.execute("globalThis.winner").unwrap();
        assert_eq!(winner["stdout"], "fast\n");

        // Other ops are still pending but race is complete
    }

    #[test]
    fn test_shell_race_first_failure() {
        // Promise.race where first completion is a failure
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.raceResult = null;
            globalThis.raceError = null;
            Promise.race([
                __amla__.shell("failing-command"),
                __amla__.shell("echo success"),
            ])
            .then(r => { globalThis.raceResult = r; })
            .catch(e => { globalThis.raceError = e.message; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 2);

        // First one fails
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({"stdout": "", "stderr": "command not found\n", "exitCode": 127}),
        )
        .unwrap();

        // Race completes with the failure result (not an exception, just non-zero exit)
        let result = rt.execute("globalThis.raceResult").unwrap();
        assert_eq!(result["exitCode"], 127);
    }

    #[test]
    fn test_shell_timeout_race_pattern() {
        // Common pattern: race a slow operation against a timeout
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.result = null;
            Promise.race([
                __amla__.shell("slow-operation"),
                __amla__.sleep(1000).then(() => ({ timeout: true })),
            ]).then(r => { globalThis.result = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 2);

        // Find the sleep op and resolve it first (timeout wins)
        let sleep_idx = ops
            .iter()
            .position(|op| matches!(op.op_type, OpType::Sleep { .. }))
            .unwrap();
        rt.resolve(&ops[sleep_idx].id, &serde_json::json!(null))
            .unwrap();

        // Timeout won the race
        let result = rt.execute("globalThis.result").unwrap();
        assert_eq!(result["timeout"], true);
    }

    #[test]
    fn test_shell_vfs_visibility() {
        // Test that file changes in one shell are visible to other shells
        // (VFS is shared, state is isolated)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.results = [];

            // First shell creates a file
            __amla__.shell("echo 'hello from shell 1' > /tmp/shared.txt")
                .then(r => {
                    globalThis.results.push({ step: 1, result: r });
                    // Second shell reads the file created by first shell
                    return __amla__.shell("cat /tmp/shared.txt");
                })
                .then(r => {
                    globalThis.results.push({ step: 2, result: r });
                    // Third shell appends to the file
                    return __amla__.shell("echo 'from shell 3' >> /tmp/shared.txt");
                })
                .then(r => {
                    globalThis.results.push({ step: 3, result: r });
                    // Fourth shell reads the combined content
                    return __amla__.shell("cat /tmp/shared.txt");
                })
                .then(r => {
                    globalThis.results.push({ step: 4, result: r });
                });
        "#,
        )
        .unwrap();

        // Step 1: Create file
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0].op_type, OpType::Shell { .. }));
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "",
                "stderr": "",
                "exitCode": 0
            }),
        )
        .unwrap();

        // Step 2: Read file
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "hello from shell 1\n",
                "stderr": "",
                "exitCode": 0
            }),
        )
        .unwrap();

        // Step 3: Append to file
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "",
                "stderr": "",
                "exitCode": 0
            }),
        )
        .unwrap();

        // Step 4: Read combined content
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "hello from shell 1\nfrom shell 3\n",
                "stderr": "",
                "exitCode": 0
            }),
        )
        .unwrap();

        // Verify the sequence of results
        let results = rt.execute("globalThis.results").unwrap();
        assert_eq!(results.as_array().unwrap().len(), 4);

        // Verify step 2 read the content from step 1
        assert_eq!(results[1]["result"]["stdout"], "hello from shell 1\n");

        // Verify step 4 read the combined content
        assert_eq!(
            results[3]["result"]["stdout"],
            "hello from shell 1\nfrom shell 3\n"
        );
    }

    #[test]
    fn test_shell_exit_code_success() {
        // Test that exit code 0 is properly returned
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.result = null;
            __amla__.shell("echo success").then(r => { globalThis.result = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "success\n",
                "stderr": "",
                "exitCode": 0
            }),
        )
        .unwrap();

        let result = rt.execute("globalThis.result").unwrap();
        assert_eq!(result["exitCode"], 0);
        assert_eq!(result["stdout"], "success\n");
        assert_eq!(result["stderr"], "");
    }

    #[test]
    fn test_shell_exit_code_failure() {
        // Test that non-zero exit codes are properly returned
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.result = null;
            __amla__.shell("exit 1").then(r => { globalThis.result = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "",
                "stderr": "",
                "exitCode": 1
            }),
        )
        .unwrap();

        let result = rt.execute("globalThis.result").unwrap();
        assert_eq!(result["exitCode"], 1);
    }

    #[test]
    fn test_shell_exit_code_various() {
        // Test various exit codes (1-255)
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.results = [];
            __amla__.shell("exit 0").then(r => { globalThis.results.push(r.exitCode); });
            __amla__.shell("exit 1").then(r => { globalThis.results.push(r.exitCode); });
            __amla__.shell("exit 42").then(r => { globalThis.results.push(r.exitCode); });
            __amla__.shell("exit 127").then(r => { globalThis.results.push(r.exitCode); });
            __amla__.shell("exit 255").then(r => { globalThis.results.push(r.exitCode); });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 5);

        // Resolve with different exit codes
        let exit_codes = [0, 1, 42, 127, 255];
        for (i, &code) in exit_codes.iter().enumerate() {
            rt.resolve(
                &ops[i].id,
                &serde_json::json!({
                    "stdout": "",
                    "stderr": "",
                    "exitCode": code
                }),
            )
            .unwrap();
        }

        let results = rt.execute("globalThis.results").unwrap();
        let arr = results.as_array().unwrap();
        assert_eq!(arr.len(), 5);
        assert_eq!(arr[0], 0);
        assert_eq!(arr[1], 1);
        assert_eq!(arr[2], 42);
        assert_eq!(arr[3], 127);
        assert_eq!(arr[4], 255);
    }

    #[test]
    fn test_shell_exit_code_with_output() {
        // Test that exit code, stdout, and stderr are all properly captured
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.result = null;
            __amla__.shell("command-with-all-outputs").then(r => { globalThis.result = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "normal output\n",
                "stderr": "error output\n",
                "exitCode": 2
            }),
        )
        .unwrap();

        let result = rt.execute("globalThis.result").unwrap();
        assert_eq!(result["exitCode"], 2);
        assert_eq!(result["stdout"], "normal output\n");
        assert_eq!(result["stderr"], "error output\n");
    }

    #[test]
    fn test_shell_exit_code_conditional_logic() {
        // Test using exit codes for conditional logic in JS
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.message = null;
            __amla__.shell("check-something")
                .then(r => {
                    if (r.exitCode === 0) {
                        globalThis.message = "Command succeeded";
                    } else if (r.exitCode === 1) {
                        globalThis.message = "Command failed";
                    } else {
                        globalThis.message = "Command errored with code " + r.exitCode;
                    }
                });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();

        // Test success case
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "",
                "stderr": "",
                "exitCode": 0
            }),
        )
        .unwrap();

        let msg = rt.execute("globalThis.message").unwrap();
        assert_eq!(msg.as_str().unwrap(), "Command succeeded");

        // Test failure case
        rt.execute(
            r#"
            globalThis.message = null;
            __amla__.shell("check-something-else")
                .then(r => {
                    if (r.exitCode === 0) {
                        globalThis.message = "Command succeeded";
                    } else if (r.exitCode === 1) {
                        globalThis.message = "Command failed";
                    } else {
                        globalThis.message = "Command errored with code " + r.exitCode;
                    }
                });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "",
                "stderr": "",
                "exitCode": 1
            }),
        )
        .unwrap();

        let msg = rt.execute("globalThis.message").unwrap();
        assert_eq!(msg.as_str().unwrap(), "Command failed");

        // Test error case
        rt.execute(
            r#"
            globalThis.message = null;
            __amla__.shell("check-third")
                .then(r => {
                    if (r.exitCode === 0) {
                        globalThis.message = "Command succeeded";
                    } else if (r.exitCode === 1) {
                        globalThis.message = "Command failed";
                    } else {
                        globalThis.message = "Command errored with code " + r.exitCode;
                    }
                });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "stdout": "",
                "stderr": "",
                "exitCode": 127
            }),
        )
        .unwrap();

        let msg = rt.execute("globalThis.message").unwrap();
        assert_eq!(msg.as_str().unwrap(), "Command errored with code 127");
    }

    // =========================================================================
    // Coverage tests for error paths and edge cases
    // =========================================================================

    #[test]
    fn test_has_pending_jobs() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Initially no pending jobs
        assert!(!rt.has_pending_jobs());

        // After creating a promise, there may be pending jobs
        rt.execute("Promise.resolve().then(() => {})").unwrap();
        // Note: jobs might be processed immediately by run_pending_jobs in execute
    }

    #[test]
    fn test_default_impl() {
        // Test the Default implementation
        let rt = RealJsRuntime::default();
        assert!(!rt.has_pending_jobs());
    }

    #[test]
    fn test_reject_operation() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            __amla__.toolCall("test", {}).catch(e => { globalThis.error = e.message; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);

        // Reject the operation
        rt.reject(&ops[0].id, "Something went wrong").unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.as_str().unwrap(), "Something went wrong");
    }

    #[test]
    fn test_reject_with_special_characters() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            __amla__.toolCall("test", {}).catch(e => { globalThis.error = e.message; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        rt.reject(&ops[0].id, "Error with \"quotes\" and \nnewlines")
            .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert!(error.as_str().unwrap().contains("quotes"));
    }

    #[test]
    fn test_invalid_op_missing_type() {
        // Test that missing operation type is rejected
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            // Manually call _createPendingOp with no type
            const id = String(__amla__._nextOpId++);
            let resolver, rejecter;
            const promise = new Promise((resolve, reject) => {
                resolver = resolve;
                rejecter = reject;
            });
            __amla__._pendingOps.set(id, { resolver, rejecter });
            // Call native without 'type' field
            const result = __native_register_op(id, JSON.stringify({ noType: true }));
            if (result && result.error) {
                __amla__._pendingOps.delete(id);
                globalThis.error = result.error;
            }
        "#,
        )
        .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.as_str().unwrap(), "Missing operation type");
    }

    #[test]
    fn test_invalid_op_unknown_type() {
        // Test that unknown operation type is rejected
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            const id = String(__amla__._nextOpId++);
            let resolver, rejecter;
            const promise = new Promise((resolve, reject) => {
                resolver = resolve;
                rejecter = reject;
            });
            __amla__._pendingOps.set(id, { resolver, rejecter });
            const result = __native_register_op(id, JSON.stringify({ type: "unknown_op_type" }));
            if (result && result.error) {
                __amla__._pendingOps.delete(id);
                globalThis.error = result.error;
            }
        "#,
        )
        .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(
            error.as_str().unwrap(),
            "Unknown operation type: unknown_op_type"
        );
    }

    #[test]
    fn test_invalid_tool_call_missing_tool() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            const id = String(__amla__._nextOpId++);
            let resolver, rejecter;
            const promise = new Promise((resolve, reject) => {
                resolver = resolve;
                rejecter = reject;
            });
            __amla__._pendingOps.set(id, { resolver, rejecter });
            // tool_call without 'tool' field
            const result = __native_register_op(id, JSON.stringify({ type: "tool_call", params: {} }));
            if (result && result.error) {
                __amla__._pendingOps.delete(id);
                globalThis.error = result.error;
            }
        "#,
        )
        .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.as_str().unwrap(), "Missing or invalid field: tool");
    }

    #[test]
    fn test_invalid_tool_call_missing_params() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            const id = String(__amla__._nextOpId++);
            let resolver, rejecter;
            const promise = new Promise((resolve, reject) => {
                resolver = resolve;
                rejecter = reject;
            });
            __amla__._pendingOps.set(id, { resolver, rejecter });
            // tool_call without 'params' field
            const result = __native_register_op(id, JSON.stringify({ type: "tool_call", tool: "test" }));
            if (result && result.error) {
                __amla__._pendingOps.delete(id);
                globalThis.error = result.error;
            }
        "#,
        )
        .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.as_str().unwrap(), "Missing field: params");
    }

    #[test]
    fn test_invalid_sleep_missing_delay() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            const id = String(__amla__._nextOpId++);
            let resolver, rejecter;
            const promise = new Promise((resolve, reject) => {
                resolver = resolve;
                rejecter = reject;
            });
            __amla__._pendingOps.set(id, { resolver, rejecter });
            // sleep without delay_ms
            const result = __native_register_op(id, JSON.stringify({ type: "sleep" }));
            if (result && result.error) {
                __amla__._pendingOps.delete(id);
                globalThis.error = result.error;
            }
        "#,
        )
        .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.as_str().unwrap(), "Missing or invalid delay_ms");
    }

    #[test]
    fn test_invalid_sleep_non_numeric_delay() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            const id = String(__amla__._nextOpId++);
            let resolver, rejecter;
            const promise = new Promise((resolve, reject) => {
                resolver = resolve;
                rejecter = reject;
            });
            __amla__._pendingOps.set(id, { resolver, rejecter });
            // sleep with non-numeric delay_ms
            const result = __native_register_op(id, JSON.stringify({ type: "sleep", delay_ms: "not a number" }));
            if (result && result.error) {
                __amla__._pendingOps.delete(id);
                globalThis.error = result.error;
            }
        "#,
        )
        .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.as_str().unwrap(), "Missing or invalid delay_ms");
    }

    #[test]
    fn test_invalid_spawn_bad_attenuations() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            const id = String(__amla__._nextOpId++);
            let resolver, rejecter;
            const promise = new Promise((resolve, reject) => {
                resolver = resolve;
                rejecter = reject;
            });
            __amla__._pendingOps.set(id, { resolver, rejecter });
            // spawn with invalid attenuations (not an array)
            const result = __native_register_op(id, JSON.stringify({ type: "spawn", attenuations: "not an array" }));
            if (result && result.error) {
                __amla__._pendingOps.delete(id);
                globalThis.error = result.error;
            }
        "#,
        )
        .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.as_str().unwrap(), "Invalid attenuations format");
    }

    #[test]
    fn test_spawn_with_valid_attenuations() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.result = null;
            __amla__.spawn([{ capability: "test:call", constraints: [] }]).then(r => { globalThis.result = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0].op_type, OpType::Spawn { .. }));
    }

    #[test]
    fn test_llm_call_invalid_messages() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.error = null;
            const id = String(__amla__._nextOpId++);
            let resolver, rejecter;
            const promise = new Promise((resolve, reject) => {
                resolver = resolve;
                rejecter = reject;
            });
            __amla__._pendingOps.set(id, { resolver, rejecter });
            // llm_call with invalid messages (not an array)
            const result = __native_register_op(id, JSON.stringify({
                type: "llm_call",
                model: "gpt-4",
                messages: "not an array"
            }));
            if (result && result.error) {
                __amla__._pendingOps.delete(id);
                globalThis.error = result.error;
            }
        "#,
        )
        .unwrap();

        let error = rt.execute("globalThis.error").unwrap();
        assert_eq!(error.as_str().unwrap(), "Invalid messages format");
    }

    #[test]
    fn test_llm_call_valid() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.result = null;
            __amla__.llm("gpt-4", [{ role: "user", content: "Hello" }], { temperature: 0.7 })
                .then(r => { globalThis.result = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        if let OpType::LlmCall {
            model,
            messages,
            options,
        } = &ops[0].op_type
        {
            assert_eq!(model, "gpt-4");
            assert_eq!(messages.len(), 1);
            assert_eq!(options.temperature, Some(0.7));
        } else {
            panic!("Expected LlmCall op");
        }
    }

    #[test]
    fn test_run_pending_jobs() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Create a promise chain
        rt.execute(
            r#"
            globalThis.value = 0;
            Promise.resolve(1)
                .then(v => { globalThis.value = v; return v + 1; })
                .then(v => { globalThis.value = v; });
        "#,
        )
        .unwrap();

        // Run pending jobs to process the chain
        let jobs = rt.run_pending_jobs().unwrap();
        assert!(jobs >= 0);

        let value = rt.execute("globalThis.value").unwrap();
        assert_eq!(value.as_i64().unwrap(), 2);
    }

    #[test]
    fn test_memory_operations() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.results = [];
            __amla__.memoryRead("key1").then(r => { globalThis.results.push({ op: "read", r }); });
            __amla__.memoryWrite("key2", { data: "value" }).then(r => { globalThis.results.push({ op: "write", r }); });
            __amla__.memoryDelete("key3").then(r => { globalThis.results.push({ op: "delete", r }); });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 3);

        // Verify operation types
        assert!(matches!(&ops[0].op_type, OpType::MemoryRead { key } if key == "key1"));
        assert!(matches!(&ops[1].op_type, OpType::MemoryWrite { key, .. } if key == "key2"));
        assert!(matches!(&ops[2].op_type, OpType::MemoryDelete { key } if key == "key3"));
    }

    #[test]
    fn test_fs_operations() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.results = [];
            __amla__.fs.readFile("/test.txt").then(r => { globalThis.results.push(r); });
            __amla__.fs.writeFile("/out.txt", "content").then(r => { globalThis.results.push(r); });
            __amla__.fs.readDir("/dir").then(r => { globalThis.results.push(r); });
            __amla__.fs.stat("/file").then(r => { globalThis.results.push(r); });
            __amla__.fs.exists("/file").then(r => { globalThis.results.push(r); });
            __amla__.fs.unlink("/file").then(r => { globalThis.results.push(r); });
            __amla__.fs.mkdir("/newdir", { recursive: true }).then(r => { globalThis.results.push(r); });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 7);

        assert!(matches!(&ops[0].op_type, OpType::FsRead { path, .. } if path == "/test.txt"));
        assert!(matches!(&ops[1].op_type, OpType::FsWrite { path, .. } if path == "/out.txt"));
        assert!(matches!(&ops[2].op_type, OpType::FsReadDir { path } if path == "/dir"));
        assert!(matches!(&ops[3].op_type, OpType::FsStat { path } if path == "/file"));
        assert!(matches!(&ops[4].op_type, OpType::FsExists { path } if path == "/file"));
        assert!(matches!(&ops[5].op_type, OpType::FsUnlink { path } if path == "/file"));
        assert!(
            matches!(&ops[6].op_type, OpType::FsMkdir { path, recursive: true } if path == "/newdir")
        );
    }

    #[test]
    fn test_console_multiple_levels() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            console.log("log message");
            console.error("error message");
            console.warn("warn message");
            console.info("info message");
            console.debug("debug message");
        "#,
        )
        .unwrap();

        let output = rt.take_console_output();
        assert_eq!(output.len(), 5);
        assert_eq!(output[0].level, "log");
        assert_eq!(output[1].level, "error");
        assert_eq!(output[2].level, "warn");
        assert_eq!(output[3].level, "info");
        assert_eq!(output[4].level, "debug");
    }

    #[test]
    fn test_param_constraint_builders() {
        let mut rt = RealJsRuntime::new().unwrap();

        // Test all Param constraint builders
        let result = rt
            .execute(
                r#"
            const p = __amla__.Param("amount");
            JSON.stringify({
                le: p.le(100),
                ge: p.ge(0),
                lt: p.lt(50),
                gt: p.gt(10),
                eq: p.eq(42),
                ne: p.ne(0),
                in_: p.in_([1, 2, 3]),
                notIn: p.notIn([4, 5, 6]),
                startsWith: p.startsWith("pre"),
                endsWith: p.endsWith("suf"),
                contains: p.contains("mid"),
            })
        "#,
            )
            .unwrap();

        let constraints: serde_json::Value =
            serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(constraints["le"]["type"], "le");
        assert_eq!(constraints["le"]["param"], "amount");
        assert_eq!(constraints["le"]["value"], 100);
        assert_eq!(constraints["ge"]["type"], "ge");
        assert_eq!(constraints["lt"]["type"], "lt");
        assert_eq!(constraints["gt"]["type"], "gt");
        assert_eq!(constraints["eq"]["type"], "eq");
        assert_eq!(constraints["ne"]["type"], "ne");
        assert_eq!(constraints["in_"]["type"], "in");
        assert_eq!(constraints["notIn"]["type"], "notIn");
        assert_eq!(constraints["startsWith"]["type"], "startsWith");
        assert_eq!(constraints["endsWith"]["type"], "endsWith");
        assert_eq!(constraints["contains"]["type"], "contains");
    }

    #[test]
    fn test_global_fetch_override() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.response = null;
            fetch("https://example.com/api", { method: "POST" })
                .then(r => { globalThis.response = r; });
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);

        if let OpType::Fetch { url, options } = &ops[0].op_type {
            assert_eq!(url, "https://example.com/api");
            assert_eq!(options.method, "POST");
        } else {
            panic!("Expected Fetch op");
        }

        // Resolve with a mock response
        rt.resolve(
            &ops[0].id,
            &serde_json::json!({
                "ok": true,
                "status": 200,
                "statusText": "OK",
                "headers": { "content-type": "application/json" },
                "body": { "data": "value" }
            }),
        )
        .unwrap();

        // Verify Response-like object
        let status = rt.execute("globalThis.response.status").unwrap();
        assert_eq!(status.as_i64().unwrap(), 200);

        let ok = rt.execute("globalThis.response.ok").unwrap();
        assert!(ok.as_bool().unwrap());
    }

    #[test]
    fn test_set_timeout_and_clear() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.called = false;
            const id = setTimeout(() => { globalThis.called = true; }, 100);
            clearTimeout(id);
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        // Should have a sleep op
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0].op_type, OpType::Sleep { delay_ms: 100 }));

        // Resolve the sleep - but callback should not run because we cleared it
        rt.resolve(&ops[0].id, &serde_json::json!(null)).unwrap();

        let called = rt.execute("globalThis.called").unwrap();
        assert!(!called.as_bool().unwrap());
    }

    #[test]
    fn test_set_interval() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(
            r#"
            globalThis.count = 0;
            globalThis.intervalId = setInterval(() => { globalThis.count++; }, 50);
        "#,
        )
        .unwrap();

        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(ops[0].op_type, OpType::Sleep { delay_ms: 50 }));

        // Resolve first tick
        rt.resolve(&ops[0].id, &serde_json::json!(null)).unwrap();

        let count = rt.execute("globalThis.count").unwrap();
        assert_eq!(count.as_i64().unwrap(), 1);

        // Should have scheduled another sleep for next tick
        let ops = rt.take_pending_ops();
        assert_eq!(ops.len(), 1);

        // Clear the interval
        rt.execute("clearInterval(globalThis.intervalId)").unwrap();

        // Resolve the pending sleep - callback should not run
        rt.resolve(&ops[0].id, &serde_json::json!(null)).unwrap();

        let count = rt.execute("globalThis.count").unwrap();
        assert_eq!(count.as_i64().unwrap(), 1); // Still 1, not incremented
    }

    #[test]
    fn test_amla_log_sync() {
        let mut rt = RealJsRuntime::new().unwrap();
        rt.execute(r#"__amla__.log("info", "Direct log call");"#)
            .unwrap();

        let output = rt.take_console_output();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].level, "info");
        assert_eq!(output[0].message, "Direct log call");
    }
}
