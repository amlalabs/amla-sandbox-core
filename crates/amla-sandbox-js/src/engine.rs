//! `QuickJS` engine wrapper.
//!
//! Provides a safe Rust interface over the C FFI bindings.
//! Works identically for native and WASM builds.

use crate::ffi;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::ptr;

/// Configuration for creating a JS engine.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// Memory limit in bytes (0 = unlimited)
    pub memory_limit: usize,
    /// Max stack size in bytes
    pub max_stack_size: usize,
    /// Instruction limit (0 = unlimited)
    ///
    /// Limits the number of JS instructions executed before the engine
    /// is interrupted with an `InternalError: interrupted` exception.
    /// This provides CPU-time-like protection against infinite loops.
    pub instruction_limit: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            memory_limit: 64 * 1024 * 1024, // 64MB
            max_stack_size: 1024 * 1024,    // 1MB
            instruction_limit: 0,           // Unlimited by default
        }
    }
}

/// JavaScript value representation.
#[derive(Debug, Clone, PartialEq)]
pub enum JsValue {
    /// JavaScript `undefined` value.
    Undefined,
    /// JavaScript `null` value.
    Null,
    /// JavaScript boolean value.
    Bool(bool),
    /// JavaScript integer (represented as i64).
    Int(i64),
    /// JavaScript floating-point number.
    Float(f64),
    /// JavaScript string.
    String(String),
    /// JavaScript array.
    Array(Vec<JsValue>),
    /// JavaScript object (key-value map).
    Object(HashMap<String, JsValue>),
}

impl JsValue {
    /// Convert from `serde_json::Value`.
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => JsValue::Null,
            serde_json::Value::Bool(b) => JsValue::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    JsValue::Int(i)
                } else if let Some(f) = n.as_f64() {
                    JsValue::Float(f)
                } else {
                    JsValue::Undefined
                }
            }
            serde_json::Value::String(s) => JsValue::String(s.clone()),
            serde_json::Value::Array(arr) => {
                JsValue::Array(arr.iter().map(JsValue::from_json).collect())
            }
            serde_json::Value::Object(obj) => JsValue::Object(
                obj.iter()
                    .map(|(k, v)| (k.clone(), JsValue::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Convert to `serde_json::Value`.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            JsValue::Undefined | JsValue::Null => serde_json::Value::Null,
            JsValue::Bool(b) => serde_json::Value::Bool(*b),
            JsValue::Int(i) => serde_json::json!(*i),
            JsValue::Float(f) => serde_json::json!(*f),
            JsValue::String(s) => serde_json::Value::String(s.clone()),
            JsValue::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(JsValue::to_json).collect())
            }
            JsValue::Object(obj) => serde_json::Value::Object(
                obj.iter().map(|(k, v)| (k.clone(), v.to_json())).collect(),
            ),
        }
    }
}

/// JavaScript engine error.
#[derive(Debug, Clone)]
pub struct EngineError {
    /// The error message.
    pub message: String,
    /// Optional stack trace.
    pub stack: Option<String>,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(stack) = &self.stack {
            write!(f, "\n{stack}")?;
        }
        Ok(())
    }
}

impl std::error::Error for EngineError {}

/// Type alias for callback functions registered with the engine.
///
/// The callback receives a JSON array of arguments and should return
/// a JSON-encoded result (or None for undefined).
pub type EngineCallback = Box<dyn Fn(&str) -> Option<String> + 'static>;

/// Stored callback with its ID for the trampoline.
struct StoredCallback {
    callback: EngineCallback,
}

/// `QuickJS` engine.
///
/// Wraps the C FFI to provide a safe Rust interface.
pub struct QuickJsEngine {
    runtime: *mut ffi::QjsRuntime,
    context: *mut ffi::QjsContext,
    /// Raw pointers to callbacks registered with the engine.
    /// These are freed in Drop AFTER the context is freed.
    /// We store raw pointers because C holds references to them.
    callback_ptrs: Vec<*mut StoredCallback>,
}

// Note: QuickJS is NOT thread-safe. QuickJsEngine is intentionally !Send and !Sync.
// Do not add unsafe impl Send/Sync - use from a single thread only.

/// C trampoline function that routes callbacks to Rust closures.
extern "C" fn callback_trampoline(args_json: *const c_char, user_data: *mut c_void) -> *mut c_char {
    if user_data.is_null() || args_json.is_null() {
        return ptr::null_mut();
    }

    // SAFETY: QuickJS calls our C-callback with the same `user_data` we registered, which holds an owned Box<StoredCallback>; borrow is scoped to this callback invocation.
    let stored = unsafe { &*(user_data as *const StoredCallback) };

    // SAFETY: `args_json` is a NUL-terminated C string provided by QuickJS for the duration of this callback invocation.
    let args_str = unsafe {
        match CStr::from_ptr(args_json).to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        }
    };

    // Call the Rust callback
    match (stored.callback)(args_str) {
        Some(result) => {
            // Allocate result string that C code will free
            match CString::new(result) {
                Ok(cstr) => cstr.into_raw(),
                Err(_) => ptr::null_mut(),
            }
        }
        None => ptr::null_mut(),
    }
}

impl QuickJsEngine {
    /// Create a new `QuickJS` engine.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        // SAFETY: All qjs_* FFI calls operate on handles we own in this scope; on failure we free the runtime before returning.
        unsafe {
            let runtime = ffi::qjs_new_runtime();
            if runtime.is_null() {
                return Err(EngineError {
                    message: "Failed to create QuickJS runtime".to_string(),
                    stack: None,
                });
            }

            if config.memory_limit > 0 {
                ffi::qjs_set_memory_limit(runtime, config.memory_limit);
            }
            ffi::qjs_set_max_stack_size(runtime, config.max_stack_size);

            if config.instruction_limit > 0 {
                ffi::qjs_set_instruction_limit(runtime, config.instruction_limit);
            }

            let context = ffi::qjs_new_context(runtime);
            if context.is_null() {
                ffi::qjs_free_runtime(runtime);
                return Err(EngineError {
                    message: "Failed to create QuickJS context".to_string(),
                    stack: None,
                });
            }

            Ok(Self {
                runtime,
                context,
                callback_ptrs: Vec::new(),
            })
        }
    }

    /// Evaluate JavaScript code.
    pub fn eval(&mut self, code: &str) -> Result<JsValue, EngineError> {
        let code_cstr = CString::new(code).map_err(|e| EngineError {
            message: format!("Invalid code string: {e}"),
            stack: None,
        })?;
        let filename = CString::new("<eval>").unwrap();

        // SAFETY: `self.context` is a valid JSContext handle owned by Engine; the CStrings live through the call.
        unsafe {
            let result = ffi::qjs_eval(
                self.context,
                code_cstr.as_ptr(),
                code.len(),
                filename.as_ptr(),
            );

            if result.is_null() {
                if let Some(exc) = self.get_exception() {
                    return Err(exc);
                }
                return Ok(JsValue::Undefined);
            }

            let result_str = CStr::from_ptr(result).to_string_lossy().to_string();
            ffi::qjs_free_string(result);

            Self::json_to_value(&result_str)
        }
    }

    /// Run pending jobs (microtask queue for Promises).
    ///
    /// Returns the number of jobs executed, or an error if:
    /// - An exception occurred during job execution
    /// - The iteration budget was exhausted (infinite loop protection)
    pub fn run_pending_jobs(&mut self) -> Result<i32, EngineError> {
        // SAFETY: `self.runtime` is a valid JSRuntime handle owned by Engine.
        unsafe {
            let ret = ffi::qjs_run_pending_jobs(self.runtime);
            if ret == ffi::QJS_BUDGET_EXHAUSTED {
                return Err(EngineError {
                    message: "Microtask budget exhausted (possible infinite loop)".to_string(),
                    stack: None,
                });
            }
            if ret == ffi::QJS_EXCEPTION {
                if let Some(exc) = self.get_exception() {
                    return Err(exc);
                }
                return Err(EngineError {
                    message: "Exception in pending job".to_string(),
                    stack: None,
                });
            }
            if ret == ffi::QJS_ERROR {
                return Err(EngineError {
                    message: "Error running pending jobs".to_string(),
                    stack: None,
                });
            }
            Ok(ret)
        }
    }

    /// Check if there are pending jobs.
    pub fn has_pending_jobs(&self) -> bool {
        // SAFETY: `self.runtime` is a valid JSRuntime handle owned by Engine.
        unsafe { ffi::qjs_has_pending_jobs(self.runtime) != 0 }
    }

    /// Set instruction limit for the engine.
    ///
    /// # Parameters
    /// - `limit`: Maximum instructions before interrupt (0 = unlimited)
    ///
    /// This provides CPU-time-like protection against infinite loops.
    /// When the limit is reached, execution is interrupted with an exception.
    pub fn set_instruction_limit(&mut self, limit: u64) {
        // SAFETY: `self.runtime` is a valid JSRuntime handle owned by Engine.
        unsafe { ffi::qjs_set_instruction_limit(self.runtime, limit) }
    }

    /// Manually interrupt execution.
    ///
    /// This can be called from a separate thread or signal handler to
    /// stop currently running JavaScript code.
    pub fn interrupt(&mut self) {
        // SAFETY: `self.runtime` is a valid JSRuntime handle owned by Engine.
        unsafe { ffi::qjs_interrupt(self.runtime) }
    }

    /// Clear interrupt flag.
    ///
    /// Call this before starting new execution if you want to reset the
    /// interrupt state after a previous interrupt.
    pub fn clear_interrupt(&mut self) {
        // SAFETY: `self.runtime` is a valid JSRuntime handle owned by Engine.
        unsafe { ffi::qjs_clear_interrupt(self.runtime) }
    }

    /// Set a global variable to a JSON value.
    pub fn set_global(&mut self, name: &str, value: &serde_json::Value) -> Result<(), EngineError> {
        let name_cstr = CString::new(name).map_err(|e| EngineError {
            message: format!("Invalid name: {e}"),
            stack: None,
        })?;
        let json = serde_json::to_string(value).map_err(|e| EngineError {
            message: format!("JSON serialization failed: {e}"),
            stack: None,
        })?;
        let json_cstr = CString::new(json).map_err(|e| EngineError {
            message: format!("Invalid JSON string: {e}"),
            stack: None,
        })?;

        // SAFETY: `self.context` is a valid JSContext handle owned by Engine; the CStrings live through the call.
        unsafe {
            let ret =
                ffi::qjs_set_global_json(self.context, name_cstr.as_ptr(), json_cstr.as_ptr());
            if ret != ffi::QJS_OK {
                if let Some(exc) = self.get_exception() {
                    return Err(exc);
                }
                return Err(EngineError {
                    message: "Failed to set global".to_string(),
                    stack: None,
                });
            }
            Ok(())
        }
    }

    /// Get a global variable as JSON.
    pub fn get_global(&self, name: &str) -> Result<serde_json::Value, EngineError> {
        let name_cstr = CString::new(name).map_err(|e| EngineError {
            message: format!("Invalid name: {e}"),
            stack: None,
        })?;

        // SAFETY: `self.context` is a valid JSContext handle owned by Engine; `name_cstr` lives through the call and any returned string is freed before return.
        unsafe {
            let result = ffi::qjs_get_global_json(self.context, name_cstr.as_ptr());
            if result.is_null() {
                return Ok(serde_json::Value::Null);
            }

            let result_str = CStr::from_ptr(result).to_string_lossy().to_string();
            ffi::qjs_free_string(result);

            serde_json::from_str(&result_str).map_err(|e| EngineError {
                message: format!("JSON parse error: {e}"),
                stack: None,
            })
        }
    }

    /// Register a native function as a global.
    ///
    /// The callback receives a JSON array of arguments and should return
    /// a JSON-encoded result string (or None for undefined).
    pub fn add_function<F>(&mut self, name: &str, callback: F) -> Result<(), EngineError>
    where
        F: Fn(&str) -> Option<String> + 'static,
    {
        let name_cstr = CString::new(name).map_err(|e| EngineError {
            message: format!("Invalid name: {e}"),
            stack: None,
        })?;

        // Box the callback and store it
        let stored = Box::new(StoredCallback {
            callback: Box::new(callback),
        });
        let user_data = Box::into_raw(stored).cast::<c_void>();

        // SAFETY: `self.context` is a valid JSContext handle; `user_data` is a freshly-leaked Box<StoredCallback> that we reclaim on the failure path via `Box::from_raw`.
        unsafe {
            let ret = ffi::qjs_add_function(
                self.context,
                name_cstr.as_ptr(),
                callback_trampoline,
                user_data,
            );
            if ret != ffi::QJS_OK {
                // Clean up on failure
                let _ = Box::from_raw(user_data.cast::<StoredCallback>());
                if let Some(exc) = self.get_exception() {
                    return Err(exc);
                }
                return Err(EngineError {
                    message: "Failed to add function".to_string(),
                    stack: None,
                });
            }
        }

        // Store the raw pointer so we can free it in Drop AFTER context is freed.
        // C holds a reference to this memory, so we must not free it until then.
        self.callback_ptrs.push(user_data.cast::<StoredCallback>());

        Ok(())
    }

    /// Call a global function with JSON arguments.
    pub fn call_function(
        &mut self,
        name: &str,
        args: &[serde_json::Value],
    ) -> Result<JsValue, EngineError> {
        let name_cstr = CString::new(name).map_err(|e| EngineError {
            message: format!("Invalid name: {e}"),
            stack: None,
        })?;

        let args_json = serde_json::to_string(args).map_err(|e| EngineError {
            message: format!("JSON serialization failed: {e}"),
            stack: None,
        })?;
        let args_cstr = CString::new(args_json).map_err(|e| EngineError {
            message: format!("Invalid args string: {e}"),
            stack: None,
        })?;

        // SAFETY: `self.context` is a valid JSContext handle owned by Engine; the CStrings live through the call.
        unsafe {
            let result =
                ffi::qjs_call_function(self.context, name_cstr.as_ptr(), args_cstr.as_ptr());

            if result.is_null() {
                if let Some(exc) = self.get_exception() {
                    return Err(exc);
                }
                return Ok(JsValue::Undefined);
            }

            let result_str = CStr::from_ptr(result).to_string_lossy().to_string();
            ffi::qjs_free_string(result);

            Self::json_to_value(&result_str)
        }
    }

    /// Create a new Promise and return its ID.
    ///
    /// Use `resolve_promise` or `reject_promise` to settle the promise.
    pub fn new_promise(&mut self) -> Result<u64, EngineError> {
        let mut promise_id: u64 = 0;

        // SAFETY: `self.context` is a valid JSContext handle owned by Engine; `&raw mut promise_id` points to this stack slot for the duration of the call.
        unsafe {
            let result = ffi::qjs_new_promise(self.context, &raw mut promise_id);
            if result.is_null() {
                if let Some(exc) = self.get_exception() {
                    return Err(exc);
                }
                return Err(EngineError {
                    message: "Failed to create promise".to_string(),
                    stack: None,
                });
            }
            ffi::qjs_free_string(result);
        }

        Ok(promise_id)
    }

    /// Resolve a promise with a value.
    pub fn resolve_promise(
        &mut self,
        promise_id: u64,
        value: &serde_json::Value,
    ) -> Result<(), EngineError> {
        let json = serde_json::to_string(value).map_err(|e| EngineError {
            message: format!("JSON serialization failed: {e}"),
            stack: None,
        })?;
        let json_cstr = CString::new(json).map_err(|e| EngineError {
            message: format!("Invalid JSON string: {e}"),
            stack: None,
        })?;

        // SAFETY: `self.context` is a valid JSContext handle owned by Engine; `json_cstr` lives through the call.
        unsafe {
            let ret = ffi::qjs_resolve_promise(self.context, promise_id, json_cstr.as_ptr());
            if ret != ffi::QJS_OK {
                if let Some(exc) = self.get_exception() {
                    return Err(exc);
                }
                return Err(EngineError {
                    message: "Failed to resolve promise".to_string(),
                    stack: None,
                });
            }
        }

        Ok(())
    }

    /// Reject a promise with an error.
    pub fn reject_promise(&mut self, promise_id: u64, error: &str) -> Result<(), EngineError> {
        let error_json = serde_json::json!({"message": error});
        let json = serde_json::to_string(&error_json).unwrap();
        let json_cstr = CString::new(json).map_err(|e| EngineError {
            message: format!("Invalid error string: {e}"),
            stack: None,
        })?;

        // SAFETY: `self.context` is a valid JSContext handle owned by Engine; `json_cstr` lives through the call.
        unsafe {
            let ret = ffi::qjs_reject_promise(self.context, promise_id, json_cstr.as_ptr());
            if ret != ffi::QJS_OK {
                if let Some(exc) = self.get_exception() {
                    return Err(exc);
                }
                return Err(EngineError {
                    message: "Failed to reject promise".to_string(),
                    stack: None,
                });
            }
        }

        Ok(())
    }

    /// Get the current exception (if any).
    fn get_exception(&self) -> Option<EngineError> {
        // SAFETY: `self.context` is a valid JSContext handle owned by Engine; any returned exception string is freed before return.
        unsafe {
            let exc = ffi::qjs_get_exception(self.context);
            if exc.is_null() {
                return None;
            }
            let exc_str = CStr::from_ptr(exc).to_string_lossy().to_string();
            ffi::qjs_free_string(exc);

            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&exc_str) {
                Some(EngineError {
                    message: v["message"].as_str().unwrap_or("Unknown error").to_string(),
                    stack: v["stack"].as_str().map(String::from),
                })
            } else {
                Some(EngineError {
                    message: exc_str,
                    stack: None,
                })
            }
        }
    }

    fn json_to_value(json: &str) -> Result<JsValue, EngineError> {
        let v: serde_json::Value = serde_json::from_str(json).map_err(|e| EngineError {
            message: format!("JSON parse error: {e}"),
            stack: None,
        })?;
        Ok(JsValue::from_json(&v))
    }
}

impl Drop for QuickJsEngine {
    fn drop(&mut self) {
        // SAFETY: `self.context` and `self.runtime` are handles owned by Engine; we free context before runtime, then reclaim each leaked Box<StoredCallback> exactly once.
        unsafe {
            // IMPORTANT: Free context first - it may still call into our callbacks
            if !self.context.is_null() {
                ffi::qjs_free_context(self.context);
            }
            if !self.runtime.is_null() {
                ffi::qjs_free_runtime(self.runtime);
            }

            // Now safe to free callbacks - context no longer holds references
            for ptr in self.callback_ptrs.drain(..) {
                if !ptr.is_null() {
                    let _ = Box::from_raw(ptr);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require the QuickJS library to be linked.
    // They will be run when the `quickjs` feature is enabled and build.rs compiles the C code.

    #[test]
    fn test_engine_config_default() {
        let config = EngineConfig::default();
        assert_eq!(config.memory_limit, 64 * 1024 * 1024);
        assert_eq!(config.max_stack_size, 1024 * 1024);
        assert_eq!(config.instruction_limit, 0); // Unlimited by default
    }

    #[test]
    fn test_js_value_from_json() {
        let json = serde_json::json!({"a": 1, "b": [2, 3]});
        let value = JsValue::from_json(&json);
        assert!(matches!(value, JsValue::Object(_)));
    }

    #[test]
    fn test_js_value_to_json() {
        let value = JsValue::Object(
            [
                ("a".to_string(), JsValue::Int(1)),
                (
                    "b".to_string(),
                    JsValue::Array(vec![JsValue::Int(2), JsValue::Int(3)]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let json = value.to_json();
        assert_eq!(json["a"], 1);
    }

    // Integration tests that actually exercise QuickJS
    #[test]
    fn test_engine_creation() {
        let engine = QuickJsEngine::new(EngineConfig::default());
        assert!(engine.is_ok());
    }

    #[test]
    fn test_eval_simple_int() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        let result = engine.eval("1 + 2").unwrap();
        assert_eq!(result, JsValue::Int(3));
    }

    #[test]
    fn test_eval_simple_string() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        let result = engine.eval("'hello'").unwrap();
        assert_eq!(result, JsValue::String("hello".to_string()));
    }

    #[test]
    fn test_eval_object() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        let result = engine.eval("({a: 1, b: 2})").unwrap();
        if let JsValue::Object(map) = result {
            assert_eq!(map.get("a"), Some(&JsValue::Int(1)));
            assert_eq!(map.get("b"), Some(&JsValue::Int(2)));
        } else {
            panic!("Expected object, got {result:?}");
        }
    }

    #[test]
    fn test_eval_array() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        let result = engine.eval("[1, 2, 3]").unwrap();
        assert_eq!(
            result,
            JsValue::Array(vec![JsValue::Int(1), JsValue::Int(2), JsValue::Int(3)])
        );
    }

    #[test]
    fn test_eval_function_call() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        let result = engine.eval("Math.max(1, 5, 3)").unwrap();
        assert_eq!(result, JsValue::Int(5));
    }

    #[test]
    fn test_set_and_get_global() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        engine
            .set_global("testValue", &serde_json::json!(42))
            .unwrap();
        let result = engine.get_global("testValue").unwrap();
        assert_eq!(result, serde_json::json!(42));
    }

    #[test]
    fn test_eval_with_global() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        engine
            .set_global("myData", &serde_json::json!({"x": 10}))
            .unwrap();
        let result = engine.eval("myData.x * 2").unwrap();
        assert_eq!(result, JsValue::Int(20));
    }

    #[test]
    fn test_add_function() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        engine
            .add_function("hostDouble", |args_json| {
                let args: Vec<serde_json::Value> = serde_json::from_str(args_json).ok()?;
                let n = args.first()?.as_i64()?;
                Some(serde_json::to_string(&(n * 2)).unwrap())
            })
            .unwrap();

        let result = engine.eval("hostDouble(21)").unwrap();
        assert_eq!(result, JsValue::Int(42));
    }

    #[test]
    fn test_eval_syntax_error() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        let result = engine.eval("let x = ;");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("unexpected") || err.message.contains("Unexpected"));
    }

    #[test]
    fn test_eval_reference_error() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        let result = engine.eval("undefinedVariable");
        assert!(result.is_err());
    }

    #[test]
    fn test_pending_jobs() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();
        // Execute a promise that resolves immediately
        engine
            .eval("Promise.resolve(42).then(x => globalThis.result = x)")
            .unwrap();
        assert!(engine.has_pending_jobs());
        let jobs_run = engine.run_pending_jobs().unwrap();
        assert!(jobs_run >= 1);
    }

    #[test]
    fn test_instruction_limit_stops_infinite_loop() {
        let config = EngineConfig {
            instruction_limit: 10000, // Limit to 10k instructions
            ..Default::default()
        };
        let mut engine = QuickJsEngine::new(config).unwrap();

        // Infinite loop should be interrupted
        let result = engine.eval("while(true) {}");
        assert!(result.is_err(), "Infinite loop should be interrupted");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("interrupted")
                || err.message.contains("Interrupt")
                || err.message.contains("InternalError"),
            "Error should indicate interruption: {}",
            err.message
        );
    }

    #[test]
    fn test_instruction_limit_allows_short_code() {
        let config = EngineConfig {
            instruction_limit: 100000, // 100k instructions should be plenty
            ..Default::default()
        };
        let mut engine = QuickJsEngine::new(config).unwrap();

        // Short computation should complete
        let result = engine.eval("let sum = 0; for(let i = 0; i < 100; i++) sum += i; sum");
        assert!(result.is_ok(), "Short code should complete");
        assert_eq!(result.unwrap(), JsValue::Int(4950)); // Sum of 0..99
    }

    #[test]
    fn test_set_instruction_limit_dynamically() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Set a limit dynamically
        engine.set_instruction_limit(5000);

        // Infinite loop should be interrupted
        let result = engine.eval("while(true) {}");
        assert!(result.is_err(), "Infinite loop should be interrupted");
    }

    #[test]
    fn test_clear_interrupt_resets_state() {
        let config = EngineConfig {
            instruction_limit: 1000,
            ..Default::default()
        };
        let mut engine = QuickJsEngine::new(config).unwrap();

        // Trigger interrupt
        let _ = engine.eval("while(true) {}");

        // Clear interrupt and try again with more budget
        engine.clear_interrupt();
        engine.set_instruction_limit(100000);

        // Should work now
        let result = engine.eval("1 + 1");
        assert!(result.is_ok());
    }

    // ========== Resource limit tests ==========

    #[test]
    fn test_memory_limit_enforced() {
        let config = EngineConfig {
            memory_limit: 1024 * 1024, // 1MB - very small
            ..Default::default()
        };
        let mut engine = QuickJsEngine::new(config).unwrap();

        // Try to allocate a huge array - should fail
        let result = engine.eval(
            r"
            let arr = [];
            for (let i = 0; i < 10000000; i++) {
                arr.push(new Array(1000).fill('x'));
            }
            arr.length
            ",
        );
        assert!(result.is_err(), "Memory allocation should fail");
    }

    #[test]
    fn test_memory_limit_allows_small_allocation() {
        let config = EngineConfig {
            memory_limit: 64 * 1024 * 1024, // 64MB
            ..Default::default()
        };
        let mut engine = QuickJsEngine::new(config).unwrap();

        // Small allocation should work
        let result = engine.eval(
            r"
            let arr = new Array(100).fill(42);
            arr.length
            ",
        );
        assert!(result.is_ok(), "Small allocation should succeed");
        assert_eq!(result.unwrap(), JsValue::Int(100));
    }

    #[test]
    #[ignore = "Can cause process abort due to actual stack overflow - run manually"]
    fn test_stack_overflow_protection() {
        let config = EngineConfig {
            max_stack_size: 256 * 1024, // 256KB - relatively small
            ..Default::default()
        };
        let mut engine = QuickJsEngine::new(config).unwrap();

        // Deep recursion should fail with stack overflow
        let result = engine.eval(
            r"
            function recurse(n) {
                if (n <= 0) return 0;
                return 1 + recurse(n - 1);
            }
            recurse(100000)
            ",
        );
        assert!(result.is_err(), "Deep recursion should fail");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("stack")
                || err.message.contains("Stack")
                || err.message.contains("overflow")
                || err.message.contains("InternalError"),
            "Error should indicate stack overflow: {}",
            err.message
        );
    }

    #[test]
    fn test_stack_allows_shallow_recursion() {
        let config = EngineConfig {
            max_stack_size: 1024 * 1024, // 1MB
            ..Default::default()
        };
        let mut engine = QuickJsEngine::new(config).unwrap();

        // Shallow recursion should work
        let result = engine.eval(
            r"
            function recurse(n) {
                if (n <= 0) return 0;
                return 1 + recurse(n - 1);
            }
            recurse(100)
            ",
        );
        assert!(result.is_ok(), "Shallow recursion should succeed");
        assert_eq!(result.unwrap(), JsValue::Int(100));
    }

    // ========== API tests ==========

    #[test]
    fn test_call_function_api() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Define a function
        engine.eval("function add(a, b) { return a + b; }").unwrap();

        // Call it via API
        let result = engine
            .call_function("add", &[serde_json::json!(3), serde_json::json!(4)])
            .unwrap();
        assert_eq!(result, JsValue::Int(7));
    }

    #[test]
    fn test_call_function_with_no_args() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        engine.eval("function getAnswer() { return 42; }").unwrap();

        let result = engine.call_function("getAnswer", &[]).unwrap();
        assert_eq!(result, JsValue::Int(42));
    }

    #[test]
    fn test_call_function_nonexistent() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine.call_function("doesNotExist", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_call_function_with_object_arg() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        engine
            .eval("function greet(obj) { return 'Hello, ' + obj.name; }")
            .unwrap();

        let result = engine
            .call_function("greet", &[serde_json::json!({"name": "World"})])
            .unwrap();
        assert_eq!(result, JsValue::String("Hello, World".to_string()));
    }

    #[test]
    fn test_new_promise_returns_valid_id() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Create a promise
        let promise_id = engine.new_promise().unwrap();
        assert!(promise_id > 0, "Promise ID should be positive");

        // Creating another should get a different ID
        let promise_id2 = engine.new_promise().unwrap();
        assert!(promise_id2 > promise_id, "Promise IDs should be sequential");
    }

    #[test]
    fn test_promise_resolve_via_stored_resolver() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Create a promise - the C API stores resolve/reject functions as globals
        let _promise_id = engine.new_promise().unwrap();

        // Set up tracking - we'll call the resolver directly
        engine.eval("var testPromiseResult = null;").unwrap();

        // The C API creates __resolve_N and __reject_N globals
        // We can create our own promise and chain it
        engine
            .eval(
                "var p = new Promise((resolve) => { \
                     globalThis.__testResolve = resolve; \
                 }); \
                 p.then(v => { testPromiseResult = v; });",
            )
            .unwrap();

        // Resolve using the global resolver
        engine.eval("__testResolve({ success: true })").unwrap();

        // Run pending jobs to process the resolution
        engine.run_pending_jobs().unwrap();

        // Check the result
        let result = engine.get_global("testPromiseResult").unwrap();
        assert_eq!(result, serde_json::json!({"success": true}));
    }

    #[test]
    fn test_promise_reject_via_stored_rejector() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Set up tracking
        engine.eval("var testRejectionReason = null;").unwrap();

        // Create our own promise and chain it
        engine
            .eval(
                "var p = new Promise((_, reject) => { \
                     globalThis.__testReject = reject; \
                 }); \
                 p.catch(e => { testRejectionReason = e.message; });",
            )
            .unwrap();

        // Reject using the global rejector
        engine
            .eval("__testReject(new Error('Something went wrong'))")
            .unwrap();

        // Run pending jobs to process the rejection
        engine.run_pending_jobs().unwrap();

        // Check the result
        let result = engine.get_global("testRejectionReason").unwrap();
        assert_eq!(result, serde_json::json!("Something went wrong"));
    }

    #[test]
    fn test_resolve_invalid_promise_id() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Try to resolve a promise that doesn't exist
        let result = engine.resolve_promise(99999, &serde_json::json!(null));
        assert!(result.is_err(), "Should fail for invalid promise ID");
    }

    #[test]
    fn test_reject_invalid_promise_id() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Try to reject a promise that doesn't exist
        let result = engine.reject_promise(99999, "error");
        assert!(result.is_err(), "Should fail for invalid promise ID");
    }

    #[test]
    fn test_multiple_promises_have_unique_ids() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Create multiple promises
        let id1 = engine.new_promise().unwrap();
        let id2 = engine.new_promise().unwrap();
        let id3 = engine.new_promise().unwrap();

        // They should all have different IDs
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);

        // IDs should be sequential
        assert!(id2 > id1);
        assert!(id3 > id2);
    }

    #[test]
    fn test_multiple_js_promises() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Set up tracking
        engine.eval("var results = [];").unwrap();

        // Create multiple promises using pure JS
        engine
            .eval(
                "var p1 = new Promise(r => { globalThis.r1 = r; }); \
                 var p2 = new Promise(r => { globalThis.r2 = r; }); \
                 var p3 = new Promise(r => { globalThis.r3 = r; }); \
                 p1.then(v => results.push('p1:' + v)); \
                 p2.then(v => results.push('p2:' + v)); \
                 p3.then(v => results.push('p3:' + v));",
            )
            .unwrap();

        // Resolve in different order
        engine.eval("r2('B')").unwrap();
        engine.eval("r3('C')").unwrap();
        engine.eval("r1('A')").unwrap();

        engine.run_pending_jobs().unwrap();

        let results = engine.get_global("results").unwrap();
        if let serde_json::Value::Array(arr) = results {
            assert_eq!(arr.len(), 3);
            assert!(arr.contains(&serde_json::json!("p1:A")));
            assert!(arr.contains(&serde_json::json!("p2:B")));
            assert!(arr.contains(&serde_json::json!("p3:C")));
        } else {
            panic!("Expected array result");
        }
    }

    #[test]
    fn test_get_global_undefined() {
        let engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Getting an undefined global should return null (undefined maps to null in JSON)
        let result = engine.get_global("nonExistentVariable").unwrap();
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn test_add_function_returns_undefined() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        engine
            .add_function("noReturn", |_args_json| {
                // Return None for undefined
                None
            })
            .unwrap();

        let result = engine.eval("noReturn()").unwrap();
        // Note: undefined maps to null in JSON, so we get Null back
        assert_eq!(result, JsValue::Null);
    }

    #[test]
    fn test_add_function_with_complex_return() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        engine
            .add_function("getComplexData", |_args_json| {
                Some(
                    serde_json::json!({
                        "items": [1, 2, 3],
                        "metadata": {
                            "count": 3,
                            "valid": true
                        }
                    })
                    .to_string(),
                )
            })
            .unwrap();

        let result = engine.eval("getComplexData()").unwrap();
        if let JsValue::Object(map) = result {
            assert!(map.contains_key("items"));
            assert!(map.contains_key("metadata"));
        } else {
            panic!("Expected object");
        }
    }

    #[test]
    fn test_eval_float_precision() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Test that floats maintain precision
        let result = engine.eval("0.1 + 0.2").unwrap();
        if let JsValue::Float(f) = result {
            // Should be close to 0.3 but not exactly due to float representation
            assert!((f - 0.30000000000000004).abs() < 1e-15);
        } else {
            panic!("Expected float");
        }
    }

    #[test]
    fn test_eval_special_floats() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // NaN - JSON doesn't support NaN, so it maps to null
        let result = engine.eval("NaN").unwrap();
        assert_eq!(result, JsValue::Null, "NaN should map to null in JSON");

        // Infinity - JSON doesn't support Infinity, so it maps to null
        let result = engine.eval("Infinity").unwrap();
        assert_eq!(result, JsValue::Null, "Infinity should map to null in JSON");

        // -Infinity - JSON doesn't support -Infinity, so it maps to null
        let result = engine.eval("-Infinity").unwrap();
        assert_eq!(
            result,
            JsValue::Null,
            "-Infinity should map to null in JSON"
        );

        // However, we can detect them in JS before JSON conversion
        let result = engine.eval("Number.isNaN(NaN)").unwrap();
        assert_eq!(result, JsValue::Bool(true));

        let result = engine.eval("Number.isFinite(Infinity)").unwrap();
        assert_eq!(result, JsValue::Bool(false));
    }

    #[test]
    fn test_microtask_budget_exhaustion() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Create an infinite microtask loop (Promise that schedules itself)
        engine
            .eval(
                r"
            function scheduleForever() {
                Promise.resolve().then(scheduleForever);
            }
            scheduleForever();
            ",
            )
            .unwrap();

        // Running pending jobs should hit budget limit
        let result = engine.run_pending_jobs();
        assert!(result.is_err(), "Should hit microtask budget");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("budget") || err.message.contains("Budget"),
            "Error should mention budget: {}",
            err.message
        );
    }

    // ========== Stress tests and edge cases ==========

    #[test]
    fn test_deep_object_nesting() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Create deeply nested object
        let result = engine
            .eval(
                r"
            let obj = { value: 'deep' };
            for (let i = 0; i < 100; i++) {
                obj = { nested: obj };
            }
            // Traverse back down
            let current = obj;
            let depth = 0;
            while (current.nested) {
                current = current.nested;
                depth++;
            }
            depth
            ",
            )
            .unwrap();
        assert_eq!(result, JsValue::Int(100));
    }

    #[test]
    fn test_deep_array_nesting() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            let arr = [42];
            for (let i = 0; i < 50; i++) {
                arr = [arr];
            }
            // Unwrap
            let current = arr;
            let depth = 0;
            while (Array.isArray(current[0])) {
                current = current[0];
                depth++;
            }
            current[0]
            ",
            )
            .unwrap();
        assert_eq!(result, JsValue::Int(42));
    }

    #[test]
    fn test_large_array_operations() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const arr = new Array(10000).fill(0).map((_, i) => i);
            const sum = arr.reduce((a, b) => a + b, 0);
            const filtered = arr.filter(x => x % 2 === 0);
            const mapped = filtered.map(x => x * 2);
            mapped.length
            ",
            )
            .unwrap();
        assert_eq!(result, JsValue::Int(5000));
    }

    #[test]
    fn test_string_operations_unicode() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Test various Unicode scenarios
        let result = engine
            .eval(
                r"
            const emoji = '😀🎉🚀';
            const chinese = '中文测试';
            const arabic = 'العربية';
            const combined = emoji + chinese + arabic;
            [
                emoji.length,           // Surrogate pairs count as 2
                [...emoji].length,      // Spread gets actual characters
                chinese.length,
                combined.includes('🎉'),
                combined.indexOf('中') >= 0
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr.len(), 5);
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_unicode_edge_cases() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            // Zero-width joiner sequences
            const family = '👨‍👩‍👧‍👦';
            // Combining characters
            const cafe = 'café';
            const cafeNFD = 'cafe\u0301';  // e + combining accent
            // Surrogate pairs
            const astral = '𝄞';  // Musical G clef (U+1D11E)

            [
                family.length > 1,  // ZWJ sequences are multiple code units
                cafe === cafeNFD,   // Should be false (different representations)
                cafe.normalize('NFD') === cafeNFD,
                astral.length,      // 2 (surrogate pair)
                astral.codePointAt(0)  // Should be 0x1D11E = 119070
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Bool(true));
            assert_eq!(arr[1], JsValue::Bool(false));
            assert_eq!(arr[2], JsValue::Bool(true));
            assert_eq!(arr[3], JsValue::Int(2));
            assert_eq!(arr[4], JsValue::Int(119070));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_closure_stress() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Create many closures capturing different scopes
        let result = engine
            .eval(
                r"
            function createClosures(n) {
                const closures = [];
                for (let i = 0; i < n; i++) {
                    closures.push(() => i * 2);
                }
                return closures;
            }
            const fns = createClosures(100);
            fns[50]()
            ",
            )
            .unwrap();
        assert_eq!(result, JsValue::Int(100)); // 50 * 2
    }

    #[test]
    fn test_nested_closures() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            function outer(a) {
                return function middle(b) {
                    return function inner(c) {
                        return function deepest(d) {
                            return a + b + c + d;
                        };
                    };
                };
            }
            outer(1)(2)(3)(4)
            ",
            )
            .unwrap();
        assert_eq!(result, JsValue::Int(10));
    }

    #[test]
    fn test_generator_functions() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            function* fibonacci() {
                let [a, b] = [0, 1];
                while (true) {
                    yield a;
                    [a, b] = [b, a + b];
                }
            }
            const fib = fibonacci();
            const first10 = [];
            for (let i = 0; i < 10; i++) {
                first10.push(fib.next().value);
            }
            first10
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(0));
            assert_eq!(arr[1], JsValue::Int(1));
            assert_eq!(arr[2], JsValue::Int(1));
            assert_eq!(arr[3], JsValue::Int(2));
            assert_eq!(arr[9], JsValue::Int(34));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_generator_delegation() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            function* inner() {
                yield 2;
                yield 3;
            }
            function* outer() {
                yield 1;
                yield* inner();
                yield 4;
            }
            [...outer()]
            ",
            )
            .unwrap();

        assert_eq!(
            result,
            JsValue::Array(vec![
                JsValue::Int(1),
                JsValue::Int(2),
                JsValue::Int(3),
                JsValue::Int(4)
            ])
        );
    }

    #[test]
    fn test_proxy_object() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const target = { x: 10, y: 20 };
            const handler = {
                get(obj, prop) {
                    if (prop === 'sum') {
                        return obj.x + obj.y;
                    }
                    return obj[prop] * 2;
                },
                set(obj, prop, value) {
                    obj[prop] = value + 100;
                    return true;
                }
            };
            const proxy = new Proxy(target, handler);
            proxy.z = 5;
            [proxy.x, proxy.y, proxy.sum, proxy.z, target.z]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(20)); // 10 * 2
            assert_eq!(arr[1], JsValue::Int(40)); // 20 * 2
            assert_eq!(arr[2], JsValue::Int(30)); // 10 + 20
            assert_eq!(arr[3], JsValue::Int(210)); // (5 + 100) * 2
            assert_eq!(arr[4], JsValue::Int(105)); // 5 + 100
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_reflect_api() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const obj = { a: 1 };
            Reflect.set(obj, 'b', 2);
            Reflect.defineProperty(obj, 'c', { value: 3, writable: false });
            [
                Reflect.get(obj, 'a'),
                Reflect.has(obj, 'b'),
                Reflect.ownKeys(obj).length,
                Reflect.getOwnPropertyDescriptor(obj, 'c').writable
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(1));
            assert_eq!(arr[1], JsValue::Bool(true));
            assert_eq!(arr[2], JsValue::Int(3));
            assert_eq!(arr[3], JsValue::Bool(false));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_symbol_handling() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const sym1 = Symbol('test');
            const sym2 = Symbol('test');
            const sym3 = Symbol.for('global');
            const sym4 = Symbol.for('global');

            const obj = {
                [sym1]: 'value1',
                [Symbol.toStringTag]: 'CustomObject'
            };

            [
                sym1 === sym2,           // false - symbols are unique
                sym3 === sym4,           // true - Symbol.for is global
                obj[sym1],
                Object.prototype.toString.call(obj)
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Bool(false));
            assert_eq!(arr[1], JsValue::Bool(true));
            assert_eq!(arr[2], JsValue::String("value1".to_string()));
            assert_eq!(arr[3], JsValue::String("[object CustomObject]".to_string()));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_weakmap_weakset() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const wm = new WeakMap();
            const ws = new WeakSet();
            const obj1 = { id: 1 };
            const obj2 = { id: 2 };

            wm.set(obj1, 'data1');
            wm.set(obj2, 'data2');
            ws.add(obj1);
            ws.add(obj2);

            [
                wm.get(obj1),
                wm.has(obj2),
                ws.has(obj1),
                ws.has({})  // Different object
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("data1".to_string()));
            assert_eq!(arr[1], JsValue::Bool(true));
            assert_eq!(arr[2], JsValue::Bool(true));
            assert_eq!(arr[3], JsValue::Bool(false));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_map_and_set() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const map = new Map();
            map.set('a', 1);
            map.set({ key: 'obj' }, 2);
            map.set(NaN, 3);  // NaN as key works in Map

            const set = new Set([1, 2, 2, 3, 3, 3]);

            [
                map.size,
                map.get('a'),
                map.has(NaN),
                set.size,
                [...set]
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(3));
            assert_eq!(arr[1], JsValue::Int(1));
            assert_eq!(arr[2], JsValue::Bool(true));
            assert_eq!(arr[3], JsValue::Int(3));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_typed_arrays() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const buffer = new ArrayBuffer(16);
            const int32View = new Int32Array(buffer);
            const uint8View = new Uint8Array(buffer);

            int32View[0] = 0x12345678;

            [
                int32View.length,
                uint8View.length,
                uint8View[0],  // Depends on endianness
                int32View[0],
                new Float64Array([1.5, 2.5, 3.5]).reduce((a, b) => a + b)
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(4));
            assert_eq!(arr[1], JsValue::Int(16));
            assert_eq!(arr[3], JsValue::Int(0x12345678));
            assert_eq!(arr[4], JsValue::Float(7.5));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_dataview() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const buffer = new ArrayBuffer(8);
            const view = new DataView(buffer);

            view.setInt32(0, 0x12345678, true);  // little-endian
            view.setFloat32(4, 3.14, true);

            [
                view.getInt32(0, true),
                view.getUint8(0),
                Math.abs(view.getFloat32(4, true) - 3.14) < 0.001
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(0x12345678));
            assert_eq!(arr[1], JsValue::Int(0x78));
            assert_eq!(arr[2], JsValue::Bool(true));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_bigint_operations() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const big1 = 9007199254740993n;  // Larger than Number.MAX_SAFE_INTEGER
            const big2 = 2n ** 64n;
            const big3 = big1 + big2;

            [
                typeof big1,
                big1 > Number.MAX_SAFE_INTEGER,
                (big2 / 2n).toString(),
                big3 > big2
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("bigint".to_string()));
            assert_eq!(arr[1], JsValue::Bool(true));
            assert_eq!(arr[2], JsValue::String("9223372036854775808".to_string()));
            assert_eq!(arr[3], JsValue::Bool(true));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_regex_edge_cases() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            // Lookahead and lookbehind
            const text = 'foo123bar456baz';
            const lookahead = text.match(/\d+(?=bar)/);

            // Named groups
            const dateRegex = /(?<year>\d{4})-(?<month>\d{2})-(?<day>\d{2})/;
            const match = '2024-03-15'.match(dateRegex);

            // Unicode regex
            const emojiRegex = /\p{Emoji}/u;

            // Sticky flag
            const sticky = /\d+/y;
            sticky.lastIndex = 3;
            const stickyMatch = sticky.exec('abc123def');

            [
                lookahead ? lookahead[0] : null,
                match ? match.groups.year : null,
                emojiRegex.test('Hello 😀'),
                stickyMatch ? stickyMatch[0] : null
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("123".to_string()));
            assert_eq!(arr[1], JsValue::String("2024".to_string()));
            assert_eq!(arr[2], JsValue::Bool(true));
            assert_eq!(arr[3], JsValue::String("123".to_string()));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_date_operations() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const d1 = new Date(2024, 0, 15, 12, 30, 45);  // Jan 15, 2024
            const d2 = new Date('2024-06-20T10:00:00Z');

            [
                d1.getFullYear(),
                d1.getMonth(),  // 0-indexed
                d1.getDate(),
                d2.getUTCHours(),
                Date.UTC(2024, 5, 20) > Date.UTC(2024, 0, 15),
                new Date(0).toISOString()
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(2024));
            assert_eq!(arr[1], JsValue::Int(0));
            assert_eq!(arr[2], JsValue::Int(15));
            assert_eq!(arr[3], JsValue::Int(10));
            assert_eq!(arr[4], JsValue::Bool(true));
            assert_eq!(
                arr[5],
                JsValue::String("1970-01-01T00:00:00.000Z".to_string())
            );
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_error_types() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const errors = [];

            try { throw new TypeError('type error'); }
            catch (e) { errors.push(e.name); }

            try { throw new RangeError('range error'); }
            catch (e) { errors.push(e.name); }

            try { throw new SyntaxError('syntax error'); }
            catch (e) { errors.push(e.name); }

            try { throw new URIError('uri error'); }
            catch (e) { errors.push(e.name); }

            // Custom error
            class CustomError extends Error {
                constructor(msg) {
                    super(msg);
                    this.name = 'CustomError';
                }
            }
            try { throw new CustomError('custom'); }
            catch (e) { errors.push(e.name); }

            errors
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("TypeError".to_string()));
            assert_eq!(arr[1], JsValue::String("RangeError".to_string()));
            assert_eq!(arr[2], JsValue::String("SyntaxError".to_string()));
            assert_eq!(arr[3], JsValue::String("URIError".to_string()));
            assert_eq!(arr[4], JsValue::String("CustomError".to_string()));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_try_catch_finally_edge_cases() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const log = [];

            // Return in try with finally
            function f1() {
                try {
                    log.push('try');
                    return 'try-return';
                } finally {
                    log.push('finally');
                }
            }
            log.push(f1());

            // Return in finally overrides try return
            function f2() {
                try {
                    return 'try';
                } finally {
                    return 'finally';
                }
            }
            log.push(f2());

            // Throw in finally
            function f3() {
                try {
                    try {
                        throw new Error('inner');
                    } finally {
                        throw new Error('finally-throw');
                    }
                } catch (e) {
                    return e.message;
                }
            }
            log.push(f3());

            log
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("try".to_string()));
            assert_eq!(arr[1], JsValue::String("finally".to_string()));
            assert_eq!(arr[2], JsValue::String("try-return".to_string()));
            assert_eq!(arr[3], JsValue::String("finally".to_string()));
            assert_eq!(arr[4], JsValue::String("finally-throw".to_string()));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_eval_within_eval() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r#"
            const x = 10;
            const result = eval('eval("x + 5")');
            result
            "#,
            )
            .unwrap();
        assert_eq!(result, JsValue::Int(15));
    }

    #[test]
    fn test_with_statement() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        // Note: 'with' is deprecated but still valid in non-strict mode
        let result = engine
            .eval(
                r"
            const obj = { a: 1, b: 2, c: 3 };
            let sum = 0;
            with (obj) {
                sum = a + b + c;
            }
            sum
            ",
            )
            .unwrap();
        assert_eq!(result, JsValue::Int(6));
    }

    #[test]
    fn test_property_descriptors() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const obj = {};

            Object.defineProperty(obj, 'readonly', {
                value: 42,
                writable: false,
                enumerable: true,
                configurable: false
            });

            Object.defineProperty(obj, 'computed', {
                get() { return this._computed * 2; },
                set(v) { this._computed = v; },
                enumerable: true
            });

            obj.computed = 10;

            // Try to modify readonly (fails silently in non-strict)
            obj.readonly = 100;

            // Note: _computed is also created as enumerable when set
            [
                obj.readonly,
                obj.computed,
                Object.keys(obj).includes('readonly'),
                Object.getOwnPropertyDescriptor(obj, 'readonly').writable
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(42)); // Unchanged
            assert_eq!(arr[1], JsValue::Int(20)); // 10 * 2
            assert_eq!(arr[2], JsValue::Bool(true)); // 'readonly' is in keys
            assert_eq!(arr[3], JsValue::Bool(false)); // 'readonly' is not writable
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_prototype_manipulation() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            function Animal(name) {
                this.name = name;
            }
            Animal.prototype.speak = function() {
                return this.name + ' makes a sound';
            };

            function Dog(name) {
                Animal.call(this, name);
            }
            Dog.prototype = Object.create(Animal.prototype);
            Dog.prototype.constructor = Dog;
            Dog.prototype.speak = function() {
                return this.name + ' barks';
            };

            const dog = new Dog('Rex');

            [
                dog.speak(),
                dog instanceof Dog,
                dog instanceof Animal,
                Object.getPrototypeOf(dog) === Dog.prototype
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("Rex barks".to_string()));
            assert_eq!(arr[1], JsValue::Bool(true));
            assert_eq!(arr[2], JsValue::Bool(true));
            assert_eq!(arr[3], JsValue::Bool(true));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_class_features() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            class Counter {
                static count = 0;
                #privateValue = 0;

                constructor(initial = 0) {
                    this.#privateValue = initial;
                    Counter.count++;
                }

                get value() {
                    return this.#privateValue;
                }

                set value(v) {
                    this.#privateValue = v;
                }

                increment() {
                    this.#privateValue++;
                    return this;
                }

                static getCount() {
                    return Counter.count;
                }
            }

            const c1 = new Counter(10);
            const c2 = new Counter(20);
            c1.increment().increment();
            c2.value = 100;

            [
                c1.value,
                c2.value,
                Counter.count,
                Counter.getCount()
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(12));
            assert_eq!(arr[1], JsValue::Int(100));
            assert_eq!(arr[2], JsValue::Int(2));
            assert_eq!(arr[3], JsValue::Int(2));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_async_iteration() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        engine
            .eval(
                r"
            globalThis.results = [];

            async function* asyncGen() {
                yield 1;
                yield 2;
                yield 3;
            }

            (async () => {
                for await (const val of asyncGen()) {
                    globalThis.results.push(val);
                }
            })();
            ",
            )
            .unwrap();

        // Run pending jobs to complete async iteration
        engine.run_pending_jobs().unwrap();

        let result = engine.get_global("results").unwrap();
        assert_eq!(result, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn test_promise_all_race_any() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        engine
            .eval(
                r"
            globalThis.results = {};

            Promise.all([
                Promise.resolve(1),
                Promise.resolve(2),
                Promise.resolve(3)
            ]).then(r => globalThis.results.all = r);

            // Promise.race - first resolved wins
            Promise.race([
                new Promise(r => { /* never resolves */ }),
                Promise.resolve('fast')
            ]).then(r => globalThis.results.race = r);

            Promise.any([
                Promise.reject('fail1'),
                Promise.resolve('success'),
                Promise.reject('fail2')
            ]).then(r => globalThis.results.any = r);

            Promise.allSettled([
                Promise.resolve('ok'),
                Promise.reject('error')
            ]).then(r => globalThis.results.settled = r.map(x => x.status));
            ",
            )
            .unwrap();

        engine.run_pending_jobs().unwrap();

        let result = engine.get_global("results").unwrap();
        assert_eq!(result["all"], serde_json::json!([1, 2, 3]));
        assert_eq!(result["race"], serde_json::json!("fast"));
        assert_eq!(result["any"], serde_json::json!("success"));
        assert_eq!(
            result["settled"],
            serde_json::json!(["fulfilled", "rejected"])
        );
    }

    #[test]
    fn test_destructuring_edge_cases() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            // Nested destructuring
            const { a: { b: { c } } } = { a: { b: { c: 42 } } };

            // Default values
            const { x = 10, y = 20 } = { x: 5 };

            // Rest in destructuring
            const { first, ...rest } = { first: 1, second: 2, third: 3 };

            // Array destructuring with holes
            const [, , third] = [1, 2, 3];

            // Swapping
            let p = 1, q = 2;
            [p, q] = [q, p];

            [c, x, y, Object.keys(rest).length, third, p, q]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(42));
            assert_eq!(arr[1], JsValue::Int(5));
            assert_eq!(arr[2], JsValue::Int(20));
            assert_eq!(arr[3], JsValue::Int(2));
            assert_eq!(arr[4], JsValue::Int(3));
            assert_eq!(arr[5], JsValue::Int(2));
            assert_eq!(arr[6], JsValue::Int(1));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_spread_operator() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const arr1 = [1, 2, 3];
            const arr2 = [4, 5, 6];
            const merged = [...arr1, ...arr2];

            const obj1 = { a: 1, b: 2 };
            const obj2 = { c: 3, d: 4 };
            const mergedObj = { ...obj1, ...obj2, e: 5 };

            function sum(...nums) {
                return nums.reduce((a, b) => a + b, 0);
            }

            [
                merged.length,
                sum(...merged),
                Object.keys(mergedObj).length,
                mergedObj.e
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(6));
            assert_eq!(arr[1], JsValue::Int(21));
            assert_eq!(arr[2], JsValue::Int(5));
            assert_eq!(arr[3], JsValue::Int(5));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_optional_chaining() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const obj = {
                a: {
                    b: {
                        c: 42
                    }
                },
                fn: () => 'called'
            };

            [
                obj?.a?.b?.c,
                obj?.a?.x?.y,
                obj?.fn?.(),
                obj?.missing?.(),
                obj.arr?.[0]
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(42));
            assert_eq!(arr[1], JsValue::Null);
            assert_eq!(arr[2], JsValue::String("called".to_string()));
            assert_eq!(arr[3], JsValue::Null);
            assert_eq!(arr[4], JsValue::Null);
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_nullish_coalescing() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const a = null ?? 'default';
            const b = undefined ?? 'default';
            const c = 0 ?? 'default';
            const d = '' ?? 'default';
            const e = false ?? 'default';

            // Nullish assignment
            let x = null;
            x ??= 'assigned';

            let y = 'existing';
            y ??= 'not assigned';

            [a, b, c, d, e, x, y]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("default".to_string()));
            assert_eq!(arr[1], JsValue::String("default".to_string()));
            assert_eq!(arr[2], JsValue::Int(0)); // 0 is not nullish
            assert_eq!(arr[3], JsValue::String(String::new())); // '' is not nullish
            assert_eq!(arr[4], JsValue::Bool(false)); // false is not nullish
            assert_eq!(arr[5], JsValue::String("assigned".to_string()));
            assert_eq!(arr[6], JsValue::String("existing".to_string()));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_template_literals_edge_cases() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const x = 5;

            // Nested template literals
            const nested = `outer ${`inner ${x}`} more`;

            // Tagged template
            function tag(strings, ...values) {
                return strings.reduce((acc, str, i) =>
                    acc + str + (values[i] !== undefined ? values[i] * 2 : ''), '');
            }
            const tagged = tag`value: ${x} and ${10}`;

            // Template with expressions
            const expr = `${1 + 2} ${x > 3 ? 'big' : 'small'}`;

            [nested, tagged, expr]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("outer inner 5 more".to_string()));
            assert_eq!(arr[1], JsValue::String("value: 10 and 20".to_string()));
            assert_eq!(arr[2], JsValue::String("3 big".to_string()));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_json_edge_cases() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r#"
            // Circular reference handling
            const circular = { a: 1 };
            circular.self = circular;
            let circularResult;
            try {
                JSON.stringify(circular);
                circularResult = 'no error';
            } catch (e) {
                circularResult = 'error';
            }

            // Replacer function
            const obj = { a: 1, b: 2, secret: 'hidden' };
            const filtered = JSON.stringify(obj, (key, value) =>
                key === 'secret' ? undefined : value
            );

            // Reviver function
            const dateStr = '{"date":"2024-01-15"}';
            const parsed = JSON.parse(dateStr, (key, value) =>
                key === 'date' ? new Date(value).getFullYear() : value
            );

            // Spacer
            const pretty = JSON.stringify({ x: 1 }, null, 2);

            [circularResult, filtered, parsed.date, pretty.includes('\n')]
            "#,
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("error".to_string()));
            assert_eq!(arr[1], JsValue::String(r#"{"a":1,"b":2}"#.to_string()));
            assert_eq!(arr[2], JsValue::Int(2024));
            assert_eq!(arr[3], JsValue::Bool(true));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_math_edge_cases() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            [
                Math.max(),         // -Infinity
                Math.min(),         // Infinity
                Math.pow(2, 10),
                Math.log2(1024),
                Math.sign(-5),
                Math.trunc(4.7),
                Math.cbrt(27),
                Math.hypot(3, 4),
                Math.imul(0xffffffff, 5),  // 32-bit integer multiplication
                Math.clz32(1)       // Count leading zeros
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            // Note: -Infinity and Infinity map to null in JSON
            assert_eq!(arr[0], JsValue::Null);
            assert_eq!(arr[1], JsValue::Null);
            assert_eq!(arr[2], JsValue::Int(1024));
            assert_eq!(arr[3], JsValue::Int(10));
            assert_eq!(arr[4], JsValue::Int(-1));
            assert_eq!(arr[5], JsValue::Int(4));
            assert_eq!(arr[6], JsValue::Int(3));
            assert_eq!(arr[7], JsValue::Int(5));
            assert_eq!(arr[8], JsValue::Int(-5));
            assert_eq!(arr[9], JsValue::Int(31));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_number_edge_cases() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            [
                Number.MAX_SAFE_INTEGER,
                Number.MIN_SAFE_INTEGER,
                Number.EPSILON > 0,
                Number.isInteger(1.0),
                Number.isInteger(1.5),
                Number.isSafeInteger(9007199254740991),
                Number.isSafeInteger(9007199254740992),
                (0.1).toFixed(20),
                (1234.5678).toPrecision(4),
                parseInt('ff', 16),
                parseFloat('3.14abc')
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(9007199254740991));
            assert_eq!(arr[1], JsValue::Int(-9007199254740991));
            assert_eq!(arr[2], JsValue::Bool(true));
            assert_eq!(arr[3], JsValue::Bool(true));
            assert_eq!(arr[4], JsValue::Bool(false));
            assert_eq!(arr[5], JsValue::Bool(true));
            assert_eq!(arr[6], JsValue::Bool(false));
            assert_eq!(arr[9], JsValue::Int(255));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_string_methods() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const s = '  Hello, World!  ';
            [
                s.trim(),
                s.trimStart(),
                s.trimEnd(),
                'abc'.repeat(3),
                'hello'.padStart(10, '*'),
                'hello'.padEnd(10, '*'),
                'a,b,c'.split(','),
                'hello'.charAt(1),
                'hello'.charCodeAt(0),
                String.fromCharCode(65, 66, 67),
                'HELLO'.toLowerCase(),
                'hello'.toUpperCase(),
                'hello world'.replaceAll('l', 'L'),
                'hello'.startsWith('hel'),
                'hello'.endsWith('lo'),
                'hello'.includes('ell')
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::String("Hello, World!".to_string()));
            assert_eq!(arr[3], JsValue::String("abcabcabc".to_string()));
            assert_eq!(arr[4], JsValue::String("*****hello".to_string()));
            assert_eq!(arr[9], JsValue::String("ABC".to_string()));
            assert_eq!(arr[12], JsValue::String("heLLo worLd".to_string()));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_array_methods() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const arr = [3, 1, 4, 1, 5, 9, 2, 6];
            [
                arr.find(x => x > 4),
                arr.findIndex(x => x > 4),
                arr.findLast(x => x < 5),
                arr.findLastIndex(x => x < 5),
                arr.includes(4),
                arr.indexOf(1),
                arr.lastIndexOf(1),
                arr.every(x => x > 0),
                arr.some(x => x > 8),
                arr.flat !== undefined,
                [[1, 2], [3, 4]].flat(),
                [1, 2, 3].flatMap(x => [x, x * 2]),
                Array.from('abc'),
                Array.of(1, 2, 3),
                [1, 2, 3].at(-1),
                [...arr].sort((a, b) => a - b)
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(5));
            assert_eq!(arr[1], JsValue::Int(4));
            assert_eq!(arr[4], JsValue::Bool(true));
            assert_eq!(arr[14], JsValue::Int(3));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_object_methods() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const obj = { a: 1, b: 2, c: 3 };
            const frozen = Object.freeze({ x: 1 });
            const sealed = Object.seal({ y: 2 });

            [
                Object.keys(obj),
                Object.values(obj),
                Object.entries(obj).length,
                Object.fromEntries([['a', 1], ['b', 2]]).b,
                Object.isFrozen(frozen),
                Object.isSealed(sealed),
                Object.isExtensible(obj),
                Object.assign({}, obj, { d: 4 }).d,
                Object.hasOwn(obj, 'a'),
                Object.hasOwn(obj, 'toString'),
                Object.getOwnPropertyNames(obj).length
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[3], JsValue::Int(2));
            assert_eq!(arr[4], JsValue::Bool(true));
            assert_eq!(arr[5], JsValue::Bool(true));
            assert_eq!(arr[6], JsValue::Bool(true));
            assert_eq!(arr[7], JsValue::Int(4));
            assert_eq!(arr[8], JsValue::Bool(true));
            assert_eq!(arr[9], JsValue::Bool(false));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_iteration_protocols() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            // Custom iterable
            const range = {
                start: 1,
                end: 5,
                [Symbol.iterator]() {
                    let current = this.start;
                    const end = this.end;
                    return {
                        next() {
                            if (current <= end) {
                                return { value: current++, done: false };
                            }
                            return { done: true };
                        }
                    };
                }
            };

            [
                [...range],
                Array.from(range),
                // Test built-in iterables
                [...'abc'],
                [...new Set([1, 2, 2, 3])],
                [...new Map([['a', 1], ['b', 2]])].length
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(
                arr[0],
                JsValue::Array(vec![
                    JsValue::Int(1),
                    JsValue::Int(2),
                    JsValue::Int(3),
                    JsValue::Int(4),
                    JsValue::Int(5)
                ])
            );
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    #[ignore = "Can cause process abort due to actual stack overflow - run manually"]
    fn test_deep_recursion_with_limit() {
        let config = EngineConfig {
            max_stack_size: 512 * 1024, // 512KB
            ..Default::default()
        };
        let mut engine = QuickJsEngine::new(config).unwrap();

        // Find maximum safe recursion depth
        let result = engine
            .eval(
                r"
            let maxDepth = 0;
            function testDepth(n) {
                maxDepth = Math.max(maxDepth, n);
                if (n > 5000) return n;  // Safety limit
                return testDepth(n + 1);
            }
            try {
                testDepth(0);
            } catch (e) {
                // Stack overflow expected
            }
            maxDepth
            ",
            )
            .unwrap();

        // Should have reached some depth before overflow
        if let JsValue::Int(depth) = result {
            assert!(depth > 100, "Should reach reasonable depth: {depth}");
        } else {
            panic!("Expected int");
        }
    }

    #[test]
    fn test_mutual_recursion() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            function isEven(n) {
                if (n === 0) return true;
                return isOdd(n - 1);
            }
            function isOdd(n) {
                if (n === 0) return false;
                return isEven(n - 1);
            }
            [isEven(100), isOdd(100), isEven(101), isOdd(101)]
            ",
            )
            .unwrap();

        assert_eq!(
            result,
            JsValue::Array(vec![
                JsValue::Bool(true),
                JsValue::Bool(false),
                JsValue::Bool(false),
                JsValue::Bool(true)
            ])
        );
    }

    #[test]
    fn test_very_long_string() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const longStr = 'x'.repeat(100000);
            [
                longStr.length,
                longStr.indexOf('x'),
                longStr.lastIndexOf('x'),
                longStr.slice(0, 5),
                longStr.charAt(50000)
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(100000));
            assert_eq!(arr[1], JsValue::Int(0));
            assert_eq!(arr[2], JsValue::Int(99999));
            assert_eq!(arr[3], JsValue::String("xxxxx".to_string()));
            assert_eq!(arr[4], JsValue::String("x".to_string()));
        } else {
            panic!("Expected array");
        }
    }

    #[test]
    fn test_rapid_object_creation() {
        let mut engine = QuickJsEngine::new(EngineConfig::default()).unwrap();

        let result = engine
            .eval(
                r"
            const objects = [];
            for (let i = 0; i < 10000; i++) {
                objects.push({ id: i, data: 'test' + i });
            }
            [
                objects.length,
                objects[5000].id,
                objects[9999].data
            ]
            ",
            )
            .unwrap();

        if let JsValue::Array(arr) = result {
            assert_eq!(arr[0], JsValue::Int(10000));
            assert_eq!(arr[1], JsValue::Int(5000));
            assert_eq!(arr[2], JsValue::String("test9999".to_string()));
        } else {
            panic!("Expected array");
        }
    }
}
