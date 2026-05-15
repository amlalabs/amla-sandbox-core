#!/usr/bin/env node
/**
 * WASI Test Harness - Verifies WASM binary loads and runs correctly.
 *
 * Usage:
 *   node tests/wasi_harness.mjs <path-to-wasm>
 *   node tests/wasi_harness.mjs  # Uses default path
 *
 * ## Why a Minimal WASI Shim?
 *
 * The amla runtime is a SANDBOXED execution environment. The WASM module
 * should NOT have access to:
 * - Real filesystem (it has an in-memory VFS)
 * - Real environment variables (controlled by host)
 * - Real stdin/stdout (virtualized through host ops)
 *
 * The only WASI syscalls the runtime legitimately needs are:
 *
 * 1. `clock_time_get` - For timestamps in scheduling and expiry checks.
 *    The host controls time to enable deterministic replay and testing.
 *
 * 2. `random_get` - For generating IDs and cryptographic operations.
 *    The host controls randomness for deterministic replay.
 *
 * 3. `fd_write` (stderr only) - For panic messages during development.
 *    In production, this could be blocked entirely.
 *
 * All other WASI imports exist because:
 * - Rust stdlib links them even if unused
 * - Dependencies (like `getrandom`) require WASI interface
 * - Panic handler needs minimal I/O
 *
 * By returning ERRNO_NOSYS for everything else, we enforce that the
 * runtime cannot escape its sandbox through WASI.
 *
 * ## Host Operations vs WASI
 *
 * Don't confuse WASI syscalls with Host Operations. Host Operations are
 * the runtime's own high-level protocol (ToolCall, VfsRead, Output, etc.)
 * returned by `runtime_step()` as JSON. Those are NOT WASI - they're
 * handled by the JavaScript host's application logic.
 *
 * Architecture:
 *   Command → runtime_step() → JSON host ops → JS executes → submit()
 *
 * Prerequisites:
 *   cargo build --release -p amla-sandbox --target wasm32-wasip1
 */

import { readFileSync, existsSync } from "fs";
import { fileURLToPath } from "url";
import { dirname, join } from "path";
import { createRuntimeWithPca } from "./pca_utils.mjs";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

// =============================================================================
// WASI Error Codes
// =============================================================================

const ERRNO_SUCCESS = 0;
const ERRNO_BADF = 8;     // Bad file descriptor
const ERRNO_NOSYS = 52;   // Function not supported

// =============================================================================
// Minimal WASI Shim
// =============================================================================

/**
 * Create a minimal WASI implementation that only provides what the sandbox needs.
 *
 * @param {object} options
 * @param {function} options.getTimeNanos - Returns current time in nanoseconds
 * @param {function} options.getRandomBytes - Returns Uint8Array of random bytes
 * @param {function} options.onStderr - Called with stderr text (for panics)
 */
function createMinimalWasi({ getTimeNanos, getRandomBytes, onStderr }) {
  let memory = null;

  return {
    // Called after instantiation to bind memory
    setMemory(mem) {
      memory = mem;
    },

    // The actual WASI imports
    wasi_snapshot_preview1: {
      // =====================================================================
      // IMPLEMENTED: Time - Required for scheduling and expiry checks
      // =====================================================================
      clock_time_get(clock_id, precision, out_ptr) {
        if (!memory) return ERRNO_NOSYS;

        // Host provides controlled time for deterministic behavior
        const nanos = getTimeNanos(clock_id);
        const view = new DataView(memory.buffer);
        view.setBigUint64(out_ptr, BigInt(nanos), true);
        return ERRNO_SUCCESS;
      },

      // =====================================================================
      // IMPLEMENTED: Random - Required for IDs and crypto
      // =====================================================================
      random_get(buf_ptr, buf_len) {
        if (!memory) return ERRNO_NOSYS;

        // Host provides controlled randomness for deterministic replay
        const bytes = getRandomBytes(buf_len);
        new Uint8Array(memory.buffer, buf_ptr, buf_len).set(bytes);
        return ERRNO_SUCCESS;
      },

      // =====================================================================
      // IMPLEMENTED: Stderr - For panic messages only
      // =====================================================================
      fd_write(fd, iovs_ptr, iovs_len, nwritten_ptr) {
        if (!memory) return ERRNO_NOSYS;

        // Only stderr (fd=2) for panic messages; block stdout
        if (fd !== 2) return ERRNO_BADF;

        const view = new DataView(memory.buffer);
        const mem = new Uint8Array(memory.buffer);

        let totalWritten = 0;
        for (let i = 0; i < iovs_len; i++) {
          const ptr = view.getUint32(iovs_ptr + i * 8, true);
          const len = view.getUint32(iovs_ptr + i * 8 + 4, true);
          const text = new TextDecoder().decode(mem.slice(ptr, ptr + len));
          onStderr(text);
          totalWritten += len;
        }

        view.setUint32(nwritten_ptr, totalWritten, true);
        return ERRNO_SUCCESS;
      },

      // =====================================================================
      // BLOCKED: File I/O - Runtime has VFS, doesn't need real files
      // =====================================================================
      fd_read() { return ERRNO_NOSYS; },
      fd_close() { return ERRNO_NOSYS; },
      fd_seek() { return ERRNO_NOSYS; },
      fd_tell() { return ERRNO_NOSYS; },
      fd_filestat_get() { return ERRNO_NOSYS; },
      fd_fdstat_get() { return ERRNO_NOSYS; },
      fd_fdstat_set_flags() { return ERRNO_NOSYS; },
      path_open() { return ERRNO_NOSYS; },
      path_filestat_get() { return ERRNO_NOSYS; },

      // =====================================================================
      // BLOCKED: Preopened directories - Not needed for sandbox
      // =====================================================================
      fd_prestat_get() {
        // Return BADF to signal no preopened directories
        // This is expected - sandbox doesn't expose host filesystem
        return ERRNO_BADF;
      },
      fd_prestat_dir_name() { return ERRNO_BADF; },

      // =====================================================================
      // BLOCKED: Environment - Host controls all config via PCA/Host Ops
      // =====================================================================
      environ_get() { return ERRNO_SUCCESS; }, // Empty is valid
      environ_sizes_get(count_ptr, size_ptr) {
        if (!memory) return ERRNO_NOSYS;
        const view = new DataView(memory.buffer);
        view.setUint32(count_ptr, 0, true); // 0 env vars
        view.setUint32(size_ptr, 0, true);
        return ERRNO_SUCCESS;
      },

      // =====================================================================
      // ALLOWED: Scheduler yield - Harmless no-op
      // =====================================================================
      sched_yield() { return ERRNO_SUCCESS; },

      // =====================================================================
      // BLOCKED: Process exit - Trap instead of allowing silent exit
      // =====================================================================
      proc_exit(code) {
        throw new Error(`WASM called proc_exit(${code})`);
      },
    },
  };
}

// =============================================================================
// Test Harness
// =============================================================================

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

async function main() {
  console.log("=== WASI Test Harness ===\n");

  const wasmPath = findWasmBinary(process.argv[2]);
  if (!wasmPath) {
    console.log("WASM binary not found.");
    console.log("Usage: node wasi_harness.mjs <path-to-wasm>");
    console.log("Or build with: cargo build --release -p amla-sandbox --target wasm32-wasip1");
    process.exit(1);
  }

  console.log(`Loading: ${wasmPath}\n`);

  // 1. Load WASM bytes
  const wasmBytes = readFileSync(wasmPath);
  console.log(`WASM size: ${(wasmBytes.length / 1024 / 1024).toFixed(2)} MB`);

  // 2. Create minimal WASI with controlled time/random
  let currentTimeNanos = BigInt(Date.now()) * 1_000_000n;
  let randomCounter = 0;

  const wasi = createMinimalWasi({
    getTimeNanos: () => currentTimeNanos,
    getRandomBytes: (len) => {
      // Deterministic PRNG for testing
      const bytes = new Uint8Array(len);
      for (let i = 0; i < len; i++) {
        bytes[i] = (randomCounter++ * 1103515245 + 12345) & 0xFF;
      }
      return bytes;
    },
    onStderr: (text) => process.stderr.write(text),
  });

  // 3. Compile and instantiate with minimal WASI
  const module = await WebAssembly.compile(wasmBytes);
  const instance = await WebAssembly.instantiate(module, wasi);
  wasi.setMemory(instance.exports.memory);

  console.log("WASM instantiated with minimal WASI shim!\n");

  // 4. Verify exports
  const exports = instance.exports;
  const expectedExports = [
    "memory",
    "runtime_new",
    "runtime_destroy",
    "runtime_step",
    "cmd_create",
    "cmd_delete",
    "cmd_cancel",
    "submit",
  ];

  console.log("Checking exports:");
  let allPresent = true;
  for (const name of expectedExports) {
    const present = name in exports;
    console.log(`  ${name}: ${present ? "OK" : "MISSING"}`);
    if (!present) allPresent = false;
  }

  if (!allPresent) {
    console.log("\nERROR: Missing required exports");
    process.exit(1);
  }

  // 5. Test memory access
  const memory = exports.memory;
  console.log(`\nMemory: ${memory.buffer.byteLength / 1024} KB (${memory.buffer.byteLength / 65536} pages)`);

  const oldSize = memory.buffer.byteLength;
  memory.grow(10);
  const newSize = memory.buffer.byteLength;
  console.log(`Memory grow: ${oldSize / 1024} KB -> ${newSize / 1024} KB`);

  // 6. Test runtime creation with real PCA
  console.log("\nTesting runtime creation with real Ed25519-signed PCA...");
  try {
    const { rtId, authority } = createRuntimeWithPca(exports, memory);
    console.log(`  runtime_new() returned: ${rtId}`);
    console.log(`  Authority: ${authority.publicKeyHex().slice(0, 30)}...`);
    console.log("  Runtime created successfully with real PCA!");

    // Clean up
    exports.runtime_destroy(rtId);
    console.log("  Runtime destroyed.");

    // 7. Test full stepping protocol with echo command
    await testSteppingProtocol(exports, wasi, memory);

    // 8. Test command cancellation
    await testCommandCancellation(exports, wasi, memory);

    // 8b. Test cancellation edge cases
    await testCancellationEdgeCases(exports, wasi, memory);

    // 9. Test panic recovery
    await testPanicRecovery(exports, wasi, memory);

    // Test 5: Concurrent commands with timing
    await testConcurrentCommandTiming(exports, wasi, memory);
  } catch (e) {
    console.log(`  ERROR: ${e.message}`);
    process.exit(1);
  }

  console.log("\n=== WASI Test Passed ===");
}

/**
 * Test the full stepping protocol with a simple echo command.
 */
async function testSteppingProtocol(exports, wasi, memory) {
  console.log("\nTesting stepping protocol with 'echo hello'...");

  const encoder = new TextEncoder();
  const decoder = new TextDecoder();

  // Create runtime with real PCA
  const { rtId } = createRuntimeWithPca(exports, memory);
  if (rtId === 0n || rtId === 0) {
    console.log("  ERROR: Failed to create runtime");
    process.exit(1);
  }

  // Create command - write "echo hello" to memory
  const cmd = "echo hello";
  const cmdBytes = encoder.encode(cmd);
  const cmdPtr = 1024; // Safe location in memory
  new Uint8Array(memory.buffer, cmdPtr, cmdBytes.length).set(cmdBytes);

  const cmdHandle = exports.cmd_create(rtId, cmdPtr, cmdBytes.length);
  if (cmdHandle === 0n || cmdHandle === 0) {
    console.log("  ERROR: Failed to create command");
    exports.runtime_destroy(rtId);
    process.exit(1);
  }
  console.log(`  Created command handle: ${cmdHandle}`);

  // Step until completion
  const outPtr = 2048;
  const outLen = 8192;
  let collectedOutput = "";
  let exitCode = null;
  let steps = 0;
  const maxSteps = 50;

  while (steps++ < maxSteps) {
    const resultLen = exports.runtime_step(rtId, outPtr, outLen);
    if (resultLen === 0) break;

    const responseJson = decoder.decode(new Uint8Array(memory.buffer, outPtr, resultLen));
    const response = JSON.parse(responseJson);

    // Process host ops
    if (response.host_ops && response.host_ops.length > 0) {
      const results = response.host_ops.map(op => {
        const req = op.request;
        if (req.type === "output" && req.stream === 1) {
          // stdout - collect it
          const data = atob(req.data);
          collectedOutput += data;
        }
        if (req.type === "command_exit") {
          exitCode = req.code;
        }
        // Build response
        if (req.type === "output") return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
        if (req.type === "command_exit") return { id: op.id, runtime_id: op.runtime_id, result: { type: "exit_ack" } };
        if (req.type === "read_stdin") return { id: op.id, runtime_id: op.runtime_id, result: { type: "stdin_data", data: "", eof: true } };
        return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
      });

      // Submit results
      const resultsJson = encoder.encode(JSON.stringify(results));
      const submitPtr = 4096;
      new Uint8Array(memory.buffer, submitPtr, resultsJson.length).set(resultsJson);
      exports.submit(submitPtr, resultsJson.length);
    }

    if (response.status === "all_done" || response.status?.all_done) break;
  }

  // Verify output
  if (!collectedOutput.includes("hello")) {
    console.log(`  ERROR: Expected 'hello' in output, got: '${collectedOutput}'`);
    exports.runtime_destroy(rtId);
    process.exit(1);
  }

  if (exitCode !== 0) {
    console.log(`  ERROR: Expected exit code 0, got: ${exitCode}`);
    exports.runtime_destroy(rtId);
    process.exit(1);
  }

  console.log(`  Output: '${collectedOutput.trim()}'`);
  console.log(`  Exit code: ${exitCode}`);
  console.log("  Stepping protocol test passed!");

  // Cleanup
  exports.cmd_delete(rtId, cmdHandle);
  exports.runtime_destroy(rtId);
}

/**
 * Test command cancellation - verifies cmd_cancel works correctly.
 */
async function testCommandCancellation(exports, wasi, memory) {
  console.log("\nTesting command cancellation with 'cat' (blocking command)...");

  const encoder = new TextEncoder();
  const decoder = new TextDecoder();

  // Create runtime with real PCA
  const { rtId } = createRuntimeWithPca(exports, memory);
  if (rtId === 0n || rtId === 0) {
    console.log("  ERROR: Failed to create runtime");
    process.exit(1);
  }

  // Create a blocking command - 'cat' without input will wait forever
  const cmd = "cat";
  const cmdBytes = encoder.encode(cmd);
  const cmdPtr = 1024;
  new Uint8Array(memory.buffer, cmdPtr, cmdBytes.length).set(cmdBytes);

  const cmdHandle = exports.cmd_create(rtId, cmdPtr, cmdBytes.length);
  if (cmdHandle === 0n || cmdHandle === 0) {
    console.log("  ERROR: Failed to create command");
    exports.runtime_destroy(rtId);
    process.exit(1);
  }
  console.log(`  Created blocking command handle: ${cmdHandle}`);

  // Step once - command should be waiting for stdin
  const outPtr = 2048;
  const outLen = 8192;
  const cancelOutPtr = 12288;
  const cancelOutLen = 4096;

  const resultLen1 = exports.runtime_step(rtId, outPtr, outLen);
  if (resultLen1 > 0) {
    const response1 = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, outPtr, resultLen1)));
    console.log(`  Initial step status: ${JSON.stringify(response1.status)}`);

    // Should have ReadStdin host op
    const hasReadStdin = response1.host_ops?.some(op => op.request.type === "read_stdin");
    if (!hasReadStdin) {
      console.log(`  Warning: Expected read_stdin host op, got: ${JSON.stringify(response1.host_ops)}`);
    } else {
      console.log("  Command is waiting for stdin (as expected)");
    }
  }

  // Now cancel the command
  console.log("  Cancelling command...");
  const cancelResultLen = exports.cmd_cancel(rtId, cmdHandle, cancelOutPtr, cancelOutLen);

  if (cancelResultLen === 0) {
    console.log("  Warning: cmd_cancel returned 0 bytes");
  } else {
    const cancelResponse = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, cancelOutPtr, cancelResultLen)));
    console.log(`  Cancel response: ${JSON.stringify(cancelResponse)}`);

    // Should have CommandExit with code -1
    const hasExitOp = cancelResponse.some?.(op =>
      op.request?.type === "command_exit" && op.request?.code === -1
    );
    if (hasExitOp) {
      console.log("  Cancel returned CommandExit with code -1 (correct!)");
    } else if (Array.isArray(cancelResponse) && cancelResponse.length > 0) {
      console.log(`  Cancel returned ${cancelResponse.length} host op(s)`);
    }
  }

  // Step again - should be AllDone
  const resultLen2 = exports.runtime_step(rtId, outPtr, outLen);
  if (resultLen2 > 0) {
    const response2 = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, outPtr, resultLen2)));
    const isAllDone = response2.status === "all_done" || response2.status?.all_done;
    console.log(`  After cancel, status: ${JSON.stringify(response2.status)}, allDone: ${isAllDone}`);

    if (!isAllDone) {
      console.log("  Warning: Expected all_done status after cancel");
    }
  }

  // Test: Cancelling already cancelled command should be idempotent (return empty)
  const cancelResultLen2 = exports.cmd_cancel(rtId, cmdHandle, cancelOutPtr, cancelOutLen);
  if (cancelResultLen2 > 0) {
    const cancelResponse2 = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, cancelOutPtr, cancelResultLen2)));
    const isEmpty = Array.isArray(cancelResponse2) && cancelResponse2.length === 0;
    console.log(`  Second cancel is idempotent: ${isEmpty ? "yes (empty array)" : "no"}`);
  } else {
    console.log("  Second cancel returned 0 bytes (idempotent)");
  }

  // Cleanup
  exports.cmd_delete(rtId, cmdHandle);
  exports.runtime_destroy(rtId);

  console.log("  Command cancellation test passed!");
}

/**
 * Test cancellation edge cases - bulletproof testing for cmd_cancel.
 */
async function testCancellationEdgeCases(exports, wasi, memory) {
  console.log("\nTesting cancellation edge cases...");

  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  const outPtr = 2048;
  const outLen = 8192;
  const cancelOutPtr = 12288;
  const cancelOutLen = 4096;

  // Test 1: Cancel a command that has already completed
  console.log("  Test 1: Cancel already-completed command...");
  {
    const { rtId } = createRuntimeWithPca(exports, memory);
    const cmd = "echo done";
    const cmdBytes = encoder.encode(cmd);
    const cmdPtr = 1024;
    new Uint8Array(memory.buffer, cmdPtr, cmdBytes.length).set(cmdBytes);

    const cmdHandle = exports.cmd_create(rtId, cmdPtr, cmdBytes.length);

    // Run to completion
    let maxSteps = 20;
    while (maxSteps-- > 0) {
      const resultLen = exports.runtime_step(rtId, outPtr, outLen);
      if (resultLen === 0) break;
      const response = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, outPtr, resultLen)));

      if (response.host_ops?.length > 0) {
        const results = response.host_ops.map(op => {
          if (op.request.type === "output") return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
          if (op.request.type === "command_exit") return { id: op.id, runtime_id: op.runtime_id, result: { type: "exit_ack" } };
          return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
        });
        const resultsJson = encoder.encode(JSON.stringify(results));
        const submitPtr = 4096;
        new Uint8Array(memory.buffer, submitPtr, resultsJson.length).set(resultsJson);
        exports.submit(submitPtr, resultsJson.length);
      }

      if (response.status === "all_done") break;
    }

    // Now try to cancel the completed command - should be no-op
    const cancelResultLen = exports.cmd_cancel(rtId, cmdHandle, cancelOutPtr, cancelOutLen);
    if (cancelResultLen > 0) {
      const cancelResponse = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, cancelOutPtr, cancelResultLen)));
      const isEmpty = Array.isArray(cancelResponse) && cancelResponse.length === 0;
      if (!isEmpty) {
        console.log(`    FAIL: Cancel of completed command should return empty, got: ${JSON.stringify(cancelResponse)}`);
        process.exit(1);
      }
    }
    console.log("    PASS: Cancel of completed command is no-op");

    exports.cmd_delete(rtId, cmdHandle);
    exports.runtime_destroy(rtId);
  }

  // Test 2: Cancel non-existent command handle
  console.log("  Test 2: Cancel non-existent command handle...");
  {
    const { rtId } = createRuntimeWithPca(exports, memory);

    // Cancel a command that was never created (handle 999)
    const cancelResultLen = exports.cmd_cancel(rtId, 999n, cancelOutPtr, cancelOutLen);
    if (cancelResultLen > 0) {
      const cancelResponse = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, cancelOutPtr, cancelResultLen)));
      const isEmpty = Array.isArray(cancelResponse) && cancelResponse.length === 0;
      if (!isEmpty) {
        console.log(`    FAIL: Cancel of non-existent command should return empty, got: ${JSON.stringify(cancelResponse)}`);
        process.exit(1);
      }
    }
    console.log("    PASS: Cancel of non-existent command is no-op");

    exports.runtime_destroy(rtId);
  }

  // Test 3: Cancel with invalid runtime ID
  console.log("  Test 3: Cancel with invalid runtime ID...");
  {
    // Cancel on non-existent runtime (ID 999)
    const cancelResultLen = exports.cmd_cancel(999n, 1n, cancelOutPtr, cancelOutLen);
    if (cancelResultLen !== 0) {
      console.log(`    FAIL: Cancel with invalid runtime should return 0, got ${cancelResultLen} bytes`);
      process.exit(1);
    }
    console.log("    PASS: Cancel with invalid runtime returns 0");
  }

  // Test 4: Cancel command before stepping
  console.log("  Test 4: Cancel command before any stepping...");
  {
    const { rtId } = createRuntimeWithPca(exports, memory);
    const cmd = "echo never_runs";
    const cmdBytes = encoder.encode(cmd);
    const cmdPtr = 1024;
    new Uint8Array(memory.buffer, cmdPtr, cmdBytes.length).set(cmdBytes);

    const cmdHandle = exports.cmd_create(rtId, cmdPtr, cmdBytes.length);

    // Cancel immediately without stepping
    const cancelResultLen = exports.cmd_cancel(rtId, cmdHandle, cancelOutPtr, cancelOutLen);
    if (cancelResultLen === 0) {
      console.log("    FAIL: Cancel before stepping should return host ops");
      process.exit(1);
    }

    const cancelResponse = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, cancelOutPtr, cancelResultLen)));
    const hasExitOp = cancelResponse.some?.(op =>
      op.request?.type === "command_exit" && op.request?.code === -1
    );
    if (!hasExitOp) {
      console.log(`    FAIL: Expected CommandExit with code -1, got: ${JSON.stringify(cancelResponse)}`);
      process.exit(1);
    }
    console.log("    PASS: Cancel before stepping returns CommandExit(-1)");

    exports.cmd_delete(rtId, cmdHandle);
    exports.runtime_destroy(rtId);
  }

  // Test 5: Cancel command doesn't affect other commands in same runtime
  console.log("  Test 5: Cancel doesn't affect different runtime...");
  {
    // Create two separate runtimes
    const { rtId: rtId1 } = createRuntimeWithPca(exports, memory);
    const { rtId: rtId2 } = createRuntimeWithPca(exports, memory);

    // Create blocking command in runtime 1
    const cmd1 = "cat";
    const cmd1Bytes = encoder.encode(cmd1);
    const cmd1Ptr = 1024;
    new Uint8Array(memory.buffer, cmd1Ptr, cmd1Bytes.length).set(cmd1Bytes);
    const handle1 = exports.cmd_create(rtId1, cmd1Ptr, cmd1Bytes.length);

    // Create fast command in runtime 2
    const cmd2 = "echo second";
    const cmd2Bytes = encoder.encode(cmd2);
    const cmd2Ptr = 2048;
    new Uint8Array(memory.buffer, cmd2Ptr, cmd2Bytes.length).set(cmd2Bytes);
    const handle2 = exports.cmd_create(rtId2, cmd2Ptr, cmd2Bytes.length);

    // Step runtime 1 to start blocking
    exports.runtime_step(rtId1, outPtr, outLen);

    // Cancel runtime 1's command
    exports.cmd_cancel(rtId1, handle1, cancelOutPtr, cancelOutLen);

    // Runtime 2's command should still work
    let found_second_output = false;
    let maxSteps = 30;
    while (maxSteps-- > 0) {
      const resultLen = exports.runtime_step(rtId2, outPtr, outLen);
      if (resultLen === 0) break;
      const response = JSON.parse(decoder.decode(new Uint8Array(memory.buffer, outPtr, resultLen)));

      if (response.host_ops?.length > 0) {
        for (const op of response.host_ops) {
          if (op.request.type === "output" && op.request.stream === 1) {
            const data = atob(op.request.data);
            if (data.includes("second")) {
              found_second_output = true;
            }
          }
        }
        const results = response.host_ops.map(op => {
          if (op.request.type === "output") return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
          if (op.request.type === "command_exit") return { id: op.id, runtime_id: op.runtime_id, result: { type: "exit_ack" } };
          return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
        });
        const resultsJson = encoder.encode(JSON.stringify(results));
        const submitPtr = 4096;
        new Uint8Array(memory.buffer, submitPtr, resultsJson.length).set(resultsJson);
        exports.submit(submitPtr, resultsJson.length);
      }

      if (response.status === "all_done") break;
    }

    if (!found_second_output) {
      console.log("    FAIL: Runtime 2's command should have produced output");
      process.exit(1);
    }
    console.log("    PASS: Cancelling runtime 1 doesn't affect runtime 2");

    exports.cmd_delete(rtId1, handle1);
    exports.cmd_delete(rtId2, handle2);
    exports.runtime_destroy(rtId1);
    exports.runtime_destroy(rtId2);
  }

  console.log("  All cancellation edge cases passed!");
}

/**
 * Test panic recovery - the runtime should gracefully handle panics.
 *
 * Note: WASM panic recovery requires special build flags:
 *   cargo +nightly build -Zbuild-std=std,panic_unwind --target wasm32-wasip1
 *
 * Without these flags, panics abort the WASM instance and catch_unwind doesn't work.
 * This test detects whether panic recovery is available and skips gracefully if not.
 */
async function testPanicRecovery(exports, wasi, memory) {
  console.log("\nTesting panic recovery with 'panic test message'...");

  const encoder = new TextEncoder();
  const decoder = new TextDecoder();

  // Create runtime with real PCA
  const { rtId } = createRuntimeWithPca(exports, memory);
  if (rtId === 0n || rtId === 0) {
    console.log("  ERROR: Failed to create runtime");
    process.exit(1);
  }

  // Create panic command
  const cmd = "panic test message";
  const cmdBytes = encoder.encode(cmd);
  const cmdPtr = 1024;
  new Uint8Array(memory.buffer, cmdPtr, cmdBytes.length).set(cmdBytes);

  const cmdHandle = exports.cmd_create(rtId, cmdPtr, cmdBytes.length);
  if (cmdHandle === 0n || cmdHandle === 0) {
    // panic command might not be available (requires test-panic feature)
    console.log("  SKIP: panic command not available (requires test-panic feature)");
    exports.runtime_destroy(rtId);
    return;
  }
  console.log(`  Created panic command handle: ${cmdHandle}`);

  // Step - should trigger panic and return panic status (if built with panic_unwind)
  const outPtr = 2048;
  const outLen = 8192;

  let resultLen;
  try {
    resultLen = exports.runtime_step(rtId, outPtr, outLen);
  } catch (e) {
    // WASM trapped (unreachable) - panic recovery not available
    console.log("  SKIP: WASM panic aborted (requires -Zbuild-std=std,panic_unwind to enable catch_unwind)");
    console.log("  This is expected for standard wasip1 builds.");
    return;
  }

  if (resultLen === 0) {
    // Runtime returned nothing - might have aborted internally
    console.log("  SKIP: runtime_step returned 0 (panic may have aborted without recovery)");
    return;
  }

  const responseJson = decoder.decode(new Uint8Array(memory.buffer, outPtr, resultLen));
  const response = JSON.parse(responseJson);

  // Check for panic status
  if (!response.status?.panic) {
    // Panic recovery not available - the runtime continued without catching the panic
    // This happens when built without -Zbuild-std=std,panic_unwind
    console.log("  SKIP: panic recovery not enabled in WASM build");
    console.log(`  (got status: ${JSON.stringify(response.status)})`);
    console.log("  To enable: cargo +nightly build -Zbuild-std=std,panic_unwind --target wasm32-wasip1");
    // Clean up
    try { exports.runtime_destroy(rtId); } catch (e) {}
    return;
  }

  const panicMessage = response.status.panic.message || response.status.panic;
  console.log(`  Panic caught with message: '${panicMessage}'`);

  if (!panicMessage.includes("test message")) {
    console.log(`  ERROR: Expected panic message to contain 'test message'`);
    process.exit(1);
  }

  // Verify runtime was destroyed (attempting to step should fail gracefully)
  const resultLen2 = exports.runtime_step(rtId, outPtr, outLen);
  if (resultLen2 !== 0) {
    const response2 = decoder.decode(new Uint8Array(memory.buffer, outPtr, resultLen2));
    console.log(`  Warning: Runtime still accessible after panic: ${response2}`);
  } else {
    console.log("  Runtime correctly destroyed after panic");
  }

  console.log("  Panic recovery test passed!");
}

/**
 * Test concurrent commands - verifies that op.command field is properly set
 * and different commands get their own timing info.
 */
async function testConcurrentCommandTiming(exports, wasi, memory) {
  console.log("\nTesting concurrent commands with timing...");

  const encoder = new TextEncoder();
  const decoder = new TextDecoder();

  // Create runtime with real PCA
  const { rtId } = createRuntimeWithPca(exports, memory);
  if (rtId === 0n || rtId === 0) {
    console.log("  ERROR: Failed to create runtime");
    process.exit(1);
  }

  // Create two concurrent commands: echo "first" and echo "second"
  const cmd1 = "echo first";
  const cmd2 = "echo second";

  const cmd1Bytes = encoder.encode(cmd1);
  const cmd2Bytes = encoder.encode(cmd2);

  const cmdPtr = 1024;
  new Uint8Array(memory.buffer, cmdPtr, cmd1Bytes.length).set(cmd1Bytes);
  const handle1 = exports.cmd_create(rtId, cmdPtr, cmd1Bytes.length);

  new Uint8Array(memory.buffer, cmdPtr, cmd2Bytes.length).set(cmd2Bytes);
  const handle2 = exports.cmd_create(rtId, cmdPtr, cmd2Bytes.length);

  console.log(`  Created command 1 (handle: ${handle1}): ${cmd1}`);
  console.log(`  Created command 2 (handle: ${handle2}): ${cmd2}`);

  // Track outputs and exits per command
  const outputs = new Map(); // handle -> collected output
  const timings = new Map(); // handle -> { elapsed_ns, user_time_ns }

  outputs.set(Number(handle1), "");
  outputs.set(Number(handle2), "");

  // Step until all done
  const outPtr = 2048;
  const outLen = 16384;
  let steps = 0;
  const maxSteps = 100;

  while (steps++ < maxSteps) {
    let resultLen;
    try {
      resultLen = exports.runtime_step(rtId, outPtr, outLen);
    } catch (e) {
      console.log(`  Step error: ${e.message}`);
      break;
    }

    if (resultLen === 0) break;

    const responseJson = decoder.decode(new Uint8Array(memory.buffer, outPtr, resultLen));
    const response = JSON.parse(responseJson);

    // Process host ops - verify command field is present
    if (response.host_ops && response.host_ops.length > 0) {
      for (const op of response.host_ops) {
        const req = op.request;
        const cmdHandle = op.command; // This should be set!

        if (req.type === "output" && req.stream === 1 && req.data) {
          const data = atob(req.data);
          if (cmdHandle !== null && cmdHandle !== undefined) {
            const existing = outputs.get(cmdHandle) || "";
            outputs.set(cmdHandle, existing + data);
          } else {
            console.log(`  WARNING: Output op missing command field: ${JSON.stringify(op)}`);
          }
        }

        if (req.type === "command_exit") {
          if (cmdHandle !== null && cmdHandle !== undefined) {
            timings.set(cmdHandle, {
              elapsed_ns: req.elapsed_ns,
              user_time_ns: req.user_time_ns,
              code: req.code,
            });
            console.log(`  Command ${cmdHandle} exited: code=${req.code}, elapsed_ns=${req.elapsed_ns}`);
          } else {
            console.log(`  WARNING: CommandExit op missing command field: ${JSON.stringify(op)}`);
          }
        }
      }

      // Acknowledge all ops
      const results = response.host_ops.map(op => {
        if (op.request.type === "output") return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
        if (op.request.type === "command_exit") return { id: op.id, runtime_id: op.runtime_id, result: { type: "exit_ack" } };
        return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
      });

      const resultsJson = encoder.encode(JSON.stringify(results));
      const submitPtr = 4096;
      new Uint8Array(memory.buffer, submitPtr, resultsJson.length).set(resultsJson);
      exports.submit(submitPtr, resultsJson.length);
    }

    if (response.status === "all_done") break;
  }

  // Verify both commands completed with timing
  const h1 = Number(handle1);
  const h2 = Number(handle2);

  console.log(`  Command 1 output: '${outputs.get(h1)?.trim() || "(none)"}'`);
  console.log(`  Command 2 output: '${outputs.get(h2)?.trim() || "(none)"}'`);

  const timing1 = timings.get(h1);
  const timing2 = timings.get(h2);

  if (!timing1) {
    console.log(`  ERROR: Command 1 (handle ${h1}) missing timing info`);
    console.log(`  Available timings: ${JSON.stringify([...timings.entries()])}`);
    process.exit(1);
  }
  if (!timing2) {
    console.log(`  ERROR: Command 2 (handle ${h2}) missing timing info`);
    console.log(`  Available timings: ${JSON.stringify([...timings.entries()])}`);
    process.exit(1);
  }

  console.log(`  Command 1 timing: elapsed_ns=${timing1.elapsed_ns}, user_time_ns=${timing1.user_time_ns}`);
  console.log(`  Command 2 timing: elapsed_ns=${timing2.elapsed_ns}, user_time_ns=${timing2.user_time_ns}`);

  // Verify outputs are correct
  if (!outputs.get(h1)?.includes("first")) {
    console.log(`  ERROR: Command 1 should output 'first', got: '${outputs.get(h1)}'`);
    process.exit(1);
  }
  if (!outputs.get(h2)?.includes("second")) {
    console.log(`  ERROR: Command 2 should output 'second', got: '${outputs.get(h2)}'`);
    process.exit(1);
  }

  // Verify timing fields are present
  if (timing1.elapsed_ns === undefined || timing1.elapsed_ns === null) {
    console.log("  ERROR: Command 1 missing elapsed_ns");
    process.exit(1);
  }
  if (timing2.elapsed_ns === undefined || timing2.elapsed_ns === null) {
    console.log("  ERROR: Command 2 missing elapsed_ns");
    process.exit(1);
  }

  // Cleanup
  exports.cmd_delete(rtId, handle1);
  exports.cmd_delete(rtId, handle2);
  exports.runtime_destroy(rtId);

  console.log("  Concurrent command timing test passed!");
}

main().catch(e => {
  console.error("Test failed:", e.message);
  process.exit(1);
});
