//! Async execution context for commands.
//!
//! Provides async I/O operations that integrate with the executor.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::pipe::AsyncPipe;
use crate::{Error, Exit, SideEffects};

/// Async execution context for a command.
///
/// Provides async read/write operations on handles.
pub struct AsyncContext {
    /// Standard input (pipe or cursor).
    stdin: IoSource,
    /// Standard output (pipe or buffer).
    stdout: IoSink,
    /// Standard error (buffer).
    stderr: Vec<u8>,
    /// Current working directory.
    cwd: String,
    /// Collected side effects.
    effects: SideEffects,
}

/// Input source for a command.
enum IoSource {
    /// Read from a pipe.
    Pipe(AsyncPipe),
    /// Read from a memory buffer.
    Cursor { data: Vec<u8>, pos: usize },
}

/// Output sink for a command.
enum IoSink {
    /// Write to a pipe.
    Pipe(AsyncPipe),
    /// Write to a memory buffer.
    Buffer(Vec<u8>),
}

impl AsyncContext {
    /// Create a context with pipes.
    #[must_use]
    pub fn with_pipes(stdin: AsyncPipe, stdout: AsyncPipe, cwd: &str) -> Self {
        Self {
            stdin: IoSource::Pipe(stdin),
            stdout: IoSink::Pipe(stdout),
            stderr: Vec::new(),
            cwd: cwd.to_string(),
            effects: SideEffects::default(),
        }
    }

    /// Create a context with memory buffers (for testing).
    #[must_use]
    pub fn with_buffers(stdin: Vec<u8>, cwd: &str) -> Self {
        Self {
            stdin: IoSource::Cursor {
                data: stdin,
                pos: 0,
            },
            stdout: IoSink::Buffer(Vec::new()),
            stderr: Vec::new(),
            cwd: cwd.to_string(),
            effects: SideEffects::default(),
        }
    }

    /// Create a context with buffer stdin and pipe stdout.
    /// Used for first command in pipeline.
    #[must_use]
    pub fn with_buffer_in_pipe_out(stdin: Vec<u8>, stdout: AsyncPipe, cwd: &str) -> Self {
        Self {
            stdin: IoSource::Cursor {
                data: stdin,
                pos: 0,
            },
            stdout: IoSink::Pipe(stdout),
            stderr: Vec::new(),
            cwd: cwd.to_string(),
            effects: SideEffects::default(),
        }
    }

    /// Create a context with pipe stdin and buffer stdout.
    /// Used for last command in pipeline.
    #[must_use]
    pub fn with_pipe_in_buffer_out(stdin: AsyncPipe, cwd: &str) -> Self {
        Self {
            stdin: IoSource::Pipe(stdin),
            stdout: IoSink::Buffer(Vec::new()),
            stderr: Vec::new(),
            cwd: cwd.to_string(),
            effects: SideEffects::default(),
        }
    }

    /// Get current working directory.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Read from stdin.
    pub fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> ReadFuture<'a> {
        ReadFuture { ctx: self, buf }
    }

    /// Write to stdout.
    pub fn write<'a>(&'a mut self, data: &'a [u8]) -> WriteFuture<'a> {
        WriteFuture { ctx: self, data }
    }

    /// Write all data to stdout.
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            written += self.write(&data[written..]).await?;
        }
        Ok(())
    }

    /// Write to stderr.
    pub fn write_stderr(&mut self, data: &[u8]) {
        self.stderr.extend_from_slice(data);
    }

    /// Write line to stderr.
    pub fn writeln_stderr(&mut self, msg: &str) {
        self.stderr.extend_from_slice(msg.as_bytes());
        self.stderr.push(b'\n');
    }

    /// Set cwd (as side effect).
    pub fn set_cwd(&mut self, path: String) {
        self.effects.cwd = Some(path);
    }

    /// Set environment variable (as side effect).
    pub fn set_env(&mut self, key: String, value: String) {
        self.effects.env_set.push((key, value));
    }

    /// Take collected side effects.
    pub fn take_effects(&mut self) -> SideEffects {
        std::mem::take(&mut self.effects)
    }

    /// Take stderr contents.
    pub fn take_stderr(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stderr)
    }

    /// Take stdout contents (only works with buffer sink).
    pub fn take_stdout(&mut self) -> Vec<u8> {
        match &mut self.stdout {
            IoSink::Buffer(buf) => std::mem::take(buf),
            IoSink::Pipe(_) => Vec::new(),
        }
    }

    /// Close stdout (signal EOF to downstream).
    pub fn close_stdout(&mut self) {
        if let IoSink::Pipe(pipe) = &self.stdout {
            pipe.close();
        }
    }

    /// Poll read from stdin.
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        match &mut self.stdin {
            IoSource::Pipe(pipe) => pipe.poll_read(cx, buf),
            IoSource::Cursor { data, pos } => {
                let remaining = &data[*pos..];
                if remaining.is_empty() {
                    return Poll::Ready(Ok(0)); // EOF
                }
                let n = buf.len().min(remaining.len());
                buf[..n].copy_from_slice(&remaining[..n]);
                *pos += n;
                Poll::Ready(Ok(n))
            }
        }
    }

    /// Poll write to stdout.
    fn poll_write(&mut self, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        match &mut self.stdout {
            IoSink::Pipe(pipe) => pipe.poll_write(cx, data),
            IoSink::Buffer(buf) => {
                buf.extend_from_slice(data);
                Poll::Ready(Ok(data.len()))
            }
        }
    }
}

/// Future for reading from context.
pub struct ReadFuture<'a> {
    ctx: &'a mut AsyncContext,
    buf: &'a mut [u8],
}

impl Future for ReadFuture<'_> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        this.ctx.poll_read(cx, this.buf)
    }
}

/// Future for writing to context.
pub struct WriteFuture<'a> {
    ctx: &'a mut AsyncContext,
    data: &'a [u8],
}

impl Future for WriteFuture<'_> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        this.ctx.poll_write(cx, this.data)
    }
}

/// Async command trait.
pub trait AsyncCommand: Send + Sync {
    /// Command name.
    fn name(&self) -> &'static str;

    /// Command description.
    fn description(&self) -> &'static str;

    /// Execute the command.
    fn execute<'a>(
        &'a self,
        ctx: &'a mut AsyncContext,
        args: &'a [&'a str],
    ) -> Pin<Box<dyn Future<Output = Result<Exit, Error>> + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::Executor;

    #[test]
    fn context_read_from_buffer() {
        let exec = Executor::new();

        exec.spawn(async {
            let mut ctx = AsyncContext::with_buffers(b"hello".to_vec(), "/");
            let mut buf = [0u8; 10];
            let n = ctx.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(&buf[..n], b"hello");

            // Read again - should get EOF
            let n = ctx.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);

            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn context_write_to_buffer() {
        let exec = Executor::new();

        exec.spawn(async {
            let mut ctx = AsyncContext::with_buffers(vec![], "/");
            ctx.write_all(b"hello world").await.unwrap();
            assert_eq!(ctx.take_stdout(), b"hello world");
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn context_pipe_read_write() {
        let exec = Executor::new();
        let pipe = AsyncPipe::new(64);
        let pipe_clone = pipe.clone();

        // Writer task
        exec.spawn(async move {
            let mut ctx = AsyncContext::with_buffer_in_pipe_out(vec![], pipe_clone, "/");
            ctx.write_all(b"hello from writer").await.unwrap();
            ctx.close_stdout();
            Ok(Exit::success())
        });

        // Reader task
        exec.spawn(async move {
            let mut ctx = AsyncContext::with_pipe_in_buffer_out(pipe, "/");
            let mut buf = [0u8; 32];
            let mut total = Vec::new();

            loop {
                let n = ctx.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                total.extend_from_slice(&buf[..n]);
            }

            assert_eq!(total, b"hello from writer");
            Ok(Exit::success())
        });

        let _ = exec.run();
    }
}
