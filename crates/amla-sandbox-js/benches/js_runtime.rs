//! Benchmarks for amla-js `QuickJS` runtime.
//!
//! These benchmarks measure various JS workloads to understand performance
//! characteristics of the `QuickJS` engine embedded in Rust.
//!
//! Run with: `cargo bench -p amla-js`
//!
//! For comparison with V8/Node.js, see the companion benchmark at:
//! `benches/compare_v8.mjs` which runs equivalent JS code in Node.js.

#![allow(missing_docs)]

use amla_js::JsRuntime;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

/// Benchmark: Runtime creation overhead
fn bench_runtime_creation(c: &mut Criterion) {
    c.bench_function("runtime_creation", |b| {
        b.iter(|| {
            let rt = JsRuntime::new();
            black_box(rt)
        });
    });
}

/// Benchmark: Simple arithmetic evaluation
fn bench_arithmetic(c: &mut Criterion) {
    let mut group = c.benchmark_group("arithmetic");

    // Simple addition
    group.bench_function("simple_add", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| rt.execute(black_box("1 + 2 + 3 + 4 + 5")));
    });

    // Complex expression
    group.bench_function("complex_expr", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            rt.execute(black_box(
                "(1 + 2) * 3 / 4 - 5 + Math.pow(2, 10) + Math.sqrt(144)",
            ))
        });
    });

    // Loop with arithmetic
    group.bench_function("loop_1000", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            rt.execute(black_box(
                r#"
                let sum = 0;
                for (let i = 0; i < 1000; i++) {
                    sum += i;
                }
                sum
                "#,
            ))
        });
    });

    group.finish();
}

/// Benchmark: String operations
fn bench_strings(c: &mut Criterion) {
    let mut group = c.benchmark_group("strings");

    // String concatenation
    group.bench_function("concat_10", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            rt.execute(black_box(
                "'a' + 'b' + 'c' + 'd' + 'e' + 'f' + 'g' + 'h' + 'i' + 'j'",
            ))
        });
    });

    // String template
    group.bench_function("template_literal", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("const name = 'World'; const count = 42;")
            .unwrap();
        b.iter(|| rt.execute(black_box("`Hello ${name}, count is ${count}`")));
    });

    // String split/join
    group.bench_function("split_join", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| rt.execute(black_box("'hello,world,foo,bar,baz'.split(',').join('-')")));
    });

    // Regex match
    group.bench_function("regex_match", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| rt.execute(black_box("'hello world 123 foo 456 bar'.match(/\\d+/g)")));
    });

    group.finish();
}

/// Benchmark: Array operations
fn bench_arrays(c: &mut Criterion) {
    let mut group = c.benchmark_group("arrays");

    // Array creation
    group.bench_function("create_100", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| rt.execute(black_box("new Array(100).fill(0)")));
    });

    // Map operation
    group.bench_function("map_100", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("const arr = new Array(100).fill(0).map((_, i) => i)")
            .unwrap();
        b.iter(|| rt.execute(black_box("arr.map(x => x * 2)")));
    });

    // Filter operation
    group.bench_function("filter_100", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("const arr2 = new Array(100).fill(0).map((_, i) => i)")
            .unwrap();
        b.iter(|| rt.execute(black_box("arr2.filter(x => x % 2 === 0)")));
    });

    // Reduce operation
    group.bench_function("reduce_100", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("const arr3 = new Array(100).fill(0).map((_, i) => i)")
            .unwrap();
        b.iter(|| rt.execute(black_box("arr3.reduce((a, b) => a + b, 0)")));
    });

    // Sort
    group.bench_function("sort_100", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            rt.execute(black_box(
                "new Array(100).fill(0).map(() => Math.random()).sort((a, b) => a - b)",
            ))
        });
    });

    group.finish();
}

/// Benchmark: Object operations
fn bench_objects(c: &mut Criterion) {
    let mut group = c.benchmark_group("objects");

    // Object creation
    group.bench_function("create_literal", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| rt.execute(black_box("({ a: 1, b: 2, c: 3, d: 4, e: 5 })")));
    });

    // Property access
    group.bench_function("property_access", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("const obj = { a: 1, b: 2, c: 3, d: 4, e: 5 }")
            .unwrap();
        b.iter(|| rt.execute(black_box("obj.a + obj.b + obj.c + obj.d + obj.e")));
    });

    // Object.keys
    group.bench_function("object_keys", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("const obj2 = { a: 1, b: 2, c: 3, d: 4, e: 5 }")
            .unwrap();
        b.iter(|| rt.execute(black_box("Object.keys(obj2)")));
    });

    // JSON stringify/parse roundtrip
    group.bench_function("json_roundtrip", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("const data = { users: [{ name: 'Alice', age: 30 }, { name: 'Bob', age: 25 }], meta: { count: 2 } }").unwrap();
        b.iter(|| rt.execute(black_box("JSON.parse(JSON.stringify(data))")));
    });

    group.finish();
}

/// Benchmark: Function calls
fn bench_functions(c: &mut Criterion) {
    let mut group = c.benchmark_group("functions");

    // Simple function call
    group.bench_function("simple_call", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("function add(a, b) { return a + b; }").unwrap();
        b.iter(|| rt.execute(black_box("add(1, 2)")));
    });

    // Recursive function (fibonacci)
    group.bench_function("fib_20", |b| {
        let mut rt = JsRuntime::new();
        rt.execute(
            r#"
            function fib(n) {
                if (n <= 1) return n;
                return fib(n - 1) + fib(n - 2);
            }
            "#,
        )
        .unwrap();
        b.iter(|| rt.execute(black_box("fib(20)")));
    });

    // Closure
    group.bench_function("closure", |b| {
        let mut rt = JsRuntime::new();
        rt.execute(
            r#"
            function makeCounter() {
                let count = 0;
                return function() { return ++count; };
            }
            const counter = makeCounter();
            "#,
        )
        .unwrap();
        b.iter(|| rt.execute(black_box("counter()")));
    });

    // Arrow function
    group.bench_function("arrow_fn", |b| {
        let mut rt = JsRuntime::new();
        rt.execute("const double = x => x * 2;").unwrap();
        b.iter(|| rt.execute(black_box("double(21)")));
    });

    group.finish();
}

/// Benchmark: Promise/async operations
fn bench_promises(c: &mut Criterion) {
    let mut group = c.benchmark_group("promises");

    // Promise creation and resolution
    group.bench_function("promise_resolve", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| rt.execute(black_box("Promise.resolve(42).then(x => x * 2)")));
    });

    // Promise chain
    group.bench_function("promise_chain_5", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            rt.execute(black_box(
                r#"
                Promise.resolve(1)
                    .then(x => x + 1)
                    .then(x => x + 1)
                    .then(x => x + 1)
                    .then(x => x + 1)
                    .then(x => x + 1)
                "#,
            ))
        });
    });

    // Promise.all
    group.bench_function("promise_all_10", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            rt.execute(black_box(
                r#"
                Promise.all([
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
                ])
                "#,
            ))
        });
    });

    group.finish();
}

/// Benchmark: __amla__ global operations (pending op creation)
fn bench_amla_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("amla_ops");

    // Tool call creation
    group.bench_function("tool_call", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            let result = rt.execute(black_box(
                "__amla__.toolCall('test:tool', { param: 'value' })",
            ));
            // Take ops to avoid accumulation
            rt.take_pending_ops();
            result
        });
    });

    // Memory operations
    group.bench_function("memory_read", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            let result = rt.execute(black_box("__amla__.memoryRead('key')"));
            rt.take_pending_ops();
            result
        });
    });

    // Sleep (pending op creation, not actual sleep)
    group.bench_function("sleep_pending", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            let result = rt.execute(black_box("__amla__.sleep(100)"));
            rt.take_pending_ops();
            result
        });
    });

    // Shell command (pending op creation)
    group.bench_function("shell_pending", |b| {
        let mut rt = JsRuntime::new();
        b.iter(|| {
            let result = rt.execute(black_box("__amla__.shell('echo hello')"));
            rt.take_pending_ops();
            result
        });
    });

    group.finish();
}

/// Benchmark: Throughput for various workload sizes
fn bench_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");

    // Measure iterations per second for different loop sizes
    for size in &[100_u64, 1000, 10000] {
        group.throughput(Throughput::Elements(*size));
        group.bench_with_input(BenchmarkId::new("loop_sum", size), size, |b, &size| {
            let mut rt = JsRuntime::new();
            let code = format!(
                r#"
                let sum = 0;
                for (let i = 0; i < {size}; i++) {{
                    sum += i;
                }}
                sum
                "#
            );
            b.iter(|| rt.execute(black_box(&code)));
        });
    }

    group.finish();
}

/// Benchmark: Real-world-ish agent workload simulation
fn bench_agent_workload(c: &mut Criterion) {
    let mut group = c.benchmark_group("agent_workload");

    // Simulate typical agent code pattern
    group.bench_function("typical_agent_step", |b| {
        let mut rt = JsRuntime::new();
        // Setup some state
        rt.execute(
            r#"
            const state = {
                messages: [],
                context: { user: 'test', session: 'abc123' }
            };

            function processMessage(msg) {
                state.messages.push(msg);
                const response = {
                    id: state.messages.length,
                    content: msg.content.toUpperCase(),
                    timestamp: Date.now()
                };
                return response;
            }
            "#,
        )
        .unwrap();

        b.iter(|| {
            rt.execute(black_box(
                r#"
                const msg = { role: 'user', content: 'Hello, how are you?' };
                const response = processMessage(msg);
                // Simulate tool call decision
                if (response.content.includes('HELLO')) {
                    __amla__.toolCall('greeting:respond', { message: response.content });
                }
                response
                "#,
            ))
        });
    });

    // JSON processing (common in LLM responses)
    group.bench_function("json_processing", |b| {
        let mut rt = JsRuntime::new();
        rt.execute(
            r#"
            const llmResponse = {
                choices: [{
                    message: {
                        role: 'assistant',
                        content: 'Here is the answer',
                        tool_calls: [
                            { id: '1', function: { name: 'search', arguments: '{"query":"test"}' } },
                            { id: '2', function: { name: 'fetch', arguments: '{"url":"https://example.com"}' } }
                        ]
                    }
                }]
            };
            "#,
        )
        .unwrap();

        b.iter(|| {
            rt.execute(black_box(
                r#"
                const choice = llmResponse.choices[0];
                const toolCalls = choice.message.tool_calls || [];
                const parsed = toolCalls.map(tc => ({
                    id: tc.id,
                    name: tc.function.name,
                    args: JSON.parse(tc.function.arguments)
                }));
                parsed
                "#,
            ))
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_runtime_creation,
    bench_arithmetic,
    bench_strings,
    bench_arrays,
    bench_objects,
    bench_functions,
    bench_promises,
    bench_amla_ops,
    bench_throughput,
    bench_agent_workload,
);

criterion_main!(benches);
