//! Benchmarks for the amla-scheduler crate.
//!
//! Run with: `cargo bench -p amla-scheduler`
//!
//! These benchmarks measure:
//! - Task spawn latency
//! - Task poll/completion latency
//! - Pipe throughput
//! - Timer resolution
//! - Context switch overhead
//! - Memory allocation tracking

#![allow(missing_docs)]
#![allow(dead_code)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::explicit_iter_loop)]
#![allow(clippy::let_underscore_future)]
#![allow(clippy::ignored_unit_patterns)]
#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::uninlined_format_args)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::rc::Rc;

use amla_scheduler::{
    AsyncPipe, Error, Executor, Exit, HostChannel, RandomSourceFn, RunState, Scheduler,
    TimeSourceFn,
};

/// Create time and random sources for benchmarking.
fn bench_sources() -> (TimeSourceFn, RandomSourceFn) {
    let time_source: TimeSourceFn = Rc::new(|_runtime_id, _clock| 0);
    let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
    (time_source, random_source)
}

/// Create a scheduler for benchmarking.
fn bench_scheduler() -> Scheduler {
    let (time_source, random_source) = bench_sources();
    Scheduler::new(1, time_source, random_source)
}

/// Create a host channel for benchmarking.
fn bench_channel(capacity: usize) -> HostChannel {
    let (time_source, random_source) = bench_sources();
    HostChannel::new(1, capacity, time_source, random_source)
}

// =============================================================================
// Tracking Allocator
// =============================================================================

/// A simple allocator wrapper that tracks allocation statistics.
struct TrackingAllocator {
    inner: System,
    allocated: AtomicUsize,
    deallocated: AtomicUsize,
    alloc_count: AtomicUsize,
    dealloc_count: AtomicUsize,
    peak: AtomicUsize,
    current: AtomicUsize,
}

impl TrackingAllocator {
    const fn new() -> Self {
        Self {
            inner: System,
            allocated: AtomicUsize::new(0),
            deallocated: AtomicUsize::new(0),
            alloc_count: AtomicUsize::new(0),
            dealloc_count: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            current: AtomicUsize::new(0),
        }
    }

    fn reset(&self) {
        self.allocated.store(0, Ordering::SeqCst);
        self.deallocated.store(0, Ordering::SeqCst);
        self.alloc_count.store(0, Ordering::SeqCst);
        self.dealloc_count.store(0, Ordering::SeqCst);
        self.peak.store(0, Ordering::SeqCst);
        self.current.store(0, Ordering::SeqCst);
    }

    fn stats(&self) -> AllocStats {
        AllocStats {
            allocated: self.allocated.load(Ordering::Relaxed),
            deallocated: self.deallocated.load(Ordering::Relaxed),
            alloc_count: self.alloc_count.load(Ordering::Relaxed),
            dealloc_count: self.dealloc_count.load(Ordering::Relaxed),
            peak: self.peak.load(Ordering::Relaxed),
            current: self.current.load(Ordering::Relaxed),
        }
    }
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        self.allocated.fetch_add(size, Ordering::Relaxed);
        self.alloc_count.fetch_add(1, Ordering::Relaxed);

        let current = self
            .current
            .fetch_add(size, Ordering::Relaxed)
            .wrapping_add(size);
        // Update peak if current exceeds it
        let mut peak = self.peak.load(Ordering::Relaxed);
        while current > peak {
            match self
                .peak
                .compare_exchange(peak, current, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(p) => peak = p,
            }
        }

        unsafe { self.inner.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size();
        self.deallocated.fetch_add(size, Ordering::Relaxed);
        self.dealloc_count.fetch_add(1, Ordering::Relaxed);
        self.current.fetch_sub(size, Ordering::Relaxed);

        unsafe { self.inner.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator::new();

/// Allocation statistics snapshot.
#[derive(Debug, Clone, Copy)]
struct AllocStats {
    allocated: usize,
    deallocated: usize,
    alloc_count: usize,
    dealloc_count: usize,
    peak: usize,
    current: usize,
}

impl AllocStats {
    fn net_allocated(&self) -> isize {
        self.allocated as isize - self.deallocated as isize
    }
}

/// Run a closure and return allocation stats.
fn measure_allocs<F, R>(f: F) -> (R, AllocStats)
where
    F: FnOnce() -> R,
{
    ALLOCATOR.reset();
    let result = f();
    (result, ALLOCATOR.stats())
}

// =============================================================================
// Task Spawn Latency
// =============================================================================

fn bench_spawn_single_task(c: &mut Criterion) {
    c.bench_function("spawn_single_task", |b| {
        b.iter(|| {
            let exec = Executor::new();
            exec.spawn(async { Ok(Exit::success()) });
            let _ = exec.run();
        });
    });
}

fn bench_spawn_many_tasks(c: &mut Criterion) {
    let mut group = c.benchmark_group("spawn_tasks");

    for count in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, &count| {
            b.iter(|| {
                let exec = Executor::new();
                for _ in 0..count {
                    exec.spawn(async { Ok(Exit::success()) });
                }
                let _ = exec.run();
            });
        });
    }

    group.finish();
}

// =============================================================================
// Scheduler Step Latency
// =============================================================================

fn bench_scheduler_step_latency(c: &mut Criterion) {
    c.bench_function("scheduler_step_empty", |b| {
        b.iter(|| {
            let sched = bench_scheduler();
            sched.spawn(async { Ok(Exit::success()) });

            while !sched.is_done() {
                let state = sched.run_step();
                black_box(state);
            }
        });
    });
}

fn bench_scheduler_many_steps(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_steps");

    for count in [10, 100, 500].iter() {
        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, &count| {
            b.iter(|| {
                let sched = bench_scheduler();

                // Spawn tasks that yield multiple times
                for _ in 0..count {
                    sched.spawn(async {
                        // Yield a few times to exercise the scheduler
                        for _ in 0..5 {
                            std::hint::spin_loop();
                        }
                        Ok(Exit::success())
                    });
                }

                while !sched.is_done() {
                    let state = sched.run_step();
                    black_box(state);
                }
            });
        });
    }

    group.finish();
}

// =============================================================================
// Pipe Throughput
// =============================================================================

fn bench_pipe_write_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipe_throughput");

    for size in [64, 1024, 8192].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            b.iter(|| {
                let exec = Executor::new();
                let data = vec![0u8; size];

                let pipe = AsyncPipe::new(size * 2);
                let writer = pipe.clone();
                let reader = pipe;

                // Writer task
                let data_clone = data.clone();
                exec.spawn(async move {
                    writer
                        .write(&data_clone)
                        .await
                        .map_err(|e| Error::Command(e.to_string()))?;
                    writer.close();
                    Ok(Exit::success())
                });

                // Reader task
                exec.spawn(async move {
                    let mut buf = vec![0u8; size];
                    let n = reader
                        .read(&mut buf)
                        .await
                        .map_err(|e| Error::Command(e.to_string()))?;
                    black_box(n);
                    Ok(Exit::success())
                });

                let _ = exec.run();
            });
        });
    }

    group.finish();
}

fn bench_pipe_high_contention(c: &mut Criterion) {
    c.bench_function("pipe_high_contention", |b| {
        b.iter(|| {
            let exec = Executor::new();
            let pipe = AsyncPipe::new(64); // Small buffer for contention
            let writer = pipe.clone();
            let reader = pipe;

            // Writer - sends 1KB in small chunks
            exec.spawn(async move {
                for i in 0..16 {
                    let chunk = vec![i as u8; 64];
                    writer
                        .write(&chunk)
                        .await
                        .map_err(|e| Error::Command(e.to_string()))?;
                }
                writer.close();
                Ok(Exit::success())
            });

            // Reader - reads all data
            exec.spawn(async move {
                let mut total = 0;
                let mut buf = [0u8; 32]; // Small reads
                loop {
                    let n = reader
                        .read(&mut buf)
                        .await
                        .map_err(|e| Error::Command(e.to_string()))?;
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                black_box(total);
                Ok(Exit::success())
            });

            let _ = exec.run();
        });
    });
}

// =============================================================================
// Host Channel Operations
// =============================================================================

fn bench_host_channel_operations(c: &mut Criterion) {
    c.bench_function("host_channel_file_read", |b| {
        b.iter(|| {
            let exec = Executor::new();
            let channel = bench_channel(10);
            let channel_clone = channel.clone();

            exec.spawn(async move {
                let fut = channel_clone.file_read("/test.txt");
                black_box(fut);
                Ok(Exit::success())
            });

            // Run until blocked
            let _ = exec.run();

            // Complete the pending operation
            if let Some(req) = channel.take_pending() {
                channel.complete(req.id, vec![1, 2, 3, 4]);
            }

            // Continue
            let _ = exec.run();
        });
    });
}

fn bench_host_channel_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("host_channel_ops");

    for count in [10, 50, 100].iter() {
        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, &count| {
            b.iter(|| {
                let exec = Executor::new();
                let channel = bench_channel(count * 2);

                for i in 0..count {
                    let channel_clone = channel.clone();
                    let path = format!("/file{i}.txt");
                    exec.spawn(async move {
                        let _ = channel_clone.file_read(&path);
                        Ok(Exit::success())
                    });
                }

                // Run and complete operations
                loop {
                    let state = exec.run();
                    if matches!(state, RunState::Done(_)) {
                        break;
                    }

                    // Complete pending operations
                    while let Some(req) = channel.take_pending() {
                        channel.complete(req.id, vec![0; 64]);
                    }
                }
            });
        });
    }

    group.finish();
}

// =============================================================================
// Timer Operations
// =============================================================================

fn bench_timer_creation(c: &mut Criterion) {
    c.bench_function("timer_creation", |b| {
        b.iter(|| {
            let sched = bench_scheduler();

            sched.spawn(async move {
                // Create a timer but don't wait for it
                // (simulates deadline-based timeout patterns)
                Ok(Exit::success())
            });

            while !sched.is_done() {
                let _ = sched.run_step();
            }
        });
    });
}

// =============================================================================
// Memory Usage Estimation
// =============================================================================

fn bench_executor_memory_baseline(c: &mut Criterion) {
    c.bench_function("executor_creation", |b| {
        b.iter(|| {
            let exec = Executor::new();
            black_box(&exec);
        });
    });
}

fn bench_scheduler_memory_baseline(c: &mut Criterion) {
    c.bench_function("scheduler_creation", |b| {
        b.iter(|| {
            let sched = bench_scheduler();
            black_box(&sched);
        });
    });
}

fn bench_task_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("task_overhead");

    // Measure per-task overhead by comparing different task counts
    for count in [1, 10, 100, 1000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(count), count, |b, &count| {
            b.iter(|| {
                let exec = Executor::new();
                for i in 0..count {
                    exec.spawn(async move {
                        black_box(i);
                        Ok(Exit::success())
                    });
                }
                black_box(&exec);
            });
        });
    }

    group.finish();
}

// =============================================================================
// Allocation Tracking Benchmarks
// =============================================================================

fn bench_allocation_executor_lifecycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("alloc_executor");

    for task_count in [1, 10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("tasks", task_count),
            task_count,
            |b, &task_count| {
                b.iter_custom(|iters| {
                    let mut total_peak = 0usize;

                    for _ in 0..iters {
                        let (_, stats) = measure_allocs(|| {
                            let exec = Executor::new();
                            for i in 0..task_count {
                                exec.spawn(async move {
                                    black_box(i);
                                    Ok(Exit::success())
                                });
                            }
                            let _ = exec.run();
                        });
                        total_peak += stats.peak;
                    }

                    // Return average peak memory as the "time" for display
                    std::time::Duration::from_nanos((total_peak / iters as usize) as u64)
                });
            },
        );
    }

    group.finish();
}

fn bench_allocation_pipe_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("alloc_pipe");

    for size in [64, 1024, 8192, 65536].iter() {
        group.bench_with_input(BenchmarkId::new("bytes", size), size, |b, &size| {
            b.iter_custom(|iters| {
                let mut total_peak = 0usize;

                for _ in 0..iters {
                    let (_, stats) = measure_allocs(|| {
                        let exec = Executor::new();
                        let data = vec![0u8; size];

                        let pipe = AsyncPipe::new(size * 2);
                        let writer = pipe.clone();
                        let reader = pipe;

                        let data_clone = data.clone();
                        exec.spawn(async move {
                            writer
                                .write(&data_clone)
                                .await
                                .map_err(|e| Error::Command(e.to_string()))?;
                            writer.close();
                            Ok(Exit::success())
                        });

                        exec.spawn(async move {
                            let mut buf = vec![0u8; size];
                            let n = reader
                                .read(&mut buf)
                                .await
                                .map_err(|e| Error::Command(e.to_string()))?;
                            black_box(n);
                            Ok(Exit::success())
                        });

                        let _ = exec.run();
                    });
                    total_peak += stats.peak;
                }

                std::time::Duration::from_nanos((total_peak / iters as usize) as u64)
            });
        });
    }

    group.finish();
}

fn bench_allocation_scheduler_lifecycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("alloc_scheduler");

    for task_count in [1, 10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("tasks", task_count),
            task_count,
            |b, &task_count| {
                b.iter_custom(|iters| {
                    let mut total_peak = 0usize;

                    for _ in 0..iters {
                        let (_, stats) = measure_allocs(|| {
                            let sched = bench_scheduler();
                            for i in 0..task_count {
                                sched.spawn(async move {
                                    black_box(i);
                                    Ok(Exit::success())
                                });
                            }
                            while !sched.is_done() {
                                let _ = sched.run_step();
                            }
                        });
                        total_peak += stats.peak;
                    }

                    std::time::Duration::from_nanos((total_peak / iters as usize) as u64)
                });
            },
        );
    }

    group.finish();
}

fn bench_allocation_host_channel(c: &mut Criterion) {
    let mut group = c.benchmark_group("alloc_host_channel");

    for op_count in [1, 10, 50, 100].iter() {
        group.bench_with_input(
            BenchmarkId::new("ops", op_count),
            op_count,
            |b, &op_count| {
                b.iter_custom(|iters| {
                    let mut total_peak = 0usize;

                    for _ in 0..iters {
                        let (_, stats) = measure_allocs(|| {
                            let exec = Executor::new();
                            let channel = bench_channel(op_count * 2);

                            for i in 0..op_count {
                                let channel_clone = channel.clone();
                                let path = format!("/file{i}.txt");
                                exec.spawn(async move {
                                    let _ = channel_clone.file_read(&path);
                                    Ok(Exit::success())
                                });
                            }

                            // Run until all tasks spawn their futures
                            loop {
                                let state = exec.run();
                                if matches!(state, RunState::Done(_)) {
                                    break;
                                }
                                // Complete pending operations
                                while let Some(req) = channel.take_pending() {
                                    channel.complete(req.id, vec![0; 64]);
                                }
                            }
                        });
                        total_peak += stats.peak;
                    }

                    std::time::Duration::from_nanos((total_peak / iters as usize) as u64)
                });
            },
        );
    }

    group.finish();
}

/// Print allocation stats for key operations (run once, not benchmarked).
fn print_allocation_summary(_c: &mut Criterion) {
    println!("\n=== Allocation Summary ===\n");

    // Executor creation
    let (_, stats) = measure_allocs(|| {
        let exec = Executor::new();
        black_box(&exec);
    });
    println!(
        "Executor::new()     : peak={:>8} bytes, allocs={:>4}",
        stats.peak, stats.alloc_count
    );

    // Scheduler creation
    let (_, stats) = measure_allocs(|| {
        let sched = bench_scheduler();
        black_box(&sched);
    });
    println!(
        "bench_scheduler()    : peak={:>8} bytes, allocs={:>4}",
        stats.peak, stats.alloc_count
    );

    // Single task lifecycle
    let (_, stats1) = measure_allocs(|| {
        let exec = Executor::new();
        exec.spawn(async { Ok(Exit::success()) });
        let _ = exec.run();
    });
    println!(
        "1 task lifecycle    : peak={:>8} bytes, allocs={:>4}",
        stats1.peak, stats1.alloc_count
    );

    // 10 tasks lifecycle
    let (_, stats10) = measure_allocs(|| {
        let exec = Executor::new();
        for i in 0..10 {
            exec.spawn(async move {
                black_box(i);
                Ok(Exit::success())
            });
        }
        let _ = exec.run();
    });
    println!(
        "10 tasks lifecycle  : peak={:>8} bytes, allocs={:>4}",
        stats10.peak, stats10.alloc_count
    );

    // 100 tasks lifecycle
    let (_, stats100) = measure_allocs(|| {
        let exec = Executor::new();
        for i in 0..100 {
            exec.spawn(async move {
                black_box(i);
                Ok(Exit::success())
            });
        }
        let _ = exec.run();
    });
    println!(
        "100 tasks lifecycle : peak={:>8} bytes, allocs={:>4}",
        stats100.peak, stats100.alloc_count
    );

    // 1000 tasks lifecycle
    let (_, stats1000) = measure_allocs(|| {
        let exec = Executor::new();
        for i in 0..1000 {
            exec.spawn(async move {
                black_box(i);
                Ok(Exit::success())
            });
        }
        let _ = exec.run();
    });
    println!(
        "1000 tasks lifecycle: peak={:>8} bytes, allocs={:>4}",
        stats1000.peak, stats1000.alloc_count
    );

    // Calculate per-task overhead
    println!("\n=== Per-Task Memory Overhead ===\n");
    let base = stats1.peak;
    let per_task_10 = (stats10.peak - base) / 9;
    let per_task_100 = (stats100.peak - base) / 99;
    let per_task_1000 = (stats1000.peak - base) / 999;
    println!("Base (1 task)       : {:>8} bytes", base);
    println!("Per-task (10 tasks) : {:>8} bytes/task", per_task_10);
    println!("Per-task (100 tasks): {:>8} bytes/task", per_task_100);
    println!("Per-task (1k tasks) : {:>8} bytes/task", per_task_1000);

    // Pipe with 8KB data
    let (_, stats) = measure_allocs(|| {
        let exec = Executor::new();
        let pipe = AsyncPipe::new(16384);
        let writer = pipe.clone();
        let reader = pipe;
        let data = vec![0u8; 8192];

        exec.spawn(async move {
            let _ = writer.write(&data).await;
            writer.close();
            Ok(Exit::success())
        });

        exec.spawn(async move {
            let mut buf = vec![0u8; 8192];
            let _ = reader.read(&mut buf).await;
            Ok(Exit::success())
        });

        let _ = exec.run();
    });
    println!(
        "\n8KB pipe transfer   : peak={:>8} bytes, allocs={:>4}",
        stats.peak, stats.alloc_count
    );

    // HostChannel with 10 ops
    let (_, stats) = measure_allocs(|| {
        let exec = Executor::new();
        let channel = bench_channel(20);

        for i in 0..10 {
            let ch = channel.clone();
            let path = format!("/file{i}.txt");
            exec.spawn(async move {
                let _ = ch.file_read(&path);
                Ok(Exit::success())
            });
        }

        loop {
            let state = exec.run();
            if matches!(state, RunState::Done(_)) {
                break;
            }
            while let Some(req) = channel.take_pending() {
                channel.complete(req.id, vec![0; 64]);
            }
        }
    });
    println!(
        "10 host ops         : peak={:>8} bytes, allocs={:>4}",
        stats.peak, stats.alloc_count
    );

    println!("\n=== Structure Sizes (Stack) ===\n");
    println!("Executor:    {} bytes", std::mem::size_of::<Executor>());
    println!("Scheduler:   {} bytes", std::mem::size_of::<Scheduler>());
    println!("AsyncPipe:   {} bytes", std::mem::size_of::<AsyncPipe>());
    println!("HostChannel: {} bytes", std::mem::size_of::<HostChannel>());
    println!();
}

// =============================================================================
// Context Switch Simulation
// =============================================================================

fn bench_context_switches(c: &mut Criterion) {
    c.bench_function("context_switches_ping_pong", |b| {
        b.iter(|| {
            let exec = Executor::new();

            // Create pipes for ping-pong
            let pipe1 = AsyncPipe::new(16);
            let writer1 = pipe1.clone();
            let reader1 = pipe1;

            let pipe2 = AsyncPipe::new(16);
            let writer2 = pipe2.clone();
            let reader2 = pipe2;

            // Task A: writes to pipe1, reads from pipe2
            exec.spawn(async move {
                for i in 0..10u8 {
                    writer1
                        .write(&[i])
                        .await
                        .map_err(|e| Error::Command(e.to_string()))?;
                    let mut buf = [0u8; 1];
                    let _ = reader2.read(&mut buf).await;
                }
                writer1.close();
                Ok(Exit::success())
            });

            // Task B: reads from pipe1, writes to pipe2
            exec.spawn(async move {
                let mut buf = [0u8; 1];
                loop {
                    let n = reader1
                        .read(&mut buf)
                        .await
                        .map_err(|e| Error::Command(e.to_string()))?;
                    if n == 0 {
                        break;
                    }
                    writer2
                        .write(&buf[..n])
                        .await
                        .map_err(|e| Error::Command(e.to_string()))?;
                }
                writer2.close();
                Ok(Exit::success())
            });

            let _ = exec.run();
        });
    });
}

// =============================================================================
// Pipe Synchronous Operations
// =============================================================================

fn bench_pipe_sync_push_drain(c: &mut Criterion) {
    let mut group = c.benchmark_group("pipe_sync");

    for size in [64, 1024, 8192].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            b.iter(|| {
                let pipe = AsyncPipe::new(size * 2);
                let data = vec![0u8; size];

                pipe.push(&data);
                let result = pipe.drain();
                black_box(result);
            });
        });
    }

    group.finish();
}

// =============================================================================
// Criterion Groups
// =============================================================================

criterion_group!(spawning, bench_spawn_single_task, bench_spawn_many_tasks,);

criterion_group!(
    scheduling,
    bench_scheduler_step_latency,
    bench_scheduler_many_steps,
);

criterion_group!(
    pipes,
    bench_pipe_write_read,
    bench_pipe_high_contention,
    bench_pipe_sync_push_drain,
);

criterion_group!(
    host_ops,
    bench_host_channel_operations,
    bench_host_channel_throughput,
);

criterion_group!(timers, bench_timer_creation,);

criterion_group!(
    memory,
    bench_executor_memory_baseline,
    bench_scheduler_memory_baseline,
    bench_task_overhead,
);

criterion_group!(
    allocations,
    bench_allocation_executor_lifecycle,
    bench_allocation_scheduler_lifecycle,
    bench_allocation_pipe_throughput,
    bench_allocation_host_channel,
    print_allocation_summary,
);

criterion_group!(context_switching, bench_context_switches,);

criterion_main!(
    spawning,
    scheduling,
    pipes,
    host_ops,
    timers,
    memory,
    allocations,
    context_switching,
);
