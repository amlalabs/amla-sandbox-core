//! Abstract syntax tree for shell commands.

use smallvec::SmallVec;

use crate::lexer::Word;

/// A shell command.
///
/// Note: The `Simple` variant is large (~848 bytes) due to inline SmallVec storage.
/// This is intentional - it avoids heap allocation for typical commands (2-8 args),
/// and Command values are short-lived (parsed, executed, dropped).
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Simple command: `ls -la /tmp`
    Simple {
        /// Command and arguments (typically 2-8, inline storage avoids heap).
        argv: SmallVec<[Word; 8]>,
        /// I/O redirections (typically 0-2).
        redirects: SmallVec<[Redirect; 2]>,
    },

    /// Pipeline: `cat file | grep foo | wc -l`
    Pipeline {
        /// Commands in the pipeline (uses Vec for indirection since Command is recursive).
        commands: Vec<Command>,
    },

    /// And list: `cmd1 && cmd2`
    And {
        left: Box<Command>,
        right: Box<Command>,
    },

    /// Or list: `cmd1 || cmd2`
    Or {
        left: Box<Command>,
        right: Box<Command>,
    },

    /// Sequence: `cmd1 ; cmd2`
    Sequence {
        /// Commands in sequence (uses Vec for indirection since Command is recursive).
        commands: Vec<Command>,
    },

    /// Background: `cmd &`
    Background { command: Box<Command> },

    /// Subshell: `(cmd1 | cmd2)`
    Subshell { command: Box<Command> },

    /// Empty command (e.g., blank line).
    Empty,
}

impl Command {
    /// Create a simple command from argv.
    pub fn simple(argv: impl Into<SmallVec<[Word; 8]>>) -> Self {
        Command::Simple {
            argv: argv.into(),
            redirects: SmallVec::new(),
        }
    }

    /// Create a simple command with redirects.
    pub fn simple_with_redirects(
        argv: impl Into<SmallVec<[Word; 8]>>,
        redirects: impl Into<SmallVec<[Redirect; 2]>>,
    ) -> Self {
        Command::Simple {
            argv: argv.into(),
            redirects: redirects.into(),
        }
    }

    /// Create a pipeline.
    pub fn pipeline(commands: Vec<Command>) -> Self {
        if commands.len() == 1 {
            commands.into_iter().next().unwrap()
        } else {
            Command::Pipeline { commands }
        }
    }

    /// Check if this is an empty command.
    pub fn is_empty(&self) -> bool {
        matches!(self, Command::Empty)
    }
}

/// I/O redirection.
#[derive(Debug, Clone)]
pub struct Redirect {
    /// Source file descriptor (0=stdin, 1=stdout, 2=stderr).
    pub source_fd: i32,
    /// Redirect operation.
    pub kind: RedirectKind,
    /// Target (file or fd).
    pub target: RedirectTarget,
}

/// Type of redirect operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    /// Write (truncate): `>`
    Write,
    /// Append: `>>`
    Append,
    /// Read: `<`
    Read,
}

/// Target of a redirect.
#[derive(Debug, Clone)]
pub enum RedirectTarget {
    /// Redirect to/from a file.
    File(Word),
    /// Duplicate another file descriptor (e.g., `2>&1`).
    Fd(i32),
    /// Here document content.
    ///
    /// The content is the literal text to provide as stdin.
    /// `strip_tabs` indicates whether leading tabs should be stripped (`<<-`).
    HereDoc {
        /// The heredoc content (lines joined with newlines).
        content: String,
        /// Whether to strip leading tabs (from `<<-` syntax).
        strip_tabs: bool,
    },
}

impl Redirect {
    /// Create stdout write redirect: `> file`
    pub fn stdout_write(path: Word) -> Self {
        Redirect {
            source_fd: 1,
            kind: RedirectKind::Write,
            target: RedirectTarget::File(path),
        }
    }

    /// Create stdout append redirect: `>> file`
    pub fn stdout_append(path: Word) -> Self {
        Redirect {
            source_fd: 1,
            kind: RedirectKind::Append,
            target: RedirectTarget::File(path),
        }
    }

    /// Create stdin read redirect: `< file`
    pub fn stdin_read(path: Word) -> Self {
        Redirect {
            source_fd: 0,
            kind: RedirectKind::Read,
            target: RedirectTarget::File(path),
        }
    }

    /// Create stderr write redirect: `2> file`
    pub fn stderr_write(path: Word) -> Self {
        Redirect {
            source_fd: 2,
            kind: RedirectKind::Write,
            target: RedirectTarget::File(path),
        }
    }

    /// Create stderr append redirect: `2>> file`
    pub fn stderr_append(path: Word) -> Self {
        Redirect {
            source_fd: 2,
            kind: RedirectKind::Append,
            target: RedirectTarget::File(path),
        }
    }

    /// Create fd duplication: `2>&1`
    pub fn dup_fd(source: i32, target: i32) -> Self {
        Redirect {
            source_fd: source,
            kind: RedirectKind::Write, // Kind doesn't matter for dup
            target: RedirectTarget::Fd(target),
        }
    }

    /// Create stdin heredoc: `<<EOF` or `<<-EOF`
    pub fn stdin_heredoc(content: String, strip_tabs: bool) -> Self {
        Redirect {
            source_fd: 0,
            kind: RedirectKind::Read,
            target: RedirectTarget::HereDoc {
                content,
                strip_tabs,
            },
        }
    }
}
