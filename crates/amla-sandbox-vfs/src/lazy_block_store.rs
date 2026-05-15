//! Lazy (on-demand) block store for large external files.
//!
//! Fetches blocks individually from the host filesystem as they're requested.
//! Blocks are cached in the `PageCache` to avoid redundant host operations.
//!
//! # Fetch Coordination
//!
//! Uses `PageCache`'s fetch coordination to avoid duplicate host ops when
//! multiple readers request the same block simultaneously:
//!
//! 1. First reader: Issues host op, other readers wait
//! 2. Host op completes: Block cached, all waiters woken
//! 3. Subsequent reads: Served from cache
//!
//! # Example
//!
//! ```ignore
//! let store = LazyBlockStore::new("/large/file.bin", 1_000_000_000, channel);
//! // Blocks fetched on-demand as FileHandle reads them
//! ```

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::task::{Context, Poll};

use amla_scheduler::{HostChannel, HostOpFuture};

use crate::VfsError;
use crate::block_store::{BLOCK_SIZE, BlockStore};
use crate::page_cache::{FetchAction, PageCache};

/// State of a pending block fetch.
struct PendingFetch {
    /// Block number being fetched.
    block_num: u64,
    /// Host operation future.
    future: HostOpFuture,
}

/// Lazy block store for on-demand fetching.
///
/// Fetches blocks individually from the host filesystem as they're requested.
/// Blocks are cached in the `PageCache` after fetching.
///
/// Read-only: writes are not supported (returns error).
pub struct LazyBlockStore {
    /// Path to the file on the host.
    path: String,
    /// Total file size in bytes.
    file_size: u64,
    /// Block size in bytes.
    block_size: usize,
    /// Host channel for file operations.
    channel: HostChannel,
    /// Currently pending fetch (if any).
    pending: RefCell<Option<PendingFetch>>,
    /// Error state (if any previous operation failed).
    error: RefCell<Option<String>>,
    /// Number of blocks successfully fetched (for stats).
    fetches: Cell<u64>,
}

impl LazyBlockStore {
    /// Create a new lazy block store.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file on the host filesystem
    /// * `file_size` - Total size of the file in bytes (must be known upfront)
    /// * `channel` - Host channel for issuing read operations
    pub fn new(path: impl Into<String>, file_size: u64, channel: HostChannel) -> Self {
        Self {
            path: path.into(),
            file_size,
            block_size: BLOCK_SIZE,
            channel,
            pending: RefCell::new(None),
            error: RefCell::new(None),
            fetches: Cell::new(0),
        }
    }

    /// Create with custom block size (for testing).
    pub fn with_block_size(
        path: impl Into<String>,
        file_size: u64,
        block_size: usize,
        channel: HostChannel,
    ) -> Self {
        Self {
            path: path.into(),
            file_size,
            block_size,
            channel,
            pending: RefCell::new(None),
            error: RefCell::new(None),
            fetches: Cell::new(0),
        }
    }

    /// Get the file path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get number of blocks fetched from host.
    pub fn fetch_count(&self) -> u64 {
        self.fetches.get()
    }

    /// Calculate byte offset for a block.
    fn block_offset(&self, block_num: u64) -> u64 {
        block_num * self.block_size as u64
    }

    /// Calculate bytes to read for a block (may be less than `block_size` at EOF).
    fn block_length(&self, block_num: u64) -> u64 {
        let offset = self.block_offset(block_num);
        if offset >= self.file_size {
            0
        } else {
            (self.file_size - offset).min(self.block_size as u64)
        }
    }

    /// Poll a pending fetch to completion.
    fn poll_pending_fetch(
        &self,
        cx: &mut Context<'_>,
        cache: &PageCache,
    ) -> Poll<Result<(), VfsError>> {
        let mut pending = self.pending.borrow_mut();
        let Some(fetch) = pending.as_mut() else {
            return Poll::Ready(Ok(()));
        };

        let block_num = fetch.block_num;

        match Pin::new(&mut fetch.future).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                // Clear the pending fetch
                let _ = pending.take();

                match result {
                    Ok(data) => {
                        // Insert into cache and wake waiters
                        cache.complete_fetch(block_num, data);
                        self.fetches.set(self.fetches.get() + 1);
                        Poll::Ready(Ok(()))
                    }
                    Err(e) => {
                        // Cancel fetch and propagate error
                        cache.cancel_fetch(block_num);
                        let msg = format!("Failed to read block {block_num}: {e}");
                        *self.error.borrow_mut() = Some(msg.clone());
                        Poll::Ready(Err(VfsError::NotFound(msg)))
                    }
                }
            }
        }
    }
}

impl BlockStore for LazyBlockStore {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn size(&self) -> u64 {
        self.file_size
    }

    fn poll_read_block(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        block_num: u64,
        cache: &PageCache,
    ) -> Poll<Result<(), VfsError>> {
        let this = self.get_mut();

        // Check for previous error
        if let Some(error) = this.error.borrow().as_ref() {
            return Poll::Ready(Err(VfsError::NotFound(error.clone())));
        }

        // Already have a pending fetch? Check if it's for our block.
        let pending_block_num = this.pending.borrow().as_ref().map(|p| p.block_num);
        if let Some(pending_num) = pending_block_num {
            if pending_num == block_num {
                // Pending fetch is for our block - poll it
                return this.poll_pending_fetch(cx, cache);
            }
            // Pending fetch is for a different block.
            // Poll it to make progress, but we can't fetch our block until it completes.
            // Note: poll_pending_fetch registers cx.waker() with the inner future,
            // so we'll be woken when the pending fetch completes.
            match this.poll_pending_fetch(cx, cache) {
                Poll::Pending => {
                    // Other fetch still in progress - inner future has our waker
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {
                    // Other fetch completed - fall through to check our block
                }
            }
        }

        // Check what action to take
        match cache.try_start_fetch(block_num) {
            FetchAction::Cached => {
                // Already in cache
                Poll::Ready(Ok(()))
            }
            FetchAction::Wait => {
                // Another task is fetching this block - register waker and wait
                cache.register_fetch_waker(block_num, cx.waker().clone());
                Poll::Pending
            }
            FetchAction::Fetch => {
                // We need to fetch this block
                let offset = this.block_offset(block_num);
                let length = this.block_length(block_num);

                if length == 0 {
                    // Past EOF - insert empty block
                    cache.complete_fetch(block_num, Vec::new());
                    return Poll::Ready(Ok(()));
                }

                // Start the host operation
                let future = this.channel.file_read_range(&this.path, offset, length);
                *this.pending.borrow_mut() = Some(PendingFetch { block_num, future });

                // Poll it once to submit
                this.poll_pending_fetch(cx, cache)
            }
        }
    }

    fn poll_write_block(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _block_num: u64,
        _data: &[u8],
        _cache: &PageCache,
    ) -> Poll<Result<(), VfsError>> {
        // Lazy block store is read-only
        Poll::Ready(Err(VfsError::PermissionDenied(
            "LazyBlockStore is read-only".into(),
        )))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
        // Nothing to flush (read-only)
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::BLOCK_SIZE;
    use amla_scheduler::{Executor, HostOpKind, RandomSourceFn, RunState, TimeSourceFn};
    use std::rc::Rc;

    /// Create a test `HostChannel` with mock time/random sources.
    fn test_channel(capacity: usize) -> HostChannel {
        let time_source: TimeSourceFn = Rc::new(|_runtime_id, _clock| 0);
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        HostChannel::new(1, capacity, time_source, random_source)
    }

    /// Test basic lazy block read through scheduler.
    #[test]
    fn lazy_store_basic_read() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();
        let cache = PageCache::new(BLOCK_SIZE, 4096);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/test/file.txt", 4096, channel);

                // Read block 0
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();

                assert!(cache.contains(0));
                assert_eq!(store.fetch_count(), 1);

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Run until blocked
        let state = exec.run();
        assert!(matches!(state, RunState::Blocked));

        // Complete the host op
        let req = channel_for_host
            .take_pending()
            .expect("should have pending");
        assert!(matches!(req.kind, HostOpKind::FileReadRange { .. }));
        channel_for_host.complete(req.id, vec![0u8; BLOCK_SIZE]);

        // Run to completion
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));
    }

    /// Test reading past EOF returns empty block.
    #[test]
    fn lazy_store_past_eof() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let cache = PageCache::new(BLOCK_SIZE, 10); // Only 10 bytes

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/test/small.txt", 10, channel);

                // Read block 1 (past EOF) - should return immediately with empty block
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 1, &cache))
                    .await
                    .unwrap();

                assert!(cache.contains(1));
                assert!(cache.get(1).unwrap().is_empty());
                assert_eq!(store.fetch_count(), 0); // No host op needed

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Should complete without blocking
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));
    }

    /// Test that writes are denied.
    #[test]
    fn lazy_store_write_denied() {
        use std::sync::Arc;
        use std::task::{Wake, Waker};

        // Simple waker that does nothing
        struct NoopWaker;
        impl Wake for NoopWaker {
            fn wake(self: Arc<Self>) {}
        }

        let channel = test_channel(10);
        let cache = PageCache::new(BLOCK_SIZE, 100);
        let mut store = LazyBlockStore::new("/test/file.txt", 100, channel);

        let waker = Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut store).poll_write_block(&mut cx, 0, b"data", &cache);
        match result {
            Poll::Ready(Err(VfsError::PermissionDenied(_))) => {}
            other => panic!("Expected PermissionDenied, got {other:?}"),
        }
    }

    /// Test that cached blocks are not refetched.
    #[test]
    fn lazy_store_cached_blocks_not_refetched() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();
        let cache = PageCache::new(BLOCK_SIZE, 4096);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/test/file.txt", 4096, channel);

                // Read block 0 first time
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();

                // Read block 0 second time - should not fetch
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();

                assert_eq!(store.fetch_count(), 1); // Only one fetch

                Ok(amla_scheduler::Exit::success())
            }
        });

        // First run - blocked waiting for fetch
        let state = exec.run();
        assert!(matches!(state, RunState::Blocked));

        // Complete the fetch
        let req = channel_for_host
            .take_pending()
            .expect("should have pending");
        channel_for_host.complete(req.id, vec![0u8; BLOCK_SIZE]);

        // Second run - completes (second read is from cache)
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));

        // No more pending requests
        assert!(channel_for_host.take_pending().is_none());
    }

    /// Test properties.
    #[test]
    fn lazy_store_properties() {
        let channel = test_channel(10);
        let store = LazyBlockStore::new("/test/file.txt", 12345, channel);

        assert_eq!(store.path(), "/test/file.txt");
        assert_eq!(store.size(), 12345);
        assert_eq!(store.block_size(), BLOCK_SIZE);
        assert_eq!(store.fetch_count(), 0);
    }

    // ========== HOST FILE READING TESTS ==========

    /// Test error handling when host op fails.
    #[test]
    fn lazy_store_host_error() {
        use std::io;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();
        let cache = PageCache::new(BLOCK_SIZE, 4096);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/nonexistent.txt", 4096, channel);

                // Read should fail
                let result =
                    std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                        .await;

                assert!(result.is_err());

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Run until blocked
        let state = exec.run();
        assert!(matches!(state, RunState::Blocked));

        // Host returns error
        let req = channel_for_host.take_pending().unwrap();
        channel_for_host.complete_err(
            req.id,
            io::Error::new(io::ErrorKind::NotFound, "file not found"),
        );

        // Task should handle error and complete
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));
    }

    /// Test that error state persists after failure.
    #[test]
    fn lazy_store_error_state_persists() {
        use std::io;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();
        let cache = PageCache::new(BLOCK_SIZE, 4096);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/error.txt", 4096, channel);

                // First read fails
                let result1 =
                    std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                        .await;
                assert!(result1.is_err());

                // Subsequent reads should also fail immediately
                let result2 =
                    std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 1, &cache))
                        .await;
                assert!(result2.is_err());

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Run until blocked
        let state = exec.run();
        assert!(matches!(state, RunState::Blocked));

        // Host returns error
        let req = channel_for_host.take_pending().unwrap();
        channel_for_host.complete_err(
            req.id,
            io::Error::new(io::ErrorKind::PermissionDenied, "access denied"),
        );

        // Should complete (second read fails immediately due to error state)
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));
    }

    /// Test sequential reads of multiple blocks.
    #[test]
    fn lazy_store_sequential_reads() {
        const NUM_BLOCKS: u64 = 5;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();
        let file_size = NUM_BLOCKS * BLOCK_SIZE as u64;
        let cache = PageCache::new(BLOCK_SIZE, file_size);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/multi.bin", file_size, channel);

                // Read all blocks sequentially
                for block_num in 0..NUM_BLOCKS {
                    std::future::poll_fn(|cx| {
                        Pin::new(&mut store).poll_read_block(cx, block_num, &cache)
                    })
                    .await
                    .unwrap();

                    assert!(cache.contains(block_num));
                }

                assert_eq!(store.fetch_count(), NUM_BLOCKS);

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Complete all fetches
        let mut fetches_completed = 0;
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending() {
                        match &req.kind {
                            HostOpKind::FileReadRange { offset, length, .. } => {
                                // Return data that encodes the block number
                                let block_num = offset / BLOCK_SIZE as u64;
                                #[allow(clippy::cast_possible_truncation)]
                                let data = vec![block_num as u8; *length as usize];
                                channel_for_host.complete(req.id, data);
                                fetches_completed += 1;
                            }
                            other => panic!("Unexpected op: {other:?}"),
                        }
                    }
                }
            }
        }

        assert_eq!(fetches_completed, NUM_BLOCKS);
    }

    /// Test partial block at end of file (non-aligned file size).
    #[test]
    fn lazy_store_partial_block() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();

        // File size: 1.5 blocks
        let file_size = BLOCK_SIZE as u64 + BLOCK_SIZE as u64 / 2;
        let cache = PageCache::new(BLOCK_SIZE, file_size);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/partial.bin", file_size, channel);

                // Read block 0 (full block)
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();

                // Read block 1 (partial block at EOF)
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 1, &cache))
                    .await
                    .unwrap();

                // Block 0 should be full size
                assert_eq!(cache.get(0).unwrap().len(), BLOCK_SIZE);
                // Block 1 should be half size
                assert_eq!(cache.get(1).unwrap().len(), BLOCK_SIZE / 2);

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Complete fetches
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending() {
                        match &req.kind {
                            HostOpKind::FileReadRange { length, .. } => {
                                let data = vec![0xAB; *length as usize];
                                channel_for_host.complete(req.id, data);
                            }
                            other => panic!("Unexpected op: {other:?}"),
                        }
                    }
                }
            }
        }
    }

    /// Test custom block size.
    #[test]
    fn lazy_store_custom_block_size() {
        let channel = test_channel(10);
        let store = LazyBlockStore::with_block_size("/test.bin", 1000, 512, channel);

        assert_eq!(store.block_size(), 512);
        assert_eq!(store.size(), 1000);
    }

    /// Test multiple stores sharing the same `PageCache`.
    #[test]
    fn lazy_store_shared_cache() {
        let exec = Executor::new();
        let channel1 = test_channel(10);
        let channel2 = test_channel(10);
        let channel1_for_host = channel1.clone();
        let channel2_for_host = channel2.clone();

        // Shared cache
        let cache = PageCache::new(BLOCK_SIZE, 8192);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store1 = LazyBlockStore::new("/file1.bin", 4096, channel1);
                let mut store2 = LazyBlockStore::new("/file2.bin", 4096, channel2);

                // Read block 0 from store1
                std::future::poll_fn(|cx| Pin::new(&mut store1).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();

                // Read block 1 from store2
                std::future::poll_fn(|cx| Pin::new(&mut store2).poll_read_block(cx, 1, &cache))
                    .await
                    .unwrap();

                // Both blocks in shared cache
                assert!(cache.contains(0));
                assert!(cache.contains(1));
                assert_eq!(cache.cached_block_count(), 2);

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Complete fetches from both channels
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    // Try both channels
                    if let Some(req) = channel1_for_host.take_pending() {
                        channel1_for_host.complete(req.id, vec![0x11; BLOCK_SIZE]);
                    }
                    if let Some(req) = channel2_for_host.take_pending() {
                        channel2_for_host.complete(req.id, vec![0x22; BLOCK_SIZE]);
                    }
                }
            }
        }
    }

    /// Test flush is no-op for read-only store.
    #[test]
    fn lazy_store_flush_noop() {
        use std::sync::Arc;
        use std::task::{Wake, Waker};

        struct NoopWaker;
        impl Wake for NoopWaker {
            fn wake(self: Arc<Self>) {}
        }

        let channel = test_channel(10);
        let mut store = LazyBlockStore::new("/test.txt", 100, channel);

        let waker = Waker::from(Arc::new(NoopWaker));
        let mut cx = Context::from_waker(&waker);

        // Flush should succeed immediately
        let result = Pin::new(&mut store).poll_flush(&mut cx);
        assert!(matches!(result, Poll::Ready(Ok(()))));
    }

    // ========== FETCH COORDINATION TESTS ==========

    /// Test that concurrent readers share a single fetch.
    /// This tests the `PageCache` fetch coordination.
    #[test]
    fn lazy_store_concurrent_readers_share_fetch() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();
        let cache = PageCache::new(BLOCK_SIZE, 4096);

        let fetch_count = Rc::new(RefCell::new(0u64));
        let fetch_count1 = fetch_count.clone();
        let fetch_count2 = fetch_count.clone();

        let channel1 = channel.clone();
        let channel2 = channel.clone();

        // Task 1: reads block 0
        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/shared.bin", 4096, channel1);

                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();

                *fetch_count1.borrow_mut() = store.fetch_count();
                Ok(amla_scheduler::Exit::success())
            }
        });

        // Task 2: also reads block 0 (should wait for task 1's fetch)
        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/shared.bin", 4096, channel2);

                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();

                *fetch_count2.borrow_mut() = store.fetch_count();
                Ok(amla_scheduler::Exit::success())
            }
        });

        // Run until blocked
        let state = exec.run();
        assert!(matches!(state, RunState::Blocked));

        // Should only have ONE pending request (coordinated fetch)
        let req = channel_for_host
            .take_pending()
            .expect("should have one pending");
        assert!(
            channel_for_host.take_pending().is_none(),
            "should not have second pending"
        );

        // Complete the single fetch
        channel_for_host.complete(req.id, vec![0x42; BLOCK_SIZE]);

        // Both tasks should complete
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));

        // One task fetched, the other got it from cache
        // (fetch counts are per-store, but cache is shared)
        let total = *fetch_count.borrow();
        assert!(total <= 2, "Expected at most 2 fetch counts, got {total}");
    }

    // ========== STRESS TESTS ==========

    /// Stress test: Sequential reads with bounded cache causing evictions.
    #[test]
    fn stress_lazy_store_bounded_cache() {
        const NUM_BLOCKS: u64 = 50;
        const MAX_CACHED: usize = 8;

        let exec = Executor::new();
        let channel = test_channel(100);
        let channel_for_host = channel.clone();
        let file_size = NUM_BLOCKS * BLOCK_SIZE as u64;
        let cache = PageCache::with_max_blocks(BLOCK_SIZE, file_size, MAX_CACHED);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/large.bin", file_size, channel);

                // Read all blocks - cache should stay bounded
                for block_num in 0..NUM_BLOCKS {
                    std::future::poll_fn(|cx| {
                        Pin::new(&mut store).poll_read_block(cx, block_num, &cache)
                    })
                    .await
                    .unwrap();

                    // Verify memory bounds after each read
                    let count = cache.cached_block_count();
                    assert!(
                        count <= MAX_CACHED,
                        "Cache exceeded max: {count} > {MAX_CACHED}"
                    );
                }

                assert_eq!(store.fetch_count(), NUM_BLOCKS);

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Complete all fetches
        let mut fetches = 0;
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending() {
                        channel_for_host.complete(req.id, vec![0u8; BLOCK_SIZE]);
                        fetches += 1;
                    }
                }
            }
        }

        assert_eq!(fetches, NUM_BLOCKS);
        assert!(
            cache.eviction_count() > 0,
            "Should have evicted some blocks"
        );
    }

    /// Stress test: Random access pattern with working set.
    #[test]
    fn stress_lazy_store_working_set() {
        const FILE_BLOCKS: u64 = 100;
        const MAX_CACHED: usize = 10;

        let exec = Executor::new();
        let channel = test_channel(100);
        let channel_for_host = channel.clone();
        let file_size = FILE_BLOCKS * BLOCK_SIZE as u64;
        let cache = PageCache::with_max_blocks(BLOCK_SIZE, file_size, MAX_CACHED);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/working.bin", file_size, channel);

                // Access pattern: blocks 0-4 repeatedly (working set)
                // then access blocks 10-14 (new working set)
                let pattern: Vec<u64> = vec![
                    0, 1, 2, 3, 4, 0, 1, 2, 3, 4, 0, 1, 2, 3, 4, // First working set
                    10, 11, 12, 13, 14, 10, 11, 12, 13, 14, // Second working set
                ];

                for block_num in pattern {
                    std::future::poll_fn(|cx| {
                        Pin::new(&mut store).poll_read_block(cx, block_num, &cache)
                    })
                    .await
                    .unwrap();
                }

                // Cache should still be bounded
                assert!(cache.cached_block_count() <= MAX_CACHED);

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Complete all fetches
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending() {
                        channel_for_host.complete(req.id, vec![0u8; BLOCK_SIZE]);
                    }
                }
            }
        }
    }

    /// Stress test: Verify `FileReadRange` parameters are correct.
    #[test]
    fn stress_lazy_store_range_params() {
        const NUM_BLOCKS: u64 = 10;

        let exec = Executor::new();
        let channel = test_channel(100);
        let channel_for_host = channel.clone();

        // Non-aligned file size to test partial final block
        let file_size = (NUM_BLOCKS - 1) * BLOCK_SIZE as u64 + 100;
        let cache = PageCache::new(BLOCK_SIZE, file_size);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/params.bin", file_size, channel);

                for block_num in 0..NUM_BLOCKS {
                    std::future::poll_fn(|cx| {
                        Pin::new(&mut store).poll_read_block(cx, block_num, &cache)
                    })
                    .await
                    .unwrap();
                }

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Verify each request has correct parameters
        let mut block_num = 0u64;
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending() {
                        match &req.kind {
                            HostOpKind::FileReadRange {
                                path,
                                offset,
                                length,
                            } => {
                                assert_eq!(path, "/params.bin");
                                assert_eq!(*offset, block_num * BLOCK_SIZE as u64);

                                // Last block should be partial
                                let expected_len = if block_num == NUM_BLOCKS - 1 {
                                    100
                                } else {
                                    BLOCK_SIZE as u64
                                };
                                assert_eq!(
                                    *length, expected_len,
                                    "Block {block_num} length mismatch"
                                );

                                channel_for_host.complete(req.id, vec![0u8; *length as usize]);
                                block_num += 1;
                            }
                            other => panic!("Unexpected op: {other:?}"),
                        }
                    }
                }
            }
        }
    }

    /// Stress test: Multiple errors don't crash.
    #[test]
    fn stress_lazy_store_multiple_errors() {
        use std::io;

        let exec = Executor::new();
        let channel = test_channel(100);
        let channel_for_host = channel.clone();
        let cache = PageCache::new(BLOCK_SIZE, 4096);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/errors.bin", 4096, channel);

                // Try multiple reads after error
                for _ in 0..10 {
                    let result = std::future::poll_fn(|cx| {
                        Pin::new(&mut store).poll_read_block(cx, 0, &cache)
                    })
                    .await;

                    // All should fail after first error
                    assert!(result.is_err());
                }

                Ok(amla_scheduler::Exit::success())
            }
        });

        // First read blocks
        let state = exec.run();
        assert!(matches!(state, RunState::Blocked));

        // Return error
        let req = channel_for_host.take_pending().unwrap();
        channel_for_host.complete_err(req.id, io::Error::other("test error"));

        // All subsequent reads fail immediately (error state)
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));
    }

    /// Test reading at exact block boundaries.
    #[test]
    fn lazy_store_exact_block_boundaries() {
        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();

        // File size exactly 2 blocks
        let file_size = 2 * BLOCK_SIZE as u64;
        let cache = PageCache::new(BLOCK_SIZE, file_size);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/exact.bin", file_size, channel);

                // Read both blocks
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 1, &cache))
                    .await
                    .unwrap();

                // Read block 2 (exactly at EOF)
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 2, &cache))
                    .await
                    .unwrap();

                // Block 2 should be empty (past EOF)
                assert!(cache.get(2).unwrap().is_empty());

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Complete fetches
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending()
                        && let HostOpKind::FileReadRange { length, .. } = &req.kind
                    {
                        channel_for_host.complete(req.id, vec![0u8; *length as usize]);
                    }
                }
            }
        }
    }

    // ========== ASYNC HOST FILE "CAT" INTEGRATION TESTS ==========
    //
    // These tests simulate reading host files like the "cat" command.
    // They exercise the full async stack: FileHandle → LazyBlockStore → HostChannel.

    /// Integration test: "cat" a small file (< 1 block) from host.
    #[test]
    fn cat_small_host_file() {
        use crate::file_handle::{FileHandle, OpenMode};
        use std::cell::RefCell;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();

        let file_content = b"Hello, host filesystem!";
        let file_size = file_content.len() as u64;

        let result = Rc::new(RefCell::new(Vec::new()));
        let result_clone = result.clone();

        exec.spawn(async move {
            let store = LazyBlockStore::new("/host/message.txt", file_size, channel);
            let cache = PageCache::new(BLOCK_SIZE, file_size);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Read entire file (like "cat")
            let mut buf = vec![0u8; 1024];
            let n = handle.read(&mut buf).await.unwrap();

            *result_clone.borrow_mut() = buf[..n].to_vec();
            Ok(amla_scheduler::Exit::success())
        });

        // Run until blocked
        let state = exec.run();
        assert!(matches!(state, RunState::Blocked));

        // Host provides file content
        let req = channel_for_host.take_pending().unwrap();
        match &req.kind {
            HostOpKind::FileReadRange {
                path,
                offset,
                length,
            } => {
                assert_eq!(path, "/host/message.txt");
                assert_eq!(*offset, 0);
                assert_eq!(*length, file_size);
            }
            other => panic!("Expected FileReadRange, got {other:?}"),
        }
        channel_for_host.complete(req.id, file_content.to_vec());

        // Task completes
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));

        // Verify content
        assert_eq!(result.borrow().as_slice(), file_content);
    }

    /// Integration test: "cat" a multi-block file from host.
    #[test]
    fn cat_large_host_file() {
        use crate::file_handle::{FileHandle, OpenMode};
        use std::cell::RefCell;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();

        // File spans 2.5 blocks
        let file_size = (BLOCK_SIZE * 2 + BLOCK_SIZE / 2) as u64;
        let file_content: Vec<u8> = (0..file_size).map(|i| (i % 256) as u8).collect();
        let file_content_clone = file_content.clone();

        let result = Rc::new(RefCell::new(Vec::new()));
        let result_clone = result.clone();

        exec.spawn(async move {
            let store = LazyBlockStore::new("/host/large.bin", file_size, channel);
            let cache = PageCache::new(BLOCK_SIZE, file_size);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Read entire file in chunks (like buffered cat)
            let mut output = Vec::new();
            loop {
                let mut buf = vec![0u8; 1024];
                let n = handle.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                output.extend_from_slice(&buf[..n]);
            }

            *result_clone.borrow_mut() = output;
            Ok(amla_scheduler::Exit::success())
        });

        // Complete host ops as they come
        let mut blocks_fetched = 0;
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending() {
                        match &req.kind {
                            HostOpKind::FileReadRange { offset, length, .. } => {
                                // Return the correct slice of file content
                                let start = *offset as usize;
                                let end = (start + *length as usize).min(file_content_clone.len());
                                let data = file_content_clone[start..end].to_vec();
                                channel_for_host.complete(req.id, data);
                                blocks_fetched += 1;
                            }
                            other => panic!("Unexpected op: {other:?}"),
                        }
                    }
                }
            }
        }

        // Should have fetched 3 blocks (2 full + 1 partial)
        assert_eq!(blocks_fetched, 3);

        // Verify full content matches
        assert_eq!(result.borrow().as_slice(), file_content.as_slice());
    }

    /// Integration test: "cat" with seek (like "tail -c +100").
    #[test]
    fn cat_with_seek() {
        use crate::file_handle::{FileHandle, OpenMode};
        use std::cell::RefCell;
        use std::io::SeekFrom;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();

        let file_size = 1000u64;

        let result = Rc::new(RefCell::new(Vec::new()));
        let result_clone = result.clone();

        exec.spawn(async move {
            let store = LazyBlockStore::new("/host/data.bin", file_size, channel);
            let cache = PageCache::new(BLOCK_SIZE, file_size);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Seek to position 500
            handle.seek(SeekFrom::Start(500)).unwrap();

            // Read remaining 500 bytes
            let mut buf = vec![0u8; 1000];
            let n = handle.read(&mut buf).await.unwrap();

            *result_clone.borrow_mut() = buf[..n].to_vec();
            Ok(amla_scheduler::Exit::success())
        });

        // Complete host op
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending()
                        && let HostOpKind::FileReadRange { offset, length, .. } = &req.kind
                    {
                        // Generate predictable content
                        let data: Vec<u8> = (*offset..offset + length)
                            .map(|i| (i % 256) as u8)
                            .collect();
                        channel_for_host.complete(req.id, data);
                    }
                }
            }
        }

        // Should have read bytes 500-999
        let expected: Vec<u8> = (500u64..1000).map(|i| (i % 256) as u8).collect();
        assert_eq!(result.borrow().as_slice(), expected.as_slice());
    }

    /// Integration test: "cat" empty file.
    #[test]
    fn cat_empty_host_file() {
        use crate::file_handle::{FileHandle, OpenMode};
        use std::cell::RefCell;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);

        let bytes_read = Rc::new(RefCell::new(0usize));
        let bytes_read_clone = bytes_read.clone();

        exec.spawn(async move {
            let store = LazyBlockStore::new("/host/empty.txt", 0, channel);
            let cache = PageCache::new(BLOCK_SIZE, 0);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Try to read from empty file
            let mut buf = vec![0u8; 1024];
            let n = handle.read(&mut buf).await.unwrap();

            *bytes_read_clone.borrow_mut() = n;
            Ok(amla_scheduler::Exit::success())
        });

        // Should complete immediately (no host ops needed for empty file)
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));
        assert_eq!(*bytes_read.borrow(), 0);
    }

    /// Integration test: "cat" with error from host.
    #[test]
    fn cat_host_error() {
        use crate::file_handle::{FileHandle, OpenMode};
        use std::cell::RefCell;
        use std::io;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();

        let got_error = Rc::new(RefCell::new(false));
        let got_error_clone = got_error.clone();

        exec.spawn(async move {
            let store = LazyBlockStore::new("/host/protected.txt", 1000, channel);
            let cache = PageCache::new(BLOCK_SIZE, 1000);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Try to read
            let mut buf = vec![0u8; 1024];
            let result = handle.read(&mut buf).await;

            *got_error_clone.borrow_mut() = result.is_err();
            Ok(amla_scheduler::Exit::success())
        });

        // Run until blocked
        let state = exec.run();
        assert!(matches!(state, RunState::Blocked));

        // Host returns permission denied
        let req = channel_for_host.take_pending().unwrap();
        channel_for_host.complete_err(
            req.id,
            io::Error::new(io::ErrorKind::PermissionDenied, "access denied"),
        );

        // Task handles error
        let state = exec.run();
        assert!(matches!(state, RunState::Done(_)));
        assert!(*got_error.borrow());
    }

    /// Stress test: "cat" many small files sequentially.
    #[test]
    fn stress_cat_many_files() {
        use crate::file_handle::{FileHandle, OpenMode};
        use std::cell::RefCell;
        use std::rc::Rc;

        const NUM_FILES: usize = 20;

        let exec = Executor::new();
        let channel = test_channel(100);
        let channel_for_host = channel.clone();

        let total_bytes = Rc::new(RefCell::new(0usize));
        let total_bytes_clone = total_bytes.clone();

        exec.spawn(async move {
            let mut total = 0;

            for i in 0..NUM_FILES {
                let path = format!("/host/file{i}.txt");
                let file_size = (i + 1) * 100; // 100, 200, 300, ...

                let store = LazyBlockStore::new(&path, file_size as u64, channel.clone());
                let cache = PageCache::new(BLOCK_SIZE, file_size as u64);
                let handle = FileHandle::new(store, cache, OpenMode::Read);

                // Read entire file
                loop {
                    let mut buf = vec![0u8; 512];
                    let n = handle.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
            }

            *total_bytes_clone.borrow_mut() = total;
            Ok(amla_scheduler::Exit::success())
        });

        // Complete host ops
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending()
                        && let HostOpKind::FileReadRange { length, .. } = &req.kind
                    {
                        channel_for_host.complete(req.id, vec![0x42; *length as usize]);
                    }
                }
            }
        }

        // Total bytes: 100 + 200 + ... + 2000 = sum(1..=20) * 100 = 210 * 100 = 21000
        let expected_total: usize = (1..=NUM_FILES).map(|i| i * 100).sum();
        assert_eq!(*total_bytes.borrow(), expected_total);
    }

    /// Integration test: Interleaved reads from multiple host files.
    #[test]
    fn cat_interleaved_files() {
        use crate::file_handle::{FileHandle, OpenMode};
        use std::cell::RefCell;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();

        let results = Rc::new(RefCell::new((Vec::new(), Vec::new())));
        let results_clone = results.clone();

        exec.spawn(async move {
            let store1 = LazyBlockStore::new("/host/a.txt", 100, channel.clone());
            let cache1 = PageCache::new(BLOCK_SIZE, 100);
            let handle1 = FileHandle::new(store1, cache1, OpenMode::Read);

            let store2 = LazyBlockStore::new("/host/b.txt", 200, channel.clone());
            let cache2 = PageCache::new(BLOCK_SIZE, 200);
            let handle2 = FileHandle::new(store2, cache2, OpenMode::Read);

            // Read alternating chunks
            let mut buf1 = vec![0u8; 50];
            let mut buf2 = vec![0u8; 100];

            let n1 = handle1.read(&mut buf1).await.unwrap();
            let n2 = handle2.read(&mut buf2).await.unwrap();
            let n1b = handle1.read(&mut buf1).await.unwrap();
            let n2b = handle2.read(&mut buf2).await.unwrap();

            let mut r = results_clone.borrow_mut();
            r.0 = vec![n1, n1b];
            r.1 = vec![n2, n2b];

            Ok(amla_scheduler::Exit::success())
        });

        // Complete host ops as they arrive
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending()
                        && let HostOpKind::FileReadRange { path, length, .. } = &req.kind
                    {
                        // Return different patterns for each file
                        let data = if path.contains("a.txt") {
                            vec![b'A'; *length as usize]
                        } else {
                            vec![b'B'; *length as usize]
                        };
                        channel_for_host.complete(req.id, data);
                    }
                }
            }
        }

        let r = results.borrow();
        // File 1: 50 + 50 = 100 bytes total
        assert_eq!(r.0, vec![50, 50]);
        // File 2: 100 + 100 = 200 bytes total
        assert_eq!(r.1, vec![100, 100]);
    }

    // =========================================================================
    // Regression tests for bug fixes
    // =========================================================================

    /// Regression test: `poll_read_block` returns correct block when pending fetch is for different block.
    /// Bug: If block 0 fetch was pending and we polled for block 1, we'd return Ready(Ok(())) for block 1
    /// even though block 1 was never fetched.
    #[test]
    fn poll_read_block_returns_correct_block_with_pending_different() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();
        let file_size = 2 * BLOCK_SIZE as u64;
        let cache = PageCache::new(BLOCK_SIZE, file_size);

        let block0_data = Rc::new(RefCell::new(Vec::new()));
        let block1_data = Rc::new(RefCell::new(Vec::new()));
        let block0_data_clone = block0_data.clone();
        let block1_data_clone = block1_data.clone();

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/test/file.bin", file_size, channel);

                // Request block 0 - this will start a fetch
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 0, &cache))
                    .await
                    .unwrap();

                // Now request block 1 - this should also fetch correctly
                std::future::poll_fn(|cx| Pin::new(&mut store).poll_read_block(cx, 1, &cache))
                    .await
                    .unwrap();

                // Verify each block has correct data
                *block0_data_clone.borrow_mut() = cache.get(0).unwrap();
                *block1_data_clone.borrow_mut() = cache.get(1).unwrap();

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Run and complete fetches with distinct data for each block
        let mut block0_completed = false;
        let mut block1_completed = false;
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending() {
                        match &req.kind {
                            HostOpKind::FileReadRange { offset, length, .. } => {
                                let block_num = offset / BLOCK_SIZE as u64;
                                // Return distinct data for each block so we can verify
                                let data = if block_num == 0 {
                                    block0_completed = true;
                                    vec![0xAA; *length as usize]
                                } else {
                                    block1_completed = true;
                                    vec![0xBB; *length as usize]
                                };
                                channel_for_host.complete(req.id, data);
                            }
                            other => panic!("Unexpected op: {other:?}"),
                        }
                    }
                }
            }
        }

        // Both blocks should have been fetched
        assert!(block0_completed, "Block 0 should have been fetched");
        assert!(block1_completed, "Block 1 should have been fetched");

        // Verify each block has the correct distinct data
        let b0 = block0_data.borrow();
        let b1 = block1_data.borrow();
        assert!(
            b0.iter().all(|&b| b == 0xAA),
            "Block 0 should contain 0xAA bytes"
        );
        assert!(
            b1.iter().all(|&b| b == 0xBB),
            "Block 1 should contain 0xBB bytes"
        );
    }

    /// Regression test: Interleaved block requests don't return wrong data.
    /// Tests the scenario where two different blocks are requested in quick succession.
    #[test]
    fn poll_read_block_interleaved_requests() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let exec = Executor::new();
        let channel = test_channel(10);
        let channel_for_host = channel.clone();
        let file_size = 4 * BLOCK_SIZE as u64;
        let cache = PageCache::new(BLOCK_SIZE, file_size);

        let results = Rc::new(RefCell::new(Vec::new()));
        let results_clone = results.clone();

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = LazyBlockStore::new("/test/multi.bin", file_size, channel);

                // Request blocks in non-sequential order
                for block_num in [2, 0, 3, 1] {
                    std::future::poll_fn(|cx| {
                        Pin::new(&mut store).poll_read_block(cx, block_num, &cache)
                    })
                    .await
                    .unwrap();
                }

                // Collect block data
                let mut data = Vec::new();
                for block_num in 0..4 {
                    let block = cache.get(block_num).unwrap();
                    data.push((block_num, block[0])); // Just check first byte
                }
                *results_clone.borrow_mut() = data;

                Ok(amla_scheduler::Exit::success())
            }
        });

        // Complete fetches with block-number-specific data
        loop {
            let state = exec.run();
            match state {
                RunState::Done(_) => break,
                RunState::Blocked => {
                    if let Some(req) = channel_for_host.take_pending()
                        && let HostOpKind::FileReadRange { offset, length, .. } = &req.kind
                    {
                        let block_num = offset / BLOCK_SIZE as u64;
                        // Each block filled with its block number as byte value
                        #[allow(clippy::cast_possible_truncation)]
                        let data = vec![block_num as u8; *length as usize];
                        channel_for_host.complete(req.id, data);
                    }
                }
            }
        }

        // Verify each block has the correct data (block N contains byte value N)
        let r = results.borrow();
        for (block_num, first_byte) in r.iter() {
            assert_eq!(
                *first_byte,
                u8::try_from(*block_num).unwrap(),
                "Block {block_num} should contain byte value {block_num}"
            );
        }
    }
}
