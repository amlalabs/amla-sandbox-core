#!/usr/bin/env node
/**
 * WASM Benchmark Harness using Minimal WASI Shim.
 *
 * ## Why a Minimal WASI Shim?
 *
 * The amla runtime is a SANDBOXED execution environment. We use a minimal
 * WASI shim instead of Node's node:wasi because:
 *
 * 1. **Sandbox Enforcement**: The runtime has its own VFS - it shouldn't
 *    access real files. Blocking fd_read/path_open enforces this.
 *
 * 2. **Controlled Time/Random**: The host provides time and randomness,
 *    enabling deterministic replay and testing.
 *
 * 3. **No Environment Leakage**: Environment variables are controlled via
 *    PCA/Host Ops, not leaked from the host process.
 *
 * The only WASI syscalls we implement:
 * - clock_time_get: Controlled time for scheduling
 * - random_get: Controlled randomness for IDs/crypto
 * - fd_write (stderr): For panic messages during development
 *
 * Everything else returns ERRNO_NOSYS to enforce the sandbox boundary.
 *
 * Prerequisites:
 *   cargo build --release -p amla-sandbox --target wasm32-wasip1
 *
 * Run with: node benches/wasm_harness.mjs
 */

import { readFileSync, existsSync } from "fs";
import { fileURLToPath } from "url";
import { dirname, join } from "path";
import { createRuntimeWithPca } from "../../amla-sandbox/tests/pca_utils.mjs";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

const WARMUP_ITERATIONS = 5;
const BENCHMARK_ITERATIONS = 50;

// =============================================================================
// WASI Error Codes
// =============================================================================

const ERRNO_SUCCESS = 0;
const ERRNO_BADF = 8;     // Bad file descriptor - used for blocked fd ops
const ERRNO_NOSYS = 52;   // Function not supported - used for blocked syscalls

// =============================================================================
// Minimal WASI Shim
// =============================================================================

/**
 * Create a minimal WASI implementation for the sandbox.
 *
 * This provides ONLY the syscalls the runtime legitimately needs:
 * - clock_time_get: For timestamps (host-controlled for determinism)
 * - random_get: For IDs and crypto (host-controlled for determinism)
 * - fd_write: For panic messages to stderr (could be blocked in production)
 *
 * All other WASI imports are blocked - the runtime uses VFS for files
 * and Host Ops for I/O. Real WASI file access would break the sandbox.
 */
function createMinimalWasi({ getTimeNanos, getRandomBytes, onStderr }) {
  let memory = null;

  return {
    setMemory(mem) { memory = mem; },

    wasi_snapshot_preview1: {
      // Time - required for scheduling, expiry checks
      clock_time_get(clock_id, precision, out_ptr) {
        if (!memory) return ERRNO_NOSYS;
        const view = new DataView(memory.buffer);
        view.setBigUint64(out_ptr, BigInt(getTimeNanos(clock_id)), true);
        return ERRNO_SUCCESS;
      },

      // Random - required for IDs and crypto operations
      random_get(buf_ptr, buf_len) {
        if (!memory) return ERRNO_NOSYS;
        new Uint8Array(memory.buffer, buf_ptr, buf_len).set(getRandomBytes(buf_len));
        return ERRNO_SUCCESS;
      },

      // Stderr only - for panic messages during development
      fd_write(fd, iovs_ptr, iovs_len, nwritten_ptr) {
        if (!memory) return ERRNO_NOSYS;
        if (fd !== 2) return ERRNO_BADF; // Only stderr

        const view = new DataView(memory.buffer);
        const mem = new Uint8Array(memory.buffer);
        let totalWritten = 0;

        for (let i = 0; i < iovs_len; i++) {
          const ptr = view.getUint32(iovs_ptr + i * 8, true);
          const len = view.getUint32(iovs_ptr + i * 8 + 4, true);
          onStderr(new TextDecoder().decode(mem.slice(ptr, ptr + len)));
          totalWritten += len;
        }

        view.setUint32(nwritten_ptr, totalWritten, true);
        return ERRNO_SUCCESS;
      },

      // BLOCKED: File I/O - runtime uses VFS, not real files
      fd_read() { return ERRNO_NOSYS; },
      fd_close() { return ERRNO_NOSYS; },
      fd_seek() { return ERRNO_NOSYS; },
      fd_tell() { return ERRNO_NOSYS; },
      fd_filestat_get() { return ERRNO_NOSYS; },
      fd_fdstat_get() { return ERRNO_NOSYS; },
      fd_fdstat_set_flags() { return ERRNO_NOSYS; },
      path_open() { return ERRNO_NOSYS; },
      path_filestat_get() { return ERRNO_NOSYS; },

      // BLOCKED: Preopens - sandbox doesn't expose host filesystem
      fd_prestat_get() { return ERRNO_BADF; },
      fd_prestat_dir_name() { return ERRNO_BADF; },

      // BLOCKED: Environment - config via PCA/Host Ops, not env vars
      environ_get() { return ERRNO_SUCCESS; },
      environ_sizes_get(count_ptr, size_ptr) {
        if (!memory) return ERRNO_NOSYS;
        const view = new DataView(memory.buffer);
        view.setUint32(count_ptr, 0, true);
        view.setUint32(size_ptr, 0, true);
        return ERRNO_SUCCESS;
      },

      // Harmless no-ops
      sched_yield() { return ERRNO_SUCCESS; },

      // Trap on exit - don't let WASM silently terminate
      proc_exit(code) { throw new Error(`WASM proc_exit(${code})`); },
    },
  };
}

// =============================================================================
// WASI Runtime Wrapper
// =============================================================================

class WasiRuntime {
  constructor(instance, memory, wasi, runtimeId) {
    this.instance = instance;
    this.memory = memory;
    this.wasi = wasi;
    this.runtimeId = runtimeId;
    this.encoder = new TextEncoder();
    this.decoder = new TextDecoder();
    this.heapBase = 10 * 1024 * 1024;
    this.currentPtr = this.heapBase;
  }

  static async create(wasmPath) {
    const wasmBytes = readFileSync(wasmPath);
    const module = await WebAssembly.compile(wasmBytes);

    // Create minimal WASI with controlled time/random
    let randomCounter = 0;
    const wasi = createMinimalWasi({
      getTimeNanos: () => BigInt(Date.now()) * 1_000_000n,
      getRandomBytes: (len) => {
        const bytes = new Uint8Array(len);
        for (let i = 0; i < len; i++) {
          bytes[i] = (randomCounter++ * 1103515245 + 12345) & 0xFF;
        }
        return bytes;
      },
      onStderr: (text) => process.stderr.write(text),
    });

    const instance = await WebAssembly.instantiate(module, wasi);
    wasi.setMemory(instance.exports.memory);

    const rt = new WasiRuntime(instance, instance.exports.memory, wasi, 0);

    // Create runtime with real Ed25519-signed PCA
    const { rtId } = createRuntimeWithPca(instance.exports, instance.exports.memory);
    rt.runtimeId = rtId;

    return rt;
  }

  alloc(size, align = 8) {
    const aligned = (this.currentPtr + align - 1) & ~(align - 1);
    this.currentPtr = aligned + size;
    if (this.currentPtr > this.memory.buffer.byteLength) {
      const pages = Math.ceil((this.currentPtr - this.memory.buffer.byteLength) / 65536) + 4;
      this.memory.grow(pages);
    }
    return aligned;
  }

  getMemory() {
    return new Uint8Array(this.memory.buffer);
  }

  runCommand(command) {
    const cmdBytes = this.encoder.encode(command);
    const cmdPtr = this.alloc(cmdBytes.length + 16);
    const outPtr = this.alloc(64 * 1024);

    this.getMemory().set(cmdBytes, cmdPtr);
    const handle = this.instance.exports.cmd_create(this.runtimeId, cmdPtr, cmdBytes.length);
    if (handle === 0n) {
      throw new Error(`Failed to create command: ${command}`);
    }

    let steps = 0;

    try {
      for (let step = 0; step < 1000; step++) {
        const written = this.instance.exports.runtime_step(this.runtimeId, outPtr, 64 * 1024);
        steps++;
        if (written === 0) break;

        const response = JSON.parse(this.decoder.decode(
          new Uint8Array(this.memory.buffer, outPtr, Number(written))
        ));

        if (response.host_ops) {
          const results = response.host_ops.map(op => {
            const req = op.request;
            if (req.type === "output") return { id: op.id, response: { output_ack: {} } };
            if (req.type === "command_exit") return { id: op.id, response: { exit_ack: {} } };
            if (req.type === "read_stdin") return { id: op.id, response: { stdin_data: { eof: true } } };
            if (req.tool_call) return { id: op.id, response: { tool_result: { success: { value: null } } } };
            return { id: op.id, response: {} };
          });
          const json = this.encoder.encode(JSON.stringify(results));
          const respPtr = this.alloc(json.length + 16);
          this.getMemory().set(json, respPtr);
          this.instance.exports.submit(respPtr, json.length);
        }

        if (response.status === "all_done") break;
      }
    } finally {
      this.instance.exports.cmd_delete(this.runtimeId, handle);
    }

    return steps;
  }

  destroy() {
    if (this.runtimeId !== 0) {
      this.instance.exports.runtime_destroy(this.runtimeId);
      this.runtimeId = 0;
    }
  }
}

// =============================================================================
// Benchmark Harness
// =============================================================================

function benchmark(name, fn, iterations = BENCHMARK_ITERATIONS) {
  for (let i = 0; i < WARMUP_ITERATIONS; i++) fn();
  if (global.gc) global.gc();

  const start = performance.now();
  for (let i = 0; i < iterations; i++) fn();
  const end = performance.now();

  const totalMs = end - start;
  const perIterUs = (totalMs * 1000) / iterations;
  const opsPerSec = Math.round(iterations / (totalMs / 1000));

  return { name, totalMs, perIterUs, opsPerSec, iterations };
}

function formatResult(result) {
  return `${result.name.padEnd(40)} ${result.perIterUs.toFixed(3).padStart(12)} µs/iter  (${result.opsPerSec.toLocaleString().padStart(10)} ops/sec)`;
}

function findWasmBinary(explicitPath) {
  // If explicit path provided, use it
  if (explicitPath) {
    if (existsSync(explicitPath)) return explicitPath;
    console.error(`WASM binary not found at: ${explicitPath}`);
    process.exit(1);
  }

  // Otherwise try default paths
  const paths = [
    join(__dirname, "..", "..", "target", "wasm32-wasip1", "release", "amla_sandbox.wasm"),
    join(__dirname, "..", "..", "target", "wasm32-wasip1", "debug", "amla_sandbox.wasm"),
  ];
  for (const path of paths) {
    if (existsSync(path)) return path;
  }
  return null;
}

// =============================================================================
// Main
// =============================================================================

async function main() {
  console.log("=".repeat(70));
  console.log("AMLA Runtime WASM Benchmark (Minimal WASI Shim)");
  console.log("=".repeat(70));
  console.log(`Node.js ${process.version}`);
  console.log(`Iterations: ${BENCHMARK_ITERATIONS} (warmup: ${WARMUP_ITERATIONS})`);
  console.log("=".repeat(70));
  console.log();

  const wasmPath = findWasmBinary(process.argv[2]);
  if (!wasmPath) {
    console.log("WASM binary not found.");
    console.log("Usage: node wasm_harness.mjs <path-to-wasm>");
    console.log("Or build with: cargo build --release -p amla-sandbox --target wasm32-wasip1");
    process.exit(1);
  }

  console.log(`WASM binary: ${wasmPath}`);
  console.log();

  let runtime;
  try {
    runtime = await WasiRuntime.create(wasmPath);
    console.log("Runtime loaded with minimal WASI shim!");
    console.log();
  } catch (e) {
    console.error("Failed to load runtime:", e.message);
    process.exit(1);
  }

  const results = [];

  // Arithmetic Benchmarks
  console.log("## Arithmetic");
  results.push(benchmark("wasm/arithmetic/simple_add", () => runtime.runCommand("1 + 2 + 3 + 4 + 5")));
  results.push(benchmark("wasm/arithmetic/complex_expr", () => runtime.runCommand("(1 + 2) * 3 / 4 - 5 + Math.pow(2, 10) + Math.sqrt(144)")));
  results.push(benchmark("wasm/arithmetic/loop_100", () => runtime.runCommand("let sum = 0; for (let i = 0; i < 100; i++) sum += i; sum")));
  results.slice(-3).forEach(r => console.log(formatResult(r)));
  console.log();

  // String Benchmarks
  console.log("## Strings");
  results.push(benchmark("wasm/strings/concat_10", () => runtime.runCommand("'a' + 'b' + 'c' + 'd' + 'e' + 'f' + 'g' + 'h' + 'i' + 'j'")));
  results.push(benchmark("wasm/strings/template_literal", () => runtime.runCommand("const name = 'World'; const count = 42; `Hello ${name}, count is ${count}`")));
  results.push(benchmark("wasm/strings/split_join", () => runtime.runCommand("'hello,world,foo,bar,baz'.split(',').join('-')")));
  results.slice(-3).forEach(r => console.log(formatResult(r)));
  console.log();

  // Array Benchmarks
  console.log("## Arrays");
  results.push(benchmark("wasm/arrays/create_100", () => runtime.runCommand("new Array(100).fill(0)")));
  results.push(benchmark("wasm/arrays/map_50", () => runtime.runCommand("new Array(50).fill(0).map((_, i) => i * 2)")));
  results.push(benchmark("wasm/arrays/reduce_50", () => runtime.runCommand("new Array(50).fill(0).map((_, i) => i).reduce((a, b) => a + b, 0)")));
  results.slice(-3).forEach(r => console.log(formatResult(r)));
  console.log();

  // Object Benchmarks
  console.log("## Objects");
  results.push(benchmark("wasm/objects/create_literal", () => runtime.runCommand("({ a: 1, b: 2, c: 3, d: 4, e: 5 })")));
  results.push(benchmark("wasm/objects/json_parse", () => runtime.runCommand('JSON.parse(\'{"a": 1, "b": [1, 2, 3]}\')')));
  results.push(benchmark("wasm/objects/json_roundtrip", () => runtime.runCommand("JSON.parse(JSON.stringify({ a: 1, b: [1, 2, 3] }))")));
  results.slice(-3).forEach(r => console.log(formatResult(r)));
  console.log();

  // Function Benchmarks
  console.log("## Functions");
  results.push(benchmark("wasm/functions/define_and_call", () => runtime.runCommand("function add(a, b) { return a + b; } add(1, 2)")));
  results.push(benchmark("wasm/functions/arrow_fn", () => runtime.runCommand("const double = x => x * 2; double(21)")));
  results.push(benchmark("wasm/functions/fib_10", () => runtime.runCommand("function fib(n) { if (n <= 1) return n; return fib(n - 1) + fib(n - 2); } fib(10)"), 20));
  results.slice(-3).forEach(r => console.log(formatResult(r)));
  console.log();

  // Agent Workload
  console.log("## Agent Workload");
  results.push(benchmark("wasm/agent/state_update", () => runtime.runCommand("const state = { messages: [], context: { user: 'test' } }; state.messages.push({ role: 'user', content: 'Hello' }); state.messages.length")));
  results.push(benchmark("wasm/agent/json_processing", () => runtime.runCommand(`const llmResponse = { choices: [{ message: { role: 'assistant', content: 'Answer', tool_calls: [{ id: '1', function: { name: 'search', arguments: '{"query":"test"}' } }] } }] }; const tc = llmResponse.choices[0].message.tool_calls[0]; JSON.parse(tc.function.arguments)`)));
  results.push(benchmark("wasm/agent/tool_call", () => runtime.runCommand("__amla__.toolCall('test:tool', { param: 'value' })")));
  results.slice(-3).forEach(r => console.log(formatResult(r)));
  console.log();

  // Summary
  console.log("=".repeat(70));
  console.log("Summary");
  console.log("=".repeat(70));
  results.forEach(r => console.log(formatResult(r)));

  runtime.destroy();
}

main().catch(e => {
  console.error("Benchmark failed:", e);
  process.exit(1);
});
