#!/usr/bin/env node
/**
 * V8/Node.js benchmark for comparison with QuickJS.
 *
 * Run with: node benches/compare_v8.mjs
 *
 * This runs the same workloads as the Rust criterion benchmarks
 * to enable direct performance comparison between V8 and QuickJS.
 */

const WARMUP_ITERATIONS = 100;
const BENCHMARK_ITERATIONS = 1000;

// Simple benchmark harness
function benchmark(name, fn, iterations = BENCHMARK_ITERATIONS) {
  // Warmup
  for (let i = 0; i < WARMUP_ITERATIONS; i++) {
    fn();
  }

  // Force GC if available
  if (global.gc) {
    global.gc();
  }

  // Benchmark
  const start = performance.now();
  for (let i = 0; i < iterations; i++) {
    fn();
  }
  const end = performance.now();

  const totalMs = end - start;
  const perIterUs = (totalMs * 1000) / iterations;
  const opsPerSec = Math.round(iterations / (totalMs / 1000));

  return { name, totalMs, perIterUs, opsPerSec, iterations };
}

function formatResult(result) {
  return `${result.name.padEnd(35)} ${result.perIterUs.toFixed(3).padStart(10)} µs/iter  (${result.opsPerSec.toLocaleString().padStart(12)} ops/sec)`;
}

console.log("=".repeat(70));
console.log("V8/Node.js Benchmark - Comparison with QuickJS");
console.log("=".repeat(70));
console.log(`Node.js ${process.version}`);
console.log(`Iterations: ${BENCHMARK_ITERATIONS} (warmup: ${WARMUP_ITERATIONS})`);
console.log("=".repeat(70));
console.log();

const results = [];

// ============================================================================
// Arithmetic
// ============================================================================
console.log("## Arithmetic");

results.push(
  benchmark("arithmetic/simple_add", () => {
    return 1 + 2 + 3 + 4 + 5;
  })
);

results.push(
  benchmark("arithmetic/complex_expr", () => {
    return (1 + 2) * 3 / 4 - 5 + Math.pow(2, 10) + Math.sqrt(144);
  })
);

results.push(
  benchmark("arithmetic/loop_1000", () => {
    let sum = 0;
    for (let i = 0; i < 1000; i++) {
      sum += i;
    }
    return sum;
  })
);

results.slice(-3).forEach((r) => console.log(formatResult(r)));
console.log();

// ============================================================================
// Strings
// ============================================================================
console.log("## Strings");

results.push(
  benchmark("strings/concat_10", () => {
    return "a" + "b" + "c" + "d" + "e" + "f" + "g" + "h" + "i" + "j";
  })
);

const templateName = "World";
const templateCount = 42;
results.push(
  benchmark("strings/template_literal", () => {
    return `Hello ${templateName}, count is ${templateCount}`;
  })
);

results.push(
  benchmark("strings/split_join", () => {
    return "hello,world,foo,bar,baz".split(",").join("-");
  })
);

results.push(
  benchmark("strings/regex_match", () => {
    return "hello world 123 foo 456 bar".match(/\d+/g);
  })
);

results.slice(-4).forEach((r) => console.log(formatResult(r)));
console.log();

// ============================================================================
// Arrays
// ============================================================================
console.log("## Arrays");

results.push(
  benchmark("arrays/create_100", () => {
    return new Array(100).fill(0);
  })
);

const mapArr = new Array(100).fill(0).map((_, i) => i);
results.push(
  benchmark("arrays/map_100", () => {
    return mapArr.map((x) => x * 2);
  })
);

const filterArr = new Array(100).fill(0).map((_, i) => i);
results.push(
  benchmark("arrays/filter_100", () => {
    return filterArr.filter((x) => x % 2 === 0);
  })
);

const reduceArr = new Array(100).fill(0).map((_, i) => i);
results.push(
  benchmark("arrays/reduce_100", () => {
    return reduceArr.reduce((a, b) => a + b, 0);
  })
);

results.push(
  benchmark("arrays/sort_100", () => {
    return new Array(100)
      .fill(0)
      .map(() => Math.random())
      .sort((a, b) => a - b);
  })
);

results.slice(-5).forEach((r) => console.log(formatResult(r)));
console.log();

// ============================================================================
// Objects
// ============================================================================
console.log("## Objects");

results.push(
  benchmark("objects/create_literal", () => {
    return { a: 1, b: 2, c: 3, d: 4, e: 5 };
  })
);

const propObj = { a: 1, b: 2, c: 3, d: 4, e: 5 };
results.push(
  benchmark("objects/property_access", () => {
    return propObj.a + propObj.b + propObj.c + propObj.d + propObj.e;
  })
);

const keysObj = { a: 1, b: 2, c: 3, d: 4, e: 5 };
results.push(
  benchmark("objects/object_keys", () => {
    return Object.keys(keysObj);
  })
);

const jsonData = {
  users: [
    { name: "Alice", age: 30 },
    { name: "Bob", age: 25 },
  ],
  meta: { count: 2 },
};
results.push(
  benchmark("objects/json_roundtrip", () => {
    return JSON.parse(JSON.stringify(jsonData));
  })
);

results.slice(-4).forEach((r) => console.log(formatResult(r)));
console.log();

// ============================================================================
// Functions
// ============================================================================
console.log("## Functions");

function add(a, b) {
  return a + b;
}
results.push(
  benchmark("functions/simple_call", () => {
    return add(1, 2);
  })
);

function fib(n) {
  if (n <= 1) return n;
  return fib(n - 1) + fib(n - 2);
}
results.push(
  benchmark(
    "functions/fib_20",
    () => {
      return fib(20);
    },
    100
  )
); // Fewer iterations - expensive

function makeCounter() {
  let count = 0;
  return function () {
    return ++count;
  };
}
const counter = makeCounter();
results.push(
  benchmark("functions/closure", () => {
    return counter();
  })
);

const double = (x) => x * 2;
results.push(
  benchmark("functions/arrow_fn", () => {
    return double(21);
  })
);

results.slice(-4).forEach((r) => console.log(formatResult(r)));
console.log();

// ============================================================================
// Promises
// ============================================================================
console.log("## Promises (sync creation only - no await)");

results.push(
  benchmark("promises/promise_resolve", () => {
    return Promise.resolve(42).then((x) => x * 2);
  })
);

results.push(
  benchmark("promises/promise_chain_5", () => {
    return Promise.resolve(1)
      .then((x) => x + 1)
      .then((x) => x + 1)
      .then((x) => x + 1)
      .then((x) => x + 1)
      .then((x) => x + 1);
  })
);

results.push(
  benchmark("promises/promise_all_10", () => {
    return Promise.all([
      Promise.resolve(1),
      Promise.resolve(2),
      Promise.resolve(3),
      Promise.resolve(4),
      Promise.resolve(5),
      Promise.resolve(6),
      Promise.resolve(7),
      Promise.resolve(8),
      Promise.resolve(9),
      Promise.resolve(10),
    ]);
  })
);

results.slice(-3).forEach((r) => console.log(formatResult(r)));
console.log();

// ============================================================================
// Throughput
// ============================================================================
console.log("## Throughput (loop iterations)");

for (const size of [100, 1000, 10000]) {
  results.push(
    benchmark(
      `throughput/loop_sum_${size}`,
      () => {
        let sum = 0;
        for (let i = 0; i < size; i++) {
          sum += i;
        }
        return sum;
      },
      Math.max(100, Math.floor(BENCHMARK_ITERATIONS / (size / 100)))
    )
  );
}

results.slice(-3).forEach((r) => console.log(formatResult(r)));
console.log();

// ============================================================================
// Agent Workload Simulation
// ============================================================================
console.log("## Agent Workload Simulation");

const agentState = {
  messages: [],
  context: { user: "test", session: "abc123" },
};

function processMessage(msg) {
  agentState.messages.push(msg);
  const response = {
    id: agentState.messages.length,
    content: msg.content.toUpperCase(),
    timestamp: Date.now(),
  };
  return response;
}

results.push(
  benchmark("agent_workload/typical_agent_step", () => {
    const msg = { role: "user", content: "Hello, how are you?" };
    const response = processMessage(msg);
    // Simulate tool call decision
    if (response.content.includes("HELLO")) {
      // In real QuickJS, this creates a pending op
      // Here we just return the decision
      return { toolCall: "greeting:respond", args: { message: response.content } };
    }
    return response;
  })
);

const llmResponse = {
  choices: [
    {
      message: {
        role: "assistant",
        content: "Here is the answer",
        tool_calls: [
          { id: "1", function: { name: "search", arguments: '{"query":"test"}' } },
          { id: "2", function: { name: "fetch", arguments: '{"url":"https://example.com"}' } },
        ],
      },
    },
  ],
};

results.push(
  benchmark("agent_workload/json_processing", () => {
    const choice = llmResponse.choices[0];
    const toolCalls = choice.message.tool_calls || [];
    const parsed = toolCalls.map((tc) => ({
      id: tc.id,
      name: tc.function.name,
      args: JSON.parse(tc.function.arguments),
    }));
    return parsed;
  })
);

results.slice(-2).forEach((r) => console.log(formatResult(r)));
console.log();

// ============================================================================
// Summary
// ============================================================================
console.log("=".repeat(70));
console.log("Summary - All Results");
console.log("=".repeat(70));
results.forEach((r) => console.log(formatResult(r)));
console.log();

// Export as JSON for comparison tooling
console.log("=".repeat(70));
console.log("JSON Output (for comparison scripts):");
console.log("=".repeat(70));
console.log(
  JSON.stringify(
    results.map((r) => ({
      name: r.name,
      us_per_iter: r.perIterUs,
      ops_per_sec: r.opsPerSec,
    })),
    null,
    2
  )
);
