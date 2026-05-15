//! Time helper for all platforms (native and WASI).
//!
//! For WASI builds, chrono uses WASI's `clock_time_get` syscall.
//! The host controls time by intercepting this WASI import.

/// Get current timestamp in milliseconds since Unix epoch.
///
/// Works on native and WASI - chrono uses WASI `clock_time_get` on WASM.
pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
