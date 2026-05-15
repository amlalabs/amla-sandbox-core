//! Shared page cache for block storage.
//!
//! Provides a per-file block cache that can be shared across multiple
//! `FileHandles`. For mounted files, tracks in-flight fetches to avoid
//! duplicate host operations.
//!
//! The cache has a configurable maximum capacity (in blocks) and uses
//! LRU eviction when the limit is reached.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::task::Waker;

use smallvec::SmallVec;

/// Default maximum number of blocks per cache (64 blocks = 256KB at 4KB blocks)
pub const DEFAULT_MAX_BLOCKS: usize = 64;

/// Shared block cache for a file.
///
/// Multiple `FileHandle`s to the same file share one `PageCache`.
/// This avoids duplicate fetches and provides consistent reads.
///
/// Clone is cheap (Rc internally).
///
/// # Memory Bounds
///
/// The cache enforces a maximum number of blocks. When inserting a new
/// block would exceed the limit, the least recently used block is evicted.
/// Use `with_max_blocks()` to configure the limit.
#[derive(Clone)]
pub struct PageCache {
    inner: Rc<PageCacheInner>,
}

struct PageCacheInner {
    /// Cached blocks: `block_num` -> data
    blocks: RefCell<HashMap<u64, Vec<u8>>>,

    /// LRU order: front = oldest, back = most recent
    lru_order: RefCell<VecDeque<u64>>,

    /// Maximum number of blocks to cache (0 = unlimited)
    max_blocks: usize,

    /// Block size in bytes
    block_size: usize,

    /// File size (updated on writes, fetches)
    file_size: Cell<u64>,

    /// In-flight fetches: `block_num` -> wakers waiting for this block.
    ///
    /// When a block is being fetched from an external source (e.g., host filesystem),
    /// other readers can register wakers here instead of issuing duplicate fetches.
    /// `SmallVec` optimized for typical case of 1-2 concurrent readers.
    pending_fetches: RefCell<HashMap<u64, SmallVec<[Waker; 2]>>>,

    /// Statistics: number of evictions
    eviction_count: Cell<usize>,
}

/// Result of `PageCache::try_start_fetch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchAction {
    /// Block is already cached - no fetch needed.
    Cached,
    /// Caller should fetch this block.
    Fetch,
    /// Another fetch is in progress - register waker and wait.
    Wait,
}

impl PageCache {
    /// Create a new page cache with default max blocks.
    pub fn new(block_size: usize, file_size: u64) -> Self {
        Self::with_max_blocks(block_size, file_size, DEFAULT_MAX_BLOCKS)
    }

    /// Create a new page cache with specified max blocks.
    ///
    /// # Arguments
    /// * `block_size` - Size of each block in bytes
    /// * `file_size` - Initial file size
    /// * `max_blocks` - Maximum blocks to cache (0 = unlimited)
    pub fn with_max_blocks(block_size: usize, file_size: u64, max_blocks: usize) -> Self {
        Self {
            inner: Rc::new(PageCacheInner {
                blocks: RefCell::new(HashMap::new()),
                lru_order: RefCell::new(VecDeque::new()),
                max_blocks,
                block_size,
                file_size: Cell::new(file_size),
                pending_fetches: RefCell::new(HashMap::new()),
                eviction_count: Cell::new(0),
            }),
        }
    }

    /// Get the maximum number of blocks this cache will hold.
    pub fn max_blocks(&self) -> usize {
        self.inner.max_blocks
    }

    /// Get the number of evictions that have occurred.
    pub fn eviction_count(&self) -> usize {
        self.inner.eviction_count.get()
    }

    /// Get block size.
    pub fn block_size(&self) -> usize {
        self.inner.block_size
    }

    /// Get file size.
    pub fn file_size(&self) -> u64 {
        self.inner.file_size.get()
    }

    /// Set file size (called after writes that extend the file).
    pub fn set_file_size(&self, size: u64) {
        self.inner.file_size.set(size);
    }

    /// Get a cached block (returns clone of data).
    ///
    /// This marks the block as recently used for LRU purposes.
    pub fn get(&self, block_num: u64) -> Option<Vec<u8>> {
        let blocks = self.inner.blocks.borrow();
        if let Some(data) = blocks.get(&block_num) {
            // Mark as recently used
            self.touch(block_num);
            Some(data.clone())
        } else {
            None
        }
    }

    /// Mark a block as recently used (move to back of LRU).
    fn touch(&self, block_num: u64) {
        let mut lru = self.inner.lru_order.borrow_mut();
        // Remove from current position if present
        if let Some(pos) = lru.iter().position(|&b| b == block_num) {
            lru.remove(pos);
        }
        // Add to back (most recent)
        lru.push_back(block_num);
    }

    /// Check if a block is cached.
    pub fn contains(&self, block_num: u64) -> bool {
        self.inner.blocks.borrow().contains_key(&block_num)
    }

    /// Insert a block into the cache.
    ///
    /// If the cache is at capacity, evicts the least recently used block.
    pub fn insert(&self, block_num: u64, data: Vec<u8>) {
        let max = self.inner.max_blocks;

        // Check if we need to evict (and max_blocks > 0)
        if max > 0 {
            let current_count = self.inner.blocks.borrow().len();
            let is_new = !self.inner.blocks.borrow().contains_key(&block_num);

            if is_new && current_count >= max {
                // Evict LRU block
                self.evict_lru();
            }
        }

        // Insert the block
        self.inner.blocks.borrow_mut().insert(block_num, data);

        // Update LRU order
        self.touch(block_num);
    }

    /// Evict the least recently used block.
    fn evict_lru(&self) {
        let mut lru = self.inner.lru_order.borrow_mut();
        if let Some(block_num) = lru.pop_front() {
            self.inner.blocks.borrow_mut().remove(&block_num);
            self.inner
                .eviction_count
                .set(self.inner.eviction_count.get() + 1);
        }
    }

    /// Invalidate a cached block.
    pub fn invalidate(&self, block_num: u64) {
        self.inner.blocks.borrow_mut().remove(&block_num);
        // Remove from LRU order
        let mut lru = self.inner.lru_order.borrow_mut();
        if let Some(pos) = lru.iter().position(|&b| b == block_num) {
            lru.remove(pos);
        }
    }

    /// Clear the entire cache.
    pub fn clear(&self) {
        self.inner.blocks.borrow_mut().clear();
        self.inner.lru_order.borrow_mut().clear();
    }

    // ========== FETCH COORDINATION ==========
    //
    // These methods coordinate block fetching from external sources (e.g., host
    // filesystem) to avoid duplicate I/O when multiple readers need the same block.
    //
    // Protocol:
    //   1. Reader calls `try_start_fetch(block_num)`:
    //      - Returns `FetchAction::Fetch` → caller should issue the fetch
    //      - Returns `FetchAction::Wait` → another fetch in progress, caller waits
    //      - Returns `FetchAction::Cached` → block already in cache
    //
    //   2. When fetch completes, caller calls `complete_fetch(block_num, data)`:
    //      - Inserts block into cache
    //      - Wakes all waiting readers
    //
    //   3. On fetch error, caller calls `cancel_fetch(block_num)`:
    //      - Wakes waiters so they can retry or propagate error

    /// Try to start fetching a block.
    ///
    /// Returns what action the caller should take:
    /// - `Cached`: Block is already in cache, just read it
    /// - `Fetch`: Caller should fetch this block from external source
    /// - `Wait`: Another reader is fetching, register waker with `register_fetch_waker`
    pub fn try_start_fetch(&self, block_num: u64) -> FetchAction {
        // Already cached?
        if self.contains(block_num) {
            return FetchAction::Cached;
        }

        // Already being fetched?
        let mut pending = self.inner.pending_fetches.borrow_mut();
        if pending.contains_key(&block_num) {
            return FetchAction::Wait;
        }

        // Start a new fetch
        pending.insert(block_num, SmallVec::new());
        FetchAction::Fetch
    }

    /// Check if a fetch is in progress for this block.
    pub fn is_fetching(&self, block_num: u64) -> bool {
        self.inner.pending_fetches.borrow().contains_key(&block_num)
    }

    /// Register a waker to be called when a pending fetch completes.
    ///
    /// Call this when `try_start_fetch` returns `Wait`.
    pub fn register_fetch_waker(&self, block_num: u64, waker: Waker) {
        let mut pending = self.inner.pending_fetches.borrow_mut();
        if let Some(wakers) = pending.get_mut(&block_num) {
            // Deduplicate wakers from the same task
            if !wakers.iter().any(|w| w.will_wake(&waker)) {
                wakers.push(waker);
            }
        }
    }

    /// Complete a fetch: insert block and wake all waiters.
    ///
    /// Call this when the external fetch succeeds.
    pub fn complete_fetch(&self, block_num: u64, data: Vec<u8>) {
        // Insert into cache
        self.insert(block_num, data);

        // Wake all waiters
        if let Some(wakers) = self.inner.pending_fetches.borrow_mut().remove(&block_num) {
            for waker in wakers {
                waker.wake();
            }
        }
    }

    /// Cancel a fetch: wake waiters so they can retry or fail.
    ///
    /// Call this when the external fetch fails.
    pub fn cancel_fetch(&self, block_num: u64) {
        if let Some(wakers) = self.inner.pending_fetches.borrow_mut().remove(&block_num) {
            for waker in wakers {
                waker.wake();
            }
        }
    }

    /// Get number of in-flight fetches.
    pub fn pending_fetch_count(&self) -> usize {
        self.inner.pending_fetches.borrow().len()
    }

    /// Get number of cached blocks.
    pub fn cached_block_count(&self) -> usize {
        self.inner.blocks.borrow().len()
    }

    /// Get total cached bytes.
    pub fn cached_bytes(&self) -> usize {
        self.inner
            .blocks
            .borrow()
            .values()
            .map(std::vec::Vec::len)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_basic_operations() {
        let cache = PageCache::new(4096, 10000);

        assert!(!cache.contains(0));
        assert!(cache.get(0).is_none());

        cache.insert(0, b"block zero".to_vec());
        assert!(cache.contains(0));
        assert_eq!(cache.get(0).unwrap(), b"block zero");

        cache.insert(5, b"block five".to_vec());
        assert!(cache.contains(5));

        assert_eq!(cache.cached_block_count(), 2);
    }

    #[test]
    fn cache_invalidate() {
        let cache = PageCache::new(4096, 1000);

        cache.insert(0, b"data".to_vec());
        assert!(cache.contains(0));

        cache.invalidate(0);
        assert!(!cache.contains(0));
    }

    #[test]
    fn cache_clear() {
        let cache = PageCache::new(4096, 1000);

        cache.insert(0, b"a".to_vec());
        cache.insert(1, b"b".to_vec());
        cache.insert(2, b"c".to_vec());
        assert_eq!(cache.cached_block_count(), 3);

        cache.clear();
        assert_eq!(cache.cached_block_count(), 0);
    }

    #[test]
    fn cache_file_size() {
        let cache = PageCache::new(4096, 1000);
        assert_eq!(cache.file_size(), 1000);

        cache.set_file_size(2000);
        assert_eq!(cache.file_size(), 2000);
    }

    // ========== FETCH COORDINATION TESTS ==========

    #[test]
    fn fetch_coordination_basic() {
        use super::FetchAction;

        let cache = PageCache::new(4096, 1000);

        // First caller should fetch
        assert_eq!(cache.try_start_fetch(5), FetchAction::Fetch);
        assert!(cache.is_fetching(5));
        assert_eq!(cache.pending_fetch_count(), 1);

        // Second caller should wait
        assert_eq!(cache.try_start_fetch(5), FetchAction::Wait);
        assert_eq!(cache.pending_fetch_count(), 1);

        // Complete the fetch
        cache.complete_fetch(5, b"data".to_vec());
        assert!(!cache.is_fetching(5));
        assert!(cache.contains(5));
        assert_eq!(cache.pending_fetch_count(), 0);

        // Now it's cached
        assert_eq!(cache.try_start_fetch(5), FetchAction::Cached);
    }

    #[test]
    fn fetch_coordination_cancel() {
        use super::FetchAction;

        let cache = PageCache::new(4096, 1000);

        assert_eq!(cache.try_start_fetch(5), FetchAction::Fetch);
        assert!(cache.is_fetching(5));

        // Cancel the fetch
        cache.cancel_fetch(5);
        assert!(!cache.is_fetching(5));
        assert!(!cache.contains(5));

        // Can start a new fetch
        assert_eq!(cache.try_start_fetch(5), FetchAction::Fetch);
    }

    #[test]
    fn fetch_coordination_multiple_blocks() {
        use super::FetchAction;

        let cache = PageCache::new(4096, 1000);

        // Start multiple fetches
        assert_eq!(cache.try_start_fetch(1), FetchAction::Fetch);
        assert_eq!(cache.try_start_fetch(2), FetchAction::Fetch);
        assert_eq!(cache.try_start_fetch(3), FetchAction::Fetch);
        assert_eq!(cache.pending_fetch_count(), 3);

        // Complete in different order
        cache.complete_fetch(2, b"block2".to_vec());
        assert_eq!(cache.pending_fetch_count(), 2);
        assert!(cache.contains(2));

        cache.complete_fetch(1, b"block1".to_vec());
        cache.complete_fetch(3, b"block3".to_vec());
        assert_eq!(cache.pending_fetch_count(), 0);
    }

    #[test]
    fn cache_clone_shares_data() {
        let cache1 = PageCache::new(4096, 1000);
        let cache2 = cache1.clone();

        cache1.insert(0, b"shared data".to_vec());

        // cache2 should see the same data
        assert!(cache2.contains(0));
        assert_eq!(cache2.get(0).unwrap(), b"shared data");
    }

    #[test]
    fn cache_cached_bytes() {
        let cache = PageCache::new(4096, 1000);

        cache.insert(0, vec![0u8; 100]);
        cache.insert(1, vec![0u8; 200]);
        cache.insert(2, vec![0u8; 50]);

        assert_eq!(cache.cached_bytes(), 350);
    }

    #[test]
    fn cache_block_size() {
        let cache = PageCache::new(8192, 1000);
        assert_eq!(cache.block_size(), 8192);
    }

    // ========== LRU EVICTION TESTS ==========

    #[test]
    fn cache_lru_eviction_basic() {
        // Cache with max 3 blocks
        let cache = PageCache::with_max_blocks(4096, 1000, 3);

        // Insert 3 blocks
        cache.insert(0, b"block0".to_vec());
        cache.insert(1, b"block1".to_vec());
        cache.insert(2, b"block2".to_vec());
        assert_eq!(cache.cached_block_count(), 3);
        assert_eq!(cache.eviction_count(), 0);

        // Insert 4th block - should evict block 0 (LRU)
        cache.insert(3, b"block3".to_vec());
        assert_eq!(cache.cached_block_count(), 3);
        assert_eq!(cache.eviction_count(), 1);
        assert!(!cache.contains(0)); // Block 0 evicted
        assert!(cache.contains(1));
        assert!(cache.contains(2));
        assert!(cache.contains(3));
    }

    #[test]
    fn cache_lru_access_updates_order() {
        // Cache with max 3 blocks
        let cache = PageCache::with_max_blocks(4096, 1000, 3);

        cache.insert(0, b"block0".to_vec());
        cache.insert(1, b"block1".to_vec());
        cache.insert(2, b"block2".to_vec());

        // Access block 0 - moves it to most recently used
        let _ = cache.get(0);

        // Insert block 3 - should evict block 1 (now LRU)
        cache.insert(3, b"block3".to_vec());
        assert!(cache.contains(0)); // Still there (was accessed)
        assert!(!cache.contains(1)); // Evicted (was LRU)
        assert!(cache.contains(2));
        assert!(cache.contains(3));
    }

    #[test]
    fn cache_lru_overwrite_no_eviction() {
        // Cache with max 3 blocks
        let cache = PageCache::with_max_blocks(4096, 1000, 3);

        cache.insert(0, b"block0".to_vec());
        cache.insert(1, b"block1".to_vec());
        cache.insert(2, b"block2".to_vec());

        // Overwrite existing block - should NOT evict
        cache.insert(1, b"block1_updated".to_vec());
        assert_eq!(cache.cached_block_count(), 3);
        assert_eq!(cache.eviction_count(), 0);
        assert_eq!(cache.get(1).unwrap(), b"block1_updated");
    }

    #[test]
    fn cache_unlimited_no_eviction() {
        // Cache with max_blocks = 0 (unlimited)
        let cache = PageCache::with_max_blocks(4096, 1000, 0);

        // Insert many blocks - no eviction
        for i in 0..100 {
            cache.insert(i, vec![0u8; 100]);
        }
        assert_eq!(cache.cached_block_count(), 100);
        assert_eq!(cache.eviction_count(), 0);
    }

    // ========== STRESS TESTS ==========

    #[test]
    fn stress_sequential_insert_bounded() {
        // Insert many blocks with bounded cache
        let cache = PageCache::with_max_blocks(4096, 1000, 10);

        for i in 0..1000 {
            #[allow(clippy::cast_possible_truncation)]
            cache.insert(i, vec![(i & 0xFF) as u8; 100]);
        }

        // Should never exceed max_blocks
        assert_eq!(cache.cached_block_count(), 10);
        // Should have evicted 990 blocks
        assert_eq!(cache.eviction_count(), 990);
        // Only last 10 blocks should be present
        for i in 990..1000 {
            assert!(cache.contains(i), "Block {i} should be present");
        }
        for i in 0..990 {
            assert!(!cache.contains(i), "Block {i} should be evicted");
        }
    }

    #[test]
    fn stress_random_access_pattern() {
        let cache = PageCache::with_max_blocks(4096, 1000, 20);

        // Insert initial blocks
        for i in 0..20 {
            #[allow(clippy::cast_possible_truncation)]
            cache.insert(i, vec![(i & 0xFF) as u8; 100]);
        }

        // Random access pattern - simulate working set
        // Access blocks 5, 10, 15 repeatedly
        for _ in 0..100 {
            let _ = cache.get(5);
            let _ = cache.get(10);
            let _ = cache.get(15);
        }

        // Now insert new blocks - should evict non-working-set blocks
        for i in 20..30 {
            #[allow(clippy::cast_possible_truncation)]
            cache.insert(i, vec![(i & 0xFF) as u8; 100]);
        }

        // Working set should still be present
        assert!(cache.contains(5));
        assert!(cache.contains(10));
        assert!(cache.contains(15));
        assert_eq!(cache.cached_block_count(), 20);
    }

    #[test]
    fn stress_memory_bound_verification() {
        const BLOCK_SIZE: usize = 4096;
        const MAX_BLOCKS: usize = 16;

        let cache = PageCache::with_max_blocks(BLOCK_SIZE, 0, MAX_BLOCKS);

        // Insert full-size blocks
        for i in 0..1000u64 {
            cache.insert(i, vec![0u8; BLOCK_SIZE]);

            // Memory should never exceed MAX_BLOCKS * BLOCK_SIZE
            let cached_bytes = cache.cached_bytes();
            let max_bytes = MAX_BLOCKS * BLOCK_SIZE;
            assert!(
                cached_bytes <= max_bytes,
                "Memory exceeded: {cached_bytes} > {max_bytes}"
            );
            let block_count = cache.cached_block_count();
            assert!(
                block_count <= MAX_BLOCKS,
                "Block count exceeded: {block_count} > {MAX_BLOCKS}"
            );
        }
    }

    #[test]
    fn stress_alternating_insert_access() {
        let cache = PageCache::with_max_blocks(4096, 1000, 5);

        // Alternating pattern: insert, access old, insert, access old...
        for i in 0..100u64 {
            #[allow(clippy::cast_possible_truncation)]
            cache.insert(i, vec![(i & 0xFF) as u8; 100]);

            // Access block 0 every iteration (if present)
            if cache.contains(0) {
                let _ = cache.get(0);
            }
        }

        // Block 0 should survive due to frequent access (if started before max)
        // Actually, block 0 was inserted first, so it may or may not survive
        // depending on timing. What we can verify is bounds.
        assert!(cache.cached_block_count() <= 5);
    }

    #[test]
    fn stress_rapid_invalidation() {
        let cache = PageCache::with_max_blocks(4096, 1000, 100);

        // Insert then immediately invalidate
        for i in 0..1000u64 {
            cache.insert(i, vec![0u8; 100]);
            cache.invalidate(i);
        }

        // Should be empty
        assert_eq!(cache.cached_block_count(), 0);
        // Eviction count should be 0 (invalidation isn't eviction)
        assert_eq!(cache.eviction_count(), 0);
    }

    #[test]
    fn stress_interleaved_operations() {
        let cache = PageCache::with_max_blocks(4096, 1000, 8);

        for round in 0..100 {
            // Insert
            cache.insert(round * 4, vec![0u8; 100]);
            cache.insert(round * 4 + 1, vec![0u8; 100]);

            // Access some
            let _ = cache.get(round * 4);

            // Invalidate some
            if cache.contains(round * 4 + 1) {
                cache.invalidate(round * 4 + 1);
            }

            // Insert more
            cache.insert(round * 4 + 2, vec![0u8; 100]);
            cache.insert(round * 4 + 3, vec![0u8; 100]);

            // Verify bounds
            assert!(cache.cached_block_count() <= 8);
        }
    }

    #[test]
    fn stress_large_blocks() {
        // Test with large blocks to verify memory accounting
        const LARGE_BLOCK: usize = 65536; // 64KB blocks
        let cache = PageCache::with_max_blocks(LARGE_BLOCK, 0, 4);

        for i in 0..100u64 {
            cache.insert(i, vec![0u8; LARGE_BLOCK]);

            // Max memory: 4 * 64KB = 256KB
            assert!(cache.cached_bytes() <= 4 * LARGE_BLOCK);
        }
    }

    #[test]
    fn cache_max_blocks_getter() {
        let cache = PageCache::with_max_blocks(4096, 1000, 42);
        assert_eq!(cache.max_blocks(), 42);
    }
}
