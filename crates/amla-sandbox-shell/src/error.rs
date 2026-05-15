//! Shell error types.

use thiserror::Error;

/// Shell error type.
#[derive(Debug, Error)]
pub enum ShellError {
    /// Command not found.
    #[error("command not found: {0}")]
    CommandNotFound(String),

    /// Syntax error in input.
    #[error("syntax error: {message}")]
    Syntax { message: String, position: usize },

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// VFS error.
    #[error("filesystem error: {0}")]
    Vfs(#[from] amla_vfs::VfsError),

    /// Argument parsing error.
    #[error("argument error: {0}")]
    Arg(String),

    /// Job not found.
    #[error("job not found: %{0}")]
    JobNotFound(usize),

    /// Redirect error.
    #[error("redirect error: {0}")]
    Redirect(String),

    /// Scheduler error.
    #[error("scheduler error: {0}")]
    Scheduler(String),

    /// Pipe error.
    #[error("broken pipe")]
    BrokenPipe,
}

impl From<lexopt::Error> for ShellError {
    fn from(e: lexopt::Error) -> Self {
        ShellError::Arg(e.to_string())
    }
}

/// Result type for shell operations.
pub type Result<T> = std::result::Result<T, ShellError>;
