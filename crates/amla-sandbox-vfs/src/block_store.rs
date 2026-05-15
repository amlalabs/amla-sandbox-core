//! Block-level storage abstraction.
//!
//! Provides a trait for block-oriented file access, with implementations
//! for in-memory files and mounted (host) files.

use std::cell::RefCell;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::VfsError;
use crate::page_cache::PageCache;

/// Block size constant (4KB pages)
pub const BLOCK_SIZE: usize = 4096;

/// Block-level storage trait.
///
/// Abstracts reading/writing files as blocks. Different implementations
/// handle in-memory vs mounted files.
pub trait BlockStore {
    /// Block size in bytes.
    fn block_size(&self) -> usize;

    /// Total file size in bytes.
    fn size(&self) -> u64;

    /// Ensure a block is in the cache.
    ///
    /// For in-memory files, this is instant.
    /// For mounted files, this may issue a host op and return Pending.
    fn poll_read_block(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        block_num: u64,
        cache: &PageCache,
    ) -> Poll<Result<(), VfsError>>;

    /// Write a block to storage.
    ///
    /// For in-memory files, this updates the backing buffer.
    /// For mounted files, this may write through or be cached.
    fn poll_write_block(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        block_num: u64,
        data: &[u8],
        cache: &PageCache,
    ) -> Poll<Result<(), VfsError>>;

    /// Flush any pending writes.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), VfsError>>;
}

/// In-memory block store.
///
/// Backed by a `Vec<u8>`. All operations are instant (no async).
pub struct MemoryBlockStore {
    /// File data
    data: RefCell<Vec<u8>>,
    /// Block size
    block_size: usize,
}

impl MemoryBlockStore {
    /// Create from existing data.
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data: RefCell::new(data),
            block_size: BLOCK_SIZE,
        }
    }

    /// Create with custom block size (for testing).
    pub fn with_block_size(data: Vec<u8>, block_size: usize) -> Self {
        Self {
            data: RefCell::new(data),
            block_size,
        }
    }

    /// Create empty.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Get reference to underlying data.
    pub fn data(&self) -> std::cell::Ref<'_, Vec<u8>> {
        self.data.borrow()
    }

    /// Get mutable reference to underlying data.
    pub fn data_mut(&self) -> std::cell::RefMut<'_, Vec<u8>> {
        self.data.borrow_mut()
    }
}

impl BlockStore for MemoryBlockStore {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn size(&self) -> u64 {
        self.data.borrow().len() as u64
    }

    fn poll_read_block(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        block_num: u64,
        cache: &PageCache,
    ) -> Poll<Result<(), VfsError>> {
        // Check if already cached
        if cache.get(block_num).is_some() {
            return Poll::Ready(Ok(()));
        }

        // Read block from memory
        let data = self.data.borrow();
        let start = (block_num as usize) * self.block_size;

        if start >= data.len() {
            // Past EOF - insert empty block
            cache.insert(block_num, Vec::new());
        } else {
            let end = (start + self.block_size).min(data.len());
            cache.insert(block_num, data[start..end].to_vec());
        }

        Poll::Ready(Ok(()))
    }

    fn poll_write_block(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        block_num: u64,
        block_data: &[u8],
        cache: &PageCache,
    ) -> Poll<Result<(), VfsError>> {
        let this = self.get_mut();
        let mut data = this.data.borrow_mut();

        let start = (block_num as usize) * this.block_size;
        let end = start + block_data.len();

        // Extend file if needed
        if end > data.len() {
            data.resize(end, 0);
        }

        // Write to backing store
        data[start..end].copy_from_slice(block_data);

        // Update cache
        cache.insert(block_num, block_data.to_vec());

        // Update file size in cache
        cache.set_file_size(data.len() as u64);

        Poll::Ready(Ok(()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), VfsError>> {
        // Memory store is always "flushed"
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amla_scheduler::Executor;

    #[test]
    fn memory_store_read_block() {
        let exec = Executor::new();
        let data = b"hello world, this is a test of block storage".to_vec();
        let cache = PageCache::new(16, data.len() as u64);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = MemoryBlockStore::with_block_size(
                    b"hello world, this is a test of block storage".to_vec(),
                    16,
                );

                // Read block 0
                ReadBlockFut::new(&mut store, 0, &cache).await.unwrap();
                assert_eq!(cache.get(0).unwrap(), b"hello world, thi");

                // Read block 1
                ReadBlockFut::new(&mut store, 1, &cache).await.unwrap();
                assert_eq!(cache.get(1).unwrap(), b"s is a test of b");

                // Read block 2
                ReadBlockFut::new(&mut store, 2, &cache).await.unwrap();
                assert_eq!(cache.get(2).unwrap(), b"lock storage");

                Ok(amla_scheduler::Exit::success())
            }
        });

        let _ = exec.run();
    }

    #[test]
    fn memory_store_read_past_eof() {
        let exec = Executor::new();
        let cache = PageCache::new(16, 5);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = MemoryBlockStore::with_block_size(b"short".to_vec(), 16);

                // Block 0 has the data
                ReadBlockFut::new(&mut store, 0, &cache).await.unwrap();
                assert_eq!(cache.get(0).unwrap(), b"short");

                // Block 1 is past EOF
                ReadBlockFut::new(&mut store, 1, &cache).await.unwrap();
                assert!(cache.get(1).unwrap().is_empty());

                Ok(amla_scheduler::Exit::success())
            }
        });

        let _ = exec.run();
    }

    #[test]
    fn memory_store_write_block() {
        let exec = Executor::new();
        let cache = PageCache::new(16, 0);

        exec.spawn({
            let cache = cache.clone();
            async move {
                let mut store = MemoryBlockStore::with_block_size(Vec::new(), 16);

                // Write block 0
                WriteBlockFut::new(&mut store, 0, b"first block data", &cache)
                    .await
                    .unwrap();
                assert_eq!(store.data().as_slice(), b"first block data");
                assert_eq!(cache.get(0).unwrap(), b"first block data");
                assert_eq!(cache.file_size(), 16);

                // Write block 2 (creates gap)
                WriteBlockFut::new(&mut store, 2, b"third block!!!!!", &cache)
                    .await
                    .unwrap();
                assert_eq!(store.size(), 48); // 3 blocks (16 bytes each)

                Ok(amla_scheduler::Exit::success())
            }
        });

        let _ = exec.run();
    }

    #[test]
    fn memory_store_size() {
        let data = vec![0u8; 100];
        let store = MemoryBlockStore::new(data);
        assert_eq!(store.size(), 100);
    }

    #[test]
    fn memory_store_empty() {
        let store = MemoryBlockStore::empty();
        assert_eq!(store.size(), 0);
    }

    // Helper futures for testing
    use std::future::Future;

    struct ReadBlockFut<'a> {
        store: &'a mut MemoryBlockStore,
        block_num: u64,
        cache: &'a PageCache,
    }

    impl<'a> ReadBlockFut<'a> {
        fn new(store: &'a mut MemoryBlockStore, block_num: u64, cache: &'a PageCache) -> Self {
            Self {
                store,
                block_num,
                cache,
            }
        }
    }

    impl Future for ReadBlockFut<'_> {
        type Output = Result<(), crate::VfsError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = &mut *self;
            Pin::new(&mut *this.store).poll_read_block(cx, this.block_num, this.cache)
        }
    }

    struct WriteBlockFut<'a> {
        store: &'a mut MemoryBlockStore,
        block_num: u64,
        data: &'a [u8],
        cache: &'a PageCache,
    }

    impl<'a> WriteBlockFut<'a> {
        fn new(
            store: &'a mut MemoryBlockStore,
            block_num: u64,
            data: &'a [u8],
            cache: &'a PageCache,
        ) -> Self {
            Self {
                store,
                block_num,
                data,
                cache,
            }
        }
    }

    impl Future for WriteBlockFut<'_> {
        type Output = Result<(), crate::VfsError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = &mut *self;
            Pin::new(&mut *this.store).poll_write_block(cx, this.block_num, this.data, this.cache)
        }
    }
}
