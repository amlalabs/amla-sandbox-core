#!/usr/bin/env node
/**
 * Benchmark: V8 vs WASM QuickJS (full execution)
 *
 * Usage: node benches/compare_all.mjs [path-to-wasm]
 *
 * This benchmark actually executes JavaScript code through:
 * 1. V8 (Node.js) - Direct JIT-compiled execution
 * 2. WASM QuickJS - Full runtime stepping loop with host ops
 *
 * Prerequisites:
 *   cargo build --release -p amla-sandbox --target wasm32-wasip1
 */

import { readFileSync, existsSync } from "fs";
import { fileURLToPath } from "url";
import { dirname, join } from "path";
import { createRuntimeWithPca } from "../../amla-sandbox/tests/pca_utils.mjs";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

const WARMUP_ITERATIONS = 5;
const BENCHMARK_ITERATIONS = 20;

// =============================================================================
// WASI Shim
// =============================================================================

const ERRNO_SUCCESS = 0;
const ERRNO_BADF = 8;
const ERRNO_NOSYS = 52;

function createMinimalWasi() {
  let memory = null;
  let currentTimeNanos = BigInt(Date.now()) * 1_000_000n;
  let randomCounter = 0;

  return {
    setMemory(mem) { memory = mem; },
    wasi_snapshot_preview1: {
      clock_time_get(clock_id, precision, out_ptr) {
        if (!memory) return ERRNO_NOSYS;
        new DataView(memory.buffer).setBigUint64(out_ptr, currentTimeNanos, true);
        currentTimeNanos += 1_000_000n;
        return ERRNO_SUCCESS;
      },
      random_get(buf_ptr, buf_len) {
        if (!memory) return ERRNO_NOSYS;
        const bytes = new Uint8Array(memory.buffer, buf_ptr, buf_len);
        for (let i = 0; i < buf_len; i++) {
          bytes[i] = (randomCounter++ * 1103515245 + 12345) & 0xff;
        }
        return ERRNO_SUCCESS;
      },
      fd_write(fd, iovs_ptr, iovs_len, nwritten_ptr) {
        if (!memory) return ERRNO_NOSYS;
        if (fd !== 2) return ERRNO_BADF;
        const view = new DataView(memory.buffer);
        let total = 0;
        for (let i = 0; i < iovs_len; i++) {
          total += view.getUint32(iovs_ptr + i * 8 + 4, true);
        }
        view.setUint32(nwritten_ptr, total, true);
        return ERRNO_SUCCESS;
      },
      fd_read: () => ERRNO_NOSYS,
      fd_close: () => ERRNO_NOSYS,
      fd_seek: () => ERRNO_NOSYS,
      fd_tell: () => ERRNO_NOSYS,
      fd_filestat_get: () => ERRNO_NOSYS,
      fd_fdstat_get: () => ERRNO_NOSYS,
      fd_fdstat_set_flags: () => ERRNO_NOSYS,
      path_open: () => ERRNO_NOSYS,
      path_filestat_get: () => ERRNO_NOSYS,
      fd_prestat_get: () => ERRNO_BADF,
      fd_prestat_dir_name: () => ERRNO_BADF,
      environ_get: () => ERRNO_SUCCESS,
      environ_sizes_get(count_ptr, size_ptr) {
        if (!memory) return ERRNO_NOSYS;
        const view = new DataView(memory.buffer);
        view.setUint32(count_ptr, 0, true);
        view.setUint32(size_ptr, 0, true);
        return ERRNO_SUCCESS;
      },
      sched_yield: () => ERRNO_SUCCESS,
      proc_exit(code) { throw new Error(`proc_exit(${code})`); },
    },
  };
}

// =============================================================================
// WASM Runtime with Full Stepping Loop
// =============================================================================

class WasmRuntime {
  constructor(instance, memory) {
    this.exports = instance.exports;
    this.memory = memory;
    this.encoder = new TextEncoder();
    this.decoder = new TextDecoder();

    // Allocate scratch buffers in high memory
    const memSize = memory.buffer.byteLength;
    this.cmdBufPtr = memSize - 64 * 1024;      // 64KB for command
    this.outBufPtr = memSize - 128 * 1024;     // 64KB for step output
    this.submitBufPtr = memSize - 192 * 1024;  // 64KB for submit
    this.outBufLen = 64 * 1024;
  }

  writeString(ptr, str) {
    const bytes = this.encoder.encode(str);
    new Uint8Array(this.memory.buffer, ptr, bytes.length).set(bytes);
    return bytes.length;
  }

  readString(ptr, len) {
    return this.decoder.decode(new Uint8Array(this.memory.buffer, ptr, len));
  }

  /**
   * Execute a shell command and return when complete.
   * Uses full stepping protocol with host op handling.
   */
  runCommand(shellCmd) {
    const { runtime_destroy, cmd_create, cmd_delete, runtime_step, submit } = this.exports;

    // Create runtime with real PCA
    const { rtId: rt } = createRuntimeWithPca(this.exports, this.memory);
    if (rt === 0n || rt === 0) {
      throw new Error("Failed to create runtime");
    }

    const rtId = typeof rt === 'bigint' ? Number(rt) : rt;

    try {
      // Create command
      const cmdLen = this.writeString(this.cmdBufPtr, shellCmd);
      const cmd = cmd_create(rt, this.cmdBufPtr, cmdLen);
      if (cmd === 0n || cmd === 0) {
        throw new Error("Failed to create command");
      }

      // Step loop
      let maxSteps = 100;
      while (maxSteps-- > 0) {
        // Step runtime
        const resultLen = runtime_step(rt, this.outBufPtr, this.outBufLen);
        if (resultLen === 0) break;

        // Parse response
        const responseJson = this.readString(this.outBufPtr, resultLen);
        let response;
        try {
          response = JSON.parse(responseJson);
        } catch (e) {
          console.error("Failed to parse step response:", responseJson.slice(0, 200));
          break;
        }

        // Check status
        if (response.status === "all_done" || response.status?.all_done) {
          break;
        }

        // Handle host ops
        const hostOps = response.host_ops || [];
        if (hostOps.length === 0) {
          // No pending ops and not done - might be running
          continue;
        }

        // Build responses for each host op
        const results = hostOps.map(op => this.handleHostOp(op, rtId));

        // Submit results
        const resultsJson = JSON.stringify(results);
        const resultsLen = this.writeString(this.submitBufPtr, resultsJson);
        submit(this.submitBufPtr, resultsLen);
      }

      cmd_delete(rt, cmd);
    } finally {
      runtime_destroy(rt);
    }
  }

  handleHostOp(op, rtId) {
    const { id, request } = op;
    const type = request?.type || request;

    // Build appropriate response based on op type
    let result;
    switch (type) {
      case "output":
        result = { type: "output_ack" };
        break;
      case "command_exit":
        result = { type: "exit_ack" };
        break;
      case "wake_at":
        // Note: JSON doesn't support u64, so precision loss occurs for nanoseconds.
        // This is fine for benchmarks (no sleeps), but production code should use
        // WASI clock_time_get which writes directly to WASM memory via BigUint64.
        result = { type: "woke_at", current_time_nanos: Date.now() * 1_000_000 };
        break;
      case "read_stdin":
        // Return empty stdin (EOF)
        result = { type: "stdin_data", data: "", eof: true };
        break;
      default:
        // Generic acknowledgment
        result = { type: "output_ack" };
    }

    return { id, runtime_id: rtId, result };
  }
}

// =============================================================================
// Benchmarks
// =============================================================================

const BENCHMARKS = {
  // Simple expressions
  "expr/add": "1+1",
  "expr/multiply": "123 * 456",

  // JSON
  "json/parse": `JSON.parse('{"a":1,"b":[1,2,3]}')`,
  "json/stringify": `JSON.stringify({a:1,b:[1,2,3],c:"hello"})`,

  // Compute
  "compute/fib_15": `(function f(n){return n<2?n:f(n-1)+f(n-2)})(15)`,
  "compute/fib_20": `(function f(n){return n<2?n:f(n-1)+f(n-2)})(20)`,
  "compute/fib_25": `(function f(n){return n<2?n:f(n-1)+f(n-2)})(25)`,
  "compute/sum_1k": `(function(){let s=0;for(let i=0;i<1000;i++)s+=i;return s})()`,
  "compute/sum_10k": `(function(){let s=0;for(let i=0;i<10000;i++)s+=i;return s})()`,

  // Strings
  "string/concat": `(function(){let s='';for(let i=0;i<50;i++)s+='x';return s})()`,
  "string/split": `'a,b,c,d,e,f,g'.split(',').join('-')`,

  // Arrays
  "array/map": `[1,2,3,4,5].map(x=>x*2)`,
  "array/reduce": `[1,2,3,4,5,6,7,8,9,10].reduce((a,b)=>a+b,0)`,
};

// =============================================================================
// Runner
// =============================================================================

function findWasmBinary(explicitPath) {
  if (explicitPath && existsSync(explicitPath)) return explicitPath;
  const paths = [
    join(__dirname, "..", "..", "target", "wasm32-wasip1", "release", "amla_sandbox.wasm"),
    join(__dirname, "..", "..", "..", "target", "wasm32-wasip1", "release", "amla_sandbox.wasm"),
  ];
  return paths.find(existsSync) || null;
}

function benchmarkV8(code, iterations) {
  // Warmup
  for (let i = 0; i < WARMUP_ITERATIONS; i++) eval(code);

  const start = process.hrtime.bigint();
  for (let i = 0; i < iterations; i++) eval(code);
  const end = process.hrtime.bigint();

  return Number(end - start) / iterations / 1000; // µs
}

function benchmarkWasm(runtime, code, iterations) {
  const cmd = `node -p '${code.replace(/'/g, "\\'")}'`;

  // Warmup
  for (let i = 0; i < WARMUP_ITERATIONS; i++) {
    try { runtime.runCommand(cmd); } catch (e) { /* ignore warmup errors */ }
  }

  const start = process.hrtime.bigint();
  for (let i = 0; i < iterations; i++) {
    runtime.runCommand(cmd);
  }
  const end = process.hrtime.bigint();

  return Number(end - start) / iterations / 1000; // µs
}

function formatTime(us) {
  if (us >= 1000) return `${(us/1000).toFixed(2)} ms`;
  return `${us.toFixed(1)} µs`;
}

function formatOps(us) {
  const ops = 1_000_000 / us;
  if (ops >= 1_000_000) return `${(ops/1e6).toFixed(1)}M`;
  if (ops >= 1_000) return `${(ops/1e3).toFixed(1)}K`;
  return ops.toFixed(0);
}

async function main() {
  console.log("═".repeat(80));
  console.log("  V8 vs WASM QuickJS Benchmark (Full Execution)");
  console.log("═".repeat(80));
  console.log(`Node.js ${process.version} | Iterations: ${BENCHMARK_ITERATIONS}`);
  console.log();

  const wasmPath = findWasmBinary(process.argv[2]);
  if (!wasmPath) {
    console.error("WASM not found. Build with:");
    console.error("  cargo build --release -p amla-sandbox --target wasm32-wasip1");
    process.exit(1);
  }

  console.log(`WASM: ${wasmPath}`);
  const wasmBytes = readFileSync(wasmPath);
  console.log(`Size: ${(wasmBytes.length / 1024 / 1024).toFixed(2)} MB`);

  const wasi = createMinimalWasi();
  const module = await WebAssembly.compile(wasmBytes);
  const instance = await WebAssembly.instantiate(module, wasi);
  wasi.setMemory(instance.exports.memory);

  // Grow memory for scratch buffers
  const pages = instance.exports.memory.buffer.byteLength / 65536;
  if (pages < 256) instance.exports.memory.grow(256 - pages); // 16MB total

  const runtime = new WasmRuntime(instance, instance.exports.memory);
  console.log("Runtime ready\n");

  // Run benchmarks
  const results = [];
  for (const [name, code] of Object.entries(BENCHMARKS)) {
    process.stdout.write(`${name.padEnd(20)}`);

    const v8Time = benchmarkV8(code, BENCHMARK_ITERATIONS);
    process.stdout.write(` V8: ${formatTime(v8Time).padStart(10)}`);

    const wasmTime = benchmarkWasm(runtime, code, BENCHMARK_ITERATIONS);
    const ratio = wasmTime / v8Time;

    console.log(` WASM: ${formatTime(wasmTime).padStart(10)}  ${ratio.toFixed(0)}x slower`);
    results.push({ name, v8Time, wasmTime, ratio });
  }

  // Summary
  console.log();
  console.log("═".repeat(80));
  console.log("  Summary");
  console.log("═".repeat(80));
  console.log();
  console.log("Benchmark            │    V8 Time │   V8 ops/s │  WASM Time │ WASM ops/s │  Ratio");
  console.log("─".repeat(85));

  for (const { name, v8Time, wasmTime, ratio } of results) {
    const v8Str = formatTime(v8Time).padStart(10);
    const v8Ops = formatOps(v8Time).padStart(10);
    const wasmStr = formatTime(wasmTime).padStart(10);
    const wasmOps = formatOps(wasmTime).padStart(10);
    const ratioStr = `${ratio.toFixed(0)}x`.padStart(6);
    console.log(`${name.padEnd(20)} │ ${v8Str} │ ${v8Ops} │ ${wasmStr} │ ${wasmOps} │ ${ratioStr}`);
  }

  // Aggregate stats
  const avgRatio = results.reduce((s, r) => s + r.ratio, 0) / results.length;
  const computeResults = results.filter(r => r.name.startsWith("compute/"));
  const avgComputeRatio = computeResults.reduce((s, r) => s + r.ratio, 0) / computeResults.length;

  console.log();
  console.log(`Average slowdown: ${avgRatio.toFixed(0)}x (all) / ${avgComputeRatio.toFixed(0)}x (compute-only)`);
  console.log();
  console.log("Note: WASM times include full runtime lifecycle per iteration:");
  console.log("  - Runtime creation (VFS, scheduler, shell)");
  console.log("  - Command parsing and execution");
  console.log("  - Host operation stepping loop");
  console.log("  - Runtime destruction");
}

main().catch(e => {
  console.error("Error:", e.message);
  console.error(e.stack);
  process.exit(1);
});
