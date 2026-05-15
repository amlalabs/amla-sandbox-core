//! # amla-scheduler
//!
//! Single-threaded async executor for WASM-compatible shell execution.
//!
//! This crate provides the core async infrastructure for running pipelines
//! of commands without requiring tokio or async-std. It's designed to be
//! fully WASM-compatible.
//!
//! ## Thread Safety
//!
//! This scheduler is designed for **single-threaded use** (e.g., WASM environments).
//! All core types ([`Scheduler`], [`Executor`], [`AsyncPipe`], [`HostChannel`]) are
//! `!Send` and `!Sync` due to their internal use of `Rc<RefCell<_>>`.
//!
//! **Do not attempt to share these types across threads.** If you need multi-threaded
//! execution, spawn separate scheduler instances per thread.
//!
//! ## Safety
//!
//! This is the **only** crate in the amla workspace that uses `unsafe` code.
//! All other crates use `#![forbid(unsafe_code)]`. The unsafe code here is
//! limited to:
//!
//! - `waker.rs`: Custom `RawWaker` implementation for the executor
//! - `stream.rs`: `Pin::new_unchecked` for pinning futures in place
//!
//! These are necessary for implementing a custom async executor without
//! depending on tokio or async-std.
//!
//! ## Components
//!
//! - [`Executor`]: Single-threaded task executor with custom wakers
//! - [`AsyncPipe`]: Bounded async pipe with backpressure
//! - [`AsyncContext`]: Execution context for commands
//! - [`HostChannel`]: Channel for host operations (file I/O, etc.)
//!
//! ## Example
//!
//! ```rust
//! use amla_scheduler::{Executor, Exit, Error};
//!
//! let exec = Executor::new();
//!
//! exec.spawn(async {
//!     // Do some async work
//!     Ok(Exit::success())
//! });
//!
//! let results = exec.run();
//! ```
//!
//! ## Host Operations
//!
//! ```rust,ignore
//! use amla_scheduler::{Executor, Exit, HostChannel};
//!
//! let exec = Executor::new();
//! let channel = HostChannel::new(10);  // Bounded queue of 10
//! let channel_clone = channel.clone();
//!
//! exec.spawn(async move {
//!     // File read appears as a future
//!     let data = channel_clone.file_read("/test.txt").await?;
//!     println!("Got {} bytes", data.len());
//!     Ok(Exit::success())
//! });
//!
//! // Run until blocked on host op
//! exec.run();
//!
//! // Runtime processes the request
//! let req = channel.take_pending().unwrap();
//! channel.complete(req.id, b"file contents".to_vec());
//!
//! // Continue execution
//! exec.run();
//! ```

mod context;
mod executor;
mod host_channel;
mod pipe;
mod scheduler;
mod spawner;
mod stream;
mod timer;
mod vfs_stream;
mod waker;

#[cfg(test)]
mod tests;

// Re-export smallvec for use by dependents (SideEffects uses it)
pub use smallvec::SmallVec;

// Primary API - the unified Scheduler
pub use scheduler::{
    HostChannelRef, JoinAll, Scheduler, SchedulerState, SelectFirst, TaskHandle, TaskId, join_all,
    noop_waker, select_first,
};

// Lower-level APIs (for advanced use)
pub use context::{AsyncCommand, AsyncContext};
pub use executor::{Executor, RunState, TaskResult};
pub use host_channel::{
    ClockType, HostChannel, HostOpFuture, HostOpId, HostOpKind, HostOpRequest, JoinAllFuture,
    RandomSourceFn, SelectFirstFuture, TaskIdRepr, TimeSourceFn, join_all as join_all_host_ops,
    select_first as select_first_host_ops,
};
pub use pipe::AsyncPipe;
pub use spawner::{
    JoinAllTasks, Pipeline, SelectFirstTask, SpawnRequest, Spawner, TaskSlot, join_all_tasks,
    select_first_task,
};
pub use stream::{AsyncRead, AsyncWrite, BoxReader, BoxWriter, ReadStream, WriteStream};
pub use timer::{SleepFuture, TimeNanos, duration_to_nanos, nanos_to_duration};
pub use vfs_stream::{VfsReader, VfsWriter};

/// Command exit status.
#[derive(Debug, Clone)]
pub struct Exit {
    /// Exit code (0 = success).
    pub code: i32,
    /// Side effects to apply.
    pub effects: SideEffects,
}

impl Exit {
    /// Successful exit.
    #[must_use]
    pub fn success() -> Self {
        Self {
            code: 0,
            effects: SideEffects::default(),
        }
    }

    /// Exit with code.
    #[must_use]
    pub fn code(code: i32) -> Self {
        Self {
            code,
            effects: SideEffects::default(),
        }
    }

    /// Exit with cwd change.
    #[must_use]
    pub fn with_cwd(path: String) -> Self {
        Self {
            code: 0,
            effects: SideEffects {
                cwd: Some(path),
                ..Default::default()
            },
        }
    }
}

/// Side effects from command execution.
#[derive(Debug, Clone, Default)]
pub struct SideEffects {
    /// New working directory.
    pub cwd: Option<String>,
    /// Environment variables to set (typically 0-2 per command).
    pub env_set: SmallVec<[(String, String); 2]>,
    /// Environment variables to unset (typically 0-1 per command).
    pub env_unset: SmallVec<[String; 2]>,
    /// Set pipefail option (None = no change, Some(true) = enable, Some(false) = disable).
    pub pipefail: Option<bool>,
}

/// Error type for async commands.
#[derive(Debug)]
pub enum Error {
    /// I/O error.
    Io(std::io::Error),
    /// Command-specific error.
    Command(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Command(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for Error {}
