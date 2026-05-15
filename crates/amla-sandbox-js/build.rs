//! Build script for amla-js.
//!
//! Compiles `QuickJS` C source into a static library that gets linked
//! into the final binary. Works for both native and WASM targets.
//!
//! ## Target support
//!
//! - **Native** (`x86_64`, `aarch64`, etc.): Uses system cc/clang via `cc` crate
//! - **wasm32-wasip1**: Uses WASI SDK clang with wasi-sysroot
//!
//! The `cc` crate automatically detects the target and uses the appropriate compiler.

fn main() {
    compile_quickjs();
}

fn compile_quickjs() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let manifest_path = std::path::PathBuf::from(&manifest_dir);
    let vendor_dir = manifest_path.join("vendor/quickjs");
    let wrapper_dir = manifest_path.join("quickjs");

    // Determine target and profile for conditional compilation
    let target = std::env::var("TARGET").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let is_wasi = target.contains("wasi");
    let is_release = profile == "release";

    let mut build = cc::Build::new();

    // Core QuickJS files (from vendor)
    build
        .file(vendor_dir.join("quickjs.c"))
        .file(vendor_dir.join("libregexp.c"))
        .file(vendor_dir.join("libunicode.c"))
        .file(vendor_dir.join("cutils.c"))
        .file(vendor_dir.join("dtoa.c"))
        // Our wrapper (separate from vendor)
        .file(wrapper_dir.join("wrapper.c"))
        .include(&vendor_dir)
        .include(&wrapper_dir);

    // Common defines
    build
        .define("_GNU_SOURCE", None)
        .define("CONFIG_VERSION", Some("\"2025-09-13\""));

    // Disable asserts for WASM (cause unreachable trap) and release builds.
    // Native debug builds keep asserts for better diagnostics.
    if is_wasi || is_release {
        build.define("NDEBUG", None);
    }

    // Optimization
    build.opt_level(2);

    // Suppress QuickJS warnings (it has many)
    build.warnings(false);

    // Target-specific configuration
    if is_wasi {
        // wasm32-wasip1 - Use WASI SDK for proper libc headers
        // WASI SDK provides headers; wasi-libc (via Rust std) provides implementations
        // Pin WASI SDK version in CI for deterministic builds
        let wasi_sdk = std::env::var("WASI_SDK_PATH").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}/.local/wasi-sdk")
        });
        if std::path::Path::new(&wasi_sdk).exists() {
            build.compiler(format!("{wasi_sdk}/bin/clang"));
            build.flag(format!("--sysroot={wasi_sdk}/share/wasi-sysroot"));
            // Enable exception handling for setjmp (used by dtoa.c)
            build.flag("-mllvm");
            build.flag("-wasm-enable-sjlj");
        } else {
            panic!(
                "WASI SDK not found. Install it or set WASI_SDK_PATH.\n\
                 Download from: https://github.com/WebAssembly/wasi-sdk/releases"
            );
        }
        // EMSCRIPTEN define tells QuickJS to disable features not available in WASM:
        // - CONFIG_ATOMICS (requires pthreads, not available in WASI)
        // - CONFIG_STACK_CHECK (not needed in sandboxed WASM)
        // - DIRECT_DISPATCH (goto computed labels, may not work in WASM)
        // Note: This is a QuickJS convention, not related to using Emscripten compiler.
        // __wasm__ and __wasi__ are auto-defined by the compiler.
        build.define("EMSCRIPTEN", None);
    }
    // Native build uses default cc configuration

    // Compile
    build.compile("quickjs");

    // Rerun if sources change
    println!("cargo:rerun-if-changed=vendor/quickjs/");
    println!("cargo:rerun-if-changed=quickjs/wrapper.c");
    println!("cargo:rerun-if-changed=quickjs/wrapper.h");
}
