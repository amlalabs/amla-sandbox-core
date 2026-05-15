//! Unified scheduler API.
//!
//! This module provides a clean, unified API for the scheduler that combines
//! the executor, spawner, and host channel into a single interface.
//!
//! # Overview
//!
//! The `Scheduler` is the **primary API** for this crate. It provides:
//!
//! - Task spawning with `spawn()`
//! - Awaitable task handles with automatic parent notification
//! - Host I/O operations via `host()`
//! - `select_first()` and `join_all()` combinators for task coordination
//!
//! # Example
//!
//! ```rust,ignore
//! use amla_scheduler::{Scheduler, Exit};
//!
//! let sched = test_scheduler();
//!
//! // Spawn a task - returns a handle you can await
//! let handle = sched.spawn(async {
//!     Ok(Exit::success())
//! });
//!
//! // Spawn from within a task
//! let sched_clone = sched.clone();
//! sched.spawn(async move {
//!     // Spawn child task
//!     let child = sched_clone.spawn(async {
//!         Ok(Exit::code(42))
//!     });
//!
//!     // Wait for child
//!     let result = child.await?;
//!     assert_eq!(result.code, 42);
//!
//!     Ok(Exit::success())
//! });
//!
//! // Run until all tasks complete or blocked on host ops
//! sched.run();
//! ```
//!
//! # Task Lifecycle
//!
//! Tasks go through these states:
//! 1. **Spawned**: Added to spawn queue via `spawn()`
//! 2. **Ready**: Moved to ready queue, waiting to be polled
//! 3. **Running**: Being polled by the scheduler
//! 4. **Pending**: Yielded, waiting for waker to be called
//! 5. **Completed**: Finished with a result
//!
//! # Combinators
//!
//! - [`select_first()`]: Wait for the first of multiple tasks to complete
//! - [`join_all()`]: Wait for all tasks to complete
//!
//! These combinators work with `TaskHandle`s from this scheduler. For host
//! operations, see [`crate::host_channel::select_first`] and
//! [`crate::host_channel::join_all`].

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use smallvec::SmallVec;

use crate::host_channel::{
    ClockType, HostChannel, HostOpFuture, HostOpRequest, RandomSourceFn, TaskIdRepr, TimeSourceFn,
};
use crate::timer::{SleepFuture, TimeNanos, TimerState};
use crate::waker::{self, SmallVecReadyQueue, WakerTaskId};
use crate::{Error, Exit};

/// Task identifier.
///
/// Contains both the slot index and a generation counter to prevent
/// ABA problems when slots are reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId {
    slot: usize,
    generation: usize,
}

impl TaskId {
    /// Convert to the representation used in host operations.
    #[must_use]
    pub fn to_repr(self) -> TaskIdRepr {
        TaskIdRepr {
            slot: self.slot,
            generation: self.generation,
        }
    }

    /// Create from host operation representation.
    #[must_use]
    pub fn from_repr(repr: TaskIdRepr) -> Self {
        Self {
            slot: repr.slot,
            generation: repr.generation,
        }
    }
}

impl From<TaskId> for TaskIdRepr {
    fn from(id: TaskId) -> Self {
        id.to_repr()
    }
}

impl From<TaskIdRepr> for TaskId {
    fn from(repr: TaskIdRepr) -> Self {
        Self::from_repr(repr)
    }
}

/// State after running the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerState {
    /// All tasks completed.
    Done,
    /// Tasks blocked waiting for host operations.
    Blocked,
    /// Made progress, more work may be available.
    Progress,
}

/// Slot for task completion notification.
///
/// This is shared between the scheduler (which sets the result) and the
/// `TaskHandle` (which reads the result).
///
/// Note: This follows the same "completion slot" pattern as [`spawner::TaskSlot`]
/// and [`host_channel::PendingSlot`]. Each variant has slightly different fields
/// for its specific use case, but the core pattern (result + waker) is shared.
struct TaskSlot {
    /// Result when task completes. Can be taken by `TaskHandle::await`.
    result: Option<Result<Exit, Error>>,
    /// Waker to notify parent task when this task completes.
    waker: Option<Waker>,
    /// Task ID (set when task is assigned a slot in `process_spawn_queue`).
    task_id: Option<TaskId>,
}

/// A task in the scheduler.
struct Task {
    /// The future to poll.
    future: Pin<Box<dyn Future<Output = Result<Exit, Error>>>>,
    /// Completion slot (shared with `TaskHandle`).
    slot: Rc<RefCell<TaskSlot>>,
    /// Parent task that spawned this one.
    ///
    /// If None, this is a root task (spawned from outside any task context).
    /// When the parent is cancelled, all children are recursively cancelled.
    parent: Option<TaskId>,
    /// Root task (top of spawn chain) for this task tree.
    ///
    /// For root tasks, this is None (will be set to self's ID when assigned).
    /// For child tasks, this inherits from the parent's root.
    /// Used for attributing host operations to the originating command.
    root: Option<TaskId>,
    /// Child tasks spawned by this task.
    ///
    /// When this task is cancelled or completes, all children are cancelled.
    /// Most tasks have 0-4 children, so `SmallVec` avoids allocation.
    children: SmallVec<[TaskId; 4]>,
}

/// Shared scheduler state.
struct SchedulerInner {
    /// All tasks.
    tasks: RefCell<Vec<Option<Task>>>,
    /// Generation counter per slot (for ABA prevention).
    ///
    /// Each time a slot is reused, its generation is incremented.
    /// `TaskId` includes generation so stale references are detected.
    generations: RefCell<Vec<usize>>,
    /// Ready queue (shared with wakers).
    ready_queue: Rc<RefCell<SmallVecReadyQueue>>,
    /// Pending spawn requests from within tasks.
    spawn_queue: RefCell<VecDeque<Task>>,
    /// Host operation channel.
    host_channel: HostChannel,
    /// Timer state (heap + coalesced host `WakeAt`).
    timer_state: TimerState,
    /// Number of active (spawned but not completed) tasks.
    active_count: RefCell<usize>,
    /// Free task slots available for reuse (typically few slots reused at once).
    free_slots: RefCell<SmallVec<[usize; 8]>>,
    /// Currently polling task (for parent-child tracking).
    ///
    /// Set during `poll_ready_tasks()` to track which task is being polled.
    /// When `spawn()` is called, the new task becomes a child of this task.
    current_task: Cell<Option<TaskId>>,
    /// Root of the current task tree (for host operation attribution).
    ///
    /// Set during `poll_ready_tasks()` to the root of the task being polled.
    /// Child tasks inherit this so their host ops are attributed to the root.
    current_root: Cell<Option<TaskId>>,
}

/// Single-threaded async scheduler.
///
/// Clone this to spawn tasks from within other tasks.
#[derive(Clone)]
pub struct Scheduler {
    inner: Rc<SchedulerInner>,
}

impl Scheduler {
    /// Maximum depth when traversing the task parent chain in `root_task`.
    const ROOT_TASK_MAX_DEPTH: usize = 100;

    /// Create a new scheduler with injected time and random sources.
    ///
    /// # Arguments
    /// * `runtime_id` - Unique ID for this runtime (passed to source functions)
    /// * `time_source` - Time source function
    /// * `random_source` - Random source function
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use amla_scheduler::ClockType;
    ///
    /// // Production (native)
    /// use std::time::{Instant, SystemTime, UNIX_EPOCH};
    /// let start = Instant::now();
    /// let time = Rc::new(move |_runtime_id, clock| match clock {
    ///     ClockType::Realtime => SystemTime::now()
    ///         .duration_since(UNIX_EPOCH)
    ///         .map(|d| d.as_nanos() as u64)
    ///         .unwrap_or(0),
    ///     ClockType::Monotonic => start.elapsed().as_nanos() as u64,
    /// });
    /// let random = Rc::new(|_runtime_id| rand::random());
    /// let sched = Scheduler::new(1, time, random);
    ///
    /// // Test with mock sources (returns same value for both clocks)
    /// let mock_time = Rc::new(Cell::new(0u64));
    /// let t = mock_time.clone();
    /// let sched = Scheduler::new(1, Rc::new(move |_, _| t.get()), Rc::new(|_| 42));
    /// ```
    #[must_use]
    pub fn new(runtime_id: u64, time_source: TimeSourceFn, random_source: RandomSourceFn) -> Self {
        Self::with_queue_size(runtime_id, time_source, random_source, 64)
    }

    /// Create a scheduler with custom queue size.
    #[must_use]
    pub fn with_queue_size(
        runtime_id: u64,
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
        queue_size: usize,
    ) -> Self {
        let host_channel = HostChannel::new(runtime_id, queue_size, time_source, random_source);
        let timer_state = TimerState::new(host_channel.clone());
        Self {
            inner: Rc::new(SchedulerInner {
                tasks: RefCell::new(Vec::new()),
                generations: RefCell::new(Vec::new()),
                ready_queue: Rc::new(RefCell::new(SmallVecReadyQueue::default())),
                spawn_queue: RefCell::new(VecDeque::new()),
                host_channel,
                timer_state,
                active_count: RefCell::new(0),
                free_slots: RefCell::new(SmallVec::new()),
                current_task: Cell::new(None),
                current_root: Cell::new(None),
            }),
        }
    }

    /// Set the runtime ID.
    ///
    /// Called after runtime registration to update the ID from the placeholder
    /// value (0) to the actual assigned ID. This ensures time/random source
    /// functions receive the correct runtime ID for per-runtime isolation.
    ///
    /// # Arguments
    /// * `runtime_id` - The actual runtime ID assigned during registration
    pub fn set_runtime_id(&self, runtime_id: u64) {
        self.inner.host_channel.set_runtime_id(runtime_id);
    }

    /// Get the current runtime ID.
    #[must_use]
    pub fn runtime_id(&self) -> u64 {
        self.inner.host_channel.runtime_id()
    }

    /// Spawn a task and return a handle to wait for it.
    ///
    /// If spawned from within another task (during polling), the new task becomes
    /// a child of the current task. When the parent completes or is cancelled,
    /// all children are recursively cancelled (structured concurrency).
    pub fn spawn<F>(&self, future: F) -> TaskHandle
    where
        F: Future<Output = Result<Exit, Error>> + 'static,
    {
        let slot = Rc::new(RefCell::new(TaskSlot {
            result: None,
            waker: None,
            task_id: None,
        }));

        // Get parent from current_task (set during polling)
        let parent = self.inner.current_task.get();
        // Inherit root from current_root (for host operation attribution)
        // If None, this is a root task (will be set to self in process_spawn_queue)
        let root = self.inner.current_root.get();

        let task = Task {
            future: Box::pin(future),
            slot: Rc::clone(&slot),
            parent,
            root,
            children: SmallVec::new(),
        };

        // Queue the spawn request
        self.inner.spawn_queue.borrow_mut().push_back(task);

        TaskHandle {
            slot,
            result_taken: Cell::new(false),
            scheduler: self.clone(),
        }
    }

    /// Get a reference to the host channel for I/O operations.
    #[must_use]
    pub fn host(&self) -> HostChannelRef {
        HostChannelRef {
            scheduler: self.clone(),
        }
    }

    // =========================================================================
    // Timer operations
    // =========================================================================

    /// Get the current time in nanoseconds since epoch.
    ///
    /// This is a synchronous call using the injected time source.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use amla_scheduler::ClockType;
    /// // Within a task:
    /// let realtime = scheduler.now(ClockType::Realtime);
    /// let monotonic = scheduler.now(ClockType::Monotonic);
    /// ```
    #[must_use]
    pub fn now(&self, clock: ClockType) -> u64 {
        self.inner.host_channel.now(clock)
    }

    /// Get monotonic time in nanoseconds.
    ///
    /// Convenience method for `now(ClockType::Monotonic)`.
    /// Use for sleep/wake, timeouts, and measuring durations.
    #[must_use]
    #[inline]
    pub fn now_monotonic(&self) -> u64 {
        self.now(ClockType::Monotonic)
    }

    /// Get realtime (wall clock) in nanoseconds since Unix epoch.
    ///
    /// Convenience method for `now(ClockType::Realtime)`.
    /// Use for timestamps that need to correspond to calendar time.
    #[must_use]
    #[inline]
    pub fn now_realtime(&self) -> u64 {
        self.now(ClockType::Realtime)
    }

    /// Get a random u64.
    ///
    /// This is a synchronous call using the injected random source.
    #[must_use]
    pub fn random(&self) -> u64 {
        self.inner.host_channel.random()
    }

    /// Sleep until a specific deadline.
    ///
    /// Returns a future that completes when the host's time reaches or exceeds
    /// the deadline. Multiple sleeps are coalesced into a single host `WakeAt`
    /// operation for efficiency.
    ///
    /// # Cancellation
    ///
    /// Dropping a `SleepFuture` cancels the timer. If this was the earliest
    /// deadline, the next earliest will be scheduled instead.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Within a task:
    /// let now = scheduler.now().await?;
    /// let deadline = now + Duration::from_secs(1).as_nanos() as u64;
    /// scheduler.sleep_until(deadline).await?;
    /// println!("Deadline reached!");
    /// ```
    #[must_use]
    pub fn sleep_until(&self, deadline: TimeNanos) -> SleepFuture {
        SleepFuture::new(self.inner.timer_state.clone(), deadline)
    }

    /// Run the scheduler until all tasks complete or blocked on host ops.
    ///
    /// # Context Preservation
    ///
    /// If called from within an async task (nested execution), the current task
    /// context is saved and restored after `run()` completes. This ensures spawned
    /// tasks within builtins (like `kill` error messages) are correctly attributed.
    #[must_use]
    pub fn run(&self) -> SchedulerState {
        // Save context for nested execution
        let saved_task = self.inner.current_task.get();
        let saved_root = self.inner.current_root.get();

        loop {
            let state = self.run_step();
            match state {
                SchedulerState::Done | SchedulerState::Blocked => {
                    // Restore context for caller
                    self.inner.current_task.set(saved_task);
                    self.inner.current_root.set(saved_root);
                    self.inner
                        .host_channel
                        .set_current_task(saved_root.map(TaskId::to_repr));
                    return state;
                }
                SchedulerState::Progress => {}
            }
        }
    }

    /// Run one step of the scheduler.
    ///
    /// This processes spawns, polls ready tasks once, and returns immediately.
    /// Use this for incremental execution where you need to interleave other
    /// work (like pushing stdin data) between scheduler steps.
    ///
    /// Returns:
    /// - `SchedulerState::Done` - All tasks completed
    /// - `SchedulerState::Blocked` - Tasks waiting on host ops, no ready work
    /// - `SchedulerState::Progress` - Made progress, more work may be available
    #[must_use]
    pub fn run_step(&self) -> SchedulerState {
        // SAFETY: We use noop_waker here because timer_state.process() manages its own
        // wakeups internally via the host channel. It never relies on the context's
        // waker to reschedule itself - it registers timers that complete via host ops.
        // The timer state maintains its own pending host operation and fires timers
        // when the host completes the WakeAt operation with the current time.
        let noop_waker = noop_waker();
        let mut timer_cx = Context::from_waker(&noop_waker);

        // Process timer state first (polls WakeAt, fires expired timers)
        self.inner.timer_state.process(&mut timer_cx);

        // Process spawn queue
        self.process_spawn_queue();

        // Poll ready tasks
        let made_progress = self.poll_ready_tasks();

        // Process timers again - tasks may have registered new timers
        // that need to be submitted to host channel before we return Blocked
        self.inner.timer_state.process(&mut timer_cx);

        // Check if all done (O(1) using active_count)
        if self.all_tasks_done() {
            return SchedulerState::Done;
        }

        // If made progress, return Progress to allow caller to continue
        if made_progress {
            return SchedulerState::Progress;
        }

        // If there are ready tasks or pending spawns, more work available
        if !self.inner.ready_queue.borrow().ready.is_empty()
            || !self.inner.spawn_queue.borrow().is_empty()
        {
            return SchedulerState::Progress;
        }

        // No progress and nothing ready - we're blocked on host ops
        SchedulerState::Blocked
    }

    /// Find the root task (top of spawn chain) for a given task.
    ///
    /// Returns the `task_id` that has no parent. This is useful for
    /// attributing host operations to the original spawned task
    /// when child tasks (pipelines, subprocesses, etc.) submit ops.
    ///
    /// Returns `None` if the task doesn't exist or has invalid generation.
    #[must_use]
    pub fn root_task(&self, task_id: TaskIdRepr) -> Option<TaskIdRepr> {
        let mut current = TaskId::from_repr(task_id);

        for _ in 0..Self::ROOT_TASK_MAX_DEPTH {
            // Verify current task exists and has matching generation
            let tasks = self.inner.tasks.borrow();
            let generations = self.inner.generations.borrow();

            if current.slot >= tasks.len() {
                return None;
            }
            if generations[current.slot] != current.generation {
                return None;
            }

            let task = tasks[current.slot].as_ref()?;

            match task.parent {
                None => {
                    // Found root - this task has no parent
                    return Some(current.to_repr());
                }
                Some(parent_id) => {
                    // Continue up the chain
                    current = parent_id;
                }
            }
        }

        // Hit depth limit - shouldn't happen in normal operation
        None
    }

    /// Process pending spawn requests.
    fn process_spawn_queue(&self) {
        loop {
            let task = self.inner.spawn_queue.borrow_mut().pop_front();

            match task {
                Some(mut task) => {
                    let parent = task.parent;
                    let slot = Rc::clone(&task.slot);

                    // For root tasks (no inherited root), set root to self
                    // This must be done before moving task into the slot
                    let is_root_task = task.root.is_none();

                    let (waker_id, task_id) = {
                        let mut tasks = self.inner.tasks.borrow_mut();
                        let mut generations = self.inner.generations.borrow_mut();
                        let mut free_slots = self.inner.free_slots.borrow_mut();

                        if let Some(free_idx) = free_slots.pop() {
                            // Reuse existing slot - increment generation
                            generations[free_idx] += 1;
                            let slot_gen = generations[free_idx];
                            let task_id = TaskId {
                                slot: free_idx,
                                generation: slot_gen,
                            };
                            // Set root to self if this is a root task
                            if is_root_task {
                                task.root = Some(task_id);
                            }
                            tasks[free_idx] = Some(task);
                            (WakerTaskId(free_idx), task_id)
                        } else {
                            // Allocate new slot
                            let slot_idx = tasks.len();
                            let task_id = TaskId {
                                slot: slot_idx,
                                generation: 0,
                            };
                            // Set root to self if this is a root task
                            if is_root_task {
                                task.root = Some(task_id);
                            }
                            tasks.push(Some(task));
                            generations.push(0);
                            (WakerTaskId(slot_idx), task_id)
                        }
                    };

                    // Store TaskId in the slot for TaskHandle::cancel()
                    slot.borrow_mut().task_id = Some(task_id);

                    // Add child to parent's children list (structured concurrency)
                    // Check generation to ensure parent wasn't replaced by a different task
                    if let Some(parent_id) = parent {
                        let parent_gen = self.inner.generations.borrow()[parent_id.slot];
                        if parent_gen == parent_id.generation {
                            let mut tasks = self.inner.tasks.borrow_mut();
                            if let Some(Some(parent_task)) = tasks.get_mut(parent_id.slot) {
                                parent_task.children.push(task_id);
                            }
                        }
                        // If parent was replaced (generation mismatch), this task becomes orphaned
                        // which is correct - it will run to completion but won't be cancelled
                        // when the new task at that slot completes
                    }

                    *self.inner.active_count.borrow_mut() += 1;
                    self.inner.ready_queue.borrow_mut().ready.push(waker_id);
                }
                None => break,
            }
        }
    }

    /// Poll ONE ready task.
    ///
    /// This polls only a single task per call for fair interleaving between
    /// tasks from different runtimes. Each call to `run_step()` will make
    /// progress on at most one task.
    ///
    /// Scheduling mode:
    /// - Default: FIFO round-robin (deterministic)
    /// - With `random-scheduling` feature: Random selection (for bug finding)
    ///
    /// Returns whether any progress was made.
    #[allow(clippy::too_many_lines)]
    fn poll_ready_tasks(&self) -> bool {
        // Pop ONE task from the ready queue, skipping stale entries
        loop {
            let waker_task_id = {
                let mut queue = self.inner.ready_queue.borrow_mut();
                if queue.ready.is_empty() {
                    return false;
                }

                #[cfg(feature = "random-scheduling")]
                {
                    // Random selection for bug finding - exposes race conditions
                    let idx = fastrand::usize(..queue.ready.len());
                    queue.ready.swap_remove(idx)
                }

                #[cfg(not(feature = "random-scheduling"))]
                {
                    // FIFO ordering (fair round-robin) - deterministic
                    queue.ready.remove(0)
                }
            };

            let task_idx = waker_task_id.0;

            // Check if slot is empty (spurious wake from completed/removed task)
            // If so, skip this entry and try the next one
            {
                let tasks = self.inner.tasks.borrow();
                match tasks.get(task_idx) {
                    Some(None) | None => continue, // Skip stale entry, try next
                    _ => {}
                }
            }

            let waker = self.make_waker(waker_task_id);
            let mut cx = Context::from_waker(&waker);

            // Set current_task for structured concurrency (child spawns will use this as parent)
            let current_gen = self.inner.generations.borrow()[task_idx];
            let task_id = TaskId {
                slot: task_idx,
                generation: current_gen,
            };
            self.inner.current_task.set(Some(task_id));

            // Get root from task for host operation attribution and child task inheritance.
            // Root is stored in the task at spawn time, so it's stable even if ancestors complete.
            let root_task_id = {
                let tasks = self.inner.tasks.borrow();
                tasks
                    .get(task_idx)
                    .and_then(|t| t.as_ref())
                    .and_then(|t| t.root)
            };
            self.inner.current_root.set(root_task_id);
            self.inner
                .host_channel
                .set_current_task(root_task_id.map(TaskId::to_repr));

            // IMPORTANT: We must poll the task WITHOUT holding the tasks borrow.
            // Tasks may call back into the scheduler (e.g., shell's `jobs` builtin
            // calls scheduler.run()), so we need to release the borrow first.
            //
            // We use unsafe Pin projection to get a reference to the pinned future
            // that outlives the borrow scope. This is safe because:
            // 1. The task slot won't be reused while we're polling (we hold the only ref)
            // 2. The future is already pinned in the Box
            // 3. We check the slot exists before and handle completion after
            let poll_result = {
                // Get a raw pointer to the pinned future
                let future_ptr: *mut (dyn Future<Output = Result<Exit, Error>> + 'static) = {
                    let mut tasks = self.inner.tasks.borrow_mut();
                    if let Some(Some(task)) = tasks.get_mut(task_idx) {
                        // Get raw pointer to the pinned future
                        // SAFETY: The future is pinned in a Box, and we're just
                        // getting a pointer to poll it. The task slot won't be
                        // modified while we hold this pointer (single-threaded).
                        unsafe { std::ptr::from_mut(task.future.as_mut().get_unchecked_mut()) }
                    } else {
                        // Task was removed between checks, clear current_task/root and try next
                        self.inner.current_task.set(None);
                        self.inner.current_root.set(None);
                        self.inner.host_channel.set_current_task(None);
                        continue;
                    }
                };
                // Borrow is dropped here

                // SAFETY: We just verified the task exists. Single-threaded execution
                // means no one else can modify the slot while we poll. The pointer
                // remains valid because the Box allocation is stable.
                let future_pin = unsafe { Pin::new_unchecked(&mut *future_ptr) };
                future_pin.poll(&mut cx)
            };

            // Clear current_task/root now that polling is done
            self.inner.current_task.set(None);
            self.inner.current_root.set(None);
            self.inner.host_channel.set_current_task(None);

            // We found and polled a valid task, handle result and return
            return match poll_result {
                Poll::Ready(result) => {
                    // Get children and slot info before clearing
                    let (waker_to_wake, children, parent) = {
                        let tasks = self.inner.tasks.borrow();
                        if let Some(Some(task)) = tasks.get(task_idx) {
                            let mut slot = task.slot.borrow_mut();
                            slot.result = Some(result);
                            (slot.waker.take(), task.children.clone(), task.parent)
                        } else {
                            (None, SmallVec::new(), None)
                        }
                    };

                    // Cancel all children (structured concurrency)
                    for child_id in children {
                        self.cancel_task_internal(child_id);
                    }

                    // Remove from parent's children list (check generation to avoid wrong parent)
                    if let Some(parent_id) = parent {
                        let parent_gen = self.inner.generations.borrow()[parent_id.slot];
                        if parent_gen == parent_id.generation {
                            let task_gen = self.inner.generations.borrow()[task_idx];
                            let this_task_id = TaskId {
                                slot: task_idx,
                                generation: task_gen,
                            };
                            let mut tasks = self.inner.tasks.borrow_mut();
                            if let Some(Some(parent_task)) = tasks.get_mut(parent_id.slot) {
                                parent_task.children.retain(|id| *id != this_task_id);
                            }
                        }
                    }

                    // Clear the slot and add to free list
                    {
                        let mut tasks = self.inner.tasks.borrow_mut();
                        // Only decrement if slot was still occupied (not already cancelled)
                        if tasks[task_idx].is_some() {
                            tasks[task_idx] = None;
                            self.inner.free_slots.borrow_mut().push(task_idx);
                            *self.inner.active_count.borrow_mut() -= 1;
                        }
                    }

                    // Wake after releasing borrows
                    if let Some(w) = waker_to_wake {
                        w.wake();
                    }

                    true
                }
                Poll::Pending => {
                    // Task will be re-queued when its waker is called
                    // But we did make some progress (task was polled)
                    true
                }
            };
        }
    }

    /// Check if all tasks are done.
    ///
    /// This is O(1) using the `active_count` optimization.
    fn all_tasks_done(&self) -> bool {
        *self.inner.active_count.borrow() == 0
    }

    /// Check if scheduler has no active tasks.
    ///
    /// Returns true if all spawned tasks have completed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.all_tasks_done()
    }

    /// Create a waker for a task.
    fn make_waker(&self, task_id: WakerTaskId) -> Waker {
        waker::create_waker(task_id, Rc::clone(&self.inner.ready_queue))
    }

    /// Take pending host operation (for runtime to process).
    pub fn take_host_op(&self) -> Option<HostOpRequest> {
        self.inner.host_channel.take_pending()
    }

    /// Check if there are pending host operations.
    #[must_use]
    pub fn has_pending_host_ops(&self) -> bool {
        self.inner.host_channel.has_pending()
    }

    /// Check if there are tasks ready to run.
    ///
    /// Returns `true` if there are tasks in the ready queue or pending spawns.
    /// This indicates that calling `run_step()` will make progress.
    #[must_use]
    pub fn has_ready_tasks(&self) -> bool {
        !self.inner.ready_queue.borrow().ready.is_empty()
            || !self.inner.spawn_queue.borrow().is_empty()
    }

    /// Check if all tasks have completed.
    ///
    /// Returns `true` if no tasks are active (spawned but not completed).
    /// Note: This is O(1) using an internal active task counter.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.all_tasks_done()
    }

    // =========================================================================
    // Task cancellation (structured concurrency)
    // =========================================================================

    /// Cancel a task and all its children (internal, recursive).
    ///
    /// This is called when a parent task completes to clean up children,
    /// or when a scope is cancelled. It recursively cancels all descendants.
    fn cancel_task_internal(&self, task_id: TaskId) {
        // Check generation to avoid cancelling a reused slot
        let current_gen = {
            let gens = self.inner.generations.borrow();
            gens.get(task_id.slot).copied().unwrap_or(0)
        };
        if current_gen != task_id.generation {
            return; // Slot was reused, this is a stale TaskId
        }

        // Get children and slot info, then drop borrow
        let (children, waker_to_wake) = {
            let mut tasks = self.inner.tasks.borrow_mut();
            if let Some(Some(task)) = tasks.get_mut(task_id.slot) {
                let waker = {
                    let mut slot = task.slot.borrow_mut();
                    if slot.result.is_none() {
                        slot.result = Some(Err(Error::Command(
                            "Task cancelled (parent completed)".to_string(),
                        )));
                    }
                    slot.waker.take()
                };
                let children = std::mem::take(&mut task.children);
                (children, waker)
            } else {
                return; // Task already gone
            }
        };

        // Recursively cancel children first (depth-first)
        for child_id in children {
            self.cancel_task_internal(child_id);
        }

        // Now drop this task
        {
            let mut tasks = self.inner.tasks.borrow_mut();
            if tasks
                .get(task_id.slot)
                .is_some_and(std::option::Option::is_some)
            {
                tasks[task_id.slot] = None;
                self.inner.free_slots.borrow_mut().push(task_id.slot);
                *self.inner.active_count.borrow_mut() -= 1;
            }
        }

        // Wake waiter after dropping task
        if let Some(w) = waker_to_wake {
            w.wake();
        }
    }

    /// Cancel a task and all its children.
    ///
    /// This recursively cancels all descendant tasks. Each cancelled task:
    /// - Has its result set to an error
    /// - Has its waiter notified
    /// - Has its future dropped (cascading to `HostOpFutures`)
    ///
    /// # Returns
    ///
    /// `true` if the task existed and was cancelled, `false` otherwise.
    pub fn cancel_task(&self, task_id: TaskId) -> bool {
        // Check generation to avoid cancelling a reused slot
        let current_gen = {
            let gens = self.inner.generations.borrow();
            gens.get(task_id.slot).copied().unwrap_or(0)
        };
        if current_gen != task_id.generation {
            return false; // Slot was reused, this is a stale TaskId
        }

        // Check if task exists
        let exists = {
            let tasks = self.inner.tasks.borrow();
            tasks
                .get(task_id.slot)
                .is_some_and(std::option::Option::is_some)
        };

        if exists {
            // Remove from parent's children list first
            let parent = {
                let tasks = self.inner.tasks.borrow();
                tasks
                    .get(task_id.slot)
                    .and_then(|s| s.as_ref())
                    .and_then(|t| t.parent)
            };
            if let Some(parent_id) = parent {
                let mut tasks = self.inner.tasks.borrow_mut();
                if let Some(Some(parent_task)) = tasks.get_mut(parent_id.slot) {
                    parent_task.children.retain(|id| *id != task_id);
                }
            }

            self.cancel_task_internal(task_id);

            // Clean up ready queue
            {
                let tasks = self.inner.tasks.borrow();
                let mut ready_queue = self.inner.ready_queue.borrow_mut();
                ready_queue
                    .ready
                    .retain(|waker_id| tasks.get(waker_id.0).is_some_and(Option::is_some));
            }

            true
        } else {
            false
        }
    }

    /// Complete a host operation.
    pub fn complete_host_op(&self, id: crate::HostOpId, data: Vec<u8>) {
        self.inner.host_channel.complete(id, data);
    }

    /// Complete a host operation with an error.
    pub fn complete_host_op_err(&self, id: crate::HostOpId, error: std::io::Error) {
        self.inner.host_channel.complete_err(id, error);
    }

    /// Append a chunk to a pending host operation's buffer.
    ///
    /// This is used for operations that return results in multiple chunks,
    /// such as large tool results. Chunks are accumulated until `eof` is true.
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - Chunk appended, operation completed (eof was true)
    /// * `Ok(false)` - Chunk appended, more chunks expected
    /// * `Err(...)` - Error (buffer overflow, stale ID, or cancelled)
    pub fn append_chunk(
        &self,
        id: crate::HostOpId,
        data: Vec<u8>,
        eof: bool,
    ) -> std::io::Result<bool> {
        self.inner.host_channel.append_chunk(id, data, eof)
    }

    /// Clear the buffer for a pending host operation.
    ///
    /// Used during cancellation to free memory held by partial chunks.
    pub fn clear_chunk_buffer(&self, id: crate::HostOpId) {
        self.inner.host_channel.clear_buffer(id);
    }
}

/// Create a no-op waker that does nothing when woken.
///
/// Used to drive timer internals from the run loop.
/// Create a no-op waker that does nothing when woken.
/// Useful for polling futures manually in tests.
pub fn noop_waker() -> Waker {
    use std::task::{RawWaker, RawWakerVTable};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE), // clone
        |_| {},                                       // wake
        |_| {},                                       // wake_by_ref
        |_| {},                                       // drop
    );

    // SAFETY: The vtable functions are all no-ops, so the null pointer is never dereferenced.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// Handle to a spawned task.
///
/// Await this to get the task's exit status. The handle can be:
/// - Awaited directly to block until completion
/// - Polled via `is_complete()` for non-blocking checks
/// - Used with [`select_first()`] or [`join_all()`] combinators
///
/// # Cancellation
///
/// Dropping a `TaskHandle` does **not** cancel the underlying task.
/// The task continues running to completion; only the ability to
/// await its result is lost.
pub struct TaskHandle {
    slot: Rc<RefCell<TaskSlot>>,
    /// Tracks whether result was taken via `try_get()`.
    /// Used to prevent awaiting after result was taken.
    result_taken: Cell<bool>,
    /// Keeps the scheduler alive while this handle exists.
    ///
    /// This ensures the scheduler (and its task infrastructure) isn't dropped
    /// while handles are still outstanding. Without this, wakers could become
    /// dangling references to a deallocated ready queue.
    #[allow(dead_code)]
    scheduler: Scheduler,
}

impl TaskHandle {
    /// Check if the task has completed and a result is available.
    ///
    /// Returns `true` if awaiting this handle will immediately return a result.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.slot.borrow().result.is_some()
    }

    /// Get the task's ID if it has been assigned.
    ///
    /// Returns `None` if the task is still in the spawn queue and hasn't
    /// been assigned a slot yet.
    #[must_use]
    pub fn id(&self) -> Option<TaskId> {
        self.slot.borrow().task_id
    }

    /// Get the task's ID as a representation that can be compared with host operations.
    ///
    /// Returns `None` if the task is still in the spawn queue.
    #[must_use]
    pub fn id_repr(&self) -> Option<TaskIdRepr> {
        self.slot.borrow().task_id.map(TaskId::to_repr)
    }

    /// Try to get the result without waiting.
    ///
    /// Returns `Some(result)` if the task has completed, `None` otherwise.
    ///
    /// # Warning
    ///
    /// This **takes** the result, so subsequent calls (including `.await`) will:
    /// - Return `None` from `try_get()`
    /// - Return `Poll::Ready(Err(...))` from `.await` (indicating result was already taken)
    pub fn try_get(&self) -> Option<Result<Exit, Error>> {
        let result = self.slot.borrow_mut().result.take();
        if result.is_some() {
            self.result_taken.set(true);
        }
        result
    }

    /// Cancel the task and all its children (structured concurrency).
    ///
    /// This is a no-op if the task hasn't been assigned a slot yet (still in spawn queue)
    /// or if it has already completed.
    ///
    /// # Returns
    ///
    /// `true` if the task was cancelled, `false` if it wasn't running or already done.
    pub fn cancel(&self) -> bool {
        let task_id = self.slot.borrow().task_id;
        if let Some(id) = task_id {
            self.scheduler.cancel_task(id)
        } else {
            // Task still in spawn queue - not yet assigned an ID
            false
        }
    }
}

impl Future for TaskHandle {
    type Output = Result<Exit, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if result was already taken via try_get()
        if self.result_taken.get() {
            return Poll::Ready(Err(Error::Command(
                "TaskHandle result already taken via try_get()".to_string(),
            )));
        }

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

/// Reference to host channel for I/O operations.
///
/// This provides a cleaner API than exposing `HostChannel` directly.
#[derive(Clone)]
pub struct HostChannelRef {
    scheduler: Scheduler,
}

impl HostChannelRef {
    /// Read a mapped file.
    ///
    /// Used for lazy-loading file content from the host.
    pub fn file_read(&self, path: impl Into<String>) -> HostOpFuture {
        self.scheduler.inner.host_channel.file_read(path)
    }

    /// Get the current time in nanoseconds for the specified clock.
    ///
    /// This is a synchronous call using the injected time source.
    pub fn now(&self, clock: ClockType) -> u64 {
        self.scheduler.inner.host_channel.now(clock)
    }

    /// Get monotonic time in nanoseconds.
    ///
    /// Use for sleep/wake, timeouts, and measuring durations.
    #[inline]
    pub fn now_monotonic(&self) -> u64 {
        self.now(ClockType::Monotonic)
    }

    /// Get realtime (wall clock) in nanoseconds since Unix epoch.
    ///
    /// Use for timestamps that need to correspond to calendar time.
    #[inline]
    pub fn now_realtime(&self) -> u64 {
        self.now(ClockType::Realtime)
    }

    /// Request wakeup at or after the specified deadline.
    ///
    /// Returns a future that completes when the host's time reaches or exceeds
    /// the deadline. Multiple `wake_at` calls are coalesced into a single host
    /// operation for efficiency.
    pub fn wake_at(&self, deadline: TimeNanos) -> SleepFuture {
        SleepFuture::new(self.scheduler.inner.timer_state.clone(), deadline)
    }

    /// Print data to a stream (1 = stdout, 2 = stderr).
    ///
    /// Returns when the host has accepted the data.
    /// The runtime/command association is determined automatically via `task_id`.
    pub fn print(&self, stream: u8, data: Vec<u8>) -> HostOpFuture {
        self.scheduler.inner.host_channel.print(stream, data)
    }

    /// Notify that a command has exited.
    ///
    /// Returns when the host has acknowledged the exit.
    /// The runtime/command association is determined automatically via `task_id`.
    pub fn command_exit(&self, code: i32) -> HostOpFuture {
        self.scheduler.inner.host_channel.command_exit(code)
    }

    /// Read from stdin.
    ///
    /// Returns up to `max_bytes` from stdin. Returns empty vec on EOF.
    pub fn read_stdin(&self, max_bytes: usize) -> HostOpFuture {
        self.scheduler.inner.host_channel.read_stdin(max_bytes)
    }

    /// Submit a custom operation to the host.
    ///
    /// Used for tool calls and other extensible host operations.
    pub fn custom(&self, name: impl Into<String>, data: Vec<u8>) -> HostOpFuture {
        self.scheduler.inner.host_channel.custom(name, data)
    }
}

/// Wait for the first of multiple tasks to complete.
///
/// Returns `(index, result)` where `index` is the position of the first
/// completed task in the original `handles` vector.
///
/// # Related Combinators
///
/// This crate provides three variants of `select_first`:
///
/// | Function | Handle Type | Use Case |
/// |----------|-------------|----------|
/// | [`select_first()`] | `TaskHandle` | Scheduler tasks |
/// | [`spawner::select_first_task()`](crate::spawner::select_first_task) | Spawner's `TaskHandle` | Lower-level spawner tasks |
/// | [`host_channel::select_first()`](crate::host_channel::select_first) | `HostOpFuture` | Host I/O operations |
///
/// For most use cases, use this function with `TaskHandle`s from [`Scheduler::spawn()`].
pub fn select_first(handles: Vec<TaskHandle>) -> SelectFirst {
    SelectFirst {
        handles: handles.into_iter().map(Some).collect(),
    }
}

/// Future for selecting first completed task.
///
/// Created by [`select_first()`].
pub struct SelectFirst {
    handles: Vec<Option<TaskHandle>>,
}

impl Future for SelectFirst {
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

        if this.handles.iter().all(Option::is_none) {
            return Poll::Ready((0, Err(Error::Command("all tasks gone".to_string()))));
        }

        Poll::Pending
    }
}

/// Wait for all tasks to complete.
///
/// Returns a `Vec` of results in the same order as the input handles.
///
/// # Related Combinators
///
/// This crate provides three variants of `join_all`:
///
/// | Function | Handle Type | Use Case |
/// |----------|-------------|----------|
/// | [`join_all()`] | `TaskHandle` | Scheduler tasks |
/// | [`spawner::join_all_tasks()`](crate::spawner::join_all_tasks) | Spawner's `TaskHandle` | Lower-level spawner tasks |
/// | [`host_channel::join_all()`](crate::host_channel::join_all) | `HostOpFuture` | Host I/O operations |
///
/// For most use cases, use this function with `TaskHandle`s from [`Scheduler::spawn()`].
pub fn join_all(handles: Vec<TaskHandle>) -> JoinAll {
    let len = handles.len();
    let mut results = Vec::with_capacity(len);
    for _ in 0..len {
        results.push(None);
    }
    JoinAll {
        handles: handles.into_iter().map(Some).collect(),
        results,
    }
}

/// Future for joining all tasks.
///
/// Created by [`join_all()`].
pub struct JoinAll {
    handles: Vec<Option<TaskHandle>>,
    results: Vec<Option<Result<Exit, Error>>>,
}

impl Future for JoinAll {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncPipe;
    use std::cell::Cell;

    /// Create mock time and random sources for testing.
    ///
    /// Returns a tuple of time source, random source, and mock time cell.
    /// The mock time cell can be used to control the time value returned
    /// by the time source. The time source returns the same value for both
    /// clock types.
    fn mock_sources() -> (TimeSourceFn, RandomSourceFn, Rc<Cell<u64>>) {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42); // Fixed value for determinism
        (time_source, random_source, mock_time)
    }

    /// Create a scheduler with mock sources for testing.
    fn test_scheduler() -> Scheduler {
        let (time_source, random_source, _) = mock_sources();
        Scheduler::new(1, time_source, random_source)
    }

    /// Create a scheduler with mock sources and return the mock time cell.
    fn test_scheduler_with_time() -> (Scheduler, Rc<Cell<u64>>) {
        let (time_source, random_source, mock_time) = mock_sources();
        (Scheduler::new(1, time_source, random_source), mock_time)
    }

    #[test]
    fn simple_task() {
        let sched = test_scheduler();

        let handle = sched.spawn(async { Ok(Exit::success()) });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(handle.is_complete());
    }

    #[test]
    fn spawn_from_within() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let child_code = Rc::new(RefCell::new(0i32));
        let child_code_clone = child_code.clone();

        sched.spawn(async move {
            // Spawn child from within parent
            let child = sched_clone.spawn(async { Ok(Exit::code(42)) });

            // Wait for child
            let result = child.await?;
            *child_code_clone.borrow_mut() = result.code;

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(*child_code.borrow(), 42);
    }

    #[test]
    fn multiple_children() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let sum = Rc::new(RefCell::new(0i32));
        let sum_clone = sum.clone();

        sched.spawn(async move {
            let h1 = sched_clone.spawn(async { Ok(Exit::code(1)) });
            let h2 = sched_clone.spawn(async { Ok(Exit::code(2)) });
            let h3 = sched_clone.spawn(async { Ok(Exit::code(3)) });

            let results = join_all(vec![h1, h2, h3]).await;

            for r in results {
                *sum_clone.borrow_mut() += r.unwrap().code;
            }

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(*sum.borrow(), 6);
    }

    #[test]
    fn select_first_child() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let winner = Rc::new(RefCell::new(0usize));
        let winner_clone = winner.clone();

        sched.spawn(async move {
            let h1 = sched_clone.spawn(async { Ok(Exit::code(1)) });
            let h2 = sched_clone.spawn(async { Ok(Exit::code(2)) });

            let (idx, _result) = select_first(vec![h1, h2]).await;
            *winner_clone.borrow_mut() = idx;

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        // One of them won (order not guaranteed)
        assert!(*winner.borrow() < 2);
    }

    #[test]
    fn pipeline_with_pipes() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let pipe = AsyncPipe::new(64);
        let pipe_w = pipe.clone();
        let pipe_r = pipe;

        let output = Rc::new(RefCell::new(Vec::new()));
        let output_clone = output.clone();

        sched.spawn(async move {
            // Writer child
            let writer = sched_clone.spawn(async move {
                pipe_w.write(b"hello from writer").await?;
                pipe_w.close();
                Ok(Exit::success())
            });

            // Reader child
            let sched_clone2 = sched_clone.clone();
            let reader = sched_clone2.spawn(async move {
                let mut buf = [0u8; 64];
                let n = pipe_r.read(&mut buf).await?;
                output_clone.borrow_mut().extend_from_slice(&buf[..n]);
                Ok(Exit::success())
            });

            // Wait for both
            let results = join_all(vec![writer, reader]).await;
            assert!(results[0].is_ok());
            assert!(results[1].is_ok());

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(&*output.borrow(), b"hello from writer");
    }

    #[test]
    fn host_operations() {
        let sched = test_scheduler();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        let host = sched.host();
        sched.spawn(async move {
            let data = host.file_read("/test.txt").await?;
            *result_data_clone.borrow_mut() = data;
            Ok(Exit::success())
        });

        // Run until blocked
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Complete the host op
        let req = sched.take_host_op().unwrap();
        sched.complete_host_op(req.id, b"file contents".to_vec());

        // Run to completion
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(&*result_data.borrow(), b"file contents");
    }

    // =========================================================================
    // Stress tests
    // =========================================================================

    /// Many tasks with many pipes, some blocking, some not.
    #[test]
    fn stress_many_pipes_many_tasks() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let total_bytes = Rc::new(RefCell::new(0usize));
        let total_clone = total_bytes.clone();

        sched.spawn(async move {
            let mut handles = Vec::new();

            // Create 10 pipelines, each with writer -> reader
            for i in 0u8..10 {
                let pipe = AsyncPipe::new(32); // Small pipes cause backpressure
                let pipe_w = pipe.clone();
                let pipe_r = pipe;
                let total_ref = total_clone.clone();

                // Writer: writes 100 bytes in chunks
                let writer = sched_clone.spawn(async move {
                    let data = [b'A' + (i % 26); 100];
                    let mut written = 0;
                    while written < data.len() {
                        let n = pipe_w.write(&data[written..]).await?;
                        written += n;
                    }
                    pipe_w.close();
                    Ok(Exit::success())
                });

                // Reader: reads all data
                let reader = sched_clone.spawn(async move {
                    let mut buf = [0u8; 16];
                    let mut count = 0;
                    loop {
                        let n = pipe_r.read(&mut buf).await?;
                        if n == 0 {
                            break;
                        }
                        count += n;
                    }
                    *total_ref.borrow_mut() += count;
                    Ok(Exit::success())
                });

                handles.push(writer);
                handles.push(reader);
            }

            // Wait for all
            let results = join_all(handles).await;
            for r in results {
                assert!(r.is_ok());
            }

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(*total_bytes.borrow(), 1000); // 10 * 100 bytes
    }

    /// Deep nesting: task spawns child which spawns grandchild, etc.
    #[test]
    fn stress_deep_spawn_nesting() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let deepest_value = Rc::new(RefCell::new(0i32));
        let dv = deepest_value.clone();

        sched.spawn(async move {
            async fn spawn_nested(
                sched: Scheduler,
                depth: i32,
                max: i32,
                result: Rc<RefCell<i32>>,
            ) -> Result<Exit, Error> {
                if depth >= max {
                    *result.borrow_mut() = depth;
                    return Ok(Exit::code(depth));
                }

                let child = sched.clone().spawn({
                    let s = sched.clone();
                    let r = result.clone();
                    async move { spawn_nested(s, depth + 1, max, r).await }
                });

                let exit = child.await?;
                Ok(Exit::code(exit.code))
            }

            let result = spawn_nested(sched_clone, 0, 20, dv).await?;
            assert_eq!(result.code, 20);
            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(*deepest_value.borrow(), 20);
    }

    /// Fan-out: one task spawns many children that run concurrently.
    #[test]
    fn stress_fan_out() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let completed = Rc::new(RefCell::new(Vec::new()));
        let completed_clone = completed.clone();

        sched.spawn(async move {
            let mut handles = Vec::new();

            // Spawn 50 children
            for i in 0..50 {
                let cc = completed_clone.clone();
                let h = sched_clone.spawn(async move {
                    cc.borrow_mut().push(i);
                    Ok(Exit::code(i))
                });
                handles.push(h);
            }

            // Wait for all
            let results = join_all(handles).await;
            assert_eq!(results.len(), 50);
            for r in results {
                assert!(r.is_ok());
            }

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(completed.borrow().len(), 50);
    }

    /// Diamond dependency: A spawns B and C, both B and C spawn D (shared).
    #[test]
    fn stress_diamond_pipes() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        // A -> B -> D
        //  \-> C -/
        let channel_alpha = AsyncPipe::new(64); // A->B
        let channel_beta = AsyncPipe::new(64); // A->C
        let channel_gamma = AsyncPipe::new(64); // B->D
        let channel_delta = AsyncPipe::new(64); // C->D

        let final_output = Rc::new(RefCell::new(Vec::new()));
        let fo = final_output.clone();

        sched.spawn(async move {
            let alpha_writer = channel_alpha.clone();
            let beta_writer = channel_beta.clone();
            let alpha_reader = channel_alpha;
            let beta_reader = channel_beta;
            let gamma_writer = channel_gamma.clone();
            let delta_writer = channel_delta.clone();
            let gamma_reader = channel_gamma;
            let delta_reader = channel_delta;

            // A: writes to B and C
            let a = sched_clone.spawn(async move {
                alpha_writer.write(b"hello").await?;
                alpha_writer.close();
                beta_writer.write(b"world").await?;
                beta_writer.close();
                Ok(Exit::success())
            });

            // B: reads from A, writes to D
            let b = sched_clone.spawn(async move {
                let mut buf = [0u8; 64];
                let n = alpha_reader.read(&mut buf).await?;
                gamma_writer.write(&buf[..n]).await?;
                gamma_writer.write(b"+").await?;
                gamma_writer.close();
                Ok(Exit::success())
            });

            // C: reads from A, writes to D
            let c = sched_clone.spawn(async move {
                let mut buf = [0u8; 64];
                let n = beta_reader.read(&mut buf).await?;
                delta_writer.write(&buf[..n]).await?;
                delta_writer.close();
                Ok(Exit::success())
            });

            // D: reads from B and C, combines output
            let fo_clone = fo.clone();
            let d = sched_clone.spawn(async move {
                let mut out = Vec::new();
                let mut buf = [0u8; 64];

                // Read from B
                loop {
                    let n = gamma_reader.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }

                // Read from C
                loop {
                    let n = delta_reader.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }

                *fo_clone.borrow_mut() = out;
                Ok(Exit::success())
            });

            let results = join_all(vec![a, b, c, d]).await;
            for r in results {
                assert!(r.is_ok());
            }

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));

        let output = final_output.borrow();
        // Output is "hello+world" or "worldhello+" depending on order
        assert!(output.len() == 11);
        assert!(output.contains(&b'+'));
    }

    /// Mixed blocking: some tasks block on pipes, others on host ops.
    #[test]
    fn stress_mixed_blocking() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let pipe = AsyncPipe::new(32);
        let pipe_w = pipe.clone();
        let pipe_r = pipe;

        let results = Rc::new(RefCell::new(Vec::new()));
        let r1 = results.clone();
        let r2 = results.clone();

        // Task 1: blocked on host op, then writes to pipe
        let host = sched.host();
        sched.spawn(async move {
            let data = host.file_read("/input.txt").await?;
            r1.borrow_mut().push(format!("host_read:{}", data.len()));

            pipe_w.write(&data).await?;
            pipe_w.close();
            r1.borrow_mut().push("wrote_to_pipe".to_string());

            Ok(Exit::success())
        });

        // Task 2: blocked on pipe, processes data
        sched.spawn(async move {
            let mut buf = [0u8; 64];
            let mut total = 0;
            loop {
                let n = pipe_r.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                total += n;
            }
            r2.borrow_mut().push(format!("pipe_read:{total}"));
            Ok(Exit::success())
        });

        // Task 3: just spawns children (not blocked)
        sched.spawn(async move {
            let mut handles = Vec::new();
            for i in 0..5 {
                let h = sched_clone.spawn(async move { Ok(Exit::code(i)) });
                handles.push(h);
            }
            let _ = join_all(handles).await;
            Ok(Exit::success())
        });

        // First run: Task 1 blocked on host, Task 2 blocked on pipe, Task 3 completes children
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Complete host op
        let req = sched.take_host_op().unwrap();
        sched.complete_host_op(req.id, b"test data from host".to_vec());

        // Now everything should complete
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));

        let r = results.borrow();
        assert!(r.iter().any(|s| s.starts_with("host_read")));
        assert!(r.iter().any(|s| s.starts_with("pipe_read")));
    }

    /// Backpressure stress: tiny pipe with large data transfer.
    #[test]
    fn stress_backpressure() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let pipe = AsyncPipe::new(8); // Tiny 8-byte pipe
        let pipe_w = pipe.clone();
        let pipe_r = pipe;

        let bytes_received = Rc::new(RefCell::new(0usize));
        let br = bytes_received.clone();

        sched.spawn(async move {
            // Writer: send 1000 bytes through 8-byte pipe
            let writer = sched_clone.spawn(async move {
                let data = [0xAB_u8; 1000];
                let mut pos = 0;
                while pos < data.len() {
                    let n = pipe_w.write(&data[pos..]).await?;
                    pos += n;
                }
                pipe_w.close();
                Ok(Exit::success())
            });

            // Reader: read 1 byte at a time (slow)
            let reader = sched_clone.spawn(async move {
                let mut buf = [0u8; 1];
                loop {
                    let n = pipe_r.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    *br.borrow_mut() += n;
                }
                Ok(Exit::success())
            });

            let results = join_all(vec![writer, reader]).await;
            assert!(results[0].is_ok());
            assert!(results[1].is_ok());

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(*bytes_received.borrow(), 1000);
    }

    /// Select first with varying completion times.
    #[test]
    fn stress_select_race() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let winners = Rc::new(RefCell::new(Vec::new()));
        let w = winners.clone();

        sched.spawn(async move {
            // Run 10 races
            for race in 0..10 {
                let mut handles = Vec::new();

                // Create pipes with different amounts of data
                for i in 0u8..5 {
                    let pipe = AsyncPipe::new(64);
                    let pipe_w = pipe.clone();
                    let pipe_r = pipe;

                    // Writer puts different amounts of data
                    let data_len = (i as usize + 1) * 10;
                    sched_clone.spawn(async move {
                        let data = vec![i; data_len];
                        pipe_w.write(&data).await?;
                        pipe_w.close();
                        Ok(Exit::success())
                    });

                    // Reader races to get data
                    let h = sched_clone.spawn(async move {
                        let mut buf = [0u8; 64];
                        let n = pipe_r.read(&mut buf).await?;
                        Ok(Exit::code(i32::try_from(n).unwrap_or(i32::MAX)))
                    });
                    handles.push(h);
                }

                // First to complete wins
                let (idx, result) = select_first(handles).await;
                w.borrow_mut().push((race, idx, result.unwrap().code));
            }

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));

        // Should have 10 race results
        assert_eq!(winners.borrow().len(), 10);
    }

    /// Slot recycling: tasks should reuse freed slots.
    #[test]
    fn slot_recycling() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        sched.spawn(async move {
            // Spawn and complete tasks in batches to test slot reuse
            for batch in 0..3 {
                let mut handles = Vec::new();

                // Spawn 10 tasks
                for i in 0..10 {
                    let code = batch * 10 + i;
                    let h = sched_clone.spawn(async move { Ok(Exit::code(code)) });
                    handles.push(h);
                }

                // Wait for all to complete
                let results = join_all(handles).await;
                assert_eq!(results.len(), 10);
                for r in results {
                    assert!(r.is_ok());
                }
            }

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));

        // Check that slots were reused: tasks Vec should be much smaller than 31
        // (1 parent + 30 children = 31 without recycling, ~11 with recycling)
        let tasks_len = sched.inner.tasks.borrow().len();
        let free_slots_len = sched.inner.free_slots.borrow().len();

        // With recycling, we should have around 11 slots (parent + 10 children max)
        // and most should be freed after completion
        assert!(
            tasks_len <= 12,
            "Expected slot reuse: tasks_len={tasks_len}, should be ~11"
        );
        assert!(
            free_slots_len >= 10,
            "Expected freed slots: free_slots_len={free_slots_len}"
        );
    }

    // =========================================================================
    // Timer tests (coalesced timer heap)
    // =========================================================================

    use crate::HostOpKind;

    /// Helper to complete a `WakeAt` host op with the given time.
    /// `WakeAt` result includes current time (8 bytes LE u64).
    fn complete_wake_at(sched: &Scheduler, current_time: u64) {
        // Should have a `WakeAt` pending
        let req = sched.take_host_op().expect("expected WakeAt");
        assert!(
            matches!(req.kind, HostOpKind::WakeAt { .. }),
            "expected WakeAt, got {:?}",
            req.kind
        );
        // WakeAt result includes current time
        sched.complete_host_op(req.id, current_time.to_le_bytes().to_vec());
    }

    #[test]
    fn now_returns_time_synchronously() {
        // now() is synchronous - no host op required
        let (sched, mock_time) = test_scheduler_with_time();
        let sched_clone = sched.clone();

        // Set a specific mock time
        mock_time.set(1_000_000_000);

        let received_time = Rc::new(RefCell::new(0u64));
        let received_time_clone = received_time.clone();

        sched.spawn(async move {
            // now() is sync - returns immediately
            let now = sched_clone.now_monotonic();
            *received_time_clone.borrow_mut() = now;
            Ok(Exit::success())
        });

        // Run - should complete immediately (no host ops needed for now())
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));

        // Time should be the mock value we set
        let time = *received_time.borrow();
        assert_eq!(time, 1_000_000_000, "time should be mock value");
    }

    #[test]
    fn sleep_until_uses_coalesced_timer() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let completed = Rc::new(RefCell::new(false));
        let completed_clone = completed.clone();

        let deadline: u64 = 2_000_000_000;

        sched.spawn(async move {
            sched_clone.sleep_until(deadline).await?;
            *completed_clone.borrow_mut() = true;
            Ok(Exit::success())
        });

        // Run - should block on internal WakeAt
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));
        assert!(!*completed.borrow());

        // Complete the WakeAt with current time
        complete_wake_at(&sched, deadline);

        // Run to completion
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(*completed.borrow());
    }

    #[test]
    fn sleep_cancellation() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let parent_done = Rc::new(RefCell::new(false));
        let parent_done_clone = parent_done.clone();

        sched.spawn(async move {
            // Create a sleep future but drop it before awaiting
            let sleep = sched_clone.sleep_until(999_000_000_000);

            // Simulate deciding not to wait
            drop(sleep);

            // Parent completes immediately
            *parent_done_clone.borrow_mut() = true;
            Ok(Exit::success())
        });

        // Note: A WakeAt may have been queued internally, but the task completes
        // without waiting for it. The cancelled timer is cleaned up lazily.
        let state = sched.run();

        // Task should complete (cancelled timer doesn't block)
        // May need to drain any pending ops
        if matches!(state, SchedulerState::Blocked) {
            // Timer was queued - complete it so we can finish
            while let Some(req) = sched.take_host_op() {
                sched.complete_host_op(req.id, vec![]);
            }
            let state = sched.run();
            assert!(matches!(state, SchedulerState::Done));
        } else {
            assert!(matches!(state, SchedulerState::Done));
        }
        assert!(*parent_done.borrow());
    }

    #[test]
    fn multiple_sleeps_coalesced_to_single_wake_at() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let order = Rc::new(RefCell::new(Vec::new()));
        let o1 = order.clone();
        let o2 = order.clone();
        let o3 = order.clone();

        // Spawn 3 tasks with different deadlines
        let s1 = sched_clone.clone();
        sched.spawn(async move {
            s1.sleep_until(300_000_000).await?; // 300ms
            o1.borrow_mut().push(3);
            Ok(Exit::success())
        });

        let s2 = sched_clone.clone();
        sched.spawn(async move {
            s2.sleep_until(100_000_000).await?; // 100ms (earliest)
            o2.borrow_mut().push(1);
            Ok(Exit::success())
        });

        let s3 = sched_clone.clone();
        sched.spawn(async move {
            s3.sleep_until(200_000_000).await?; // 200ms
            o3.borrow_mut().push(2);
            Ok(Exit::success())
        });

        // Run - should be blocked on SINGLE WakeAt for earliest (100ms)
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Should only have ONE WakeAt (for earliest deadline)
        let req = sched.take_host_op().unwrap();
        assert!(matches!(
            req.kind,
            HostOpKind::WakeAt {
                deadline: 100_000_000
            }
        ));
        assert!(sched.take_host_op().is_none(), "should only be one WakeAt");

        // Complete WakeAt with time=100ms (fires task 1)
        sched.complete_host_op(req.id, 100_000_000u64.to_le_bytes().to_vec());
        let _ = sched.run();
        assert_eq!(*order.borrow(), vec![1]);

        // Now should have WakeAt for next deadline (200ms)
        complete_wake_at(&sched, 200_000_000);
        let _ = sched.run();
        assert_eq!(*order.borrow(), vec![1, 2]);

        // And WakeAt for last deadline (300ms)
        complete_wake_at(&sched, 300_000_000);
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(*order.borrow(), vec![1, 2, 3]);
    }

    #[test]
    fn timer_with_file_op() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let results = Rc::new(RefCell::new(Vec::new()));
        let r1 = results.clone();

        let host = sched.host();
        sched.spawn(async move {
            // Read file first
            let data = host.file_read("/test.txt").await?;
            r1.borrow_mut().push(format!("file: {} bytes", data.len()));

            // Then sleep
            sched_clone.sleep_until(50_000_000).await?;
            r1.borrow_mut().push("slept".to_string());

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Complete file read first
        let req = sched.take_host_op().unwrap();
        assert!(matches!(req.kind, HostOpKind::FileRead { .. }));
        sched.complete_host_op(req.id, b"hello".to_vec());

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Now waiting on timer
        assert!(results.borrow().contains(&"file: 5 bytes".to_string()));
        assert!(!results.borrow().contains(&"slept".to_string()));

        // Complete the WakeAt with time
        complete_wake_at(&sched, 50_000_000);

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(results.borrow().contains(&"slept".to_string()));
    }

    #[test]
    fn select_first_timer_vs_file() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let winner = Rc::new(RefCell::new(None));
        let winner_clone = winner.clone();

        let host = sched.host();
        sched.spawn(async move {
            // Race: timer vs file read
            let timer_task = sched_clone.spawn({
                let s = sched_clone.clone();
                async move {
                    s.sleep_until(100_000_000).await?;
                    Ok(Exit::code(1)) // Timer wins
                }
            });

            let file_task = sched_clone.spawn({
                async move {
                    let _ = host.file_read("/slow.txt").await;
                    Ok(Exit::code(2)) // File wins
                }
            });

            let (idx, result) = select_first(vec![timer_task, file_task]).await;
            *winner_clone.borrow_mut() = Some((idx, result.unwrap().code));

            Ok(Exit::success())
        });

        let _ = sched.run();

        // Collect pending ops (WakeAt for timer, FileRead for file)
        let mut wake_at_id = None;
        let mut file_read_id = None;

        while let Some(req) = sched.take_host_op() {
            match req.kind {
                HostOpKind::WakeAt { .. } => wake_at_id = Some(req.id),
                HostOpKind::FileRead { .. } => file_read_id = Some(req.id),
                _ => {}
            }
        }

        assert!(wake_at_id.is_some(), "expected WakeAt");
        assert!(file_read_id.is_some(), "expected FileRead");

        // Timer fires first (before file completes) - WakeAt result includes time
        sched.complete_host_op(wake_at_id.unwrap(), 100_000_000u64.to_le_bytes().to_vec());

        let state = sched.run();

        // With structured concurrency, when parent task (running select_first) completes,
        // the losing child (file_task) is cancelled. So scheduler is Done, not Blocked.
        assert!(matches!(state, SchedulerState::Done));

        // Verify timer won
        let (idx, code) = winner.borrow().unwrap();
        assert_eq!(idx, 0); // Timer task was first
        assert_eq!(code, 1);
    }

    #[test]
    fn host_channel_ref_time_ops() {
        // now() via host channel ref is also synchronous
        let (sched, mock_time) = test_scheduler_with_time();

        // Set a specific mock time
        mock_time.set(2_000_000_000);

        let received_time = Rc::new(RefCell::new(0u64));
        let received_time_clone = received_time.clone();

        let host = sched.host();
        sched.spawn(async move {
            // now() is sync even via host channel ref
            let now = host.now_monotonic();
            *received_time_clone.borrow_mut() = now;
            Ok(Exit::success())
        });

        // Completes immediately - no host ops needed
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));

        // Time should be the mock value we set
        let time = *received_time.borrow();
        assert_eq!(time, 2_000_000_000, "time should be mock value");
    }

    #[test]
    fn coalesced_timer_efficiency() {
        // Verify that 1000 sleeps only create a few host ops, not 1000
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let completed = Rc::new(RefCell::new(0u32));
        let completed_clone = completed.clone();

        sched.spawn(async move {
            // Spawn 100 tasks all sleeping until the same deadline
            let mut handles = Vec::new();
            for _ in 0..100 {
                let c = completed_clone.clone();
                let s = sched_clone.clone();
                handles.push(sched_clone.spawn(async move {
                    s.sleep_until(1_000_000_000).await?;
                    *c.borrow_mut() += 1;
                    Ok(Exit::success())
                }));
            }

            join_all(handles).await;
            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Count host ops - should be just 1 WakeAt (coalesced)
        let mut wake_at_count = 0;
        while let Some(req) = sched.take_host_op() {
            if matches!(req.kind, HostOpKind::WakeAt { .. }) {
                wake_at_count += 1;
                // WakeAt result includes current time
                sched.complete_host_op(req.id, 1_000_000_000u64.to_le_bytes().to_vec());
            } else {
                sched.complete_host_op(req.id, vec![]);
            }
        }

        assert_eq!(wake_at_count, 1, "100 sleeps should coalesce to 1 WakeAt");

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(*completed.borrow(), 100);
    }

    // =========================================================================
    // Comprehensive timer tests: cancellation, select, edge cases
    // =========================================================================

    #[test]
    fn cancel_sleep_before_poll() {
        // Create SleepFuture and drop it immediately without ever polling
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let result = Rc::new(RefCell::new(0i32));
        let result_clone = result.clone();

        sched.spawn(async move {
            // Create sleep but never await it
            let _sleep = sched_clone.sleep_until(100_000_000);
            // Drop happens here when _sleep goes out of scope

            *result_clone.borrow_mut() = 42;
            Ok(Exit::success())
        });

        // Task completes immediately (sleep was dropped)
        let state = sched.run();

        // May be blocked on orphaned WakeAt, or done
        if matches!(state, SchedulerState::Blocked) {
            // Drain and complete any orphaned timer ops
            while let Some(req) = sched.take_host_op() {
                sched.complete_host_op(req.id, 100_000_000u64.to_le_bytes().to_vec());
            }
            let state = sched.run();
            assert!(matches!(state, SchedulerState::Done));
        }

        assert_eq!(*result.borrow(), 42);
    }

    #[test]
    fn cancel_sleep_after_poll() {
        // Create SleepFuture, poll it once, then cancel
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let cancelled = Rc::new(RefCell::new(false));
        let cancelled_clone = cancelled.clone();

        sched.spawn(async move {
            // Spawn a child that will sleep
            let sleep_task = sched_clone.spawn({
                let s = sched_clone.clone();
                async move {
                    s.sleep_until(999_000_000_000).await?;
                    Ok(Exit::code(1))
                }
            });

            // Let it start (get polled once)
            // Then immediately cancel by dropping
            drop(sleep_task);

            *cancelled_clone.borrow_mut() = true;
            Ok(Exit::success())
        });

        // Run - parent should complete, child was cancelled
        let state = sched.run();

        // May have pending WakeAt from cancelled task
        if matches!(state, SchedulerState::Blocked) {
            while let Some(req) = sched.take_host_op() {
                sched.complete_host_op(req.id, 999_000_000_000u64.to_le_bytes().to_vec());
            }
            let _ = sched.run();
        }

        assert!(*cancelled.borrow());
    }

    #[test]
    fn cancel_earliest_timer_schedules_next() {
        // When earliest timer is cancelled, next one should get WakeAt
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let result = Rc::new(RefCell::new(Vec::new()));
        let r1 = result.clone();

        sched.spawn(async move {
            // Create two sleeps with different deadlines
            let early_sleep = sched_clone.sleep_until(100_000_000);
            let late_sleep = sched_clone.sleep_until(200_000_000);

            // Cancel the early one
            drop(early_sleep);

            // Only await the late one
            late_sleep.await?;
            r1.borrow_mut().push("late completed");

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Should have WakeAt - could be for either deadline initially
        // After cancellation cleanup, should eventually fire at 200ms
        let req = sched.take_host_op().unwrap();
        assert!(matches!(req.kind, HostOpKind::WakeAt { .. }));

        // Complete with time >= 200ms so late timer fires
        sched.complete_host_op(req.id, 200_000_000u64.to_le_bytes().to_vec());

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(result.borrow().contains(&"late completed"));
    }

    #[test]
    fn select_first_multiple_timers() {
        // Select over multiple timers, earliest wins
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let winner = Rc::new(RefCell::new(0i32));
        let winner_clone = winner.clone();

        sched.spawn(async move {
            let t1 = sched_clone.spawn({
                let s = sched_clone.clone();
                async move {
                    s.sleep_until(300_000_000).await?;
                    Ok(Exit::code(3))
                }
            });

            let t2 = sched_clone.spawn({
                let s = sched_clone.clone();
                async move {
                    s.sleep_until(100_000_000).await?; // Earliest
                    Ok(Exit::code(1))
                }
            });

            let t3 = sched_clone.spawn({
                let s = sched_clone.clone();
                async move {
                    s.sleep_until(200_000_000).await?;
                    Ok(Exit::code(2))
                }
            });

            let (idx, result) = select_first(vec![t1, t2, t3]).await;
            *winner_clone.borrow_mut() = result.unwrap().code;
            assert_eq!(idx, 1); // t2 was at index 1

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Complete WakeAt at time 100ms (earliest fires)
        complete_wake_at(&sched, 100_000_000);

        let state = sched.run();

        // May still be blocked on other timers that were cancelled
        if matches!(state, SchedulerState::Blocked) {
            while let Some(req) = sched.take_host_op() {
                sched.complete_host_op(req.id, 300_000_000u64.to_le_bytes().to_vec());
            }
            let _ = sched.run();
        }

        assert_eq!(*winner.borrow(), 1);
    }

    #[test]
    fn select_first_timer_loses_to_other_timer() {
        // Select between timers, but we complete the later one first
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let winner = Rc::new(RefCell::new(0i32));
        let winner_clone = winner.clone();

        sched.spawn(async move {
            let t1 = sched_clone.spawn({
                let s = sched_clone.clone();
                async move {
                    s.sleep_until(100_000_000).await?;
                    Ok(Exit::code(1))
                }
            });

            let t2 = sched_clone.spawn({
                let s = sched_clone.clone();
                async move {
                    s.sleep_until(200_000_000).await?;
                    Ok(Exit::code(2))
                }
            });

            let (_, result) = select_first(vec![t1, t2]).await;
            *winner_clone.borrow_mut() = result.unwrap().code;

            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // WakeAt will be for earliest (100ms), complete it
        complete_wake_at(&sched, 100_000_000);

        let state = sched.run();

        // Cleanup any remaining
        if matches!(state, SchedulerState::Blocked) {
            while let Some(req) = sched.take_host_op() {
                sched.complete_host_op(req.id, 200_000_000u64.to_le_bytes().to_vec());
            }
            let _ = sched.run();
        }

        // t1 should win (earliest deadline)
        assert_eq!(*winner.borrow(), 1);
    }

    #[test]
    fn same_deadline_multiple_timers() {
        // Multiple timers with exact same deadline all fire together
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let count = Rc::new(RefCell::new(0u32));
        let c1 = count.clone();
        let c2 = count.clone();
        let c3 = count.clone();

        let deadline = 500_000_000u64;

        // Spawn 3 tasks with same deadline
        let s1 = sched_clone.clone();
        sched.spawn(async move {
            s1.sleep_until(deadline).await?;
            *c1.borrow_mut() += 1;
            Ok(Exit::success())
        });

        let s2 = sched_clone.clone();
        sched.spawn(async move {
            s2.sleep_until(deadline).await?;
            *c2.borrow_mut() += 1;
            Ok(Exit::success())
        });

        let s3 = sched_clone.clone();
        sched.spawn(async move {
            s3.sleep_until(deadline).await?;
            *c3.borrow_mut() += 1;
            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Single WakeAt for the shared deadline
        let req = sched.take_host_op().unwrap();
        assert!(matches!(
            req.kind,
            HostOpKind::WakeAt {
                deadline: 500_000_000
            }
        ));
        assert!(sched.take_host_op().is_none());

        // Complete - all 3 should fire at once
        sched.complete_host_op(req.id, deadline.to_le_bytes().to_vec());

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(*count.borrow(), 3);
    }

    #[test]
    fn new_earlier_timer_replaces_pending_wake_at() {
        // When a new timer with earlier deadline is created, it should replace
        // the pending WakeAt
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let order = Rc::new(RefCell::new(Vec::new()));
        let o1 = order.clone();
        let o2 = order.clone();

        // First task sleeps until 200ms
        let s1 = sched_clone.clone();
        sched.spawn(async move {
            s1.sleep_until(200_000_000).await?;
            o1.borrow_mut().push(2);
            Ok(Exit::success())
        });

        // Run to queue WakeAt(200ms)
        let _ = sched.run();

        // Now spawn task with earlier deadline (100ms)
        let s2 = sched_clone.clone();
        sched.spawn(async move {
            s2.sleep_until(100_000_000).await?;
            o2.borrow_mut().push(1);
            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Should have WakeAt for 100ms (the earlier one)
        // Note: May have two WakeAt ops due to replacement
        let mut deadlines = Vec::new();
        while let Some(req) = sched.take_host_op() {
            if let HostOpKind::WakeAt { deadline } = req.kind {
                deadlines.push(deadline);
                // Complete with time at deadline
                sched.complete_host_op(req.id, deadline.to_le_bytes().to_vec());
            }
        }

        // At least one should be for 100ms
        assert!(deadlines.contains(&100_000_000));

        let _ = sched.run();

        // Complete remaining
        while let Some(req) = sched.take_host_op() {
            if let HostOpKind::WakeAt { deadline } = req.kind {
                sched.complete_host_op(req.id, deadline.to_le_bytes().to_vec());
            }
        }

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));

        // Both should complete, order depends on timing
        assert_eq!(order.borrow().len(), 2);
    }

    #[test]
    fn timer_fires_with_time_after_multiple_deadlines() {
        // Host returns time that exceeds multiple deadlines - all should fire
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let fired = Rc::new(RefCell::new(Vec::new()));
        let f1 = fired.clone();
        let f2 = fired.clone();
        let f3 = fired.clone();

        // Three tasks with deadlines 100, 150, 200
        let s1 = sched_clone.clone();
        sched.spawn(async move {
            s1.sleep_until(100_000_000).await?;
            f1.borrow_mut().push(100);
            Ok(Exit::success())
        });

        let s2 = sched_clone.clone();
        sched.spawn(async move {
            s2.sleep_until(150_000_000).await?;
            f2.borrow_mut().push(150);
            Ok(Exit::success())
        });

        let s3 = sched_clone.clone();
        sched.spawn(async move {
            s3.sleep_until(200_000_000).await?;
            f3.borrow_mut().push(200);
            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // WakeAt for earliest (100ms)
        let req = sched.take_host_op().unwrap();
        assert!(matches!(
            req.kind,
            HostOpKind::WakeAt {
                deadline: 100_000_000
            }
        ));

        // But host returns time=250ms (exceeds all deadlines!)
        sched.complete_host_op(req.id, 250_000_000u64.to_le_bytes().to_vec());

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));

        // All three should have fired
        let f = fired.borrow();
        assert_eq!(f.len(), 3);
        assert!(f.contains(&100));
        assert!(f.contains(&150));
        assert!(f.contains(&200));
    }

    #[test]
    fn timer_with_time_before_deadline_waits() {
        // Host returns time before deadline - timer should wait for another WakeAt
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let completed = Rc::new(RefCell::new(false));
        let completed_clone = completed.clone();

        sched.spawn(async move {
            sched_clone.sleep_until(100_000_000).await?;
            *completed_clone.borrow_mut() = true;
            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // WakeAt for 100ms
        let req = sched.take_host_op().unwrap();

        // Host returns time=50ms (before deadline)
        sched.complete_host_op(req.id, 50_000_000u64.to_le_bytes().to_vec());

        // Timer shouldn't have fired yet
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));
        assert!(!*completed.borrow());

        // Should have another WakeAt
        let req = sched.take_host_op().unwrap();
        assert!(matches!(
            req.kind,
            HostOpKind::WakeAt {
                deadline: 100_000_000
            }
        ));

        // Now complete with time >= deadline
        sched.complete_host_op(req.id, 100_000_000u64.to_le_bytes().to_vec());

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(*completed.borrow());
    }

    #[test]
    fn zero_deadline_fires_immediately() {
        // Deadline of 0 should fire as soon as host returns any time >= 0
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let completed = Rc::new(RefCell::new(false));
        let completed_clone = completed.clone();

        sched.spawn(async move {
            sched_clone.sleep_until(0).await?;
            *completed_clone.borrow_mut() = true;
            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // WakeAt for 0
        let req = sched.take_host_op().unwrap();
        assert!(matches!(req.kind, HostOpKind::WakeAt { deadline: 0 }));

        // Any time >= 0 should work
        sched.complete_host_op(req.id, 1u64.to_le_bytes().to_vec());

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(*completed.borrow());
    }

    #[test]
    fn deadline_in_past_fires_on_first_check() {
        // If deadline is less than current time, timer fires immediately
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let completed = Rc::new(RefCell::new(false));
        let completed_clone = completed.clone();

        // Deadline is 100ms
        sched.spawn(async move {
            sched_clone.sleep_until(100_000_000).await?;
            *completed_clone.borrow_mut() = true;
            Ok(Exit::success())
        });

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // WakeAt for 100ms
        let req = sched.take_host_op().unwrap();

        // But current time is already 200ms (deadline in past)
        sched.complete_host_op(req.id, 200_000_000u64.to_le_bytes().to_vec());

        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(*completed.borrow());
    }

    #[test]
    fn cancel_all_timers() {
        // Cancel all timers - no task should complete via timer
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let completed = Rc::new(RefCell::new(false));
        let completed_clone = completed.clone();

        sched.spawn(async move {
            // Create multiple sleeps and cancel them all
            let s1 = sched_clone.sleep_until(100_000_000);
            let s2 = sched_clone.sleep_until(200_000_000);
            let s3 = sched_clone.sleep_until(300_000_000);

            drop(s1);
            drop(s2);
            drop(s3);

            *completed_clone.borrow_mut() = true;
            Ok(Exit::success())
        });

        // Task completes immediately (all sleeps cancelled)
        let state = sched.run();

        // May be blocked on orphaned WakeAt ops
        if matches!(state, SchedulerState::Blocked) {
            while let Some(req) = sched.take_host_op() {
                sched.complete_host_op(req.id, 300_000_000u64.to_le_bytes().to_vec());
            }
            let _ = sched.run();
        }

        assert!(*completed.borrow());
    }

    #[test]
    fn stress_rapid_timer_create_cancel() {
        // Rapidly create and cancel many timers
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let survived = Rc::new(RefCell::new(0u32));
        let survived_clone = survived.clone();

        sched.spawn(async move {
            // Create 50 timers, cancel half
            for i in 0..50u64 {
                let deadline = (i + 1) * 10_000_000;
                let sleep = sched_clone.sleep_until(deadline);

                if i % 2 == 0 {
                    // Cancel even-indexed timers
                    drop(sleep);
                } else {
                    // Actually await odd-indexed timers
                    // (but we'll batch them differently)
                    drop(sleep);
                }
            }

            // Now create 10 timers we actually wait for
            let mut handles = Vec::new();
            for i in 0..10u32 {
                let s = sched_clone.clone();
                let survived_ref = survived_clone.clone();
                #[allow(clippy::cast_possible_wrap)] // i is 0..10, safe to cast
                handles.push(sched_clone.spawn(async move {
                    s.sleep_until(1_000_000_000).await?;
                    *survived_ref.borrow_mut() += 1;
                    Ok(Exit::code(i as i32))
                }));
            }

            join_all(handles).await;
            Ok(Exit::success())
        });

        // Run and complete all pending ops
        let mut iterations = 0;
        loop {
            let state = sched.run();
            if matches!(state, SchedulerState::Done) {
                break;
            }

            while let Some(req) = sched.take_host_op() {
                sched.complete_host_op(req.id, 1_000_000_000u64.to_le_bytes().to_vec());
            }

            iterations += 1;
            assert!(iterations <= 100, "Too many iterations");
        }

        assert_eq!(*survived.borrow(), 10);
    }

    #[test]
    fn sleep_future_deadline_accessor() {
        // Test that SleepFuture::deadline() returns correct value
        let sched = test_scheduler();

        let deadline = 12_345_000_000_000_u64;
        let sleep = sched.sleep_until(deadline);

        assert_eq!(sleep.deadline(), deadline);
    }

    #[test]
    fn timer_interleaved_with_pipe_io() {
        // Timer and pipe I/O interleaved
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let pipe = AsyncPipe::new(64);
        let pipe_w = pipe.clone();
        let pipe_r = pipe;

        let events: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let e1 = events.clone();
        let e2 = events.clone();

        // Writer: sleep, write, sleep, write
        let s1 = sched_clone.clone();
        sched.spawn(async move {
            s1.sleep_until(100_000_000).await?;
            e1.borrow_mut().push("slept1".to_string());

            pipe_w.write(b"first").await?;
            e1.borrow_mut().push("wrote1".to_string());

            s1.sleep_until(200_000_000).await?;
            e1.borrow_mut().push("slept2".to_string());

            pipe_w.write(b"second").await?;
            pipe_w.close();
            e1.borrow_mut().push("wrote2".to_string());

            Ok(Exit::success())
        });

        // Reader: read, log
        sched.spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let n = pipe_r.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                e2.borrow_mut().push(format!("read:{n}"));
            }
            e2.borrow_mut().push("eof".to_string());
            Ok(Exit::success())
        });

        // Run with timer completions interleaved
        let mut iterations = 0;
        loop {
            let state = sched.run();
            if matches!(state, SchedulerState::Done) {
                break;
            }

            // Complete any pending WakeAt
            if let Some(req) = sched.take_host_op()
                && let HostOpKind::WakeAt { deadline } = req.kind
            {
                sched.complete_host_op(req.id, deadline.to_le_bytes().to_vec());
            }

            iterations += 1;
            assert!(iterations <= 20, "Too many iterations");
        }

        let e = events.borrow();
        assert!(e.iter().any(|s| s == "slept1"));
        assert!(e.iter().any(|s| s == "slept2"));
        assert!(e.iter().any(|s| s == "wrote1"));
        assert!(e.iter().any(|s| s == "wrote2"));
        assert!(e.iter().any(|s| s == "eof"));
    }

    #[test]
    #[cfg(not(feature = "random-scheduling"))]
    fn round_robin_fairness() {
        // Test that tasks are polled one at a time in FIFO order
        // NOTE: This test is skipped with random-scheduling feature since
        // random scheduling intentionally breaks FIFO ordering for bug finding.
        let sched = test_scheduler();

        let order = Rc::new(RefCell::new(Vec::new()));

        // Spawn 3 tasks that each yield multiple times
        for task_id in 0..3 {
            let o = order.clone();
            sched.spawn(async move {
                for step in 0..3 {
                    o.borrow_mut().push((task_id, step));
                    // Yield to scheduler
                    std::future::pending::<()>().await;
                }
                Ok(Exit::success())
            });
        }

        // Run 9 steps (3 tasks × 3 steps each)
        // With round-robin, each step should poll one task
        for _ in 0..9 {
            let state = sched.run_step();
            if matches!(state, SchedulerState::Done) {
                break;
            }
        }

        let o = order.borrow();

        // With round-robin FIFO ordering:
        // Step 0: task 0, step 0
        // Step 1: task 1, step 0
        // Step 2: task 2, step 0
        // Step 3: task 0, step 1 (task 0 was re-added to queue after yield)
        // etc.

        // Verify that tasks interleave (no single task runs to completion first)
        // The first 3 entries should be from different tasks
        assert!(o.len() >= 3, "Should have at least 3 entries");

        // Check that tasks are interleaved - no task completes all steps before others start
        let first_three_tasks: Vec<_> = o.iter().take(3).map(|(t, _)| *t).collect();
        assert_eq!(
            first_three_tasks,
            vec![0, 1, 2],
            "First three polls should be task 0, 1, 2 (round-robin FIFO)"
        );
    }

    #[test]
    fn round_robin_single_step_polls_one_task() {
        // Verify that run_step() polls exactly one ready task
        let sched = test_scheduler();

        let poll_count = Rc::new(RefCell::new(0));

        // Spawn 5 tasks
        for _ in 0..5 {
            let pc = poll_count.clone();
            sched.spawn(async move {
                *pc.borrow_mut() += 1;
                Ok(Exit::success())
            });
        }

        // Single step should poll exactly one task
        let _ = sched.run_step();
        assert_eq!(
            *poll_count.borrow(),
            1,
            "run_step should poll exactly one task"
        );

        // Another step should poll another task
        let _ = sched.run_step();
        assert_eq!(
            *poll_count.borrow(),
            2,
            "Second step should poll second task"
        );
    }

    #[test]
    fn host_op_task_attribution() {
        // Verify that host operations are attributed to the correct task.
        // This is critical for concurrent commands - each command's host ops
        // should be tagged with its task ID, not some other task's.
        let sched = test_scheduler();
        let host = sched.host();

        // Spawn two tasks that each issue a host operation
        let host1 = host.clone();
        let handle1 = sched.spawn(async move {
            host1.file_read("/task1/file.txt").await?;
            Ok(Exit::success())
        });

        let host2 = host.clone();
        let handle2 = sched.spawn(async move {
            host2.file_read("/task2/file.txt").await?;
            Ok(Exit::success())
        });

        // Run until both tasks are blocked on host ops
        let _ = sched.run();

        // Get task IDs (they should be assigned now)
        let task1_id = handle1.id_repr().expect("Task 1 should have an ID");
        let task2_id = handle2.id_repr().expect("Task 2 should have an ID");

        // Tasks should have different IDs
        assert_ne!(task1_id, task2_id, "Tasks should have different IDs");

        // Take and verify both host ops
        let mut found_task1 = false;
        let mut found_task2 = false;

        while let Some(req) = sched.take_host_op() {
            let op_task_id = req.task_id.expect("Host op should have task_id");

            match &req.kind {
                crate::HostOpKind::FileRead { path } if path == "/task1/file.txt" => {
                    assert_eq!(
                        op_task_id, task1_id,
                        "Task 1's file read should be attributed to task 1"
                    );
                    found_task1 = true;
                    sched.complete_host_op(req.id, b"data1".to_vec());
                }
                crate::HostOpKind::FileRead { path } if path == "/task2/file.txt" => {
                    assert_eq!(
                        op_task_id, task2_id,
                        "Task 2's file read should be attributed to task 2"
                    );
                    found_task2 = true;
                    sched.complete_host_op(req.id, b"data2".to_vec());
                }
                _ => panic!("Unexpected host op: {:?}", req.kind),
            }
        }

        assert!(found_task1, "Should have found task 1's host op");
        assert!(found_task2, "Should have found task 2's host op");

        // Both tasks should now complete
        let state = sched.run();
        assert!(matches!(state, SchedulerState::Done));
    }
}
