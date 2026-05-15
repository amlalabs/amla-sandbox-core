//! C FFI bindings to `QuickJS` via wrapper.c
//!
//! These bindings work identically for native and WASM builds.
//! The C code is compiled at build time and linked in via the `cc` crate.
//!
//! All value exchange uses JSON strings for simplicity and portability.

use std::os::raw::{c_char, c_int, c_void};

/// Opaque `QuickJS` runtime handle.
#[repr(C)]
pub struct QjsRuntime {
    _private: [u8; 0],
}

/// Opaque `QuickJS` context handle.
#[repr(C)]
pub struct QjsContext {
    _private: [u8; 0],
}

/// Success return code.
pub const QJS_OK: c_int = 0;

/// Error return code.
pub const QJS_ERROR: c_int = -1;

/// Exception occurred.
pub const QJS_EXCEPTION: c_int = -2;

/// Iteration budget exhausted (infinite loop protection).
pub const QJS_BUDGET_EXHAUSTED: c_int = -3;

/// C callback type for registered functions.
///
/// # Parameters
/// - `args_json`: JSON array of arguments (caller owns)
/// - `user_data`: User-provided data from `qjs_add_function`
///
/// # Returns
/// JSON-encoded result (caller must free with `qjs_free_string`) or NULL for undefined.
pub type QjsCCallback =
    extern "C" fn(args_json: *const c_char, user_data: *mut c_void) -> *mut c_char;

unsafe extern "C" {
    // ========== Runtime lifecycle ==========

    /// Create a new `QuickJS` runtime.
    /// Returns NULL on failure.
    pub fn qjs_new_runtime() -> *mut QjsRuntime;

    /// Free a `QuickJS` runtime and all associated contexts.
    pub fn qjs_free_runtime(rt: *mut QjsRuntime);

    /// Set memory limit in bytes (0 = unlimited).
    pub fn qjs_set_memory_limit(rt: *mut QjsRuntime, limit: usize);

    /// Set maximum stack size in bytes.
    pub fn qjs_set_max_stack_size(rt: *mut QjsRuntime, size: usize);

    /// Set interrupt handler with instruction limit.
    ///
    /// # Parameters
    /// - `instruction_limit`: Number of instructions before interrupt (0 = disable)
    ///
    /// This provides a CPU-time-like limit by counting instructions executed.
    /// When the limit is reached, JS execution is interrupted and returns
    /// an exception with `InternalError: interrupted`.
    pub fn qjs_set_instruction_limit(rt: *mut QjsRuntime, instruction_limit: u64);

    /// Manually interrupt execution.
    ///
    /// This can be called from any context (e.g., signal handler, timer thread)
    /// to interrupt currently running JavaScript code.
    pub fn qjs_interrupt(rt: *mut QjsRuntime);

    /// Clear interrupt flag without triggering.
    ///
    /// Call this before starting new execution if you want to reset the
    /// interrupt state.
    pub fn qjs_clear_interrupt(rt: *mut QjsRuntime);

    // ========== Context lifecycle ==========

    /// Create a new `QuickJS` context within a runtime.
    /// Includes all standard intrinsics (Date, JSON, Promise, etc).
    /// Returns NULL on failure.
    pub fn qjs_new_context(rt: *mut QjsRuntime) -> *mut QjsContext;

    /// Free a `QuickJS` context.
    pub fn qjs_free_context(ctx: *mut QjsContext);

    // ========== Evaluation ==========

    /// Evaluate JavaScript code.
    ///
    /// # Parameters
    /// - `ctx`: `QuickJS` context
    /// - `code`: JavaScript source (not null-terminated, use len)
    /// - `len`: Length of code in bytes
    /// - `filename`: Filename for error messages (can be NULL)
    ///
    /// # Returns
    /// JSON-encoded result (caller must free with `qjs_free_string`)
    /// or NULL on exception (call `qjs_get_exception`)
    ///
    /// Note: Supports top-level await (`JS_EVAL_FLAG_ASYNC`).
    pub fn qjs_eval(
        ctx: *mut QjsContext,
        code: *const c_char,
        len: usize,
        filename: *const c_char,
    ) -> *mut c_char;

    /// Free a string returned by qjs_* functions.
    pub fn qjs_free_string(s: *mut c_char);

    // ========== Exception handling ==========

    /// Get the current exception as JSON.
    ///
    /// Returns JSON object with "message" and optional "stack" fields
    /// (caller must free with `qjs_free_string`) or NULL if no exception.
    ///
    /// Note: Calling this function clears the exception.
    pub fn qjs_get_exception(ctx: *mut QjsContext) -> *mut c_char;

    // ========== Global object manipulation ==========

    /// Set a global variable to a JSON value.
    ///
    /// # Parameters
    /// - `name`: Variable name
    /// - `json`: JSON-encoded value (NULL for undefined)
    ///
    /// # Returns
    /// `QJS_OK`, `QJS_ERROR`, or `QJS_EXCEPTION`
    pub fn qjs_set_global_json(
        ctx: *mut QjsContext,
        name: *const c_char,
        json: *const c_char,
    ) -> c_int;

    /// Get a global variable as JSON.
    ///
    /// Returns JSON-encoded value (caller must free with `qjs_free_string`)
    /// or NULL if not found or on error.
    pub fn qjs_get_global_json(ctx: *mut QjsContext, name: *const c_char) -> *mut c_char;

    // ========== Function registration ==========

    /// Register a native function as a global.
    ///
    /// # Parameters
    /// - `name`: Function name in global scope
    /// - `callback`: C function to call (receives JSON args, returns JSON result)
    /// - `user_data`: Arbitrary data passed to callback
    ///
    /// # Returns
    /// `QJS_OK`, `QJS_ERROR`, or `QJS_EXCEPTION`
    ///
    /// Note: callback will receive args as JSON array: "[arg0, arg1, ...]"
    pub fn qjs_add_function(
        ctx: *mut QjsContext,
        name: *const c_char,
        callback: QjsCCallback,
        user_data: *mut c_void,
    ) -> c_int;

    // ========== Function calling ==========

    /// Call a global function with JSON arguments.
    ///
    /// # Parameters
    /// - `name`: Function name in global scope
    /// - `args_json`: JSON array of arguments (e.g., "[1, \"hello\", true]")
    ///   or NULL for no arguments
    ///
    /// # Returns
    /// JSON-encoded result (caller must free with `qjs_free_string`)
    /// or NULL on exception (call `qjs_get_exception`)
    pub fn qjs_call_function(
        ctx: *mut QjsContext,
        name: *const c_char,
        args_json: *const c_char,
    ) -> *mut c_char;

    // ========== Promise/Job queue ==========

    /// Run pending jobs (microtask queue) with default iteration limit.
    ///
    /// Returns number of jobs executed, `QJS_EXCEPTION` on error, or
    /// `QJS_BUDGET_EXHAUSTED` if iteration limit reached.
    ///
    /// Uses a default limit of 10000 iterations to prevent infinite loops.
    pub fn qjs_run_pending_jobs(rt: *mut QjsRuntime) -> c_int;

    /// Run pending jobs with custom iteration limit.
    ///
    /// # Parameters
    /// - `max_iterations`: Maximum number of jobs to execute (0 = use default)
    ///
    /// # Returns
    /// Number of jobs executed, `QJS_EXCEPTION` on error, or
    /// `QJS_BUDGET_EXHAUSTED` if iteration limit reached.
    ///
    /// Use this to prevent infinite microtask loops from untrusted JS code.
    #[allow(dead_code)] // Available for future use with custom iteration limits
    pub fn qjs_run_pending_jobs_limited(rt: *mut QjsRuntime, max_iterations: c_int) -> c_int;

    /// Check if there are pending jobs.
    ///
    /// Returns 1 if pending jobs exist, 0 otherwise.
    pub fn qjs_has_pending_jobs(rt: *mut QjsRuntime) -> c_int;

    // ========== Promise manipulation ==========

    /// Create a new Promise and return its ID.
    ///
    /// # Parameters
    /// - `promise_id`: Output parameter for the promise ID
    ///
    /// # Returns
    /// JSON representation of the promise (for reference tracking) or NULL on error.
    ///
    /// Use `qjs_resolve_promise` or `qjs_reject_promise` to settle the promise.
    pub fn qjs_new_promise(ctx: *mut QjsContext, promise_id: *mut u64) -> *mut c_char;

    /// Resolve a promise created with `qjs_new_promise`.
    ///
    /// # Parameters
    /// - `promise_id`: ID returned by `qjs_new_promise`
    /// - `value_json`: JSON-encoded value to resolve with (NULL for undefined)
    ///
    /// # Returns
    /// `QJS_OK`, `QJS_ERROR`, or `QJS_EXCEPTION`
    pub fn qjs_resolve_promise(
        ctx: *mut QjsContext,
        promise_id: u64,
        value_json: *const c_char,
    ) -> c_int;

    /// Reject a promise created with `qjs_new_promise`.
    ///
    /// # Parameters
    /// - `promise_id`: ID returned by `qjs_new_promise`
    /// - `error_json`: JSON-encoded error to reject with
    ///
    /// # Returns
    /// `QJS_OK`, `QJS_ERROR`, or `QJS_EXCEPTION`
    pub fn qjs_reject_promise(
        ctx: *mut QjsContext,
        promise_id: u64,
        error_json: *const c_char,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants() {
        assert_eq!(QJS_OK, 0);
    }
}
