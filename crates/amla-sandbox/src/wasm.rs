//! WASM export functions for the runtime interface.
//!
//! This module provides the WASM interface for runtime execution:
//!
//! ## Runtime Management
//! - `runtime_new` - Create a runtime from PCA, returns a runtime ID
//! - `runtime_destroy` - Destroy a runtime and all its commands
//! - `runtime_step` - Step a runtime, returns pending host ops and status
//!
//! ## Command Management (require runtime ID + handle)
//! - `cmd_create` - Create a command instance in a runtime
//! - `cmd_delete` - Delete a command instance
//! - `cmd_cancel` - Cancel a running command and its pending host ops
//!
//! ## Host Operations
//! - `submit` - Provide host operation results
//!
//! # Safety
//!
//! All functions use raw pointers for WASM interop. Callers must ensure:
//! - Pointers are valid for the specified lengths
//! - Memory is properly allocated before calls
//! - No concurrent access (WASM is single-threaded)

// Expected patterns for WASM extern "C" functions
#![allow(unsafe_code)] // WASM exports require unsafe FFI
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::similar_names)]

#[cfg(feature = "panic-recovery")]
use std::panic::{AssertUnwindSafe, catch_unwind};

use std::cell::Cell;
use std::rc::Rc;

use amla_scheduler::{RandomSourceFn, TimeSourceFn};

use crate::host_ops::HostOpResult;
use crate::runtime::{self, Runtime};
use crate::types::{CommandHandle, RuntimeId};

// =============================================================================
// Time and Random Source Functions
// =============================================================================
//
// For WASI builds, time and random come from WASI syscalls:
// - clock_time_get -> std::time::SystemTime
// - random_get -> getrandom crate (automatic with WASI)
//
// The host controls these for determinism by intercepting WASI imports
// and routing to per-runtime state based on which runtime is being stepped.

use amla_scheduler::ClockType;
use std::time::Instant;

/// Process start time for monotonic clock base.
///
/// We use `Instant::now()` at process start as the monotonic epoch.
/// All monotonic timestamps are relative to this, converted to nanoseconds.
static MONOTONIC_EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn monotonic_epoch() -> &'static Instant {
    MONOTONIC_EPOCH.get_or_init(Instant::now)
}

/// Get realtime (wall clock) timestamp in nanoseconds since Unix epoch.
///
/// Uses `SystemTime::now()` which routes through WASI `clock_time_get(REALTIME)`.
/// Can jump backwards due to NTP adjustments.
#[allow(clippy::cast_possible_truncation)]
fn get_realtime_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos().min(u128::from(u64::MAX)) as u64)
}

/// Get monotonic timestamp in nanoseconds since process start.
///
/// Uses `Instant::now()` which routes through WASI `clock_time_get(MONOTONIC)`.
/// Never goes backwards, suitable for measuring durations and scheduling sleeps.
#[allow(clippy::cast_possible_truncation)]
fn get_monotonic_nanos() -> u64 {
    monotonic_epoch()
        .elapsed()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}

/// Create a time source function for WASM.
///
/// Handles both clock types:
/// - `Realtime`: Wall clock time (nanoseconds since Unix epoch)
/// - `Monotonic`: Monotonic time (nanoseconds since process start)
///
/// For deterministic replay, the host intercepts WASI `clock_time_get` and
/// provides controlled values. The `runtime_id` parameter enables per-runtime
/// time isolation.
fn wasm_time_source() -> TimeSourceFn {
    Rc::new(|_runtime_id, clock| match clock {
        ClockType::Realtime => get_realtime_nanos(),
        ClockType::Monotonic => get_monotonic_nanos(),
    })
}

/// Create a random source function for WASM.
///
/// Returns a counter value for deterministic ordering.
/// For actual randomness, this could be replaced with a host import.
fn wasm_random_source() -> RandomSourceFn {
    let counter = Rc::new(Cell::new(0u64));
    Rc::new(move |_runtime_id| {
        let c = counter.get();
        counter.set(c.wrapping_add(1));
        c
    })
}

#[cfg(feature = "panic-recovery")]
use crate::types::{HostOpsVec, RuntimeStatus, StepResponse};

// =============================================================================
// Panic Recovery (only available with panic-recovery feature)
// =============================================================================

/// Extract a message from a panic payload.
#[cfg(feature = "panic-recovery")]
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Write a panic response to the output buffer.
///
/// Returns the number of bytes written.
#[cfg(feature = "panic-recovery")]
fn write_panic_response(
    runtime_id: RuntimeId,
    message: &str,
    out_ptr: *mut u8,
    out_len: usize,
) -> usize {
    // Destroy the panicked runtime
    if runtime_id.is_valid() {
        let _ = runtime::remove_runtime(runtime_id);
    }

    // Create panic response
    let response = StepResponse {
        host_ops: HostOpsVec::new(),
        status: RuntimeStatus::panic(message),
    };

    // Serialize and write
    let Ok(json) = serde_json::to_string(&response) else {
        return 0;
    };

    let bytes = json.as_bytes();
    let n = bytes.len().min(out_len);

    if !out_ptr.is_null() && out_len > 0 {
        // SAFETY: Caller guarantees valid pointer and length
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, n);
        }
    }

    n
}

// =============================================================================
// Error Tracking (for debugging)
// =============================================================================

thread_local! {
    /// Last error message for debugging.
    static LAST_ERROR: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Get the last error message.
///
/// # Arguments
/// * `out_ptr` - Pointer to output buffer for error message (UTF-8)
/// * `out_len` - Length of output buffer
///
/// # Returns
/// Length of error message written, or 0 if no error.
#[unsafe(no_mangle)]
pub extern "C" fn get_last_error(out_ptr: *mut u8, out_len: usize) -> usize {
    LAST_ERROR.with(|err| {
        let msg = err.borrow();
        if msg.is_empty() {
            return 0;
        }
        let bytes = msg.as_bytes();
        let n = bytes.len().min(out_len);
        if !out_ptr.is_null() && out_len > 0 {
            // SAFETY: Caller guarantees valid pointer and length
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, n);
            }
        }
        n
    })
}

/// Set the last error message.
fn set_last_error(msg: &str) {
    LAST_ERROR.with(|err| {
        *err.borrow_mut() = msg.to_string();
    });
}

/// Clear the last error message.
fn clear_last_error() {
    LAST_ERROR.with(|err| {
        err.borrow_mut().clear();
    });
}

// =============================================================================
// Trusted Authorities Configuration
// =============================================================================

/// Set the trusted authorities for PCA validation.
///
/// PCAs must be signed by one of these authorities to be accepted.
/// Call this before creating any runtimes.
///
/// # Arguments
///
/// * `authorities_ptr` - Pointer to JSON array of hex-encoded public keys
/// * `authorities_len` - Length of JSON in bytes
///
/// # JSON Format
///
/// ```json
/// ["ed25519:abc123...", "ed25519:def456..."]
/// ```
///
/// Each entry is a public key in the format `algorithm:hex_bytes`.
/// Currently only `ed25519` is supported.
///
/// # Returns
///
/// Number of authorities set (0 on parse error).
///
/// # Safety
///
/// Caller must ensure `authorities_ptr` points to valid UTF-8 memory.
#[unsafe(no_mangle)]
pub extern "C" fn set_trusted_authorities(
    authorities_ptr: *const u8,
    authorities_len: usize,
) -> usize {
    if authorities_ptr.is_null() || authorities_len == 0 {
        crate::protocol::clear_trusted_authorities();
        return 0;
    }

    // SAFETY: Caller guarantees valid pointer and length
    let json_bytes = unsafe { std::slice::from_raw_parts(authorities_ptr, authorities_len) };
    let Ok(json_str) = std::str::from_utf8(json_bytes) else {
        return 0;
    };

    // Parse JSON array of public key strings
    let keys: Vec<String> = match serde_json::from_str(json_str) {
        Ok(k) => k,
        Err(_) => return 0,
    };

    // Parse each public key
    let mut authorities = Vec::with_capacity(keys.len());
    for key_str in &keys {
        match amla_protocol::PublicKey::from_hex(key_str) {
            Ok(pk) => authorities.push(pk),
            Err(_) => return 0,
        }
    }

    let count = authorities.len();
    crate::protocol::set_trusted_authorities(authorities);
    count
}

/// Clear all trusted authorities.
///
/// After calling this, `runtime_new` will fail until new authorities are set.
#[unsafe(no_mangle)]
pub extern "C" fn clear_trusted_authorities() {
    crate::protocol::clear_trusted_authorities();
}

// =============================================================================
// Runtime Management
// =============================================================================

/// Create a runtime from PCA blob.
///
/// Before calling this function, you must configure trusted authorities
/// using `set_trusted_authorities`. If no trusted authorities are configured,
/// this function will return 0 (error).
///
/// # Arguments
///
/// * `pca_ptr` - Pointer to PCA CBOR bytes
/// * `pca_len` - Length of PCA in bytes
///
/// # Returns
///
/// Runtime ID (> 0 on success, 0 on error/invalid PCA/no trusted authorities).
///
/// # Safety
///
/// Caller must ensure `pca_ptr` points to valid memory of at least `pca_len` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn runtime_new(pca_ptr: *const u8, pca_len: usize) -> u64 {
    clear_last_error();

    if pca_ptr.is_null() || pca_len == 0 {
        set_last_error("pca_ptr is null or pca_len is 0");
        return 0;
    }

    // SAFETY: Caller guarantees valid pointer and length
    let pca_bytes = unsafe { std::slice::from_raw_parts(pca_ptr, pca_len) };

    match Runtime::from_pca_bytes(pca_bytes, wasm_time_source(), wasm_random_source()) {
        Ok(rt) => runtime::register_runtime(rt).raw(),
        Err(e) => {
            set_last_error(&format!("{e}"));
            0
        }
    }
}

/// Create a runtime from PCA blob with MCP tool definitions.
///
/// This is the preferred method when you have tool definitions available,
/// as it generates richer stubs with parameter documentation.
///
/// # Arguments
///
/// * `pca_ptr` - Pointer to PCA CBOR bytes
/// * `pca_len` - Length of PCA in bytes
/// * `tools_ptr` - Pointer to JSON array of MCP tool definitions (UTF-8)
/// * `tools_len` - Length of tools JSON in bytes
///
/// # Tool Definition Format
///
/// The tools JSON should be an array of MCP-compatible tool definitions:
///
/// ```json
/// [
///   {
///     "name": "stripe:charge",
///     "description": "Charge a customer's card",
///     "inputSchema": {
///       "type": "object",
///       "properties": {
///         "amount": {"type": "integer", "description": "Amount in cents"},
///         "currency": {"type": "string", "enum": ["USD", "EUR", "GBP"]}
///       },
///       "required": ["amount", "currency"]
///     }
///   }
/// ]
/// ```
///
/// # Returns
///
/// Runtime ID (> 0 on success, 0 on error/invalid PCA or tools).
///
/// # Safety
///
/// Caller must ensure:
/// - `pca_ptr` points to valid memory of at least `pca_len` bytes
/// - `tools_ptr` points to valid UTF-8 memory of at least `tools_len` bytes
#[unsafe(no_mangle)]
pub extern "C" fn runtime_new_with_tools(
    pca_ptr: *const u8,
    pca_len: usize,
    tools_ptr: *const u8,
    tools_len: usize,
) -> u64 {
    clear_last_error();

    if pca_ptr.is_null() || pca_len == 0 {
        set_last_error("pca_ptr is null or pca_len is 0");
        return 0;
    }

    // SAFETY: Caller guarantees valid pointers and lengths
    let pca_bytes = unsafe { std::slice::from_raw_parts(pca_ptr, pca_len) };

    // Tools JSON can be empty (use empty array "[]")
    let tools_json = if tools_ptr.is_null() || tools_len == 0 {
        "[]"
    } else {
        // SAFETY: `(tools_ptr, tools_len)` was validated against the wasm guest's linear memory by the caller.
        let tools_bytes = unsafe { std::slice::from_raw_parts(tools_ptr, tools_len) };
        if let Ok(s) = std::str::from_utf8(tools_bytes) {
            s
        } else {
            set_last_error("tools_json is not valid UTF-8");
            return 0;
        }
    };

    match Runtime::from_pca_bytes_with_tools(
        pca_bytes,
        tools_json,
        wasm_time_source(),
        wasm_random_source(),
    ) {
        Ok(rt) => runtime::register_runtime(rt).raw(),
        Err(e) => {
            set_last_error(&format!("{e}"));
            0
        }
    }
}

/// Destroy a runtime and all its commands.
///
/// # Arguments
///
/// * `runtime_id` - Runtime ID from `runtime_new`
#[unsafe(no_mangle)]
pub extern "C" fn runtime_destroy(runtime_id: u64) {
    if runtime_id == 0 {
        return;
    }
    let _ = runtime::remove_runtime(RuntimeId::new(runtime_id));
}

/// Get the number of active runtimes.
#[unsafe(no_mangle)]
pub extern "C" fn runtime_count() -> u64 {
    runtime::runtime_count() as u64
}

// =============================================================================
// Command Management
// =============================================================================

/// Create a command instance in a runtime.
///
/// # Arguments
///
/// * `runtime_id` - Runtime ID from `runtime_new`
/// * `cmd_ptr` - Pointer to command string (UTF-8)
/// * `cmd_len` - Length of command string in bytes
///
/// # Returns
///
/// Command handle (> 0 on success, 0 on error).
///
/// # Safety
///
/// Caller must ensure `cmd_ptr` points to valid memory of at least `cmd_len` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn cmd_create(runtime_id: u64, cmd_ptr: *const u8, cmd_len: usize) -> u64 {
    if runtime_id == 0 || cmd_ptr.is_null() || cmd_len == 0 {
        return 0;
    }

    // SAFETY: Caller guarantees valid pointer and length
    let cmd_bytes = unsafe { std::slice::from_raw_parts(cmd_ptr, cmd_len) };
    let Ok(cmd_str) = std::str::from_utf8(cmd_bytes) else {
        return 0;
    };

    runtime::with_runtime_mut(RuntimeId::new(runtime_id), |rt| {
        rt.create_command(cmd_str).raw()
    })
    .unwrap_or(0)
}

/// Delete a command instance.
///
/// # Arguments
///
/// * `runtime_id` - Runtime ID
/// * `handle` - Command handle from `cmd_create`
#[unsafe(no_mangle)]
pub extern "C" fn cmd_delete(runtime_id: u64, handle: u64) {
    if runtime_id == 0 || handle == 0 {
        return;
    }

    let _ = runtime::with_runtime_mut(RuntimeId::new(runtime_id), |rt| {
        rt.delete_command(CommandHandle::new(handle));
    });
}

/// Cancel a running command.
///
/// This terminates the command and all its pending host operations.
/// Cancelling a command that has already completed, been cancelled, or
/// doesn't exist is a no-op.
///
/// # Arguments
///
/// * `runtime_id` - Runtime ID
/// * `handle` - Command handle
/// * `out_ptr` - Pointer to output buffer for JSON response
/// * `out_len` - Length of output buffer in bytes
///
/// # Returns
///
/// Number of bytes written to output buffer.
/// The output is a JSON array of pending host ops (just `CommandExit` with code -1).
/// Returns 0 if runtime/command doesn't exist, was already done, or buffer is invalid.
///
/// # Safety
///
/// Caller must ensure `out_ptr` points to writable memory of at least `out_len` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn cmd_cancel(
    runtime_id: u64,
    handle: u64,
    out_ptr: *mut u8,
    out_len: usize,
) -> usize {
    if runtime_id == 0 || handle == 0 || out_ptr.is_null() || out_len == 0 {
        return 0;
    }

    // Cancel the command and get pending host ops
    let Some(pending_ops) =
        runtime::cancel_command(RuntimeId::new(runtime_id), CommandHandle::new(handle))
    else {
        return 0;
    };

    // Serialize pending ops to JSON
    let Ok(json) = serde_json::to_string(&pending_ops) else {
        return 0;
    };

    let bytes = json.as_bytes();
    let n = bytes.len().min(out_len);

    // SAFETY: Caller guarantees valid pointer and length
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, n);
    }

    n
}

// =============================================================================
// Runtime Execution
// =============================================================================

/// Step a runtime's commands and return pending host ops.
///
/// # Arguments
///
/// * `runtime_id` - Runtime ID from `runtime_new`
/// * `out_ptr` - Pointer to output buffer
/// * `out_len` - Length of output buffer in bytes
///
/// # Returns
///
/// Number of bytes written to output buffer.
/// The output is a JSON `StepResponse` containing pending host ops and status.
/// Returns 0 if runtime doesn't exist or buffer is invalid.
///
/// # Panic Recovery (requires `panic-recovery` feature)
///
/// If the `panic-recovery` feature is enabled and a panic occurs during stepping
/// (e.g., from a buggy command), the runtime is destroyed and a `StepResponse`
/// with `status: "panic"` is returned. Other runtimes are unaffected.
///
/// Without the feature, panics will abort the entire WASM instance.
///
/// # Safety
///
/// Caller must ensure:
/// - `out_ptr` points to writable memory of at least `out_len` bytes
#[cfg(feature = "panic-recovery")]
#[unsafe(no_mangle)]
pub extern "C" fn runtime_step(runtime_id: u64, out_ptr: *mut u8, out_len: usize) -> usize {
    if runtime_id == 0 || out_ptr.is_null() || out_len == 0 {
        return 0;
    }

    let rt_id = RuntimeId::new(runtime_id);

    // Wrap the step in catch_unwind to recover from panics
    let result = catch_unwind(AssertUnwindSafe(|| {
        runtime_step_inner(rt_id, out_ptr, out_len)
    }));

    match result {
        Ok(n) => n,
        Err(payload) => {
            let message = panic_message(&payload);
            write_panic_response(rt_id, &message, out_ptr, out_len)
        }
    }
}

/// Step a runtime (without panic recovery).
///
/// # Safety
///
/// Caller must ensure:
/// - `out_ptr` points to writable memory of at least `out_len` bytes
#[cfg(not(feature = "panic-recovery"))]
#[unsafe(no_mangle)]
pub extern "C" fn runtime_step(runtime_id: u64, out_ptr: *mut u8, out_len: usize) -> usize {
    if runtime_id == 0 || out_ptr.is_null() || out_len == 0 {
        return 0;
    }

    runtime_step_inner(RuntimeId::new(runtime_id), out_ptr, out_len)
}

/// Inner implementation of `runtime_step` (called within `catch_unwind`).
fn runtime_step_inner(runtime_id: RuntimeId, out_ptr: *mut u8, out_len: usize) -> usize {
    // Step the specific runtime
    let Some(response) = runtime::step_runtime(runtime_id) else {
        return 0;
    };

    // Serialize response
    let Ok(json) = serde_json::to_string(&response) else {
        return 0;
    };

    let bytes = json.as_bytes();
    let n = bytes.len().min(out_len);

    // SAFETY: Caller guarantees valid pointer and length
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, n);
    }

    n
}

/// Submit host operation results.
///
/// Host operations include a runtime ID field so they can be routed
/// to the appropriate runtime. After submitting results, call `runtime_step()`
/// again to continue command execution.
///
/// # Arguments
///
/// * `results_ptr` - Pointer to JSON array of [`HostOpResult`]
/// * `results_len` - Length of results in bytes
///
/// # Returns
///
/// Number of results successfully processed, or 0 on parse error.
///
/// # Safety
///
/// Caller must ensure `results_ptr` points to valid memory of at least `results_len` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn submit(results_ptr: *const u8, results_len: usize) -> usize {
    if results_ptr.is_null() || results_len == 0 {
        return 0;
    }

    // SAFETY: Caller guarantees valid pointer and length
    let results_bytes = unsafe { std::slice::from_raw_parts(results_ptr, results_len) };

    let Ok(results): Result<Vec<HostOpResult>, _> = serde_json::from_slice(results_bytes) else {
        return 0;
    };

    runtime::submit_all(&results)
}

// =============================================================================
// Key Management
// =============================================================================

use crate::keys;

/// Generate a new Ed25519 keypair from host-provided seed and store it in the registry.
///
/// This is the deterministic key generation function for WASM. The host MUST
/// provide exactly 32 bytes of cryptographically random data as the seed.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8)
/// * `key_id_len` - Length of key ID in bytes
/// * `seed_ptr` - Pointer to random seed data
/// * `seed_len` - Length of seed (must be exactly 32 bytes for Ed25519)
/// * `out_ptr` - Pointer to output buffer for public key hex
/// * `out_len` - Length of output buffer in bytes
///
/// # Returns
///
/// Number of bytes written to output buffer (public key hex),
/// or 0 on error (key already exists, invalid UTF-8, buffer too small, wrong seed length).
///
/// # Safety
///
/// Caller must ensure:
/// - `key_id_ptr` points to valid memory of at least `key_id_len` bytes
/// - `seed_ptr` points to valid memory of at least `seed_len` bytes
/// - `out_ptr` points to writable memory of at least `out_len` bytes
#[unsafe(no_mangle)]
pub extern "C" fn key_generate(
    key_id_ptr: *const u8,
    key_id_len: usize,
    seed_ptr: *const u8,
    seed_len: usize,
    out_ptr: *mut u8,
    out_len: usize,
) -> usize {
    // Validate all parameters upfront
    if key_id_ptr.is_null()
        || key_id_len == 0
        || seed_ptr.is_null()
        || out_ptr.is_null()
        || out_len == 0
    {
        return 0;
    }

    // Seed must be exactly 32 bytes for Ed25519
    if seed_len != 32 {
        return 0;
    }

    // SAFETY: Caller guarantees valid pointers and lengths
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        return 0;
    };

    // SAFETY: Caller guarantees seed_ptr points to at least seed_len (32) bytes
    let seed_slice = unsafe { std::slice::from_raw_parts(seed_ptr, seed_len) };
    let mut seed = [0u8; 32];
    seed.copy_from_slice(seed_slice);

    match keys::key_generate_from_seed(key_id, &seed) {
        Ok(public_hex) => {
            let bytes = public_hex.as_bytes();
            let n = bytes.len().min(out_len);
            // SAFETY: Caller guarantees valid output pointer
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, n);
            }
            n
        }
        Err(_) => 0,
    }
}

/// Import an existing keypair from a private key.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8)
/// * `key_id_len` - Length of key ID in bytes
/// * `private_key_ptr` - Pointer to private key hex string (UTF-8, e.g., "ed25519:...")
/// * `private_key_len` - Length of private key string in bytes
/// * `out_ptr` - Pointer to output buffer for public key hex
/// * `out_len` - Length of output buffer in bytes
///
/// # Returns
///
/// Number of bytes written to output buffer (public key hex),
/// or 0 on error.
///
/// # Safety
///
/// Caller must ensure all pointers point to valid memory of the specified lengths.
#[unsafe(no_mangle)]
pub extern "C" fn key_set(
    key_id_ptr: *const u8,
    key_id_len: usize,
    private_key_ptr: *const u8,
    private_key_len: usize,
    out_ptr: *mut u8,
    out_len: usize,
) -> usize {
    if key_id_ptr.is_null()
        || key_id_len == 0
        || private_key_ptr.is_null()
        || private_key_len == 0
        || out_ptr.is_null()
        || out_len == 0
    {
        return 0;
    }

    // SAFETY: Caller guarantees valid pointers and lengths
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    // SAFETY: `(private_key_ptr, private_key_len)` was validated against the wasm guest's linear memory by the caller.
    let private_key_bytes = unsafe { std::slice::from_raw_parts(private_key_ptr, private_key_len) };

    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        return 0;
    };
    let Ok(private_key_hex) = std::str::from_utf8(private_key_bytes) else {
        return 0;
    };

    match keys::key_set(key_id, private_key_hex) {
        Ok(public_hex) => {
            let bytes = public_hex.as_bytes();
            let n = bytes.len().min(out_len);
            // SAFETY: `(out_ptr, out_len)` was validated against the wasm guest's linear memory by the caller; `n <= out_len`.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, n);
            }
            n
        }
        Err(_) => 0,
    }
}

/// Get the public key for a stored keypair.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8)
/// * `key_id_len` - Length of key ID in bytes
/// * `out_ptr` - Pointer to output buffer for public key hex
/// * `out_len` - Length of output buffer in bytes
///
/// # Returns
///
/// Number of bytes written to output buffer,
/// or 0 if key not found or error.
///
/// # Safety
///
/// Caller must ensure pointers point to valid memory.
#[unsafe(no_mangle)]
pub extern "C" fn key_get_public(
    key_id_ptr: *const u8,
    key_id_len: usize,
    out_ptr: *mut u8,
    out_len: usize,
) -> usize {
    if key_id_ptr.is_null() || key_id_len == 0 || out_ptr.is_null() || out_len == 0 {
        return 0;
    }

    // SAFETY: `(key_id_ptr, key_id_len)` was validated against the wasm guest's linear memory by the caller.
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        return 0;
    };

    match keys::key_get_public(key_id) {
        Ok(public_hex) => {
            let bytes = public_hex.as_bytes();
            let n = bytes.len().min(out_len);
            // SAFETY: `(out_ptr, out_len)` was validated against the wasm guest's linear memory by the caller; `n <= out_len`.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, n);
            }
            n
        }
        Err(_) => 0,
    }
}

/// Sign data with a stored keypair.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8)
/// * `key_id_len` - Length of key ID in bytes
/// * `data_ptr` - Pointer to data to sign
/// * `data_len` - Length of data in bytes
/// * `out_ptr` - Pointer to output buffer for signature hex
/// * `out_len` - Length of output buffer in bytes
///
/// # Returns
///
/// Number of bytes written to output buffer (signature hex),
/// or 0 if key not found or error.
///
/// # Safety
///
/// Caller must ensure pointers point to valid memory.
#[unsafe(no_mangle)]
pub extern "C" fn key_sign(
    key_id_ptr: *const u8,
    key_id_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    out_ptr: *mut u8,
    out_len: usize,
) -> usize {
    if key_id_ptr.is_null() || key_id_len == 0 || out_ptr.is_null() || out_len == 0 {
        return 0;
    }

    // SAFETY: `(key_id_ptr, key_id_len)` was validated against the wasm guest's linear memory by the caller.
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        return 0;
    };

    // Data can be empty (data_ptr null with data_len 0)
    let data = if data_ptr.is_null() || data_len == 0 {
        &[]
    } else {
        // SAFETY: `(data_ptr, data_len)` was validated against the wasm guest's linear memory by the caller.
        unsafe { std::slice::from_raw_parts(data_ptr, data_len) }
    };

    match keys::key_sign(key_id, data) {
        Ok(sig_hex) => {
            let bytes = sig_hex.as_bytes();
            let n = bytes.len().min(out_len);
            // SAFETY: `(out_ptr, out_len)` was validated against the wasm guest's linear memory by the caller; `n <= out_len`.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ptr, n);
            }
            n
        }
        Err(_) => 0,
    }
}

/// Verify a signature against a public key.
///
/// This does NOT require the key to be in the registry.
///
/// # Arguments
///
/// * `public_key_ptr` - Pointer to public key hex string (UTF-8)
/// * `public_key_len` - Length of public key string in bytes
/// * `data_ptr` - Pointer to original data that was signed
/// * `data_len` - Length of data in bytes
/// * `sig_ptr` - Pointer to signature hex string (UTF-8)
/// * `sig_len` - Length of signature string in bytes
///
/// # Returns
///
/// 1 if signature is valid, 0 if invalid or on error.
///
/// # Safety
///
/// Caller must ensure pointers point to valid memory.
#[unsafe(no_mangle)]
pub extern "C" fn key_verify(
    public_key_ptr: *const u8,
    public_key_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    sig_ptr: *const u8,
    sig_len: usize,
) -> i32 {
    if public_key_ptr.is_null() || public_key_len == 0 || sig_ptr.is_null() || sig_len == 0 {
        return 0;
    }

    // SAFETY: `(public_key_ptr, public_key_len)` was validated against the wasm guest's linear memory by the caller.
    let public_key_bytes = unsafe { std::slice::from_raw_parts(public_key_ptr, public_key_len) };
    // SAFETY: `(sig_ptr, sig_len)` was validated against the wasm guest's linear memory by the caller.
    let sig_bytes = unsafe { std::slice::from_raw_parts(sig_ptr, sig_len) };

    let Ok(public_key_hex) = std::str::from_utf8(public_key_bytes) else {
        return 0;
    };
    let Ok(sig_hex) = std::str::from_utf8(sig_bytes) else {
        return 0;
    };

    // Data can be empty
    let data = if data_ptr.is_null() || data_len == 0 {
        &[]
    } else {
        // SAFETY: `(data_ptr, data_len)` was validated against the wasm guest's linear memory by the caller.
        unsafe { std::slice::from_raw_parts(data_ptr, data_len) }
    };

    match keys::key_verify(public_key_hex, data, sig_hex) {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

/// Delete a key from the registry.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8)
/// * `key_id_len` - Length of key ID in bytes
///
/// # Returns
///
/// 1 if a key was deleted, 0 if no key existed with this ID.
///
/// # Safety
///
/// Caller must ensure pointer points to valid memory.
#[unsafe(no_mangle)]
pub extern "C" fn key_delete(key_id_ptr: *const u8, key_id_len: usize) -> i32 {
    if key_id_ptr.is_null() || key_id_len == 0 {
        return 0;
    }

    // SAFETY: `(key_id_ptr, key_id_len)` was validated against the wasm guest's linear memory by the caller.
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        return 0;
    };

    i32::from(keys::key_delete(key_id))
}

/// Check if a key exists in the registry.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8)
/// * `key_id_len` - Length of key ID in bytes
///
/// # Returns
///
/// 1 if key exists, 0 otherwise.
///
/// # Safety
///
/// Caller must ensure pointer points to valid memory.
#[unsafe(no_mangle)]
pub extern "C" fn key_exists(key_id_ptr: *const u8, key_id_len: usize) -> i32 {
    if key_id_ptr.is_null() || key_id_len == 0 {
        return 0;
    }

    // SAFETY: `(key_id_ptr, key_id_len)` was validated against the wasm guest's linear memory by the caller.
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        return 0;
    };

    i32::from(keys::key_exists(key_id))
}

/// Get the number of keys in the registry.
#[unsafe(no_mangle)]
pub extern "C" fn key_count() -> u64 {
    keys::key_count() as u64
}

// =============================================================================
// PCA Creation Export
// =============================================================================

/// Create a signed PCA with the specified capabilities.
///
/// This allows the host to create PCAs without knowing the CBOR format.
/// The PCA is signed by the key specified by `key_id`.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8) for signing
/// * `key_id_len` - Length of key ID in bytes
/// * `caps_ptr` - Pointer to capabilities JSON (UTF-8 array of patterns like `["stripe:*"]`)
/// * `caps_len` - Length of capabilities JSON in bytes
/// * `deadline_ns` - Absolute expiration deadline in nanoseconds (matches WASI clock format)
/// * `out_ptr` - Pointer to output buffer for PCA CBOR bytes
/// * `out_len` - Length of output buffer in bytes
///
/// # Returns
///
/// Number of bytes written to output buffer (PCA CBOR),
/// or 0 on error.
///
/// # Example JSON
///
/// ```json
/// ["stripe:charge", "notion:search", "github:*"]
/// ```
///
/// # Safety
///
/// Caller must ensure pointers point to valid memory of the specified lengths.
#[unsafe(no_mangle)]
pub extern "C" fn pca_create(
    key_id_ptr: *const u8,
    key_id_len: usize,
    caps_ptr: *const u8,
    caps_len: usize,
    deadline_ns: u64,
    out_ptr: *mut u8,
    out_len: usize,
) -> usize {
    use amla_protocol::{CapabilityData, PcaBuilder, Version};

    clear_last_error();

    if key_id_ptr.is_null()
        || key_id_len == 0
        || caps_ptr.is_null()
        || caps_len == 0
        || out_ptr.is_null()
        || out_len == 0
    {
        set_last_error("pca_create: null or zero-length parameter");
        return 0;
    }

    // Get key ID
    // SAFETY: `(key_id_ptr, key_id_len)` was validated against the wasm guest's linear memory by the caller.
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        set_last_error("pca_create: invalid UTF-8 key_id");
        return 0;
    };

    // Get keypair for signing
    let Ok(keypair) = keys::key_get_keypair(key_id) else {
        set_last_error("pca_create: key not found");
        return 0;
    };

    // Parse capabilities JSON
    // SAFETY: `(caps_ptr, caps_len)` was validated against the wasm guest's linear memory by the caller.
    let caps_bytes = unsafe { std::slice::from_raw_parts(caps_ptr, caps_len) };
    let Ok(caps_str) = std::str::from_utf8(caps_bytes) else {
        set_last_error("pca_create: invalid UTF-8 caps");
        return 0;
    };

    // Parse capabilities JSON - supports two formats:
    // 1. Simple: ["stripe:charge", "notion:search"]
    // 2. Extended: [{"tool": "stripe:charge", "constraints": [{"param": "amount", "op": "<=", "value": 10000}]}]
    // Can also mix: ["notion:search", {"tool": "stripe:charge", "constraints": [...]}]
    let cap_values: Vec<serde_json::Value> = match serde_json::from_str(caps_str) {
        Ok(p) => p,
        Err(e) => {
            set_last_error(&format!("pca_create: invalid JSON: {e}"));
            return 0;
        }
    };

    // Convert deadline_ns to chrono DateTime
    #[allow(clippy::cast_possible_wrap)]
    let expires_at = chrono::DateTime::from_timestamp_nanos(deadline_ns as i64);

    let mut builder = PcaBuilder::new()
        .version(Version::new(0, 1))
        .designated_executor(keypair.public_key())
        .expires_at(expires_at);

    // Add capabilities as tool_call patterns
    for (i, cap_value) in cap_values.iter().enumerate() {
        // Create ToolCallCap payload - handle both string and object formats
        let payload = match cap_value {
            serde_json::Value::String(pattern) => {
                // Simple format: just tool name, no constraints
                serde_json::json!({
                    "tool": pattern,
                    "constraints": []
                })
            }
            serde_json::Value::Object(obj) => {
                // Extended format: object with tool and optional constraints
                let tool = obj
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let constraints = obj
                    .get("constraints")
                    .cloned()
                    .unwrap_or(serde_json::json!([]));
                serde_json::json!({
                    "tool": tool,
                    "constraints": constraints
                })
            }
            _ => {
                set_last_error(&format!(
                    "pca_create: capability must be string or object at index {i}"
                ));
                return 0;
            }
        };

        let cap = match CapabilityData::from_json(&format!("cap:{i}"), "tool-call", &payload) {
            Ok(c) => c,
            Err(e) => {
                set_last_error(&format!("pca_create: invalid capability: {e}"));
                return 0;
            }
        };
        builder = builder.add_capability(cap);
    }

    // Sign and serialize
    let pca = match builder.build_and_sign(&keypair) {
        Ok(p) => p,
        Err(e) => {
            set_last_error(&format!("pca_create: build failed: {e}"));
            return 0;
        }
    };

    let cbor = match pca.to_cbor() {
        Ok(c) => c,
        Err(e) => {
            set_last_error(&format!("pca_create: serialize failed: {e}"));
            return 0;
        }
    };

    // Write to output buffer
    let n = cbor.len().min(out_len);
    if n < cbor.len() {
        set_last_error("pca_create: output buffer too small");
        return 0;
    }

    // SAFETY: `(out_ptr, out_len)` was validated against the wasm guest's linear memory by the caller; `n <= out_len`.
    unsafe {
        std::ptr::copy_nonoverlapping(cbor.as_ptr(), out_ptr, n);
    }
    n
}

/// Inspect a PCA and return its fields as JSON.
///
/// This allows the frontend to display PCA contents without understanding CBOR.
/// All PCA parsing happens in Rust.
///
/// # Arguments
///
/// * `pca_ptr` - Pointer to PCA CBOR bytes
/// * `pca_len` - Length of PCA in bytes
/// * `out_ptr` - Pointer to output buffer for JSON
/// * `out_len` - Length of output buffer
///
/// # Returns
///
/// Number of bytes written to output buffer (JSON string),
/// or 0 on error.
///
/// # Output Format
///
/// ```json
/// {
///   "issuer": "ed25519:abc123...",
///   "expires_at": "2024-01-15T12:00:00+00:00",
///   "capabilities": ["stripe:charge", "notion:search"],
///   "signature_valid": true
/// }
/// ```
///
/// # Safety
///
/// Caller must ensure pointers point to valid memory of the specified lengths.
#[unsafe(no_mangle)]
pub extern "C" fn pca_inspect(
    pca_ptr: *const u8,
    pca_len: usize,
    out_ptr: *mut u8,
    out_len: usize,
) -> usize {
    use amla_protocol::Pca;

    clear_last_error();

    if pca_ptr.is_null() || pca_len == 0 || out_ptr.is_null() || out_len == 0 {
        set_last_error("pca_inspect: null or zero-length parameter");
        return 0;
    }

    // SAFETY: `(pca_ptr, pca_len)` was validated against the wasm guest's linear memory by the caller.
    let pca_bytes = unsafe { std::slice::from_raw_parts(pca_ptr, pca_len) };

    // Parse PCA
    let pca = match Pca::from_cbor(pca_bytes) {
        Ok(p) => p,
        Err(e) => {
            set_last_error(&format!("pca_inspect: invalid PCA: {e}"));
            return 0;
        }
    };

    // Extract capability details including constraints
    let capabilities: Vec<serde_json::Value> = pca
        .capabilities()
        .iter()
        .filter_map(|cap| {
            // Try to extract tool pattern and constraints from capability data
            if cap.capability_type() == "tool-call" {
                // Parse the CBOR payload to get tool and constraints
                if let Ok(payload) = cap.to_json() {
                    let tool = payload
                        .get("tool")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");
                    let constraints = payload.get("constraints").cloned();
                    return Some(serde_json::json!({
                        "tool": tool,
                        "constraints": constraints.unwrap_or(serde_json::json!([]))
                    }));
                }
            }
            None
        })
        .collect();

    // Also provide legacy flat list of tool names for backwards compatibility
    let tool_names: Vec<String> = capabilities
        .iter()
        .filter_map(|c| {
            c.get("tool")
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        })
        .collect();

    // Check signature validity
    let signature_valid = pca.try_verify_signature().is_ok();

    // Build response JSON with both detailed and legacy formats
    let response = serde_json::json!({
        "issuer": pca.issuer().to_hex(),
        "expires_at": pca.expires_at().to_rfc3339(),
        "capabilities": tool_names,  // Legacy: just tool names
        "capabilities_detailed": capabilities,  // New: includes constraints
        "signature_valid": signature_valid,
    });

    let json_str = response.to_string();
    let json_bytes = json_str.as_bytes();

    if json_bytes.len() > out_len {
        set_last_error("pca_inspect: output buffer too small");
        return 0;
    }

    // SAFETY: `(out_ptr, out_len)` was validated against the wasm guest's linear memory by the caller; `json_bytes.len() <= out_len`.
    unsafe {
        std::ptr::copy_nonoverlapping(json_bytes.as_ptr(), out_ptr, json_bytes.len());
    }
    json_bytes.len()
}

/// Create a runtime from PCA blob, validating that the key matches the designated executor.
///
/// This is the secure version of `runtime_new` that enforces executor binding.
/// The key referenced by `key_id` must be registered via `key_generate` or `key_set`
/// before calling this function.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8)
/// * `key_id_len` - Length of key ID in bytes
/// * `pca_ptr` - Pointer to PCA CBOR bytes
/// * `pca_len` - Length of PCA in bytes
///
/// # Returns
///
/// Runtime ID (> 0 on success), or 0 on error:
/// - Key not found
/// - Invalid PCA
/// - PCA signature verification failed
/// - PCA expired
/// - Executor mismatch (PCA designates a different executor)
///
/// # Safety
///
/// Caller must ensure pointers point to valid memory of the specified lengths.
#[unsafe(no_mangle)]
pub extern "C" fn runtime_new_with_key(
    key_id_ptr: *const u8,
    key_id_len: usize,
    pca_ptr: *const u8,
    pca_len: usize,
) -> u64 {
    if key_id_ptr.is_null() || key_id_len == 0 || pca_ptr.is_null() || pca_len == 0 {
        return 0;
    }

    // Get key ID
    // SAFETY: `(key_id_ptr, key_id_len)` was validated against the wasm guest's linear memory by the caller.
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        return 0;
    };

    // Get executor public key from registry
    let Ok(executor_key) = keys::key_get_public_key(key_id) else {
        return 0;
    };

    // Parse PCA
    // SAFETY: `(pca_ptr, pca_len)` was validated against the wasm guest's linear memory by the caller.
    let pca_bytes = unsafe { std::slice::from_raw_parts(pca_ptr, pca_len) };
    let Ok(pca) = amla_protocol::Pca::from_cbor(pca_bytes) else {
        return 0;
    };

    // Validate PCA with executor binding
    // Use time_source instead of chrono::Utc::now() (works on WASM)
    let time_source = wasm_time_source();
    let current_nanos = time_source(0, amla_scheduler::ClockType::Realtime);
    #[allow(clippy::cast_possible_wrap)]
    let current_time = chrono::DateTime::from_timestamp_nanos(current_nanos as i64);
    if crate::protocol::validate_pca(&pca, Some(&executor_key), current_time).is_err() {
        return 0;
    }

    // Create runtime
    match Runtime::from_pca(pca, time_source, wasm_random_source()) {
        Ok(rt) => runtime::register_runtime(rt).raw(),
        Err(_) => 0,
    }
}

/// Create a runtime from PCA with tools, validating executor binding.
///
/// Combines `runtime_new_with_tools` with executor validation.
///
/// # Arguments
///
/// * `key_id_ptr` - Pointer to key ID string (UTF-8)
/// * `key_id_len` - Length of key ID in bytes
/// * `pca_ptr` - Pointer to PCA CBOR bytes
/// * `pca_len` - Length of PCA in bytes
/// * `tools_ptr` - Pointer to JSON array of MCP tool definitions (UTF-8)
/// * `tools_len` - Length of tools JSON in bytes
///
/// # Returns
///
/// Runtime ID (> 0 on success), or 0 on error.
///
/// # Safety
///
/// Caller must ensure pointers point to valid memory.
#[unsafe(no_mangle)]
pub extern "C" fn runtime_new_with_key_and_tools(
    key_id_ptr: *const u8,
    key_id_len: usize,
    pca_ptr: *const u8,
    pca_len: usize,
    tools_ptr: *const u8,
    tools_len: usize,
) -> u64 {
    if key_id_ptr.is_null() || key_id_len == 0 || pca_ptr.is_null() || pca_len == 0 {
        return 0;
    }

    // Get key ID
    // SAFETY: `(key_id_ptr, key_id_len)` was validated against the wasm guest's linear memory by the caller.
    let key_id_bytes = unsafe { std::slice::from_raw_parts(key_id_ptr, key_id_len) };
    let Ok(key_id) = std::str::from_utf8(key_id_bytes) else {
        return 0;
    };

    // Get executor public key from registry
    let Ok(executor_key) = keys::key_get_public_key(key_id) else {
        return 0;
    };

    // Parse PCA
    // SAFETY: `(pca_ptr, pca_len)` was validated against the wasm guest's linear memory by the caller.
    let pca_bytes = unsafe { std::slice::from_raw_parts(pca_ptr, pca_len) };
    let Ok(pca) = amla_protocol::Pca::from_cbor(pca_bytes) else {
        return 0;
    };

    // Validate PCA with executor binding
    // Use time_source instead of chrono::Utc::now() (works on WASM)
    let time_source = wasm_time_source();
    let current_nanos = time_source(0, amla_scheduler::ClockType::Realtime);
    #[allow(clippy::cast_possible_wrap)]
    let current_time = chrono::DateTime::from_timestamp_nanos(current_nanos as i64);
    if crate::protocol::validate_pca(&pca, Some(&executor_key), current_time).is_err() {
        return 0;
    }

    // Parse tools JSON
    let tools_json = if tools_ptr.is_null() || tools_len == 0 {
        "[]"
    } else {
        // SAFETY: `(tools_ptr, tools_len)` was validated against the wasm guest's linear memory by the caller.
        let tools_bytes = unsafe { std::slice::from_raw_parts(tools_ptr, tools_len) };
        match std::str::from_utf8(tools_bytes) {
            Ok(s) => s,
            Err(_) => return 0,
        }
    };

    let Ok(tools) = crate::mcp::load_mcp_tools(tools_json) else {
        return 0;
    };

    // Create runtime with tools (reuse time_source from validation)
    match Runtime::from_pca_with_tools(pca, &tools, time_source, wasm_random_source()) {
        Ok(rt) => runtime::register_runtime(rt).raw(),
        Err(_) => 0,
    }
}

// =============================================================================
// Audit Buffer Exports
// =============================================================================

/// Get the number of audit log bytes available for reading.
///
/// The host should call this to check if there are logs to drain.
/// Returns 0 if no logs are available or if the runtime doesn't exist.
///
/// # Arguments
/// * `runtime_id` - Runtime ID from `runtime_new`
#[unsafe(no_mangle)]
pub extern "C" fn audit_available(runtime_id: u64) -> usize {
    if runtime_id == 0 {
        return 0;
    }

    runtime::with_runtime(RuntimeId::new(runtime_id), |rt| {
        rt.audit_buffer().available()
    })
    .unwrap_or(0)
}

/// Drain audit logs into host buffer.
///
/// Returns the number of bytes written (JSONL format, newline-separated entries).
/// The host should call this periodically or after each step to collect logs.
/// Logs are removed from the ring buffer after draining.
///
/// # Arguments
/// * `runtime_id` - Runtime ID from `runtime_new`
/// * `out_ptr` - Pointer to output buffer
/// * `out_len` - Length of output buffer
///
/// # Safety
/// Caller must ensure `out_ptr` points to writable memory of at least `out_len` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn audit_drain(runtime_id: u64, out_ptr: *mut u8, out_len: usize) -> usize {
    if runtime_id == 0 || out_ptr.is_null() || out_len == 0 {
        return 0;
    }

    runtime::with_runtime_mut(RuntimeId::new(runtime_id), |rt| {
        // SAFETY: `(out_ptr, out_len)` was validated against the wasm guest's linear memory by the caller; the slice lifetime is bounded by this closure.
        let out = unsafe { std::slice::from_raw_parts_mut(out_ptr, out_len) };
        rt.audit_buffer_mut().drain(out)
    })
    .unwrap_or(0)
}

/// Configure audit logging.
///
/// # Arguments
/// * `runtime_id` - Runtime ID from `runtime_new`
/// * `config_ptr` - Pointer to JSON config
/// * `config_len` - Length of config
///
/// # Config Format
/// ```json
/// {
///   "level": "info",        // "debug", "info", "warn", "error"
///   "preview_chars": 128    // Max chars for text preview (0-1024)
/// }
/// ```
///
/// # Returns
/// 0 on success, -1 on error (invalid config or runtime).
///
/// # Safety
/// Caller must ensure `config_ptr` points to valid UTF-8 memory of at least `config_len` bytes.
#[unsafe(no_mangle)]
pub extern "C" fn audit_configure(
    runtime_id: u64,
    config_ptr: *const u8,
    config_len: usize,
) -> i32 {
    if runtime_id == 0 {
        return -1;
    }

    // Parse config if provided
    let config_json = if config_ptr.is_null() || config_len == 0 {
        "{}"
    } else {
        // SAFETY: `(config_ptr, config_len)` was validated against the wasm guest's linear memory by the caller.
        let config_bytes = unsafe { std::slice::from_raw_parts(config_ptr, config_len) };
        match std::str::from_utf8(config_bytes) {
            Ok(s) => s,
            Err(_) => return -1,
        }
    };

    // Parse JSON
    let Ok(config): Result<serde_json::Value, _> = serde_json::from_str(config_json) else {
        return -1;
    };

    runtime::with_runtime_mut(RuntimeId::new(runtime_id), |rt| {
        let mut buffer = rt.audit_buffer_mut();

        // Apply level if specified
        if let Some(level_str) = config.get("level").and_then(|v| v.as_str()) {
            buffer.level = match level_str {
                "debug" => amla_audit::LogLevel::Debug,
                "info" => amla_audit::LogLevel::Info,
                "warn" => amla_audit::LogLevel::Warn,
                "error" => amla_audit::LogLevel::Error,
                _ => return -1,
            };
        }

        // Apply preview_chars if specified
        if let Some(preview) = config
            .get("preview_chars")
            .and_then(serde_json::Value::as_u64)
        {
            // Safe: we clamp to 1024, which fits in usize on all platforms
            #[allow(clippy::cast_possible_truncation)]
            let chars = (preview as usize).min(1024);
            buffer.preview_chars = chars;
        }

        0
    })
    .unwrap_or(-1)
}

// =============================================================================
// Safe Rust API (for testing and native use)
// =============================================================================

/// Safe wrapper for the WASM interface.
///
/// This provides a Rust-native API for testing without raw pointers.
#[cfg(test)]
pub mod safe {
    use super::*;
    use crate::types::{CommandHandle, RuntimeId, StepResponse};

    /// Create a runtime from PCA bytes.
    pub fn runtime_new_safe(pca_bytes: &[u8]) -> RuntimeId {
        match Runtime::from_pca_bytes(pca_bytes, wasm_time_source(), wasm_random_source()) {
            Ok(rt) => runtime::register_runtime(rt),
            Err(_) => RuntimeId::new(0),
        }
    }

    /// Destroy a runtime.
    pub fn runtime_destroy_safe(runtime_id: RuntimeId) {
        let _ = runtime::remove_runtime(runtime_id);
    }

    /// Create a command (safe version).
    pub fn cmd_create_safe(runtime_id: RuntimeId, cmd: &str) -> CommandHandle {
        runtime::with_runtime_mut(runtime_id, |rt| rt.create_command(cmd))
            .unwrap_or(CommandHandle::new(0))
    }

    /// Delete a command (safe version).
    pub fn cmd_delete_safe(runtime_id: RuntimeId, handle: CommandHandle) {
        let _ = runtime::with_runtime_mut(runtime_id, |rt| {
            rt.delete_command(handle);
        });
    }

    /// Cancel a command (safe version).
    ///
    /// Returns the pending host ops (just `CommandExit` with code -1).
    /// Returns `None` if runtime doesn't exist.
    /// Returns empty vec if command doesn't exist or was already done/cancelled.
    pub fn cmd_cancel_safe(
        runtime_id: RuntimeId,
        handle: CommandHandle,
    ) -> Option<crate::types::HostOpsVec> {
        runtime::cancel_command(runtime_id, handle)
    }

    /// Step a runtime (safe version).
    ///
    /// Wraps the step in `catch_unwind` to recover from panics.
    /// If the runtime panics, it is destroyed and a `RuntimeStatus::Panic` is returned.
    pub fn step_safe(runtime_id: RuntimeId) -> Option<StepResponse> {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let result = catch_unwind(AssertUnwindSafe(|| runtime::step_runtime(runtime_id)));

        match result {
            Ok(response) => response,
            Err(payload) => {
                // Extract panic message
                let message = if let Some(s) = payload.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };

                // Destroy the panicked runtime
                let _ = runtime::remove_runtime(runtime_id);

                // Return panic response
                Some(StepResponse {
                    host_ops: crate::types::HostOpsVec::new(),
                    status: crate::types::RuntimeStatus::Panic { message },
                })
            }
        }
    }

    /// Submit results (safe version).
    pub fn submit_safe(results: &[HostOpResult]) -> usize {
        runtime::submit_all(results)
    }

    /// RAII guard that automatically destroys a runtime when dropped.
    ///
    /// Use this in tests to ensure runtimes are properly cleaned up,
    /// even if the test panics.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let guard = RuntimeGuard::new(&pca_bytes);
    /// let rt_id = guard.id();
    /// // ... use rt_id ...
    /// // Runtime is automatically destroyed when guard goes out of scope
    /// ```
    #[cfg(test)]
    pub struct RuntimeGuard {
        id: RuntimeId,
    }

    #[cfg(test)]
    impl RuntimeGuard {
        /// Create a new runtime and return a guard that will destroy it on drop.
        pub fn new(pca_bytes: &[u8]) -> Self {
            Self {
                id: runtime_new_safe(pca_bytes),
            }
        }

        /// Get the runtime ID.
        pub fn id(&self) -> RuntimeId {
            self.id
        }

        /// Get the raw runtime ID as u64.
        pub fn raw_id(&self) -> u64 {
            self.id.raw()
        }
    }

    #[cfg(test)]
    impl Drop for RuntimeGuard {
        fn drop(&mut self) {
            runtime_destroy_safe(self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::safe::*;
    use super::*;
    use crate::host_ops::{HostOpRequest, HostOpResponse, HostOpResult};
    use amla_capabilities::ToolCallCap;
    use amla_protocol::{Algorithm, CapabilityData, KeyPair, PcaBuilder, Version};
    use chrono::{Duration, Utc};
    use std::collections::HashMap;

    /// Collected output for a single command.
    #[derive(Debug, Default)]
    struct CollectedOutput {
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
    }

    /// Step a single runtime until all its commands complete.
    fn run_to_completion(runtime_id: RuntimeId) -> HashMap<Option<CommandHandle>, CollectedOutput> {
        // Higher limit to handle concurrent commands with timer operations
        const MAX_ITERATIONS: usize = 500;
        let mut collected: HashMap<Option<CommandHandle>, CollectedOutput> = HashMap::new();
        let mut iterations = 0;

        loop {
            iterations += 1;
            if iterations > MAX_ITERATIONS {
                eprintln!("run_to_completion: exceeded max iterations");
                break;
            }

            let Some(response) = step_safe(runtime_id) else {
                break;
            };

            // Process host ops
            // Note: command is in PendingHostOp, not HostOpRequest
            for op in &response.host_ops {
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

            // Respond appropriately to each host op type
            let results: Vec<HostOpResult> = response
                .host_ops
                .iter()
                .map(|op| {
                    let result = match &op.request {
                        HostOpRequest::WakeAt { deadline_nanos } => {
                            // Simulate immediate wake with the deadline as current time
                            HostOpResponse::woke_at(*deadline_nanos)
                        }
                        HostOpRequest::ReadStdin { .. } => {
                            // Respond with EOF immediately for tests that don't need stdin
                            HostOpResponse::StdinData {
                                data: vec![],
                                eof: true,
                            }
                        }
                        _ => HostOpResponse::OutputAck,
                    };
                    HostOpResult {
                        id: op.id,
                        runtime_id: op.runtime_id,
                        result,
                    }
                })
                .collect();
            submit_safe(&results);

            if response.all_done() {
                break;
            }
        }

        collected
    }

    /// Get all registered runtime IDs.
    fn all_runtime_ids() -> Vec<RuntimeId> {
        runtime::runtime_ids()
    }

    /// Step all registered runtimes until all complete (round-robin).
    fn run_all_registered_to_completion()
    -> HashMap<RuntimeId, HashMap<Option<CommandHandle>, CollectedOutput>> {
        let runtime_ids = all_runtime_ids();
        run_all_to_completion(&runtime_ids)
    }

    /// Step multiple runtimes until all complete (round-robin).
    fn run_all_to_completion(
        runtime_ids: &[RuntimeId],
    ) -> HashMap<RuntimeId, HashMap<Option<CommandHandle>, CollectedOutput>> {
        const MAX_ITERATIONS: usize = 100;
        let mut collected: HashMap<RuntimeId, HashMap<Option<CommandHandle>, CollectedOutput>> =
            HashMap::new();
        let mut iterations = 0;

        loop {
            iterations += 1;
            if iterations > MAX_ITERATIONS {
                eprintln!("run_all_to_completion: exceeded max iterations");
                break;
            }

            let mut any_running = false;

            for &rt_id in runtime_ids {
                let Some(response) = step_safe(rt_id) else {
                    continue;
                };

                // Process host ops
                // Note: runtime_id and command are in PendingHostOp, not HostOpRequest
                for op in &response.host_ops {
                    match &op.request {
                        HostOpRequest::Output { stream, data, .. } => {
                            let rt_outputs = collected.entry(op.runtime_id).or_default();
                            let entry = rt_outputs.entry(op.command).or_default();
                            let text = String::from_utf8_lossy(data);
                            if *stream == 1 {
                                entry.stdout.push_str(&text);
                            } else {
                                entry.stderr.push_str(&text);
                            }
                        }
                        HostOpRequest::CommandExit { code, .. } => {
                            let rt_outputs = collected.entry(op.runtime_id).or_default();
                            let entry = rt_outputs.entry(op.command).or_default();
                            entry.exit_code = Some(*code);
                        }
                        _ => {}
                    }
                }

                // Respond appropriately to each host op type
                let results: Vec<HostOpResult> = response
                    .host_ops
                    .iter()
                    .map(|op| {
                        let result = match &op.request {
                            HostOpRequest::WakeAt { deadline_nanos } => {
                                // Simulate immediate wake with the deadline as current time
                                HostOpResponse::woke_at(*deadline_nanos)
                            }
                            HostOpRequest::ReadStdin { .. } => {
                                // Respond with EOF immediately for tests that don't need stdin
                                HostOpResponse::StdinData {
                                    data: vec![],
                                    eof: true,
                                }
                            }
                            _ => HostOpResponse::OutputAck,
                        };
                        HostOpResult {
                            id: op.id,
                            runtime_id: op.runtime_id,
                            result,
                        }
                    })
                    .collect();
                submit_safe(&results);

                if !response.all_done() {
                    any_running = true;
                }
            }

            if !any_running {
                break;
            }
        }

        collected
    }

    /// Helper to set up trusted authorities and clean up after test.
    struct TrustedAuthoritiesGuard;

    impl TrustedAuthoritiesGuard {
        fn new(authorities: Vec<amla_protocol::PublicKey>) -> Self {
            crate::protocol::set_trusted_authorities(authorities);
            Self
        }
    }

    impl Drop for TrustedAuthoritiesGuard {
        fn drop(&mut self) {
            crate::protocol::clear_trusted_authorities();
        }
    }

    fn create_test_pca_bytes() -> (Vec<u8>, TrustedAuthoritiesGuard) {
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

        (pca.to_cbor().unwrap(), guard)
    }

    fn setup() -> (RuntimeGuard, TrustedAuthoritiesGuard) {
        let (pca_bytes, guard) = create_test_pca_bytes();
        (RuntimeGuard::new(&pca_bytes), guard)
    }

    #[test]
    fn test_runtime_new_destroy() {
        let initial_count = runtime::runtime_count();

        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt_id = runtime_new_safe(&pca_bytes);

        assert!(rt_id > RuntimeId::new(0));
        assert_eq!(runtime::runtime_count(), initial_count + 1);

        runtime_destroy_safe(rt_id);
        assert_eq!(runtime::runtime_count(), initial_count);
    }

    #[test]
    fn test_runtime_new_invalid_pca() {
        let rt_id = runtime_new_safe(b"invalid pca data");
        assert_eq!(rt_id, RuntimeId::new(0));
    }

    #[test]
    fn test_cmd_create_delete() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h1 = cmd_create_safe(rt_id, "echo hello");
        let h2 = cmd_create_safe(rt_id, "echo world");

        assert!(h1 > CommandHandle::new(0));
        assert!(h2 > CommandHandle::new(0));
        assert_ne!(h1, h2);

        cmd_delete_safe(rt_id, h1);
        cmd_delete_safe(rt_id, h2);
    }

    #[test]
    fn test_cmd_create_wrong_runtime() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Try to create command in non-existent runtime
        let h = cmd_create_safe(RuntimeId::new(rt_id.raw() + 999), "echo hello");
        assert_eq!(h, CommandHandle::new(0));
    }

    #[test]
    fn test_empty_command() {
        let (guard, _auth_guard) = setup();
        let h = cmd_create_safe(guard.id(), "");
        assert_eq!(h, CommandHandle::new(0));
    }

    #[test]
    fn test_run_single() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "echo test");
        let resp = step_safe(rt_id).expect("runtime should exist");

        // Should have host ops (Output and/or CommandExit)
        assert!(!resp.host_ops.is_empty() || resp.all_done());

        cmd_delete_safe(rt_id, h);
    }

    #[test]
    fn test_run_multiple_runtimes() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let guard1 = RuntimeGuard::new(&pca_bytes);
        let guard2 = RuntimeGuard::new(&pca_bytes);
        let rt1 = guard1.id();
        let rt2 = guard2.id();

        let h1 = cmd_create_safe(rt1, "echo runtime1");
        let h2 = cmd_create_safe(rt2, "echo runtime2");

        let collected = run_all_to_completion(&[rt1, rt2]);

        assert!(collected.contains_key(&rt1));
        assert!(collected.contains_key(&rt2));

        let rt1_output = collected.get(&rt1).unwrap().get(&Some(h1)).unwrap();
        let rt2_output = collected.get(&rt2).unwrap().get(&Some(h2)).unwrap();

        assert!(
            rt1_output.stdout.contains("runtime1"),
            "rt1 stdout should contain 'runtime1', got: {:?}",
            rt1_output.stdout
        );
        assert!(
            rt2_output.stdout.contains("runtime2"),
            "rt2 stdout should contain 'runtime2', got: {:?}",
            rt2_output.stdout
        );

        cmd_delete_safe(rt1, h1);
        cmd_delete_safe(rt2, h2);
    }

    #[test]
    fn test_wasm_api_null_safety() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // These should not crash
        assert_eq!(runtime_new(std::ptr::null(), 10), 0);
        assert_eq!(cmd_create(rt_id.raw(), std::ptr::null(), 10), 0);
        assert_eq!(cmd_create(rt_id.raw(), b"test".as_ptr(), 0), 0);

        cmd_delete(rt_id.raw(), 0);
    }

    // =========================================================================
    // End-to-End Integration Tests
    // =========================================================================

    #[test]
    fn e2e_full_lifecycle() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Create command
        let h = cmd_create_safe(rt_id, "echo 'Hello, World!'");
        assert!(h > CommandHandle::new(0), "Command creation should succeed");

        // Run until completion and collect streamed output
        let collected = run_to_completion(rt_id);

        // Verify output
        let output = collected
            .get(&Some(h))
            .expect("Should have output for handle");
        assert!(
            output.stdout.contains("Hello, World!"),
            "stdout should contain greeting: {:?}",
            output.stdout
        );
        assert!(output.stderr.is_empty(), "stderr should be empty");
        assert_eq!(output.exit_code, Some(0), "exit code should be 0");

        // Delete and verify cleanup
        cmd_delete_safe(rt_id, h);
    }

    #[test]
    fn e2e_concurrent_commands() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Create multiple commands
        let h1 = cmd_create_safe(rt_id, "echo first");
        let h2 = cmd_create_safe(rt_id, "echo second");
        let h3 = cmd_create_safe(rt_id, "echo third");

        let zero = CommandHandle::new(0);
        assert!(h1 > zero && h2 > zero && h3 > zero);
        assert!(h1 != h2 && h2 != h3 && h1 != h3, "Handles should be unique");

        // Run all and collect streamed output
        let collected = run_to_completion(rt_id);

        // Verify all completed
        assert_eq!(collected.len(), 3, "Should have 3 outputs");

        // Verify each output
        let o1 = collected.get(&Some(h1)).unwrap();
        let o2 = collected.get(&Some(h2)).unwrap();
        let o3 = collected.get(&Some(h3)).unwrap();

        assert!(o1.stdout.contains("first"));
        assert!(o2.stdout.contains("second"));
        assert!(o3.stdout.contains("third"));

        cmd_delete_safe(rt_id, h1);
        cmd_delete_safe(rt_id, h2);
        cmd_delete_safe(rt_id, h3);
    }

    #[test]
    fn e2e_stdin_flow() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Create cat command (reads from stdin)
        let h = cmd_create_safe(rt_id, "cat");

        // First step - should emit ReadStdin host op
        let resp1 = step_safe(rt_id).unwrap();
        let stdin_op = resp1.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::ReadStdin { .. }) && op.command == Some(h)
        });
        assert!(stdin_op.is_some(), "cat should emit ReadStdin host op");
        let stdin_op = stdin_op.unwrap();

        // Respond with stdin data (using StdinData response)
        let results = vec![HostOpResult {
            id: stdin_op.id,
            runtime_id: stdin_op.runtime_id,
            result: HostOpResponse::StdinData {
                data: b"test input data".to_vec(),
                eof: true,
            },
        }];
        submit_safe(&results);

        // Run to completion - cat should output the data
        let collected = run_to_completion(rt_id);
        let output = collected.get(&Some(h)).unwrap();

        assert_eq!(
            output.exit_code,
            Some(0),
            "Should exit with code 0, got: {:?}",
            output.exit_code
        );
        assert!(
            output.stdout.contains("test input data"),
            "stdout should contain stdin data: {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    #[test]
    fn e2e_pipeline() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "echo hello world | cat");
        let collected = run_all_registered_to_completion();

        let output = collected.get(&rt_id).unwrap().get(&Some(h)).unwrap();
        assert_eq!(
            output.exit_code,
            Some(0),
            "Pipeline should exit with code 0"
        );
        assert!(
            output.stdout.contains("hello world"),
            "Pipeline should pass through output: {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    #[test]
    fn e2e_vfs_write_read() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Write a file
        let h1 = cmd_create_safe(rt_id, "echo 'file content' > /workspace/test.txt");
        let collected1 = run_all_registered_to_completion();
        assert!(
            collected1
                .get(&rt_id)
                .unwrap()
                .get(&Some(h1))
                .unwrap()
                .exit_code
                == Some(0),
            "Write command should succeed"
        );
        cmd_delete_safe(rt_id, h1);

        // Read it back
        let h2 = cmd_create_safe(rt_id, "cat /workspace/test.txt");
        let collected2 = run_all_registered_to_completion();

        let output = collected2.get(&rt_id).unwrap().get(&Some(h2)).unwrap();
        assert_eq!(output.exit_code, Some(0), "Read command should succeed");
        assert!(
            output.stdout.contains("file content"),
            "Should read back written content: {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h2);
    }

    #[test]
    fn e2e_isolated_vfs() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        // Write to rt1's VFS
        let h1 = cmd_create_safe(rt1, "echo 'rt1 data' > /workspace/test.txt");
        run_all_registered_to_completion();
        cmd_delete_safe(rt1, h1);

        // Write different content to rt2's VFS
        let h2 = cmd_create_safe(rt2, "echo 'rt2 data' > /workspace/test.txt");
        run_all_registered_to_completion();
        cmd_delete_safe(rt2, h2);

        // Read from each runtime - should be isolated
        let h1_read = cmd_create_safe(rt1, "cat /workspace/test.txt");
        let h2_read = cmd_create_safe(rt2, "cat /workspace/test.txt");
        let collected = run_all_registered_to_completion();

        let o1 = collected.get(&rt1).unwrap().get(&Some(h1_read)).unwrap();
        let o2 = collected.get(&rt2).unwrap().get(&Some(h2_read)).unwrap();

        assert!(
            o1.stdout.contains("rt1 data"),
            "rt1 should have its own data"
        );
        assert!(
            o2.stdout.contains("rt2 data"),
            "rt2 should have its own data"
        );

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
    }

    // =========================================================================
    // Comprehensive Multi-Runtime Tests
    // =========================================================================

    /// Test many runtimes with varying command counts
    #[test]
    fn e2e_many_runtimes_varying_commands() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        // Create 5 runtimes with different numbers of commands
        // rt1: 1 command, rt2: 2 commands, rt3: 3 commands, etc.
        let mut runtimes: Vec<(RuntimeId, Vec<CommandHandle>)> = Vec::new();

        for i in 1..=5 {
            let rt_id = runtime_new_safe(&pca_bytes);
            assert!(rt_id > RuntimeId::new(0), "Runtime {i} should be created");

            let mut handles = Vec::new();
            for j in 1..=i {
                let h = cmd_create_safe(rt_id, &format!("echo rt{i}_cmd{j}"));
                assert!(
                    h > CommandHandle::new(0),
                    "Command {j} in runtime {i} should be created"
                );
                handles.push(h);
            }
            runtimes.push((rt_id, handles));
        }

        assert_eq!(runtime::runtime_count(), 5);

        // Run all commands across all runtimes and collect streamed output
        let collected = run_all_registered_to_completion();

        // Verify each runtime has correct number of outputs
        for (i, (rt_id, handles)) in runtimes.iter().enumerate() {
            let rt_output = collected
                .get(rt_id)
                .unwrap_or_else(|| panic!("Should have output for runtime {}", i + 1));
            assert_eq!(
                rt_output.len(),
                handles.len(),
                "Runtime {} should have {} outputs",
                i + 1,
                handles.len()
            );

            // Verify each command output
            for (j, h) in handles.iter().enumerate() {
                let output = rt_output
                    .get(&Some(*h))
                    .unwrap_or_else(|| panic!("Should have output for handle {h}"));
                let expected = format!("rt{}_cmd{}", i + 1, j + 1);
                assert!(
                    output.stdout.contains(&expected),
                    "Output should contain '{}', got: {:?}",
                    expected,
                    output.stdout
                );
                assert_eq!(output.exit_code, Some(0), "Command should succeed");
            }
        }

        // Cleanup
        for (rt_id, handles) in &runtimes {
            for h in handles {
                cmd_delete_safe(*rt_id, *h);
            }
            runtime_destroy_safe(*rt_id);
        }

        assert_eq!(runtime::runtime_count(), 0);
    }

    /// Test interleaved command creation and execution across runtimes
    #[test]
    fn e2e_interleaved_commands() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);
        let rt3 = runtime_new_safe(&pca_bytes);

        // Create commands in interleaved order
        let h1_a = cmd_create_safe(rt1, "echo rt1_first");
        let h2_a = cmd_create_safe(rt2, "echo rt2_first");
        let h3_a = cmd_create_safe(rt3, "echo rt3_first");
        let h1_b = cmd_create_safe(rt1, "echo rt1_second");
        let h2_b = cmd_create_safe(rt2, "echo rt2_second");
        let h1_c = cmd_create_safe(rt1, "echo rt1_third");

        // Run all and collect streamed output
        let collected = run_all_registered_to_completion();

        // Verify rt1 has 3 commands
        let rt1_output = collected.get(&rt1).unwrap();
        assert_eq!(rt1_output.len(), 3);
        assert!(
            rt1_output
                .get(&Some(h1_a))
                .unwrap()
                .stdout
                .contains("rt1_first")
        );
        assert!(
            rt1_output
                .get(&Some(h1_b))
                .unwrap()
                .stdout
                .contains("rt1_second")
        );
        assert!(
            rt1_output
                .get(&Some(h1_c))
                .unwrap()
                .stdout
                .contains("rt1_third")
        );

        // Verify rt2 has 2 commands
        let rt2_output = collected.get(&rt2).unwrap();
        assert_eq!(rt2_output.len(), 2);
        assert!(
            rt2_output
                .get(&Some(h2_a))
                .unwrap()
                .stdout
                .contains("rt2_first")
        );
        assert!(
            rt2_output
                .get(&Some(h2_b))
                .unwrap()
                .stdout
                .contains("rt2_second")
        );

        // Verify rt3 has 1 command
        let rt3_output = collected.get(&rt3).unwrap();
        assert_eq!(rt3_output.len(), 1);
        assert!(
            rt3_output
                .get(&Some(h3_a))
                .unwrap()
                .stdout
                .contains("rt3_first")
        );

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
        runtime_destroy_safe(rt3);
    }

    /// Test stdin to different commands in different runtimes
    #[test]
    fn e2e_stdin_multiple_runtimes() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        let h1 = cmd_create_safe(rt1, "cat");
        let h2 = cmd_create_safe(rt2, "cat");

        // First step - both should emit ReadStdin host op
        let resp1 = step_safe(rt1).unwrap();
        let stdin_op1 = resp1.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::ReadStdin { .. }) && op.command == Some(h1)
        });
        assert!(stdin_op1.is_some(), "rt1 cat should emit ReadStdin host op");
        let stdin_op1 = stdin_op1.unwrap();

        let resp2 = step_safe(rt2).unwrap();
        let stdin_op2 = resp2.host_ops.iter().find(|op| {
            matches!(&op.request, HostOpRequest::ReadStdin { .. }) && op.command == Some(h2)
        });
        assert!(stdin_op2.is_some(), "rt2 cat should emit ReadStdin host op");
        let stdin_op2 = stdin_op2.unwrap();

        // Respond with different stdin data for each
        let results = vec![
            HostOpResult {
                id: stdin_op1.id,
                runtime_id: stdin_op1.runtime_id,
                result: HostOpResponse::StdinData {
                    data: b"data for runtime 1".to_vec(),
                    eof: true,
                },
            },
            HostOpResult {
                id: stdin_op2.id,
                runtime_id: stdin_op2.runtime_id,
                result: HostOpResponse::StdinData {
                    data: b"data for runtime 2".to_vec(),
                    eof: true,
                },
            },
        ];
        submit_safe(&results);

        // Run to completion and collect output
        let collected = run_all_registered_to_completion();

        let o1 = collected.get(&rt1).unwrap().get(&Some(h1)).unwrap();
        let o2 = collected.get(&rt2).unwrap().get(&Some(h2)).unwrap();

        assert_eq!(o1.exit_code, Some(0));
        assert_eq!(o2.exit_code, Some(0));
        assert!(o1.stdout.contains("data for runtime 1"));
        assert!(o2.stdout.contains("data for runtime 2"));

        // Ensure data didn't cross over
        assert!(!o1.stdout.contains("runtime 2"));
        assert!(!o2.stdout.contains("runtime 1"));

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
    }

    /// Test runtime destruction while commands are pending
    #[test]
    fn e2e_destroy_with_pending_commands() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        // Create commands in both
        let _h1 = cmd_create_safe(rt1, "echo rt1");
        let h2 = cmd_create_safe(rt2, "echo rt2");

        // Destroy rt1 before running
        runtime_destroy_safe(rt1);
        assert_eq!(runtime::runtime_count(), 1);

        // Run - should only have rt2
        let collected = run_all_registered_to_completion();
        assert!(
            !collected.contains_key(&rt1),
            "Destroyed runtime should not be in response"
        );
        assert!(collected.contains_key(&rt2), "rt2 should still work");
        assert!(
            collected
                .get(&rt2)
                .unwrap()
                .get(&Some(h2))
                .unwrap()
                .stdout
                .contains("rt2")
        );

        runtime_destroy_safe(rt2);
    }

    /// Test mixed command types across runtimes
    #[test]
    fn e2e_mixed_commands_multiple_runtimes() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);
        let rt3 = runtime_new_safe(&pca_bytes);

        // rt1: simple echo
        let h1 = cmd_create_safe(rt1, "echo simple");

        // rt2: pipeline
        let h2 = cmd_create_safe(rt2, "echo pipeline | cat");

        // rt3: file write operation
        let h3a = cmd_create_safe(rt3, "echo 'file data' > /workspace/test.txt");

        // Run all - outputs are streamed
        let collected1 = run_all_registered_to_completion();

        // Verify first batch outputs (captured from first run)
        assert!(
            collected1
                .get(&rt1)
                .unwrap()
                .get(&Some(h1))
                .unwrap()
                .stdout
                .contains("simple"),
            "rt1 echo should output 'simple'"
        );
        assert!(
            collected1
                .get(&rt2)
                .unwrap()
                .get(&Some(h2))
                .unwrap()
                .stdout
                .contains("pipeline"),
            "rt2 pipeline should output 'pipeline'"
        );

        // Clean up first batch
        cmd_delete_safe(rt1, h1);
        cmd_delete_safe(rt2, h2);
        cmd_delete_safe(rt3, h3a);

        // rt3: read the file we just wrote
        let h3b = cmd_create_safe(rt3, "cat /workspace/test.txt");

        let collected2 = run_all_registered_to_completion();

        // Verify file read (from second run)
        assert!(
            collected2
                .get(&rt3)
                .unwrap()
                .get(&Some(h3b))
                .unwrap()
                .stdout
                .contains("file data"),
            "rt3 cat should output 'file data'"
        );

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
        runtime_destroy_safe(rt3);
    }

    /// Test command handles are unique per runtime but can overlap across runtimes
    #[test]
    fn e2e_handle_namespacing() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        // Both start at handle 1
        let h1 = cmd_create_safe(rt1, "echo rt1");
        let h2 = cmd_create_safe(rt2, "echo rt2");

        // Handles might be the same value (both could be 1)
        // But they refer to different commands in different runtimes

        let collected = run_all_registered_to_completion();

        // Even if h1 == h2, they should produce different outputs
        let o1 = collected.get(&rt1).unwrap().get(&Some(h1)).unwrap();
        let o2 = collected.get(&rt2).unwrap().get(&Some(h2)).unwrap();

        assert!(
            o1.stdout.contains("rt1"),
            "rt1's command should output 'rt1'"
        );
        assert!(
            o2.stdout.contains("rt2"),
            "rt2's command should output 'rt2'"
        );

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
    }

    /// Test 10 runtimes with 10 commands each (stress test)
    #[test]
    fn e2e_stress_many_runtimes_many_commands() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let num_runtimes = 10;
        let commands_per_runtime = 10;

        let mut runtimes: Vec<(RuntimeId, Vec<CommandHandle>)> = Vec::new();

        for i in 0..num_runtimes {
            let rt_id = runtime_new_safe(&pca_bytes);
            let mut handles = Vec::new();
            for j in 0..commands_per_runtime {
                let h = cmd_create_safe(rt_id, &format!("echo r{i}c{j}"));
                handles.push(h);
            }
            runtimes.push((rt_id, handles));
        }

        assert_eq!(runtime::runtime_count(), num_runtimes);

        let collected = run_all_registered_to_completion();
        assert_eq!(collected.len(), num_runtimes);

        // Verify all outputs
        for (i, (rt_id, handles)) in runtimes.iter().enumerate() {
            let rt_output = collected.get(rt_id).unwrap();
            assert_eq!(rt_output.len(), commands_per_runtime);

            for (j, h) in handles.iter().enumerate() {
                let output = rt_output.get(&Some(*h)).unwrap();
                assert!(output.stdout.contains(&format!("r{i}c{j}")));
                assert_eq!(output.exit_code, Some(0));
            }
        }

        // Cleanup
        for (rt_id, _) in &runtimes {
            runtime_destroy_safe(*rt_id);
        }

        assert_eq!(runtime::runtime_count(), 0);
    }

    /// Test that operations on wrong runtime ID are no-ops
    #[test]
    fn e2e_wrong_runtime_id_operations() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt = runtime_new_safe(&pca_bytes);
        let h = cmd_create_safe(rt, "echo test");

        let wrong_rt = RuntimeId::new(rt.raw() + 999);

        // These should all be no-ops (not crash)
        cmd_delete_safe(wrong_rt, h);

        // Original command should still work
        let collected = run_all_registered_to_completion();
        assert!(
            collected
                .get(&rt)
                .unwrap()
                .get(&Some(h))
                .unwrap()
                .stdout
                .contains("test")
        );

        runtime_destroy_safe(rt);
    }

    // =========================================================================
    // Async Host Operations Tests
    // =========================================================================

    /// Test that `submit_safe` returns correct count
    #[test]
    fn async_host_ops_submit_empty() {
        let count = submit_safe(&[]);
        assert_eq!(count, 0);
    }

    /// Test submit with results for non-existent runtime
    #[test]
    fn async_host_ops_submit_wrong_runtime() {
        let results = vec![HostOpResult {
            id: 1,
            runtime_id: RuntimeId::new(9999), // Non-existent
            result: HostOpResponse::woke_at(1_000_000_000),
        }];

        let count = submit_safe(&results);
        assert_eq!(
            count, 0,
            "Should not process results for non-existent runtime"
        );
    }

    /// Test submit results routes to correct runtime
    #[test]
    fn async_host_ops_submit_routing() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        // Create commands (they won't trigger host ops, but runtime exists)
        let _h1 = cmd_create_safe(rt1, "echo test1");
        let _h2 = cmd_create_safe(rt2, "echo test2");

        // Step each runtime (these don't need host ops)
        let _resp1 = step_safe(rt1);
        let _resp2 = step_safe(rt2);

        // Submit results with different runtime_ids
        // These won't match any pending ops, but the routing code runs
        let results = vec![
            HostOpResult {
                id: 1,
                runtime_id: rt1,
                result: HostOpResponse::woke_at(12345),
            },
            HostOpResult {
                id: 2,
                runtime_id: rt2,
                result: HostOpResponse::woke_at(1_000_000_000),
            },
            HostOpResult {
                id: 3,
                runtime_id: RuntimeId::new(9999), // Non-existent - should be skipped
                result: HostOpResponse::woke_at(1_000_000_000),
            },
        ];

        // The submit should process results for existing runtimes only
        // (Though they won't match pending ops, the routing happens)
        let _count = submit_safe(&results);

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
    }

    /// Test that host ops vec uses `SmallVec` optimization
    #[test]
    fn async_host_ops_smallvec_optimization() {
        use crate::types::HostOpsVec;

        // SmallVec<[PendingHostOp; 4]> should not allocate for <= 4 items
        let mut vec: HostOpsVec = HostOpsVec::new();
        assert!(!vec.spilled(), "Empty SmallVec should not spill");

        // Add 4 items (should still be inline)
        for i in 0..4 {
            vec.push(crate::host_ops::PendingHostOp {
                id: i,
                runtime_id: RuntimeId::new(1),
                command: Some(CommandHandle::new(1)),
                request: crate::host_ops::HostOpRequest::WakeAt { deadline_nanos: i },
            });
        }
        assert!(!vec.spilled(), "SmallVec with 4 items should not spill");

        // Add 5th item - should spill to heap
        vec.push(crate::host_ops::PendingHostOp {
            id: 4,
            runtime_id: RuntimeId::new(1),
            command: Some(CommandHandle::new(1)),
            request: crate::host_ops::HostOpRequest::WakeAt { deadline_nanos: 4 },
        });
        assert!(vec.spilled(), "SmallVec with 5 items should spill");
    }

    /// Test `StepResponse` serialization with `host_ops`
    #[test]
    fn async_host_ops_response_serialization() {
        use crate::types::{HostOpsVec, RuntimeStatus, StepResponse};

        // Empty host_ops should be skipped in serialization
        let resp_empty = StepResponse {
            host_ops: HostOpsVec::new(),
            status: RuntimeStatus::AllDone,
        };
        let json = serde_json::to_string(&resp_empty).unwrap();
        assert!(
            !json.contains("host_ops"),
            "Empty host_ops should be skipped: {json}"
        );

        // Non-empty host_ops should be included
        let mut host_ops = HostOpsVec::new();
        host_ops.push(crate::host_ops::PendingHostOp {
            id: 42,
            runtime_id: RuntimeId::new(1),
            command: Some(CommandHandle::new(1)),
            request: crate::host_ops::HostOpRequest::WakeAt {
                deadline_nanos: 1_000_000_000,
            },
        });
        let resp_with_ops = StepResponse {
            host_ops,
            status: RuntimeStatus::AllBlocked,
        };
        let json = serde_json::to_string(&resp_with_ops).unwrap();
        assert!(
            json.contains("host_ops"),
            "Non-empty host_ops should be included: {json}"
        );
        assert!(
            json.contains("42"),
            "host_ops should contain the op id: {json}"
        );
    }

    /// Test `HostOpResult` serialization roundtrip
    #[test]
    fn async_host_ops_result_roundtrip() {
        let results = vec![
            HostOpResult {
                id: 1,
                runtime_id: RuntimeId::new(100),
                result: HostOpResponse::woke_at(999_000_000),
            },
            HostOpResult {
                id: 2,
                runtime_id: RuntimeId::new(100),
                result: HostOpResponse::woke_at(1_000_000_000),
            },
            HostOpResult {
                id: 3,
                runtime_id: RuntimeId::new(200),
                result: HostOpResponse::VfsData {
                    data: b"file content".to_vec(),
                },
            },
            HostOpResult {
                id: 4,
                runtime_id: RuntimeId::new(300),
                result: HostOpResponse::error("not_found", "File not found"),
            },
        ];

        let json = serde_json::to_string(&results).unwrap();
        let parsed: Vec<HostOpResult> = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].id, 1);
        assert_eq!(parsed[0].runtime_id, RuntimeId::new(100));
        assert_eq!(parsed[2].runtime_id, RuntimeId::new(200));
        assert_eq!(parsed[3].runtime_id, RuntimeId::new(300));
    }

    /// Test `convert_host_op` mapping
    #[test]
    fn async_host_ops_conversion() {
        use amla_scheduler::HostOpKind;

        // Test WakeAt -> WakeAt (passthrough)
        let wake_req = crate::runtime::convert_host_op(&HostOpKind::WakeAt {
            deadline: 5_000_000_000, // 5 seconds in nanos
        });
        assert!(matches!(
            wake_req,
            Some(crate::host_ops::HostOpRequest::WakeAt {
                deadline_nanos: 5_000_000_000
            })
        ));

        // Test FileRead -> VfsRead
        let read_req = crate::runtime::convert_host_op(&HostOpKind::FileRead {
            path: "/test/file.txt".to_string(),
        });
        if let Some(crate::host_ops::HostOpRequest::VfsRead { path, offset, .. }) = read_req {
            assert_eq!(path, "/test/file.txt");
            assert_eq!(offset, 0);
        } else {
            panic!("Expected VfsRead");
        }

        // Test Print -> Output (streamed to host)
        // Note: runtime_id and command come from PendingHostOp, not the HostOpRequest itself
        let print_req = crate::runtime::convert_host_op(&HostOpKind::Print {
            stream: 1,
            data: b"output".to_vec(),
        });
        assert!(
            matches!(
                print_req,
                Some(crate::host_ops::HostOpRequest::Output { stream: 1, .. })
            ),
            "Print should map to Output"
        );

        // Test ReadStdin -> ReadStdin (exposed to host for pull-based stdin)
        let stdin_req = crate::runtime::convert_host_op(&HostOpKind::ReadStdin { max_bytes: 100 });
        assert!(
            matches!(
                stdin_req,
                Some(crate::host_ops::HostOpRequest::ReadStdin { max_bytes: 100 })
            ),
            "ReadStdin should map to ReadStdin"
        );
    }

    /// Test `response_to_bytes` conversion
    #[test]
    fn async_host_ops_response_to_bytes() {
        // Test WokeAt
        let woke_bytes =
            crate::runtime::response_to_bytes(&HostOpResponse::woke_at(2_000_000_000)).unwrap();
        let wake_nanos = u64::from_le_bytes(woke_bytes.try_into().unwrap());
        assert_eq!(wake_nanos, 2_000_000_000);

        // Test VfsData
        let data_bytes = crate::runtime::response_to_bytes(&HostOpResponse::VfsData {
            data: b"test".to_vec(),
        })
        .unwrap();
        assert_eq!(data_bytes, b"test");

        // Test Error - now returns std::io::Error with HostOpError inside
        let err_result =
            crate::runtime::response_to_bytes(&HostOpResponse::error("fail", "it failed"));
        assert!(err_result.is_err());
        let io_err = err_result.unwrap_err();
        // Check error kind is Other (unknown code)
        assert_eq!(io_err.kind(), std::io::ErrorKind::Other);
        // Check error message contains the code
        assert!(io_err.to_string().contains("fail"));
        // Verify we can downcast to HostOpError
        let host_err = io_err
            .get_ref()
            .expect("should have inner error")
            .downcast_ref::<crate::HostOpError>()
            .expect("should be HostOpError");
        assert_eq!(host_err.message(), "it failed");
    }

    /// Test `HostOpId` From implementations
    #[test]
    fn async_host_ops_id_conversion() {
        use amla_scheduler::HostOpId;

        // u64 -> HostOpId
        let id: HostOpId = 42u64.into();

        // HostOpId -> u64
        let val: u64 = id.into();
        assert_eq!(val, 42);
    }

    /// Test that commands can work without triggering async host ops
    #[test]
    fn async_host_ops_simple_commands_no_ops() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt_id = runtime_new_safe(&pca_bytes);

        // Simple echo command emits Output and CommandExit host ops for streaming
        let _h = cmd_create_safe(rt_id, "echo hello");
        let collected = run_all_registered_to_completion();

        // Verify command completed via collected output
        let rt_output = collected.get(&rt_id).unwrap();
        assert_eq!(rt_output.len(), 1, "Should have one command output");

        runtime_destroy_safe(rt_id);
    }

    /// Test `HostOpsVec` Default implementation
    #[test]
    fn async_host_ops_vec_default() {
        use crate::types::HostOpsVec;

        let vec: HostOpsVec = HostOpsVec::default();
        assert!(vec.is_empty());
        assert!(!vec.spilled());
    }

    /// Test async cycle with sleep command (triggers `WakeAt` host op)
    #[test]
    fn async_host_ops_sleep_command_cycle() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt_id = runtime_new_safe(&pca_bytes);

        // Create a sleep command - this will trigger Now and WakeAt host ops
        let h = cmd_create_safe(rt_id, "sleep 0.001"); // 1ms sleep
        assert!(h > CommandHandle::new(0), "Command creation should succeed");

        // Run to completion with streaming output
        let collected = run_to_completion(rt_id);

        // Sleep command should complete
        let output = collected
            .get(&Some(h))
            .expect("Should have output for handle");
        assert_eq!(
            output.exit_code,
            Some(0),
            "Sleep command should exit with code 0"
        );

        runtime_destroy_safe(rt_id);
    }

    /// Test async cycle with date command (triggers Now host op)
    #[test]
    fn async_host_ops_date_command_cycle() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt_id = runtime_new_safe(&pca_bytes);

        // Date command needs current time
        let h = cmd_create_safe(rt_id, "date");
        assert!(h > CommandHandle::new(0), "Command creation should succeed");

        // Run to completion with streaming output
        let collected = run_to_completion(rt_id);

        // Date command should complete
        let output = collected
            .get(&Some(h))
            .expect("Should have output for handle");
        assert_eq!(
            output.exit_code,
            Some(0),
            "Date command should exit with code 0"
        );
        // Should have some output (the date string)
        assert!(
            !output.stdout.is_empty(),
            "Date command should produce output"
        );

        runtime_destroy_safe(rt_id);
    }

    /// Test multiple runtimes running date commands concurrently.
    ///
    /// This previously failed with the global scheduler because host ops from
    /// one runtime could be incorrectly attributed to another. With per-runtime
    /// schedulers, each runtime's host ops are isolated.
    #[test]
    fn async_host_ops_multi_runtime_interleaved() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        // Both runtimes run date commands (uses sync now(), no host op)
        let h1 = cmd_create_safe(rt1, "date");
        let h2 = cmd_create_safe(rt2, "date");
        let zero = CommandHandle::new(0);
        assert!(h1 > zero && h2 > zero, "Commands should be created");

        // Run all to completion
        let collected = run_all_registered_to_completion();

        // Both runtimes should have output
        let o1 = collected
            .get(&rt1)
            .unwrap()
            .get(&Some(h1))
            .expect("rt1 should have output");
        let o2 = collected
            .get(&rt2)
            .unwrap()
            .get(&Some(h2))
            .expect("rt2 should have output");

        assert_eq!(o1.exit_code, Some(0), "rt1 date should exit with code 0");
        assert_eq!(o2.exit_code, Some(0), "rt2 date should exit with code 0");
        // Date output should not be empty
        assert!(!o1.stdout.is_empty(), "rt1 date should produce output");
        assert!(!o2.stdout.is_empty(), "rt2 date should produce output");

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
    }

    /// Test multiple concurrent sleep commands
    #[test]
    fn async_host_ops_concurrent_sleeps() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt_id = runtime_new_safe(&pca_bytes);

        // Create multiple sleep commands that will all need host ops
        let h1 = cmd_create_safe(rt_id, "sleep 0.001");
        let h2 = cmd_create_safe(rt_id, "sleep 0.002");
        let h3 = cmd_create_safe(rt_id, "sleep 0.003");

        let zero = CommandHandle::new(0);
        assert!(h1 > zero && h2 > zero && h3 > zero);

        // Run to completion
        let collected = run_to_completion(rt_id);

        // All sleep commands should complete
        assert_eq!(collected.len(), 3, "Should have 3 outputs");
        for (handle, output) in &collected {
            if output.exit_code != Some(0) {
                eprintln!(
                    "Command {:?} failed with exit code {:?}",
                    handle, output.exit_code
                );
                eprintln!("  stderr: {}", output.stderr);
                eprintln!("  stdout: {}", output.stdout);
            }
            assert_eq!(
                output.exit_code,
                Some(0),
                "Sleep command should exit with code 0, stderr: {}",
                output.stderr
            );
        }

        runtime_destroy_safe(rt_id);
    }

    /// Test async host ops with path mounts (reading mounted files triggers `VfsRead`)
    #[test]
    fn async_host_ops_with_path_mounts() {
        use crate::runtime::{PathMount, Runtime};

        // Create PCA
        let keypair =
            amla_protocol::KeyPair::from_seed(amla_protocol::Algorithm::Ed25519, &[1; 32]);
        let executor =
            amla_protocol::KeyPair::from_seed(amla_protocol::Algorithm::Ed25519, &[2; 32]);

        // Set up trusted authority
        let _guard = TrustedAuthoritiesGuard::new(vec![keypair.public_key()]);

        let tool_cap = amla_capabilities::ToolCallCap::new("test:echo");
        let cap_data =
            amla_protocol::CapabilityData::new("cap:echo", "tool-call", &tool_cap).unwrap();
        let pca = amla_protocol::PcaBuilder::new()
            .version(amla_protocol::Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(chrono::Utc::now() + chrono::Duration::hours(1))
            .build_and_sign(&keypair)
            .unwrap();

        // Create path mounts - map a "host" file into the sandbox
        let mounts = vec![PathMount::new(
            "/host/data/config.json",
            "/data/config.json",
        )];

        // Create runtime with mounts
        let runtime = Runtime::from_pca_with_mounts(
            pca,
            &[],
            &mounts,
            wasm_time_source(),
            wasm_random_source(),
        )
        .unwrap();

        // Verify mount is registered
        assert!(runtime.is_mounted("/data/config.json"));
        assert_eq!(
            runtime.get_host_path("/data/config.json"),
            Some("/host/data/config.json".to_string())
        );

        // Register the runtime
        let rt_id = crate::runtime::register_runtime(runtime);

        // The mounted path should be accessible - reading it would normally
        // trigger a host op if the VFS delegates to host for mounted files.
        // For now, verify the mount infrastructure works.

        // Try to list - run to completion
        let h = cmd_create_safe(rt_id, "ls /data");
        let collected = run_to_completion(rt_id);

        let output = collected
            .get(&Some(h))
            .expect("Should have output for handle");

        // Should show config.json in listing (as a mounted placeholder)
        // or error if not implemented
        assert!(
            output.exit_code.is_some(),
            "ls command should complete: {output:?}"
        );

        runtime_destroy_safe(rt_id);
    }

    /// Test parallel host ops from multiple commands in multiple runtimes
    #[test]
    fn async_host_ops_stress_parallel() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        // Create 3 runtimes, each with 2 echo commands
        let mut runtime_ids = Vec::new();
        for i in 0..3 {
            let rt_id = runtime_new_safe(&pca_bytes);
            let _h1 = cmd_create_safe(rt_id, &format!("echo runtime_{i}_cmd_a"));
            let _h2 = cmd_create_safe(rt_id, &format!("echo runtime_{i}_cmd_b"));
            runtime_ids.push(rt_id);
        }

        // Run all to completion
        let collected = run_all_registered_to_completion();

        // All runtimes should have completed
        for rt_id in &runtime_ids {
            let rt_output = collected.get(rt_id).expect("Runtime should have output");
            assert_eq!(
                rt_output.len(),
                2,
                "Each runtime should have 2 command outputs"
            );
            for output in rt_output.values() {
                assert_eq!(output.exit_code, Some(0), "All commands should exit with 0");
            }
        }

        // Cleanup
        for rt_id in runtime_ids {
            runtime_destroy_safe(rt_id);
        }
    }

    // =========================================================================
    // Streaming Output Tests
    // =========================================================================

    /// Test streaming small output
    #[test]
    fn streaming_small_output() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "echo hello");
        let collected = run_all_registered_to_completion();

        let output = collected.get(&rt_id).unwrap().get(&Some(h)).unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert!(
            output.stdout.contains("hello"),
            "Expected 'hello' in stdout: {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test streaming large output (multiple chunks)
    #[test]
    fn streaming_large_output() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Generate output larger than the 256-byte buffer using multiple echo commands
        // Each line is ~10 chars, so 30 lines gives ~300 bytes
        let h = cmd_create_safe(
            rt_id,
            "echo line_001 && echo line_002 && echo line_003 && echo line_004 && echo line_005 && \
             echo line_006 && echo line_007 && echo line_008 && echo line_009 && echo line_010 && \
             echo line_011 && echo line_012 && echo line_013 && echo line_014 && echo line_015 && \
             echo line_016 && echo line_017 && echo line_018 && echo line_019 && echo line_020 && \
             echo line_021 && echo line_022 && echo line_023 && echo line_024 && echo line_025 && \
             echo line_026 && echo line_027 && echo line_028 && echo line_029 && echo line_030",
        );
        let collected = run_all_registered_to_completion();

        let output = collected.get(&rt_id).unwrap().get(&Some(h)).unwrap();
        assert_eq!(output.exit_code, Some(0));
        // Should contain various lines
        assert!(
            output.stdout.contains("line_001"),
            "Should contain 'line_001'"
        );
        assert!(
            output.stdout.contains("line_015"),
            "Should contain 'line_015'"
        );
        assert!(
            output.stdout.contains("line_030"),
            "Should contain 'line_030'"
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test streaming output without trailing newline
    #[test]
    fn streaming_output_no_newline() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // echo -n doesn't add trailing newline
        let h = cmd_create_safe(rt_id, "printf 'no newline'");
        let collected = run_all_registered_to_completion();

        let output = collected.get(&rt_id).unwrap().get(&Some(h)).unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(
            output.stdout, "no newline",
            "Output should be exactly 'no newline'"
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test that failed commands output to stderr and have correct exit code
    #[test]
    fn streaming_failed_command() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Run a command that doesn't exist
        let h = cmd_create_safe(rt_id, "nonexistent_command_xyz");
        let collected = run_all_registered_to_completion();

        let output = collected.get(&rt_id).unwrap().get(&Some(h)).unwrap();
        // Should have non-zero exit code
        assert_ne!(
            output.exit_code,
            Some(0),
            "Failed command should have non-zero exit code"
        );
        // Should have error message in stderr
        assert!(
            !output.stderr.is_empty(),
            "Failed command should have stderr output: {:?}",
            output.stderr
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test streaming with exit codes
    #[test]
    fn streaming_exit_codes() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Success (exit 0)
        let h1 = cmd_create_safe(rt_id, "exit 0");
        // Failure (exit 1)
        let h2 = cmd_create_safe(rt_id, "exit 1");
        // Custom exit code
        let h3 = cmd_create_safe(rt_id, "exit 42");

        let collected = run_all_registered_to_completion();

        assert_eq!(
            collected
                .get(&rt_id)
                .unwrap()
                .get(&Some(h1))
                .unwrap()
                .exit_code,
            Some(0)
        );
        assert_eq!(
            collected
                .get(&rt_id)
                .unwrap()
                .get(&Some(h2))
                .unwrap()
                .exit_code,
            Some(1)
        );
        assert_eq!(
            collected
                .get(&rt_id)
                .unwrap()
                .get(&Some(h3))
                .unwrap()
                .exit_code,
            Some(42)
        );

        cmd_delete_safe(rt_id, h1);
        cmd_delete_safe(rt_id, h2);
        cmd_delete_safe(rt_id, h3);
    }

    /// Test multiple concurrent commands streaming output
    #[test]
    fn streaming_concurrent_commands() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h1 = cmd_create_safe(rt_id, "echo first");
        let h2 = cmd_create_safe(rt_id, "echo second");
        let h3 = cmd_create_safe(rt_id, "echo third");

        let collected = run_all_registered_to_completion();

        let o1 = collected.get(&rt_id).unwrap().get(&Some(h1)).unwrap();
        let o2 = collected.get(&rt_id).unwrap().get(&Some(h2)).unwrap();
        let o3 = collected.get(&rt_id).unwrap().get(&Some(h3)).unwrap();

        assert!(o1.stdout.contains("first"));
        assert!(o2.stdout.contains("second"));
        assert!(o3.stdout.contains("third"));

        assert_eq!(o1.exit_code, Some(0));
        assert_eq!(o2.exit_code, Some(0));
        assert_eq!(o3.exit_code, Some(0));

        cmd_delete_safe(rt_id, h1);
        cmd_delete_safe(rt_id, h2);
        cmd_delete_safe(rt_id, h3);
    }

    /// Test streaming across multiple runtimes
    #[test]
    fn streaming_multiple_runtimes() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        let h1 = cmd_create_safe(rt1, "echo runtime1");
        let h2 = cmd_create_safe(rt2, "echo runtime2");

        let collected = run_all_registered_to_completion();

        let o1 = collected.get(&rt1).unwrap().get(&Some(h1)).unwrap();
        let o2 = collected.get(&rt2).unwrap().get(&Some(h2)).unwrap();

        assert!(o1.stdout.contains("runtime1"));
        assert!(o2.stdout.contains("runtime2"));
        assert_eq!(o1.exit_code, Some(0));
        assert_eq!(o2.exit_code, Some(0));

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
    }

    /// Test that output host ops have correct `runtime_id` and command fields
    #[test]
    fn streaming_output_routing() {
        let (pca_bytes, _guard) = create_test_pca_bytes();

        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        let h1 = cmd_create_safe(rt1, "echo msg1");
        let h2 = cmd_create_safe(rt2, "echo msg2");

        // Step rt1 and verify its host ops have correct routing info
        let resp1 = step_safe(rt1).unwrap();
        for op in &resp1.host_ops {
            if matches!(&op.request, HostOpRequest::Output { .. }) {
                // Routing info is in PendingHostOp, not HostOpRequest
                assert_eq!(op.runtime_id, rt1, "rt1 Output runtime_id should match");
                assert_eq!(op.command, Some(h1), "rt1 Output command should match h1");
            }
        }

        // Step rt2 and verify its host ops have correct routing info
        let resp2 = step_safe(rt2).unwrap();
        for op in &resp2.host_ops {
            if matches!(&op.request, HostOpRequest::Output { .. }) {
                // Routing info is in PendingHostOp, not HostOpRequest
                assert_eq!(op.runtime_id, rt2, "rt2 Output runtime_id should match");
                assert_eq!(op.command, Some(h2), "rt2 Output command should match h2");
            }
        }

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
    }

    // =========================================================================
    // Command Cancellation Tests
    // =========================================================================

    /// Test cancelling a command that is blocked on stdin
    #[test]
    fn cancel_command_blocked_on_stdin() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Create cat command (reads from stdin, will block)
        let h = cmd_create_safe(rt_id, "cat");

        // Step to trigger ReadStdin
        let resp1 = step_safe(rt_id).unwrap();
        let has_read_stdin = resp1.host_ops.iter().any(|op| {
            matches!(&op.request, HostOpRequest::ReadStdin { .. }) && op.command == Some(h)
        });
        assert!(has_read_stdin, "cat should emit ReadStdin host op");

        // Cancel the command
        let pending_ops = cmd_cancel_safe(rt_id, h).expect("Runtime should exist");

        // Should have emitted CommandExit with code -1
        assert_eq!(pending_ops.len(), 1, "Should have exactly one pending op");
        match &pending_ops[0].request {
            HostOpRequest::CommandExit { code, .. } => {
                // Routing info (command) is in PendingHostOp, not HostOpRequest
                assert_eq!(
                    pending_ops[0].command,
                    Some(h),
                    "CommandExit should be for our handle"
                );
                assert_eq!(*code, -1, "Cancelled command should have exit code -1");
            }
            _ => panic!(
                "Expected CommandExit host op, got {:?}",
                pending_ops[0].request
            ),
        }

        // Step again - should report AllDone
        let resp2 = step_safe(rt_id).unwrap();
        assert!(resp2.all_done(), "Runtime should be AllDone after cancel");

        cmd_delete_safe(rt_id, h);
    }

    /// Test cancelling a command that hasn't started stepping yet
    #[test]
    fn cancel_command_before_stepping() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Create command but don't step
        let h = cmd_create_safe(rt_id, "echo hello");

        // Cancel immediately
        let pending_ops = cmd_cancel_safe(rt_id, h).expect("Runtime should exist");

        // Should have emitted CommandExit
        assert_eq!(pending_ops.len(), 1);
        match &pending_ops[0].request {
            HostOpRequest::CommandExit { code, .. } => {
                assert_eq!(*code, -1, "Cancelled command should have exit code -1");
            }
            _ => panic!("Expected CommandExit"),
        }

        // Step should show AllDone
        let resp = step_safe(rt_id).unwrap();
        assert!(resp.all_done(), "Runtime should be AllDone after cancel");

        cmd_delete_safe(rt_id, h);
    }

    /// Test cancelling is idempotent (cancelling twice is no-op)
    #[test]
    fn cancel_command_idempotent() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "cat");
        let _ = step_safe(rt_id); // Trigger ReadStdin

        // First cancel - should emit CommandExit
        let ops1 = cmd_cancel_safe(rt_id, h).expect("Runtime should exist");
        assert_eq!(ops1.len(), 1, "First cancel should emit CommandExit");

        // Second cancel - should be no-op (empty)
        let ops2 = cmd_cancel_safe(rt_id, h).expect("Runtime should exist");
        assert!(ops2.is_empty(), "Second cancel should be no-op");

        // Third cancel - still no-op
        let ops3 = cmd_cancel_safe(rt_id, h).expect("Runtime should exist");
        assert!(ops3.is_empty(), "Third cancel should be no-op");

        cmd_delete_safe(rt_id, h);
    }

    /// Test cancelling an already completed command is a no-op
    #[test]
    fn cancel_completed_command_noop() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Create and run command to completion
        let h = cmd_create_safe(rt_id, "echo done");
        let _collected = run_to_completion(rt_id);

        // Try to cancel completed command
        let pending_ops = cmd_cancel_safe(rt_id, h).expect("Runtime should exist");

        // Should be empty - already completed, no CommandExit emitted
        assert!(
            pending_ops.is_empty(),
            "Cancelling completed command should be no-op"
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test cancelling non-existent command
    #[test]
    fn cancel_nonexistent_command() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Try to cancel command that doesn't exist
        let pending_ops =
            cmd_cancel_safe(rt_id, CommandHandle::new(999)).expect("Runtime should exist");
        assert!(
            pending_ops.is_empty(),
            "Cancelling non-existent command should be no-op"
        );
    }

    /// Test cancelling command in non-existent runtime
    #[test]
    fn cancel_command_wrong_runtime() {
        // Try to cancel in non-existent runtime
        let result = cmd_cancel_safe(RuntimeId::new(999), CommandHandle::new(1));
        assert!(
            result.is_none(),
            "Cancelling in non-existent runtime should return None"
        );
    }

    /// Test cancelling across multiple runtimes (isolation)
    ///
    /// NOTE: Commands within the same runtime share a scheduler. For reliable
    /// command isolation, use separate runtimes. This test verifies that
    /// cancelling a command in one runtime doesn't affect another runtime.
    #[test]
    fn cancel_command_runtime_isolation() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        // Create commands in both runtimes - rt1 has cat (blocks), rt2 has echo (completes)
        let h1 = cmd_create_safe(rt1, "cat");
        let h2 = cmd_create_safe(rt2, "echo rt2_output");

        // Step rt1 to trigger ReadStdin
        let resp1 = step_safe(rt1).unwrap();
        let has_h1_read_stdin = resp1.host_ops.iter().any(|op| {
            matches!(&op.request, HostOpRequest::ReadStdin { .. }) && op.command == Some(h1)
        });
        assert!(has_h1_read_stdin, "rt1 cat should emit ReadStdin");

        // Cancel rt1's command
        let ops1 = cmd_cancel_safe(rt1, h1).expect("rt1 should exist");
        assert_eq!(ops1.len(), 1, "rt1 cancel should emit CommandExit");

        // rt2's command should still work - cancelling rt1 shouldn't affect rt2
        let collected = run_all_to_completion(&[rt2]);

        let o2 = collected.get(&rt2).unwrap().get(&Some(h2)).unwrap();
        assert_eq!(
            o2.exit_code,
            Some(0),
            "rt2 command should complete normally"
        );
        assert!(
            o2.stdout.contains("rt2_output"),
            "rt2 should have correct output"
        );

        runtime_destroy_safe(rt1);
        runtime_destroy_safe(rt2);
    }

    /// Test WASM FFI `cmd_cancel` with null/invalid arguments
    #[test]
    fn cancel_wasm_api_null_safety() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();
        let h = cmd_create_safe(rt_id, "cat");
        let _ = step_safe(rt_id);

        let mut buffer = [0u8; 1024];

        // Null output pointer
        assert_eq!(
            cmd_cancel(rt_id.raw(), h.raw(), std::ptr::null_mut(), 1024),
            0
        );

        // Zero output length
        assert_eq!(cmd_cancel(rt_id.raw(), h.raw(), buffer.as_mut_ptr(), 0), 0);

        // Zero runtime ID
        assert_eq!(cmd_cancel(0, h.raw(), buffer.as_mut_ptr(), 1024), 0);

        // Zero handle
        assert_eq!(cmd_cancel(rt_id.raw(), 0, buffer.as_mut_ptr(), 1024), 0);

        // Valid call should work
        let n = cmd_cancel(rt_id.raw(), h.raw(), buffer.as_mut_ptr(), 1024);
        assert!(n > 0, "Valid cancel should write JSON to buffer");

        // Parse the JSON
        let json_str = std::str::from_utf8(&buffer[..n]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
        assert!(parsed.is_array(), "Output should be JSON array");

        cmd_delete_safe(rt_id, h);
    }

    // =========================================================================
    // Command substitution end-to-end tests
    // =========================================================================

    /// Test simple command substitution
    #[test]
    fn e2e_command_substitution_simple() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "echo $(echo hello)");
        let collected = run_to_completion(rt_id);

        let output = collected.get(&Some(h)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0));
        assert!(
            output.stdout.contains("hello"),
            "stdout should contain 'hello': {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test nested command substitution
    #[test]
    fn e2e_command_substitution_nested() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "echo $(echo $(echo nested))");
        let collected = run_to_completion(rt_id);

        let output = collected.get(&Some(h)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0));
        assert!(
            output.stdout.contains("nested"),
            "stdout should contain 'nested': {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test command substitution with pipeline
    #[test]
    fn e2e_command_substitution_with_pipeline() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Create a file, then use command substitution with pipeline
        let h1 = cmd_create_safe(
            rt_id,
            "echo -e 'apple\\nbanana\\ncherry' > /workspace/fruits.txt",
        );
        run_to_completion(rt_id);
        cmd_delete_safe(rt_id, h1);

        let h2 = cmd_create_safe(rt_id, "echo count: $(cat /workspace/fruits.txt | wc -l)");
        let collected = run_to_completion(rt_id);

        let output = collected.get(&Some(h2)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0));
        // wc -l may include leading spaces, so check for "count:" and "3"
        assert!(
            output.stdout.contains("count:") && output.stdout.contains('3'),
            "stdout should contain 'count:' and '3': {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h2);
    }

    /// Test command substitution in variable assignment
    #[test]
    fn e2e_command_substitution_variable_assignment() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Use sequence: assign variable with substitution, then use it
        let h = cmd_create_safe(rt_id, "export VAL=$(echo computed); echo VAL=$VAL");
        let collected = run_to_completion(rt_id);

        let output = collected.get(&Some(h)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0));
        assert!(
            output.stdout.contains("VAL=computed"),
            "stdout should contain 'VAL=computed': {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test command substitution in double quotes
    #[test]
    fn e2e_command_substitution_in_quotes() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "echo \"result: $(echo quoted value)\"");
        let collected = run_to_completion(rt_id);

        let output = collected.get(&Some(h)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0));
        assert!(
            output.stdout.contains("result: quoted value"),
            "stdout should contain 'result: quoted value': {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test single quotes prevent command substitution
    #[test]
    fn e2e_command_substitution_single_quotes_literal() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "echo '$(echo nope)'");
        let collected = run_to_completion(rt_id);

        let output = collected.get(&Some(h)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0));
        assert!(
            output.stdout.contains("$(echo nope)"),
            "stdout should contain literal '$(echo nope)': {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test multiple command substitutions in one line
    #[test]
    fn e2e_command_substitution_multiple() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        let h = cmd_create_safe(rt_id, "echo $(echo first) $(echo second) $(echo third)");
        let collected = run_to_completion(rt_id);

        let output = collected.get(&Some(h)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0));
        assert!(
            output.stdout.contains("first second third"),
            "stdout should contain 'first second third': {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h);
    }

    /// Test command substitution with file operations
    #[test]
    fn e2e_command_substitution_file_ops() {
        let (guard, _auth_guard) = setup();
        let rt_id = guard.id();

        // Write a filename to a file, then use it in command substitution
        let h1 = cmd_create_safe(rt_id, "echo myfile.txt > /workspace/filename.txt");
        run_to_completion(rt_id);
        cmd_delete_safe(rt_id, h1);

        let h2 = cmd_create_safe(
            rt_id,
            "echo content > /workspace/$(cat /workspace/filename.txt)",
        );
        run_to_completion(rt_id);
        cmd_delete_safe(rt_id, h2);

        // Verify the dynamically-named file was created
        let h3 = cmd_create_safe(rt_id, "cat /workspace/myfile.txt");
        let collected = run_to_completion(rt_id);

        let output = collected.get(&Some(h3)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0));
        assert!(
            output.stdout.contains("content"),
            "Dynamic file should contain 'content': {:?}",
            output.stdout
        );

        cmd_delete_safe(rt_id, h3);
    }

    // =========================================================================
    // Panic Recovery Tests
    // =========================================================================

    /// Test that a panic in one runtime doesn't affect others.
    #[test]
    fn panic_kills_only_one_runtime() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt1 = runtime_new_safe(&pca_bytes); // Will panic
        let rt2 = runtime_new_safe(&pca_bytes); // Should survive

        // Create commands - rt1 will panic, rt2 runs echo
        let _h1 = cmd_create_safe(rt1, "panic test message");
        let h2 = cmd_create_safe(rt2, "echo survivor");

        // Step rt1 - should panic and return panic status
        let resp1 = step_safe(rt1).expect("Should get panic response");
        assert!(
            resp1.status.is_panic(),
            "Expected panic status, got {:?}",
            resp1.status
        );

        // rt1 should be destroyed (step returns None)
        assert!(
            step_safe(rt1).is_none(),
            "rt1 should be destroyed after panic"
        );

        // rt2 should still work fine
        let collected = run_to_completion(rt2);
        let output = collected.get(&Some(h2)).expect("Should have output");
        assert_eq!(output.exit_code, Some(0), "rt2 should complete normally");
        assert!(
            output.stdout.contains("survivor"),
            "rt2 output should contain 'survivor'"
        );
    }

    /// Test that panic message is captured.
    #[test]
    fn panic_message_captured() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt = runtime_new_safe(&pca_bytes);

        cmd_create_safe(rt, "panic custom panic message here");

        let resp = step_safe(rt).expect("Should get panic response");
        match &resp.status {
            crate::types::RuntimeStatus::Panic { message } => {
                assert!(
                    message.contains("custom panic message"),
                    "Panic message should contain custom text: {message}"
                );
            }
            other => panic!("Expected Panic status, got {other:?}"),
        }
    }

    /// Test panic with default message.
    #[test]
    fn panic_default_message() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt = runtime_new_safe(&pca_bytes);

        cmd_create_safe(rt, "panic");

        let resp = step_safe(rt).expect("Should get panic response");
        match &resp.status {
            crate::types::RuntimeStatus::Panic { message } => {
                assert!(
                    message.contains("test panic"),
                    "Default panic message should be 'test panic': {message}"
                );
            }
            other => panic!("Expected Panic status, got {other:?}"),
        }
    }

    /// Test that runtime count decreases after panic.
    #[test]
    fn panic_decreases_runtime_count() {
        let (pca_bytes, _guard) = create_test_pca_bytes();
        let rt1 = runtime_new_safe(&pca_bytes);
        let rt2 = runtime_new_safe(&pca_bytes);

        assert_eq!(crate::runtime::runtime_count(), 2, "Should have 2 runtimes");

        // Panic in rt1
        cmd_create_safe(rt1, "panic");
        let _ = step_safe(rt1);

        assert_eq!(
            crate::runtime::runtime_count(),
            1,
            "Should have 1 runtime after panic"
        );

        // rt2 still works
        cmd_create_safe(rt2, "echo hello");
        let _ = run_to_completion(rt2);

        runtime_destroy_safe(rt2);
        assert_eq!(
            crate::runtime::runtime_count(),
            0,
            "Should have 0 runtimes after cleanup"
        );
    }

    // =========================================================================
    // PCA Create and Inspect Tests
    // =========================================================================

    /// Test `pca_create` creates valid PCAs.
    #[test]
    fn test_pca_create() {
        // Generate a key
        crate::keys::clear_keys();
        let key_id = "test-pca-key";
        let seed = [42u8; 32];
        let public_hex = crate::keys::key_generate_from_seed(key_id, &seed).unwrap();

        // Set up trusted authority
        let public_key = amla_protocol::PublicKey::from_hex(&public_hex).unwrap();
        let _guard = TrustedAuthoritiesGuard::new(vec![public_key.clone()]);

        // Create PCA using WASM export
        let key_id_bytes = key_id.as_bytes();
        let caps_json = r#"["stripe:charge", "notion:search"]"#;
        let caps_bytes = caps_json.as_bytes();
        // Deadline: year 2033 in nanoseconds (2_000_000_000 seconds * 1_000_000_000 ns)
        let deadline_ns = 2_000_000_000_000_000_000u64;

        let mut out_buf = vec![0u8; 4096];
        let pca_len = pca_create(
            key_id_bytes.as_ptr(),
            key_id_bytes.len(),
            caps_bytes.as_ptr(),
            caps_bytes.len(),
            deadline_ns,
            out_buf.as_mut_ptr(),
            out_buf.len(),
        );

        assert!(pca_len > 0, "pca_create should return non-zero length");

        // Verify we can parse the PCA
        let pca_bytes = &out_buf[..pca_len];
        let pca = amla_protocol::Pca::from_cbor(pca_bytes).expect("Should parse created PCA");

        assert_eq!(pca.issuer().to_hex(), public_hex);
        assert!(pca.try_verify_signature().is_ok());
        assert_eq!(pca.capabilities().len(), 2);
    }

    /// Test `pca_inspect` returns correct fields.
    #[test]
    fn test_pca_inspect() {
        // Generate a key and create PCA
        crate::keys::clear_keys();
        let key_id = "test-inspect-key";
        let seed = [43u8; 32];
        let public_hex = crate::keys::key_generate_from_seed(key_id, &seed).unwrap();

        let public_key = amla_protocol::PublicKey::from_hex(&public_hex).unwrap();
        let _guard = TrustedAuthoritiesGuard::new(vec![public_key]);

        // Create PCA
        let key_id_bytes = key_id.as_bytes();
        let caps_json = r#"["stripe:charge"]"#;
        let caps_bytes = caps_json.as_bytes();
        // Deadline: year 2033 in nanoseconds
        let deadline_ns = 2_000_000_000_000_000_000u64;

        let mut pca_buf = vec![0u8; 4096];
        let pca_len = pca_create(
            key_id_bytes.as_ptr(),
            key_id_bytes.len(),
            caps_bytes.as_ptr(),
            caps_bytes.len(),
            deadline_ns,
            pca_buf.as_mut_ptr(),
            pca_buf.len(),
        );
        assert!(pca_len > 0);

        // Now inspect the PCA
        let mut inspect_buf = vec![0u8; 4096];
        let inspect_len = pca_inspect(
            pca_buf.as_ptr(),
            pca_len,
            inspect_buf.as_mut_ptr(),
            inspect_buf.len(),
        );

        assert!(inspect_len > 0, "pca_inspect should return non-zero length");

        // Parse the JSON response
        let json_str = std::str::from_utf8(&inspect_buf[..inspect_len]).unwrap();
        let inspect: serde_json::Value = serde_json::from_str(json_str).unwrap();

        assert_eq!(inspect["issuer"].as_str().unwrap(), &public_hex);
        assert!(inspect["signature_valid"].as_bool().unwrap());

        let capabilities = inspect["capabilities"].as_array().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].as_str().unwrap(), "stripe:charge");

        // expires_at should be a valid timestamp
        let expires_at = inspect["expires_at"].as_str().unwrap();
        assert!(
            expires_at.contains('T'),
            "expires_at should be RFC3339 format"
        );
    }

    /// Test `pca_inspect` with invalid PCA returns error.
    #[test]
    fn test_pca_inspect_invalid() {
        let invalid_pca = b"not valid cbor";
        let mut out_buf = vec![0u8; 1024];

        let len = pca_inspect(
            invalid_pca.as_ptr(),
            invalid_pca.len(),
            out_buf.as_mut_ptr(),
            out_buf.len(),
        );

        assert_eq!(len, 0, "pca_inspect should return 0 for invalid PCA");
    }

    /// Test `pca_create` with non-existent key.
    #[test]
    fn test_pca_create_key_not_found() {
        crate::keys::clear_keys();

        let key_id = b"nonexistent-key";
        let caps = b"[]";
        let mut out_buf = vec![0u8; 1024];
        let deadline_ns = 2_000_000_000_000_000_000u64;

        let len = pca_create(
            key_id.as_ptr(),
            key_id.len(),
            caps.as_ptr(),
            caps.len(),
            deadline_ns,
            out_buf.as_mut_ptr(),
            out_buf.len(),
        );

        assert_eq!(len, 0, "pca_create should return 0 for non-existent key");
    }
}
