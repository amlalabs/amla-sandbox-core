//! Async VFS wrapper with file descriptor table.
//!
//! Provides an async API on top of the synchronous VFS, managing
//! file handles with a file descriptor table.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::block_store::MemoryBlockStore;
use crate::file_handle::{FileHandle, OpenMode};
use crate::page_cache::PageCache;
use crate::{Permission, Vfs, VfsError};

/// File descriptor type.
pub type Fd = u32;

/// Async VFS wrapper.
///
/// Provides file descriptor-based access to the underlying VFS,
/// with shared page caches per file path.
pub struct AsyncVfs {
    /// Underlying synchronous VFS
    vfs: RefCell<Vfs>,

    /// File descriptor table: fd -> `FileHandle`
    fd_table: RefCell<HashMap<Fd, OpenFile>>,

    /// Per-path page cache for sharing between handles
    caches: RefCell<HashMap<String, PageCache>>,

    /// Next available file descriptor
    next_fd: Cell<Fd>,
}

/// An open file with its handle and metadata.
struct OpenFile {
    /// The file handle
    handle: FileHandle<MemoryBlockStore>,
    /// Path to the file
    path: String,
}

impl AsyncVfs {
    /// Create a new `AsyncVfs` with the standard directory structure.
    pub fn new() -> Self {
        Self {
            vfs: RefCell::new(Vfs::new()),
            fd_table: RefCell::new(HashMap::new()),
            caches: RefCell::new(HashMap::new()),
            next_fd: Cell::new(3), // 0, 1, 2 reserved for stdin/stdout/stderr
        }
    }

    /// Create an `AsyncVfs` from an existing Vfs.
    pub fn from_vfs(vfs: Vfs) -> Self {
        Self {
            vfs: RefCell::new(vfs),
            fd_table: RefCell::new(HashMap::new()),
            caches: RefCell::new(HashMap::new()),
            next_fd: Cell::new(3),
        }
    }

    /// Get a reference to the underlying VFS.
    pub fn vfs(&self) -> std::cell::Ref<'_, Vfs> {
        self.vfs.borrow()
    }

    /// Get a mutable reference to the underlying VFS.
    pub fn vfs_mut(&self) -> std::cell::RefMut<'_, Vfs> {
        self.vfs.borrow_mut()
    }

    /// Open a file.
    ///
    /// Returns a file descriptor that can be used for read/write operations.
    ///
    /// # Write/Append Modes
    ///
    /// For `Write`, `ReadWrite`, and `Append` modes, the file doesn't need to exist
    /// but the **parent directory must exist and be writable**. If the parent is
    /// missing or read-only, this returns `PermissionDenied` immediately at open time.
    ///
    /// When opening a non-existent file in write mode:
    /// 1. Parent directory is checked at `open()` time
    /// 2. File is created in VFS when the handle is closed (or explicitly flushed)
    ///
    /// This means callers must ensure parent directories exist before opening
    /// files for writing. Use `Vfs::create_dir_all()` if needed.
    #[allow(clippy::too_many_lines)]
    pub fn open(&self, path: &str, mode: OpenMode) -> Result<Fd, VfsError> {
        let vfs = self.vfs.borrow();

        // Validate path and permissions
        let normalized = Self::normalize_path(path)?;

        // Check if file exists (for read modes) or can be created (for write modes)
        let (content, _permission) = match mode {
            OpenMode::Read => {
                // File must exist for read
                match vfs.stat(&normalized)? {
                    crate::Entry::File {
                        content,
                        permission,
                    } => (content, permission),
                    crate::Entry::Directory { .. } => {
                        return Err(VfsError::NotAFile(normalized));
                    }
                }
            }
            OpenMode::Write | OpenMode::ReadWrite => {
                // For write modes, create if doesn't exist
                if vfs.exists(&normalized) {
                    match vfs.stat(&normalized)? {
                        crate::Entry::File {
                            content,
                            permission,
                        } => {
                            // Check we can write (not read-only or append-only)
                            if permission == Permission::ReadOnly {
                                return Err(VfsError::PermissionDenied(format!(
                                    "file is read-only: {normalized}"
                                )));
                            }
                            if permission == Permission::AppendOnly {
                                return Err(VfsError::PermissionDenied(format!(
                                    "file is append-only, use Append mode: {normalized}"
                                )));
                            }
                            if mode == OpenMode::Write {
                                // Truncate for Write mode
                                (Vec::new(), permission)
                            } else {
                                (content, permission)
                            }
                        }
                        crate::Entry::Directory { .. } => {
                            return Err(VfsError::NotAFile(normalized));
                        }
                    }
                } else {
                    // Check parent directory exists and allows file creation.
                    // can_write_to() returns false if:
                    // - Parent directory doesn't exist
                    // - Parent directory is read-only or append-only
                    if !vfs.can_write_to(&normalized) {
                        return Err(VfsError::PermissionDenied(format!(
                            "cannot create file: parent directory missing or not writable: {normalized}"
                        )));
                    }
                    // Parent is writable. Buffer starts empty; actual file will be
                    // created in VFS when handle is closed (or flushed).
                    (Vec::new(), Permission::ReadWrite)
                }
            }
            OpenMode::Append => {
                // File must exist for append (or we create it)
                if vfs.exists(&normalized) {
                    match vfs.stat(&normalized)? {
                        crate::Entry::File {
                            content,
                            permission,
                        } => {
                            if permission == Permission::ReadOnly {
                                return Err(VfsError::PermissionDenied(format!(
                                    "file is read-only: {normalized}"
                                )));
                            }
                            (content, permission)
                        }
                        crate::Entry::Directory { .. } => {
                            return Err(VfsError::NotAFile(normalized));
                        }
                    }
                } else {
                    // Check parent directory exists and allows file creation.
                    // can_write_to() returns false if:
                    // - Parent directory doesn't exist
                    // - Parent directory is read-only or append-only
                    if !vfs.can_write_to(&normalized) {
                        return Err(VfsError::PermissionDenied(format!(
                            "cannot create file: parent directory missing or not writable: {normalized}"
                        )));
                    }
                    // Parent is writable. Buffer starts empty; actual file will be
                    // created in VFS when handle is closed (or flushed).
                    (Vec::new(), Permission::ReadWrite)
                }
            }
        };

        drop(vfs); // Release borrow

        // Get or create page cache for this path
        let cache = self.get_or_create_cache(&normalized, content.len() as u64);

        // Pre-populate cache with existing content
        // This is necessary so that close() can properly reconstruct the file
        if !content.is_empty() {
            let block_size = crate::block_store::BLOCK_SIZE;
            let num_blocks = content.len().div_ceil(block_size);
            for block_num in 0..num_blocks {
                let start = block_num * block_size;
                let end = (start + block_size).min(content.len());
                cache.insert(block_num as u64, content[start..end].to_vec());
            }
        }

        // Create block store with file content
        let store = MemoryBlockStore::new(content);

        // Create file handle
        let handle = FileHandle::new(store, cache, mode);

        // Allocate file descriptor
        let fd = self.next_fd.get();
        self.next_fd.set(fd + 1);

        // Store in fd table
        self.fd_table.borrow_mut().insert(
            fd,
            OpenFile {
                handle,
                path: normalized,
            },
        );

        Ok(fd)
    }

    /// Close a file descriptor.
    ///
    /// Flushes any pending writes to the VFS.
    pub fn close(&self, fd: Fd) -> Result<(), VfsError> {
        let mut fd_table = self.fd_table.borrow_mut();
        let open_file = fd_table
            .remove(&fd)
            .ok_or_else(|| VfsError::NotFound(format!("invalid fd: {fd}")))?;

        // Sync content back to VFS if file was opened for writing
        if open_file.handle.mode().can_write() {
            // Get all cached blocks and reconstruct file content
            let file_size = open_file.handle.size() as usize;
            let mut content = Vec::with_capacity(file_size);

            if file_size > 0 {
                let block_size = crate::block_store::BLOCK_SIZE;
                let num_blocks = file_size.div_ceil(block_size);

                // Read blocks from cache through the file handle's cache
                let cache = self.caches.borrow();
                if let Some(page_cache) = cache.get(&open_file.path) {
                    for block_num in 0..num_blocks as u64 {
                        if let Some(block_data) = page_cache.get(block_num) {
                            content.extend_from_slice(&block_data);
                        }
                    }
                }
                drop(cache);

                // Truncate to actual file size
                content.truncate(file_size);
            }

            // Write back to VFS (including empty files)
            let permission = self
                .vfs
                .borrow()
                .stat(&open_file.path)
                .map_or(Permission::ReadWrite, |e| e.permission());

            // Use insert_file to bypass permission checks for sync
            // (we already validated permissions on open)
            let mut vfs = self.vfs.borrow_mut();
            vfs.insert_file(&open_file.path, &content, permission)?;
        }

        // Clean up cache if no other handles reference it
        let refs = fd_table
            .values()
            .filter(|f| f.path == open_file.path)
            .count();
        if refs == 0 {
            self.caches.borrow_mut().remove(&open_file.path);
        }

        Ok(())
    }

    /// Read from a file descriptor.
    ///
    /// Returns a future that resolves to the number of bytes read.
    pub fn read<'a>(&'a self, fd: Fd, buf: &'a mut [u8]) -> FdReadFuture<'a> {
        FdReadFuture { vfs: self, fd, buf }
    }

    /// Write to a file descriptor.
    ///
    /// Returns a future that resolves to the number of bytes written.
    pub fn write<'a>(&'a self, fd: Fd, buf: &'a [u8]) -> FdWriteFuture<'a> {
        FdWriteFuture { vfs: self, fd, buf }
    }

    /// Seek on a file descriptor.
    pub fn seek(&self, fd: Fd, pos: SeekFrom) -> Result<u64, VfsError> {
        let fd_table = self.fd_table.borrow();
        let open_file = fd_table
            .get(&fd)
            .ok_or_else(|| VfsError::NotFound(format!("invalid fd: {fd}")))?;
        open_file.handle.seek(pos)
    }

    /// Get current position of a file descriptor.
    pub fn position(&self, fd: Fd) -> Result<u64, VfsError> {
        let fd_table = self.fd_table.borrow();
        let open_file = fd_table
            .get(&fd)
            .ok_or_else(|| VfsError::NotFound(format!("invalid fd: {fd}")))?;
        Ok(open_file.handle.position())
    }

    /// Get size of file associated with a file descriptor.
    pub fn size(&self, fd: Fd) -> Result<u64, VfsError> {
        let fd_table = self.fd_table.borrow();
        let open_file = fd_table
            .get(&fd)
            .ok_or_else(|| VfsError::NotFound(format!("invalid fd: {fd}")))?;
        Ok(open_file.handle.size())
    }

    /// Check if file descriptor is at end of file.
    pub fn is_eof(&self, fd: Fd) -> Result<bool, VfsError> {
        let fd_table = self.fd_table.borrow();
        let open_file = fd_table
            .get(&fd)
            .ok_or_else(|| VfsError::NotFound(format!("invalid fd: {fd}")))?;
        Ok(open_file.handle.is_eof())
    }

    /// Get number of open file descriptors.
    pub fn open_fd_count(&self) -> usize {
        self.fd_table.borrow().len()
    }

    // === Internal helpers ===

    fn normalize_path(path: &str) -> Result<String, VfsError> {
        // Use Vfs's normalize (it's private, so we replicate basic logic)
        let path = path.trim();
        if path.is_empty() || path == "/" {
            return Ok("/".to_string());
        }

        let mut components = Vec::new();
        for component in path.split('/') {
            match component {
                "" | "." => continue,
                ".." => {
                    if components.pop().is_none() {
                        return Err(VfsError::InvalidPath(format!("path escapes root: {path}")));
                    }
                }
                s => {
                    if s.contains('\0') {
                        return Err(VfsError::InvalidPath(format!("null byte in path: {path}")));
                    }
                    components.push(s);
                }
            }
        }

        if components.is_empty() {
            Ok("/".to_string())
        } else {
            Ok(format!("/{}", components.join("/")))
        }
    }

    fn get_or_create_cache(&self, path: &str, file_size: u64) -> PageCache {
        let mut caches = self.caches.borrow_mut();
        if let Some(cache) = caches.get(path) {
            cache.clone()
        } else {
            let cache = PageCache::new(crate::block_store::BLOCK_SIZE, file_size);
            caches.insert(path.to_string(), cache.clone());
            cache
        }
    }
}

impl Default for AsyncVfs {
    fn default() -> Self {
        Self::new()
    }
}

use std::future::Future;

/// Future for reading from a file descriptor.
pub struct FdReadFuture<'a> {
    vfs: &'a AsyncVfs,
    fd: Fd,
    buf: &'a mut [u8],
}

impl Future for FdReadFuture<'_> {
    type Output = Result<usize, VfsError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let mut fd_table = this.vfs.fd_table.borrow_mut();

        let open_file = match fd_table.get_mut(&this.fd) {
            Some(f) => f,
            None => {
                return Poll::Ready(Err(VfsError::NotFound(format!("invalid fd: {}", this.fd))));
            }
        };

        // Use the FileHandle's read method which returns a future
        // For MemoryBlockStore, this completes instantly
        let mut read_fut = open_file.handle.read(this.buf);
        match Pin::new(&mut read_fut).poll(cx) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Future for writing to a file descriptor.
pub struct FdWriteFuture<'a> {
    vfs: &'a AsyncVfs,
    fd: Fd,
    buf: &'a [u8],
}

impl Future for FdWriteFuture<'_> {
    type Output = Result<usize, VfsError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &*self;
        let mut fd_table = this.vfs.fd_table.borrow_mut();

        let open_file = match fd_table.get_mut(&this.fd) {
            Some(f) => f,
            None => {
                return Poll::Ready(Err(VfsError::NotFound(format!("invalid fd: {}", this.fd))));
            }
        };

        // Use the FileHandle's write method
        let mut write_fut = open_file.handle.write(this.buf);
        match Pin::new(&mut write_fut).poll(cx) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amla_scheduler::Executor;

    #[test]
    fn async_vfs_open_read_close() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Write a file first
            async_vfs
                .vfs_mut()
                .write_file("/workspace/test.txt", b"hello world", Permission::ReadWrite)
                .unwrap();

            // Open for reading
            let fd = async_vfs
                .open("/workspace/test.txt", OpenMode::Read)
                .unwrap();
            assert!(fd >= 3); // fd 0,1,2 reserved

            // Check state
            assert_eq!(async_vfs.position(fd).unwrap(), 0);
            assert_eq!(async_vfs.size(fd).unwrap(), 11);
            assert!(!async_vfs.is_eof(fd).unwrap());

            // Close
            async_vfs.close(fd).unwrap();
            assert_eq!(async_vfs.open_fd_count(), 0);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_open_write_close() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Open for writing (creates file)
            let fd = async_vfs
                .open("/workspace/new.txt", OpenMode::Write)
                .unwrap();

            // Write should work
            assert_eq!(async_vfs.size(fd).unwrap(), 0);

            // Close
            async_vfs.close(fd).unwrap();

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_seek() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            async_vfs
                .vfs_mut()
                .write_file("/workspace/test.txt", b"0123456789", Permission::ReadWrite)
                .unwrap();

            let fd = async_vfs
                .open("/workspace/test.txt", OpenMode::Read)
                .unwrap();

            // Seek from start
            assert_eq!(async_vfs.seek(fd, SeekFrom::Start(5)).unwrap(), 5);
            assert_eq!(async_vfs.position(fd).unwrap(), 5);

            // Seek from current
            assert_eq!(async_vfs.seek(fd, SeekFrom::Current(2)).unwrap(), 7);

            // Seek from end
            assert_eq!(async_vfs.seek(fd, SeekFrom::End(-3)).unwrap(), 7);

            async_vfs.close(fd).unwrap();

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_multiple_fds() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            async_vfs
                .vfs_mut()
                .write_file("/workspace/a.txt", b"aaa", Permission::ReadWrite)
                .unwrap();
            async_vfs
                .vfs_mut()
                .write_file("/workspace/b.txt", b"bbb", Permission::ReadWrite)
                .unwrap();

            let fd1 = async_vfs.open("/workspace/a.txt", OpenMode::Read).unwrap();
            let fd2 = async_vfs.open("/workspace/b.txt", OpenMode::Read).unwrap();

            assert_ne!(fd1, fd2);
            assert_eq!(async_vfs.open_fd_count(), 2);

            // Seek independently
            async_vfs.seek(fd1, SeekFrom::Start(1)).unwrap();
            async_vfs.seek(fd2, SeekFrom::Start(2)).unwrap();

            assert_eq!(async_vfs.position(fd1).unwrap(), 1);
            assert_eq!(async_vfs.position(fd2).unwrap(), 2);

            async_vfs.close(fd1).unwrap();
            assert_eq!(async_vfs.open_fd_count(), 1);

            async_vfs.close(fd2).unwrap();
            assert_eq!(async_vfs.open_fd_count(), 0);

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_open_readonly_file_for_write_fails() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            async_vfs
                .vfs_mut()
                .write_file("/workspace/readonly.txt", b"data", Permission::ReadOnly)
                .unwrap();

            // Should fail - file is read-only
            let result = async_vfs.open("/workspace/readonly.txt", OpenMode::Write);
            assert!(result.is_err());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_open_nonexistent_for_read_fails() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            let result = async_vfs.open("/workspace/nonexistent.txt", OpenMode::Read);
            assert!(result.is_err());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_close_invalid_fd_fails() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            let result = async_vfs.close(999);
            assert!(result.is_err());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_append_mode() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            async_vfs
                .vfs_mut()
                .write_file("/workspace/log.txt", b"line1\n", Permission::ReadWrite)
                .unwrap();

            let fd = async_vfs
                .open("/workspace/log.txt", OpenMode::Append)
                .unwrap();

            // Position should be at end
            assert_eq!(async_vfs.position(fd).unwrap(), 6);
            assert_eq!(async_vfs.size(fd).unwrap(), 6);

            async_vfs.close(fd).unwrap();

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_from_vfs() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/existing.txt", b"data", Permission::ReadWrite)
            .unwrap();

        let async_vfs = AsyncVfs::from_vfs(vfs);

        assert!(async_vfs.vfs().exists("/workspace/existing.txt"));
    }

    #[test]
    fn async_vfs_path_normalization() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            async_vfs
                .vfs_mut()
                .write_file("/workspace/test.txt", b"data", Permission::ReadWrite)
                .unwrap();

            // Should normalize path
            let fd = async_vfs
                .open("/workspace/./test.txt", OpenMode::Read)
                .unwrap();
            async_vfs.close(fd).unwrap();

            // Path traversal should fail
            let result = async_vfs.open("/../etc/passwd", OpenMode::Read);
            assert!(result.is_err());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_read_via_fd() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Write a file
            async_vfs
                .vfs_mut()
                .write_file("/workspace/test.txt", b"hello world", Permission::ReadWrite)
                .unwrap();

            // Open and read
            let fd = async_vfs
                .open("/workspace/test.txt", OpenMode::Read)
                .unwrap();

            let mut buf = [0u8; 5];
            let n = async_vfs.read(fd, &mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b"hello");

            // Position should advance
            assert_eq!(async_vfs.position(fd).unwrap(), 5);

            // Read more
            let n = async_vfs.read(fd, &mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b" worl");

            async_vfs.close(fd).unwrap();

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_write_via_fd() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Open for writing
            let fd = async_vfs
                .open("/workspace/output.txt", OpenMode::Write)
                .unwrap();

            // Write data
            let n = async_vfs.write(fd, b"hello").await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(async_vfs.position(fd).unwrap(), 5);

            let n = async_vfs.write(fd, b" world").await.unwrap();
            assert_eq!(n, 6);
            assert_eq!(async_vfs.size(fd).unwrap(), 11);

            // Close to flush
            async_vfs.close(fd).unwrap();

            // Verify content was written to VFS
            let content = async_vfs.vfs().read_file("/workspace/output.txt").unwrap();
            assert_eq!(content, b"hello world");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_read_write_roundtrip() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Write
            let fd = async_vfs
                .open("/workspace/data.txt", OpenMode::Write)
                .unwrap();
            async_vfs.write(fd, b"test data").await.unwrap();
            async_vfs.close(fd).unwrap();

            // Read back
            let fd = async_vfs
                .open("/workspace/data.txt", OpenMode::Read)
                .unwrap();
            let mut buf = [0u8; 20];
            let n = async_vfs.read(fd, &mut buf).await.unwrap();
            assert_eq!(n, 9);
            assert_eq!(&buf[..n], b"test data");
            async_vfs.close(fd).unwrap();

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_read_eof() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            async_vfs
                .vfs_mut()
                .write_file("/workspace/short.txt", b"hi", Permission::ReadWrite)
                .unwrap();

            let fd = async_vfs
                .open("/workspace/short.txt", OpenMode::Read)
                .unwrap();

            // Read all
            let mut buf = [0u8; 10];
            let n = async_vfs.read(fd, &mut buf).await.unwrap();
            assert_eq!(n, 2);

            // EOF
            let n = async_vfs.read(fd, &mut buf).await.unwrap();
            assert_eq!(n, 0);
            assert!(async_vfs.is_eof(fd).unwrap());

            async_vfs.close(fd).unwrap();

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_write_append() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Create initial file
            async_vfs
                .vfs_mut()
                .write_file("/workspace/log.txt", b"line1\n", Permission::ReadWrite)
                .unwrap();

            // Open for append
            let fd = async_vfs
                .open("/workspace/log.txt", OpenMode::Append)
                .unwrap();

            // Write appends at end
            async_vfs.write(fd, b"line2\n").await.unwrap();
            async_vfs.close(fd).unwrap();

            // Verify
            let content = async_vfs.vfs().read_file("/workspace/log.txt").unwrap();
            assert_eq!(content, b"line1\nline2\n");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_open_directory_for_read_fails() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // /workspace is a directory
            let result = async_vfs.open("/workspace", OpenMode::Read);
            assert!(result.is_err());
            assert!(matches!(result, Err(VfsError::NotAFile(_))));

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_open_directory_for_write_fails() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // /workspace is a directory
            let result = async_vfs.open("/workspace", OpenMode::Write);
            assert!(result.is_err());
            assert!(matches!(result, Err(VfsError::NotAFile(_))));

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_open_directory_for_append_fails() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // /workspace is a directory
            let result = async_vfs.open("/workspace", OpenMode::Append);
            assert!(result.is_err());
            assert!(matches!(result, Err(VfsError::NotAFile(_))));

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_append_readonly_file_fails() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            async_vfs
                .vfs_mut()
                .write_file("/workspace/readonly.txt", b"data", Permission::ReadOnly)
                .unwrap();

            let result = async_vfs.open("/workspace/readonly.txt", OpenMode::Append);
            assert!(result.is_err());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_append_creates_new_file() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Open non-existent file for append
            let fd = async_vfs
                .open("/workspace/new_log.txt", OpenMode::Append)
                .unwrap();

            // Write data
            async_vfs.write(fd, b"first entry").await.unwrap();
            async_vfs.close(fd).unwrap();

            // Verify file was created
            let content = async_vfs.vfs().read_file("/workspace/new_log.txt").unwrap();
            assert_eq!(content, b"first entry");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_readwrite_existing_file() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Create a file
            async_vfs
                .vfs_mut()
                .write_file("/workspace/data.txt", b"original", Permission::ReadWrite)
                .unwrap();

            // Open for read-write
            let fd = async_vfs
                .open("/workspace/data.txt", OpenMode::ReadWrite)
                .unwrap();

            // Read first
            let mut buf = [0u8; 8];
            let n = async_vfs.read(fd, &mut buf).await.unwrap();
            assert_eq!(n, 8);
            assert_eq!(&buf, b"original");

            // Seek back and overwrite
            async_vfs.seek(fd, SeekFrom::Start(0)).unwrap();
            async_vfs.write(fd, b"modified").await.unwrap();
            async_vfs.close(fd).unwrap();

            // Verify
            let content = async_vfs.vfs().read_file("/workspace/data.txt").unwrap();
            assert_eq!(content, b"modified");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn async_vfs_write_truncates_existing() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Create a file with content
            async_vfs
                .vfs_mut()
                .write_file(
                    "/workspace/file.txt",
                    b"old content here",
                    Permission::ReadWrite,
                )
                .unwrap();

            // Open for write (should truncate)
            let fd = async_vfs
                .open("/workspace/file.txt", OpenMode::Write)
                .unwrap();

            // File should be empty after open
            assert_eq!(async_vfs.size(fd).unwrap(), 0);

            // Write new content
            async_vfs.write(fd, b"new").await.unwrap();
            async_vfs.close(fd).unwrap();

            // Verify - should only have new content
            let content = async_vfs.vfs().read_file("/workspace/file.txt").unwrap();
            assert_eq!(content, b"new");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    // =========================================================================
    // Regression tests for bug fixes
    // =========================================================================

    /// Regression test: `AppendOnly` files cannot be opened with Write mode.
    /// Bug: `open()` only checked for `ReadOnly`, allowing Write mode on `AppendOnly` files.
    #[test]
    fn async_vfs_appendonly_file_rejects_write_mode() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Create an append-only file in /workspace (which exists by default)
            async_vfs
                .vfs_mut()
                .write_file("/workspace/app.log", b"log entry\n", Permission::AppendOnly)
                .unwrap();

            // Should fail - cannot open append-only file with Write mode
            let result = async_vfs.open("/workspace/app.log", OpenMode::Write);
            assert!(result.is_err());
            assert!(matches!(result, Err(VfsError::PermissionDenied(_))));

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    /// Regression test: `AppendOnly` files cannot be opened with `ReadWrite` mode.
    #[test]
    fn async_vfs_appendonly_file_rejects_readwrite_mode() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Create an append-only file in /workspace
            async_vfs
                .vfs_mut()
                .write_file("/workspace/app.log", b"log entry\n", Permission::AppendOnly)
                .unwrap();

            // Should fail - cannot open append-only file with ReadWrite mode
            let result = async_vfs.open("/workspace/app.log", OpenMode::ReadWrite);
            assert!(result.is_err());
            assert!(matches!(result, Err(VfsError::PermissionDenied(_))));

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    /// Positive test: `AppendOnly` files CAN be opened with Append mode.
    #[test]
    fn async_vfs_appendonly_file_allows_append_mode() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Create an append-only file in /workspace
            async_vfs
                .vfs_mut()
                .write_file("/workspace/app.log", b"line1\n", Permission::AppendOnly)
                .unwrap();

            // Should succeed - Append mode is allowed
            let fd = async_vfs
                .open("/workspace/app.log", OpenMode::Append)
                .unwrap();

            // Append new content
            async_vfs.write(fd, b"line2\n").await.unwrap();
            async_vfs.close(fd).unwrap();

            // Verify both lines are present
            let content = async_vfs.vfs().read_file("/workspace/app.log").unwrap();
            assert_eq!(content, b"line1\nline2\n");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    /// Regression test: Empty files are persisted on close.
    /// Bug: `close()` skipped sync when `file_size == 0`, causing empty files to not be created.
    #[test]
    fn async_vfs_empty_file_persisted_on_close() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Open for write (creates file) but don't write anything
            let fd = async_vfs
                .open("/workspace/empty.txt", OpenMode::Write)
                .unwrap();

            // Close without writing
            async_vfs.close(fd).unwrap();

            // Verify empty file was created
            assert!(async_vfs.vfs().exists("/workspace/empty.txt"));
            let content = async_vfs.vfs().read_file("/workspace/empty.txt").unwrap();
            assert!(content.is_empty());

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    /// Regression test: Truncating a file to 0 bytes persists the truncation.
    /// Bug: `close()` skipped sync when `file_size == 0`, leaving old content in VFS.
    #[test]
    fn async_vfs_truncate_to_zero_persisted() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // Create a file with content
            async_vfs
                .vfs_mut()
                .write_file(
                    "/workspace/data.txt",
                    b"this content should be deleted",
                    Permission::ReadWrite,
                )
                .unwrap();

            // Open for write (truncates to 0) and close immediately
            let fd = async_vfs
                .open("/workspace/data.txt", OpenMode::Write)
                .unwrap();
            assert_eq!(async_vfs.size(fd).unwrap(), 0);
            async_vfs.close(fd).unwrap();

            // Verify file is now empty (truncation persisted)
            let content = async_vfs.vfs().read_file("/workspace/data.txt").unwrap();
            assert!(content.is_empty(), "File should be empty after truncation");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    /// Regression test: Cannot create new file in read-only directory via Write mode.
    #[test]
    fn async_vfs_cannot_create_file_in_readonly_dir_write() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // /tools is read-only by default - should fail to create file
            let result = async_vfs.open("/tools/malicious.sh", OpenMode::Write);
            assert!(result.is_err());
            assert!(matches!(result, Err(VfsError::PermissionDenied(_))));

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    /// Regression test: Cannot create new file in read-only directory via Append mode.
    #[test]
    fn async_vfs_cannot_create_file_in_readonly_dir_append() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // /tools is read-only by default - should fail to create file
            let result = async_vfs.open("/tools/log.txt", OpenMode::Append);
            assert!(result.is_err());
            assert!(matches!(result, Err(VfsError::PermissionDenied(_))));

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }

    /// Positive test: Can create new file in writable directory.
    #[test]
    fn async_vfs_can_create_file_in_writable_dir() {
        let exec = Executor::new();

        exec.spawn(async {
            let async_vfs = AsyncVfs::new();

            // /workspace is read-write - should succeed
            let fd = async_vfs
                .open("/workspace/newfile.txt", OpenMode::Write)
                .unwrap();
            async_vfs.write(fd, b"content").await.unwrap();
            async_vfs.close(fd).unwrap();

            // Verify file was created
            assert!(async_vfs.vfs().exists("/workspace/newfile.txt"));
            let content = async_vfs.vfs().read_file("/workspace/newfile.txt").unwrap();
            assert_eq!(content, b"content");

            Ok(amla_scheduler::Exit::success())
        });

        let _ = exec.run();
    }
}
