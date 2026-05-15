//! Unified async I/O abstraction for shell commands.
//!
//! Replaces fd numbers with Rust types. Each command task owns its I/O handles
//! with no shared mutable state between tasks.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use amla_scheduler::{AsyncPipe, Scheduler};
use amla_vfs::{Permission, Vfs, VfsError};

/// I/O error type for shell operations.
#[derive(Debug)]
pub enum IoError {
    /// File not found.
    NotFound(String),
    /// Is a directory.
    IsDir(String),
    /// Not a directory.
    NotDir(String),
    /// Permission denied.
    Permission(String),
    /// File exists.
    Exists(String),
    /// Invalid seek.
    InvalidSeek,
    /// I/O error.
    Io(String),
    /// VFS error.
    Vfs(VfsError),
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "no such file or directory: {p}"),
            Self::IsDir(p) => write!(f, "is a directory: {p}"),
            Self::NotDir(p) => write!(f, "not a directory: {p}"),
            Self::Permission(p) => write!(f, "permission denied: {p}"),
            Self::Exists(p) => write!(f, "file exists: {p}"),
            Self::InvalidSeek => write!(f, "invalid seek"),
            Self::Io(msg) => write!(f, "I/O error: {msg}"),
            Self::Vfs(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for IoError {}

impl From<VfsError> for IoError {
    fn from(e: VfsError) -> Self {
        Self::Vfs(e)
    }
}

impl From<std::io::Error> for IoError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<IoError> for amla_scheduler::Error {
    fn from(e: IoError) -> Self {
        amla_scheduler::Error::Command(e.to_string())
    }
}

/// File stat information.
#[derive(Debug, Clone)]
pub struct Stat {
    /// File size in bytes.
    pub size: u64,
    /// True if this is a directory.
    pub is_dir: bool,
    /// True if this is a file.
    pub is_file: bool,
}

/// Directory entry.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Entry name (not full path).
    pub name: String,
    /// True if this is a directory.
    pub is_dir: bool,
}

/// Unified async I/O handle.
///
/// Replaces fd numbers with Rust types. Each variant knows how to
/// read/write asynchronously. Handles are cheaply cloneable.
#[derive(Clone)]
pub enum IoHandle {
    /// Async pipe (for pipelines, stdin/stdout between commands).
    Pipe(AsyncPipe),

    /// VFS file with position tracking.
    File {
        /// VFS reference.
        vfs: Rc<RefCell<Vfs>>,
        /// File path.
        path: String,
        /// Current position in file (usize since VFS is in-memory).
        pos: Rc<Cell<usize>>,
        /// Whether file is readable.
        readable: bool,
        /// Whether file is writable.
        writable: bool,
        /// Whether to append on write.
        append: bool,
    },

    /// In-memory buffer (for capture/testing).
    Buffer {
        /// Buffer data.
        data: Rc<RefCell<Vec<u8>>>,
        /// Current read position.
        pos: Rc<Cell<usize>>,
    },

    /// Null device (/dev/null - reads return EOF, writes succeed).
    Null,

    /// Host stream (uses host ops for I/O).
    ///
    /// This variant uses line buffering with a max size fallback:
    /// - Flushes on newline for natural terminal output
    /// - Flushes at max_size bytes for long lines or binary data
    ///
    /// For input, it emits `ReadStdin` host ops.
    ///
    /// The runtime/command association is determined automatically by the
    /// scheduler via task_id tracking - see `HostChannel::set_current_task()`.
    HostStream {
        /// Scheduler for host ops.
        scheduler: Scheduler,
        /// Stream type (0=stdin, 1=stdout, 2=stderr).
        stream: u8,
        /// Internal buffer for writes.
        buffer: Rc<RefCell<Vec<u8>>>,
        /// Maximum buffer size before auto-flush (also flushes on newline).
        max_size: usize,
    },
}

impl IoHandle {
    /// Create a new pipe handle.
    pub fn pipe(capacity: usize) -> Self {
        Self::Pipe(AsyncPipe::new(capacity))
    }

    /// Create a new buffer handle.
    pub fn buffer() -> Self {
        Self::Buffer {
            data: Rc::new(RefCell::new(Vec::new())),
            pos: Rc::new(Cell::new(0)),
        }
    }

    /// Create a buffer handle with initial data.
    ///
    /// Useful for providing heredoc content as stdin.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::Buffer {
            data: Rc::new(RefCell::new(bytes)),
            pos: Rc::new(Cell::new(0)),
        }
    }

    /// Create a buffer handle from a string.
    ///
    /// Useful for providing heredoc content as stdin.
    pub fn from_string(s: &str) -> Self {
        Self::from_bytes(s.as_bytes().to_vec())
    }

    /// Take the contents of a buffer handle.
    ///
    /// Returns the buffer contents and resets the buffer.
    /// Returns empty Vec for non-buffer handles.
    pub fn take_buffer(&self) -> Vec<u8> {
        match self {
            Self::Buffer { data, pos } => {
                pos.set(0);
                std::mem::take(&mut *data.borrow_mut())
            }
            _ => Vec::new(),
        }
    }

    /// Create a new null handle (/dev/null).
    pub fn null() -> Self {
        Self::Null
    }

    /// Default max buffer size for host streams (256 bytes).
    /// Output is line-buffered, but this caps long lines.
    pub const DEFAULT_HOST_BUFFER_SIZE: usize = 256;

    /// Create a host stdout handle.
    ///
    /// Uses line buffering (flushes on newline) with `max_size` byte fallback.
    /// The runtime/command association is determined automatically via task_id.
    pub fn host_stdout(scheduler: Scheduler, max_size: usize) -> Self {
        Self::HostStream {
            scheduler,
            stream: 1,
            buffer: Rc::new(RefCell::new(Vec::new())),
            max_size,
        }
    }

    /// Create a host stderr handle.
    ///
    /// Uses line buffering (flushes on newline) with `max_size` byte fallback.
    /// The runtime/command association is determined automatically via task_id.
    pub fn host_stderr(scheduler: Scheduler, max_size: usize) -> Self {
        Self::HostStream {
            scheduler,
            stream: 2,
            buffer: Rc::new(RefCell::new(Vec::new())),
            max_size,
        }
    }

    /// Create a host stdin handle.
    ///
    /// Reads from stdin via `ReadStdin` host ops.
    /// The runtime/command association is determined automatically via task_id.
    pub fn host_stdin(scheduler: Scheduler) -> Self {
        Self::HostStream {
            scheduler,
            stream: 0,
            buffer: Rc::new(RefCell::new(Vec::new())),
            max_size: 0, // Not used for stdin
        }
    }

    /// Take buffered output data without going through async flush.
    ///
    /// Returns `Some((stream, data))` if this is a `HostStream` for stdout/stderr
    /// with non-empty buffered data. Returns `None` otherwise.
    ///
    /// This is used for exit-time cleanup where we need to extract buffered
    /// data synchronously without risk of the async flush being dropped.
    pub fn take_buffered_output(&self) -> Option<(u8, Vec<u8>)> {
        if let Self::HostStream { stream, buffer, .. } = self {
            // Only for stdout (1) and stderr (2), not stdin (0)
            if *stream == 0 {
                return None;
            }
            let mut buf = buffer.borrow_mut();
            if buf.is_empty() {
                return None;
            }
            Some((*stream, buf.drain(..).collect()))
        } else {
            None
        }
    }

    /// Create a file handle for reading.
    pub fn open_read(vfs: Rc<RefCell<Vfs>>, path: String) -> Result<Self, IoError> {
        // Check file exists and is not a directory
        {
            let vfs_ref = vfs.borrow();
            if !vfs_ref.exists(&path) {
                return Err(IoError::NotFound(path));
            }
            if vfs_ref.is_dir(&path) {
                return Err(IoError::IsDir(path));
            }
        }

        Ok(Self::File {
            vfs,
            path,
            pos: Rc::new(Cell::new(0)),
            readable: true,
            writable: false,
            append: false,
        })
    }

    /// Create a file handle for writing (creates/truncates).
    pub fn open_write(vfs: Rc<RefCell<Vfs>>, path: String) -> Result<Self, IoError> {
        // Check not a directory, create if needed
        {
            let vfs_ref = vfs.borrow();
            if vfs_ref.is_dir(&path) {
                return Err(IoError::IsDir(path));
            }
        }

        // Create/truncate the file
        {
            let mut vfs_ref = vfs.borrow_mut();
            vfs_ref.write_file(&path, &[], Permission::ReadWrite)?;
        }

        Ok(Self::File {
            vfs,
            path,
            pos: Rc::new(Cell::new(0)),
            readable: false,
            writable: true,
            append: false,
        })
    }

    /// Create a file handle for appending.
    pub fn open_append(vfs: Rc<RefCell<Vfs>>, path: String) -> Result<Self, IoError> {
        // Check not a directory
        {
            let vfs_ref = vfs.borrow();
            if vfs_ref.is_dir(&path) {
                return Err(IoError::IsDir(path));
            }
        }

        // Create file if it doesn't exist
        {
            let vfs_ref = vfs.borrow();
            if !vfs_ref.exists(&path) {
                drop(vfs_ref);
                let mut vfs_mut = vfs.borrow_mut();
                vfs_mut.write_file(&path, &[], Permission::ReadWrite)?;
            }
        }

        // Get current file size for initial position
        let size = {
            let vfs_ref = vfs.borrow();
            vfs_ref.read_file(&path).map(|v| v.len()).unwrap_or(0)
        };

        Ok(Self::File {
            vfs,
            path,
            pos: Rc::new(Cell::new(size)),
            readable: false,
            writable: true,
            append: true,
        })
    }

    /// Open a file for reading, or return stdin if path is "-".
    ///
    /// This implements the POSIX convention where "-" represents stdin.
    /// Centralizes the common pattern found in many commands like cat, head, tail, etc.
    pub fn open_file_or_stdin(
        vfs: Rc<RefCell<Vfs>>,
        path: &str,
        stdin: IoHandle,
    ) -> Result<Self, IoError> {
        if path == "-" {
            Ok(stdin)
        } else {
            Self::open_read(vfs, path.to_string())
        }
    }

    /// Read from the handle.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize, IoError> {
        match self {
            Self::Pipe(pipe) => Ok(pipe.read(buf).await?),

            Self::File {
                vfs,
                path,
                pos,
                readable,
                ..
            } => {
                if !readable {
                    return Err(IoError::Permission(path.clone()));
                }

                let vfs_ref = vfs.borrow();
                let content = vfs_ref.read_file(path)?;
                let current_pos = pos.get();

                if current_pos >= content.len() {
                    return Ok(0); // EOF
                }

                let available = content.len() - current_pos;
                let to_read = buf.len().min(available);
                buf[..to_read].copy_from_slice(&content[current_pos..current_pos + to_read]);
                pos.set(current_pos + to_read);

                Ok(to_read)
            }

            Self::Buffer { data, pos } => {
                let data_ref = data.borrow();
                let current_pos = pos.get();

                if current_pos >= data_ref.len() {
                    return Ok(0); // EOF
                }

                let available = data_ref.len() - current_pos;
                let to_read = buf.len().min(available);
                buf[..to_read].copy_from_slice(&data_ref[current_pos..current_pos + to_read]);
                pos.set(current_pos + to_read);

                Ok(to_read)
            }

            Self::Null => Ok(0), // EOF

            Self::HostStream {
                scheduler,
                stream,
                buffer,
                ..
            } => {
                // For stdin (stream 0), read from host via host op
                if *stream == 0 {
                    // First check if buffer has data
                    {
                        let mut buf_ref = buffer.borrow_mut();
                        if !buf_ref.is_empty() {
                            let to_read = buf.len().min(buf_ref.len());
                            buf[..to_read].copy_from_slice(&buf_ref[..to_read]);
                            buf_ref.drain(..to_read);
                            return Ok(to_read);
                        }
                    }

                    // Buffer empty, request from host
                    let data = scheduler
                        .host()
                        .read_stdin(buf.len())
                        .await
                        .map_err(|e| IoError::Io(e.to_string()))?;

                    if data.is_empty() {
                        return Ok(0); // EOF
                    }

                    let to_read = buf.len().min(data.len());
                    buf[..to_read].copy_from_slice(&data[..to_read]);

                    // Store excess in buffer
                    if data.len() > to_read {
                        buffer.borrow_mut().extend_from_slice(&data[to_read..]);
                    }

                    Ok(to_read)
                } else {
                    // stdout/stderr are write-only
                    Err(IoError::Permission(
                        "cannot read from stdout/stderr".to_string(),
                    ))
                }
            }
        }
    }

    /// Write to the handle.
    pub async fn write(&self, buf: &[u8]) -> Result<usize, IoError> {
        match self {
            Self::Pipe(pipe) => {
                pipe.write(buf).await?;
                Ok(buf.len())
            }

            Self::File {
                vfs,
                path,
                pos,
                writable,
                append,
                ..
            } => {
                if !writable {
                    return Err(IoError::Permission(path.clone()));
                }

                let mut vfs_mut = vfs.borrow_mut();

                if *append {
                    // Use append_file for append mode - works with AppendOnly files
                    vfs_mut.append_file(path, buf)?;
                    // Update position to new end
                    let new_size = vfs_mut.read_file(path).map(|v| v.len()).unwrap_or(0);
                    pos.set(new_size);
                } else {
                    // Regular write mode - read, modify, write back
                    let current_pos = pos.get();
                    let mut content = vfs_mut.read_file(path).unwrap_or_default();

                    // Extend file if needed
                    if current_pos + buf.len() > content.len() {
                        content.resize(current_pos + buf.len(), 0);
                    }

                    content[current_pos..current_pos + buf.len()].copy_from_slice(buf);
                    vfs_mut.write_file(path, &content, Permission::ReadWrite)?;

                    pos.set(current_pos + buf.len());
                }

                Ok(buf.len())
            }

            Self::Buffer { data, pos } => {
                let mut data_mut = data.borrow_mut();
                let current_pos = pos.get();

                // Extend buffer if needed
                if current_pos + buf.len() > data_mut.len() {
                    data_mut.resize(current_pos + buf.len(), 0);
                }

                data_mut[current_pos..current_pos + buf.len()].copy_from_slice(buf);
                pos.set(current_pos + buf.len());

                Ok(buf.len())
            }

            Self::Null => Ok(buf.len()), // Discard

            Self::HostStream {
                scheduler,
                stream,
                buffer,
                max_size,
            } => {
                // stdin is read-only
                if *stream == 0 {
                    return Err(IoError::Permission("cannot write to stdin".to_string()));
                }

                // Buffer the data
                buffer.borrow_mut().extend_from_slice(buf);

                // Line buffering: flush on newline OR when buffer exceeds max_size
                loop {
                    let to_flush: Option<Vec<u8>> = {
                        let mut buf_ref = buffer.borrow_mut();

                        // Check for newline first (line buffering)
                        if let Some(newline_pos) = buf_ref.iter().position(|&b| b == b'\n') {
                            // Flush up to and including the newline
                            Some(buf_ref.drain(..=newline_pos).collect())
                        } else if buf_ref.len() >= *max_size {
                            // Fallback: flush at max_size for long lines
                            Some(buf_ref.drain(..(*max_size)).collect())
                        } else {
                            None
                        }
                    };

                    let Some(data) = to_flush else {
                        break;
                    };

                    // Emit Print host op (runtime/command determined via task_id)
                    scheduler
                        .host()
                        .print(*stream, data)
                        .await
                        .map_err(|e| IoError::Io(e.to_string()))?;
                }

                Ok(buf.len())
            }
        }
    }

    /// Write all bytes.
    pub async fn write_all(&self, buf: &[u8]) -> Result<(), IoError> {
        let mut written = 0;
        while written < buf.len() {
            written += self.write(&buf[written..]).await?;
        }
        Ok(())
    }

    /// Get the buffer contents (for Buffer and `HostStream` variants).
    pub fn get_buffer(&self) -> Option<Vec<u8>> {
        match self {
            Self::Buffer { data, .. } => Some(data.borrow().clone()),
            Self::HostStream { buffer, .. } => Some(buffer.borrow().clone()),
            _ => None,
        }
    }

    /// Drain all available data synchronously.
    ///
    /// Works for Pipe, Buffer, and `HostStream`. Returns None for other variants.
    /// This clears the underlying buffer.
    pub fn drain(&self) -> Option<Vec<u8>> {
        match self {
            Self::Pipe(pipe) => Some(pipe.drain()),
            Self::Buffer { data, pos } => {
                let mut data_mut = data.borrow_mut();
                let result = data_mut.drain(..).collect();
                pos.set(0);
                Some(result)
            }
            Self::HostStream { buffer, .. } => {
                let mut buf_ref = buffer.borrow_mut();
                Some(buf_ref.drain(..).collect())
            }
            _ => None,
        }
    }

    /// Flush any buffered output via host ops.
    ///
    /// For `HostStream`, this flushes any remaining buffered data.
    /// For other variants, this is a no-op.
    pub async fn flush(&self) -> Result<(), IoError> {
        if let Self::HostStream {
            scheduler,
            stream,
            buffer,
            ..
        } = self
        {
            // stdin doesn't need flushing
            if *stream == 0 {
                return Ok(());
            }

            let to_flush: Vec<u8> = {
                let mut buf_ref = buffer.borrow_mut();
                if buf_ref.is_empty() {
                    return Ok(());
                }
                buf_ref.drain(..).collect()
            };

            // Runtime/command determined via task_id
            scheduler
                .host()
                .print(*stream, to_flush)
                .await
                .map_err(|e| IoError::Io(e.to_string()))?;
        }
        Ok(())
    }

    /// Close the pipe (signals EOF to readers).
    pub fn close(&self) {
        if let Self::Pipe(pipe) = self {
            pipe.close();
        }
    }

    /// Check if this handle is a pipe.
    pub fn is_pipe(&self) -> bool {
        matches!(self, Self::Pipe(_))
    }

    /// Check if a reader is blocked waiting for data on this pipe.
    ///
    /// Returns true only for `Pipe` variant when a reader has polled and
    /// returned Pending because the buffer is empty and not closed.
    /// Returns false for all other handle types.
    ///
    /// This allows the runtime to detect when a command is waiting for stdin.
    pub fn is_reader_blocked(&self) -> bool {
        match self {
            Self::Pipe(pipe) => pipe.is_reader_blocked(),
            _ => false,
        }
    }

    /// Synchronous write (for builtins that don't run in async context).
    ///
    /// Only works for Buffer and Null handles. Returns error for Pipe and File.
    pub fn write_sync(&self, buf: &[u8]) -> Result<usize, IoError> {
        match self {
            Self::Buffer { data, pos } => {
                let mut data_mut = data.borrow_mut();
                let current_pos = pos.get();

                if current_pos + buf.len() > data_mut.len() {
                    data_mut.resize(current_pos + buf.len(), 0);
                }

                data_mut[current_pos..current_pos + buf.len()].copy_from_slice(buf);
                pos.set(current_pos + buf.len());

                Ok(buf.len())
            }
            Self::Null => Ok(buf.len()),
            Self::Pipe(_) => Err(IoError::Io("cannot write synchronously to pipe".into())),
            Self::File { path, .. } => Err(IoError::Io(format!(
                "cannot write synchronously to file: {path}"
            ))),
            Self::HostStream { .. } => Err(IoError::Io(
                "cannot write synchronously to host stream".into(),
            )),
        }
    }

    /// Synchronous write all (for builtins).
    pub fn write_all_sync(&self, buf: &[u8]) -> Result<(), IoError> {
        let mut written = 0;
        while written < buf.len() {
            written += self.write_sync(&buf[written..])?;
        }
        Ok(())
    }

    /// Write a line synchronously (for builtins).
    pub fn println_sync(&self, s: &str) -> Result<(), IoError> {
        self.write_all_sync(s.as_bytes())?;
        self.write_all_sync(b"\n")?;
        Ok(())
    }

    /// Push data into a pipe synchronously.
    ///
    /// Only works for Pipe handles. Returns bytes written.
    pub fn push(&self, data: &[u8]) -> Result<usize, IoError> {
        match self {
            Self::Pipe(pipe) => Ok(pipe.push(data)),
            Self::Null => Ok(data.len()),
            _ => Err(IoError::Io("push only works on pipes".into())),
        }
    }

    /// Push all data into a pipe synchronously.
    ///
    /// Only works for Pipe handles.
    pub fn push_all(&self, data: &[u8]) -> Result<(), IoError> {
        match self {
            Self::Pipe(pipe) => pipe.push_all(data).map_err(|e| IoError::Io(e.to_string())),
            Self::Null => Ok(()),
            _ => Err(IoError::Io("push only works on pipes".into())),
        }
    }
}

impl std::fmt::Debug for IoHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pipe(_) => write!(f, "IoHandle::Pipe"),
            Self::File { path, .. } => write!(f, "IoHandle::File({path})"),
            Self::Buffer { .. } => write!(f, "IoHandle::Buffer"),
            Self::Null => write!(f, "IoHandle::Null"),
            Self::HostStream { stream, .. } => {
                let name = match stream {
                    0 => "stdin",
                    1 => "stdout",
                    2 => "stderr",
                    _ => "unknown",
                };
                write!(f, "IoHandle::HostStream({name})")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amla_scheduler::{RandomSourceFn, TimeSourceFn};
    use std::cell::Cell;

    fn test_scheduler() -> Scheduler {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    // =========================================================================
    // Buffer handle tests
    // =========================================================================

    #[test]
    fn buffer_read_write() {
        let handle = IoHandle::buffer();
        pollster::block_on(async {
            // Write
            handle.write(b"hello").await.unwrap();
            handle.write(b" world").await.unwrap();

            // Get buffer
            let content = handle.get_buffer().unwrap();
            assert_eq!(content, b"hello world");
        });
    }

    #[test]
    fn buffer_read_sequential() {
        let handle = IoHandle::buffer();
        pollster::block_on(async {
            // Write data
            handle.write(b"hello world").await.unwrap();
        });

        // Reset position for reading (create new handle with same buffer)
        let IoHandle::Buffer { data, .. } = handle.clone() else {
            panic!("expected buffer");
        };
        let read_handle = IoHandle::Buffer {
            data: data.clone(),
            pos: Rc::new(Cell::new(0)),
        };

        pollster::block_on(async {
            let mut buf = [0u8; 5];
            let n = read_handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b"hello");

            let n = read_handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf, b" worl");

            let n = read_handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 1);
            assert_eq!(&buf[..n], b"d");

            // EOF
            let n = read_handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);
        });
    }

    #[test]
    fn buffer_empty_read() {
        let handle = IoHandle::buffer();
        pollster::block_on(async {
            let mut buf = [0u8; 10];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 0); // Empty buffer = EOF
        });
    }

    #[test]
    fn buffer_get_buffer() {
        let handle = IoHandle::buffer();
        pollster::block_on(async {
            handle.write(b"test data").await.unwrap();
        });
        let content = handle.get_buffer().unwrap();
        assert_eq!(content, b"test data");
    }

    #[test]
    fn buffer_get_buffer_empty() {
        let handle = IoHandle::buffer();
        let content = handle.get_buffer().unwrap();
        assert!(content.is_empty());
    }

    // =========================================================================
    // Null device tests
    // =========================================================================

    #[test]
    fn null_device() {
        pollster::block_on(async {
            let handle = IoHandle::null();

            // Writes succeed but are discarded
            assert_eq!(handle.write(b"test").await.unwrap(), 4);

            // Reads return EOF
            let mut buf = [0u8; 10];
            assert_eq!(handle.read(&mut buf).await.unwrap(), 0);
        });
    }

    #[test]
    fn null_write_all() {
        pollster::block_on(async {
            let handle = IoHandle::null();
            handle.write_all(b"large data chunk").await.unwrap();
            // Succeeds, data is discarded
        });
    }

    #[test]
    fn null_get_buffer() {
        let handle = IoHandle::null();
        assert!(handle.get_buffer().is_none());
    }

    // =========================================================================
    // File handle tests
    // =========================================================================

    #[test]
    fn file_read() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));

        // Create a test file
        vfs.borrow_mut()
            .write_file("/workspace/test.txt", b"hello", Permission::ReadWrite)
            .unwrap();

        pollster::block_on(async {
            let handle =
                IoHandle::open_read(Rc::clone(&vfs), "/workspace/test.txt".to_string()).unwrap();

            let mut buf = [0u8; 10];
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf[..n], b"hello");

            // EOF
            let n = handle.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);
        });
    }

    #[test]
    fn file_write() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));

        pollster::block_on(async {
            let handle =
                IoHandle::open_write(Rc::clone(&vfs), "/workspace/out.txt".to_string()).unwrap();

            handle.write(b"hello").await.unwrap();
            handle.write(b" world").await.unwrap();
        });

        // Verify file contents
        let content = vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(content, b"hello world");
    }

    #[test]
    fn file_write_truncates() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));

        // Create file with initial content
        vfs.borrow_mut()
            .write_file(
                "/workspace/test.txt",
                b"old content here",
                Permission::ReadWrite,
            )
            .unwrap();

        pollster::block_on(async {
            let handle =
                IoHandle::open_write(Rc::clone(&vfs), "/workspace/test.txt".to_string()).unwrap();
            handle.write(b"new").await.unwrap();
        });

        let content = vfs.borrow().read_file("/workspace/test.txt").unwrap();
        assert_eq!(content, b"new");
    }

    #[test]
    fn file_append() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));

        // Create file with initial content
        vfs.borrow_mut()
            .write_file("/workspace/test.txt", b"hello", Permission::ReadWrite)
            .unwrap();

        pollster::block_on(async {
            let handle =
                IoHandle::open_append(Rc::clone(&vfs), "/workspace/test.txt".to_string()).unwrap();
            handle.write(b" world").await.unwrap();
        });

        let content = vfs.borrow().read_file("/workspace/test.txt").unwrap();
        assert_eq!(content, b"hello world");
    }

    #[test]
    fn file_append_creates() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));

        pollster::block_on(async {
            let handle =
                IoHandle::open_append(Rc::clone(&vfs), "/workspace/new.txt".to_string()).unwrap();
            handle.write(b"content").await.unwrap();
        });

        let content = vfs.borrow().read_file("/workspace/new.txt").unwrap();
        assert_eq!(content, b"content");
    }

    #[test]
    fn file_append_to_append_only() {
        // Test that append mode works with AppendOnly files (e.g., /log/actions.jsonl)
        let vfs = Rc::new(RefCell::new(Vfs::new()));

        // /log/actions.jsonl is created by Vfs::new() with AppendOnly permission
        pollster::block_on(async {
            let handle =
                IoHandle::open_append(Rc::clone(&vfs), "/log/actions.jsonl".to_string()).unwrap();
            handle.write(b"line1\n").await.unwrap();
            handle.write(b"line2\n").await.unwrap();
        });

        let content = vfs.borrow().read_file("/log/actions.jsonl").unwrap();
        assert_eq!(content, b"line1\nline2\n");
    }

    #[test]
    fn file_read_not_found() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let result = IoHandle::open_read(vfs, "/workspace/nonexistent.txt".to_string());
        assert!(matches!(result, Err(IoError::NotFound(_))));
    }

    #[test]
    fn file_read_is_directory() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir("/workspace/dir", Permission::ReadWrite)
            .unwrap();

        let result = IoHandle::open_read(vfs, "/workspace/dir".to_string());
        assert!(matches!(result, Err(IoError::IsDir(_))));
    }

    #[test]
    fn file_write_is_directory() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir("/workspace/dir", Permission::ReadWrite)
            .unwrap();

        let result = IoHandle::open_write(vfs, "/workspace/dir".to_string());
        assert!(matches!(result, Err(IoError::IsDir(_))));
    }

    #[test]
    fn file_append_is_directory() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir("/workspace/dir", Permission::ReadWrite)
            .unwrap();

        let result = IoHandle::open_append(vfs, "/workspace/dir".to_string());
        assert!(matches!(result, Err(IoError::IsDir(_))));
    }

    #[test]
    fn file_read_permission_denied() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file("/workspace/test.txt", b"content", Permission::ReadWrite)
            .unwrap();

        pollster::block_on(async {
            // Open for write only
            let handle =
                IoHandle::open_write(Rc::clone(&vfs), "/workspace/test.txt".to_string()).unwrap();

            // Try to read
            let mut buf = [0u8; 10];
            let result = handle.read(&mut buf).await;
            assert!(matches!(result, Err(IoError::Permission(_))));
        });
    }

    #[test]
    fn file_write_permission_denied() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file("/workspace/test.txt", b"content", Permission::ReadWrite)
            .unwrap();

        pollster::block_on(async {
            // Open for read only
            let handle =
                IoHandle::open_read(Rc::clone(&vfs), "/workspace/test.txt".to_string()).unwrap();

            // Try to write
            let result = handle.write(b"data").await;
            assert!(matches!(result, Err(IoError::Permission(_))));
        });
    }

    // =========================================================================
    // Pipe handle tests
    // =========================================================================

    #[test]
    fn pipe_creation() {
        let handle = IoHandle::pipe(1024);
        assert!(handle.is_pipe());
    }

    #[test]
    fn pipe_close() {
        let pipe = AsyncPipe::new(1024);
        let handle = IoHandle::Pipe(pipe.clone());
        handle.close();
        // Should not panic
    }

    #[test]
    fn is_pipe() {
        assert!(IoHandle::pipe(1024).is_pipe());
        assert!(!IoHandle::buffer().is_pipe());
        assert!(!IoHandle::null().is_pipe());
    }

    #[test]
    fn get_buffer_non_buffer() {
        let pipe = IoHandle::pipe(1024);
        assert!(pipe.get_buffer().is_none());

        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file("/workspace/test.txt", b"x", Permission::ReadWrite)
            .unwrap();
        let file = IoHandle::open_read(vfs, "/workspace/test.txt".to_string()).unwrap();
        assert!(file.get_buffer().is_none());
    }

    // =========================================================================
    // Synchronous write tests
    // =========================================================================

    #[test]
    fn write_sync_buffer() {
        let handle = IoHandle::buffer();
        handle.write_sync(b"hello").unwrap();
        handle.write_sync(b" world").unwrap();

        let content = handle.get_buffer().unwrap();
        assert_eq!(content, b"hello world");
    }

    #[test]
    fn write_sync_null() {
        let handle = IoHandle::null();
        let n = handle.write_sync(b"discarded").unwrap();
        assert_eq!(n, 9);
    }

    #[test]
    fn write_sync_pipe_error() {
        let handle = IoHandle::pipe(1024);
        let result = handle.write_sync(b"data");
        assert!(result.is_err());
    }

    #[test]
    fn write_sync_file_error() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file("/workspace/test.txt", b"x", Permission::ReadWrite)
            .unwrap();
        let handle = IoHandle::open_write(vfs, "/workspace/test.txt".to_string()).unwrap();

        let result = handle.write_sync(b"data");
        assert!(result.is_err());
    }

    #[test]
    fn write_all_sync() {
        let handle = IoHandle::buffer();
        handle.write_all_sync(b"complete data").unwrap();

        let content = handle.get_buffer().unwrap();
        assert_eq!(content, b"complete data");
    }

    #[test]
    fn println_sync() {
        let handle = IoHandle::buffer();
        handle.println_sync("line 1").unwrap();
        handle.println_sync("line 2").unwrap();

        let content = handle.get_buffer().unwrap();
        assert_eq!(content, b"line 1\nline 2\n");
    }

    // =========================================================================
    // Debug formatting tests
    // =========================================================================

    #[test]
    fn debug_pipe() {
        let handle = IoHandle::pipe(1024);
        let debug = format!("{handle:?}");
        assert!(debug.contains("Pipe"));
    }

    #[test]
    fn debug_buffer() {
        let handle = IoHandle::buffer();
        let debug = format!("{handle:?}");
        assert!(debug.contains("Buffer"));
    }

    #[test]
    fn debug_null() {
        let handle = IoHandle::null();
        let debug = format!("{handle:?}");
        assert!(debug.contains("Null"));
    }

    #[test]
    fn debug_file() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file("/workspace/test.txt", b"x", Permission::ReadWrite)
            .unwrap();
        let handle = IoHandle::open_read(vfs, "/workspace/test.txt".to_string()).unwrap();
        let debug = format!("{handle:?}");
        assert!(debug.contains("File"));
        assert!(debug.contains("test.txt"));
    }

    // =========================================================================
    // IoError tests
    // =========================================================================

    #[test]
    fn io_error_display() {
        let errors = [
            (IoError::NotFound("/path".into()), "no such file"),
            (IoError::IsDir("/path".into()), "is a directory"),
            (IoError::NotDir("/path".into()), "not a directory"),
            (IoError::Permission("/path".into()), "permission denied"),
            (IoError::Exists("/path".into()), "file exists"),
            (IoError::InvalidSeek, "invalid seek"),
            (IoError::Io("test error".into()), "I/O error"),
        ];

        for (error, expected_fragment) in errors {
            let display = format!("{error}");
            assert!(
                display.contains(expected_fragment),
                "expected '{expected_fragment}' in '{display}'"
            );
        }
    }

    #[test]
    fn io_error_from_vfs() {
        let vfs_error = VfsError::NotFound("/path".into());
        let io_error: IoError = vfs_error.into();
        assert!(matches!(io_error, IoError::Vfs(_)));
    }

    #[test]
    fn io_error_from_std_io() {
        let std_error = std::io::Error::other("test");
        let io_error: IoError = std_error.into();
        assert!(matches!(io_error, IoError::Io(_)));
    }

    #[test]
    fn io_error_to_scheduler() {
        let io_error = IoError::NotFound("/path".into());
        let sched_error: amla_scheduler::Error = io_error.into();
        assert!(matches!(sched_error, amla_scheduler::Error::Command(_)));
    }

    // =========================================================================
    // Stat and DirEntry tests
    // =========================================================================

    #[test]
    fn stat_fields() {
        let stat = Stat {
            size: 1024,
            is_dir: false,
            is_file: true,
        };
        assert_eq!(stat.size, 1024);
        assert!(!stat.is_dir);
        assert!(stat.is_file);
    }

    #[test]
    fn dir_entry_fields() {
        let entry = DirEntry {
            name: "file.txt".into(),
            is_dir: false,
        };
        assert_eq!(entry.name, "file.txt");
        assert!(!entry.is_dir);
    }

    // =========================================================================
    // write_all tests
    // =========================================================================

    #[test]
    fn write_all_buffer() {
        pollster::block_on(async {
            let handle = IoHandle::buffer();
            handle.write_all(b"complete write").await.unwrap();

            let content = handle.get_buffer().unwrap();
            assert_eq!(content, b"complete write");
        });
    }

    #[test]
    fn write_all_null() {
        pollster::block_on(async {
            let handle = IoHandle::null();
            handle.write_all(b"discarded").await.unwrap();
        });
    }

    // =========================================================================
    // Close non-pipe
    // =========================================================================

    #[test]
    fn close_non_pipe() {
        // Calling close on non-pipe handles should be a no-op
        let handle = IoHandle::buffer();
        handle.close(); // Should not panic

        let handle = IoHandle::null();
        handle.close(); // Should not panic
    }

    // =========================================================================
    // HostStream tests
    // =========================================================================

    use amla_scheduler::{HostOpKind, SchedulerState};

    #[test]
    fn host_stdout_buffers_until_newline_or_max_size() {
        let scheduler = test_scheduler();
        let handle = IoHandle::host_stdout(scheduler.clone(), 256);

        // Write without newline - should buffer (no host op until newline or max size)
        scheduler.spawn(async move {
            handle.write(b"hello").await.unwrap();
            // Buffer should have data, but no host op emitted yet
            assert_eq!(handle.get_buffer().unwrap(), b"hello");
            Ok(amla_scheduler::Exit::success())
        });

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        // No host ops should have been emitted (no newline, under max size)
        assert!(!scheduler.has_pending_host_ops());
    }

    #[test]
    fn host_stdout_flushes_at_newline() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        let handle = IoHandle::host_stdout(scheduler.clone(), 256);

        // Write with newline - should trigger flush
        sched_clone.spawn(async move {
            handle.write(b"hello\n").await.unwrap();
            Ok(amla_scheduler::Exit::success())
        });

        // Run until blocked on host op
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Should have Print host op
        let req = scheduler.take_host_op().unwrap();
        assert!(matches!(
            &req.kind,
            HostOpKind::Print { stream: 1, data, .. } if data == b"hello\n"
        ));

        // Complete the host op
        scheduler.complete_host_op(req.id, vec![]);

        // Run to completion
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
    }

    #[test]
    fn host_stdout_flushes_at_max_size() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        // Use small max size to test fallback
        let handle = IoHandle::host_stdout(scheduler.clone(), 5);

        // Write exactly 5 bytes without newline - should trigger flush at max size
        sched_clone.spawn(async move {
            handle.write(b"12345").await.unwrap();
            Ok(amla_scheduler::Exit::success())
        });

        // Run until blocked on host op
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Should have Print host op
        let req = scheduler.take_host_op().unwrap();
        assert!(matches!(
            &req.kind,
            HostOpKind::Print { stream: 1, data, .. } if data == b"12345"
        ));

        // Complete the host op
        scheduler.complete_host_op(req.id, vec![]);

        // Run to completion
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
    }

    #[test]
    fn host_stdout_flushes_multiple_lines() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        let handle = IoHandle::host_stdout(scheduler.clone(), 256);

        // Write multiple lines - should trigger multiple flushes
        sched_clone.spawn(async move {
            handle.write(b"line1\nline2\n").await.unwrap();
            Ok(amla_scheduler::Exit::success())
        });

        // First line
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));
        let req = scheduler.take_host_op().unwrap();
        assert!(matches!(
            &req.kind,
            HostOpKind::Print { stream: 1, data, .. } if data == b"line1\n"
        ));
        scheduler.complete_host_op(req.id, vec![]);

        // Second line
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));
        let req = scheduler.take_host_op().unwrap();
        assert!(matches!(
            &req.kind,
            HostOpKind::Print { stream: 1, data, .. } if data == b"line2\n"
        ));
        scheduler.complete_host_op(req.id, vec![]);

        // Should complete now
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
    }

    #[test]
    fn host_stdout_flush_drains_buffer() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        let handle = IoHandle::host_stdout(scheduler.clone(), 256);

        // Write some data without newline
        sched_clone.clone().spawn({
            let handle = handle.clone();
            async move {
                handle.write(b"partial data").await.unwrap();
                Ok(amla_scheduler::Exit::success())
            }
        });

        // Run write to completion
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        // No host op yet (no newline, under max size)
        assert!(!scheduler.has_pending_host_ops());

        // Now flush
        sched_clone.spawn(async move {
            handle.flush().await.unwrap();
            Ok(amla_scheduler::Exit::success())
        });

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Should have Print host op with remaining buffer
        let req = scheduler.take_host_op().unwrap();
        assert!(matches!(
            &req.kind,
            HostOpKind::Print { stream: 1, data, .. } if data == b"partial data"
        ));

        scheduler.complete_host_op(req.id, vec![]);
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
    }

    #[test]
    fn host_stderr_uses_stream_2() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        let handle = IoHandle::host_stderr(scheduler.clone(), 5);

        sched_clone.spawn(async move {
            handle.write(b"error").await.unwrap();
            Ok(amla_scheduler::Exit::success())
        });

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        let req = scheduler.take_host_op().unwrap();
        assert!(matches!(
            &req.kind,
            HostOpKind::Print { stream: 2, data, .. } if data == b"error"
        ));
    }

    #[test]
    fn host_stdin_reads_from_host() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        let handle = IoHandle::host_stdin(scheduler.clone());

        let result = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let result_clone = result.clone();

        sched_clone.spawn(async move {
            let mut buf = [0u8; 10];
            let n = handle.read(&mut buf).await.unwrap();
            result_clone.borrow_mut().extend_from_slice(&buf[..n]);
            Ok(amla_scheduler::Exit::success())
        });

        // Blocked waiting for stdin
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Should have ReadStdin host op
        let req = scheduler.take_host_op().unwrap();
        assert!(matches!(&req.kind, HostOpKind::ReadStdin { max_bytes: 10 }));

        // Provide input
        scheduler.complete_host_op(req.id, b"user input".to_vec());

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert_eq!(&*result.borrow(), b"user input");
    }

    #[test]
    fn host_stdin_eof() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        let handle = IoHandle::host_stdin(scheduler.clone());

        let got_eof = std::rc::Rc::new(std::cell::RefCell::new(false));
        let got_eof_clone = got_eof.clone();

        sched_clone.spawn(async move {
            let mut buf = [0u8; 10];
            let n = handle.read(&mut buf).await.unwrap();
            *got_eof_clone.borrow_mut() = n == 0;
            Ok(amla_scheduler::Exit::success())
        });

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        let req = scheduler.take_host_op().unwrap();
        // Return empty vec for EOF
        scheduler.complete_host_op(req.id, vec![]);

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(*got_eof.borrow());
    }

    #[test]
    fn host_stdout_cannot_read() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        let handle = IoHandle::host_stdout(scheduler.clone(), 100);

        let got_error = std::rc::Rc::new(std::cell::RefCell::new(false));
        let got_error_clone = got_error.clone();

        sched_clone.spawn(async move {
            let mut buf = [0u8; 10];
            let result = handle.read(&mut buf).await;
            *got_error_clone.borrow_mut() = result.is_err();
            Ok(amla_scheduler::Exit::success())
        });

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(*got_error.borrow(), "reading from stdout should fail");
    }

    #[test]
    fn host_stdin_cannot_write() {
        let scheduler = test_scheduler();
        let sched_clone = scheduler.clone();
        let handle = IoHandle::host_stdin(scheduler.clone());

        let got_error = std::rc::Rc::new(std::cell::RefCell::new(false));
        let got_error_clone = got_error.clone();

        sched_clone.spawn(async move {
            let result = handle.write(b"test").await;
            *got_error_clone.borrow_mut() = result.is_err();
            Ok(amla_scheduler::Exit::success())
        });

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(*got_error.borrow(), "writing to stdin should fail");
    }

    #[test]
    fn host_stream_debug() {
        let scheduler = test_scheduler();
        let stdin = IoHandle::host_stdin(scheduler.clone());
        let stdout = IoHandle::host_stdout(scheduler.clone(), 100);
        let stderr = IoHandle::host_stderr(scheduler, 100);

        assert_eq!(format!("{stdin:?}"), "IoHandle::HostStream(stdin)");
        assert_eq!(format!("{stdout:?}"), "IoHandle::HostStream(stdout)");
        assert_eq!(format!("{stderr:?}"), "IoHandle::HostStream(stderr)");
    }

    #[test]
    fn take_buffered_output_extracts_data() {
        // Regression test: exit-time flush must not lose buffered output.
        // The take_buffered_output() method extracts buffered data synchronously
        // without going through async flush (which could be dropped or fail).
        let scheduler = test_scheduler();
        let stdout = IoHandle::host_stdout(scheduler.clone(), 256);

        // Write data without newline (stays buffered, no flush)
        scheduler.clone().spawn({
            let stdout = stdout.clone();
            async move {
                stdout.write(b"partial output").await.unwrap();
                Ok(amla_scheduler::Exit::success())
            }
        });
        let _ = scheduler.run();

        // No host ops should be pending (data is buffered, not flushed)
        assert!(
            scheduler.take_host_op().is_none(),
            "Data should be buffered, not submitted"
        );

        // Extract buffered output synchronously
        let result = stdout.take_buffered_output();
        assert!(result.is_some(), "Should have buffered data");
        let (stream, data) = result.unwrap();
        assert_eq!(stream, 1, "Should be stdout stream");
        assert_eq!(data, b"partial output", "Should contain all buffered data");

        // Buffer should now be empty
        assert!(
            stdout.take_buffered_output().is_none(),
            "Buffer should be empty after take"
        );
    }

    #[test]
    fn take_buffered_output_returns_none_for_stdin() {
        let scheduler = test_scheduler();
        let stdin = IoHandle::host_stdin(scheduler);

        // stdin has no output buffer
        assert!(
            stdin.take_buffered_output().is_none(),
            "stdin should not have output buffer"
        );
    }

    #[test]
    fn take_buffered_output_returns_none_for_non_host_stream() {
        let pipe = IoHandle::pipe(100);
        let buffer = IoHandle::buffer();
        let null = IoHandle::Null;

        assert!(pipe.take_buffered_output().is_none());
        assert!(buffer.take_buffered_output().is_none());
        assert!(null.take_buffered_output().is_none());
    }

    // =========================================================================
    // is_reader_blocked() tests for IoHandle
    // =========================================================================
    //
    // These tests verify that is_reader_blocked() correctly delegates to the
    // underlying pipe and handles non-pipe variants.

    #[test]
    fn is_reader_blocked_pipe_empty() {
        use amla_scheduler::{AsyncPipe, noop_waker};
        use std::task::Context;

        // Create a pipe handle
        let pipe = AsyncPipe::new(100);
        let handle = IoHandle::Pipe(pipe.clone());

        // Initially not blocked
        assert!(!handle.is_reader_blocked());

        // Read from empty pipe - should set blocked
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            let _ = pipe.poll_read(&mut cx, &mut buf);
        }

        // Now blocked
        assert!(handle.is_reader_blocked());
    }

    #[test]
    fn is_reader_blocked_pipe_with_data() {
        use amla_scheduler::AsyncPipe;

        // Create a pipe handle with data
        let pipe = AsyncPipe::new(100);
        pipe.push(b"hello world");
        let handle = IoHandle::Pipe(pipe);

        // Not blocked - has data
        assert!(!handle.is_reader_blocked());
    }

    #[test]
    fn is_reader_blocked_pipe_closed() {
        use amla_scheduler::{AsyncPipe, noop_waker};
        use std::task::Context;

        // Create a pipe handle
        let pipe = AsyncPipe::new(100);
        let handle = IoHandle::Pipe(pipe.clone());

        // Read from empty pipe - sets blocked
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            let _ = pipe.poll_read(&mut cx, &mut buf);
        }
        assert!(handle.is_reader_blocked());

        // Close pipe - clears blocked
        pipe.close();
        assert!(!handle.is_reader_blocked());
    }

    #[test]
    fn is_reader_blocked_non_pipe_variants() {
        // Buffer - never blocked
        let buffer = IoHandle::buffer();
        assert!(!buffer.is_reader_blocked());

        // Null - never blocked
        let null = IoHandle::null();
        assert!(!null.is_reader_blocked());

        // File - never blocked
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file("/workspace/test.txt", b"content", Permission::ReadWrite)
            .unwrap();
        let file = IoHandle::open_read(vfs, "/workspace/test.txt".to_string()).unwrap();
        assert!(!file.is_reader_blocked());
    }

    #[test]
    fn is_reader_blocked_cleared_after_push() {
        use amla_scheduler::{AsyncPipe, noop_waker};
        use std::task::Context;

        // Create a pipe handle
        let pipe = AsyncPipe::new(100);
        let handle = IoHandle::Pipe(pipe.clone());

        // Read from empty pipe - sets blocked
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 10];
            let _ = pipe.poll_read(&mut cx, &mut buf);
        }
        assert!(handle.is_reader_blocked());

        // Push data through IoHandle - should clear blocked
        handle.push(b"data").unwrap();
        assert!(!handle.is_reader_blocked());
    }

    #[test]
    fn is_reader_blocked_stdin_simulation() {
        // Simulate stdin pipe behavior as used in runtime
        use amla_scheduler::{AsyncPipe, noop_waker};
        use std::task::Context;

        // Scenario: command reads from stdin, stdin is empty
        let pipe = AsyncPipe::new(64 * 1024);
        let stdin = IoHandle::Pipe(pipe.clone());

        // Initial state - not blocked
        assert!(!stdin.is_reader_blocked());

        // Command tries to read - blocks
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 1024];
            let _ = pipe.poll_read(&mut cx, &mut buf);
        }
        assert!(stdin.is_reader_blocked());

        // User provides stdin data via push_all
        stdin.push_all(b"input from user\n").unwrap();
        assert!(!stdin.is_reader_blocked());

        // Command reads the data
        pollster::block_on(async {
            let mut buf = [0u8; 100];
            let n = stdin.read(&mut buf).await.unwrap();
            assert_eq!(n, 16);
            assert_eq!(&buf[..n], b"input from user\n");
        });
        assert!(!stdin.is_reader_blocked());

        // Command tries to read more - blocks again
        {
            let waker = noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut buf = [0u8; 1024];
            let _ = pipe.poll_read(&mut cx, &mut buf);
        }
        assert!(stdin.is_reader_blocked());

        // User closes stdin (EOF)
        stdin.close();
        assert!(!stdin.is_reader_blocked());

        // Command reads EOF
        pollster::block_on(async {
            let mut buf = [0u8; 100];
            let n = stdin.read(&mut buf).await.unwrap();
            assert_eq!(n, 0); // EOF
        });
        assert!(!stdin.is_reader_blocked());
    }
}
