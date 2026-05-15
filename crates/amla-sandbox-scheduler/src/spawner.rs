//! Task spawning from within tasks.
//!
//! This module provides a way for tasks to spawn child tasks and wait for
//! their completion. This is essential for the shell, which needs to spawn
//! command pipelines and wait for their results.
//!
//! ## Design
//!
//! ```text
//! Shell Task                  Spawner                      Executor
//!     |                          |                            |
//!     |-- spawn(cmd_future) ---->|                            |
//!     |<-- TaskHandle -----------|                            |
//!     |                          |--- spawn_request --------->|
//!     |                          |                            |
//!     |-- handle.await --------->|                            |
//!     |   (shell yields)         |                            |
//!     |                          |<-- task completes ---------|
//!     |<-- Exit -----------------|                            |
//! ```
//!
//! ## Example
//!
//! ```rust,ignore
//! // Within a shell task:
//! let pipe = AsyncPipe::new(4096);
//!
//! // Spawn child command
//! let handle = spawner.spawn(async move {
//!     ctx.write_all(b"hello").await?;
//!     ctx.close_stdout();
//!     Ok(Exit::success())
//! });
//!
//! // Wait for child
//! let exit = handle.await;
//! assert_eq!(exit.code, 0);
//! ```

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use smallvec::SmallVec;

use crate::executor::TaskId;
use crate::{Error, Exit};

/// A spawn request from a task.
pub struct SpawnRequest {
    /// The future to spawn.
    pub future: Pin<Box<dyn Future<Output = Result<Exit, Error>>>>,
    /// Where to deliver the result.
    pub result_slot: Rc<RefCell<TaskSlot>>,
}

/// Slot for task completion notification.
///
/// Note: This follows the same "completion slot" pattern as `scheduler::TaskSlot`
/// and `host_channel::PendingSlot`. This variant includes `task_id` because the
/// spawner operates separately from the executor.
pub struct TaskSlot {
    /// Result when task completes.
    pub result: Option<Result<Exit, Error>>,
    /// Waker to notify parent.
    pub waker: Option<Waker>,
    /// Task ID once spawned.
    pub task_id: Option<TaskId>,
}

/// Shared state for spawner.
struct SpawnerInner {
    /// Pending spawn requests.
    requests: VecDeque<SpawnRequest>,
    /// Waker for executor when requests are available.
    executor_waker: Option<Waker>,
}

/// Task spawner that can be used from within tasks.
///
/// Clone this to share between parent task and executor.
#[derive(Clone)]
pub struct Spawner {
    inner: Rc<RefCell<SpawnerInner>>,
}

impl Spawner {
    /// Create a new spawner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(SpawnerInner {
                requests: VecDeque::new(),
                executor_waker: None,
            })),
        }
    }

    /// Spawn a child task and return a handle to wait for it.
    pub fn spawn<F>(&self, future: F) -> TaskHandle
    where
        F: Future<Output = Result<Exit, Error>> + 'static,
    {
        let slot = Rc::new(RefCell::new(TaskSlot {
            result: None,
            waker: None,
            task_id: None,
        }));

        let request = SpawnRequest {
            future: Box::pin(future),
            result_slot: Rc::clone(&slot),
        };

        {
            let mut inner = self.inner.borrow_mut();
            inner.requests.push_back(request);
            if let Some(waker) = inner.executor_waker.take() {
                waker.wake();
            }
        }

        TaskHandle { slot }
    }

    /// Take the next spawn request (called by executor).
    pub fn take_request(&self) -> Option<SpawnRequest> {
        self.inner.borrow_mut().requests.pop_front()
    }

    /// Check if there are pending requests.
    #[must_use]
    pub fn has_requests(&self) -> bool {
        !self.inner.borrow().requests.is_empty()
    }

    /// Register executor waker for when requests are available.
    pub fn register_executor_waker(&self, waker: Waker) {
        self.inner.borrow_mut().executor_waker = Some(waker);
    }
}

impl Default for Spawner {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to a spawned task.
///
/// Await this to get the task's exit status.
pub struct TaskHandle {
    slot: Rc<RefCell<TaskSlot>>,
}

impl TaskHandle {
    /// Get the task ID (only available after executor processes the spawn request).
    #[must_use]
    pub fn task_id(&self) -> Option<TaskId> {
        self.slot.borrow().task_id
    }

    /// Check if the task has completed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.slot.borrow().result.is_some()
    }
}

impl Future for TaskHandle {
    type Output = Result<Exit, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slot = self.slot.borrow_mut();

        if let Some(result) = slot.result.take() {
            return Poll::Ready(result);
        }

        // Only clone waker if it changed (optimization)
        if slot.waker.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
            slot.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

/// Wait for the first of multiple tasks to complete.
///
/// Returns the index and result of the first completed task.
/// Other tasks continue running (not cancelled).
pub fn select_first_task(handles: Vec<TaskHandle>) -> SelectFirstTask {
    SelectFirstTask {
        handles: handles.into_iter().map(Some).collect(),
    }
}

/// Future for selecting first completed task.
pub struct SelectFirstTask {
    handles: Vec<Option<TaskHandle>>,
}

impl Future for SelectFirstTask {
    type Output = (usize, Result<Exit, Error>);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        for (idx, handle_opt) in this.handles.iter_mut().enumerate() {
            if let Some(handle) = handle_opt {
                let result = {
                    let mut slot = handle.slot.borrow_mut();
                    if let Some(result) = slot.result.take() {
                        Some(result)
                    } else {
                        // Only clone waker if it changed (optimization)
                        if slot.waker.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                            slot.waker = Some(cx.waker().clone());
                        }
                        None
                    }
                };

                if let Some(result) = result {
                    let _ = handle_opt.take();
                    return Poll::Ready((idx, result));
                }
            }
        }

        // Check if all handles are gone
        if this.handles.iter().all(Option::is_none) {
            return Poll::Ready((
                0,
                Err(Error::Command("all tasks completed or dropped".to_string())),
            ));
        }

        Poll::Pending
    }
}

/// Wait for all tasks to complete.
///
/// Returns results in the same order as handles were provided.
pub fn join_all_tasks(handles: Vec<TaskHandle>) -> JoinAllTasks {
    let len = handles.len();
    let mut results = Vec::with_capacity(len);
    for _ in 0..len {
        results.push(None);
    }
    JoinAllTasks {
        handles: handles.into_iter().map(Some).collect(),
        results,
    }
}

/// Future for joining all tasks.
pub struct JoinAllTasks {
    handles: Vec<Option<TaskHandle>>,
    results: Vec<Option<Result<Exit, Error>>>,
}

impl Future for JoinAllTasks {
    type Output = Vec<Result<Exit, Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let mut all_done = true;

        for (idx, handle_opt) in this.handles.iter_mut().enumerate() {
            if this.results[idx].is_some() {
                continue;
            }

            if let Some(handle) = handle_opt {
                let result = {
                    let mut slot = handle.slot.borrow_mut();
                    if let Some(result) = slot.result.take() {
                        Some(result)
                    } else {
                        // Only clone waker if it changed (optimization)
                        if slot.waker.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                            slot.waker = Some(cx.waker().clone());
                        }
                        None
                    }
                };

                if let Some(result) = result {
                    this.results[idx] = Some(result);
                    let _ = handle_opt.take();
                } else {
                    all_done = false;
                }
            }
        }

        if all_done {
            let results: Vec<_> = this.results.iter_mut().map(|r| r.take().unwrap()).collect();
            Poll::Ready(results)
        } else {
            Poll::Pending
        }
    }
}

/// Pipeline builder for connecting commands with pipes.
pub struct Pipeline {
    /// Stages in the pipeline.
    stages: SmallVec<[PipelineStage; 4]>,
}

/// A stage in a pipeline.
struct PipelineStage {
    /// The command future.
    future: Pin<Box<dyn Future<Output = Result<Exit, Error>>>>,
}

impl Pipeline {
    /// Create a new empty pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stages: SmallVec::new(),
        }
    }

    /// Add a stage to the pipeline.
    pub fn add<F>(&mut self, future: F)
    where
        F: Future<Output = Result<Exit, Error>> + 'static,
    {
        self.stages.push(PipelineStage {
            future: Box::pin(future),
        });
    }

    /// Execute the pipeline, returning handles to all stages.
    pub fn execute(self, spawner: &Spawner) -> Vec<TaskHandle> {
        self.stages
            .into_iter()
            .map(|stage| {
                let slot = Rc::new(RefCell::new(TaskSlot {
                    result: None,
                    waker: None,
                    task_id: None,
                }));

                let request = SpawnRequest {
                    future: stage.future,
                    result_slot: Rc::clone(&slot),
                };

                {
                    let mut inner = spawner.inner.borrow_mut();
                    inner.requests.push_back(request);
                }

                TaskHandle { slot }
            })
            .collect()
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncPipe;
    use crate::executor::Executor;

    fn complete_slot(slot: &Rc<RefCell<TaskSlot>>, result: Result<Exit, Error>) {
        let mut s = slot.borrow_mut();
        s.result = Some(result);
        if let Some(waker) = s.waker.take() {
            waker.wake();
        }
    }

    #[test]
    fn spawn_and_wait_single() {
        let exec = Executor::new();
        let spawner = Spawner::new();
        let spawner_clone = spawner.clone();

        let child_ran = Rc::new(RefCell::new(false));
        let child_ran_clone = child_ran.clone();

        exec.spawn(async move {
            // Parent spawns a child
            let handle = spawner_clone.spawn(async move {
                *child_ran_clone.borrow_mut() = true;
                Ok(Exit::code(42))
            });

            // Wait for child
            let result = handle.await?;
            assert_eq!(result.code, 42);

            Ok(Exit::success())
        });

        // Run - parent spawns child, then waits
        let state = exec.run();
        assert!(matches!(state, crate::RunState::Blocked));

        // Process spawn request
        let request = spawner.take_request().unwrap();
        let slot = request.result_slot.clone();

        // Spawn it
        exec.spawn(async move {
            // Run the child future
            let result = request.future.await;
            complete_slot(&slot, result);
            Ok(Exit::success())
        });

        // Run to completion
        let state = exec.run();
        assert!(matches!(state, crate::RunState::Done(_)));
        assert!(*child_ran.borrow());
    }

    #[test]
    fn spawn_multiple_children() {
        let exec = Executor::new();
        let spawner = Spawner::new();
        let spawner_clone = spawner.clone();

        let results = Rc::new(RefCell::new(Vec::new()));
        let results_clone = results.clone();

        exec.spawn(async move {
            let h1 = spawner_clone.spawn(async { Ok(Exit::code(1)) });
            let h2 = spawner_clone.spawn(async { Ok(Exit::code(2)) });
            let h3 = spawner_clone.spawn(async { Ok(Exit::code(3)) });

            let all_results = join_all_tasks(vec![h1, h2, h3]).await;

            for r in all_results {
                results_clone.borrow_mut().push(r.unwrap().code);
            }

            Ok(Exit::success())
        });

        // Run until blocked
        let _ = exec.run();

        // Process all spawn requests
        while let Some(request) = spawner.take_request() {
            let slot = request.result_slot.clone();
            exec.spawn(async move {
                let result = request.future.await;
                complete_slot(&slot, result);
                Ok(Exit::success())
            });
        }

        // Run to completion
        let _ = exec.run();

        let r = results.borrow();
        assert_eq!(r.len(), 3);
        assert!(r.contains(&1));
        assert!(r.contains(&2));
        assert!(r.contains(&3));
    }

    #[test]
    fn select_first_task_completes() {
        let exec = Executor::new();
        let spawner = Spawner::new();
        let spawner_clone = spawner.clone();

        let winner_idx = Rc::new(RefCell::new(None));
        let winner_idx_clone = winner_idx.clone();

        exec.spawn(async move {
            let h1 = spawner_clone.spawn(async { Ok(Exit::code(1)) });
            let h2 = spawner_clone.spawn(async { Ok(Exit::code(2)) });
            let h3 = spawner_clone.spawn(async { Ok(Exit::code(3)) });

            let (idx, result) = select_first_task(vec![h1, h2, h3]).await;
            *winner_idx_clone.borrow_mut() = Some(idx);
            assert!(result.is_ok());

            Ok(Exit::success())
        });

        let _ = exec.run();

        // Only complete the second one
        let req1 = spawner.take_request().unwrap();
        let req2 = spawner.take_request().unwrap();
        let _req3 = spawner.take_request().unwrap();

        // Complete req2 first
        let slot2 = req2.result_slot.clone();
        exec.spawn(async move {
            let result = req2.future.await;
            complete_slot(&slot2, result);
            Ok(Exit::success())
        });

        let _ = exec.run();

        assert_eq!(*winner_idx.borrow(), Some(1));

        // Clean up req1 (would still be running in real scenario)
        let slot1 = req1.result_slot.clone();
        exec.spawn(async move {
            let result = req1.future.await;
            complete_slot(&slot1, result);
            Ok(Exit::success())
        });
    }

    #[test]
    fn pipeline_with_pipes() {
        let exec = Executor::new();
        let spawner = Spawner::new();
        let spawner_clone = spawner.clone();

        // Create pipe for connecting stages
        let pipe = AsyncPipe::new(64);
        let pipe_writer = pipe.clone();
        let pipe_reader = pipe;

        let final_output = Rc::new(RefCell::new(Vec::new()));
        let final_output_clone = final_output.clone();

        exec.spawn(async move {
            // Stage 1: writes to pipe
            let h1 = spawner_clone.spawn(async move {
                pipe_writer.write(b"hello from stage 1").await?;
                pipe_writer.close();
                Ok(Exit::success())
            });

            // Stage 2: reads from pipe
            let h2 = spawner_clone.spawn(async move {
                let mut buf = [0u8; 64];
                let n = pipe_reader.read(&mut buf).await?;
                final_output_clone.borrow_mut().extend_from_slice(&buf[..n]);
                Ok(Exit::success())
            });

            // Wait for both
            let results = join_all_tasks(vec![h1, h2]).await;
            assert!(results[0].is_ok());
            assert!(results[1].is_ok());

            Ok(Exit::success())
        });

        let _ = exec.run();

        // Process spawn requests
        while let Some(request) = spawner.take_request() {
            let slot = request.result_slot.clone();
            exec.spawn(async move {
                let result = request.future.await;
                complete_slot(&slot, result);
                Ok(Exit::success())
            });
        }

        let _ = exec.run();

        assert_eq!(&*final_output.borrow(), b"hello from stage 1");
    }
}
