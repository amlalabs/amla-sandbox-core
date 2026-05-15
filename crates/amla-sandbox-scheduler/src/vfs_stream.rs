//! Async VFS read/write streams.
//!
//! These allow commands to read/write files in chunks without
//! loading entire files into memory.
//!
//! `VfsReader` and `VfsWriter` implement `AsyncRead`/`AsyncWrite` traits,
//! so they can be used with `ReadStream`/`WriteStream` wrappers.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::Error;
use crate::stream::{AsyncRead, AsyncWrite};

/// Async reader for VFS files.
///
/// Reads a file in chunks, yielding to the executor between chunks.
/// This allows large files to be processed without blocking.
pub struct VfsReader {
    /// File data.
    data: Vec<u8>,
    /// Current read position.
    pos: usize,
    /// Chunk size for reads.
    chunk_size: usize,
}

impl VfsReader {
    /// Create a new reader from file data.
    #[must_use]
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            pos: 0,
            chunk_size: 8192, // 8KB default chunks
        }
    }

    /// Create a reader with custom chunk size.
    #[must_use]
    pub fn with_chunk_size(data: Vec<u8>, chunk_size: usize) -> Self {
        Self {
            data,
            pos: 0,
            chunk_size,
        }
    }

    /// Check if all data has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Get remaining bytes.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Async read into buffer.
    pub fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> VfsReadFuture<'a> {
        VfsReadFuture { reader: self, buf }
    }

    /// Read all remaining data.
    pub fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let remaining = &self.data[self.pos..];
        let n = remaining.len();
        buf.extend_from_slice(remaining);
        self.pos = self.data.len();
        Ok(n)
    }
}

/// Future for async VFS read.
pub struct VfsReadFuture<'a> {
    reader: &'a mut VfsReader,
    buf: &'a mut [u8],
}

impl Future for VfsReadFuture<'_> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let remaining = &this.reader.data[this.reader.pos..];

        if remaining.is_empty() {
            return Poll::Ready(Ok(0)); // EOF
        }

        // Read up to chunk_size or buffer size, whichever is smaller
        let n = this
            .buf
            .len()
            .min(remaining.len())
            .min(this.reader.chunk_size);

        this.buf[..n].copy_from_slice(&remaining[..n]);
        this.reader.pos += n;

        Poll::Ready(Ok(n))
    }
}

// Implement AsyncRead trait for VfsReader
impl AsyncRead for VfsReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        let reader = self.get_mut();
        let remaining = &reader.data[reader.pos..];

        if remaining.is_empty() {
            return Poll::Ready(Ok(0)); // EOF
        }

        let n = buf.len().min(remaining.len()).min(reader.chunk_size);
        buf[..n].copy_from_slice(&remaining[..n]);
        reader.pos += n;

        Poll::Ready(Ok(n))
    }
}

/// Async writer for VFS files.
///
/// Accumulates writes and allows streaming output.
pub struct VfsWriter {
    /// Accumulated data.
    data: Vec<u8>,
    /// Maximum buffer size before auto-flush would be needed.
    max_size: Option<usize>,
}

impl VfsWriter {
    /// Create a new writer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Vec::new(),
            max_size: None,
        }
    }

    /// Create a writer with size limit.
    #[must_use]
    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            data: Vec::new(),
            max_size: Some(max_size),
        }
    }

    /// Get current size.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Async write.
    pub fn write<'a>(&'a mut self, data: &'a [u8]) -> VfsWriteFuture<'a> {
        VfsWriteFuture { writer: self, data }
    }

    /// Write all data.
    pub fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.data.extend_from_slice(data);
        Ok(())
    }

    /// Take the accumulated data.
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// Get a reference to accumulated data.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }
}

impl Default for VfsWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Future for async VFS write.
pub struct VfsWriteFuture<'a> {
    writer: &'a mut VfsWriter,
    data: &'a [u8],
}

impl Future for VfsWriteFuture<'_> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        // Check size limit
        if let Some(max) = this.writer.max_size
            && this.writer.data.len() + this.data.len() > max
        {
            return Poll::Ready(Err(io::Error::other("write would exceed max size")));
        }

        this.writer.data.extend_from_slice(this.data);
        Poll::Ready(Ok(this.data.len()))
    }
}

// Implement AsyncWrite trait for VfsWriter
impl AsyncWrite for VfsWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        let writer = self.get_mut();

        // Check size limit
        if let Some(max) = writer.max_size
            && writer.data.len() + buf.len() > max
        {
            return Poll::Ready(Err(Error::Command("write would exceed max size".into())));
        }

        writer.data.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        // VfsWriter doesn't need explicit close - data is available via into_bytes()
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Exit;
    use crate::executor::Executor;
    use crate::stream::{ReadStream, WriteStream};

    #[test]
    fn vfs_reader_basic() {
        let exec = Executor::new();

        exec.spawn(async {
            let mut reader = VfsReader::new(b"hello world".to_vec());
            let mut buf = [0u8; 5];

            let n = reader.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf[..n], b"hello");

            let n = reader.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf[..n], b" worl");

            let n = reader.read(&mut buf).await.unwrap();
            assert_eq!(n, 1);
            assert_eq!(&buf[..n], b"d");

            // EOF
            let n = reader.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);

            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn vfs_reader_chunked() {
        let exec = Executor::new();

        exec.spawn(async {
            let data = vec![0u8; 1000];
            let mut reader = VfsReader::with_chunk_size(data, 100);
            let mut buf = [0u8; 500];

            // Should only read 100 bytes (chunk size) even though buffer is 500
            let n = reader.read(&mut buf).await.unwrap();
            assert_eq!(n, 100);
            assert_eq!(reader.remaining(), 900);

            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn vfs_writer_basic() {
        let exec = Executor::new();

        exec.spawn(async {
            let mut writer = VfsWriter::new();

            writer.write_all(b"hello ").unwrap();
            writer.write_all(b"world").unwrap();

            assert_eq!(writer.as_bytes(), b"hello world");
            assert_eq!(writer.into_bytes(), b"hello world");

            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn vfs_writer_max_size() {
        let exec = Executor::new();

        exec.spawn(async {
            let mut writer = VfsWriter::with_max_size(10);

            writer.write(b"hello").await.unwrap();
            writer.write(b"world").await.unwrap();

            // This should fail - would exceed max size
            let result = writer.write(b"!").await;
            assert!(result.is_err());

            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn vfs_as_unified_stream() {
        // VfsReader/VfsWriter can be used through ReadStream/WriteStream
        // This is the unified interface that amla-sandbox will use
        let exec = Executor::new();

        exec.spawn(async {
            // Read through unified interface
            let reader = VfsReader::new(b"hello from vfs".to_vec());
            let mut stream = ReadStream::new(reader);
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            assert_eq!(buf, b"hello from vfs");

            // Write through unified interface
            let writer = VfsWriter::new();
            let mut stream = WriteStream::new(writer);
            stream.write_all(b"written via stream").await.unwrap();
            stream.close().await.unwrap();
            // Note: Can't get data out after wrapping in WriteStream
            // In practice, runtime would own the VfsWriter directly

            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn vfs_reader_is_empty_and_remaining() {
        let mut reader = VfsReader::new(b"hello".to_vec());
        assert!(!reader.is_empty());
        assert_eq!(reader.remaining(), 5);

        // Simulate reading all data
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert!(reader.is_empty());
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn vfs_writer_len_is_empty() {
        let writer = VfsWriter::new();
        assert!(writer.is_empty());
        assert_eq!(writer.len(), 0);

        let mut writer2 = VfsWriter::new();
        writer2.write_all(b"hello").unwrap();
        assert!(!writer2.is_empty());
        assert_eq!(writer2.len(), 5);
    }

    #[test]
    fn vfs_writer_as_bytes() {
        let mut writer = VfsWriter::new();
        writer.write_all(b"test data").unwrap();
        assert_eq!(writer.as_bytes(), b"test data");
    }

    #[test]
    fn vfs_writer_default() {
        let writer = VfsWriter::default();
        assert!(writer.is_empty());
    }

    #[test]
    fn vfs_writer_async_write_max_size_exceeded() {
        let exec = Executor::new();

        exec.spawn(async {
            let mut writer = VfsWriter::with_max_size(5);
            writer.write_all(b"hello").unwrap();

            // Now try to write more through the async write - should fail
            let result = writer.write(b"extra").await;
            assert!(result.is_err());

            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn vfs_writer_poll_write_max_size() {
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        // Create a simple no-op waker for testing
        fn noop_waker() -> Waker {
            const VTABLE: RawWakerVTable = RawWakerVTable::new(
                |_| RawWaker::new(std::ptr::null(), &VTABLE),
                |_| {},
                |_| {},
                |_| {},
            );
            // SAFETY: We implement a valid waker vtable
            unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
        }

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut writer = VfsWriter::with_max_size(10);

        // First write succeeds
        let pinned = Pin::new(&mut writer);
        let result = AsyncWrite::poll_write(pinned, &mut cx, b"hello");
        assert!(matches!(result, Poll::Ready(Ok(5))));

        // Second write succeeds
        let pinned = Pin::new(&mut writer);
        let result = AsyncWrite::poll_write(pinned, &mut cx, b"world");
        assert!(matches!(result, Poll::Ready(Ok(5))));

        // Third write exceeds max_size
        let pinned = Pin::new(&mut writer);
        let result = AsyncWrite::poll_write(pinned, &mut cx, b"!");
        assert!(matches!(result, Poll::Ready(Err(_))));
    }

    #[test]
    fn vfs_writer_poll_close() {
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn noop_waker() -> Waker {
            const VTABLE: RawWakerVTable = RawWakerVTable::new(
                |_| RawWaker::new(std::ptr::null(), &VTABLE),
                |_| {},
                |_| {},
                |_| {},
            );
            // SAFETY: VTABLE's fns are no-ops that ignore `data`; null is accepted because none of the vtable entries dereference it.
            unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
        }

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut writer = VfsWriter::new();
        let pinned = Pin::new(&mut writer);
        let result = AsyncWrite::poll_close(pinned, &mut cx);
        assert!(matches!(result, Poll::Ready(Ok(()))));
    }

    #[test]
    fn vfs_reader_poll_read() {
        use std::pin::Pin;
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn noop_waker() -> Waker {
            const VTABLE: RawWakerVTable = RawWakerVTable::new(
                |_| RawWaker::new(std::ptr::null(), &VTABLE),
                |_| {},
                |_| {},
                |_| {},
            );
            // SAFETY: VTABLE's fns are no-ops that ignore `data`; null is accepted because none of the vtable entries dereference it.
            unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
        }

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut reader = VfsReader::new(b"hello".to_vec());
        let mut buf = [0u8; 10];

        let pinned = Pin::new(&mut reader);
        let result = AsyncRead::poll_read(pinned, &mut cx, &mut buf);
        assert!(matches!(result, Poll::Ready(Ok(5))));
        assert_eq!(&buf[..5], b"hello");

        // Second poll returns EOF
        let pinned = Pin::new(&mut reader);
        let result = AsyncRead::poll_read(pinned, &mut cx, &mut buf);
        assert!(matches!(result, Poll::Ready(Ok(0))));
    }
}
