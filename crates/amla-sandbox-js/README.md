# amla-js

QuickJS JavaScript engine compiled to WASM for sandboxed agent execution.

## Why QuickJS

We need JavaScript execution inside a WASM sandbox. The options:

| Engine | WASM-compatible | Size | Performance |
|--------|-----------------|------|-------------|
| V8 | No (too complex) | - | 1x baseline |
| SpiderMonkey | No (too complex) | - | ~1x |
| JavaScriptCore | No (too complex) | - | ~1x |
| QuickJS | Yes | ~2MB | ~30x slower |
| Duktape | Yes | ~300KB | ~50x slower |

QuickJS wins: small, embeddable, ES2020 compliant, maintained by Fabrice Bellard.

## Performance

Benchmarked against V8 (Node.js) on JSON parsing workloads:

| Payload | V8 | QuickJS (WASM) | Ratio |
|---------|-----|----------------|-------|
| 1 object | 0.5µs | 0.87ms | 1666x (overhead-bound) |
| 100 objects | 12µs | 1.5ms | 125x |
| 1000 objects | 112µs | 4.3ms | 38x |

**What the delta shows:**

```
V8:   0.5µs → 112µs = +111µs actual parse work
WASM: 0.87ms → 4.3ms = +3.4ms actual parse work
Ratio of actual work: ~30x
```

The first call pays ~500-900µs of runtime initialization overhead. After that, QuickJS is ~30x slower than V8 for compute.

```
┌─────────────────────────────────────────────────────────┐
│  ACTUAL QuickJS vs V8 Performance: ~30x slower         │
│  Runtime overhead: ~500-900µs (paid once per runtime)  │
│                                                        │
│  For real agent workloads: TOTALLY ACCEPTABLE          │
└─────────────────────────────────────────────────────────┘
```

**Why this doesn't matter for agents:**

- LLM inference: 500ms - 5s
- Network calls: 50ms - 500ms
- JS computation: 1ms → 30ms

Turning 1ms of JS into 30ms is noise when the LLM call took 2 seconds.

## Architecture

```
┌────────────────────────────────────────────────────────────┐
│                      Rust (amla-js)                        │
│  ┌──────────────────────────────────────────────────────┐  │
│  │                   JsRuntime                          │  │
│  │  ┌────────────────┐  ┌────────────────────────────┐  │  │
│  │  │ QuickJsEngine  │  │     Pending Ops Queue      │  │  │
│  │  │ (FFI wrapper)  │  │  Vec<PendingOp>            │  │  │
│  │  └───────┬────────┘  └────────────────────────────┘  │  │
│  │          │                        ▲                   │  │
│  │          ▼                        │                   │  │
│  │  ┌────────────────────────────────┴───────────────┐  │  │
│  │  │              FFI Bridge (ffi.rs)               │  │  │
│  │  │  __native_register_op() → pushes to queue      │  │  │
│  │  │  __native_console()     → captures output      │  │  │
│  │  └────────────────────────────────────────────────┘  │  │
│  └──────────────────────────────────────────────────────┘  │
│                           │                                │
│                           ▼                                │
│  ┌──────────────────────────────────────────────────────┐  │
│  │              QuickJS (C, compiled to WASM)           │  │
│  │  ┌────────────────┐  ┌────────────────────────────┐  │  │
│  │  │   JS Context   │  │      __amla__ global       │  │  │
│  │  │   (ES2020)     │  │  toolCall(), fetch(), ...  │  │  │
│  │  └────────────────┘  └────────────────────────────┘  │  │
│  └──────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────┘
```

### Async Operation Flow

JavaScript async operations (tool calls, fetch, etc.) yield to the host:

```
┌─────────────────────────┐    ┌─────────────────────────┐
│      JS Runtime         │    │         Host            │
│                         │    │                         │
│  await toolCall("x")────┼───►│                         │
│         │               │    │                         │
│         ▼               │    │                         │
│  1. Create Promise      │    │                         │
│  2. __native_register_op│    │                         │
│  3. Return to execute() │    │                         │
│         │               │    │                         │
│  execute() returns ─────┼───►│  4. Receive pending_ops │
│                         │    │         │               │
│                         │    │         ▼               │
│                         │    │  5. Do async I/O        │
│                         │    │         │               │
│                         │◄───┼─  6. resolve(id, result)│
│  7. _resolve(id, result)│    │                         │
│  8. Run Promise.then()  │    │                         │
│         │               │    │                         │
│  (may create more ops)──┼───►│  9. Check for more ops  │
└─────────────────────────┘    └─────────────────────────┘
```

This stepping protocol enables:

- Concurrent tool calls via `Promise.all()`
- Host-controlled pacing and timeouts
- Deterministic replay (host provides all async results)

## Usage

### Basic Execution

```rust
use amla_js::JsRuntime;

let mut runtime = JsRuntime::new();

// Synchronous JS
let result = runtime.execute("1 + 2 * 3").unwrap();
assert_eq!(result.value, serde_json::json!(7));

// Console output captured
let result = runtime.execute("console.log('hello')").unwrap();
assert!(result.console_output[0].message.contains("hello"));

// Functions, closures, ES2020 features
let result = runtime.execute(r#"
    const add = (a, b) => a + b;
    const nums = [1, 2, 3];
    nums.map(n => add(n, 10));
"#).unwrap();
assert_eq!(result.value, serde_json::json!([11, 12, 13]));
```

### Async Operations

```rust
use amla_js::{JsRuntime, OpType};

let mut runtime = JsRuntime::new();

// Start async operation
let result = runtime.execute(r#"
    __amla__.toolCall("stripe:charge", {amount: 1000, currency: "USD"})
"#).unwrap();

// Host receives pending ops
assert_eq!(result.pending_ops.len(), 1);
let op = &result.pending_ops[0];

if let OpType::ToolCall { tool, params } = &op.op_type {
    assert_eq!(tool, "stripe:charge");

    // Host executes the operation
    let tool_result = serde_json::json!({"id": "ch_123", "status": "succeeded"});

    // Return result to JS
    runtime.resolve(&op.id, &tool_result).unwrap();
}

// Continue execution (runs Promise continuations)
runtime.run_pending_jobs().unwrap();
```

### Promise Chains

```rust
let result = runtime.execute(r#"
    globalThis.finalResult = null;

    __amla__.toolCall("first:call", {})
        .then(r => __amla__.toolCall("second:call", {prev: r}))
        .then(r => { globalThis.finalResult = r; });
"#).unwrap();

// First op
let op1_id = result.pending_ops[0].id.clone();
runtime.resolve(&op1_id, &serde_json::json!({"step": 1})).unwrap();

// Second op appears after first resolves
let ops = runtime.take_pending_ops();
assert_eq!(ops.len(), 1);
runtime.resolve(&ops[0].id, &serde_json::json!({"step": 2})).unwrap();

// Check final result
let final_result = runtime.execute("globalThis.finalResult").unwrap();
assert_eq!(final_result.value["step"], 2);
```

### Configuration

```rust
use amla_js::{JsRuntime, EngineConfig};

let config = EngineConfig {
    memory_limit: Some(64 * 1024 * 1024),  // 64MB
    stack_size: Some(512 * 1024),           // 512KB stack
    ..Default::default()
};

let runtime = JsRuntime::with_config(config).unwrap();
```

## JS Global API

The `__amla__` global provides the agent interface:

```javascript
// Tool calls (yield to host for execution)
const result = await __amla__.toolCall("stripe:charge", {amount: 100});

// HTTP fetch (routed through host)
const response = await __amla__.fetch("https://api.example.com/data");

// Partitioned memory (scoped to transaction)
await __amla__.memoryWrite("user/prefs", {theme: "dark"});
const prefs = await __amla__.memoryRead("user/prefs");
await __amla__.memoryDelete("user/prefs");

// Spawn child with attenuated capabilities
const child = await __amla__.spawn([{
    capability: "stripe:charge",
    constraints: [
        Param("amount").le(5000),
        Param("currency").eq("USD")
    ]
}]);

// LLM calls
const response = await __amla__.llm("gpt-4", [
    {role: "user", content: "Hello!"}
], {temperature: 0.7});

// Synchronous logging
__amla__.log("info", "Processing started");
```

## Operation Types

```rust
pub enum OpType {
    ToolCall { tool: String, params: Value },
    Fetch { url: String, options: FetchOptions },
    MemoryRead { key: String },
    MemoryWrite { key: String, value: Value },
    MemoryDelete { key: String },
    Spawn { attenuations: Vec<AttenuationSpec> },
    LlmCall { model: String, messages: Vec<LlmMessage>, options: LlmOptions },
}
```

All operations yield to the host. The host decides whether to execute, deny, or modify the request based on capabilities.

## QuickJS Integration

QuickJS is vendored in `vendor/quickjs/` and compiled via `build.rs`:

```
amla-js/
├── src/
│   ├── lib.rs          # Public API
│   ├── runtime.rs      # JsRuntime wrapper
│   ├── engine.rs       # QuickJsEngine (low-level)
│   ├── ffi.rs          # C FFI bindings
│   ├── globals.rs      # __amla__ JS source
│   └── ops.rs          # PendingOp types
├── quickjs/
│   ├── wrapper.c       # Rust↔QuickJS bridge
│   └── wrapper.h
├── vendor/
│   └── quickjs/        # QuickJS source (MIT licensed)
└── build.rs            # Compiles QuickJS to WASM
```

### FFI Functions

```c
// wrapper.c - Rust calls these
JSContext* qjs_new_context(void);
int qjs_eval(JSContext* ctx, const char* code, char** result);
void qjs_resolve_promise(JSContext* ctx, const char* op_id, const char* value);
void qjs_reject_promise(JSContext* ctx, const char* op_id, const char* error);

// Called from JS via __native_*
void __native_register_op(const char* op_json);  // Pushes to pending queue
void __native_console(const char* level, const char* message);  // Captures output
```

## Features

```toml
[features]
default = []
# All features currently compiled in; future: optional QuickJS
```

## Building

```bash
# Native (for testing)
cargo build -p amla-js

# WASM (for sandbox)
cargo build -p amla-js --target wasm32-wasip1

# Run tests
cargo test -p amla-js

# Run benchmarks (requires Node.js for V8 comparison)
cd benches && ./run_comparison.sh
```

## Memory Safety

- QuickJS runs in WASM linear memory (sandboxed)
- No direct memory access from JS to Rust heap
- All data crosses FFI as JSON strings (serialization boundary)
- JS cannot escape the WASM sandbox

## License

QuickJS is MIT licensed (Fabrice Bellard).
This crate is AGPL-3.0-or-later OR BUSL-1.1.
