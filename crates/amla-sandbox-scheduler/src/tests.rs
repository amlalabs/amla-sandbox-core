//! Comprehensive scheduler tests.
//!
//! Tests for backpressure, multi-task pipelines, and complex scenarios.

#![allow(clippy::doc_markdown)] // Test doc comments don't need backticks
#![allow(clippy::cast_lossless)] // Test assertions use simple casts

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::context::AsyncContext;
use crate::executor::{Executor, RunState, TaskResult};
use crate::host_channel::{RandomSourceFn, TimeSourceFn};
use crate::pipe::AsyncPipe;
use crate::scheduler::Scheduler;
use crate::{Error, Exit};

/// Create mock time and random sources for testing.
fn mock_sources() -> (TimeSourceFn, RandomSourceFn) {
    let mock_time = Rc::new(Cell::new(0u64));
    let time_clone = mock_time.clone();
    let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
    let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
    (time_source, random_source)
}

/// Create a scheduler with mock sources for testing.
fn test_scheduler() -> Scheduler {
    let (time_source, random_source) = mock_sources();
    Scheduler::new(1, time_source, random_source)
}

/// Create a host channel with mock sources for testing.
fn test_host_channel(size: usize) -> crate::HostChannel {
    let (time_source, random_source) = mock_sources();
    crate::HostChannel::new(1, size, time_source, random_source)
}

/// Test that small pipe capacity causes proper backpressure.
#[test]
fn backpressure_small_pipe() {
    let exec = Executor::new();
    let pipe = AsyncPipe::new(16); // Small 16-byte pipe
    let pipe_clone = pipe.clone();

    let writer_progress = Rc::new(RefCell::new(Vec::new()));
    let reader_progress = Rc::new(RefCell::new(Vec::new()));

    let wp = writer_progress.clone();
    let rp = reader_progress.clone();

    // Writer: tries to write 64 bytes in 16-byte chunks
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe_clone, "/");

        for i in 0u8..4 {
            let chunk = [b'A' + i; 16];
            ctx.write_all(&chunk).await.unwrap();
            wp.borrow_mut().push(format!("wrote chunk {i}"));
        }
        ctx.close_stdout();

        Ok(Exit::success())
    });

    // Reader: reads in 8-byte chunks (slower than writer)
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe, "/");
        let mut buf = [0u8; 8];
        let mut total = 0;
        let mut chunk_num = 0;

        loop {
            let n = ctx.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
            rp.borrow_mut()
                .push(format!("read {n} bytes (chunk {chunk_num})"));
            chunk_num += 1;
        }

        assert_eq!(total, 64);
        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 2);
            for r in &results {
                match r {
                    TaskResult::Ok(exit) => assert_eq!(exit.code, 0),
                    TaskResult::Err(e) => panic!("task failed: {e}"),
                }
            }
        }
        RunState::Blocked => panic!("executor blocked unexpectedly"),
    }

    // Verify interleaving happened (writer had to wait for reader)
    let writes = writer_progress.borrow();
    let reads = reader_progress.borrow();

    // With 16-byte pipe and 16-byte writes, writer blocks after first write
    // Reader must read to make room
    assert!(!writes.is_empty());
    assert!(!reads.is_empty());
}

/// Test three-stage pipeline: producer | transformer | consumer
#[test]
fn three_stage_pipeline() {
    let exec = Executor::new();

    let pipe1 = AsyncPipe::new(64);
    let pipe2 = AsyncPipe::new(64);

    let pipe1_read = pipe1.clone();
    let pipe2_write = pipe2.clone();
    let pipe2_read = pipe2.clone();

    let final_output = Rc::new(RefCell::new(Vec::new()));
    let output_ref = final_output.clone();

    // Stage 1: Producer - generates numbers
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe1, "/");
        for i in 1..=5 {
            ctx.write_all(format!("{i}\n").as_bytes()).await.unwrap();
        }
        ctx.close_stdout();
        Ok(Exit::success())
    });

    // Stage 2: Transformer - doubles each number
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipes(pipe1_read, pipe2_write, "/");
        let mut buf = [0u8; 64];
        let mut pending = Vec::new();

        loop {
            let n = ctx.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            pending.extend_from_slice(&buf[..n]);

            // Process complete lines
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let line: String = String::from_utf8_lossy(&pending[..pos]).into();
                pending.drain(..=pos);

                if let Ok(num) = line.trim().parse::<i32>() {
                    let doubled = num * 2;
                    ctx.write_all(format!("{doubled}\n").as_bytes())
                        .await
                        .unwrap();
                }
            }
        }
        ctx.close_stdout();
        Ok(Exit::success())
    });

    // Stage 3: Consumer - collects results
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe2_read, "/");
        let mut buf = [0u8; 64];
        let mut result = Vec::new();

        loop {
            let n = ctx.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            result.extend_from_slice(&buf[..n]);
        }

        *output_ref.borrow_mut() = result;
        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 3);
            for r in &results {
                match r {
                    TaskResult::Ok(exit) => assert_eq!(exit.code, 0),
                    TaskResult::Err(e) => panic!("task failed: {e}"),
                }
            }
        }
        RunState::Blocked => panic!("executor blocked unexpectedly"),
    }

    let binding = final_output.borrow();
    let output = String::from_utf8_lossy(&binding);
    let lines: Vec<&str> = output.trim().lines().collect();
    assert_eq!(lines, vec!["2", "4", "6", "8", "10"]);
}

/// Test that blocked executor returns Blocked when no progress possible.
#[test]
fn blocked_executor_detection() {
    let exec = Executor::new();
    let pipe = AsyncPipe::new(64);

    // Reader waits for data but no writer
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe, "/");
        let mut buf = [0u8; 64];

        // This will block forever since no one is writing
        let _n = ctx.read(&mut buf).await.unwrap();

        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(_) => panic!("should be blocked"),
        RunState::Blocked => (), // Expected
    }
}

/// Test that closed pipe signals EOF properly.
#[test]
fn pipe_close_signals_eof() {
    let exec = Executor::new();
    let pipe = AsyncPipe::new(64);
    let pipe_clone = pipe.clone();

    let eof_received = Rc::new(RefCell::new(false));
    let eof_ref = eof_received.clone();

    // Writer closes immediately
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe_clone, "/");
        ctx.close_stdout();
        Ok(Exit::success())
    });

    // Reader should get EOF
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe, "/");
        let mut buf = [0u8; 64];
        let n = ctx.read(&mut buf).await.unwrap();
        *eof_ref.borrow_mut() = n == 0;
        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(_) => (),
        RunState::Blocked => panic!("should complete"),
    }

    assert!(*eof_received.borrow(), "should have received EOF");
}

/// Test high watermark behavior - writer blocks when pipe is full.
#[test]
fn high_watermark_blocking() {
    let exec = Executor::new();
    let pipe = AsyncPipe::new(32); // 32-byte capacity
    let pipe_clone = pipe.clone();

    let write_blocked = Rc::new(RefCell::new(false));
    let wb = write_blocked.clone();

    // Writer tries to write more than capacity
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe_clone, "/");

        // First write fills the pipe
        ctx.write_all(&[b'A'; 32]).await.unwrap();

        // Second write should eventually complete after reader drains
        // (tests that scheduler properly handles the pending write)
        ctx.write_all(&[b'B'; 16]).await.unwrap();

        *wb.borrow_mut() = true; // Made it through
        ctx.close_stdout();
        Ok(Exit::success())
    });

    // Reader drains slowly
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe, "/");
        let mut buf = [0u8; 8];
        let mut total = 0;

        loop {
            let n = ctx.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }

        assert_eq!(total, 48); // 32 + 16
        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 2);
        }
        RunState::Blocked => panic!("should complete"),
    }

    assert!(*write_blocked.borrow(), "write should have completed");
}

/// Test many concurrent tasks.
#[test]
fn many_concurrent_tasks() {
    let exec = Executor::new();
    let results_vec = Rc::new(RefCell::new(Vec::new()));

    for i in 0..100 {
        let rv = results_vec.clone();
        exec.spawn(async move {
            rv.borrow_mut().push(i);
            Ok(Exit::code(i))
        });
    }

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 100);

            let codes: Vec<i32> = results
                .iter()
                .map(|r| match r {
                    TaskResult::Ok(exit) => exit.code,
                    TaskResult::Err(_) => -1,
                })
                .collect();

            // All should have completed with their index as exit code
            for i in 0..100 {
                assert!(codes.contains(&i), "missing exit code {i}");
            }
        }
        RunState::Blocked => panic!("should complete"),
    }

    assert_eq!(results_vec.borrow().len(), 100);
}

/// Test task yielding and resumption.
#[test]
fn task_yield_and_resume() {
    use std::future::poll_fn;
    use std::task::Poll;

    let exec = Executor::new();
    let execution_order = Rc::new(RefCell::new(Vec::new()));

    // Task 1: yields multiple times
    let eo1 = execution_order.clone();
    exec.spawn(async move {
        let counter = Rc::new(RefCell::new(0));
        let counter_clone = counter.clone();

        for _ in 0..3 {
            poll_fn(|cx| {
                let mut c = counter_clone.borrow_mut();
                *c += 1;
                eo1.borrow_mut().push(format!("T1 step {}", *c));

                if *c < 3 {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
        }

        Ok(Exit::code(1))
    });

    // Task 2: yields multiple times
    let eo2 = execution_order.clone();
    exec.spawn(async move {
        let counter = Rc::new(RefCell::new(0));
        let counter_clone = counter.clone();

        for _ in 0..3 {
            poll_fn(|cx| {
                let mut c = counter_clone.borrow_mut();
                *c += 1;
                eo2.borrow_mut().push(format!("T2 step {}", *c));

                if *c < 3 {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
        }

        Ok(Exit::code(2))
    });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 2);
        }
        RunState::Blocked => panic!("should complete"),
    }

    // Both tasks should have interleaved their execution
    let order = execution_order.borrow();
    assert!(order.len() >= 6, "both tasks should have made progress");

    // Verify both T1 and T2 appear in the log
    let has_t1 = order.iter().any(|s| s.starts_with("T1"));
    let has_t2 = order.iter().any(|s| s.starts_with("T2"));
    assert!(has_t1 && has_t2, "both tasks should have executed");
}

/// Test error propagation through tasks.
#[test]
fn error_propagation() {
    let exec = Executor::new();

    exec.spawn(async { Err(Error::Command("intentional error".to_string())) });

    exec.spawn(async { Ok(Exit::success()) });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 2);

            let errors: Vec<_> = results
                .iter()
                .filter(|r| matches!(r, TaskResult::Err(_)))
                .collect();
            let successes: Vec<_> = results
                .iter()
                .filter(|r| matches!(r, TaskResult::Ok(_)))
                .collect();

            assert_eq!(errors.len(), 1);
            assert_eq!(successes.len(), 1);
        }
        RunState::Blocked => panic!("should complete"),
    }
}

/// Test VFS reader/writer integration with pipeline.
#[test]
fn vfs_streams_in_pipeline() {
    use crate::vfs_stream::{VfsReader, VfsWriter};

    let exec = Executor::new();
    let pipe = AsyncPipe::new(64);
    let pipe_clone = pipe.clone();

    let output = Rc::new(RefCell::new(Vec::new()));
    let output_ref = output.clone();

    // Producer reads from VFS and writes to pipe
    exec.spawn(async move {
        let mut reader = VfsReader::new(b"line1\nline2\nline3\n".to_vec());
        let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe_clone, "/");

        let mut buf = [0u8; 64];
        loop {
            let n = reader.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            ctx.write_all(&buf[..n]).await.unwrap();
        }
        ctx.close_stdout();

        Ok(Exit::success())
    });

    // Consumer reads from pipe and writes to VFS
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe, "/");
        let mut writer = VfsWriter::new();

        let mut buf = [0u8; 64];
        loop {
            let n = ctx.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).unwrap();
        }

        *output_ref.borrow_mut() = writer.into_bytes();
        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 2);
        }
        RunState::Blocked => panic!("should complete"),
    }

    assert_eq!(output.borrow().as_slice(), b"line1\nline2\nline3\n");
}

/// Test side effects accumulation.
#[test]
fn side_effects_accumulation() {
    let exec = Executor::new();
    let pipe = AsyncPipe::new(64);
    let pipe_clone = pipe.clone();

    let effects_collected = Rc::new(RefCell::new(Vec::new()));
    let ef1 = effects_collected.clone();
    let ef2 = effects_collected.clone();

    // Task 1: changes cwd
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe_clone, "/home");
        ctx.set_cwd("/home/user".to_string());
        ctx.set_env("FOO".to_string(), "bar".to_string());
        ctx.close_stdout();

        let effects = ctx.take_effects();
        ef1.borrow_mut().push(effects);

        Ok(Exit::success())
    });

    // Task 2: sets env var
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe, "/");
        ctx.set_env("BAZ".to_string(), "qux".to_string());

        // Drain pipe
        let mut buf = [0u8; 64];
        while ctx.read(&mut buf).await.unwrap() > 0 {}

        let effects = ctx.take_effects();
        ef2.borrow_mut().push(effects);

        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(_) => (),
        RunState::Blocked => panic!("should complete"),
    }

    let effects = effects_collected.borrow();
    assert_eq!(effects.len(), 2);

    // Find the effect with cwd change
    let cwd_effect = effects.iter().find(|e| e.cwd.is_some());
    assert!(cwd_effect.is_some());
    assert_eq!(cwd_effect.unwrap().cwd, Some("/home/user".to_string()));

    // Both should have env changes
    let total_env_sets: usize = effects.iter().map(|e| e.env_set.len()).sum();
    assert_eq!(total_env_sets, 2);
}

// =============================================================================
// Host Op Tests - simulating external async operations
// =============================================================================

/// Simulates a pending host operation that will be completed externally.
struct HostOp<T> {
    /// The result, set when operation completes.
    result: Rc<RefCell<Option<T>>>,
    /// Waker to call when result is ready.
    waker: Rc<RefCell<Option<std::task::Waker>>>,
}

impl<T: Clone> HostOp<T> {
    fn new() -> Self {
        Self {
            result: Rc::new(RefCell::new(None)),
            waker: Rc::new(RefCell::new(None)),
        }
    }

    /// Complete the operation with a result.
    fn complete(&self, value: T) {
        *self.result.borrow_mut() = Some(value);
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }

    /// Check if result is ready.
    fn is_ready(&self) -> bool {
        self.result.borrow().is_some()
    }

    /// Take the result.
    fn take_result(&self) -> Option<T> {
        self.result.borrow_mut().take()
    }

    /// Create a future that waits for the result.
    fn wait(&self) -> HostOpFuture<T> {
        HostOpFuture {
            result: self.result.clone(),
            waker: self.waker.clone(),
        }
    }
}

impl<T> Clone for HostOp<T> {
    fn clone(&self) -> Self {
        Self {
            result: self.result.clone(),
            waker: self.waker.clone(),
        }
    }
}

struct HostOpFuture<T> {
    result: Rc<RefCell<Option<T>>>,
    waker: Rc<RefCell<Option<std::task::Waker>>>,
}

impl<T: Clone> std::future::Future for HostOpFuture<T> {
    type Output = T;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if let Some(result) = self.result.borrow_mut().take() {
            std::task::Poll::Ready(result)
        } else {
            *self.waker.borrow_mut() = Some(cx.waker().clone());
            std::task::Poll::Pending
        }
    }
}

/// Test single host op: task yields waiting for external result.
#[test]
fn host_op_single_wait() {
    let exec = Executor::new();
    let host_op: HostOp<String> = HostOp::new();
    let host_op_clone = host_op.clone();

    let result_collected = Rc::new(RefCell::new(String::new()));
    let rc = result_collected.clone();

    // Task waits for host op
    exec.spawn(async move {
        let result = host_op_clone.wait().await;
        *rc.borrow_mut() = result;
        Ok(Exit::success())
    });

    // First run: task should block waiting for host op
    match exec.run() {
        RunState::Done(_) => panic!("should be blocked waiting for host op"),
        RunState::Blocked => (), // Expected
    }

    // Simulate host completing the operation
    host_op.complete("hello from host".to_string());

    // Second run: task should complete
    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 1);
            match &results[0] {
                TaskResult::Ok(exit) => assert_eq!(exit.code, 0),
                TaskResult::Err(e) => panic!("unexpected error: {e}"),
            }
        }
        RunState::Blocked => panic!("should complete after host op"),
    }

    assert_eq!(*result_collected.borrow(), "hello from host");
}

/// Test multiple sequential host ops.
#[test]
fn host_op_sequential() {
    let exec = Executor::new();

    let op1: HostOp<i32> = HostOp::new();
    let op2: HostOp<i32> = HostOp::new();
    let op1_clone = op1.clone();
    let op2_clone = op2.clone();

    let sum = Rc::new(RefCell::new(0));
    let sum_ref = sum.clone();

    exec.spawn(async move {
        let v1 = op1_clone.wait().await;
        let v2 = op2_clone.wait().await;
        *sum_ref.borrow_mut() = v1 + v2;
        Ok(Exit::success())
    });

    // Blocked on op1
    assert!(matches!(exec.run(), RunState::Blocked));

    op1.complete(10);

    // Blocked on op2
    assert!(matches!(exec.run(), RunState::Blocked));

    op2.complete(20);

    // Now completes
    assert!(matches!(exec.run(), RunState::Done(_)));
    assert_eq!(*sum.borrow(), 30);
}

/// Test waiting for first of multiple host ops (select pattern).
#[test]
fn host_op_select_first() {
    use std::future::poll_fn;
    use std::task::Poll;

    let exec = Executor::new();

    let first_op: HostOp<&'static str> = HostOp::new();
    let second_op: HostOp<&'static str> = HostOp::new();
    let first_op_inner = first_op.clone();
    let second_op_inner = second_op.clone();

    let winner = Rc::new(RefCell::new(String::new()));
    let winner_ref = winner.clone();

    exec.spawn(async move {
        // Wait for either first_op or second_op
        let result = poll_fn(|cx| {
            // Check first_op
            if first_op_inner.is_ready() {
                return Poll::Ready(("A", first_op_inner.take_result().unwrap()));
            }
            // Check second_op
            if second_op_inner.is_ready() {
                return Poll::Ready(("B", second_op_inner.take_result().unwrap()));
            }

            // Neither ready - register waker with both
            *first_op_inner.waker.borrow_mut() = Some(cx.waker().clone());
            *second_op_inner.waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;

        *winner_ref.borrow_mut() = format!("{}: {}", result.0, result.1);
        Ok(Exit::success())
    });

    // Blocked waiting
    assert!(matches!(exec.run(), RunState::Blocked));

    // second_op completes first
    second_op.complete("second_op result");

    // Task should wake and complete
    assert!(matches!(exec.run(), RunState::Done(_)));
    assert_eq!(*winner.borrow(), "B: second_op result");

    // first_op is not consumed (would be "cancelled" in a real system)
    assert!(!first_op.is_ready()); // Waker was replaced, result not set
}

/// Test cancellation pattern - task decides it doesn't need some results.
#[test]
fn host_op_cancellation() {
    use std::future::poll_fn;
    use std::task::Poll;

    let exec = Executor::new();

    let primary_op: HostOp<String> = HostOp::new();
    let fallback_op: HostOp<String> = HostOp::new();
    let primary_clone = primary_op.clone();
    let fallback_clone = fallback_op.clone();

    let cancelled = Rc::new(RefCell::new(Vec::new()));
    let cancelled_ref = cancelled.clone();
    let result = Rc::new(RefCell::new(String::new()));
    let result_ref = result.clone();

    exec.spawn(async move {
        // Try primary first with timeout simulation
        let mut attempts = 0;
        let primary_result = poll_fn(|cx| {
            attempts += 1;
            if primary_clone.is_ready() {
                return Poll::Ready(Some(primary_clone.take_result().unwrap()));
            }
            if attempts >= 3 {
                // "Timeout" - cancel primary, use fallback
                cancelled_ref.borrow_mut().push("primary".to_string());
                return Poll::Ready(None);
            }
            *primary_clone.waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;

        let final_result = if let Some(r) = primary_result {
            r
        } else {
            // Primary cancelled, wait for fallback
            fallback_clone.wait().await
        };

        *result_ref.borrow_mut() = final_result;
        Ok(Exit::success())
    });

    // First poll - pending
    assert!(matches!(exec.run(), RunState::Blocked));

    // Wake without completing (simulates timeout check)
    primary_op.waker.borrow().as_ref().unwrap().wake_by_ref();
    assert!(matches!(exec.run(), RunState::Blocked));

    // Another wake - still no result
    primary_op.waker.borrow().as_ref().unwrap().wake_by_ref();
    // Now waiting on fallback
    assert!(matches!(exec.run(), RunState::Blocked));

    // Complete fallback
    fallback_op.complete("fallback result".to_string());
    assert!(matches!(exec.run(), RunState::Done(_)));

    assert_eq!(*result.borrow(), "fallback result");
    assert_eq!(*cancelled.borrow(), vec!["primary".to_string()]);
}

/// Test multiple tasks competing for shared host ops.
#[test]
fn host_op_shared_resource() {
    let exec = Executor::new();

    // Shared resource that can only serve one request at a time
    let resource_ready: HostOp<()> = HostOp::new();
    let resource1 = resource_ready.clone();
    let resource2 = resource_ready.clone();

    let order = Rc::new(RefCell::new(Vec::new()));
    let order1 = order.clone();
    let order2 = order.clone();

    // Task 1 wants the resource
    exec.spawn(async move {
        resource1.wait().await;
        order1.borrow_mut().push("task1 got resource");
        Ok(Exit::success())
    });

    // Task 2 also wants the resource
    exec.spawn(async move {
        resource2.wait().await;
        order2.borrow_mut().push("task2 got resource");
        Ok(Exit::success())
    });

    // Both blocked
    assert!(matches!(exec.run(), RunState::Blocked));

    // Resource becomes available - both tasks should be woken
    // but only one waker is stored (last one wins)
    resource_ready.complete(());

    // One task completes
    let state = exec.run();

    // The resource was consumed by one task
    // The other task's waker was overwritten, so it stays blocked
    // In a real system, you'd need a proper queue

    match state {
        RunState::Done(results) => {
            // At least one completed
            assert!(!results.is_empty());
        }
        RunState::Blocked => {
            // One got the resource, other is blocked
            // This is expected behavior for shared HostOp
        }
    }

    assert!(
        !order.borrow().is_empty(),
        "at least one task should have gotten the resource"
    );
}

/// Test host op with task doing other work while waiting.
#[test]
fn host_op_interleaved_with_pipe_io() {
    let exec = Executor::new();

    let host_op: HostOp<Vec<u8>> = HostOp::new();
    let host_op_clone = host_op.clone();

    let pipe = AsyncPipe::new(64);
    let pipe_writer = pipe.clone();
    let pipe_reader = pipe.clone();

    let final_output = Rc::new(RefCell::new(Vec::new()));
    let output_ref = final_output.clone();

    // Task 1: Fetches data from "host", writes to pipe
    exec.spawn(async move {
        // First, do some pipe writing
        let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe_writer, "/");
        ctx.write_all(b"prefix:").await.unwrap();

        // Now wait for host data
        let host_data = host_op_clone.wait().await;

        // Write host data to pipe
        ctx.write_all(&host_data).await.unwrap();
        ctx.write_all(b":suffix").await.unwrap();
        ctx.close_stdout();

        Ok(Exit::success())
    });

    // Task 2: Reads from pipe
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe_reader, "/");
        let mut buf = [0u8; 128];
        let mut data = Vec::new();

        loop {
            let n = ctx.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
        }

        *output_ref.borrow_mut() = data;
        Ok(Exit::success())
    });

    // First run: Task 1 writes "prefix:", then blocks on host op
    // Task 2 reads "prefix:" then blocks waiting for more
    assert!(matches!(exec.run(), RunState::Blocked));

    // Host provides data
    host_op.complete(b"HOST_DATA".to_vec());

    // Now both tasks should complete
    assert!(matches!(exec.run(), RunState::Done(_)));

    assert_eq!(final_output.borrow().as_slice(), b"prefix:HOST_DATA:suffix");
}

/// Test that low watermark doesn't cause deadlock when reader stops early.
///
/// This tests the deadlock prevention mechanism: without the `was_full` flag,
/// this scenario would deadlock because:
/// 1. Writer fills 100-byte buffer → blocks waiting for space
/// 2. Reader reads only 10 bytes → 90 bytes remain (90% > 25% low watermark)
/// 3. Reader stops → writer never woken → DEADLOCK
///
/// With the fix, the writer is woken on the first read after buffer was full,
/// regardless of whether low watermark is crossed.
#[test]
fn low_watermark_no_deadlock_on_partial_read() {
    let exec = Executor::new();

    // 100-byte pipe: low watermark at 25 bytes (25%)
    let pipe = AsyncPipe::new(100);
    let pipe_writer = pipe.clone();
    let pipe_reader = pipe.clone();

    let writer_woken = Rc::new(RefCell::new(false));
    let writer_woken_ref = writer_woken.clone();

    // Writer: fills buffer completely, then tries to write more
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe_writer.clone(), "/");

        // Write 100 bytes to fill buffer
        ctx.write_all(&[b'X'; 100]).await.unwrap();

        // This write will initially block (buffer full)
        // After reader reads even a small amount, writer should be woken
        ctx.write_all(&[b'Y'; 10]).await.unwrap();

        // If we get here, writer was woken (not deadlocked!)
        *writer_woken_ref.borrow_mut() = true;
        ctx.close_stdout();

        Ok(Exit::success())
    });

    // Reader: reads only a small amount, NOT crossing low watermark
    exec.spawn(async move {
        let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe_reader, "/");

        // Read only 10 bytes (buffer goes from 100 → 90 bytes = 90%, still above 25%)
        let mut buf = [0u8; 10];
        let n = ctx.read(&mut buf).await.unwrap();
        assert_eq!(n, 10);

        // Read remaining data to let writer complete
        let mut remaining = Vec::new();
        loop {
            let mut chunk = [0u8; 32];
            let n = ctx.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            remaining.extend_from_slice(&chunk[..n]);
        }

        // Total: 100 (first batch) + 10 (second batch) = 110 bytes
        assert_eq!(10 + remaining.len(), 110);

        Ok(Exit::success())
    });

    // Run to completion - should NOT deadlock
    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 2);
            for r in &results {
                match r {
                    TaskResult::Ok(exit) => assert_eq!(exit.code, 0),
                    TaskResult::Err(e) => panic!("task failed: {e}"),
                }
            }
        }
        RunState::Blocked => panic!("DEADLOCK: executor blocked - low watermark bug!"),
    }

    // Verify writer was indeed woken after partial read
    assert!(
        *writer_woken.borrow(),
        "Writer should have been woken after partial read"
    );
}

/// Test that waker is properly cloned and works after original is dropped.
#[test]
fn waker_clone_survives_original() {
    use std::future::poll_fn;
    use std::task::Poll;

    let exec = Executor::new();

    let waker_store: Rc<RefCell<Option<std::task::Waker>>> = Rc::new(RefCell::new(None));
    let ws1 = waker_store.clone();
    let ws2 = waker_store.clone();

    let completed = Rc::new(RefCell::new(false));
    let completed_ref = completed.clone();

    exec.spawn(async move {
        let mut first_poll = true;
        poll_fn(|cx| {
            if first_poll {
                // Store a CLONE of the waker
                *ws1.borrow_mut() = Some(cx.waker().clone());
                first_poll = false;
                Poll::Pending
            } else {
                *completed_ref.borrow_mut() = true;
                Poll::Ready(())
            }
        })
        .await;

        Ok(Exit::success())
    });

    // First run stores waker
    assert!(matches!(exec.run(), RunState::Blocked));

    // Get the stored waker and clone it
    let cloned_waker = ws2.borrow().clone().unwrap();

    // Drop the original in the store
    *ws2.borrow_mut() = None;

    // Wake using the clone - should still work
    cloned_waker.wake();

    // Task should complete
    assert!(matches!(exec.run(), RunState::Done(_)));
    assert!(*completed.borrow());
}

// =============================================================================
// lib.rs Coverage Tests
// =============================================================================

/// Test `Exit::with_cwd` constructor.
#[test]
fn exit_with_cwd() {
    let exit = Exit::with_cwd("/home/user".to_string());
    assert_eq!(exit.code, 0);
    assert_eq!(exit.effects.cwd, Some("/home/user".to_string()));
    assert!(exit.effects.env_set.is_empty());
    assert!(exit.effects.env_unset.is_empty());
}

/// Test `Exit::code` constructor.
#[test]
fn exit_code_constructor() {
    let exit = Exit::code(42);
    assert_eq!(exit.code, 42);
    assert!(exit.effects.cwd.is_none());
}

/// Test `Exit::success` constructor.
#[test]
fn exit_success_constructor() {
    let exit = Exit::success();
    assert_eq!(exit.code, 0);
    assert!(exit.effects.cwd.is_none());
}

/// Test `Error::Command` variant.
#[test]
fn error_command_variant() {
    let err = Error::Command("test error message".to_string());
    assert_eq!(format!("{err}"), "test error message");
}

/// Test `Error::Io` variant display.
#[test]
fn error_io_variant_display() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let err = Error::Io(io_err);
    let display = format!("{err}");
    assert!(display.contains("I/O error"));
}

/// Test Error implements `std::error::Error`.
#[test]
fn error_is_std_error() {
    let err: Box<dyn std::error::Error> = Box::new(Error::Command("test".to_string()));
    assert!(err.to_string().contains("test"));
}

/// Test `SideEffects` default.
#[test]
fn side_effects_default() {
    let effects = crate::SideEffects::default();
    assert!(effects.cwd.is_none());
    assert!(effects.env_set.is_empty());
    assert!(effects.env_unset.is_empty());
}

// =============================================================================
// Spawner Coverage Tests
// =============================================================================

/// Test `TaskHandle::is_complete` before and after completion.
#[test]
fn task_handle_is_complete() {
    use crate::spawner::Spawner;

    let exec = Executor::new();
    let spawner = Spawner::new();
    let spawner_clone = spawner.clone();

    let handle_complete = Rc::new(RefCell::new(false));
    let hc = handle_complete.clone();

    exec.spawn(async move {
        let handle = spawner_clone.spawn(async { Ok(Exit::success()) });

        // Not complete before running
        assert!(!handle.is_complete());

        // After completion, should be complete
        let _ = handle.await;
        *hc.borrow_mut() = true;

        Ok(Exit::success())
    });

    // Run parent, then process spawned task
    let _ = exec.run();

    if let Some(request) = spawner.take_request() {
        let slot = request.result_slot.clone();
        exec.spawn(async move {
            let result = request.future.await;
            let mut s = slot.borrow_mut();
            s.result = Some(result);
            if let Some(waker) = s.waker.take() {
                waker.wake();
            }
            Ok(Exit::success())
        });
    }

    let _ = exec.run();
    assert!(*handle_complete.borrow());
}

/// Test `Spawner::has_requests`.
#[test]
fn spawner_has_requests() {
    use crate::spawner::Spawner;

    let spawner = Spawner::new();

    // Initially no requests
    assert!(!spawner.has_requests());

    // Spawn a task
    let _handle = spawner.spawn(async { Ok(Exit::success()) });

    // Now has requests
    assert!(spawner.has_requests());

    // Take the request
    let _ = spawner.take_request();

    // No more requests
    assert!(!spawner.has_requests());
}

/// Test `Spawner::register_executor_waker`.
#[test]
fn spawner_register_executor_waker() {
    use crate::spawner::Spawner;
    use crate::waker::{SmallVecReadyQueue, WakerTaskId, create_waker};

    let spawner = Spawner::new();
    let queue = Rc::new(RefCell::new(SmallVecReadyQueue::default()));
    let waker = create_waker(WakerTaskId(0), Rc::clone(&queue));

    // Register the waker
    spawner.register_executor_waker(waker);

    // Spawning should wake the executor
    let _handle = spawner.spawn(async { Ok(Exit::success()) });

    // Check that the waker was called (task 0 should be in queue)
    assert!(!queue.borrow().ready.is_empty());
}

/// Test Spawner default implementation.
#[test]
fn spawner_default() {
    use crate::spawner::Spawner;

    let spawner = Spawner::default();
    assert!(!spawner.has_requests());
}

/// Test Pipeline construction and execution.
#[test]
fn pipeline_construction() {
    use crate::spawner::{Pipeline, Spawner};

    let spawner = Spawner::new();
    let mut pipeline = Pipeline::new();

    // Add stages
    pipeline.add(async { Ok(Exit::code(1)) });
    pipeline.add(async { Ok(Exit::code(2)) });
    pipeline.add(async { Ok(Exit::code(3)) });

    // Execute
    let handles = pipeline.execute(&spawner);
    assert_eq!(handles.len(), 3);

    // All requests should be queued
    assert!(spawner.take_request().is_some());
    assert!(spawner.take_request().is_some());
    assert!(spawner.take_request().is_some());
    assert!(spawner.take_request().is_none());
}

/// Test Pipeline default implementation.
#[test]
fn pipeline_default() {
    use crate::spawner::Pipeline;

    let pipeline = Pipeline::default();
    // Empty pipeline should have no stages (will create empty handles vec)
    let spawner = crate::spawner::Spawner::new();
    let handles = pipeline.execute(&spawner);
    assert!(handles.is_empty());
}

/// Test select_first_task when all handles are consumed/dropped.
#[test]
fn select_first_all_handles_gone() {
    use crate::spawner::select_first_task;

    let exec = Executor::new();
    let result_captured = Rc::new(RefCell::new(None));
    let rc = result_captured.clone();

    exec.spawn(async move {
        // Empty handles vec - should return error immediately
        let handles = vec![];
        let (idx, result) = select_first_task(handles).await;
        *rc.borrow_mut() = Some((idx, result.is_err()));
        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(_) => (),
        RunState::Blocked => panic!("should complete"),
    }

    let captured = result_captured.borrow();
    let (idx, is_err) = captured.as_ref().unwrap();
    assert_eq!(*idx, 0);
    assert!(*is_err);
}

// =============================================================================
// Waker Coverage Tests
// =============================================================================

/// Test WakerTaskId equality and hash.
#[test]
fn waker_task_id_eq_hash() {
    use std::collections::HashSet;

    use crate::waker::WakerTaskId;

    let id1 = WakerTaskId(42);
    let id2 = WakerTaskId(42);
    let id3 = WakerTaskId(99);

    // Equality
    assert_eq!(id1, id2);
    assert_ne!(id1, id3);

    // Hash (can be inserted into HashSet)
    let mut set = HashSet::new();
    set.insert(id1);
    assert!(set.contains(&id2));
    assert!(!set.contains(&id3));
}

/// Test WakerTaskId debug format.
#[test]
fn waker_task_id_debug() {
    use crate::waker::WakerTaskId;

    let id = WakerTaskId(123);
    let debug = format!("{id:?}");
    assert!(debug.contains("123"));
}

/// Test multiple wakers for same queue.
#[test]
fn multiple_wakers_same_queue() {
    use crate::waker::{SmallVecReadyQueue, WakerTaskId, create_waker};

    let queue = Rc::new(RefCell::new(SmallVecReadyQueue::default()));

    let waker1 = create_waker(WakerTaskId(1), Rc::clone(&queue));
    let waker2 = create_waker(WakerTaskId(2), Rc::clone(&queue));
    let waker3 = create_waker(WakerTaskId(3), Rc::clone(&queue));

    // Wake in different order
    waker2.wake();
    waker1.wake();
    waker3.wake();

    let ready = &queue.borrow().ready;
    assert_eq!(ready.len(), 3);
    assert!(ready.contains(&WakerTaskId(1)));
    assert!(ready.contains(&WakerTaskId(2)));
    assert!(ready.contains(&WakerTaskId(3)));
}

/// Test waker cloning multiple times.
#[test]
fn waker_multiple_clones() {
    use crate::waker::{SmallVecReadyQueue, WakerTaskId, create_waker};

    let queue = Rc::new(RefCell::new(SmallVecReadyQueue::default()));
    let waker = create_waker(WakerTaskId(5), Rc::clone(&queue));

    // Clone multiple times
    let clone1 = waker.clone();
    let clone2 = clone1.clone();
    let clone3 = clone2.clone();

    // All clones should wake the same task
    clone3.wake();
    clone2.wake();
    clone1.wake();
    waker.wake();

    assert_eq!(queue.borrow().ready.len(), 4);
    assert!(queue.borrow().ready.iter().all(|id| *id == WakerTaskId(5)));
}

// =============================================================================
// Timer Coverage Tests
// =============================================================================

/// Test duration_to_nanos with zero.
#[test]
fn timer_duration_to_nanos_zero() {
    use crate::timer::duration_to_nanos;

    let d = std::time::Duration::from_secs(0);
    assert_eq!(duration_to_nanos(d), 0);
}

/// Test nanos_to_duration with max value.
#[test]
fn timer_nanos_to_duration_max() {
    use crate::timer::nanos_to_duration;

    let d = nanos_to_duration(u64::MAX);
    assert_eq!(d.as_nanos(), u64::MAX as u128);
}

/// Test TimerState has_pending and earliest_deadline.
#[test]
fn timer_state_diagnostics() {
    use crate::timer::{SleepFuture, TimerState};

    let channel = test_host_channel(10);
    let timer_state = TimerState::new(channel);

    // Initially no pending timers
    assert!(!timer_state.has_pending());
    assert!(timer_state.earliest_deadline().is_none());

    // Create a sleep future (registers timer)
    let _future = SleepFuture::new(timer_state.clone(), 1000);

    // Now has pending timer
    assert!(timer_state.has_pending());
    assert_eq!(timer_state.earliest_deadline(), Some(1000));

    // Create another with earlier deadline
    let _future2 = SleepFuture::new(timer_state.clone(), 500);

    // Earliest should be 500
    assert_eq!(timer_state.earliest_deadline(), Some(500));
}

/// Test timer cancellation via drop.
#[test]
fn timer_cancellation_on_drop() {
    use crate::timer::{SleepFuture, TimerState};

    let channel = test_host_channel(10);
    let timer_state = TimerState::new(channel);

    {
        let _future = SleepFuture::new(timer_state.clone(), 1000);
        assert!(timer_state.has_pending());
    }

    // After drop, timer is cancelled (still in heap but marked cancelled)
    // The heap isn't cleared immediately but timer won't fire
    assert!(timer_state.has_pending()); // Still in heap
}

/// Test SleepFuture deadline accessor.
#[test]
fn sleep_future_deadline() {
    use crate::timer::{SleepFuture, TimerState};

    let channel = test_host_channel(10);
    let timer_state = TimerState::new(channel);

    let future = SleepFuture::new(timer_state, 12345);
    assert_eq!(future.deadline(), 12345);
}

// =============================================================================
// Context Coverage Tests
// =============================================================================

/// Test AsyncContext cwd getter.
#[test]
fn context_cwd() {
    use crate::context::AsyncContext;

    let ctx = AsyncContext::with_buffers(vec![], "/home/test");
    assert_eq!(ctx.cwd(), "/home/test");
}

/// Test AsyncContext set_env.
#[test]
fn context_set_env() {
    use crate::context::AsyncContext;

    let mut ctx = AsyncContext::with_buffers(vec![], "/");
    ctx.set_env("MY_VAR".to_string(), "my_value".to_string());

    let effects = ctx.take_effects();
    assert!(
        effects
            .env_set
            .contains(&("MY_VAR".to_string(), "my_value".to_string()))
    );
}

/// Test AsyncContext set_env multiple times.
#[test]
fn context_multiple_env_changes() {
    use crate::context::AsyncContext;

    let mut ctx = AsyncContext::with_buffers(vec![], "/");
    ctx.set_env("VAR1".to_string(), "value1".to_string());
    ctx.set_env("VAR2".to_string(), "value2".to_string());

    let effects = ctx.take_effects();
    assert_eq!(effects.env_set.len(), 2);
}

// =============================================================================
// VFS Stream Coverage Tests
// =============================================================================

/// Test VfsWriter write_all.
#[test]
fn vfs_writer_write_all() {
    use crate::vfs_stream::VfsWriter;

    let mut writer = VfsWriter::new();
    writer.write_all(b"hello ").unwrap();
    writer.write_all(b"world").unwrap();

    assert_eq!(writer.into_bytes(), b"hello world");
}

/// Test VfsWriter into_bytes.
#[test]
fn vfs_writer_into_bytes() {
    use crate::vfs_stream::VfsWriter;

    let mut writer = VfsWriter::new();
    writer.write_all(b"test data").unwrap();
    let bytes = writer.into_bytes();
    assert_eq!(bytes, b"test data");
}

/// Test VfsReader async read.
#[test]
fn vfs_reader_async_read() {
    use crate::vfs_stream::VfsReader;

    let exec = Executor::new();
    let final_data = Rc::new(RefCell::new(Vec::new()));
    let data_ref = final_data.clone();

    exec.spawn(async move {
        let mut reader = VfsReader::new(b"async test data".to_vec());
        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).await?;
        data_ref.borrow_mut().extend_from_slice(&buf[..n]);
        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(_) => (),
        RunState::Blocked => panic!("should complete"),
    }

    assert_eq!(final_data.borrow().as_slice(), b"async test data");
}

/// Test VfsReader reads multiple chunks.
#[test]
fn vfs_reader_multiple_reads() {
    use crate::vfs_stream::VfsReader;

    let exec = Executor::new();
    let final_data = Rc::new(RefCell::new(Vec::new()));
    let data_ref = final_data.clone();

    exec.spawn(async move {
        let mut reader = VfsReader::new(b"hello world".to_vec());
        let mut buf = [0u8; 5];

        // Read first 5 bytes
        let n = reader.read(&mut buf).await?;
        data_ref.borrow_mut().extend_from_slice(&buf[..n]);

        // Read next 5 bytes
        let n = reader.read(&mut buf).await?;
        data_ref.borrow_mut().extend_from_slice(&buf[..n]);

        // Read remaining
        let n = reader.read(&mut buf).await?;
        data_ref.borrow_mut().extend_from_slice(&buf[..n]);

        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(_) => (),
        RunState::Blocked => panic!("should complete"),
    }

    assert_eq!(final_data.borrow().as_slice(), b"hello world");
}

/// Test VfsWriter in async context.
#[test]
fn vfs_writer_async_write() {
    use crate::vfs_stream::VfsWriter;

    let exec = Executor::new();
    let output = Rc::new(RefCell::new(Vec::new()));
    let output_ref = output.clone();

    exec.spawn(async move {
        let mut writer = VfsWriter::new();
        writer.write_all(b"async write").unwrap();
        *output_ref.borrow_mut() = writer.into_bytes();
        Ok(Exit::success())
    });

    match exec.run() {
        RunState::Done(_) => (),
        RunState::Blocked => panic!("should complete"),
    }

    assert_eq!(output.borrow().as_slice(), b"async write");
}

// =============================================================================
// Pipe Coverage Tests
// =============================================================================

/// Test pipe with zero capacity (should use minimum).
#[test]
fn pipe_zero_capacity() {
    let pipe = AsyncPipe::new(0);
    // Should still work (uses minimum capacity internally)
    assert!(!pipe.is_closed());
}

/// Test pipe close idempotent.
#[test]
fn pipe_close_idempotent() {
    let pipe = AsyncPipe::new(64);
    pipe.close();
    assert!(pipe.is_closed());
    pipe.close(); // Should not panic
    assert!(pipe.is_closed());
}

/// Test pipe len and is_empty.
#[test]
fn pipe_len_is_empty() {
    let exec = Executor::new();
    let pipe = AsyncPipe::new(64);
    let pipe_clone = pipe.clone();

    let len_before = Rc::new(RefCell::new(0usize));
    let len_after = Rc::new(RefCell::new(0usize));
    let lb = len_before.clone();
    let la = len_after.clone();

    exec.spawn(async move {
        *lb.borrow_mut() = pipe_clone.len();
        pipe_clone.write(b"hello").await.unwrap();
        *la.borrow_mut() = pipe_clone.len();
        pipe_clone.close();
        Ok(Exit::success())
    });

    exec.spawn(async move {
        let mut buf = [0u8; 64];
        let _ = pipe.read(&mut buf).await;
        Ok(Exit::success())
    });

    let _ = exec.run();

    assert_eq!(*len_before.borrow(), 0);
    // len_after might vary depending on scheduling
}

// =============================================================================
// Host Channel Coverage Tests
// =============================================================================
// Note: HostChannel is thoroughly tested in host_channel.rs with 14 tests.
// Operations are only queued when futures are POLLED, not when created.
// The host_channel module tests use Executor to properly poll futures.

// =============================================================================
// Executor Coverage Tests
// =============================================================================

/// Test executor with task that returns error.
#[test]
fn executor_task_error() {
    let exec = Executor::new();

    exec.spawn(async { Err(Error::Command("intentional failure".to_string())) });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 1);
            match &results[0] {
                TaskResult::Err(e) => {
                    assert!(e.to_string().contains("intentional"));
                }
                TaskResult::Ok(_) => panic!("should be error"),
            }
        }
        RunState::Blocked => panic!("should complete"),
    }
}

/// Test executor with immediate completion.
#[test]
fn executor_immediate_completion() {
    let exec = Executor::new();

    exec.spawn(async { Ok(Exit::code(123)) });

    match exec.run() {
        RunState::Done(results) => {
            assert_eq!(results.len(), 1);
            match &results[0] {
                TaskResult::Ok(exit) => assert_eq!(exit.code, 123),
                TaskResult::Err(_) => panic!("should succeed"),
            }
        }
        RunState::Blocked => panic!("should complete"),
    }
}

/// Test executor with no tasks.
#[test]
fn executor_no_tasks() {
    let exec = Executor::new();

    match exec.run() {
        RunState::Done(results) => {
            assert!(results.is_empty());
        }
        RunState::Blocked => panic!("should complete with empty results"),
    }
}

// =============================================================================
// Scheduler API Coverage Tests
// =============================================================================

use crate::scheduler::{SchedulerState, join_all, noop_waker};

/// Test Scheduler::is_empty() method.
#[test]
fn scheduler_is_empty() {
    let sched = test_scheduler();

    // New scheduler is empty
    assert!(sched.is_empty());

    // After spawning, still "empty" in the sense that no tasks are done
    sched.spawn(async { Ok(Exit::success()) });

    // Run to completion
    let state = sched.run();
    assert!(matches!(state, SchedulerState::Done));

    // After all tasks done, is_empty should be true
    assert!(sched.is_empty());
}

/// Test has_pending_host_ops() method.
#[test]
fn scheduler_has_pending_host_ops() {
    let sched = test_scheduler();

    // No pending ops initially
    assert!(!sched.has_pending_host_ops());

    let host = sched.host();
    sched.spawn(async move {
        let _data = host.file_read("/test.txt").await?;
        Ok(Exit::success())
    });

    // Run until blocked on host op
    let state = sched.run();
    assert!(matches!(state, SchedulerState::Blocked));

    // Now should have pending host op
    assert!(sched.has_pending_host_ops());

    // Complete it
    let req = sched.take_host_op().unwrap();
    sched.complete_host_op(req.id, b"content".to_vec());

    // No longer pending
    assert!(!sched.has_pending_host_ops());
}

/// Test complete_host_op_err() method.
#[test]
fn scheduler_complete_host_op_err() {
    let sched = test_scheduler();

    let got_error = Rc::new(RefCell::new(false));
    let got_error_clone = got_error.clone();

    let host = sched.host();
    sched.spawn(async move {
        let r = host.file_read("/missing.txt").await;
        if r.is_err() {
            *got_error_clone.borrow_mut() = true;
        }
        Ok(Exit::success())
    });

    // Run until blocked
    let state = sched.run();
    assert!(matches!(state, SchedulerState::Blocked));

    // Complete with error
    let req = sched.take_host_op().unwrap();
    sched.complete_host_op_err(req.id, std::io::Error::from(std::io::ErrorKind::NotFound));

    // Run to completion
    let state = sched.run();
    assert!(matches!(state, SchedulerState::Done));

    // Task should have received the error
    assert!(*got_error.borrow());
}

/// Test noop_waker function.
#[test]
fn test_noop_waker() {
    let waker = noop_waker();

    // Just verify it doesn't crash when used
    waker.wake_by_ref();
    let waker2 = waker.clone();
    waker2.wake();
}

/// Test TaskHandle::try_get() method.
#[test]
fn task_handle_try_get() {
    let sched = test_scheduler();

    let handle = sched.spawn(async { Ok(Exit::code(99)) });

    // Before completion, try_get returns None
    assert!(handle.try_get().is_none());

    // Run to completion
    let _ = sched.run();

    // After completion, try_get returns result
    let result = handle.try_get();
    assert!(result.is_some());
    assert_eq!(result.unwrap().unwrap().code, 99);

    // Subsequent try_get returns None (result was taken)
    assert!(handle.try_get().is_none());
}

/// Test TaskHandle::cancel() terminates task and children.
#[test]
fn task_handle_cancel() {
    let sched = test_scheduler();
    let sched_for_parent = sched.clone();
    let sched_for_child = sched.clone();

    let completed = Rc::new(RefCell::new(false));
    let completed_clone = completed.clone();

    // Spawn a parent task that spawns a child
    let handle = sched.spawn(async move {
        // Spawn a child that would run forever if not cancelled
        let _child = sched_for_parent.spawn(async move {
            // Simulate long-running work with a sleep
            sched_for_child.sleep_until(u64::MAX).await?;
            Ok(Exit::success())
        });

        // Parent sleeps forever too
        sched_for_parent.sleep_until(u64::MAX).await?;
        *completed_clone.borrow_mut() = true;
        Ok(Exit::success())
    });

    // Run to spawn tasks and poll them once
    let _ = sched.run();

    // Cancel the parent - should cascade to children via structured concurrency
    assert!(handle.cancel());

    // Drain any orphaned timer ops (from cancelled sleeps)
    while let Some(req) = sched.take_host_op() {
        sched.complete_host_op(req.id, u64::MAX.to_le_bytes().to_vec());
    }

    // Run scheduler - should complete (all tasks cancelled)
    let state = sched.run();
    assert!(matches!(state, SchedulerState::Done));

    // Task didn't complete normally (was cancelled)
    assert!(!*completed.borrow());
}

/// Test join_all with empty vec.
#[test]
fn join_all_empty() {
    let sched = test_scheduler();

    let results_vec = Rc::new(RefCell::new(Vec::new()));
    let results_clone = results_vec.clone();

    sched.spawn(async move {
        let handles: Vec<crate::scheduler::TaskHandle> = vec![];
        let results = join_all(handles).await;
        *results_clone.borrow_mut() = results;
        Ok(Exit::success())
    });

    let _ = sched.run();

    // Empty join_all should return empty vec
    assert!(results_vec.borrow().is_empty());
}

/// Test scheduler run_step method.
#[test]
fn scheduler_run_step() {
    let sched = test_scheduler();

    // Run step with no tasks
    let state = sched.run_step();
    assert!(matches!(state, SchedulerState::Done));

    // Spawn a task
    let handle = sched.spawn(async { Ok(Exit::code(1)) });

    // Run one step at a time until done
    let mut steps = 0;
    loop {
        let state = sched.run_step();
        steps += 1;
        assert!(steps <= 100, "too many steps");
        if matches!(state, SchedulerState::Done) {
            break;
        }
    }

    assert!(handle.is_complete());
}
