//! Byte-level file handle with position tracking.
//!
//! `FileHandle` wraps a `BlockStore` and `PageCache`, providing byte-oriented
//! reads/writes with automatic block management.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io::SeekFrom;
use std::pin::Pin;
use std::task::{Context, Poll};

use amla_scheduler::{AsyncRead, AsyncWrite, Error as SchedulerError};

use crate::VfsError;
use crate::block_store::BlockStore;
use crate::page_cache::PageCache;

/// File open mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Read-only
    Read,
    /// Write-only (truncates)
    Write,
    /// Read-write
    ReadWrite,
    /// Append (writes go to end)
    Append,
}

impl OpenMode {
    /// Can this mode read?
    pub fn can_read(&self) -> bool {
        matches!(self, OpenMode::Read | OpenMode::ReadWrite)
    }

    /// Can this mode write?
    pub fn can_write(&self) -> bool {
        matches!(
            self,
            OpenMode::Write | OpenMode::ReadWrite | OpenMode::Append
        )
    }
}

/// Byte-level file handle.
///
/// Provides streaming read/write with automatic position tracking
/// and block-level caching.
pub struct FileHandle<S: BlockStore> {
    /// Block store for this file
    store: RefCell<S>,

    /// Shared page cache
    cache: PageCache,

    /// Current position in file
    pos: Cell<u64>,

    /// Open mode
    mode: OpenMode,
}

impl<S: BlockStore> FileHandle<S> {
    /// Create a new file handle.
    pub fn new(store: S, cache: PageCache, mode: OpenMode) -> Self {
        let pos = if mode == OpenMode::Append {
            cache.file_size()
        } else {
            0
        };

        Self {
            store: RefCell::new(store),
            cache,
            pos: Cell::new(pos),
            mode,
        }
    }

    /// Get current position.
    pub fn position(&self) -> u64 {
        self.pos.get()
    }

    /// Get file size.
    pub fn size(&self) -> u64 {
        self.cache.file_size()
    }

    /// Seek to a new position.
    pub fn seek(&self, from: SeekFrom) -> Result<u64, VfsError> {
        let file_size = self.cache.file_size();
        let current = self.pos.get();

        let new_pos = match from {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => {
                if n >= 0 {
                    file_size.saturating_add(n as u64)
                } else {
                    file_size.saturating_sub((-n) as u64)
                }
            }
            SeekFrom::Current(n) => {
                if n >= 0 {
                    current.saturating_add(n as u64)
                } else {
                    current.saturating_sub((-n) as u64)
                }
            }
        };

        self.pos.set(new_pos);
        Ok(new_pos)
    }

    /// Check if at end of file.
    pub fn is_eof(&self) -> bool {
        self.pos.get() >= self.cache.file_size()
    }

    /// Get the open mode.
    pub fn mode(&self) -> OpenMode {
        self.mode
    }

    /// Read bytes at current position.
    ///
    /// Returns a future that resolves when data is available.
    pub fn read<'a>(&'a self, buf: &'a mut [u8]) -> ReadFuture<'a, S> {
        ReadFuture {
            handle: self,
            buf,
            bytes_read: 0,
        }
    }

    /// Write bytes at current position.
    ///
    /// Returns a future that resolves when write completes.
    pub fn write<'a>(&'a self, buf: &'a [u8]) -> WriteFuture<'a, S> {
        WriteFuture {
            handle: self,
            buf,
            bytes_written: 0,
        }
    }
}

/// Future for reading from a `FileHandle`.
pub struct ReadFuture<'a, S: BlockStore> {
    handle: &'a FileHandle<S>,
    buf: &'a mut [u8],
    bytes_read: usize,
}

impl<S: BlockStore + Unpin> Future for ReadFuture<'_, S> {
    type Output = Result<usize, VfsError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        // Check read permission
        if !this.handle.mode.can_read() {
            return Poll::Ready(Err(VfsError::PermissionDenied(
                "file not opened for reading".into(),
            )));
        }

        let file_size = this.handle.cache.file_size();
        let pos = this.handle.pos.get();

        // EOF check
        if pos >= file_size {
            return Poll::Ready(Ok(0));
        }

        let block_size = this.handle.cache.block_size() as u64;
        let block_num = pos / block_size;

        // Ensure block is in cache
        {
            let mut store = this.handle.store.borrow_mut();
            match Pin::new(&mut *store).poll_read_block(cx, block_num, &this.handle.cache) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        // Read from cached block
        let block_data = this.handle.cache.get(block_num).unwrap();
        let offset_in_block = (pos % block_size) as usize;
        let remaining_in_block = block_data.len().saturating_sub(offset_in_block);
        let remaining_in_file = (file_size - pos) as usize;
        let remaining_in_buf = this.buf.len() - this.bytes_read;

        let to_read = remaining_in_buf
            .min(remaining_in_block)
            .min(remaining_in_file);

        if to_read > 0 {
            this.buf[this.bytes_read..this.bytes_read + to_read]
                .copy_from_slice(&block_data[offset_in_block..offset_in_block + to_read]);
            this.bytes_read += to_read;
            this.handle.pos.set(pos + to_read as u64);
        }

        Poll::Ready(Ok(this.bytes_read))
    }
}

/// Future for writing to a `FileHandle`.
pub struct WriteFuture<'a, S: BlockStore> {
    handle: &'a FileHandle<S>,
    buf: &'a [u8],
    bytes_written: usize,
}

impl<S: BlockStore + Unpin> Future for WriteFuture<'_, S> {
    type Output = Result<usize, VfsError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        // Check write permission
        if !this.handle.mode.can_write() {
            return Poll::Ready(Err(VfsError::PermissionDenied(
                "file not opened for writing".into(),
            )));
        }

        if this.bytes_written >= this.buf.len() {
            return Poll::Ready(Ok(this.bytes_written));
        }

        let pos = this.handle.pos.get();
        let block_size = this.handle.cache.block_size();
        let block_num = pos / block_size as u64;
        let offset_in_block = (pos % block_size as u64) as usize;

        // Calculate how much to write in this block
        let remaining_in_buf = this.buf.len() - this.bytes_written;
        let remaining_in_block = block_size - offset_in_block;
        let to_write = remaining_in_buf.min(remaining_in_block);

        // Prepare block data - only as large as needed
        let block_data = if offset_in_block == 0 && to_write == block_size {
            // Full block write - no need to read existing
            this.buf[this.bytes_written..this.bytes_written + to_write].to_vec()
        } else {
            // Partial block - need to merge with existing
            let mut block = if let Some(existing) = this.handle.cache.get(block_num) {
                existing
            } else {
                // Only create as large as needed, not full block
                vec![0u8; offset_in_block + to_write]
            };

            // Ensure block is large enough for our write
            if block.len() < offset_in_block + to_write {
                block.resize(offset_in_block + to_write, 0);
            }

            block[offset_in_block..offset_in_block + to_write]
                .copy_from_slice(&this.buf[this.bytes_written..this.bytes_written + to_write]);

            block
        };

        // Write block
        {
            let mut store = this.handle.store.borrow_mut();
            match Pin::new(&mut *store).poll_write_block(
                cx,
                block_num,
                &block_data,
                &this.handle.cache,
            ) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        // Update position
        let new_pos = pos + to_write as u64;
        this.bytes_written += to_write;
        this.handle.pos.set(new_pos);

        // Update file size if we extended beyond current end
        let current_size = this.handle.cache.file_size();
        if new_pos > current_size {
            this.handle.cache.set_file_size(new_pos);
        }

        Poll::Ready(Ok(this.bytes_written))
    }
}

// Implement AsyncRead for FileHandle
impl<S: BlockStore + Unpin + 'static> AsyncRead for FileHandle<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, SchedulerError>> {
        // Check read permission
        if !self.mode.can_read() {
            return Poll::Ready(Err(SchedulerError::Command(
                "file not opened for reading".into(),
            )));
        }

        let file_size = self.cache.file_size();
        let pos = self.pos.get();

        if pos >= file_size {
            return Poll::Ready(Ok(0)); // EOF
        }

        let block_size = self.cache.block_size() as u64;
        let block_num = pos / block_size;

        // Ensure block is in cache
        {
            let mut store = self.store.borrow_mut();
            match Pin::new(&mut *store).poll_read_block(cx, block_num, &self.cache) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(SchedulerError::Command(e.to_string())));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        // Read from cached block
        let block_data = self.cache.get(block_num).unwrap();
        let offset_in_block = (pos % block_size) as usize;
        let remaining_in_block = block_data.len().saturating_sub(offset_in_block);
        let remaining_in_file = (file_size - pos) as usize;

        let to_read = buf.len().min(remaining_in_block).min(remaining_in_file);

        buf[..to_read].copy_from_slice(&block_data[offset_in_block..offset_in_block + to_read]);
        self.pos.set(pos + to_read as u64);

        Poll::Ready(Ok(to_read))
    }
}

// Implement AsyncWrite for FileHandle
impl<S: BlockStore + Unpin + 'static> AsyncWrite for FileHandle<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, SchedulerError>> {
        if !self.mode.can_write() {
            return Poll::Ready(Err(SchedulerError::Command(
                "file not opened for writing".into(),
            )));
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let pos = self.pos.get();
        let block_size = self.cache.block_size();
        let block_num = pos / block_size as u64;
        let offset_in_block = (pos % block_size as u64) as usize;

        let remaining_in_block = block_size - offset_in_block;
        let to_write = buf.len().min(remaining_in_block);

        // Prepare block data
        let block_data = if offset_in_block == 0 && to_write == block_size {
            buf[..to_write].to_vec()
        } else {
            let mut block = if let Some(existing) = self.cache.get(block_num) {
                existing
            } else {
                vec![0u8; block_size]
            };

            if block.len() < offset_in_block + to_write {
                block.resize(offset_in_block + to_write, 0);
            }

            block[offset_in_block..offset_in_block + to_write].copy_from_slice(&buf[..to_write]);
            block
        };

        // Write block
        {
            let mut store = self.store.borrow_mut();
            match Pin::new(&mut *store).poll_write_block(cx, block_num, &block_data, &self.cache) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(SchedulerError::Command(e.to_string())));
                }
                Poll::Pending => return Poll::Pending,
            }
        }

        self.pos.set(pos + to_write as u64);
        Poll::Ready(Ok(to_write))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), SchedulerError>> {
        // Flush the store
        let mut store = self.store.borrow_mut();
        match Pin::new(&mut *store).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(SchedulerError::Command(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::MemoryBlockStore;
    use amla_scheduler::Executor;

    #[test]
    fn file_handle_read_basic() {
        let exec = Executor::new();

        exec.spawn(async {
            let data = b"hello world, this is a test file".to_vec();
            let store = MemoryBlockStore::with_block_size(data.clone(), 16);
            let cache = PageCache::new(16, data.len() as u64);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            let mut buf = [0u8; 5];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b"hello");

            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b" worl");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_read_to_eof() {
        let exec = Executor::new();

        exec.spawn(async {
            let data = b"short".to_vec();
            let store = MemoryBlockStore::with_block_size(data.clone(), 16);
            let cache = PageCache::new(16, data.len() as u64);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            let mut buf = [0u8; 100];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf[..n], b"short");

            // EOF
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_write_basic() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(Vec::new(), 16);
            let cache = PageCache::new(16, 0);
            let handle = FileHandle::new(store, cache.clone(), OpenMode::Write);

            let n = handle.write(b"hello").await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(handle.position(), 5);

            let n = handle.write(b" world").await.unwrap();
            assert_eq!(n, 6);
            assert_eq!(handle.position(), 11);

            assert_eq!(cache.file_size(), 11);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_seek() {
        let exec = Executor::new();

        exec.spawn(async {
            let data = b"0123456789".to_vec();
            let store = MemoryBlockStore::with_block_size(data.clone(), 16);
            let cache = PageCache::new(16, data.len() as u64);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Seek from start
            assert_eq!(handle.seek(SeekFrom::Start(5)).unwrap(), 5);
            assert_eq!(handle.position(), 5);

            // Read from position
            let mut buf = [0u8; 3];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 3);
            assert_eq!(&buf, b"567");

            // Seek from current
            assert_eq!(handle.seek(SeekFrom::Current(-2)).unwrap(), 6);

            // Seek from end
            assert_eq!(handle.seek(SeekFrom::End(-3)).unwrap(), 7);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_read_write_mode() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(b"hello".to_vec(), 16);
            let cache = PageCache::new(16, 5);
            let handle = FileHandle::new(store, cache, OpenMode::ReadWrite);

            // Read
            let mut buf = [0u8; 5];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b"hello");

            // Seek back and write
            handle.seek(SeekFrom::Start(0)).unwrap();
            let n = handle.write(b"HELLO").await.unwrap();
            assert_eq!(n, 5);

            // Seek back and verify
            handle.seek(SeekFrom::Start(0)).unwrap();
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b"HELLO");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_append_mode() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(b"hello".to_vec(), 16);
            let cache = PageCache::new(16, 5);
            let handle = FileHandle::new(store, cache.clone(), OpenMode::Append);

            // Position should start at end
            assert_eq!(handle.position(), 5);

            // Write appends
            let n = handle.write(b" world").await.unwrap();
            assert_eq!(n, 6);
            assert_eq!(cache.file_size(), 11);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_read_permission_denied() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(b"data".to_vec(), 16);
            let cache = PageCache::new(16, 4);
            let handle = FileHandle::new(store, cache, OpenMode::Write);

            // Should fail - write-only mode
            let mut buf = [0u8; 4];
            let result = handle.read(&mut buf).await;
            assert!(result.is_err());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_write_permission_denied() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(b"data".to_vec(), 16);
            let cache = PageCache::new(16, 4);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Should fail - read-only mode
            let result = handle.write(b"new data").await;
            assert!(result.is_err());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_cross_block_read() {
        let exec = Executor::new();

        exec.spawn(async {
            // 32 bytes of data, 8 byte blocks
            let data = b"01234567abcdefghIJKLMNOPqrstuvwx".to_vec();
            let store = MemoryBlockStore::with_block_size(data.clone(), 8);
            let cache = PageCache::new(8, data.len() as u64);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Seek to middle of block 0
            handle.seek(SeekFrom::Start(4)).unwrap();

            // Read across block boundary
            let mut buf = [0u8; 8];
            let n = handle.read(&mut buf).await.unwrap();
            // Should only read to end of current block
            assert_eq!(n, 4);
            assert_eq!(&buf[..n], b"4567");

            // Next read gets next block
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 8);
            assert_eq!(&buf, b"abcdefgh");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_cross_block_write() {
        let exec = Executor::new();

        exec.spawn(async {
            // 8 byte blocks
            let store = MemoryBlockStore::with_block_size(Vec::new(), 8);
            let cache = PageCache::new(8, 0);
            let handle = FileHandle::new(store, cache.clone(), OpenMode::Write);

            // Write data that spans multiple blocks
            let n = handle.write(b"0123456789ABCDEF").await.unwrap();
            assert_eq!(n, 8); // First write fills block 0

            let n = handle.write(b"89ABCDEF").await.unwrap();
            assert_eq!(n, 8); // Second write fills block 1

            assert_eq!(handle.position(), 16);
            assert_eq!(cache.file_size(), 16);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_large_file() {
        let exec = Executor::new();

        exec.spawn(async {
            // Create a 10KB file with 4KB blocks
            let data: Vec<u8> = (0..10240_u32).map(|i| (i & 0xFF) as u8).collect();
            let store = MemoryBlockStore::new(data.clone());
            let cache = PageCache::new(4096, data.len() as u64);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Read entire file in chunks
            let mut buf = [0u8; 1024];
            let mut total_read = 0;
            let mut result = Vec::new();

            loop {
                let n = handle.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                result.extend_from_slice(&buf[..n]);
                total_read += n;
            }

            assert_eq!(total_read, 10240);
            assert_eq!(result, data);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_write_creates_gap() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(Vec::new(), 16);
            let cache = PageCache::new(16, 0);
            let handle = FileHandle::new(store, cache.clone(), OpenMode::Write);

            // Seek past beginning and write
            handle.seek(SeekFrom::Start(10)).unwrap();
            let n = handle.write(b"hello").await.unwrap();
            assert_eq!(n, 5);

            // File size should include the gap
            assert_eq!(cache.file_size(), 15);
            assert_eq!(handle.position(), 15);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_overwrite_middle() {
        let exec = Executor::new();

        exec.spawn(async {
            let data = b"XXXXXXXXXXXX".to_vec();
            let store = MemoryBlockStore::with_block_size(data.clone(), 16);
            let cache = PageCache::new(16, 12);

            // Pre-populate cache (simulating what AsyncVfs does on open)
            cache.insert(0, data);

            let handle = FileHandle::new(store, cache.clone(), OpenMode::ReadWrite);

            // Seek to middle and overwrite
            handle.seek(SeekFrom::Start(4)).unwrap();
            let n = handle.write(b"YYYY").await.unwrap();
            assert_eq!(n, 4);

            // File size should remain same
            assert_eq!(cache.file_size(), 12);

            // Read back and verify
            handle.seek(SeekFrom::Start(0)).unwrap();
            let mut buf = [0u8; 12];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 12);
            assert_eq!(&buf, b"XXXXYYYYXXXX");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_seek_past_eof_then_write() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(b"abc".to_vec(), 16);
            let cache = PageCache::new(16, 3);
            let handle = FileHandle::new(store, cache.clone(), OpenMode::ReadWrite);

            // Seek past EOF
            handle.seek(SeekFrom::Start(10)).unwrap();
            assert_eq!(handle.position(), 10);

            // Write extends file
            let n = handle.write(b"xyz").await.unwrap();
            assert_eq!(n, 3);
            assert_eq!(cache.file_size(), 13);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_empty_read() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(b"data".to_vec(), 16);
            let cache = PageCache::new(16, 4);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Read with zero-length buffer
            let mut buf = [];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_empty_write() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(Vec::new(), 16);
            let cache = PageCache::new(16, 0);
            let handle = FileHandle::new(store, cache.clone(), OpenMode::Write);

            // Write empty buffer
            let n = handle.write(b"").await.unwrap();
            assert_eq!(n, 0);
            assert_eq!(cache.file_size(), 0);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_is_eof() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(b"12345".to_vec(), 16);
            let cache = PageCache::new(16, 5);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            assert!(!handle.is_eof());

            // Seek to end
            handle.seek(SeekFrom::End(0)).unwrap();
            assert!(handle.is_eof());

            // Seek back
            handle.seek(SeekFrom::Start(3)).unwrap();
            assert!(!handle.is_eof());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn file_handle_size_getter() {
        let store = MemoryBlockStore::with_block_size(b"hello".to_vec(), 16);
        let cache = PageCache::new(16, 5);
        let handle = FileHandle::new(store, cache, OpenMode::Read);

        assert_eq!(handle.size(), 5);
    }

    #[test]
    fn file_handle_mode_getter() {
        let store = MemoryBlockStore::with_block_size(Vec::new(), 16);
        let cache = PageCache::new(16, 0);
        let handle = FileHandle::new(store, cache, OpenMode::Append);

        assert_eq!(handle.mode(), OpenMode::Append);
    }

    #[test]
    fn open_mode_can_read() {
        assert!(OpenMode::Read.can_read());
        assert!(!OpenMode::Write.can_read());
        assert!(OpenMode::ReadWrite.can_read());
        assert!(!OpenMode::Append.can_read());
    }

    #[test]
    fn open_mode_can_write() {
        assert!(!OpenMode::Read.can_write());
        assert!(OpenMode::Write.can_write());
        assert!(OpenMode::ReadWrite.can_write());
        assert!(OpenMode::Append.can_write());
    }

    // =========================================================================
    // AsyncRead/AsyncWrite Trait Implementation Tests
    // =========================================================================

    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    fn noop_waker() -> Waker {
        Waker::noop().clone()
    }

    #[test]
    fn async_read_basic() {
        let data = b"hello world".to_vec();
        let store = MemoryBlockStore::with_block_size(data.clone(), 16);
        let cache = PageCache::new(16, data.len() as u64);

        // Pre-populate cache
        cache.insert(0, data);

        let mut handle = FileHandle::new(store, cache, OpenMode::Read);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut buf = [0u8; 5];
        let result = Pin::new(&mut handle).poll_read(&mut cx, &mut buf);

        match result {
            Poll::Ready(Ok(n)) => {
                assert_eq!(n, 5);
                assert_eq!(&buf, b"hello");
            }
            _ => panic!("expected Ready(Ok)"),
        }
    }

    #[test]
    fn async_read_permission_denied() {
        let store = MemoryBlockStore::with_block_size(b"data".to_vec(), 16);
        let cache = PageCache::new(16, 4);
        let mut handle = FileHandle::new(store, cache, OpenMode::Write);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut buf = [0u8; 4];
        let result = Pin::new(&mut handle).poll_read(&mut cx, &mut buf);

        match result {
            Poll::Ready(Err(_)) => {} // Expected
            _ => panic!("expected permission denied error"),
        }
    }

    #[test]
    fn async_read_eof() {
        let store = MemoryBlockStore::with_block_size(Vec::new(), 16);
        let cache = PageCache::new(16, 0);
        let mut handle = FileHandle::new(store, cache, OpenMode::Read);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut buf = [0u8; 10];
        let result = Pin::new(&mut handle).poll_read(&mut cx, &mut buf);

        match result {
            Poll::Ready(Ok(0)) => {} // EOF
            _ => panic!("expected EOF (0 bytes)"),
        }
    }

    #[test]
    fn async_write_basic() {
        let store = MemoryBlockStore::with_block_size(Vec::new(), 16);
        let cache = PageCache::new(16, 0);
        let mut handle = FileHandle::new(store, cache.clone(), OpenMode::Write);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut handle).poll_write(&mut cx, b"hello");

        match result {
            Poll::Ready(Ok(n)) => {
                assert_eq!(n, 5);
                assert_eq!(handle.position(), 5);
            }
            _ => panic!("expected Ready(Ok)"),
        }
    }

    #[test]
    fn async_write_permission_denied() {
        let store = MemoryBlockStore::with_block_size(b"data".to_vec(), 16);
        let cache = PageCache::new(16, 4);
        let mut handle = FileHandle::new(store, cache, OpenMode::Read);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut handle).poll_write(&mut cx, b"new");

        match result {
            Poll::Ready(Err(_)) => {} // Expected
            _ => panic!("expected permission denied error"),
        }
    }

    #[test]
    fn async_write_empty() {
        let store = MemoryBlockStore::with_block_size(Vec::new(), 16);
        let cache = PageCache::new(16, 0);
        let mut handle = FileHandle::new(store, cache, OpenMode::Write);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut handle).poll_write(&mut cx, b"");

        match result {
            Poll::Ready(Ok(0)) => {} // Empty write
            _ => panic!("expected 0 bytes written"),
        }
    }

    #[test]
    fn async_write_full_block() {
        // Test writing exactly one full block
        let store = MemoryBlockStore::with_block_size(Vec::new(), 8);
        let cache = PageCache::new(8, 0);
        let mut handle = FileHandle::new(store, cache.clone(), OpenMode::Write);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut handle).poll_write(&mut cx, b"12345678");

        match result {
            Poll::Ready(Ok(8)) => {
                assert_eq!(handle.position(), 8);
            }
            _ => panic!("expected 8 bytes written"),
        }
    }

    #[test]
    fn async_write_partial_block() {
        // Test writing a partial block (needs to merge with existing)
        let store = MemoryBlockStore::with_block_size(b"XXXXXXXX".to_vec(), 8);
        let cache = PageCache::new(8, 8);

        // Pre-populate cache with existing data
        cache.insert(0, b"XXXXXXXX".to_vec());

        let mut handle = FileHandle::new(store, cache.clone(), OpenMode::ReadWrite);

        // Seek to middle
        handle.seek(SeekFrom::Start(2)).unwrap();

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut handle).poll_write(&mut cx, b"YY");

        match result {
            Poll::Ready(Ok(2)) => {
                assert_eq!(handle.position(), 4);
            }
            _ => panic!("expected 2 bytes written"),
        }
    }

    #[test]
    fn async_close() {
        let store = MemoryBlockStore::with_block_size(Vec::new(), 16);
        let cache = PageCache::new(16, 0);
        let mut handle = FileHandle::new(store, cache, OpenMode::Write);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let result = Pin::new(&mut handle).poll_close(&mut cx);

        match result {
            Poll::Ready(Ok(())) => {} // Successfully closed
            _ => panic!("expected successful close"),
        }
    }

    #[test]
    fn seek_from_end_positive() {
        // SeekFrom::End with positive offset (past end of file)
        let store = MemoryBlockStore::with_block_size(b"hello".to_vec(), 16);
        let cache = PageCache::new(16, 5);
        let handle = FileHandle::new(store, cache, OpenMode::Read);

        let pos = handle.seek(SeekFrom::End(3)).unwrap();
        assert_eq!(pos, 8); // 5 + 3 = 8
    }

    #[test]
    fn seek_from_current_positive() {
        let store = MemoryBlockStore::with_block_size(b"hello".to_vec(), 16);
        let cache = PageCache::new(16, 5);
        let handle = FileHandle::new(store, cache, OpenMode::Read);

        // Start at 0, seek forward 3
        let pos = handle.seek(SeekFrom::Current(3)).unwrap();
        assert_eq!(pos, 3);

        // From 3, seek forward 2 more
        let pos = handle.seek(SeekFrom::Current(2)).unwrap();
        assert_eq!(pos, 5);
    }

    #[test]
    fn seek_saturation() {
        let store = MemoryBlockStore::with_block_size(b"hello".to_vec(), 16);
        let cache = PageCache::new(16, 5);
        let handle = FileHandle::new(store, cache, OpenMode::Read);

        // Seek backward past start should saturate at 0
        let pos = handle.seek(SeekFrom::Current(-100)).unwrap();
        assert_eq!(pos, 0);

        // Seek backward past start from end
        handle.seek(SeekFrom::End(0)).unwrap();
        let pos = handle.seek(SeekFrom::End(-100)).unwrap();
        assert_eq!(pos, 0);
    }

    // =========================================================================
    // WriteFuture edge cases
    // =========================================================================

    #[test]
    fn write_future_extends_file() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(Vec::new(), 8);
            let cache = PageCache::new(8, 0);
            let handle = FileHandle::new(store, cache.clone(), OpenMode::Write);

            // Write first chunk
            let n1 = handle.write(b"aaaa").await.unwrap();
            assert_eq!(n1, 4);

            // Write second chunk
            let n2 = handle.write(b"bbbb").await.unwrap();
            assert_eq!(n2, 4);

            // File size should be updated
            assert_eq!(cache.file_size(), 8);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn write_future_partial_then_full() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(Vec::new(), 8);
            let cache = PageCache::new(8, 0);
            let handle = FileHandle::new(store, cache.clone(), OpenMode::Write);

            // Write partial block
            let n1 = handle.write(b"abc").await.unwrap();
            assert_eq!(n1, 3);
            assert_eq!(handle.position(), 3);

            // Write to fill rest of block
            let n2 = handle.write(b"defgh").await.unwrap();
            assert_eq!(n2, 5); // Fills to block boundary
            assert_eq!(handle.position(), 8);

            // Write next block
            let n3 = handle.write(b"12345678").await.unwrap();
            assert_eq!(n3, 8);
            assert_eq!(handle.position(), 16);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn read_after_seek_past_eof() {
        let exec = Executor::new();

        exec.spawn(async {
            let store = MemoryBlockStore::with_block_size(b"hello".to_vec(), 16);
            let cache = PageCache::new(16, 5);
            let handle = FileHandle::new(store, cache, OpenMode::Read);

            // Seek past EOF
            handle.seek(SeekFrom::Start(100)).unwrap();

            // Read should return 0 (EOF)
            let mut buf = [0u8; 10];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }
}
