//! Timer primitives for the scheduler.
//!
//! This module provides time management with efficient coalescing:
//!
//! - **`TimeNanos`**: Time represented as nanoseconds since an arbitrary epoch.
//! - **`TimerState`**: Internal timer heap that coalesces to single host `WakeAt`.
//! - **`SleepFuture`**: Future that completes at a deadline.
//!
//! ## Usage
//!
//! ```rust,ignore
//! // Get current time (synchronous)
//! let now = scheduler.now();
//!
//! // Sleep until a deadline
//! let deadline = now + Duration::from_secs(1).as_nanos() as u64;
//! scheduler.sleep_until(deadline).await?;
//! ```
//!
//! ## How It Works
//!
//! The timer system uses a two-level design:
//!
//! 1. **Internal heap**: All `sleep_until()` calls register in a min-heap
//! 2. **Single host op**: Only ONE `WakeAt` request to host for earliest deadline
//! 3. **Batch firing**: When `WakeAt` completes, all expired timers fire at once
//!
//! This means 1000 sleeping tasks = 1 host timer, not 1000.
//!
//! ## Time Flow
//!
//! ```text
//! Task A: sleep_until(100) ─┐
//! Task B: sleep_until(50)  ─┼─► Timer Heap ─► WakeAt(50) to Host
//! Task C: sleep_until(200) ─┘         │
//!                                     ▼
//!                           Host completes WakeAt (returns current time)
//!                                     │
//!                                     ▼
//!                           Fire all expired (B), schedule WakeAt(100)
//! ```

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use smallvec::SmallVec;

use crate::host_channel::{HostChannel, HostOpFuture};

/// Time represented as nanoseconds since an arbitrary monotonic epoch.
///
/// The epoch is determined by the host runtime. Common choices:
/// - Process start time (for relative timing)
/// - UNIX epoch (for absolute timestamps)
///
/// Using `u64` allows representing ~584 years from epoch.
pub type TimeNanos = u64;

/// Unique identifier for a timer in the heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

impl TimerId {
    /// Convert to slot index.
    ///
    /// Timer IDs equal slot indices, so this is always valid on 64-bit systems.
    /// On 32-bit systems, this could theoretically truncate if billions of
    /// timers are created without reuse, but that's not a practical concern.
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    fn as_index(self) -> usize {
        self.0 as usize
    }
}

/// Convert Duration to nanoseconds (saturating at `u64::MAX`).
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn duration_to_nanos(d: std::time::Duration) -> TimeNanos {
    d.as_nanos().min(u128::from(TimeNanos::MAX)) as TimeNanos
}

/// Convert nanoseconds to Duration.
#[inline]
pub fn nanos_to_duration(n: TimeNanos) -> std::time::Duration {
    std::time::Duration::from_nanos(n)
}

// ============================================================================
// Timer State (internal heap + single host WakeAt)
// ============================================================================

/// Entry in the timer heap.
#[derive(Debug)]
struct TimerEntry {
    deadline: TimeNanos,
    id: TimerId,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline)
    }
}

/// Slot for timer completion notification.
#[derive(Debug, Default)]
struct TimerSlot {
    /// Whether the timer has fired.
    fired: bool,
    /// Waker to notify when timer fires.
    waker: Option<Waker>,
    /// Whether this slot has been cancelled.
    cancelled: bool,
}

/// State of the pending host `WakeAt` operation.
enum WakeAtState {
    /// No pending `WakeAt`.
    Idle,
    /// `WakeAt` in progress. Result includes current time (8 bytes LE u64).
    Pending {
        deadline: TimeNanos,
        future: HostOpFuture,
    },
}

/// Internal timer state shared across all `SleepFuture`s.
struct TimerStateInner {
    /// Min-heap of pending timers (earliest deadline first).
    heap: BinaryHeap<Reverse<TimerEntry>>,
    /// Timer slots indexed by timer ID.
    slots: Vec<Option<TimerSlot>>,
    /// Free slot indices for reuse (typically few timers cancelled at once).
    free_slots: SmallVec<[usize; 8]>,
    /// Current `WakeAt` state.
    wake_at_state: WakeAtState,
    /// Reference to host channel.
    host_channel: HostChannel,
}

/// Shared timer state.
///
/// This manages an internal timer heap and coalesces all sleep requests
/// into a single host `WakeAt` operation for the earliest deadline.
#[derive(Clone)]
pub struct TimerState {
    inner: Rc<RefCell<TimerStateInner>>,
}

impl TimerState {
    /// Create a new timer state with the given host channel.
    pub fn new(host_channel: HostChannel) -> Self {
        Self {
            inner: Rc::new(RefCell::new(TimerStateInner {
                heap: BinaryHeap::new(),
                slots: Vec::new(),
                free_slots: SmallVec::new(),
                wake_at_state: WakeAtState::Idle,
                host_channel,
            })),
        }
    }

    /// Register a new timer and return its ID.
    fn register(&self, deadline: TimeNanos) -> TimerId {
        let mut inner = self.inner.borrow_mut();

        // Allocate slot - reuse if available
        let slot_idx = if let Some(idx) = inner.free_slots.pop() {
            inner.slots[idx] = Some(TimerSlot::default());
            idx
        } else {
            let idx = inner.slots.len();
            inner.slots.push(Some(TimerSlot::default()));
            idx
        };

        // Timer ID equals slot index for O(1) lookup
        let id = TimerId(slot_idx as u64);

        // Add to heap
        inner.heap.push(Reverse(TimerEntry { deadline, id }));

        // Check if we need to update the host WakeAt
        let need_new_wake_at = match &inner.wake_at_state {
            WakeAtState::Idle => true,
            WakeAtState::Pending {
                deadline: current, ..
            } => deadline < *current,
        };

        if need_new_wake_at {
            // Request new WakeAt for this earlier deadline
            let future = inner.host_channel.wake_at(deadline);
            inner.wake_at_state = WakeAtState::Pending { deadline, future };
        }

        id
    }

    /// Cancel a timer.
    fn cancel(&self, id: TimerId) {
        let mut inner = self.inner.borrow_mut();
        let idx = id.as_index();

        if let Some(Some(slot)) = inner.slots.get_mut(idx) {
            slot.cancelled = true;
            // Note: We don't remove from heap (lazy removal on fire)
        }
    }

    /// Check if a timer has fired.
    fn is_fired(&self, id: TimerId) -> bool {
        let inner = self.inner.borrow();
        let idx = id.as_index();
        inner
            .slots
            .get(idx)
            .and_then(|s| s.as_ref())
            .is_some_and(|slot| slot.fired)
    }

    /// Register waker for a timer.
    fn register_waker(&self, id: TimerId, waker: &Waker) {
        let mut inner = self.inner.borrow_mut();
        let idx = id.as_index();

        if let Some(Some(slot)) = inner.slots.get_mut(idx) {
            slot.waker = Some(waker.clone());
        }
    }

    /// Process timers: poll `WakeAt`, fire expired, schedule next.
    ///
    /// Call this from the scheduler's run loop.
    pub fn process(&self, cx: &mut Context<'_>) {
        // Need to be careful with borrowing - poll may trigger callbacks
        loop {
            let action = {
                let mut inner = self.inner.borrow_mut();

                match &mut inner.wake_at_state {
                    WakeAtState::Idle => {
                        // Check if we have pending timers
                        if let Some(Reverse(entry)) = inner.heap.peek() {
                            let deadline = entry.deadline;
                            let future = inner.host_channel.wake_at(deadline);
                            inner.wake_at_state = WakeAtState::Pending { deadline, future };
                            continue;
                        }
                        break;
                    }

                    WakeAtState::Pending { future, .. } => {
                        match Pin::new(future).poll(cx) {
                            Poll::Ready(Ok(bytes)) => {
                                // WakeAt result includes current time (8 bytes LE u64)
                                let now = if bytes.len() >= 8 {
                                    let arr: [u8; 8] = bytes[..8].try_into().unwrap();
                                    u64::from_le_bytes(arr)
                                } else {
                                    // Fallback: use 0 (will fire nothing)
                                    0
                                };
                                ProcessAction::FireExpired(now)
                            }
                            Poll::Ready(Err(_)) => {
                                // Error - reset and try again
                                inner.wake_at_state = WakeAtState::Idle;
                                continue;
                            }
                            Poll::Pending => break,
                        }
                    }
                }
            };

            // Handle action outside borrow
            match action {
                ProcessAction::FireExpired(now) => {
                    self.fire_expired(now);
                    self.inner.borrow_mut().wake_at_state = WakeAtState::Idle;
                    // Loop to schedule next WakeAt
                }
            }
        }
    }

    /// Fire all timers with deadline <= now.
    fn fire_expired(&self, now: TimeNanos) {
        let mut wakers_to_wake = Vec::new();

        {
            let mut inner = self.inner.borrow_mut();

            while let Some(Reverse(entry)) = inner.heap.peek() {
                if entry.deadline > now {
                    break;
                }

                let Reverse(entry) = inner.heap.pop().unwrap();
                let idx = entry.id.as_index();

                if let Some(Some(slot)) = inner.slots.get_mut(idx) {
                    if slot.cancelled {
                        // Skip cancelled timers
                        inner.slots[idx] = None;
                        inner.free_slots.push(idx);
                        continue;
                    }

                    slot.fired = true;
                    if let Some(waker) = slot.waker.take() {
                        wakers_to_wake.push(waker);
                    }
                }
            }
        }

        // Wake outside borrow
        for waker in wakers_to_wake {
            waker.wake();
        }
    }

    /// Clean up a completed timer slot.
    fn cleanup(&self, id: TimerId) {
        let mut inner = self.inner.borrow_mut();
        let idx = id.as_index();

        if idx < inner.slots.len() {
            inner.slots[idx] = None;
            inner.free_slots.push(idx);
        }
    }

    /// Check if there are pending timers.
    ///
    /// Useful for scheduler diagnostics and debugging.
    #[must_use]
    #[allow(dead_code)]
    pub fn has_pending(&self) -> bool {
        let inner = self.inner.borrow();
        !inner.heap.is_empty()
    }

    /// Get the earliest deadline (if any).
    ///
    /// Useful for scheduler diagnostics and debugging.
    #[must_use]
    #[allow(dead_code)]
    pub fn earliest_deadline(&self) -> Option<TimeNanos> {
        let inner = self.inner.borrow();
        inner.heap.peek().map(|Reverse(e)| e.deadline)
    }
}

enum ProcessAction {
    FireExpired(TimeNanos),
}

// ============================================================================
// SleepFuture (uses timer heap)
// ============================================================================

/// Future that completes when a deadline is reached.
///
/// Created by [`Scheduler::sleep_until()`](crate::Scheduler::sleep_until).
///
/// # Cancellation
///
/// Dropping a `SleepFuture` cancels the timer. If this was the earliest
/// deadline, the next earliest will be scheduled instead.
pub struct SleepFuture {
    timer_state: TimerState,
    id: TimerId,
    deadline: TimeNanos,
    registered: bool,
}

impl SleepFuture {
    /// Create a new `SleepFuture`.
    pub(crate) fn new(timer_state: TimerState, deadline: TimeNanos) -> Self {
        let id = timer_state.register(deadline);
        Self {
            timer_state,
            id,
            deadline,
            registered: true,
        }
    }

    /// Get the deadline this future is waiting for.
    pub fn deadline(&self) -> TimeNanos {
        self.deadline
    }
}

impl Future for SleepFuture {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.timer_state.is_fired(self.id) {
            self.registered = false;
            self.timer_state.cleanup(self.id);
            return Poll::Ready(Ok(()));
        }

        self.timer_state.register_waker(self.id, cx.waker());
        Poll::Pending
    }
}

impl Drop for SleepFuture {
    fn drop(&mut self) {
        if self.registered {
            self.timer_state.cancel(self.id);
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_to_nanos_normal() {
        let d = std::time::Duration::from_millis(100);
        assert_eq!(duration_to_nanos(d), 100_000_000);
    }

    #[test]
    fn duration_to_nanos_saturates() {
        let d = std::time::Duration::from_secs(u64::MAX);
        assert_eq!(duration_to_nanos(d), u64::MAX);
    }

    #[test]
    fn nanos_to_duration_roundtrip() {
        let nanos: TimeNanos = 1_500_000_000;
        let d = nanos_to_duration(nanos);
        assert_eq!(d.as_nanos(), 1_500_000_000);
    }
}
