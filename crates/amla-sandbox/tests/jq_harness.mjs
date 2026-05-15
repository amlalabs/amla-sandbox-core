#!/usr/bin/env node
/**
 * JQ E2E Test Harness - Comprehensive tests for the jq command.
 *
 * Tests:
 *   - Basic jq filters
 *   - Pipes (echo | jq)
 *   - File input
 *   - Complex expressions
 *   - Options (-r, -c, -s, -n, -e)
 *   - NDJSON processing
 *   - Chained pipes (jq | grep | wc)
 *
 * Usage:
 *   node tests/jq_harness.mjs <path-to-wasm>
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
const ERRNO_BADF = 8;
const ERRNO_NOSYS = 52;

// =============================================================================
// Minimal WASI Shim (same as wasi_harness.mjs)
// =============================================================================

function createMinimalWasi({ getTimeNanos, getRandomBytes, onStderr }) {
  let memory = null;

  return {
    setMemory(mem) {
      memory = mem;
    },

    wasi_snapshot_preview1: {
      clock_time_get(clock_id, precision, out_ptr) {
        if (!memory) return ERRNO_NOSYS;
        const nanos = getTimeNanos(clock_id);
        const view = new DataView(memory.buffer);
        view.setBigUint64(out_ptr, BigInt(nanos), true);
        return ERRNO_SUCCESS;
      },

      random_get(buf_ptr, buf_len) {
        if (!memory) return ERRNO_NOSYS;
        const bytes = getRandomBytes(buf_len);
        new Uint8Array(memory.buffer, buf_ptr, buf_len).set(bytes);
        return ERRNO_SUCCESS;
      },

      fd_write(fd, iovs_ptr, iovs_len, nwritten_ptr) {
        if (!memory) return ERRNO_NOSYS;
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

      fd_read() { return ERRNO_NOSYS; },
      fd_close() { return ERRNO_NOSYS; },
      fd_seek() { return ERRNO_NOSYS; },
      fd_tell() { return ERRNO_NOSYS; },
      fd_filestat_get() { return ERRNO_NOSYS; },
      fd_fdstat_get() { return ERRNO_NOSYS; },
      fd_fdstat_set_flags() { return ERRNO_NOSYS; },
      path_open() { return ERRNO_NOSYS; },
      path_filestat_get() { return ERRNO_NOSYS; },
      fd_prestat_get() { return ERRNO_BADF; },
      fd_prestat_dir_name() { return ERRNO_BADF; },
      environ_get() { return ERRNO_SUCCESS; },
      environ_sizes_get(count_ptr, size_ptr) {
        if (!memory) return ERRNO_NOSYS;
        const view = new DataView(memory.buffer);
        view.setUint32(count_ptr, 0, true);
        view.setUint32(size_ptr, 0, true);
        return ERRNO_SUCCESS;
      },
      sched_yield() { return ERRNO_SUCCESS; },
      proc_exit(code) {
        throw new Error(`WASM called proc_exit(${code})`);
      },
    },
  };
}

// =============================================================================
// Runtime Helpers
// =============================================================================

class Runtime {
  constructor(exports, wasi) {
    this.exports = exports;
    this.wasi = wasi;
    this.encoder = new TextEncoder();
    this.decoder = new TextDecoder();
    this.memory = exports.memory;
    this.rtId = null;
  }

  create() {
    const { rtId } = createRuntimeWithPca(this.exports, this.memory);
    this.rtId = rtId;
    return this.rtId;
  }

  destroy() {
    if (this.rtId) {
      this.exports.runtime_destroy(this.rtId);
      this.rtId = null;
    }
  }

  /**
   * Run a shell command and return { stdout, stderr, exitCode }
   */
  async runCommand(cmd, { stdin = "", maxSteps = 100 } = {}) {
    const cmdBytes = this.encoder.encode(cmd);
    const cmdPtr = 1024;
    new Uint8Array(this.memory.buffer, cmdPtr, cmdBytes.length).set(cmdBytes);

    const cmdHandle = this.exports.cmd_create(this.rtId, cmdPtr, cmdBytes.length);
    if (cmdHandle === 0n || cmdHandle === 0) {
      throw new Error(`Failed to create command: ${cmd}`);
    }

    const outPtr = 2048;
    const outLen = 16384;
    let stdout = "";
    let stderr = "";
    let exitCode = null;
    let stdinSent = false;
    let steps = 0;

    while (steps++ < maxSteps) {
      const resultLen = this.exports.runtime_step(this.rtId, outPtr, outLen);
      if (resultLen === 0) break;

      const responseJson = this.decoder.decode(
        new Uint8Array(this.memory.buffer, outPtr, resultLen)
      );
      const response = JSON.parse(responseJson);

      if (response.status?.error) {
        throw new Error(`Runtime error: ${JSON.stringify(response.status.error)}`);
      }
      if (response.status?.panic) {
        throw new Error(`Panic: ${response.status.panic.message || response.status.panic}`);
      }

      // Process host_ops BEFORE checking all_done (exit comes in final response)
      if (response.host_ops && response.host_ops.length > 0) {
        const results = response.host_ops.map(op => {
          const req = op.request;

          if (req.type === "output") {
            const data = atob(req.data);
            if (req.stream === 1) stdout += data;
            if (req.stream === 2) stderr += data;
            return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
          }

          if (req.type === "command_exit") {
            exitCode = req.code;
            return { id: op.id, runtime_id: op.runtime_id, result: { type: "exit_ack" } };
          }

          if (req.type === "read_stdin") {
            if (!stdinSent && stdin) {
              stdinSent = true;
              return {
                id: op.id,
                runtime_id: op.runtime_id,
                result: { type: "stdin_data", data: btoa(stdin), eof: true }
              };
            }
            return {
              id: op.id,
              runtime_id: op.runtime_id,
              result: { type: "stdin_data", data: "", eof: true }
            };
          }

          // Default ack
          return { id: op.id, runtime_id: op.runtime_id, result: { type: "output_ack" } };
        });

        const resultsJson = this.encoder.encode(JSON.stringify(results));
        const submitPtr = 8192;
        new Uint8Array(this.memory.buffer, submitPtr, resultsJson.length).set(resultsJson);
        this.exports.submit(submitPtr, resultsJson.length);
      }

      // Check all_done AFTER processing host_ops
      if (response.status === "all_done" || response.status?.all_done) break;
    }

    this.exports.cmd_delete(this.rtId, cmdHandle);

    return { stdout, stderr, exitCode };
  }
}

// =============================================================================
// Test Framework
// =============================================================================

let testsPassed = 0;
let testsFailed = 0;

function test(name, fn) {
  return { name, fn };
}

async function runTests(runtime, tests) {
  console.log(`\nRunning ${tests.length} tests...\n`);

  for (const { name, fn } of tests) {
    process.stdout.write(`  ${name}... `);
    try {
      await fn(runtime);
      console.log("PASS");
      testsPassed++;
    } catch (e) {
      console.log(`FAIL\n    ${e.message}`);
      testsFailed++;
    }
  }

  console.log(`\n${testsPassed} passed, ${testsFailed} failed`);
  return testsFailed === 0;
}

function assertEqual(actual, expected, msg = "") {
  if (actual !== expected) {
    throw new Error(`${msg}\n      Expected: ${JSON.stringify(expected)}\n      Actual:   ${JSON.stringify(actual)}`);
  }
}

function assertIncludes(actual, substring, msg = "") {
  if (!actual.includes(substring)) {
    throw new Error(`${msg}\n      Expected to include: ${JSON.stringify(substring)}\n      Actual: ${JSON.stringify(actual)}`);
  }
}

function assertJSON(actual, expected, msg = "") {
  const actualParsed = JSON.parse(actual.trim());
  const expectedParsed = typeof expected === "string" ? JSON.parse(expected) : expected;
  if (JSON.stringify(actualParsed) !== JSON.stringify(expectedParsed)) {
    throw new Error(`${msg}\n      Expected: ${JSON.stringify(expectedParsed)}\n      Actual:   ${JSON.stringify(actualParsed)}`);
  }
}

// =============================================================================
// JQ Tests
// =============================================================================

const jqTests = [
  // Basic identity filter
  test("jq identity filter (.)", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":1}' | jq .`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, { a: 1 }, "output");
  }),

  // Field access
  test("jq field access (.foo)", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"foo":"bar"}' | jq .foo`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), '"bar"', "output");
  }),

  // Nested field access
  test("jq nested field access (.a.b.c)", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":{"b":{"c":42}}}' | jq .a.b.c`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "42", "output");
  }),

  // Array indexing
  test("jq array index (.[0])", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[10,20,30]' | jq '.[0]'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "10", "output");
  }),

  // Array iteration
  test("jq array iteration (.[])", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[1,2,3]' | jq '.[]'`);
    assertEqual(exitCode, 0, "exit code");
    const lines = stdout.trim().split("\n");
    assertEqual(lines.length, 3, "should output 3 values");
    assertEqual(lines[0], "1");
    assertEqual(lines[1], "2");
    assertEqual(lines[2], "3");
  }),

  // Pipe within jq
  test("jq pipe expression (.foo | .bar)", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"foo":{"bar":"baz"}}' | jq '.foo | .bar'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), '"baz"', "output");
  }),

  // Object construction
  test("jq object construction ({x: .a})", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":1,"b":2}' | jq '{x: .a}'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, { x: 1 }, "output");
  }),

  // Array construction
  test("jq array construction ([.a, .b])", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":1,"b":2}' | jq '[.a, .b]'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, [1, 2], "output");
  }),

  // Map function
  test("jq map function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[1,2,3]' | jq 'map(. * 2)'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, [2, 4, 6], "output");
  }),

  // Select function
  test("jq select function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[1,2,3,4,5]' | jq '.[] | select(. > 3)'`);
    assertEqual(exitCode, 0, "exit code");
    const lines = stdout.trim().split("\n");
    assertEqual(lines.length, 2, "should output 2 values");
    assertEqual(lines[0], "4");
    assertEqual(lines[1], "5");
  }),

  // Keys function
  test("jq keys function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"b":1,"a":2}' | jq 'keys'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, ["a", "b"], "output (keys should be sorted)");
  }),

  // Length function
  test("jq length function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":1,"b":2,"c":3}' | jq 'length'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "3", "output");
  }),

  // Type function
  test("jq type function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":1}' | jq 'type'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), '"object"', "output");
  }),

  // Raw output (-r)
  test("jq raw output (-r)", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"msg":"hello world"}' | jq -r '.msg'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "hello world", "raw output without quotes");
  }),

  // Compact output (-c)
  test("jq compact output (-c)", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":1,"b":2}' | jq -c .`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), '{"a":1,"b":2}', "compact output");
  }),

  // Null input (-n)
  test("jq null input (-n)", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`jq -n '{hello: "world"}'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, { hello: "world" }, "output from null input");
  }),

  // Exit status (-e) with truthy value
  test("jq exit status (-e) truthy", async (rt) => {
    const { exitCode } = await rt.runCommand(`echo '{"ok":true}' | jq -e '.ok'`);
    assertEqual(exitCode, 0, "truthy value should exit 0");
  }),

  // Exit status (-e) with null
  test("jq exit status (-e) null", async (rt) => {
    const { exitCode } = await rt.runCommand(`echo '{"ok":null}' | jq -e '.ok'`);
    assertEqual(exitCode, 1, "null value should exit 1");
  }),

  // Exit status (-e) with false
  test("jq exit status (-e) false", async (rt) => {
    const { exitCode } = await rt.runCommand(`echo '{"ok":false}' | jq -e '.ok'`);
    assertEqual(exitCode, 1, "false value should exit 1");
  }),

  // Slurp mode (-s)
  test("jq slurp mode (-s)", async (rt) => {
    // Slurp multiple JSON values into an array
    const { stdout, exitCode } = await rt.runCommand(
      `printf '{"a":1}\\n{"a":2}\\n{"a":3}' | jq -s 'map(.a)'`
    );
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, [1, 2, 3], "slurped values");
  }),

  // Addition operator
  test("jq addition operator", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":1,"b":2}' | jq '.a + .b'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "3", "addition result");
  }),

  // String concatenation
  test("jq string concatenation", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"first":"hello","last":"world"}' | jq -r '.first + " " + .last'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "hello world", "concatenation result");
  }),

  // Comparison operators
  test("jq comparison operators", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[1,5,10]' | jq '.[] | . > 3'`);
    assertEqual(exitCode, 0, "exit code");
    const lines = stdout.trim().split("\n");
    assertEqual(lines[0], "false");
    assertEqual(lines[1], "true");
    assertEqual(lines[2], "true");
  }),

  // Conditional (if-then-else)
  test("jq conditional (if-then-else)", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '5' | jq 'if . > 3 then "big" else "small" end'`
    );
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), '"big"', "conditional result");
  }),

  // Has function
  test("jq has function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"foo":1}' | jq 'has("foo")'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "true", "has result");
  }),

  // Sort function
  test("jq sort function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[3,1,2]' | jq 'sort'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, [1, 2, 3], "sorted array");
  }),

  // Reverse function
  test("jq reverse function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[1,2,3]' | jq 'reverse'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, [3, 2, 1], "reversed array");
  }),

  // First/last
  test("jq first function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[1,2,3]' | jq 'first'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "1", "first element");
  }),

  // Unique function
  test("jq unique function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[1,2,1,3,2]' | jq 'unique'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, [1, 2, 3], "unique values");
  }),

  // Group by
  test("jq group_by function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '[{"k":"a"},{"k":"b"},{"k":"a"}]' | jq 'group_by(.k) | length'`
    );
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "2", "group count");
  }),

  // Shell pipe: jq output to grep
  test("shell pipe: jq | grep", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '["apple","banana","apricot"]' | jq -r '.[]' | grep ap`
    );
    assertEqual(exitCode, 0, "exit code");
    const lines = stdout.trim().split("\n");
    assertEqual(lines.length, 2, "should match 2 lines");
    assertIncludes(stdout, "apple");
    assertIncludes(stdout, "apricot");
  }),

  // Shell pipe: jq output to wc
  test("shell pipe: jq | wc -l", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '[1,2,3,4,5]' | jq '.[]' | wc -l`
    );
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "5", "line count");
  }),

  // Shell pipe: jq output to sort
  test("shell pipe: jq -r | sort", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '["cherry","apple","banana"]' | jq -r '.[]' | sort`
    );
    assertEqual(exitCode, 0, "exit code");
    const lines = stdout.trim().split("\n");
    assertEqual(lines[0], "apple");
    assertEqual(lines[1], "banana");
    assertEqual(lines[2], "cherry");
  }),

  // Shell pipe: echo to jq to grep to wc
  test("shell pipe chain: echo | jq | grep | wc", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '["foo","bar","foobar","baz"]' | jq -r '.[]' | grep foo | wc -l`
    );
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "2", "should match 2 lines containing foo");
  }),

  // Empty input handling
  test("jq with empty object", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{}' | jq .`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, {}, "empty object");
  }),

  // Empty array handling
  test("jq with empty array", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '[]' | jq .`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, [], "empty array");
  }),

  // Null value handling
  test("jq with null value", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo 'null' | jq .`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "null", "null value");
  }),

  // Boolean handling
  test("jq with booleans", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"t":true,"f":false}' | jq '[.t, .f]'`);
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, [true, false], "booleans");
  }),

  // Number handling (integers and floats)
  test("jq with numbers", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"i":42,"f":3.14}' | jq '.i + .f'`);
    assertEqual(exitCode, 0, "exit code");
    const result = parseFloat(stdout.trim());
    if (Math.abs(result - 45.14) > 0.001) {
      throw new Error(`Expected ~45.14, got ${result}`);
    }
  }),

  // Unicode handling
  test("jq with unicode", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"emoji":"hello"}' | jq -r '.emoji'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "hello", "unicode emoji");
  }),

  // Nested array/object access
  test("jq complex nested access", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '{"users":[{"name":"alice","age":30},{"name":"bob","age":25}]}' | jq '.users[1].name'`
    );
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), '"bob"', "nested access");
  }),

  // Error handling: missing field returns null
  test("jq missing field returns null", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(`echo '{"a":1}' | jq '.nonexistent'`);
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "null", "missing field");
  }),

  // Multiple outputs with pipe
  test("jq multiple outputs piped", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '{"items":[{"id":1},{"id":2}]}' | jq -c '.items[]'`
    );
    assertEqual(exitCode, 0, "exit code");
    const lines = stdout.trim().split("\n");
    assertEqual(lines.length, 2, "two outputs");
    assertJSON(lines[0], { id: 1 });
    assertJSON(lines[1], { id: 2 });
  }),

  // Complex filter: transformation pipeline
  test("jq complex transformation", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '[{"name":"a","val":1},{"name":"b","val":2}]' | jq 'map({(.name): .val}) | add'`
    );
    assertEqual(exitCode, 0, "exit code");
    assertJSON(stdout, { a: 1, b: 2 }, "transformation result");
  }),

  // Reduce function
  test("jq reduce function", async (rt) => {
    const { stdout, exitCode } = await rt.runCommand(
      `echo '[1,2,3,4,5]' | jq 'reduce .[] as $x (0; . + $x)'`
    );
    assertEqual(exitCode, 0, "exit code");
    assertEqual(stdout.trim(), "15", "reduce sum");
  }),

  // Invalid filter syntax (should error)
  test("jq invalid filter syntax", async (rt) => {
    const { exitCode, stderr } = await rt.runCommand(`echo '{}' | jq '.[[[invalid'`);
    assertEqual(exitCode, 3, "should exit with error code 3");
  }),

  // Invalid JSON input (should error)
  test("jq invalid JSON input", async (rt) => {
    const { exitCode, stderr } = await rt.runCommand(`echo 'not json' | jq .`);
    if (exitCode === 0) {
      throw new Error("Expected non-zero exit code for invalid JSON");
    }
    // Exit code may vary, just ensure it's not 0
  }),
];

// =============================================================================
// Main
// =============================================================================

function findWasmBinary(explicitPath) {
  if (explicitPath) {
    if (existsSync(explicitPath)) return explicitPath;
    console.error(`WASM binary not found at: ${explicitPath}`);
    process.exit(1);
  }

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
  console.log("=== JQ E2E Test Harness ===\n");

  const wasmPath = findWasmBinary(process.argv[2]);
  if (!wasmPath) {
    console.log("WASM binary not found.");
    console.log("Build with: cargo build --release -p amla-sandbox --target wasm32-wasip1");
    process.exit(1);
  }

  console.log(`Loading: ${wasmPath}\n`);

  const wasmBytes = readFileSync(wasmPath);
  console.log(`WASM size: ${(wasmBytes.length / 1024 / 1024).toFixed(2)} MB`);

  let currentTimeNanos = BigInt(Date.now()) * 1_000_000n;
  let randomCounter = 0;

  const wasi = createMinimalWasi({
    getTimeNanos: () => currentTimeNanos,
    getRandomBytes: (len) => {
      const bytes = new Uint8Array(len);
      for (let i = 0; i < len; i++) {
        bytes[i] = (randomCounter++ * 1103515245 + 12345) & 0xFF;
      }
      return bytes;
    },
    onStderr: (text) => process.stderr.write(text),
  });

  const module = await WebAssembly.compile(wasmBytes);
  const instance = await WebAssembly.instantiate(module, wasi);
  wasi.setMemory(instance.exports.memory);

  console.log("WASM instantiated!\n");

  const runtime = new Runtime(instance.exports, wasi);
  runtime.create();
  console.log("Runtime created with real Ed25519-signed PCA\n");

  try {
    const passed = await runTests(runtime, jqTests);
    runtime.destroy();

    if (passed) {
      console.log("\n=== All JQ Tests Passed ===");
      process.exit(0);
    } else {
      console.log("\n=== Some JQ Tests Failed ===");
      process.exit(1);
    }
  } catch (e) {
    runtime.destroy();
    console.error("\nFatal error:", e.message);
    process.exit(1);
  }
}

main().catch(e => {
  console.error("Test failed:", e.message);
  process.exit(1);
});
