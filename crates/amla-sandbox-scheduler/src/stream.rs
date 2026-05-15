//! Unified async stream interface.
//!
//! Provides traits and wrappers for async I/O. The VFS implements these
//! for files, pipes, devices, etc.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::{AsyncPipe, Error};

/// Async read trait.
pub trait AsyncRead {
    /// Read into buffer, returns bytes read (0 = EOF).
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Error>>;
}

/// Async write trait.
pub trait AsyncWrite {
    /// Write from buffer, returns bytes written.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>>;

    /// Close the stream.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>>;
}

/// Type-erased reader.
pub type BoxReader = Pin<Box<dyn AsyncRead>>;

/// Type-erased writer.
pub type BoxWriter = Pin<Box<dyn AsyncWrite>>;

// AsyncPipe implements both traits
//
// IMPORTANT: We call poll_read/poll_write directly on the pipe instead of
// creating temporary PipeRead/PipeWrite futures. This is critical because
// those futures have Drop impls that clear wakers when dropped, which would
// immediately invalidate any waker we just registered.

impl AsyncRead for AsyncPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        // Call poll_read directly - don't create a PipeRead future because
        // its Drop impl would clear the waker we just registered
        match self.get_mut().poll_read(cx, buf) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(Error::Io(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for AsyncPipe {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        // Call poll_write directly - don't create a PipeWrite future because
        // its Drop impl would clear the waker we just registered
        match self.get_mut().poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(Error::Io(e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.get_mut().close();
        Poll::Ready(Ok(()))
    }
}

/// Ergonomic read stream wrapper.
pub struct ReadStream {
    inner: BoxReader,
}

impl ReadStream {
    /// Wrap any `AsyncRead`.
    pub fn new<R: AsyncRead + 'static>(reader: R) -> Self {
        Self {
            inner: Box::pin(reader),
        }
    }

    /// Read into buffer.
    pub fn read<'a>(
        &'a mut self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = Result<usize, Error>> + 'a {
        ReadFut { stream: self, buf }
    }

    /// Read all to end.
    pub async fn read_to_end(&mut self, out: &mut Vec<u8>) -> Result<usize, Error> {
        let mut total = 0;
        let mut buf = [0u8; 4096];
        loop {
            let n = self.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            total += n;
        }
        Ok(total)
    }
}

struct ReadFut<'a> {
    stream: &'a mut ReadStream,
    buf: &'a mut [u8],
}

impl Future for ReadFut<'_> {
    type Output = Result<usize, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        this.stream.inner.as_mut().poll_read(cx, this.buf)
    }
}

/// Ergonomic write stream wrapper.
pub struct WriteStream {
    inner: BoxWriter,
}

impl WriteStream {
    /// Wrap any `AsyncWrite`.
    pub fn new<W: AsyncWrite + 'static>(writer: W) -> Self {
        Self {
            inner: Box::pin(writer),
        }
    }

    /// Write buffer.
    pub fn write<'a>(
        &'a mut self,
        buf: &'a [u8],
    ) -> impl Future<Output = Result<usize, Error>> + 'a {
        WriteFut { stream: self, buf }
    }

    /// Write all bytes.
    pub async fn write_all(&mut self, buf: &[u8]) -> Result<(), Error> {
        let mut pos = 0;
        while pos < buf.len() {
            let n = self.write(&buf[pos..]).await?;
            if n == 0 {
                return Err(Error::Command("write returned 0".into()));
            }
            pos += n;
        }
        Ok(())
    }

    /// Close the stream.
    pub async fn close(&mut self) -> Result<(), Error> {
        CloseFut { stream: self }.await
    }
}

struct WriteFut<'a> {
    stream: &'a mut WriteStream,
    buf: &'a [u8],
}

impl Future for WriteFut<'_> {
    type Output = Result<usize, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        this.stream.inner.as_mut().poll_write(cx, this.buf)
    }
}

struct CloseFut<'a> {
    stream: &'a mut WriteStream,
}

impl Future for CloseFut<'_> {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream.inner.as_mut().poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Scheduler;
    use crate::host_channel::{RandomSourceFn, TimeSourceFn};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// Create a scheduler with mock sources for testing.
    fn test_scheduler() -> Scheduler {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    #[test]
    fn pipe_as_stream() {
        let sched = test_scheduler();
        let sched_clone = sched.clone();

        let pipe = AsyncPipe::new(64);
        let pipe_r = pipe.clone();
        let pipe_w = pipe.clone();

        let received = Rc::new(RefCell::new(Vec::new()));
        let received_clone = received.clone();

        sched.spawn(async move {
            let writer = sched_clone.spawn(async move {
                let mut stream = WriteStream::new(pipe_w);
                stream.write_all(b"hello").await?;
                stream.close().await?;
                Ok(crate::Exit::success())
            });

            let reader = sched_clone.spawn(async move {
                let mut stream = ReadStream::new(pipe_r);
                let mut buf = Vec::new();
                stream.read_to_end(&mut buf).await?;
                received_clone.borrow_mut().extend(buf);
                Ok(crate::Exit::success())
            });

            let _ = crate::join_all(vec![writer, reader]).await;
            Ok(crate::Exit::success())
        });

        let state = sched.run();

        // Debug: if blocked, check pipe state
        #[cfg(feature = "random-scheduling")]
        if matches!(state, crate::SchedulerState::Blocked) {
            eprintln!(
                "DEBUG: Blocked! pipe.len={}, pipe.is_closed={}, pipe.is_empty={}",
                pipe.len(),
                pipe.is_closed(),
                pipe.is_empty()
            );
        }

        assert!(
            matches!(state, crate::SchedulerState::Done),
            "Expected Done, got {state:?}"
        );
        assert_eq!(&*received.borrow(), b"hello");
    }
}
