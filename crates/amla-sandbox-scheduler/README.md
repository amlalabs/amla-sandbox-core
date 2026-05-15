# amla-scheduler

Single-threaded async executor for WASM. No tokio. No async-std. No threads.

## Why a Custom Scheduler

Standard async runtimes don't work in WASM:

- Tokio requires threads and OS timers
- async-std requires system calls
- No runtime provides deterministic time control

This scheduler runs entirely in userspace, yielding to the host for all I/O. The host controls time, enabling deterministic replay.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Scheduler                                   │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                      Task Registry                           │   │
│  │  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐        │   │
│  │  │ Task 0  │  │ Task 1  │  │ Task 2  │  │ Task 3  │  ...   │   │
│  │  │ Ready   │  │ Running │  │ Pending │  │ Pending │        │   │
│  │  └────┬────┘  └────┬────┘  └────┬────┘  └────┬────┘        │   │
│  │       │            │            │            │              │   │
│  │       ▼            ▼            ▼            ▼              │   │
│  │  ┌──────────────────────────────────────────────────────┐  │   │
│  │  │                   Ready Queue                         │  │   │
│  │  │  [0] ←── waker notifies when task should run again   │  │   │
│  │  └──────────────────────────────────────────────────────┘  │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌───────────────────────────┴───────────────────────────────────┐ │
│  │                        Host Channel                            │ │
│  │  Pending host operations: [ToolCall, WakeAt, VfsRead, ...]    │ │
│  └───────────────────────────────────────────────────────────────┘ │
│                              │                                      │
│                              ▼ yield                                │
└═════════════════════════════════════════════════════════════════════┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────────────┐
│                            Host                                     │
│  - Execute pending operations (tool calls, file I/O)               │
│  - Provide responses                                                │
│  - Control time and randomness                                      │
└─────────────────────────────────────────────────────────────────────┘
```

## Structured Concurrency

Tasks form a parent-child tree. When a parent is cancelled, all children are cancelled:

```rust
let sched = Scheduler::new(runtime_id, time_source, random_source);

// Root task
sched.spawn(async move {
    // Child tasks
    let child1 = sched.spawn(async { /* ... */ });
    let child2 = sched.spawn(async { /* ... */ });

    // Wait for both (structured concurrency)
    let (r1, r2) = join_all([child1, child2]).await;

    Ok(Exit::success())
});

// If root is cancelled, child1 and child2 are automatically cancelled
```

Task relationships are tracked via `parent` and `children` fields:

```rust
struct Task {
    future: Pin<Box<dyn Future<Output = Result<Exit, Error>>>>,
    parent: Option<TaskId>,      // Who spawned me
    children: SmallVec<[TaskId; 4]>,  // My spawned children
    root: Option<TaskId>,        // Top of spawn chain (for attribution)
}
```

## Coroutine Model

Tasks are cooperative coroutines that yield at I/O boundaries:

```rust
// Inside a task
async {
    // Yield: request file read from host
    let data = channel.file_read("/data.json").await?;

    // Yield: request tool call from host
    let result = channel.tool_call("stripe:charge", params).await?;

    // Yield: sleep until deadline
    channel.sleep(Duration::from_secs(5)).await;

    Ok(Exit::success())
}
```

Each `await` potentially yields control back to the scheduler. The scheduler tracks which tasks are ready (woken) vs. pending (waiting for host response).

## Stepping Protocol

The scheduler runs until blocked, then yields to the host:

```rust
loop {
    match sched.run() {
        SchedulerState::Done => break,       // All tasks completed
        SchedulerState::Progress => continue, // Made progress, keep going
        SchedulerState::Blocked => {
            // Tasks waiting for host operations
            let pending = sched.host().take_pending();

            for op in pending {
                match op.kind {
                    HostOpKind::ToolCall { tool, params } => {
                        let result = execute_tool(&tool, &params);
                        sched.host().complete(op.id, result);
                    }
                    HostOpKind::WakeAt { deadline } => {
                        // Host controls time - advance clock
                        sched.host().complete(op.id, current_time);
                    }
                    // ...
                }
            }
        }
    }
}
```

## Combinators

### select_first

Wait for the first task to complete:

```rust
let task1 = sched.spawn(async { /* fast */ });
let task2 = sched.spawn(async { /* slow */ });

// Returns when either completes, cancels the other
let winner = select_first([task1, task2]).await;
```

### join_all

Wait for all tasks:

```rust
let tasks = vec![
    sched.spawn(async { /* work 1 */ }),
    sched.spawn(async { /* work 2 */ }),
    sched.spawn(async { /* work 3 */ }),
];

// Returns when all complete
let results = join_all(tasks).await;
```

## Host Operations

The scheduler doesn't perform I/O directly. Instead, tasks request operations from the host via `HostChannel`. This is the core abstraction that enables:

1. **Sandboxing**: All external access goes through the host
2. **Determinism**: Host provides recorded responses for replay
3. **Control**: Host can deny, modify, or delay operations

### Operation Lifecycle

```
┌─────────────┐                    ┌─────────────┐
│    Task     │                    │    Host     │
└──────┬──────┘                    └──────┬──────┘
       │                                  │
       │  1. await tool_call(...)         │
       │  ────────────────────────────►   │
       │  (task yields, op queued)        │
       │                                  │
       │                                  │  2. take_pending()
       │                                  │  ◄────────────────
       │                                  │  (host gets op)
       │                                  │
       │                                  │  3. execute tool
       │                                  │  ...
       │                                  │
       │  4. complete(op_id, result)      │
       │  ◄────────────────────────────   │
       │  (task wakes, resumes)           │
       │                                  │
       ▼                                  ▼
```

### Operation Types

```rust
pub enum HostOpKind {
    /// Read a mapped file (full content)
    FileRead { path: String },

    /// Read a range of bytes from a mapped file
    FileReadRange { path: String, offset: u64, len: usize },
}
```

Note: The scheduler provides a minimal set of host operations for file I/O. Higher-level operations like `ToolCall`, `Output`, `ReadStdin`, and `Delegate` are defined in `amla-sandbox/src/host_ops.rs` and built on top of the scheduler's primitives.

### HostChannel API

```rust
// Create channel with queue size
let channel = HostChannel::new(runtime_id, queue_size, time_source, random_source);

// Inside task: request operation (returns future)
let future: HostOpFuture = channel.request(HostOpKind::ToolCall {
    tool: "stripe:charge".into(),
    params: json!({"amount": 1000}),
});

// Future polls as Pending until host completes
let result: Vec<u8> = future.await;

// From host: get pending operations
let pending: Vec<HostOpRequest> = channel.take_pending();

// From host: complete an operation
channel.complete(op_id, result_bytes);

// From host: complete with error
channel.complete_error(op_id, HostErrorCode::NotFound, "file not found");
```

### Request Structure

Each pending operation includes:

```rust
pub struct HostOpRequest {
    /// Unique operation ID (for completion)
    pub id: HostOpId,

    /// Runtime that owns this operation
    pub runtime_id: u64,

    /// Task that requested (for attribution)
    pub task_id: TaskIdRepr,

    /// The actual operation
    pub kind: HostOpKind,
}
```

### Error Handling

Operations can fail with structured error codes:

```rust
pub enum HostErrorCode {
    NotFound,        // Resource doesn't exist
    PermissionDenied, // Capability check failed
    Timeout,         // Operation timed out
    InvalidInput,    // Bad parameters
    Unsupported,     // Operation not supported
    OutOfMemory,     // Resource exhausted
    AlreadyExists,   // Resource already exists
}
```

These map to `std::io::ErrorKind` for compatibility.

### Concurrent Operations

Multiple tasks can have pending operations simultaneously:

```rust
// Task 1: waiting on tool call
let op1 = channel.request(ToolCall { tool: "a", .. });

// Task 2: waiting on different tool call
let op2 = channel.request(ToolCall { tool: "b", .. });

// Host sees both in take_pending()
// Can complete in any order
channel.complete(op2.id, result_b);  // Task 2 wakes first
channel.complete(op1.id, result_a);  // Task 1 wakes
```

The scheduler handles waking the correct task when its operation completes.

## Time Control

The host provides time via injected source function:

```rust
let time_source: TimeSourceFn = Rc::new(move |runtime_id, clock| {
    match clock {
        ClockType::Realtime => wall_clock_nanos(),
        ClockType::Monotonic => elapsed_since_start(),
    }
});

let sched = Scheduler::new(runtime_id, time_source, random_source);
```

For deterministic execution, the host provides recorded timestamps:

```rust
let recorded_times = vec![/* from replay log */];
let idx = Cell::new(0);
let time_source = Rc::new(move |_, _| {
    let t = recorded_times[idx.get()];
    idx.set(idx.get() + 1);
    t
});
```

## Waker Implementation

Custom `RawWaker` for single-threaded WASM:

```rust
// waker.rs - the only unsafe code in amla-*
unsafe fn clone(data: *const ()) -> RawWaker { /* ... */ }
unsafe fn wake(data: *const ()) { /* ... */ }
unsafe fn drop(_: *const ()) { /* ... */ }

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop);
```

When a task's waker is called, its ID is added to the ready queue.

## Thread Safety

**This scheduler is `!Send` and `!Sync` by design.**

All types use `Rc<RefCell<_>>` internally. Do not share across threads. For multi-threaded execution, create separate scheduler instances per thread.

## API

### Scheduler

```rust
// Create
let sched = Scheduler::new(runtime_id, time_source, random_source);

// Spawn task
let handle: TaskHandle = sched.spawn(async { Ok(Exit::success()) });

// Run until blocked or done
let state: SchedulerState = sched.run();

// Access host channel
let channel: HostChannelRef = sched.host();

// Cancel task
sched.cancel(task_id);
```

### TaskHandle

```rust
// Await task completion
let result: Result<Exit, Error> = handle.await?;

// Get task ID
let id: TaskId = handle.id();
```

### HostChannel

```rust
// Request operations
let future = channel.tool_call("tool", params);
let future = channel.sleep(duration);
let future = channel.file_read(path);

// Complete pending operations (host side)
let pending: Vec<HostOpRequest> = channel.take_pending();
channel.complete(op_id, result);
```

## Building

```bash
cargo build -p amla-scheduler
cargo test -p amla-scheduler
cargo bench -p amla-scheduler
```

## License

AGPL-3.0-or-later OR BUSL-1.1
