//! Async pipe with bounded buffer and backpressure.
//!
//! This pipe uses a ring buffer and wakers to implement async read/write
//! with proper backpressure - writers block when the buffer is full,
//! readers block when the buffer is empty.
//!
//! ## Watermarks
//!
//! To avoid thrashing (excessive wake/sleep cycles), the pipe uses watermarks:
//!
//! - **High watermark (100%)**: Writers block when buffer is full
//! - **Low watermark (25%)**: Writers are woken only when buffer drops below this level
//!
//! This means a blocked writer won't wake up until significant space is available,
//! reducing context switches when producer is faster than consumer.
//!
//! ## Deadlock Prevention
//!
//! The low watermark alone could cause deadlock if a reader stops before crossing
//! the threshold. To prevent this, writers are also woken on the first read after
//! the buffer was full (regardless of watermark). This guarantees progress:
//!
//! 1. Writer fills buffer → blocks
//! 2. Reader reads any amount → writer woken (`was_full` transition)
//! 3. Writer writes more → may block again
//! 4. Subsequent reads only wake writer at low watermark (anti-thrashing)
//!
//! This balances liveness (no deadlock) with efficiency (minimal thrashing).

use std::cell::RefCell;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Default pipe capacity (64KB).
pub const DEFAULT_CAPACITY: usize = 64 * 1024;

/// Low watermark percentage (0-100). Writers wake when buffer drops below this.
const LOW_WATERMARK_PERCENT: usize = 25;

/// Shared pipe state.
#[allow(clippy::struct_excessive_bools)]
struct PipeInner {
    /// Ring buffer.
    buffer: Box<[u8]>,
    /// Read position (head).
    head: usize,
    /// Write position (tail).
    tail: usize,
    /// Number of bytes in buffer.
    len: usize,
    /// Whether write end is closed.
    closed: bool,
    /// Waker for blocked reader.
    read_waker: Option<Waker>,
    /// Waker for blocked writer.
    write_waker: Option<Waker>,
    /// Whether writer is blocked (used for low watermark logic).
    writer_blocked: bool,
    /// Whether buffer was full before last read (for deadlock prevention).
    was_full: bool,
    /// Whether reader is blocked waiting for data (empty buffer, not closed).
    /// This allows external code to detect when a task is waiting for input.
    reader_blocked: bool,
}

impl PipeInner {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: vec![0u8; capacity].into_boxed_slice(),
            head: 0,
            tail: 0,
            len: 0,
            closed: false,
            read_waker: None,
            write_waker: None,
            writer_blocked: false,
            was_full: false,
            reader_blocked: false,
        }
    }

    fn capacity(&self) -> usize {
        self.buffer.len()
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len == self.capacity()
    }

    fn available_write(&self) -> usize {
        self.capacity() - self.len
    }

    /// Check if buffer is below low watermark (writer should be woken).
    fn is_below_low_watermark(&self) -> bool {
        let threshold = self.capacity() * LOW_WATERMARK_PERCENT / 100;
        self.len <= threshold
    }

    /// Read up to `buf.len()` bytes into `buf`.
    fn read_into(&mut self, buf: &mut [u8]) -> usize {
        let to_read = buf.len().min(self.len);
        if to_read == 0 {
            return 0;
        }

        let cap = self.capacity();

        // Handle wrap-around
        if self.head + to_read <= cap {
            buf[..to_read].copy_from_slice(&self.buffer[self.head..self.head + to_read]);
        } else {
            let first = cap - self.head;
            buf[..first].copy_from_slice(&self.buffer[self.head..]);
            buf[first..to_read].copy_from_slice(&self.buffer[..to_read - first]);
        }

        self.head = (self.head + to_read) % cap;
        self.len -= to_read;
        to_read
    }

    /// Write up to `data.len()` bytes from `data`.
    fn write_from(&mut self, data: &[u8]) -> usize {
        let to_write = data.len().min(self.available_write());
        if to_write == 0 {
            return 0;
        }

        let cap = self.capacity();

        // Handle wrap-around
        if self.tail + to_write <= cap {
            self.buffer[self.tail..self.tail + to_write].copy_from_slice(&data[..to_write]);
        } else {
            let first = cap - self.tail;
            self.buffer[self.tail..].copy_from_slice(&data[..first]);
            self.buffer[..to_write - first].copy_from_slice(&data[first..to_write]);
        }

        self.tail = (self.tail + to_write) % cap;
        self.len += to_write;
        to_write
    }
}

/// Async pipe for connecting pipeline stages.
#[derive(Clone)]
pub struct AsyncPipe {
    inner: Rc<RefCell<PipeInner>>,
}

impl AsyncPipe {
    /// Create a new pipe with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Rc::new(RefCell::new(PipeInner::new(capacity))),
        }
    }

    /// Create a new pipe with default capacity.
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }

    /// Get the capacity of the pipe.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.borrow().capacity()
    }

    /// Check if the pipe is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }

    /// Check if the pipe is full.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.inner.borrow().is_full()
    }

    /// Get the number of bytes available to read.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.borrow().len
    }

    /// Close the write end of the pipe.
    ///
    /// This signals EOF to readers.
    pub fn close(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.closed = true;
        inner.reader_blocked = false; // EOF available, reader no longer blocked
        if let Some(waker) = inner.read_waker.take() {
            waker.wake();
        }
    }

    /// Check if the pipe is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.borrow().closed
    }

    /// Check if a reader is blocked waiting for data.
    ///
    /// Returns true if a reader has polled and returned Pending because
    /// the buffer is empty and not closed. This allows external code
    /// (like the runtime) to detect when a command is waiting for stdin.
    #[must_use]
    pub fn is_reader_blocked(&self) -> bool {
        self.inner.borrow().reader_blocked
    }

    /// Drain all available data from the pipe synchronously.
    ///
    /// This is useful for reading output after a command completes.
    /// Returns all buffered data and clears the pipe.
    #[must_use]
    pub fn drain(&self) -> Vec<u8> {
        let mut inner = self.inner.borrow_mut();
        let len = inner.len;
        if len == 0 {
            return Vec::new();
        }

        let mut result = vec![0u8; len];
        let n = inner.read_into(&mut result);
        result.truncate(n);

        // Wake blocked writer since we cleared space
        if inner.writer_blocked {
            inner.writer_blocked = false;
            if let Some(waker) = inner.write_waker.take() {
                waker.wake();
            }
        }

        result
    }

    /// Synchronously push data into the pipe.
    ///
    /// Returns the number of bytes written. May write fewer bytes than
    /// requested if the pipe is full.
    pub fn push(&self, data: &[u8]) -> usize {
        let mut inner = self.inner.borrow_mut();
        let n = inner.write_from(data);

        // Wake blocked reader since we added data
        if n > 0 {
            inner.reader_blocked = false; // Data available, reader no longer blocked
            if let Some(waker) = inner.read_waker.take() {
                waker.wake();
            }
        }

        n
    }

    /// Synchronously push all data into the pipe.
    ///
    /// Writes all data, returning error if pipe is closed or full.
    pub fn push_all(&self, data: &[u8]) -> io::Result<()> {
        let mut pos = 0;
        while pos < data.len() {
            let n = self.push(&data[pos..]);
            if n == 0 {
                if self.is_closed() {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
                }
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "pipe full"));
            }
            pos += n;
        }
        Ok(())
    }

    /// Async read from the pipe.
    pub fn read<'a>(&'a self, buf: &'a mut [u8]) -> PipeRead<'a> {
        PipeRead {
            pipe: self,
            buf,
            registered: false,
        }
    }

    /// Async write to the pipe.
    pub fn write<'a>(&'a self, data: &'a [u8]) -> PipeWrite<'a> {
        PipeWrite {
            pipe: self,
            data,
            registered: false,
        }
    }

    /// Poll for read readiness.
    pub fn poll_read(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let mut inner = self.inner.borrow_mut();

        if !inner.is_empty() {
            // Track if buffer was full before read (for deadlock prevention)
            let was_full = inner.was_full;
            let n = inner.read_into(buf);
            inner.was_full = false; // Clear after read
            inner.reader_blocked = false; // Got data, no longer blocked

            // Wake blocked writer if:
            // 1. Writer is actually blocked, AND
            // 2. EITHER:
            //    a. Buffer dropped below low watermark (normal anti-thrashing), OR
            //    b. Buffer just transitioned from FULL (deadlock prevention)
            //
            // The low watermark prevents thrashing when producer is faster than consumer.
            // The was_full check prevents deadlock if reader stops before crossing threshold.
            if inner.writer_blocked && (inner.is_below_low_watermark() || was_full) {
                inner.writer_blocked = false;
                if let Some(waker) = inner.write_waker.take() {
                    waker.wake();
                }
            }

            return Poll::Ready(Ok(n));
        }

        if inner.closed {
            inner.reader_blocked = false; // EOF, no longer waiting
            return Poll::Ready(Ok(0)); // EOF
        }

        // Empty buffer, not closed - reader is blocked waiting for data
        inner.reader_blocked = true;
        // Only clone waker if it changed (optimization)
        if inner
            .read_waker
            .as_ref()
            .is_none_or(|w| !w.will_wake(cx.waker()))
        {
            inner.read_waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }

    /// Poll for write readiness.
    pub fn poll_write(&self, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let mut inner = self.inner.borrow_mut();

        if inner.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pipe closed",
            )));
        }

        if !inner.is_full() {
            inner.writer_blocked = false;
            let n = inner.write_from(data);

            // Track if we just filled the buffer (for deadlock prevention)
            if inner.is_full() {
                inner.was_full = true;
            }

            // Data is now available, reader is no longer blocked
            if n > 0 {
                inner.reader_blocked = false;
            }

            // Wake reader if it was waiting
            if let Some(waker) = inner.read_waker.take() {
                waker.wake();
            }
            return Poll::Ready(Ok(n));
        }

        // Mark writer as blocked and register waker
        inner.writer_blocked = true;
        inner.was_full = true; // Buffer is full when we block
        // Only clone waker if it changed (optimization)
        if inner
            .write_waker
            .as_ref()
            .is_none_or(|w| !w.will_wake(cx.waker()))
        {
            inner.write_waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

/// Future for reading from a pipe.
pub struct PipeRead<'a> {
    pipe: &'a AsyncPipe,
    buf: &'a mut [u8],
    /// Whether this future has registered a waker (for cleanup on drop).
    registered: bool,
}

impl Future for PipeRead<'_> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let result = this.pipe.poll_read(cx, this.buf);
        // Track if we registered a waker (Pending means we did)
        this.registered = matches!(result, Poll::Pending);
        result
    }
}

impl Drop for PipeRead<'_> {
    fn drop(&mut self) {
        // Clear stale waker if we registered one
        if self.registered {
            self.pipe.inner.borrow_mut().read_waker = None;
        }
    }
}

/// Future for writing to a pipe.
pub struct PipeWrite<'a> {
    pipe: &'a AsyncPipe,
    data: &'a [u8],
    /// Whether this future has registered a waker (for cleanup on drop).
    registered: bool,
}

impl Future for PipeWrite<'_> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = self.pipe.poll_write(cx, self.data);
        // Track if we registered a waker (Pending means we did)
        self.registered = matches!(result, Poll::Pending);
        result
    }
}

impl Drop for PipeWrite<'_> {
    fn drop(&mut self) {
        // Clear stale waker if we registered one
        if self.registered {
            let mut inner = self.pipe.inner.borrow_mut();
            inner.write_waker = None;
            inner.writer_blocked = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_basic_write_read() {
        let pipe = AsyncPipe::new(64);

        // Write some data
        {
            let mut inner = pipe.inner.borrow_mut();
            let n = inner.write_from(b"hello");
            assert_eq!(n, 5);
        }

        // Read it back
        {
            let mut inner = pipe.inner.borrow_mut();
            let mut buf = [0u8; 10];
            let n = inner.read_into(&mut buf);
            assert_eq!(n, 5);
            assert_eq!(&buf[..n], b"hello");
        }
    }

    #[test]
    fn pipe_wrap_around() {
        let pipe = AsyncPipe::new(8);

        // Write 6 bytes
        {
            let mut inner = pipe.inner.borrow_mut();
            inner.write_from(b"123456");
        }

        // Read 4 bytes (head moves to 4)
        {
            let mut inner = pipe.inner.borrow_mut();
            let mut buf = [0u8; 4];
            inner.read_into(&mut buf);
            assert_eq!(&buf, b"1234");
        }

        // Write 4 more bytes (wraps around)
        {
            let mut inner = pipe.inner.borrow_mut();
            let n = inner.write_from(b"abcd");
            assert_eq!(n, 4);
        }

        // Read all remaining
        {
            let mut inner = pipe.inner.borrow_mut();
            let mut buf = [0u8; 10];
            let n = inner.read_into(&mut buf);
            assert_eq!(n, 6);
            assert_eq!(&buf[..n], b"56abcd");
        }
    }

    #[test]
    fn pipe_full_empty() {
        let pipe = AsyncPipe::new(4);

        assert!(pipe.is_empty());
        assert!(!pipe.is_full());

        {
            let mut inner = pipe.inner.borrow_mut();
            inner.write_from(b"1234");
        }

        assert!(!pipe.is_empty());
        assert!(pipe.is_full());

        // Can't write more when full
        {
            let mut inner = pipe.inner.borrow_mut();
            let n = inner.write_from(b"5");
            assert_eq!(n, 0);
        }
    }

    #[test]
    fn pipe_close_eof() {
        let pipe = AsyncPipe::new(64);
        pipe.close();

        assert!(pipe.is_closed());
    }

    #[test]
    fn pipe_low_watermark() {
        // Pipe capacity 100, low watermark at 25%
        let pipe = AsyncPipe::new(100);

        // Fill to capacity
        {
            let mut inner = pipe.inner.borrow_mut();
            inner.write_from(&[0u8; 100]);
            assert!(inner.is_full());
            inner.writer_blocked = true; // Simulate blocked writer
        }

        // Read 50 bytes - still above 25% (50/100 = 50%)
        {
            let mut inner = pipe.inner.borrow_mut();
            let mut buf = [0u8; 50];
            inner.read_into(&mut buf);
            assert_eq!(inner.len, 50);
            // Writer should NOT be woken yet (still above low watermark)
            assert!(!inner.is_below_low_watermark());
        }

        // Read 26 more bytes - now at 24% (24/100), below low watermark
        {
            let mut inner = pipe.inner.borrow_mut();
            let mut buf = [0u8; 26];
            inner.read_into(&mut buf);
            assert_eq!(inner.len, 24);
            // Now below low watermark
            assert!(inner.is_below_low_watermark());
        }
    }

    #[test]
    fn pipe_deadlock_prevention() {
        // Test that was_full flag prevents deadlock when reader doesn't cross low watermark
        let pipe = AsyncPipe::new(100);

        // Fill to capacity
        {
            let mut inner = pipe.inner.borrow_mut();
            inner.write_from(&[0u8; 100]);
            assert!(inner.is_full());
            inner.was_full = true; // Simulate blocked writer (set by poll_write)
            inner.writer_blocked = true;
        }

        // Read just 1 byte - nowhere near low watermark (99% full)
        // But was_full should still trigger writer wake
        {
            let inner = pipe.inner.borrow();
            assert!(inner.was_full);
            assert!(inner.writer_blocked);
            assert!(!inner.is_below_low_watermark()); // 99 bytes = 99% > 25%
        }

        // After poll_read, was_full is cleared and writer should be woken
        // We can verify the state transitions
        {
            let mut inner = pipe.inner.borrow_mut();

            // Simulate what poll_read does
            let was_full = inner.was_full;
            let mut buf = [0u8; 1];
            inner.read_into(&mut buf);
            inner.was_full = false;

            // Check: writer should be woken because was_full was true
            // (even though we're still at 99% capacity)
            assert!(was_full, "was_full should have been true before read");
            assert!(!inner.is_below_low_watermark(), "still above low watermark");

            // The condition in poll_read: writer_blocked && (is_below_low_watermark() || was_full)
            // With was_full = true, this evaluates to true, so writer would be woken
        }
    }

    #[test]
    fn pipe_was_full_only_on_first_read() {
        // Verify that was_full only triggers once per fill cycle
        let pipe = AsyncPipe::new(100);

        // Fill to capacity
        {
            let mut inner = pipe.inner.borrow_mut();
            inner.write_from(&[0u8; 100]);
            inner.was_full = true;
            inner.writer_blocked = true;
        }

        // First read clears was_full
        {
            let mut inner = pipe.inner.borrow_mut();
            assert!(inner.was_full);
            let mut buf = [0u8; 10];
            inner.read_into(&mut buf);
            inner.was_full = false; // Simulating poll_read behavior
            assert!(!inner.was_full);
        }

        // Subsequent reads don't have was_full set
        {
            let mut inner = pipe.inner.borrow_mut();
            assert!(!inner.was_full);
            inner.writer_blocked = true; // Writer blocked again after first wake
            let mut buf = [0u8; 10];
            inner.read_into(&mut buf);
            // was_full is still false - writer won't be woken until low watermark
            assert!(!inner.was_full);
            assert!(!inner.is_below_low_watermark()); // 80 bytes = 80% > 25%
        }
    }

    // =========================================================================
    // Comprehensive deadlock prevention tests
    // =========================================================================
    //
    // These tests verify the pipe never deadlocks under any scenario.
    // The key mechanisms are:
    // 1. was_full flag: Wakes writer on first read after buffer was full
    // 2. Low watermark: Wakes writer when buffer drops below 25%
    // 3. Close propagation: Wakes blocked parties when pipe closes

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{RawWaker, RawWakerVTable, Waker};

    /// Helper to unwrap `Poll::Ready(Ok(n))`
    fn poll_ready_ok(result: Poll<io::Result<usize>>) -> usize {
        match result {
            Poll::Ready(Ok(n)) => n,
            Poll::Ready(Err(e)) => panic!("unexpected error: {e}"),
            Poll::Pending => panic!("unexpected Pending"),
        }
    }

    /// Create a waker that increments a counter when woken.
    ///
    /// # Safety Invariants
    ///
    /// The data pointer in all vtable functions is a valid `*const AtomicUsize`
    /// created via `Arc::into_raw`. The ownership model:
    /// - `clone_fn`: Borrows the Arc, creates a new Arc (caller owns original)
    /// - `wake_fn`: Takes ownership via `Arc::from_raw`, consumes the pointer
    /// - `wake_by_ref_fn`: Borrows only, pointer remains valid
    /// - `drop_fn`: Takes ownership via `Arc::from_raw`, drops the Arc
    fn make_counting_waker(counter: Arc<AtomicUsize>) -> Waker {
        fn clone_fn(data: *const ()) -> RawWaker {
            // SAFETY: data was created from Arc::into_raw. Bumping the
            // strong count directly gives us a second logical owner
            // without the reconstruct-and-forget dance.
            let ptr = data.cast::<AtomicUsize>();
            unsafe { Arc::increment_strong_count(ptr) };
            RawWaker::new(ptr.cast::<()>(), &VTABLE)
        }
        fn wake_fn(data: *const ()) {
            // SAFETY: data was created from Arc::into_raw. We take ownership
            // and the Arc will be dropped at the end of this function.
            let counter = unsafe { Arc::from_raw(data.cast::<AtomicUsize>()) };
            counter.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref_fn(data: *const ()) {
            // SAFETY: data was created from Arc::into_raw. We only borrow,
            // so the pointer remains valid after this call.
            let counter = unsafe { &*data.cast::<AtomicUsize>() };
            counter.fetch_add(1, Ordering::SeqCst);
        }
        fn drop_fn(data: *const ()) {
            // SAFETY: data was created from Arc::into_raw. We take ownership
            // to drop the Arc and deallocate if this was the last reference.
            unsafe { Arc::from_raw(data.cast::<AtomicUsize>()) };
        }
        static VTABLE: RawWakerVTable =
            RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);

        let raw = RawWaker::new(Arc::into_raw(counter).cast::<()>(), &VTABLE);
        // SAFETY: The vtable functions correctly implement the RawWaker contract
        // as documented above. The data pointer is valid.
        unsafe { Waker::from_raw(raw) }
    }

    /// Case 1: `was_full` triggers writer wake on first read after full buffer
    ///
    /// Scenario: Writer fills buffer, blocks. Reader reads partial data (above threshold).
    /// Expected: Writer woken immediately (not waiting for low watermark).
    #[test]
    fn deadlock_case1_was_full_triggers_on_first_read() {
        let write_wakes = Arc::new(AtomicUsize::new(0));
        let pipe = AsyncPipe::new(100);

        // Fill buffer completely
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 100]),
                Poll::Ready(Ok(100))
            ));
        }

        // Writer blocks trying to write more
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 10]),
                Poll::Pending
            ));
            assert!(pipe.inner.borrow().writer_blocked);
            assert!(pipe.inner.borrow().was_full);
        }

        write_wakes.store(0, Ordering::SeqCst);

        // Reader reads 10 bytes (90% remaining, above 25% threshold)
        {
            let read_wakes = Arc::new(AtomicUsize::new(0));
            let waker = make_counting_waker(read_wakes);
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(
                pipe.poll_read(&mut cx, &mut buf),
                Poll::Ready(Ok(10))
            ));
            assert!(!pipe.inner.borrow().is_below_low_watermark()); // 90% > 25%
        }

        // Writer MUST be woken (was_full triggered)
        assert_eq!(write_wakes.load(Ordering::SeqCst), 1);
        assert!(!pipe.inner.borrow().was_full); // Cleared after use
    }

    /// Case 2: `was_full` only triggers ONCE per fill cycle
    ///
    /// Scenario: After first wake, subsequent reads don't wake until threshold.
    /// Expected: Only first read wakes writer, subsequent reads don't (until threshold).
    #[test]
    fn deadlock_case2_was_full_triggers_only_once() {
        let write_wakes = Arc::new(AtomicUsize::new(0));
        let pipe = AsyncPipe::new(100);

        // Fill and block
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            poll_ready_ok(pipe.poll_write(&mut cx, &[0u8; 100]));
        }
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 10]),
                Poll::Pending
            ));
        }

        write_wakes.store(0, Ordering::SeqCst);

        // First read - wakes writer (was_full)
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            poll_ready_ok(pipe.poll_read(&mut cx, &mut buf));
        }
        assert_eq!(write_wakes.load(Ordering::SeqCst), 1);

        // Simulate writer waking but blocking again (buffer still 90% full)
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            // Writer can now write 10 bytes (space available)
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 10]),
                Poll::Ready(Ok(10))
            ));
            // Buffer back to 100%, writer tries more and blocks
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 10]),
                Poll::Pending
            ));
        }

        write_wakes.store(0, Ordering::SeqCst);

        // Second read - should NOT wake (was_full is set again when writer blocked on full buffer)
        // Actually, writer just blocked on full buffer, so was_full IS set
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            poll_ready_ok(pipe.poll_read(&mut cx, &mut buf));
        }
        // Writer should be woken again because buffer was full when it blocked
        assert_eq!(write_wakes.load(Ordering::SeqCst), 1);
    }

    /// Case 3: Low watermark triggers writer wake
    ///
    /// Scenario: Buffer above threshold, reads eventually cross threshold.
    /// Expected: Writer woken when buffer drops to/below 25%.
    #[test]
    fn deadlock_case3_low_watermark_triggers_wake() {
        let write_wakes = Arc::new(AtomicUsize::new(0));
        let pipe = AsyncPipe::new(100);

        // Fill and block
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            poll_ready_ok(pipe.poll_write(&mut cx, &[0u8; 100]));
        }
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 10]),
                Poll::Pending
            ));
        }

        // First read clears was_full and wakes writer
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            poll_ready_ok(pipe.poll_read(&mut cx, &mut buf));
        }

        // Writer woken, writes 10 bytes and blocks again (now at 100 bytes)
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            poll_ready_ok(pipe.poll_write(&mut cx, &[0u8; 10])); // Writes 10, now 100 bytes
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 10]),
                Poll::Pending
            )); // Blocks
        }

        // Clear was_full to test threshold only (writer is blocked with waker registered)
        pipe.inner.borrow_mut().was_full = false;

        write_wakes.store(0, Ordering::SeqCst);

        // Read 75 bytes: 100 - 75 = 25 bytes (exactly at 25% threshold)
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 75];
            poll_ready_ok(pipe.poll_read(&mut cx, &mut buf));
            assert!(pipe.inner.borrow().is_below_low_watermark()); // 25% <= 25%
        }

        // Writer should be woken via low watermark
        assert_eq!(write_wakes.load(Ordering::SeqCst), 1);
    }

    /// Case 4: Reader blocked on empty pipe, writer writes, reader woken
    ///
    /// Scenario: Empty pipe, reader blocks, writer writes data.
    /// Expected: Reader woken when data available.
    #[test]
    fn deadlock_case4_reader_woken_when_data_available() {
        let read_wakes = Arc::new(AtomicUsize::new(0));
        let pipe = AsyncPipe::new(100);

        // Reader blocks on empty pipe
        {
            let waker = make_counting_waker(Arc::clone(&read_wakes));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(pipe.poll_read(&mut cx, &mut buf), Poll::Pending));
        }

        read_wakes.store(0, Ordering::SeqCst);

        // Writer writes data
        {
            let write_wakes = Arc::new(AtomicUsize::new(0));
            let waker = make_counting_waker(write_wakes);
            let mut cx = Context::from_waker(&waker);
            assert!(matches!(
                pipe.poll_write(&mut cx, &[1u8; 50]),
                Poll::Ready(Ok(50))
            ));
        }

        // Reader MUST be woken
        assert_eq!(read_wakes.load(Ordering::SeqCst), 1);
    }

    /// Case 5: Close wakes blocked reader
    ///
    /// Scenario: Reader blocked on empty pipe, pipe closed.
    /// Expected: Reader woken and gets EOF (0 bytes).
    #[test]
    fn deadlock_case5_close_wakes_blocked_reader() {
        let read_wakes = Arc::new(AtomicUsize::new(0));
        let pipe = AsyncPipe::new(100);

        // Reader blocks on empty pipe
        {
            let waker = make_counting_waker(Arc::clone(&read_wakes));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(pipe.poll_read(&mut cx, &mut buf), Poll::Pending));
        }

        read_wakes.store(0, Ordering::SeqCst);

        // Close pipe
        pipe.close();

        // Reader MUST be woken
        assert_eq!(read_wakes.load(Ordering::SeqCst), 1);

        // Reader should get EOF
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(
                pipe.poll_read(&mut cx, &mut buf),
                Poll::Ready(Ok(0))
            ));
        }
    }

    /// Case 6: Writer gets `BrokenPipe` on closed pipe
    ///
    /// Scenario: Pipe closed, writer tries to write.
    /// Expected: Writer gets error immediately (no deadlock).
    #[test]
    fn deadlock_case6_writer_error_on_closed_pipe() {
        let pipe = AsyncPipe::new(100);
        pipe.close();

        let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
        let mut cx = Context::from_waker(&waker);

        let result = pipe.poll_write(&mut cx, &[0u8; 10]);
        assert!(matches!(result, Poll::Ready(Err(_))));
    }

    /// Case 7: No spurious writer wakes on non-full buffer
    ///
    /// Scenario: Buffer never fills, reader reads.
    /// Expected: No writer wakes (writer isn't blocked).
    #[test]
    fn deadlock_case7_no_spurious_wakes() {
        let write_wakes = Arc::new(AtomicUsize::new(0));
        let pipe = AsyncPipe::new(100);

        // Write 50 bytes (not full)
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            poll_ready_ok(pipe.poll_write(&mut cx, &[0u8; 50]));
        }

        assert!(!pipe.inner.borrow().writer_blocked);
        assert!(!pipe.inner.borrow().was_full);

        write_wakes.store(0, Ordering::SeqCst);

        // Read some data
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 20];
            poll_ready_ok(pipe.poll_read(&mut cx, &mut buf));
        }

        // No writer wake (writer wasn't blocked)
        assert_eq!(write_wakes.load(Ordering::SeqCst), 0);
    }

    /// Case 8: Multiple fill cycles all prevent deadlock
    ///
    /// Scenario: Fill → partial read → write → fill → partial read → ...
    /// Expected: Every fill cycle's first read wakes the writer.
    #[test]
    fn deadlock_case8_multiple_fill_cycles() {
        let write_wakes = Arc::new(AtomicUsize::new(0));
        let pipe = AsyncPipe::new(100);
        let write_buf = [0u8; 100];

        for cycle in 0..3 {
            // Fill buffer
            {
                let waker = make_counting_waker(Arc::clone(&write_wakes));
                let mut cx = Context::from_waker(&waker);
                let available = pipe.inner.borrow().available_write();
                if available > 0 {
                    poll_ready_ok(pipe.poll_write(&mut cx, &write_buf[..available]));
                }
            }

            if pipe.inner.borrow().is_full() {
                // Try to write more - should block
                {
                    let waker = make_counting_waker(Arc::clone(&write_wakes));
                    let mut cx = Context::from_waker(&waker);
                    assert!(matches!(
                        pipe.poll_write(&mut cx, &[0u8; 10]),
                        Poll::Pending
                    ));
                }

                write_wakes.store(0, Ordering::SeqCst);

                // Partial read (stays above threshold)
                {
                    let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
                    let mut cx = Context::from_waker(&waker);
                    let mut buf = [0u8; 10];
                    poll_ready_ok(pipe.poll_read(&mut cx, &mut buf));
                }

                // Writer MUST be woken on each cycle
                assert_eq!(
                    write_wakes.load(Ordering::SeqCst),
                    1,
                    "Cycle {cycle}: writer should be woken on partial read from full buffer"
                );
            }

            // Drain some data for next cycle
            {
                let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
                let mut cx = Context::from_waker(&waker);
                let mut buf = [0u8; 80];
                let _ = pipe.poll_read(&mut cx, &mut buf);
            }
        }
    }

    /// Case 9: Exact threshold boundary
    ///
    /// Scenario: Buffer at exactly 26% (just above threshold), then at 25% (at threshold).
    /// Expected: Wake happens at 25%, not at 26%.
    #[test]
    fn deadlock_case9_exact_threshold_boundary() {
        let write_wakes = Arc::new(AtomicUsize::new(0));
        let pipe = AsyncPipe::new(100);

        // Fill and block
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            poll_ready_ok(pipe.poll_write(&mut cx, &[0u8; 100]));
        }
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 10]),
                Poll::Pending
            ));
        }

        // Read to get first-read was_full wake out of the way
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 1];
            poll_ready_ok(pipe.poll_read(&mut cx, &mut buf)); // 99 bytes remain
        }

        // Writer woken, writes 1 byte and blocks again (now at 100 bytes)
        {
            let waker = make_counting_waker(Arc::clone(&write_wakes));
            let mut cx = Context::from_waker(&waker);
            poll_ready_ok(pipe.poll_write(&mut cx, &[0u8; 1])); // Writes 1, now 100 bytes
            assert!(matches!(
                pipe.poll_write(&mut cx, &[0u8; 10]),
                Poll::Pending
            )); // Blocks
        }

        // Clear was_full to test threshold only
        pipe.inner.borrow_mut().was_full = false;

        write_wakes.store(0, Ordering::SeqCst);

        // Read to 26 bytes (26% - just above threshold)
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 74]; // 100 - 74 = 26
            poll_ready_ok(pipe.poll_read(&mut cx, &mut buf));
            assert_eq!(pipe.inner.borrow().len, 26);
            assert!(!pipe.inner.borrow().is_below_low_watermark()); // 26 > 25
        }

        // Writer NOT woken yet (26% > 25%)
        assert_eq!(write_wakes.load(Ordering::SeqCst), 0);

        // Read 1 more byte to hit exactly 25%
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 1];
            poll_ready_ok(pipe.poll_read(&mut cx, &mut buf));
            assert_eq!(pipe.inner.borrow().len, 25);
            assert!(pipe.inner.borrow().is_below_low_watermark()); // 25 <= 25
        }

        // NOW writer should be woken
        assert_eq!(write_wakes.load(Ordering::SeqCst), 1);
    }

    /// Case 10: Empty buffer read doesn't wake anyone
    ///
    /// Scenario: Empty pipe, reader polls (gets Pending), no writer registered.
    /// Expected: No crashes, no spurious behavior.
    #[test]
    fn deadlock_case10_empty_read_safe() {
        let pipe = AsyncPipe::new(100);

        // Reader polls empty pipe
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(pipe.poll_read(&mut cx, &mut buf), Poll::Pending));
        }

        // No writer waker registered, no crash
        assert!(pipe.inner.borrow().write_waker.is_none());
        assert!(!pipe.inner.borrow().writer_blocked);
    }

    // =========================================================================
    // Reader blocked tracking tests
    // =========================================================================
    //
    // These tests verify that is_reader_blocked() correctly tracks when a
    // reader is waiting for data. This enables dynamic stdin blocking detection.

    /// Verify `reader_blocked` is set when reading from empty pipe
    #[test]
    fn reader_blocked_on_empty_read() {
        let pipe = AsyncPipe::new(100);

        // Initially not blocked
        assert!(!pipe.is_reader_blocked());

        // Read from empty pipe - should be marked as blocked
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(pipe.poll_read(&mut cx, &mut buf), Poll::Pending));
        }

        // Now blocked
        assert!(pipe.is_reader_blocked());
    }

    /// Verify `reader_blocked` is cleared when data is pushed
    #[test]
    fn reader_blocked_cleared_on_push() {
        let pipe = AsyncPipe::new(100);

        // Read from empty pipe to set blocked
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(pipe.poll_read(&mut cx, &mut buf), Poll::Pending));
        }
        assert!(pipe.is_reader_blocked());

        // Push data - should clear blocked
        let n = pipe.push(b"hello");
        assert_eq!(n, 5);

        // No longer blocked
        assert!(!pipe.is_reader_blocked());
    }

    /// Verify `reader_blocked` is cleared when pipe is closed
    #[test]
    fn reader_blocked_cleared_on_close() {
        let pipe = AsyncPipe::new(100);

        // Read from empty pipe to set blocked
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(pipe.poll_read(&mut cx, &mut buf), Poll::Pending));
        }
        assert!(pipe.is_reader_blocked());

        // Close pipe - should clear blocked
        pipe.close();

        // No longer blocked (EOF available)
        assert!(!pipe.is_reader_blocked());
    }

    /// Verify `reader_blocked` is cleared when data is read successfully
    #[test]
    fn reader_blocked_cleared_on_successful_read() {
        let pipe = AsyncPipe::new(100);

        // Push some data first
        pipe.push(b"hello");

        // Read from empty pipe to set blocked (shouldn't be blocked - has data!)
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            let result = pipe.poll_read(&mut cx, &mut buf);
            assert!(matches!(result, Poll::Ready(Ok(5))));
        }

        // Not blocked (data was available)
        assert!(!pipe.is_reader_blocked());
    }

    /// Verify `reader_blocked` is cleared when reading EOF
    #[test]
    fn reader_blocked_cleared_on_eof() {
        let pipe = AsyncPipe::new(100);

        // Read from empty pipe to set blocked
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(pipe.poll_read(&mut cx, &mut buf), Poll::Pending));
        }
        assert!(pipe.is_reader_blocked());

        // Close pipe
        pipe.close();
        assert!(!pipe.is_reader_blocked());

        // Read again - should get EOF (0 bytes)
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            let result = pipe.poll_read(&mut cx, &mut buf);
            assert!(matches!(result, Poll::Ready(Ok(0))));
        }

        // Still not blocked
        assert!(!pipe.is_reader_blocked());
    }

    /// Verify `reader_blocked` is cleared when data is written via `poll_write`
    #[test]
    fn reader_blocked_cleared_on_async_write() {
        let pipe = AsyncPipe::new(100);

        // Read from empty pipe to set blocked
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            assert!(matches!(pipe.poll_read(&mut cx, &mut buf), Poll::Pending));
        }
        assert!(pipe.is_reader_blocked());

        // Write via `poll_write` - should clear blocked
        {
            let waker = make_counting_waker(Arc::new(AtomicUsize::new(0)));
            let mut cx = Context::from_waker(&waker);
            let result = pipe.poll_write(&mut cx, b"world");
            assert!(matches!(result, Poll::Ready(Ok(5))));
        }

        // No longer blocked
        assert!(!pipe.is_reader_blocked());
    }
}
