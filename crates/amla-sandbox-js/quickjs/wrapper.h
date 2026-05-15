/*
 * wrapper.h - Simplified FFI wrapper for QuickJS
 *
 * Provides a C API surface suitable for Rust FFI bindings.
 * Works identically for native and WASM (wasm32-wasip1) builds.
 */

#ifndef AMLA_QJS_WRAPPER_H
#define AMLA_QJS_WRAPPER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque handles */
typedef struct QJS_Runtime QJS_Runtime;
typedef struct QJS_Context QJS_Context;

/* Error codes */
#define QJS_OK 0
#define QJS_ERROR -1
#define QJS_EXCEPTION -2
#define QJS_BUDGET_EXHAUSTED                                                   \
  -3 /* Iteration budget exceeded (infinite loop protection) */

/*
 * Callback type for registered functions.
 *
 * args_json: JSON array of arguments (caller owns)
 * user_data: User-provided data from qjs_add_function
 *
 * Returns: JSON-encoded result (caller must free with qjs_free_string)
 *          or NULL for undefined
 */
typedef char *(*QJS_CCallback)(const char *args_json, void *user_data);

/* ========== Runtime lifecycle ========== */

/*
 * Create a new QuickJS runtime.
 * Returns NULL on failure.
 */
QJS_Runtime *qjs_new_runtime(void);

/*
 * Free a QuickJS runtime and all associated contexts.
 */
void qjs_free_runtime(QJS_Runtime *rt);

/*
 * Set memory limit in bytes (0 = unlimited).
 */
void qjs_set_memory_limit(QJS_Runtime *rt, size_t limit);

/*
 * Set maximum stack size in bytes.
 */
void qjs_set_max_stack_size(QJS_Runtime *rt, size_t size);

/*
 * Set interrupt handler with instruction limit.
 *
 * instruction_limit: Number of instructions before interrupt (0 = disable)
 *
 * This provides a CPU-time-like limit by counting instructions executed.
 * When the limit is reached, JS execution is interrupted and returns
 * QJS_EXCEPTION with an "InternalError: interrupted" error.
 *
 * Note: The count resets on each call to qjs_eval or qjs_call_function.
 */
void qjs_set_instruction_limit(QJS_Runtime *rt, uint64_t instruction_limit);

/*
 * Manually interrupt execution.
 *
 * This can be called from any context (e.g., signal handler, timer thread)
 * to interrupt currently running JavaScript code.
 */
void qjs_interrupt(QJS_Runtime *rt);

/*
 * Clear interrupt flag without triggering.
 *
 * Call this before starting new execution if you want to reset the
 * interrupt state.
 */
void qjs_clear_interrupt(QJS_Runtime *rt);

/* ========== Context lifecycle ========== */

/*
 * Create a new QuickJS context within a runtime.
 * Includes all standard intrinsics (Date, JSON, Promise, etc).
 * Returns NULL on failure.
 */
QJS_Context *qjs_new_context(QJS_Runtime *rt);

/*
 * Free a QuickJS context.
 */
void qjs_free_context(QJS_Context *ctx);

/* ========== Evaluation ========== */

/*
 * Evaluate JavaScript code.
 *
 * code: JavaScript source (not null-terminated, use len)
 * len: Length of code in bytes
 * filename: Filename for error messages (can be NULL)
 *
 * Returns: JSON-encoded result (caller must free with qjs_free_string)
 *          or NULL on exception (call qjs_get_exception)
 *
 * Note: Supports top-level await (JS_EVAL_FLAG_ASYNC).
 */
char *qjs_eval(QJS_Context *ctx, const char *code, size_t len,
               const char *filename);

/*
 * Free a string returned by qjs_* functions.
 */
void qjs_free_string(char *str);

/* ========== Exception handling ========== */

/*
 * Get the current exception as JSON.
 *
 * Returns: JSON object with "message" and optional "stack" fields
 *          (caller must free with qjs_free_string)
 *          or NULL if no exception
 *
 * Note: Calling this function clears the exception.
 */
char *qjs_get_exception(QJS_Context *ctx);

/*
 * Clear the current exception without retrieving it.
 */
void qjs_clear_exception(QJS_Context *ctx);

/* ========== Global object manipulation ========== */

/*
 * Set a global variable to a JSON value.
 *
 * name: Variable name
 * json: JSON-encoded value (NULL for undefined)
 *
 * Returns: QJS_OK, QJS_ERROR, or QJS_EXCEPTION
 */
int qjs_set_global_json(QJS_Context *ctx, const char *name, const char *json);

/*
 * Get a global variable as JSON.
 *
 * name: Variable name
 *
 * Returns: JSON-encoded value (caller must free with qjs_free_string)
 *          or NULL if not found or on error
 */
char *qjs_get_global_json(QJS_Context *ctx, const char *name);

/* ========== Function registration ========== */

/*
 * Register a native function as a global.
 *
 * name: Function name in global scope
 * callback: C function to call (receives JSON args, returns JSON result)
 * user_data: Arbitrary data passed to callback
 *
 * Returns: QJS_OK, QJS_ERROR, or QJS_EXCEPTION
 *
 * Note: callback will receive args as JSON array: "[arg0, arg1, ...]"
 */
int qjs_add_function(QJS_Context *ctx, const char *name, QJS_CCallback callback,
                     void *user_data);

/* ========== Function calling ========== */

/*
 * Call a global function with JSON arguments.
 *
 * name: Function name in global scope
 * args_json: JSON array of arguments (e.g., "[1, \"hello\", true]")
 *            or NULL for no arguments
 *
 * Returns: JSON-encoded result (caller must free with qjs_free_string)
 *          or NULL on exception (call qjs_get_exception)
 */
char *qjs_call_function(QJS_Context *ctx, const char *name,
                        const char *args_json);

/* ========== Promise/Job queue ========== */

/*
 * Run pending jobs (microtask queue) with default iteration limit.
 *
 * Returns: Number of jobs executed, QJS_EXCEPTION on error, or
 *          QJS_BUDGET_EXHAUSTED if iteration limit reached
 *
 * Note: Uses a default limit of 10000 iterations to prevent infinite loops.
 */
int qjs_run_pending_jobs(QJS_Runtime *rt);

/*
 * Run pending jobs with custom iteration limit.
 *
 * max_iterations: Maximum number of jobs to execute (0 = use default)
 *
 * Returns: Number of jobs executed, QJS_EXCEPTION on error, or
 *          QJS_BUDGET_EXHAUSTED if iteration limit reached
 *
 * Use this to prevent infinite microtask loops from untrusted JS code.
 */
int qjs_run_pending_jobs_limited(QJS_Runtime *rt, int max_iterations);

/*
 * Check if there are pending jobs.
 *
 * Returns: 1 if pending jobs exist, 0 otherwise
 */
int qjs_has_pending_jobs(QJS_Runtime *rt);

/* ========== Context opaque data ========== */

/*
 * Set opaque user data on context (for callbacks).
 */
void qjs_set_context_opaque(QJS_Context *ctx, void *opaque);

/*
 * Get opaque user data from context.
 */
void *qjs_get_context_opaque(QJS_Context *ctx);

/* ========== Promise manipulation ========== */

/*
 * Create a new Promise and return its ID.
 *
 * promise_id: Output parameter for the promise ID
 *
 * Returns: JSON representation of the promise (for reference tracking)
 *          or NULL on error
 *
 * Use qjs_resolve_promise or qjs_reject_promise to settle the promise.
 */
char *qjs_new_promise(QJS_Context *ctx, uint64_t *promise_id);

/*
 * Resolve a promise created with qjs_new_promise.
 *
 * promise_id: ID returned by qjs_new_promise
 * value_json: JSON-encoded value to resolve with (NULL for undefined)
 *
 * Returns: QJS_OK, QJS_ERROR, or QJS_EXCEPTION
 */
int qjs_resolve_promise(QJS_Context *ctx, uint64_t promise_id,
                        const char *value_json);

/*
 * Reject a promise created with qjs_new_promise.
 *
 * promise_id: ID returned by qjs_new_promise
 * error_json: JSON-encoded error to reject with
 *
 * Returns: QJS_OK, QJS_ERROR, or QJS_EXCEPTION
 */
int qjs_reject_promise(QJS_Context *ctx, uint64_t promise_id,
                       const char *error_json);

#ifdef __cplusplus
}
#endif

#endif /* AMLA_QJS_WRAPPER_H */
