//! Single-threaded async executor.
//!
//! This executor is designed to be wasm-friendly:
//! - No threads, no async-std/tokio
//! - Uses `RefCell` for interior mutability
//! - Cooperative: tasks must yield via `Poll::Pending`
//!
//! # API Hierarchy
//!
//! This is a **lower-level API**. For most use cases, prefer [`Scheduler`](crate::Scheduler)
//! which provides a unified interface combining the executor, spawner, and host channel.
//!
//! Use `Executor` directly only when you need:
//! - Fine-grained control over task scheduling
//! - Integration with a custom spawner or host channel
//! - Testing individual components in isolation

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use smallvec::SmallVec;

use crate::waker::{self, SmallVecReadyQueue, WakerTaskId};
use crate::{Error, Exit};

/// Unique task identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(usize);

/// Result of a completed task.
#[derive(Debug)]
pub enum TaskResult {
    /// Task completed successfully.
    Ok(Exit),
    /// Task failed with error.
    Err(Error),
}

/// State of executor after running.
#[derive(Debug)]
pub enum RunState {
    /// All tasks completed.
    Done(Vec<TaskResult>),
    /// Tasks are blocked (no progress possible without external input).
    Blocked,
}

/// A task in the executor.
struct Task {
    /// The future to poll.
    future: Pin<Box<dyn Future<Output = Result<Exit, Error>>>>,
    /// Whether the task has completed.
    completed: bool,
    /// Result if completed.
    result: Option<TaskResult>,
}

/// Single-threaded async executor.
pub struct Executor {
    /// Tasks indexed by `TaskId`.
    tasks: RefCell<Vec<Option<Task>>>,
    /// Ready queue shared with wakers.
    ready_queue: Rc<RefCell<SmallVecReadyQueue>>,
}

impl Executor {
    /// Create a new executor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: RefCell::new(Vec::new()),
            ready_queue: Rc::new(RefCell::new(SmallVecReadyQueue::default())),
        }
    }

    /// Spawn a task, returns its ID.
    pub fn spawn(&self, future: impl Future<Output = Result<Exit, Error>> + 'static) -> TaskId {
        let mut tasks = self.tasks.borrow_mut();
        let id = TaskId(tasks.len());
        tasks.push(Some(Task {
            future: Box::pin(future),
            completed: false,
            result: None,
        }));
        self.ready_queue.borrow_mut().ready.push(WakerTaskId(id.0));
        id
    }

    /// Run until all tasks complete or no progress is possible.
    #[must_use]
    pub fn run(&self) -> RunState {
        loop {
            // Poll all ready tasks
            let mut made_progress = false;

            // Collect ready tasks first to avoid borrow conflicts
            let ready_tasks: SmallVec<[WakerTaskId; 8]> =
                { self.ready_queue.borrow_mut().ready.drain(..).collect() };

            for waker_task_id in ready_tasks {
                let task_idx = waker_task_id.0;
                let waker = self.make_waker(waker_task_id);
                let mut cx = Context::from_waker(&waker);

                let poll_result = {
                    let mut tasks = self.tasks.borrow_mut();
                    let task = match tasks.get_mut(task_idx) {
                        Some(Some(t)) if !t.completed => t,
                        _ => continue,
                    };
                    task.future.as_mut().poll(&mut cx)
                };

                match poll_result {
                    Poll::Ready(result) => {
                        let mut tasks = self.tasks.borrow_mut();
                        if let Some(Some(task)) = tasks.get_mut(task_idx) {
                            task.completed = true;
                            task.result = Some(match result {
                                Ok(exit) => TaskResult::Ok(exit),
                                Err(e) => TaskResult::Err(e),
                            });
                        }
                        made_progress = true;
                    }
                    Poll::Pending => {
                        // Task will be re-queued when its waker is called
                    }
                }
            }

            // Check if all tasks are done
            let tasks = self.tasks.borrow();
            let all_done = tasks.iter().all(|t| t.as_ref().is_none_or(|t| t.completed));

            if all_done {
                drop(tasks);
                return RunState::Done(self.collect_results());
            }

            // If no progress and ready queue is empty, we're blocked
            if !made_progress && self.ready_queue.borrow().ready.is_empty() {
                return RunState::Blocked;
            }
        }
    }

    /// Collect results from all completed tasks.
    fn collect_results(&self) -> Vec<TaskResult> {
        let mut tasks = self.tasks.borrow_mut();
        tasks
            .iter_mut()
            .filter_map(|t| t.as_mut().and_then(|t| t.result.take()))
            .collect()
    }

    /// Wake a task (add to ready queue).
    pub fn wake(&self, task_id: TaskId) {
        self.ready_queue
            .borrow_mut()
            .ready
            .push(WakerTaskId(task_id.0));
    }

    /// Create a waker for a task.
    fn make_waker(&self, task_id: WakerTaskId) -> Waker {
        waker::create_waker(task_id, Rc::clone(&self.ready_queue))
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;

    #[test]
    fn executor_simple_task() {
        let exec = Executor::new();

        exec.spawn(async { Ok(Exit::success()) });

        match exec.run() {
            RunState::Done(results) => {
                assert_eq!(results.len(), 1);
                match &results[0] {
                    TaskResult::Ok(exit) => assert_eq!(exit.code, 0),
                    TaskResult::Err(_) => panic!("unexpected error"),
                }
            }
            RunState::Blocked => panic!("unexpected blocked"),
        }
    }

    #[test]
    fn executor_multiple_tasks() {
        let exec = Executor::new();

        exec.spawn(async { Ok(Exit::code(1)) });
        exec.spawn(async { Ok(Exit::code(2)) });
        exec.spawn(async { Ok(Exit::code(3)) });

        match exec.run() {
            RunState::Done(results) => {
                assert_eq!(results.len(), 3);
            }
            RunState::Blocked => panic!("unexpected blocked"),
        }
    }

    #[test]
    fn executor_pending_then_ready() {
        let exec = Executor::new();
        let counter = Rc::new(RefCell::new(0));
        let counter_clone = Rc::clone(&counter);

        exec.spawn(async move {
            // Yield once, then complete
            poll_fn(|cx| {
                let mut c = counter_clone.borrow_mut();
                *c += 1;
                if *c < 2 {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            Ok(Exit::success())
        });

        match exec.run() {
            RunState::Done(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(*counter.borrow(), 2);
            }
            RunState::Blocked => panic!("unexpected blocked"),
        }
    }
}
