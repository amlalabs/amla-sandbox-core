//! Build script for amla-sandbox.
//!
//! Currently empty - memcmp/bcmp are now provided as Rust implementations
//! directly in wasm.rs with `#[no_mangle]` to override the broken
//! `compiler_builtins` versions.

fn main() {
    // Nothing to do - Rust implementations in wasm.rs handle memcmp/bcmp
}
