# amla-sandbox-core

WASM sandbox with capability enforcement for secure code execution. This is the Rust core that powers [amla-sandbox](https://github.com/amlalabs/amla-sandbox), the Python package.

## Crates

| Crate | Description |
|-------|-------------|
| `amla-sandbox` | Main integration crate, WASM exports |
| `amla-js` | QuickJS runtime bindings |
| `amla-shell` | Shell implementation (pipes, builtins) |
| `amla-vfs` | In-memory virtual filesystem |
| `amla-scheduler` | Single-threaded async executor |
| `amla-tools` | Tool catalog with BM25/semantic search |
| `amla-audit` | Structured audit logging |

## Features

- **WASM isolation** - Runs in WebAssembly with memory safety guarantees
- **Capability enforcement** - Every tool call validated against capabilities
- **Virtual filesystem** - Sandboxed VFS with configurable permissions
- **Shell pipelines** - Built-in shell with `grep`, `jq`, `sort`, etc.
- **Async scheduling** - Non-blocking tool calls with cooperative multitasking

## Building

```bash
# Native build
cargo build

# WASM build (requires wasm32-wasip1 target)
rustup target add wasm32-wasip1
cargo build --release -p amla-sandbox --target wasm32-wasip1
```

## Architecture

```
┌────────────────────────────────────────────────┐
│              WASM Sandbox                      │
│  ┌──────────────────────────────────────────┐  │
│  │         Async Scheduler                  │  │
│  │   tasks waiting/running/ready            │  │
│  └──────────────────────────────────────────┘  │
│  ┌────────────┐ ┌──────────┐ ┌──────────────┐  │
│  │  VFS       │ │ Shell    │ │ Capabilities │  │
│  │ /workspace │ │ builtins │ │ validation   │  │
│  └────────────┘ └──────────┘ └──────────────┘  │
│                    ↓ yield                     │
└════════════════════════════════════════════════┘
                     │
                     ▼
┌─────────────────────────────────────────────┐
│              Host Runtime                   │
│                                             │
│   while sandbox.has_work():                 │
│       req = sandbox.step()  # tool call     │
│       sandbox.resume(execute(req))          │
│                                             │
└─────────────────────────────────────────────┘
```

## Dependencies

This crate depends on [amla-protocol](https://github.com/amlalabs/amla-protocol) for capability definitions and the PCA token format.

## License

AGPL-3.0-or-later OR BUSL-1.1
