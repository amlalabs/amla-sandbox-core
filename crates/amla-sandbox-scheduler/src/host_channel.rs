//! Host operation channel with bounded queue and select support.
//!
//! This module provides communication between async tasks and the host runtime
//! for operations like file I/O, network calls, etc.
//!
//! ## Design
//!
//! ```text
//! Task                      HostChannel                     Runtime
//!   |                            |                             |
//!   |-- file_read("/x") -------->|  (returns HostOpFuture)     |
//!   |                            |                             |
//!   |-- .await ----------------->|                             |
//!   |   (task yields, op queued) |                             |
//!   |                            |<-- take_pending() ----------|
//!   |                            |--- Some(FileRead) --------->|
//!   |                            |                             |
//!   |                            |<-- complete(id, data) ------|
//!   |<-- result ----------------|  (task resumes)             |
//! ```
//!
//! ## Usage
//!
//! ```rust,ignore
//! // Simple await
//! let data = channel.file_read("/test.txt").await?;
//!
//! // Select first of multiple
//! let op1 = channel.file_read("/a.txt");
//! let op2 = channel.file_read("/b.txt");
//! let (idx, result) = select_first(vec![op1, op2]).await;
//!
//! // Cancellation: just drop the future
//! let op = channel.file_read("/slow.txt");
//! drop(op);  // Operation cancelled
//! ```
//!
//! ## Cancellation Semantics
//!
//! Dropping a [`HostOpFuture`] cancels the operation. The cancellation behavior
//! depends on the operation's state:
//!
//! | State | Drop Behavior |
//! |-------|---------------|
//! | Not yet queued | Operation never submitted |
//! | Queued, not taken | Removed from queue |
//! | Taken by runtime | Marked cancelled; result discarded when complete |
//! | Already completed | Result discarded |
//!
//! **Important:** The runtime may still complete the operation after cancellation.
//! For example, if a file write is cancelled after the runtime takes it, the write
//! may still occur. Cancellation only guarantees the task won't receive the result.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use smallvec::SmallVec;

/// Task identifier representation for host operations.
///
/// This is a simple (slot, generation) pair that can be compared across
/// the scheduler boundary without creating circular dependencies.
/// The scheduler's `TaskId` can be converted to/from this representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskIdRepr {
    /// Slot index in the task array.
    pub slot: usize,
    /// Generation counter for ABA problem prevention.
    pub generation: usize,
}

/// Unique identifier for a host operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostOpId(u64);

impl From<u64> for HostOpId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<HostOpId> for u64 {
    fn from(id: HostOpId) -> Self {
        id.0
    }
}

impl HostOpId {
    /// Get the slot index for this operation ID.
    ///
    /// The ID encodes both index (low 32 bits) and generation (high 32 bits).
    /// This method extracts just the index.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    fn index(self) -> usize {
        (self.0 & 0xFFFF_FFFF) as usize
    }

    /// Get the generation for this operation ID.
    #[inline]
    #[allow(dead_code)]
    fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

/// Type of host operation.
#[derive(Debug, Clone)]
pub enum HostOpKind {
    /// Read a mapped file.
    ///
    /// Used for lazy-loading file content from the host.
    FileRead {
        /// Path to the file.
        path: String,
    },
    /// Read a range of bytes from a mapped file.
    ///
    /// More efficient than `FileRead` for large files when only a portion
    /// is needed (e.g., lazy block loading).
    FileReadRange {
        /// Path to the file.
        path: String,
        /// Offset to start reading from.
        offset: u64,
        /// Maximum bytes to read.
        length: u64,
    },
    /// Custom operation (for extensibility).
    Custom {
        /// Operation name.
        name: String,
        /// Operation data.
        data: Vec<u8>,
    },
    /// Request wakeup at or after the specified deadline.
    ///
    /// The host should complete this operation when the current time
    /// reaches or exceeds the deadline. The response must be 8 bytes
    /// representing the current time as a little-endian u64 (same format as `Now`).
    WakeAt {
        /// Deadline in nanoseconds since epoch.
        deadline: u64,
    },
    /// Print data to stdout or stderr.
    ///
    /// This is used when command output needs to be streamed to the host.
    /// The host should write the data to the appropriate stream and return
    /// an empty response (or error if writing fails).
    ///
    /// The runtime/command association comes from the `task_id` in `HostOpRequest`,
    /// which is automatically set by the scheduler during task polling.
    ///
    /// Stream values:
    /// - 1: stdout
    /// - 2: stderr
    Print {
        /// Stream to write to (1 = stdout, 2 = stderr).
        stream: u8,
        /// Data to write.
        data: Vec<u8>,
    },
    /// Command has exited.
    ///
    /// Notifies the host that a command has completed with an exit code.
    /// The runtime/command association comes from the `task_id` in `HostOpRequest`.
    CommandExit {
        /// Exit code (0 = success).
        code: i32,
    },
    /// Read from stdin.
    ///
    /// The host should read up to `max_bytes` from stdin and return the data.
    /// Returns empty vec on EOF.
    ReadStdin {
        /// Maximum bytes to read.
        max_bytes: usize,
    },
}

/// A pending host operation request.
#[derive(Debug)]
pub struct HostOpRequest {
    /// Unique ID for this operation.
    pub id: HostOpId,
    /// The operation to perform.
    pub kind: HostOpKind,
    /// The task that submitted this operation (if known).
    ///
    /// This is set when the operation is submitted during task polling,
    /// allowing the runtime to attribute operations to specific commands.
    pub task_id: Option<TaskIdRepr>,
}

/// Default maximum size for chunked result accumulation (10 MB).
pub const DEFAULT_CHUNK_BUFFER_LIMIT: usize = 10 * 1024 * 1024;

/// Slot for tracking a pending operation's result.
///
/// Note: This follows the same "completion slot" pattern as [`scheduler::TaskSlot`]
/// and [`spawner::TaskSlot`]. This variant includes `cancelled` for host operation
/// cancellation semantics (see module docs for details).
///
/// ## Chunked Results
///
/// For operations that return results in chunks (like large tool results), the
/// `buffer` field accumulates data until the final chunk is received. The `buffer_limit`
/// prevents unbounded memory growth from malicious or buggy hosts.
struct PendingSlot {
    /// The result, if completed.
    result: Option<io::Result<Vec<u8>>>,
    /// Buffer for accumulating chunked results.
    buffer: Vec<u8>,
    /// Maximum allowed buffer size (protects against unbounded growth).
    buffer_limit: usize,
    /// Waker to call when result is ready.
    waker: Option<Waker>,
    /// Whether this slot has been cancelled.
    cancelled: bool,
}

/// State of a host operation future.
enum HostOpState {
    /// Not yet submitted to the queue.
    Pending {
        /// The operation kind.
        kind: HostOpKind,
    },
    /// Submitted and waiting for result.
    Submitted {
        /// Operation ID.
        id: HostOpId,
    },
    /// Completed or cancelled.
    Done,
}

/// Info about a slot for generation tracking.
#[derive(Clone, Copy)]
struct SlotInfo {
    /// Generation number for this slot. Incremented on each reuse.
    /// Starts at 1 to avoid collision with `NOTIFICATION_ID` (0) in `host_ops`.
    generation: u32,
}

impl Default for SlotInfo {
    fn default() -> Self {
        // Start generation at 1, not 0, to avoid ID collision with NOTIFICATION_ID.
        // First allocated ID will be (1 << 32) | 0 = 4294967296, not 0.
        Self { generation: 1 }
    }
}

/// Shared state for the host channel.
///
/// # Performance Note
///
/// We use `Vec<Option<PendingSlot>>` instead of `HashMap<HostOpId, PendingSlot>` for slot storage.
/// The slot index and generation are encoded in `HostOpId` for O(1) lookup with generation validation.
/// This is more cache-friendly for typical workloads (<100 concurrent operations).
/// Slots are recycled via `free_slots` to bound memory growth.
///
/// # ID Format
///
/// `HostOpId` encodes both index and generation: `(generation << 32) | index`.
/// When a slot is reused, its generation increments, making old IDs invalid.
/// This prevents completing the wrong operation when IDs are reused within a step.
struct HostChannelInner {
    /// Queue of pending requests (bounded).
    pending_requests: VecDeque<HostOpRequest>,
    /// Maximum pending requests (backpressure threshold).
    max_pending: usize,
    /// Slots for tracking results. Vec with slot index for O(1) lookup.
    slots: Vec<Option<PendingSlot>>,
    /// Generation info per slot index (never shrinks).
    slot_info: Vec<SlotInfo>,
    /// Free slot indices for reuse.
    free_slots: SmallVec<[usize; 8]>,
    /// FIFO queue of wakers for tasks blocked on submit when queue was full.
    ///
    /// We wake one waiter per slot freed (not all), avoiding thundering herd.
    submit_wakers: VecDeque<Waker>,
    /// Currently polling task (set by scheduler during task polling).
    ///
    /// This is used to track which task submitted each host operation,
    /// enabling correct attribution in the runtime.
    current_task: Option<TaskIdRepr>,
}

impl HostChannelInner {
    fn new(max_pending: usize) -> Self {
        Self {
            pending_requests: VecDeque::new(),
            max_pending,
            slots: Vec::new(),
            slot_info: Vec::new(),
            free_slots: SmallVec::new(),
            submit_wakers: VecDeque::new(),
            current_task: None,
        }
    }

    /// Allocate a slot and return its ID (encodes index + generation).
    fn allocate_id(&mut self) -> HostOpId {
        let slot_idx = if let Some(idx) = self.free_slots.pop() {
            // Reusing a slot - increment its generation
            self.slot_info[idx].generation = self.slot_info[idx].generation.wrapping_add(1);
            idx
        } else {
            let idx = self.slots.len();
            self.slots.push(None);
            self.slot_info.push(SlotInfo::default());
            idx
        };

        // Encode: generation in high 32 bits, index in low 32 bits
        let generation = self.slot_info[slot_idx].generation;
        #[allow(clippy::cast_lossless)]
        let id = (u64::from(generation) << 32) | (slot_idx as u64);
        HostOpId(id)
    }

    /// Extract slot index from ID.
    fn id_to_index(id: HostOpId) -> usize {
        (id.0 & 0xFFFF_FFFF) as usize
    }

    /// Extract generation from ID.
    fn id_to_generation(id: HostOpId) -> u32 {
        (id.0 >> 32) as u32
    }

    /// Check if ID matches current slot generation.
    fn is_valid_id(&self, id: HostOpId) -> bool {
        let idx = Self::id_to_index(id);
        let generation = Self::id_to_generation(id);
        idx < self.slot_info.len() && self.slot_info[idx].generation == generation
    }

    /// Free a slot for reuse (generation stays, will increment on next allocate).
    fn free_slot(&mut self, id: HostOpId) {
        let idx = Self::id_to_index(id);
        if idx < self.slots.len() && self.is_valid_id(id) {
            self.slots[idx] = None;
            self.free_slots.push(idx);
        }
    }
}

/// Clock types for time source queries.
///
/// These correspond to POSIX clock types but are abstracted for portability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ClockType {
    /// Wall clock time (`CLOCK_REALTIME`).
    /// Can jump backwards due to NTP adjustments.
    /// Use for timestamps that need to correspond to calendar time.
    Realtime = 0,

    /// Monotonic clock (`CLOCK_MONOTONIC`).
    /// Never goes backwards, suitable for measuring durations.
    /// Use for sleep/wake, timeouts, and performance measurement.
    Monotonic = 1,
}

/// Type alias for injected time source function.
///
/// Takes a runtime ID and clock type, returns nanoseconds.
/// The runtime ID allows per-runtime isolation in WASM environments.
pub type TimeSourceFn = Rc<dyn Fn(u64, ClockType) -> u64>;

/// Type alias for injected random source function.
///
/// Takes a runtime ID and returns a random u64.
/// The runtime ID allows per-runtime isolation in WASM environments.
pub type RandomSourceFn = Rc<dyn Fn(u64) -> u64>;

/// Channel for host operations.
///
/// Clone this to share between tasks and runtime.
#[derive(Clone)]
pub struct HostChannel {
    inner: Rc<RefCell<HostChannelInner>>,
    /// Runtime ID for source function calls.
    ///
    /// Uses `Cell` for interior mutability so we can update the ID after
    /// runtime registration without requiring `&mut self`.
    runtime_id: Cell<u64>,
    /// Time source function.
    time_source: TimeSourceFn,
    /// Random source function.
    random_source: RandomSourceFn,
}

impl HostChannel {
    /// Create a new host channel with bounded queue and injected sources.
    ///
    /// # Arguments
    /// * `runtime_id` - Unique ID for this runtime (passed to source functions)
    /// * `max_pending` - Maximum pending host operations before backpressure
    /// * `time_source` - Time source function
    /// * `random_source` - Random source function
    #[must_use]
    pub fn new(
        runtime_id: u64,
        max_pending: usize,
        time_source: TimeSourceFn,
        random_source: RandomSourceFn,
    ) -> Self {
        Self {
            inner: Rc::new(RefCell::new(HostChannelInner::new(max_pending))),
            runtime_id: Cell::new(runtime_id),
            time_source,
            random_source,
        }
    }

    /// Set the runtime ID.
    ///
    /// Called after runtime registration to update the ID from the placeholder
    /// value (0) to the actual assigned ID. This ensures time/random source
    /// functions receive the correct runtime ID for per-runtime isolation.
    pub fn set_runtime_id(&self, runtime_id: u64) {
        self.runtime_id.set(runtime_id);
    }

    /// Set the currently polling task.
    ///
    /// Called by the scheduler before polling each task. Any host operations
    /// submitted during polling will be attributed to this task.
    ///
    /// Pass `None` to clear the current task (after polling completes).
    pub fn set_current_task(&self, task_id: Option<TaskIdRepr>) {
        self.inner.borrow_mut().current_task = task_id;
    }

    /// Read a mapped file.
    ///
    /// Used for lazy-loading file content from the host.
    #[must_use]
    pub fn file_read(&self, path: impl Into<String>) -> HostOpFuture {
        HostOpFuture {
            channel: self.clone(),
            state: HostOpState::Pending {
                kind: HostOpKind::FileRead { path: path.into() },
            },
        }
    }

    /// Read a range of bytes from a mapped file.
    ///
    /// More efficient than `file_read` for large files when only a portion
    /// is needed (e.g., lazy block loading).
    #[must_use]
    pub fn file_read_range(
        &self,
        path: impl Into<String>,
        offset: u64,
        length: u64,
    ) -> HostOpFuture {
        HostOpFuture {
            channel: self.clone(),
            state: HostOpState::Pending {
                kind: HostOpKind::FileReadRange {
                    path: path.into(),
                    offset,
                    length,
                },
            },
        }
    }

    /// Submit a custom operation.
    #[must_use]
    pub fn custom(&self, name: impl Into<String>, data: Vec<u8>) -> HostOpFuture {
        HostOpFuture {
            channel: self.clone(),
            state: HostOpState::Pending {
                kind: HostOpKind::Custom {
                    name: name.into(),
                    data,
                },
            },
        }
    }

    /// Get the current time in nanoseconds for the specified clock.
    ///
    /// This is a synchronous call using the injected time source.
    #[must_use]
    pub fn now(&self, clock: ClockType) -> u64 {
        (self.time_source)(self.runtime_id.get(), clock)
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
        (self.random_source)(self.runtime_id.get())
    }

    /// Get the runtime ID.
    #[must_use]
    pub fn runtime_id(&self) -> u64 {
        self.runtime_id.get()
    }

    /// Request wakeup at or after the specified deadline.
    ///
    /// The host should complete this operation when the current time
    /// reaches or exceeds the deadline. The response must include the
    /// current time as 8 bytes (little-endian u64).
    #[must_use]
    pub fn wake_at(&self, deadline: u64) -> HostOpFuture {
        HostOpFuture {
            channel: self.clone(),
            state: HostOpState::Pending {
                kind: HostOpKind::WakeAt { deadline },
            },
        }
    }

    /// Print data to a stream (1 = stdout, 2 = stderr).
    ///
    /// Returns when the host has accepted the data.
    /// The runtime/command association is determined automatically via `task_id`.
    #[must_use]
    pub fn print(&self, stream: u8, data: Vec<u8>) -> HostOpFuture {
        HostOpFuture {
            channel: self.clone(),
            state: HostOpState::Pending {
                kind: HostOpKind::Print { stream, data },
            },
        }
    }

    /// Notify that a command has exited.
    ///
    /// Returns when the host has acknowledged the exit.
    /// The runtime/command association is determined automatically via `task_id`.
    #[must_use]
    pub fn command_exit(&self, code: i32) -> HostOpFuture {
        HostOpFuture {
            channel: self.clone(),
            state: HostOpState::Pending {
                kind: HostOpKind::CommandExit { code },
            },
        }
    }

    /// Read from stdin.
    ///
    /// Returns up to `max_bytes` from stdin. Returns empty vec on EOF.
    #[must_use]
    pub fn read_stdin(&self, max_bytes: usize) -> HostOpFuture {
        HostOpFuture {
            channel: self.clone(),
            state: HostOpState::Pending {
                kind: HostOpKind::ReadStdin { max_bytes },
            },
        }
    }

    /// Take the next pending request (for runtime to process).
    ///
    /// Skips cancelled requests (from dropped futures).
    pub fn take_pending(&self) -> Option<HostOpRequest> {
        let mut inner = self.inner.borrow_mut();

        loop {
            let req = inner.pending_requests.pop_front()?;

            // Wake ONE blocked submitter per slot freed (FIFO order).
            // This avoids thundering herd where all waiters wake for one slot.
            if let Some(waker) = inner.submit_wakers.pop_front() {
                waker.wake();
            }

            // Skip cancelled requests - their slot was marked cancelled when future was dropped
            let idx = req.id.index();
            if let Some(Some(slot)) = inner.slots.get(idx)
                && slot.cancelled
            {
                // Clean up the slot and continue to next request
                inner.free_slot(req.id);
                continue;
            }

            return Some(req);
        }
    }

    /// Check if there are pending non-cancelled requests.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        let inner = self.inner.borrow();
        inner.pending_requests.iter().any(|req| {
            inner
                .slots
                .get(req.id.index())
                .and_then(|s| s.as_ref())
                .is_some_and(|slot| !slot.cancelled)
        })
    }

    /// Get the number of pending non-cancelled requests.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        let inner = self.inner.borrow();
        inner
            .pending_requests
            .iter()
            .filter(|req| {
                inner
                    .slots
                    .get(req.id.index())
                    .and_then(|s| s.as_ref())
                    .is_some_and(|slot| !slot.cancelled)
            })
            .count()
    }

    /// Complete a host operation with success.
    ///
    /// This is called by the runtime when an operation finishes.
    pub fn complete(&self, id: HostOpId, data: Vec<u8>) {
        self.complete_result(id, Ok(data));
    }

    /// Complete a host operation with an error.
    pub fn complete_err(&self, id: HostOpId, error: io::Error) {
        self.complete_result(id, Err(error));
    }

    /// Complete a host operation with a result.
    ///
    /// If the ID's generation doesn't match the slot's current generation,
    /// the completion is silently ignored (the slot was reused for a new operation).
    pub fn complete_result(&self, id: HostOpId, result: io::Result<Vec<u8>>) {
        let mut inner = self.inner.borrow_mut();

        // Validate generation to detect stale completions
        if !inner.is_valid_id(id) {
            // Stale ID - slot has been reused, ignore this completion
            return;
        }

        let idx = id.index();

        if let Some(Some(slot)) = inner.slots.get_mut(idx) {
            if slot.cancelled {
                // Future was dropped, discard result
                inner.free_slot(id);
                return;
            }

            slot.result = Some(result);
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
        }
    }

    /// Append a chunk to a pending operation's buffer.
    ///
    /// This is used for operations that return results in multiple chunks,
    /// such as large tool results. Chunks are accumulated in the slot's buffer
    /// until `eof` is true, at which point the operation is completed with
    /// the accumulated data.
    ///
    /// # Arguments
    ///
    /// * `id` - The operation ID (must match a pending operation)
    /// * `data` - The chunk data to append
    /// * `eof` - True if this is the final chunk
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - Chunk appended, operation completed (eof was true)
    /// * `Ok(false)` - Chunk appended, more chunks expected
    /// * `Err(...)` - Error (buffer overflow, stale ID, or cancelled)
    ///
    /// # Buffer Overflow
    ///
    /// If appending the chunk would exceed the buffer limit, the operation
    /// is completed with an error and `Err` is returned. This protects against
    /// unbounded memory growth from malicious or buggy hosts.
    pub fn append_chunk(&self, id: HostOpId, data: Vec<u8>, eof: bool) -> io::Result<bool> {
        let mut inner = self.inner.borrow_mut();

        // Validate generation to detect stale completions
        if !inner.is_valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stale operation ID",
            ));
        }

        let idx = id.index();

        let Some(Some(slot)) = inner.slots.get_mut(idx) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "operation slot not found",
            ));
        };

        if slot.cancelled {
            // Future was dropped, discard chunk and free slot
            inner.free_slot(id);
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "operation was cancelled",
            ));
        }

        // Check buffer limit before appending
        if slot.buffer.len() + data.len() > slot.buffer_limit {
            // Buffer overflow - complete with error
            slot.result = Some(Err(io::Error::other(format!(
                "chunked result exceeded maximum size ({} bytes)",
                slot.buffer_limit
            ))));
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
            return Err(io::Error::other("buffer overflow"));
        }

        // Append chunk data
        slot.buffer.extend(data);

        if eof {
            // Final chunk - complete the operation with accumulated buffer
            let accumulated = std::mem::take(&mut slot.buffer);
            slot.result = Some(Ok(accumulated));
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
            Ok(true)
        } else {
            // More chunks expected
            Ok(false)
        }
    }

    /// Clear the buffer for a pending operation.
    ///
    /// This is used during cancellation to free memory held by partial chunks.
    pub fn clear_buffer(&self, id: HostOpId) {
        let mut inner = self.inner.borrow_mut();

        if !inner.is_valid_id(id) {
            return;
        }

        let idx = id.index();
        if let Some(Some(slot)) = inner.slots.get_mut(idx) {
            slot.buffer.clear();
            slot.buffer.shrink_to_fit();
        }
    }

    /// Check if there's room to submit an operation.
    fn can_submit(&self) -> bool {
        let inner = self.inner.borrow();
        inner.pending_requests.len() < inner.max_pending
    }

    /// Submit an operation to the queue.
    ///
    /// # Panics
    ///
    /// Panics if the queue is full. Callers should check `can_submit()` first.
    fn submit(&self, kind: HostOpKind) -> HostOpId {
        let mut inner = self.inner.borrow_mut();

        assert!(
            inner.pending_requests.len() < inner.max_pending,
            "submit called on full queue - this is a bug, caller should check can_submit() first"
        );

        let id = inner.allocate_id();
        let idx = id.index();
        let task_id = inner.current_task;
        inner
            .pending_requests
            .push_back(HostOpRequest { id, kind, task_id });
        // Slot was allocated by allocate_id(), now fill it
        inner.slots[idx] = Some(PendingSlot {
            result: None,
            buffer: Vec::new(),
            buffer_limit: DEFAULT_CHUNK_BUFFER_LIMIT,
            waker: None,
            cancelled: false,
        });

        id
    }

    /// Register waker for when queue has space (FIFO queue).
    ///
    /// Wakers are woken one at a time as slots free, in registration order.
    fn register_submit_waker(&self, waker: Waker) {
        let mut inner = self.inner.borrow_mut();
        // Avoid duplicate wakers from the same task being re-polled
        if !inner.submit_wakers.iter().any(|w| w.will_wake(&waker)) {
            inner.submit_wakers.push_back(waker);
        }
    }

    /// Poll for a specific operation's result.
    fn poll_result(&self, cx: &mut Context<'_>, id: HostOpId) -> Poll<io::Result<Vec<u8>>> {
        let mut inner = self.inner.borrow_mut();

        // Validate generation
        if !inner.is_valid_id(id) {
            return Poll::Ready(Err(io::Error::other("operation ID is stale (slot reused)")));
        }

        let idx = id.index();

        if let Some(Some(slot)) = inner.slots.get_mut(idx) {
            if let Some(result) = slot.result.take() {
                inner.free_slot(id);
                return Poll::Ready(result);
            }

            // Only clone waker if it changed (optimization)
            if slot.waker.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                slot.waker = Some(cx.waker().clone());
            }
            return Poll::Pending;
        }

        // Slot not found - should not happen if used correctly
        Poll::Ready(Err(io::Error::other("operation slot not found")))
    }

    /// Cancel an operation.
    fn cancel(&self, id: HostOpId) {
        let mut inner = self.inner.borrow_mut();

        // Validate generation
        if !inner.is_valid_id(id) {
            return;
        }

        let idx = id.index();

        if let Some(Some(slot)) = inner.slots.get_mut(idx) {
            if slot.result.is_some() {
                // Already completed, just remove
                inner.free_slot(id);
            } else {
                // Mark as cancelled so result is discarded
                slot.cancelled = true;
            }
        }
    }
}

/// Future for a host operation.
///
/// Await this to get the result. Dropping cancels the operation.
///
/// # Cancellation
///
/// Dropping a `HostOpFuture` cancels the operation, but the runtime may have
/// already started executing it. For operations with side effects (like file
/// writes), the operation may complete even after cancellation.
///
/// See the `host_channel` module-level documentation for detailed
/// cancellation semantics.
///
/// # Example
///
/// ```rust,ignore
/// // Start an operation
/// let future = channel.file_read("/data.txt");
///
/// // Cancel it by dropping
/// drop(future);  // No await, so operation is cancelled
/// ```
pub struct HostOpFuture {
    channel: HostChannel,
    state: HostOpState,
}

impl HostOpFuture {
    /// Get the operation kind (only available before first poll).
    #[must_use]
    pub fn kind(&self) -> Option<&HostOpKind> {
        match &self.state {
            HostOpState::Pending { kind } => Some(kind),
            _ => None,
        }
    }
}

impl Future for HostOpFuture {
    type Output = io::Result<Vec<u8>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match &mut self.state {
                HostOpState::Pending { .. } => {
                    // Check if queue has room before taking ownership (avoids clone)
                    if !self.channel.can_submit() {
                        self.channel.register_submit_waker(cx.waker().clone());
                        return Poll::Pending;
                    }

                    // Take the kind out of state and submit
                    let HostOpState::Pending { kind } =
                        std::mem::replace(&mut self.state, HostOpState::Done)
                    else {
                        unreachable!()
                    };

                    let id = self.channel.submit(kind);
                    self.state = HostOpState::Submitted { id };
                    // Continue to poll for result
                }
                HostOpState::Submitted { id } => {
                    let id = *id;
                    return self.channel.poll_result(cx, id);
                }
                HostOpState::Done => {
                    return Poll::Ready(Err(io::Error::other("future polled after completion")));
                }
            }
        }
    }
}

impl Drop for HostOpFuture {
    fn drop(&mut self) {
        if let HostOpState::Submitted { id } = self.state {
            self.channel.cancel(id);
        }
    }
}

/// Wait for the first of multiple operations to complete.
///
/// Returns the index of the completed operation and its result.
/// Other operations are cancelled.
pub fn select_first(futures: Vec<HostOpFuture>) -> SelectFirstFuture {
    SelectFirstFuture {
        futures: futures.into_iter().map(Some).collect(),
    }
}

/// Future for selecting first completed operation.
pub struct SelectFirstFuture {
    futures: Vec<Option<HostOpFuture>>,
}

impl Future for SelectFirstFuture {
    type Output = (usize, io::Result<Vec<u8>>);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        // Check each future for completion
        for (idx, fut_opt) in this.futures.iter_mut().enumerate() {
            if let Some(fut) = fut_opt {
                // Poll the inner future
                let poll_result = Pin::new(fut).poll(cx);

                match poll_result {
                    Poll::Ready(result) => {
                        // Take this future
                        let _ = fut_opt.take();

                        // Cancel all others
                        for other in &mut this.futures {
                            if let Some(f) = other.take() {
                                drop(f); // Drop cancels
                            }
                        }

                        return Poll::Ready((idx, result));
                    }
                    Poll::Pending => {}
                }
            }
        }

        // Check if all futures are gone (all cancelled externally)
        if this.futures.iter().all(Option::is_none) {
            return Poll::Ready((0, Err(io::Error::other("all operations cancelled"))));
        }

        Poll::Pending
    }
}

impl Drop for SelectFirstFuture {
    fn drop(&mut self) {
        // Cancel any remaining futures
        for fut_opt in &mut self.futures {
            if let Some(f) = fut_opt.take() {
                drop(f);
            }
        }
    }
}

/// Wait for all operations to complete.
///
/// Returns results in the same order as futures were provided.
pub fn join_all(futures: Vec<HostOpFuture>) -> JoinAllFuture {
    let len = futures.len();
    let mut results = Vec::with_capacity(len);
    for _ in 0..len {
        results.push(None);
    }
    JoinAllFuture {
        futures: futures.into_iter().map(Some).collect(),
        results,
    }
}

/// Future for joining all operations.
pub struct JoinAllFuture {
    futures: Vec<Option<HostOpFuture>>,
    results: Vec<Option<io::Result<Vec<u8>>>>,
}

impl Future for JoinAllFuture {
    type Output = Vec<io::Result<Vec<u8>>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let mut all_done = true;

        for (idx, fut_opt) in this.futures.iter_mut().enumerate() {
            if this.results[idx].is_some() {
                continue;
            }

            if let Some(fut) = fut_opt {
                match Pin::new(fut).poll(cx) {
                    Poll::Ready(result) => {
                        this.results[idx] = Some(result);
                        let _ = fut_opt.take();
                    }
                    Poll::Pending => {
                        all_done = false;
                    }
                }
            } else if this.results[idx].is_none() {
                // Future was taken but result not set - cancelled
                this.results[idx] = Some(Err(io::Error::other("operation cancelled")));
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
    use std::cell::Cell;

    use super::*;
    use crate::Exit;
    use crate::executor::Executor;

    /// Create a host channel with mock sources for testing.
    fn test_channel(size: usize) -> HostChannel {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        HostChannel::new(1, size, time_source, random_source)
    }

    #[test]
    fn simple_file_read() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.file_read("/test.txt").await?;
            *result_data_clone.borrow_mut() = data;
            Ok(Exit::success())
        });

        // Run until blocked
        let state = exec.run();
        assert!(matches!(state, crate::RunState::Blocked));

        // Complete the operation
        let req = channel.take_pending().unwrap();
        assert!(matches!(req.kind, HostOpKind::FileRead { .. }));
        channel.complete(req.id, b"hello world".to_vec());

        // Run to completion
        let state = exec.run();
        assert!(matches!(state, crate::RunState::Done(_)));
        assert_eq!(&*result_data.borrow(), b"hello world");
    }

    #[test]
    fn file_read_error() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let got_error = Rc::new(RefCell::new(false));
        let got_error_clone = got_error.clone();

        exec.spawn(async move {
            let result = channel_clone.file_read("/nonexistent.txt").await;
            *got_error_clone.borrow_mut() = result.is_err();
            Ok(Exit::success())
        });

        let _ = exec.run();

        let req = channel.take_pending().unwrap();
        channel.complete_err(req.id, io::Error::new(io::ErrorKind::NotFound, "not found"));

        let _ = exec.run();

        assert!(*got_error.borrow());
    }

    #[test]
    fn bounded_queue_backpressure() {
        let exec = Executor::new();
        let channel = test_channel(2); // Very small queue
        let channel_clone = channel.clone();

        let completed = Rc::new(RefCell::new(0usize));
        let completed_clone = completed.clone();

        exec.spawn(async move {
            // Start 3 operations concurrently on a queue of size 2
            // Third should block on submit until space is available
            let futures = vec![
                channel_clone.file_read("/file0.txt"),
                channel_clone.file_read("/file1.txt"),
                channel_clone.file_read("/file2.txt"),
            ];

            for r in join_all(futures).await {
                if r.is_ok() {
                    *completed_clone.borrow_mut() += 1;
                }
            }
            Ok(Exit::success())
        });

        // Run - first two ops submitted, third blocked waiting for queue space
        let state = exec.run();
        assert!(matches!(state, crate::RunState::Blocked));

        // Only 2 are in the queue (third is waiting to submit)
        assert_eq!(channel.pending_count(), 2);

        // Complete first one - this makes room for third
        let req = channel.take_pending().unwrap();
        channel.complete(req.id, vec![]);

        // Run - third can now submit
        let _ = exec.run();

        // Now all 3 should be submitted (2 in queue + 1 just completed)
        assert_eq!(channel.pending_count(), 2);

        // Complete remaining two
        let req = channel.take_pending().unwrap();
        channel.complete(req.id, vec![]);
        let _ = exec.run();

        let req = channel.take_pending().unwrap();
        channel.complete(req.id, vec![]);
        let _ = exec.run();

        // All 3 should have completed
        assert_eq!(*completed.borrow(), 3);
    }

    #[test]
    fn select_first_returns_winner() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_idx = Rc::new(RefCell::new(None));
        let result_idx_clone = result_idx.clone();
        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let op1 = channel_clone.file_read("/slow.txt");
            let op2 = channel_clone.file_read("/fast.txt");
            let op3 = channel_clone.file_read("/medium.txt");

            let (idx, result) = select_first(vec![op1, op2, op3]).await;
            *result_idx_clone.borrow_mut() = Some(idx);
            if let Ok(data) = result {
                *result_data_clone.borrow_mut() = data;
            }

            Ok(Exit::success())
        });

        let _ = exec.run();

        // Take all 3 requests
        let req1 = channel.take_pending().unwrap();
        let req2 = channel.take_pending().unwrap();
        let req3 = channel.take_pending().unwrap();

        // Complete the second one first (the "fast" one)
        channel.complete(req2.id, b"fast".to_vec());

        let _ = exec.run();

        // Should have gotten index 1 (the second future)
        assert_eq!(*result_idx.borrow(), Some(1));
        assert_eq!(&*result_data.borrow(), b"fast");

        // Other operations were cancelled - completing them is a no-op
        channel.complete(req1.id, b"slow".to_vec());
        channel.complete(req3.id, b"medium".to_vec());
    }

    #[test]
    fn join_all_waits_for_all() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let results_data = Rc::new(RefCell::new(Vec::new()));
        let results_data_clone = results_data.clone();

        exec.spawn(async move {
            let op1 = channel_clone.file_read("/a.txt");
            let op2 = channel_clone.file_read("/b.txt");
            let op3 = channel_clone.file_read("/c.txt");

            let results = join_all(vec![op1, op2, op3]).await;
            for data in results.into_iter().flatten() {
                results_data_clone.borrow_mut().push(data);
            }

            Ok(Exit::success())
        });

        let _ = exec.run();

        // Complete in reverse order
        let req1 = channel.take_pending().unwrap();
        let req2 = channel.take_pending().unwrap();
        let req3 = channel.take_pending().unwrap();

        channel.complete(req3.id, b"c".to_vec());
        let _ = exec.run(); // Still blocked

        channel.complete(req1.id, b"a".to_vec());
        let _ = exec.run(); // Still blocked

        channel.complete(req2.id, b"b".to_vec());
        let _ = exec.run(); // Now done

        let results = results_data.borrow();
        assert_eq!(results.len(), 3);
        // Results are in order of futures, not completion order
        assert_eq!(results[0], b"a");
        assert_eq!(results[1], b"b");
        assert_eq!(results[2], b"c");
    }

    #[test]
    fn drop_cancels_operation() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        exec.spawn(async move {
            let op = channel_clone.file_read("/test.txt");
            drop(op); // Cancel before awaiting
            Ok(Exit::success())
        });

        let _ = exec.run();

        // The operation was submitted but then cancelled
        // Take the request
        if let Some(req) = channel.take_pending() {
            // Completing should be a no-op (slot was cancelled)
            channel.complete(req.id, b"data".to_vec());
        }
    }

    #[test]
    fn sequential_operations() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            // First operation
            let data1 = channel_clone.file_read("/first.txt").await?;
            result_data_clone.borrow_mut().extend(data1);

            // Second operation (after first completes)
            let data2 = channel_clone.file_read("/second.txt").await?;
            result_data_clone.borrow_mut().extend(data2);

            Ok(Exit::success())
        });

        // First op
        let _ = exec.run();
        let req1 = channel.take_pending().unwrap();
        channel.complete(req1.id, b"hello ".to_vec());

        // Second op
        let _ = exec.run();
        let req2 = channel.take_pending().unwrap();
        channel.complete(req2.id, b"world".to_vec());

        let _ = exec.run();

        assert_eq!(&*result_data.borrow(), b"hello world");
    }

    #[test]
    fn print_stdout_operation() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        exec.spawn(async move {
            channel_clone.print(1, b"hello world".to_vec()).await?;
            Ok(Exit::success())
        });

        let _ = exec.run();

        let req = channel.take_pending().unwrap();
        match &req.kind {
            HostOpKind::Print { stream, data } => {
                assert_eq!(*stream, 1); // stdout
                assert_eq!(data, b"hello world");
            }
            _ => panic!("expected Print"),
        }
        channel.complete(req.id, vec![]);

        let _ = exec.run();
    }

    #[test]
    fn print_stderr_operation() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        exec.spawn(async move {
            channel_clone.print(2, b"error!".to_vec()).await?;
            Ok(Exit::success())
        });

        let _ = exec.run();

        let req = channel.take_pending().unwrap();
        match &req.kind {
            HostOpKind::Print { stream, data } => {
                assert_eq!(*stream, 2); // stderr
                assert_eq!(data, b"error!");
            }
            _ => panic!("expected Print"),
        }
        channel.complete(req.id, vec![]);

        let _ = exec.run();
    }

    #[test]
    fn read_stdin_operation() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.read_stdin(1024).await?;
            *result_data_clone.borrow_mut() = data;
            Ok(Exit::success())
        });

        let _ = exec.run();

        let req = channel.take_pending().unwrap();
        match &req.kind {
            HostOpKind::ReadStdin { max_bytes } => {
                assert_eq!(*max_bytes, 1024);
            }
            _ => panic!("expected ReadStdin"),
        }
        // Simulate user input
        channel.complete(req.id, b"user input".to_vec());

        let _ = exec.run();

        assert_eq!(&*result_data.borrow(), b"user input");
    }

    #[test]
    fn read_stdin_eof() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(None));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.read_stdin(1024).await?;
            *result_data_clone.borrow_mut() = Some(data);
            Ok(Exit::success())
        });

        let _ = exec.run();

        let req = channel.take_pending().unwrap();
        // Simulate EOF
        channel.complete(req.id, vec![]);

        let _ = exec.run();

        assert_eq!(result_data.borrow().as_ref().unwrap(), &Vec::<u8>::new());
    }

    #[test]
    fn print_multiple_chunks() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        // Simulate fixed-size buffer that requires multiple flushes
        exec.spawn(async move {
            // First chunk
            channel_clone.print(1, b"chunk1".to_vec()).await?;
            // Second chunk
            channel_clone.print(1, b"chunk2".to_vec()).await?;
            // Third chunk
            channel_clone.print(1, b"chunk3".to_vec()).await?;
            Ok(Exit::success())
        });

        let _ = exec.run();

        // Process first chunk
        let req = channel.take_pending().unwrap();
        assert!(
            matches!(&req.kind, HostOpKind::Print { stream: 1, data, .. } if data == b"chunk1")
        );
        channel.complete(req.id, vec![]);

        let _ = exec.run();

        // Process second chunk
        let req = channel.take_pending().unwrap();
        assert!(
            matches!(&req.kind, HostOpKind::Print { stream: 1, data, .. } if data == b"chunk2")
        );
        channel.complete(req.id, vec![]);

        let _ = exec.run();

        // Process third chunk
        let req = channel.take_pending().unwrap();
        assert!(
            matches!(&req.kind, HostOpKind::Print { stream: 1, data, .. } if data == b"chunk3")
        );
        channel.complete(req.id, vec![]);

        let _ = exec.run();
    }

    #[test]
    fn interleaved_stdout_stderr() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        exec.spawn(async move {
            // Interleave stdout and stderr
            channel_clone.print(1, b"out1".to_vec()).await?;
            channel_clone.print(2, b"err1".to_vec()).await?;
            channel_clone.print(1, b"out2".to_vec()).await?;
            Ok(Exit::success())
        });

        let _ = exec.run();

        // stdout out1
        let req = channel.take_pending().unwrap();
        assert!(matches!(&req.kind, HostOpKind::Print { stream: 1, .. }));
        channel.complete(req.id, vec![]);
        let _ = exec.run();

        // stderr err1
        let req = channel.take_pending().unwrap();
        assert!(matches!(&req.kind, HostOpKind::Print { stream: 2, .. }));
        channel.complete(req.id, vec![]);
        let _ = exec.run();

        // stdout out2
        let req = channel.take_pending().unwrap();
        assert!(matches!(&req.kind, HostOpKind::Print { stream: 1, .. }));
        channel.complete(req.id, vec![]);
        let _ = exec.run();
    }

    #[test]
    #[allow(clippy::similar_names)] // task_a/task_b intentionally similar
    fn multiple_tasks_blocked_on_full_queue_fifo_wake() {
        // Regression test: when multiple tasks are blocked waiting for queue space,
        // they must be woken in FIFO order (one at a time as slots free).
        // Previously, only a single submit_waker was stored, causing earlier
        // waiters to deadlock forever.
        let exec = Executor::new();
        let channel = test_channel(1); // Queue size 1

        let task_a_done = Rc::new(Cell::new(false));
        let task_b_done = Rc::new(Cell::new(false));

        let channel_a = channel.clone();
        let done_flag_a = task_a_done.clone();
        exec.spawn(async move {
            channel_a.file_read("/task_a").await?;
            done_flag_a.set(true);
            Ok(Exit::success())
        });

        let channel_b = channel.clone();
        let done_flag_b = task_b_done.clone();
        exec.spawn(async move {
            channel_b.file_read("/task_b").await?;
            done_flag_b.set(true);
            Ok(Exit::success())
        });

        // Run - one task submits, other blocks on full queue
        let _ = exec.run();

        // Take and complete first request - this wakes ONE blocked task (FIFO)
        let req1 = channel.take_pending().unwrap();
        channel.complete(req1.id, b"data1".to_vec());

        // Run - the woken task submits
        let _ = exec.run();

        // Take and complete second request
        let req2 = channel.take_pending().unwrap();
        channel.complete(req2.id, b"data2".to_vec());

        // Both tasks should complete
        let _ = exec.run();

        assert!(
            task_a_done.get(),
            "Task A should complete (was blocked on queue)"
        );
        assert!(
            task_b_done.get(),
            "Task B should complete (was blocked on queue)"
        );
    }

    #[test]
    fn first_allocated_id_is_not_zero() {
        // Regression test: First allocated ID must not be 0 to avoid collision
        // with NOTIFICATION_ID (0) used by amla-sandbox for one-way notifications.
        // Previously, SlotInfo::default() had generation=0, so the first ID was
        // (0 << 32) | 0 = 0, which would collide with NOTIFICATION_ID.
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let exec = Executor::new();
        exec.spawn(async move {
            let _ = channel_clone.file_read("/test.txt").await;
            Ok(Exit::success())
        });

        // Run until blocked on host op
        let _ = exec.run();

        // First allocated ID should NOT be 0
        let req = channel.take_pending().unwrap();
        assert_ne!(
            req.id.0, 0,
            "First host op ID must not be 0 (NOTIFICATION_ID)"
        );
        // Should be (1 << 32) = 4294967296 (generation 1, slot 0)
        assert_eq!(
            req.id.0,
            1u64 << 32,
            "First ID should be generation=1, slot=0"
        );
    }

    // ========================================================================
    // Chunked Tool Result Tests (append_chunk)
    // ========================================================================

    #[test]
    fn append_chunk_single_chunk_with_eof() {
        // Single chunk with eof=true should complete immediately
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.file_read("/test.txt").await?;
            *result_data_clone.borrow_mut() = data;
            Ok(Exit::success())
        });

        let _ = exec.run();

        let req = channel.take_pending().unwrap();
        assert!(matches!(&req.kind, HostOpKind::FileRead { .. }));

        // Single chunk with eof=true (using append_chunk instead of complete)
        let completed = channel
            .append_chunk(req.id, b"small result".to_vec(), true)
            .unwrap();
        assert!(completed, "append_chunk with eof=true should return true");

        let _ = exec.run();
        assert_eq!(&*result_data.borrow(), b"small result");
    }

    #[test]
    fn append_chunk_multiple_chunks() {
        // Multiple chunks should accumulate until eof
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.file_read("/test.txt").await?;
            *result_data_clone.borrow_mut() = data;
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // First chunk - not complete
        let completed = channel
            .append_chunk(req.id, b"chunk1-".to_vec(), false)
            .unwrap();
        assert!(!completed, "append_chunk without eof should return false");

        // Second chunk - not complete
        let completed = channel
            .append_chunk(req.id, b"chunk2-".to_vec(), false)
            .unwrap();
        assert!(!completed);

        // Third chunk - final
        let completed = channel
            .append_chunk(req.id, b"chunk3".to_vec(), true)
            .unwrap();
        assert!(completed, "append_chunk with eof=true should return true");

        let _ = exec.run();
        assert_eq!(&*result_data.borrow(), b"chunk1-chunk2-chunk3");
    }

    #[test]
    fn append_chunk_empty_final_chunk() {
        // Empty chunk with eof=true should complete with accumulated data
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.file_read("/test.txt").await?;
            *result_data_clone.borrow_mut() = data;
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // Data chunk
        channel
            .append_chunk(req.id, b"all data here".to_vec(), false)
            .unwrap();

        // Empty final chunk
        let completed = channel.append_chunk(req.id, vec![], true).unwrap();
        assert!(completed);

        let _ = exec.run();
        assert_eq!(&*result_data.borrow(), b"all data here");
    }

    #[test]
    fn append_chunk_empty_result() {
        // Single empty chunk with eof=true (empty tool result)
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(None));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.file_read("/test.txt").await?;
            *result_data_clone.borrow_mut() = Some(data);
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // Empty final chunk
        let completed = channel.append_chunk(req.id, vec![], true).unwrap();
        assert!(completed);

        let _ = exec.run();
        assert_eq!(result_data.borrow().as_ref().unwrap(), &Vec::<u8>::new());
    }

    #[test]
    fn append_chunk_stale_id_rejected() {
        // Using an old/stale ID should fail
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        exec.spawn(async move {
            let _ = channel_clone.file_read("/test.txt").await;
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // Complete normally first - this frees the slot
        channel.complete(req.id, b"result".to_vec());
        let _ = exec.run();

        // Now try to append_chunk with the old ID - should fail
        // The slot was freed by complete(), so we get NotFound
        let result = channel.append_chunk(req.id, b"late data".to_vec(), true);
        assert!(result.is_err());
        // After complete(), the slot is freed (None), so NotFound is returned
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn append_chunk_cancelled_operation() {
        // Chunks for cancelled operations should be rejected
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        exec.spawn(async move {
            let op = channel_clone.file_read("/test.txt");
            drop(op); // Cancel before completion
            Ok(Exit::success())
        });

        let _ = exec.run();

        if let Some(req) = channel.take_pending() {
            // Try to append chunk to cancelled operation
            let result = channel.append_chunk(req.id, b"data".to_vec(), true);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        }
    }

    #[test]
    fn append_chunk_buffer_overflow_protection() {
        // Buffer overflow should abort the operation
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let got_error = Rc::new(RefCell::new(false));
        let got_error_clone = got_error.clone();

        exec.spawn(async move {
            let result = channel_clone.file_read("/test.txt").await;
            *got_error_clone.borrow_mut() = result.is_err();
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // Try to exceed the buffer limit (DEFAULT_CHUNK_BUFFER_LIMIT = 10MB)
        // We'll simulate by setting a smaller limit in the slot and then exceeding it
        // Since we can't change the limit directly, we'll send chunks that exceed 10MB
        let large_chunk = vec![0u8; 11 * 1024 * 1024]; // 11MB > 10MB limit

        let result = channel.append_chunk(req.id, large_chunk, true);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);

        // The operation should complete with an error
        let _ = exec.run();
        assert!(
            *got_error.borrow(),
            "Operation should complete with error after buffer overflow"
        );
    }

    #[test]
    fn append_chunk_incremental_overflow() {
        // Multiple chunks that together exceed the limit
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let got_error = Rc::new(RefCell::new(false));
        let got_error_clone = got_error.clone();

        exec.spawn(async move {
            let result = channel_clone.file_read("/test.txt").await;
            *got_error_clone.borrow_mut() = result.is_err();
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // Send chunks that together exceed the limit
        let chunk_size = 3 * 1024 * 1024; // 3MB each
        let chunk = vec![0u8; chunk_size];

        // 3MB - ok
        assert!(channel.append_chunk(req.id, chunk.clone(), false).is_ok());
        // 6MB - ok
        assert!(channel.append_chunk(req.id, chunk.clone(), false).is_ok());
        // 9MB - ok
        assert!(channel.append_chunk(req.id, chunk.clone(), false).is_ok());
        // 12MB - exceeds 10MB limit, should fail
        let result = channel.append_chunk(req.id, chunk, false);
        assert!(result.is_err());

        let _ = exec.run();
        assert!(*got_error.borrow());
    }

    #[test]
    fn append_chunk_interleaved_operations() {
        // Multiple concurrent operations with interleaved chunks
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result1 = Rc::new(RefCell::new(Vec::new()));
        let result2 = Rc::new(RefCell::new(Vec::new()));
        let result1_clone = result1.clone();
        let result2_clone = result2.clone();

        exec.spawn(async move {
            // Start two concurrent operations
            let op1 = channel_clone.file_read("/file1.txt");
            let op2 = channel_clone.file_read("/file2.txt");

            let outcomes = join_all(vec![op1, op2]).await;
            if let Ok(data) = &outcomes[0] {
                *result1_clone.borrow_mut() = data.clone();
            }
            if let Ok(data) = &outcomes[1] {
                *result2_clone.borrow_mut() = data.clone();
            }
            Ok(Exit::success())
        });

        let _ = exec.run();

        // Take both requests
        let req1 = channel.take_pending().unwrap();
        let req2 = channel.take_pending().unwrap();

        // Interleave chunks from both operations
        channel
            .append_chunk(req1.id, b"A1-".to_vec(), false)
            .unwrap();
        channel
            .append_chunk(req2.id, b"B1-".to_vec(), false)
            .unwrap();
        channel.append_chunk(req1.id, b"A2".to_vec(), true).unwrap(); // Complete first
        channel.append_chunk(req2.id, b"B2".to_vec(), true).unwrap(); // Complete second

        let _ = exec.run();

        assert_eq!(&*result1.borrow(), b"A1-A2");
        assert_eq!(&*result2.borrow(), b"B1-B2");
    }

    #[test]
    fn append_chunk_vs_complete_coexistence() {
        // append_chunk and complete should both work in the same session
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_chunked = Rc::new(RefCell::new(Vec::new()));
        let result_atomic = Rc::new(RefCell::new(Vec::new()));
        let result_chunked_clone = result_chunked.clone();
        let result_atomic_clone = result_atomic.clone();

        exec.spawn(async move {
            // Two concurrent operations
            let op_chunked = channel_clone.file_read("/chunked.txt");
            let op_atomic = channel_clone.file_read("/atomic.txt");

            let results = join_all(vec![op_chunked, op_atomic]).await;
            if let Ok(data) = &results[0] {
                *result_chunked_clone.borrow_mut() = data.clone();
            }
            if let Ok(data) = &results[1] {
                *result_atomic_clone.borrow_mut() = data.clone();
            }
            Ok(Exit::success())
        });

        let _ = exec.run();

        let req_chunked = channel.take_pending().unwrap();
        let req_atomic = channel.take_pending().unwrap();

        // Complete one with chunks
        channel
            .append_chunk(req_chunked.id, b"ch1-".to_vec(), false)
            .unwrap();
        channel
            .append_chunk(req_chunked.id, b"ch2".to_vec(), true)
            .unwrap();

        // Complete the other atomically
        channel.complete(req_atomic.id, b"atomic result".to_vec());

        let _ = exec.run();

        assert_eq!(&*result_chunked.borrow(), b"ch1-ch2");
        assert_eq!(&*result_atomic.borrow(), b"atomic result");
    }

    #[test]
    fn clear_buffer_frees_partial_data() {
        // clear_buffer should free accumulated partial chunks
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        exec.spawn(async move {
            let _ = channel_clone.file_read("/test.txt").await;
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // Accumulate some data
        channel
            .append_chunk(req.id, b"partial data".to_vec(), false)
            .unwrap();

        // Clear the buffer (simulates cancellation cleanup)
        channel.clear_buffer(req.id);

        // Complete with empty data - should not include the cleared partial data
        channel
            .append_chunk(req.id, b"final".to_vec(), true)
            .unwrap();

        // Note: clear_buffer only clears the buffer, the operation can still be completed
        // The final result should only contain "final", not "partial datafinal"
    }

    #[test]
    fn append_chunk_after_clear_buffer() {
        // After clear_buffer, subsequent chunks should work correctly
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.file_read("/test.txt").await?;
            *result_data_clone.borrow_mut() = data;
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // Accumulate some data then clear
        channel
            .append_chunk(req.id, b"will be cleared".to_vec(), false)
            .unwrap();
        channel.clear_buffer(req.id);

        // New data after clear
        channel
            .append_chunk(req.id, b"fresh start".to_vec(), true)
            .unwrap();

        let _ = exec.run();
        assert_eq!(&*result_data.borrow(), b"fresh start");
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)] // % 256 guarantees value fits in u8
    fn append_chunk_large_binary_data() {
        // Test with larger binary data (but under the limit)
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_clone = channel.clone();

        let result_data = Rc::new(RefCell::new(Vec::new()));
        let result_data_clone = result_data.clone();

        exec.spawn(async move {
            let data = channel_clone.file_read("/binary.bin").await?;
            *result_data_clone.borrow_mut() = data;
            Ok(Exit::success())
        });

        let _ = exec.run();
        let req = channel.take_pending().unwrap();

        // Simulate chunked binary data (e.g., large JSON or file contents)
        let chunk_size = 100_000; // 100KB chunks
        let total_size = 500_000; // 500KB total
        let num_chunks = total_size / chunk_size;

        for i in 0..num_chunks {
            let is_last = i == num_chunks - 1;
            // Fill with pattern that lets us verify integrity
            let chunk: Vec<u8> = (0..chunk_size)
                .map(|j| ((i * chunk_size + j) % 256) as u8)
                .collect();
            channel.append_chunk(req.id, chunk, is_last).unwrap();
        }

        let _ = exec.run();

        // Verify total size
        assert_eq!(result_data.borrow().len(), total_size);

        // Verify data integrity
        let data = result_data.borrow();
        for (i, &byte) in data.iter().enumerate() {
            assert_eq!(byte, (i % 256) as u8, "Data corruption at byte {i}");
        }
    }
}
