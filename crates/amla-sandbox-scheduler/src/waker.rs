//! Shared waker implementation for the executor.
//!
//! This module provides the custom waker vtable used by both `Executor` and
//! `Scheduler`. The waker stores a task ID and a reference to the ready queue,
//! allowing tasks to be re-queued when woken.
//!
//! # Safety
//!
//! The waker implementation uses raw pointers and unsafe code. The invariants are:
//!
//! 1. **Data pointer validity**: The data pointer passed to vtable functions was
//!    created by `Box::into_raw` in `create_waker` or `clone_waker`.
//!
//! 2. **Ownership transfer**: `wake` and `drop_waker` consume the data pointer
//!    (take ownership via `Box::from_raw`). After these calls, the pointer is invalid.
//!
//! 3. **Reference validity**: `wake_by_ref` only borrows the data, so the pointer
//!    remains valid after the call.
//!
//! 4. **Ready queue lifetime**: The `Rc<RefCell<ReadyQueue>>` ensures the ready
//!    queue outlives all wakers that reference it.

use std::cell::RefCell;
use std::rc::Rc;
use std::task::{RawWaker, RawWakerVTable, Waker};

use smallvec::SmallVec;

/// Task identifier used by the waker system.
///
/// This is a simple newtype over `usize` representing an index into the task array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WakerTaskId(pub usize);

/// Ready queue abstraction for waker integration.
///
/// This trait allows different executor implementations to share the waker code
/// while maintaining their own ready queue structures.
pub trait ReadyQueue {
    /// Add a task to the ready queue.
    fn enqueue(&self, task_id: WakerTaskId);
}

/// Simple ready queue implementation using `SmallVec`.
///
/// This is the standard implementation used by both `Executor` and `Scheduler`.
#[derive(Default)]
pub struct SmallVecReadyQueue {
    /// The ready task IDs. Inline storage for up to 8 tasks.
    pub ready: SmallVec<[WakerTaskId; 8]>,
}

impl ReadyQueue for RefCell<SmallVecReadyQueue> {
    fn enqueue(&self, task_id: WakerTaskId) {
        self.borrow_mut().ready.push(task_id);
    }
}

/// Data stored in the waker's raw pointer.
///
/// This is boxed and converted to a raw pointer for the waker vtable.
type WakerData<Q> = (WakerTaskId, Rc<Q>);

/// The waker vtable for our executor.
///
/// All functions in this vtable expect the data pointer to be a valid
/// `*const WakerData<Q>` created by `create_waker` or `clone_waker`.
pub const fn vtable<Q: ReadyQueue + 'static>() -> &'static RawWakerVTable {
    &RawWakerVTable::new(
        clone_waker::<Q>,
        wake::<Q>,
        wake_by_ref::<Q>,
        drop_waker::<Q>,
    )
}

/// Create a new waker for a task.
///
/// # Arguments
///
/// * `task_id` - The ID of the task this waker will wake.
/// * `ready_queue` - Reference to the ready queue where the task will be enqueued.
///
/// # Returns
///
/// A `Waker` that, when woken, will enqueue `task_id` into `ready_queue`.
pub fn create_waker<Q: ReadyQueue + 'static>(task_id: WakerTaskId, ready_queue: Rc<Q>) -> Waker {
    let data = Box::into_raw(Box::new((task_id, ready_queue)));

    // SAFETY: We implement the waker vtable correctly below. The data pointer
    // is valid because we just created it from Box::into_raw. Each vtable
    // function handles the pointer according to the documented ownership rules.
    unsafe { Waker::from_raw(RawWaker::new(data.cast::<()>(), vtable::<Q>())) }
}

/// Clone a waker.
///
/// # Safety
///
/// The `data` pointer must have been created by `create_waker` or a previous
/// `clone_waker` call, and must not have been consumed by `wake` or `drop_waker`.
unsafe fn clone_waker<Q: ReadyQueue + 'static>(data: *const ()) -> RawWaker {
    // SAFETY: Caller guarantees data was created from Box::into_raw in create_waker.
    // We only borrow the data to clone it, so the original remains valid.
    let original = unsafe { &*data.cast::<WakerData<Q>>() };
    let cloned = Box::new((original.0, Rc::clone(&original.1)));
    RawWaker::new(Box::into_raw(cloned).cast::<()>(), vtable::<Q>())
}

/// Wake a task and consume the waker.
///
/// This enqueues the task ID into the ready queue and deallocates the waker data.
///
/// # Safety
///
/// The `data` pointer must have been created by `create_waker` or `clone_waker`.
/// After this call, the `data` pointer is invalid and must not be used.
unsafe fn wake<Q: ReadyQueue + 'static>(data: *const ()) {
    // SAFETY: Caller guarantees data was created from Box::into_raw.
    // We take ownership via Box::from_raw, which will deallocate when dropped.
    let boxed = unsafe { Box::from_raw(data.cast::<WakerData<Q>>().cast_mut()) };
    boxed.1.enqueue(boxed.0);
}

/// Wake a task without consuming the waker.
///
/// This enqueues the task ID into the ready queue but leaves the waker valid.
///
/// # Safety
///
/// The `data` pointer must have been created by `create_waker` or `clone_waker`.
/// The pointer remains valid after this call.
unsafe fn wake_by_ref<Q: ReadyQueue + 'static>(data: *const ()) {
    // SAFETY: Caller guarantees data was created from Box::into_raw.
    // We only borrow the data, so it remains valid after this call.
    let waker_data = unsafe { &*data.cast::<WakerData<Q>>() };
    waker_data.1.enqueue(waker_data.0);
}

/// Drop a waker, deallocating its data.
///
/// # Safety
///
/// The `data` pointer must have been created by `create_waker` or `clone_waker`.
/// After this call, the `data` pointer is invalid and must not be used.
unsafe fn drop_waker<Q: ReadyQueue + 'static>(data: *const ()) {
    // SAFETY: Caller guarantees data was created from Box::into_raw.
    // Box::from_raw takes ownership and drop deallocates the memory.
    unsafe {
        drop(Box::from_raw(data.cast::<WakerData<Q>>().cast_mut()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waker_enqueues_task() {
        let queue = Rc::new(RefCell::new(SmallVecReadyQueue::default()));
        let waker = create_waker(WakerTaskId(42), Rc::clone(&queue));

        assert!(queue.borrow().ready.is_empty());

        waker.wake_by_ref();
        assert_eq!(queue.borrow().ready.len(), 1);
        assert_eq!(queue.borrow().ready[0], WakerTaskId(42));

        waker.wake(); // Consumes the waker
        assert_eq!(queue.borrow().ready.len(), 2);
    }

    #[test]
    fn waker_clone_is_independent() {
        let queue = Rc::new(RefCell::new(SmallVecReadyQueue::default()));
        let waker1 = create_waker(WakerTaskId(1), Rc::clone(&queue));
        let waker2 = waker1.clone();

        waker1.wake();
        assert_eq!(queue.borrow().ready.len(), 1);

        waker2.wake();
        assert_eq!(queue.borrow().ready.len(), 2);
    }

    #[test]
    fn waker_drop_does_not_enqueue() {
        let queue = Rc::new(RefCell::new(SmallVecReadyQueue::default()));
        let waker = create_waker(WakerTaskId(99), Rc::clone(&queue));

        drop(waker);
        assert!(queue.borrow().ready.is_empty());
    }
}
