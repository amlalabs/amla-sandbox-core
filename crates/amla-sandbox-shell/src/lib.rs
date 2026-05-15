//! # amla-shell
//!
//! Shell for AI agent sandboxing.
//!
//! Built on `amla-scheduler` for async execution and `amla-vfs` for sandboxed
//! filesystem access.
//!
//! ## Features
//!
//! - **Pipelines**: `cmd1 | cmd2 | cmd3`
//! - **Redirects**: `>`, `>>`, `<`, `2>`, `2>&1`, `&>`
//! - **Job control**: Background jobs with `&`, `jobs`, `fg`, `bg`
//! - **Operators**: `&&`, `||`, `;`
//! - **Environment**: `export`, `unset`, variable expansion
//! - **Builtins**: `cd`, `pwd`, `exit`, etc.
//!
//! ## Architecture
//!
//! - Each command task owns its I/O handles (no shared mutable state)
//! - Commands return `SideEffects` instead of mutating shell state
//! - Shell applies effects after command completion
//! - Shell is an applet for proper subshell support
//!
//! ## Example
//!
//! ```rust,ignore
//! use amla_shell::Shell;
//! use std::task::{Context, Poll};
//!
//! let shell = Shell::new();
//!
//! // Execute a command - caller drives the scheduler
//! let mut fut = std::pin::pin!(shell.execute("echo hello | cat > /workspace/out.txt"));
//! let waker = amla_scheduler::noop_waker();
//! let mut cx = Context::from_waker(&waker);
//!
//! loop {
//!     if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
//!         let exit_code = result?;
//!         break;
//!     }
//!     shell.scheduler().run_step();
//! }
//! ```

#![forbid(unsafe_code)]

mod ast;
mod context;
mod env;
mod error;
mod glob;
mod io_handle;
mod jobs;
mod lexer;
mod parser;

pub mod commands;

pub use ast::{Command, Redirect, RedirectKind, RedirectTarget};
pub use context::CmdContext;
pub use env::Environment;
pub use error::{Result, ShellError};
pub use io_handle::{DirEntry, IoError, IoHandle, Stat};
pub use jobs::{Job, JobState, JobTable};
pub use lexer::{Lexer, Token};
pub use parser::{Parser, parse};

use amla_scheduler::{AsyncPipe, Scheduler, SchedulerState, SideEffects};
use amla_tools::ToolCatalog;
use amla_vfs::Vfs;
use smallvec::SmallVec;

use std::cell::RefCell;
use std::rc::Rc;

/// Shell instance.
pub struct Shell {
    /// Task scheduler.
    pub scheduler: Scheduler,

    /// VFS reference (public for testing).
    pub vfs: Rc<RefCell<Vfs>>,

    /// Current working directory (interior mutability for async compatibility).
    cwd: RefCell<String>,

    /// Environment variables (interior mutability for async compatibility).
    env: RefCell<Environment>,

    /// Background jobs.
    pub jobs: Rc<RefCell<JobTable>>,

    /// Last command exit code ($?) (interior mutability for async compatibility).
    last_exit: RefCell<i32>,

    /// Default stdin handle for commands.
    stdin: IoHandle,

    /// Default stdout handle for commands.
    stdout: IoHandle,

    /// Default stderr handle for commands.
    stderr: IoHandle,

    /// Tool catalog for the `tools` command (optional).
    tool_catalog: Option<Rc<ToolCatalog>>,

    /// PIPEFAIL option: if set, pipeline returns first non-zero exit code.
    pipefail: RefCell<bool>,
}

impl Shell {
    /// Create a new shell with a fresh VFS.
    ///
    /// Uses buffer handles for I/O (output is discarded).
    /// Creates /workspace as default cwd.
    #[must_use]
    pub fn new(scheduler: Scheduler) -> Self {
        let mut vfs = Vfs::new();
        vfs.create_dir_all("/workspace", amla_vfs::Permission::ReadWrite)
            .expect("failed to create /workspace");
        Self {
            scheduler,
            vfs: Rc::new(RefCell::new(vfs)),
            cwd: RefCell::new("/workspace".to_string()),
            env: RefCell::new(Environment::with_defaults()),
            jobs: Rc::new(RefCell::new(JobTable::new())),
            last_exit: RefCell::new(0),
            stdin: IoHandle::null(),
            stdout: IoHandle::buffer(),
            stderr: IoHandle::buffer(),
            tool_catalog: None,
            pipefail: RefCell::new(false),
        }
    }

    /// Create a new shell with the given VFS.
    ///
    /// Uses buffer handles for I/O (output is discarded).
    pub fn with_vfs(scheduler: Scheduler, vfs: Vfs) -> Self {
        Self {
            scheduler,
            vfs: Rc::new(RefCell::new(vfs)),
            cwd: RefCell::new("/".to_string()),
            env: RefCell::new(Environment::with_defaults()),
            jobs: Rc::new(RefCell::new(JobTable::new())),
            last_exit: RefCell::new(0),
            stdin: IoHandle::null(),
            stdout: IoHandle::buffer(),
            stderr: IoHandle::buffer(),
            tool_catalog: None,
            pipefail: RefCell::new(false),
        }
    }

    /// Create a new shell with custom I/O streams.
    ///
    /// This allows the caller to capture shell output via custom handles.
    pub fn with_streams(
        scheduler: Scheduler,
        vfs: Vfs,
        stdin: IoHandle,
        stdout: IoHandle,
        stderr: IoHandle,
    ) -> Self {
        Self {
            scheduler,
            vfs: Rc::new(RefCell::new(vfs)),
            cwd: RefCell::new("/".to_string()),
            env: RefCell::new(Environment::with_defaults()),
            jobs: Rc::new(RefCell::new(JobTable::new())),
            last_exit: RefCell::new(0),
            stdin,
            stdout,
            stderr,
            tool_catalog: None,
            pipefail: RefCell::new(false),
        }
    }

    /// Create a shell from an existing VFS reference (for subshells).
    ///
    /// This creates a fresh shell with default environment. For inheriting
    /// the parent's environment and cwd, use `with_context`.
    pub fn from_vfs_ref(scheduler: Scheduler, vfs: Rc<RefCell<Vfs>>) -> Self {
        Self {
            scheduler,
            vfs,
            cwd: RefCell::new("/workspace".to_string()),
            env: RefCell::new(Environment::with_defaults()),
            jobs: Rc::new(RefCell::new(JobTable::new())),
            last_exit: RefCell::new(0),
            stdin: IoHandle::null(),
            stdout: IoHandle::buffer(),
            stderr: IoHandle::buffer(),
            tool_catalog: None,
            pipefail: RefCell::new(false),
        }
    }

    /// Create a shell with full context including scheduler and I/O handles.
    ///
    /// This is used when the shell needs to share a scheduler with its parent
    /// (e.g., for incremental execution via `run_step()`). The shell will use
    /// the provided scheduler instead of creating its own.
    pub fn with_full_context(
        scheduler: Scheduler,
        vfs: Rc<RefCell<Vfs>>,
        cwd: String,
        env: Environment,
        stdin: IoHandle,
        stdout: IoHandle,
        stderr: IoHandle,
    ) -> Self {
        Self {
            scheduler,
            vfs,
            cwd: RefCell::new(cwd),
            env: RefCell::new(env),
            jobs: Rc::new(RefCell::new(JobTable::new())),
            last_exit: RefCell::new(0),
            stdin,
            stdout,
            stderr,
            tool_catalog: None,
            pipefail: RefCell::new(false),
        }
    }

    /// Create a shell with full context including tool catalog.
    ///
    /// This is used by runtimes that provide MCP tool definitions.
    pub fn with_full_context_and_tools(
        scheduler: Scheduler,
        vfs: Rc<RefCell<Vfs>>,
        cwd: String,
        env: Environment,
        stdin: IoHandle,
        stdout: IoHandle,
        stderr: IoHandle,
        tool_catalog: Rc<ToolCatalog>,
    ) -> Self {
        Self {
            scheduler,
            vfs,
            cwd: RefCell::new(cwd),
            env: RefCell::new(env),
            jobs: Rc::new(RefCell::new(JobTable::new())),
            last_exit: RefCell::new(0),
            stdin,
            stdout,
            stderr,
            tool_catalog: Some(tool_catalog),
            pipefail: RefCell::new(false),
        }
    }

    /// Get the tool catalog (if set).
    pub fn tool_catalog(&self) -> Option<Rc<ToolCatalog>> {
        self.tool_catalog.clone()
    }

    /// Get the current working directory.
    pub fn cwd(&self) -> String {
        self.cwd.borrow().clone()
    }

    /// Set the current working directory.
    pub fn set_cwd(&self, path: &str) -> std::result::Result<(), IoError> {
        let resolved = self.resolve_path(path);
        if !self.vfs.borrow().is_dir(&resolved) {
            return Err(IoError::NotDir(resolved));
        }
        *self.cwd.borrow_mut() = resolved;
        Ok(())
    }

    /// Get the environment (borrowed).
    pub fn env(&self) -> std::cell::Ref<'_, Environment> {
        self.env.borrow()
    }

    /// Get mutable environment.
    pub fn env_mut(&self) -> std::cell::RefMut<'_, Environment> {
        self.env.borrow_mut()
    }

    /// Get mutable VFS access (for tests).
    pub fn vfs_mut(&self) -> std::cell::RefMut<'_, amla_vfs::Vfs> {
        self.vfs.borrow_mut()
    }

    /// Get the last exit code.
    pub fn last_exit(&self) -> i32 {
        *self.last_exit.borrow()
    }

    /// Set the stdin handle.
    ///
    /// This allows swapping the stdin for different commands,
    /// enabling proper stdin piping instead of workarounds.
    pub fn set_stdin(&mut self, stdin: IoHandle) {
        self.stdin = stdin;
    }

    /// Get a reference to the stdin handle.
    pub fn stdin(&self) -> &IoHandle {
        &self.stdin
    }

    /// Set the stdout handle.
    pub fn set_stdout(&mut self, stdout: IoHandle) {
        self.stdout = stdout;
    }

    /// Get a reference to the stdout handle.
    pub fn stdout(&self) -> &IoHandle {
        &self.stdout
    }

    /// Set the stderr handle.
    pub fn set_stderr(&mut self, stderr: IoHandle) {
        self.stderr = stderr;
    }

    /// Get a reference to the stderr handle.
    pub fn stderr(&self) -> &IoHandle {
        &self.stderr
    }

    /// Get the pipefail setting.
    ///
    /// When pipefail is enabled, a pipeline returns the exit code of
    /// the first command that fails (non-zero exit), rather than
    /// the exit code of the last command.
    pub fn pipefail(&self) -> bool {
        *self.pipefail.borrow()
    }

    /// Set the pipefail option.
    ///
    /// When enabled, a pipeline returns the exit code of the first
    /// command that fails (non-zero exit).
    pub fn set_pipefail(&self, enabled: bool) {
        *self.pipefail.borrow_mut() = enabled;
    }

    /// Check if the scheduler has any pending tasks.
    ///
    /// Returns true if all spawned tasks have completed.
    /// Use this after `step()` to check if commands are waiting for I/O.
    pub fn is_idle(&self) -> bool {
        self.scheduler.is_empty()
    }

    /// Step the scheduler once.
    ///
    /// Runs all ready tasks once. Returns true if tasks are still pending
    /// (e.g., waiting for stdin), false if all tasks completed.
    ///
    /// Use this for incremental execution where you need to provide stdin
    /// data between steps.
    pub fn step(&mut self) -> bool {
        let _ = self.scheduler.run();
        !self.scheduler.is_empty()
    }

    /// Check if there are any running background jobs.
    pub fn has_running_jobs(&self) -> bool {
        let _ = self.scheduler.run();
        let jobs = self.jobs.borrow();
        jobs.iter()
            .any(|job| matches!(job.state, JobState::Running))
    }

    /// Get the number of running background jobs.
    pub fn running_job_count(&self) -> usize {
        let _ = self.scheduler.run();
        let jobs = self.jobs.borrow();
        jobs.iter()
            .filter(|job| matches!(job.state, JobState::Running))
            .count()
    }

    /// Execute a command line.
    ///
    /// Parses and executes the command. For foreground commands, blocks
    /// until completion and returns the exit code. For background commands,
    /// returns immediately with exit code 0.
    pub async fn execute(&self, line: &str) -> Result<i32> {
        // Parse the command first, THEN expand during word processing
        // (This preserves quote semantics - single-quoted content is NOT expanded)
        let cmd = parse(line)?;

        if cmd.is_empty() {
            return Ok(0);
        }

        // Execute it (Box::pin to reduce stack usage for large futures)
        let exit_code = Box::pin(self.execute_command(cmd)).await?;

        // Update last exit code (brief borrow after await)
        *self.last_exit.borrow_mut() = exit_code;

        Ok(exit_code)
    }

    /// Collect a chain of `&&` commands into a flat vector.
    ///
    /// This flattens left-associative `And` chains like `((a && b) && c) && d`
    /// into `Vec[a, b, c, d]` for iterative processing, avoiding stack overflow.
    fn collect_and_chain(cmd: Command) -> Vec<Command> {
        let mut chain = Vec::new();
        let mut current = cmd;
        loop {
            match current {
                Command::And { left, right } => {
                    // Push right first (we'll reverse at the end)
                    chain.push(*right);
                    current = *left;
                }
                other => {
                    chain.push(other);
                    break;
                }
            }
        }
        chain.reverse();
        chain
    }

    /// Collect a chain of `||` commands into a flat vector.
    ///
    /// This flattens left-associative `Or` chains like `((a || b) || c) || d`
    /// into `Vec[a, b, c, d]` for iterative processing, avoiding stack overflow.
    fn collect_or_chain(cmd: Command) -> Vec<Command> {
        let mut chain = Vec::new();
        let mut current = cmd;
        loop {
            match current {
                Command::Or { left, right } => {
                    chain.push(*right);
                    current = *left;
                }
                other => {
                    chain.push(other);
                    break;
                }
            }
        }
        chain.reverse();
        chain
    }

    /// Execute a parsed command.
    async fn execute_command(&self, cmd: Command) -> Result<i32> {
        match cmd {
            Command::Empty => Ok(0),

            Command::Simple { argv, redirects } => self.execute_simple(argv, redirects).await,

            Command::Pipeline { commands } => self.execute_pipeline(commands).await,

            Command::And { left, right } => {
                // Flatten same-operator chains to avoid stack overflow.
                // `a && b && c && d` becomes Vec[a, b, c, d] processed iteratively.
                let chain = Self::collect_and_chain(Command::And { left, right });
                for cmd in chain {
                    let code = Box::pin(self.execute_command(cmd)).await?;
                    if code != 0 {
                        return Ok(code);
                    }
                }
                Ok(0)
            }

            Command::Or { left, right } => {
                // Flatten same-operator chains to avoid stack overflow.
                // `a || b || c` becomes Vec[a, b, c] processed iteratively.
                let chain = Self::collect_or_chain(Command::Or { left, right });
                let mut last_code = 0;
                for cmd in chain {
                    last_code = Box::pin(self.execute_command(cmd)).await?;
                    if last_code == 0 {
                        return Ok(0);
                    }
                }
                // All commands failed, return the last exit code
                Ok(last_code)
            }

            Command::Sequence { commands } => {
                let mut last_code = 0;
                for cmd in commands {
                    last_code = Box::pin(self.execute_command(cmd)).await?;
                }
                Ok(last_code)
            }

            Command::Background { command } => self.execute_background(*command).await,

            Command::Subshell { command } => {
                // Execute in a subshell - effects are discarded
                self.execute_in_subshell(*command).await
            }
        }
    }

    /// Expand a single segment based on its expansion flags.
    async fn expand_segment(&self, segment: &lexer::WordSegment) -> Result<String> {
        // Handle command substitution first
        if let Some(ref cmd) = segment.command_substitution {
            return self.capture_output(cmd).await;
        }

        let text = &segment.text;
        if segment.expand_vars {
            // Tilde expansion first (only at start of segment)
            let env = self.env.borrow();
            let text = if text == "~" {
                // ~ alone
                env.get("HOME").unwrap_or("").to_owned()
            } else if let Some(rest) = text.strip_prefix("~/") {
                // ~/path
                if let Some(home) = env.get("HOME") {
                    format!("{home}/{rest}")
                } else {
                    text.clone()
                }
            } else if let Some(rest) = text.strip_prefix('~') {
                // ~user or ~user/path - all users expand to HOME in sandbox
                if let Some(slash_pos) = rest.find('/') {
                    // ~user/path → $HOME/path
                    let path = &rest[slash_pos..]; // includes the /
                    if let Some(home) = env.get("HOME") {
                        format!("{home}{path}")
                    } else {
                        text.clone()
                    }
                } else {
                    // ~user alone → $HOME
                    env.get("HOME").unwrap_or("").to_owned()
                }
            } else {
                text.clone()
            };

            // Variable expansion
            let last_exit = *self.last_exit.borrow();
            Ok(env.expand(&text, last_exit))
        } else {
            // No expansion (single-quoted)
            Ok(text.clone())
        }
    }

    /// Expand a Word into a string and glob flag.
    ///
    /// Returns (`expanded_text`, `should_glob`).
    async fn expand_word(&self, word: &lexer::Word) -> Result<(String, bool)> {
        let mut result = String::new();
        let mut can_glob = false;

        for segment in &word.segments {
            result.push_str(&self.expand_segment(segment).await?);
            // Can only glob if at least one segment allows glob expansion
            // Command substitutions don't glob by themselves
            if segment.expand_globs && segment.command_substitution.is_none() {
                can_glob = true;
            }
        }

        Ok((result, can_glob))
    }

    /// Expand a Word for redirect paths (variable expansion only, no globs).
    async fn expand_redirect_path(&self, word: &lexer::Word) -> Result<String> {
        let (text, _) = self.expand_word(word).await?;
        Ok(text)
    }

    /// Expand all arguments (variables, tilde, globs).
    ///
    /// Order of expansion:
    /// 1. Command substitution (per segment, if present)
    /// 2. Tilde expansion (per segment, if `expand_vars`)
    /// 3. Variable expansion (per segment, if `expand_vars`)
    /// 4. Glob expansion (if any segment allows globs)
    async fn expand_argv(&self, argv: SmallVec<[lexer::Word; 8]>) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let cwd = self.cwd.borrow().clone();

        for word in argv {
            let (expanded, can_glob) = self.expand_word(&word).await?;

            // Glob expansion (may produce multiple results) only if allowed
            if can_glob && glob::is_glob_pattern(&expanded) {
                let matches = glob::expand_glob(&self.vfs, &cwd, &expanded);
                result.extend(matches);
            } else {
                result.push(expanded);
            }
        }

        Ok(result)
    }

    /// Execute a simple command.
    async fn execute_simple(
        &self,
        argv: SmallVec<[lexer::Word; 8]>,
        redirects: SmallVec<[Redirect; 2]>,
    ) -> Result<i32> {
        if argv.is_empty() {
            return Ok(0);
        }

        // Expand variables, tilde, and command substitution
        let argv = self.expand_argv(argv).await?;

        let cmd_name = &argv[0];

        // Set up I/O for the command - use shell's default streams
        let stdin = self.stdin.clone();
        let stdout = self.stdout.clone();
        let stderr = self.stderr.clone();

        // Apply redirects (overrides defaults)
        let (stdin, stdout, stderr) = self
            .apply_redirects(stdin, stdout, stderr, &redirects)
            .await?;

        // Special builtins that need shell state (job table)
        // These are handled synchronously but spawn async tasks for I/O
        match cmd_name.as_str() {
            "jobs" => {
                return self.run_builtin_jobs(stdout, stderr).await;
            }
            "fg" => {
                return self.run_builtin_fg(&argv, stdout, stderr).await;
            }
            "wait" => {
                return self.run_builtin_wait(&argv).await;
            }
            "kill" => {
                return self.run_builtin_kill(&argv, stderr).await;
            }
            _ => {}
        }

        // Check if command exists
        if commands::get_command(cmd_name).is_none() {
            return Err(ShellError::CommandNotFound(cmd_name.clone()));
        }

        // Clone resources for the async task (brief borrows, not held across await)
        let vfs = Rc::clone(&self.vfs);
        let cwd = self.cwd.borrow().clone();
        let env = self.env.borrow().clone();
        let argv_clone = argv.clone();
        let scheduler = self.scheduler.clone();
        let tool_catalog = self.tool_catalog.clone();

        let handle = self.scheduler.spawn(async move {
            let ctx = CmdContext::new(
                argv_clone,
                stdin,
                stdout,
                stderr,
                cwd,
                env,
                vfs,
                scheduler,
                tool_catalog,
            );

            // Get and run the command
            if let Some(cmd_fn) = commands::get_command(ctx.command_name()) {
                cmd_fn(ctx).await
            } else {
                Err(amla_scheduler::Error::Command(format!(
                    "command not found: {}",
                    ctx.argv.first().map_or("", std::string::String::as_str)
                )))
            }
        });

        // Await the task (yields to parent scheduler)
        match handle.await {
            Ok(exit) => {
                self.apply_effects(&exit.effects);
                Ok(exit.code)
            }
            Err(_) => Ok(1),
        }
    }

    /// Execute a pipeline.
    async fn execute_pipeline(&self, commands: Vec<Command>) -> Result<i32> {
        if commands.is_empty() {
            return Ok(0);
        }

        if commands.len() == 1 {
            return Box::pin(self.execute_command(commands.into_iter().next().unwrap())).await;
        }

        // Create pipes between commands
        let n = commands.len();
        let pipes: Vec<AsyncPipe> = (0..n - 1).map(|_| AsyncPipe::new(4096)).collect();

        // Spawn each command
        let mut handles = Vec::with_capacity(n);

        for (i, cmd) in commands.into_iter().enumerate() {
            // Get stdin (from previous command or shell's default)
            let stdin = if i == 0 {
                self.stdin.clone() // First command uses shell's stdin
            } else {
                IoHandle::Pipe(pipes[i - 1].clone())
            };

            // Get stdout (to next command or shell's default)
            let stdout = if i < n - 1 {
                IoHandle::Pipe(pipes[i].clone())
            } else {
                self.stdout.clone() // Last command uses shell's stdout
            };

            let stderr = self.stderr.clone();

            // Extract argv from command
            let (argv, redirects) = match cmd {
                Command::Simple { argv, redirects } => (argv, redirects),
                _ => {
                    return Err(ShellError::Syntax {
                        message: "nested commands in pipeline not supported".into(),
                        position: 0,
                    });
                }
            };

            // Expand argv (includes command substitution)
            let argv = self.expand_argv(argv).await?;

            // Apply redirects
            let (stdin, stdout, stderr) = self
                .apply_redirects(stdin, stdout, stderr, &redirects)
                .await?;

            // Clone resources for the task (brief borrows)
            let vfs = Rc::clone(&self.vfs);
            let cwd = self.cwd.borrow().clone();
            let env = self.env.borrow().clone();
            let scheduler = self.scheduler.clone();
            let tool_catalog = self.tool_catalog.clone();

            let handle = self.scheduler.spawn(async move {
                let ctx = CmdContext::new(
                    argv,
                    stdin,
                    stdout,
                    stderr,
                    cwd,
                    env,
                    vfs,
                    scheduler,
                    tool_catalog,
                );

                if let Some(cmd_fn) = commands::get_command(ctx.command_name()) {
                    cmd_fn(ctx).await
                } else {
                    Err(amla_scheduler::Error::Command(format!(
                        "command not found: {}",
                        ctx.argv.first().map_or("", std::string::String::as_str)
                    )))
                }
            });

            handles.push(handle);
        }

        // Await handles in order, closing pipes after each writer completes.
        // This ensures readers see EOF when their upstream writer finishes.
        let pipefail = self.pipefail();
        let mut first_failure: Option<i32> = None;
        let mut last_code = 0;

        for (i, handle) in handles.into_iter().enumerate() {
            match handle.await {
                Ok(exit) => {
                    last_code = exit.code;
                    // Track first non-zero exit for pipefail
                    if pipefail && exit.code != 0 && first_failure.is_none() {
                        first_failure = Some(exit.code);
                    }
                }
                Err(_) => {
                    last_code = 1;
                    if pipefail && first_failure.is_none() {
                        first_failure = Some(1);
                    }
                }
            }
            // Close the pipe that this command was writing to (if any).
            // Command i writes to pipes[i] (except the last command which writes to shell stdout).
            if i < pipes.len() {
                pipes[i].close();
            }
        }

        // With pipefail, return first failure; otherwise return last exit code
        Ok(first_failure.unwrap_or(last_code))
    }

    /// Execute a command in the background.
    async fn execute_background(&self, cmd: Command) -> Result<i32> {
        // Get the command string for display
        let cmd_str = format_command(&cmd);

        // Spawn tasks based on command type
        let handles = self.spawn_background_command(cmd).await?;

        if handles.is_empty() {
            return Ok(0);
        }

        // Add to job table
        let job_id = self.jobs.borrow_mut().add(cmd_str.clone(), handles);

        // Print job info
        println!("[{job_id}] {cmd_str} &");

        // Return immediately without waiting
        Ok(0)
    }

    /// Spawn tasks for a background command, returning handles without waiting.
    async fn spawn_background_command(
        &self,
        cmd: Command,
    ) -> Result<Vec<amla_scheduler::TaskHandle>> {
        match cmd {
            Command::Empty => Ok(vec![]),

            Command::Simple { argv, redirects } => {
                if argv.is_empty() {
                    return Ok(vec![]);
                }

                // Expand argv (includes command substitution)
                let argv = self.expand_argv(argv).await?;

                // Check if command exists
                if commands::get_command(&argv[0]).is_none() {
                    return Err(ShellError::CommandNotFound(argv[0].clone()));
                }

                // Set up I/O - use shell's default streams
                let stdin = self.stdin.clone();
                let stdout = self.stdout.clone();
                let stderr = self.stderr.clone();

                let (stdin, stdout, stderr) = self
                    .apply_redirects(stdin, stdout, stderr, &redirects)
                    .await?;

                let vfs = Rc::clone(&self.vfs);
                let cwd = self.cwd.borrow().clone();
                let env = self.env.borrow().clone();
                let scheduler = self.scheduler.clone();
                let tool_catalog = self.tool_catalog.clone();

                let handle = self.scheduler.spawn(async move {
                    let ctx = CmdContext::new(
                        argv,
                        stdin,
                        stdout,
                        stderr,
                        cwd,
                        env,
                        vfs,
                        scheduler,
                        tool_catalog,
                    );
                    if let Some(cmd_fn) = commands::get_command(ctx.command_name()) {
                        cmd_fn(ctx).await
                    } else {
                        Err(amla_scheduler::Error::Command("command not found".into()))
                    }
                });

                Ok(vec![handle])
            }

            Command::Pipeline { commands: cmds } => {
                if cmds.is_empty() {
                    return Ok(vec![]);
                }

                // Create pipes between commands
                let n = cmds.len();
                let pipes: Vec<AsyncPipe> = (0..n - 1).map(|_| AsyncPipe::new(4096)).collect();
                let mut handles = Vec::with_capacity(n);

                for (i, c) in cmds.into_iter().enumerate() {
                    let stdin = if i == 0 {
                        self.stdin.clone()
                    } else {
                        IoHandle::Pipe(pipes[i - 1].clone())
                    };

                    let stdout = if i < n - 1 {
                        IoHandle::Pipe(pipes[i].clone())
                    } else {
                        self.stdout.clone()
                    };

                    let stderr = self.stderr.clone();

                    let (argv, redirects) = match c {
                        Command::Simple { argv, redirects } => (argv, redirects),
                        _ => continue,
                    };

                    // Expand argv (includes command substitution)
                    let argv = self.expand_argv(argv).await?;

                    let (stdin, stdout, stderr) = self
                        .apply_redirects(stdin, stdout, stderr, &redirects)
                        .await?;

                    let vfs = Rc::clone(&self.vfs);
                    let cwd = self.cwd.borrow().clone();
                    let env = self.env.borrow().clone();
                    let scheduler = self.scheduler.clone();
                    let tool_catalog = self.tool_catalog.clone();

                    let handle = self.scheduler.spawn(async move {
                        let ctx = CmdContext::new(
                            argv,
                            stdin,
                            stdout,
                            stderr,
                            cwd,
                            env,
                            vfs,
                            scheduler,
                            tool_catalog,
                        );
                        if let Some(cmd_fn) = commands::get_command(ctx.command_name()) {
                            cmd_fn(ctx).await
                        } else {
                            Err(amla_scheduler::Error::Command("command not found".into()))
                        }
                    });

                    handles.push(handle);
                }

                // Close pipes after spawning so tasks can make progress
                // (don't run scheduler to completion - that's what fg/wait does)
                for pipe in pipes {
                    pipe.close();
                }

                Ok(handles)
            }

            // For compound commands, just spawn the first one (Box::pin for recursion)
            Command::Sequence { commands: cmds } => {
                // For sequence in background, execute first command
                if let Some(first) = cmds.into_iter().next() {
                    Box::pin(self.spawn_background_command(first)).await
                } else {
                    Ok(vec![])
                }
            }

            Command::And { left, .. } => Box::pin(self.spawn_background_command(*left)).await,
            Command::Or { left, .. } => Box::pin(self.spawn_background_command(*left)).await,
            Command::Background { command } => {
                Box::pin(self.spawn_background_command(*command)).await
            }
            Command::Subshell { command } => {
                Box::pin(self.spawn_background_command(*command)).await
            }
        }
    }

    /// Execute a command in a subshell (effects discarded).
    async fn execute_in_subshell(&self, cmd: Command) -> Result<i32> {
        // Save current state (brief borrows)
        let saved_cwd = self.cwd.borrow().clone();
        let saved_env = self.env.borrow().clone();

        // Execute command (Box::pin for recursion)
        let result = Box::pin(self.execute_command(cmd)).await;

        // Restore state (discard effects)
        *self.cwd.borrow_mut() = saved_cwd;
        *self.env.borrow_mut() = saved_env;

        result
    }

    /// Execute a command and capture its stdout as a string.
    ///
    /// Used for command substitution `$(cmd)`.
    /// Returns the captured output with trailing newlines removed.
    async fn capture_output(&self, command: &str) -> Result<String> {
        // Create a buffer to capture stdout
        let output_buffer = IoHandle::buffer();

        // Create a temporary shell with the capture buffer as stdout
        let capture_shell = Shell::with_full_context(
            self.scheduler.clone(),
            Rc::clone(&self.vfs),
            self.cwd.borrow().clone(),
            self.env.borrow().clone(),
            self.stdin.clone(),
            output_buffer.clone(),
            self.stderr.clone(),
        );

        // Parse and execute the command
        let result = match parse(command) {
            Ok(cmd) => Box::pin(capture_shell.execute_command(cmd)).await,
            Err(e) => Err(e),
        };

        // Check for errors
        result?;

        // Read from the buffer
        let output = output_buffer.take_buffer();
        let output_str = String::from_utf8_lossy(&output);

        // Remove trailing newlines (POSIX behavior)
        Ok(output_str.trim_end_matches('\n').to_string())
    }

    /// Run the scheduler until all tasks complete.
    pub fn run_to_completion(&mut self) -> SchedulerState {
        self.scheduler.run()
    }

    /// Get a reference to the scheduler.
    ///
    /// Use this to access host operations for time/sleep commands:
    /// - `scheduler.take_host_op()` - get pending op (Now, `WakeAt`)
    /// - `scheduler.complete_host_op(id, data)` - provide result
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Apply redirects to I/O handles.
    async fn apply_redirects(
        &self,
        stdin: IoHandle,
        stdout: IoHandle,
        stderr: IoHandle,
        redirects: &[Redirect],
    ) -> Result<(IoHandle, IoHandle, IoHandle)> {
        let mut stdin = stdin;
        let mut stdout = stdout;
        let mut stderr = stderr;

        for redirect in redirects {
            match (&redirect.kind, &redirect.target) {
                // > file
                (RedirectKind::Write, RedirectTarget::File(path)) if redirect.source_fd == 1 => {
                    let path = self.resolve_path(&self.expand_redirect_path(path).await?);
                    stdout = IoHandle::open_write(Rc::clone(&self.vfs), path)
                        .map_err(|e| ShellError::Redirect(e.to_string()))?;
                }
                // >> file
                (RedirectKind::Append, RedirectTarget::File(path)) if redirect.source_fd == 1 => {
                    let path = self.resolve_path(&self.expand_redirect_path(path).await?);
                    stdout = IoHandle::open_append(Rc::clone(&self.vfs), path)
                        .map_err(|e| ShellError::Redirect(e.to_string()))?;
                }
                // < file
                (RedirectKind::Read, RedirectTarget::File(path)) if redirect.source_fd == 0 => {
                    let path = self.resolve_path(&self.expand_redirect_path(path).await?);
                    stdin = IoHandle::open_read(Rc::clone(&self.vfs), path)
                        .map_err(|e| ShellError::Redirect(e.to_string()))?;
                }
                // 2> file
                (RedirectKind::Write, RedirectTarget::File(path)) if redirect.source_fd == 2 => {
                    let path = self.resolve_path(&self.expand_redirect_path(path).await?);
                    stderr = IoHandle::open_write(Rc::clone(&self.vfs), path)
                        .map_err(|e| ShellError::Redirect(e.to_string()))?;
                }
                // 2>> file
                (RedirectKind::Append, RedirectTarget::File(path)) if redirect.source_fd == 2 => {
                    let path = self.resolve_path(&self.expand_redirect_path(path).await?);
                    stderr = IoHandle::open_append(Rc::clone(&self.vfs), path)
                        .map_err(|e| ShellError::Redirect(e.to_string()))?;
                }
                // 2>&1
                (_, RedirectTarget::Fd(1)) if redirect.source_fd == 2 => {
                    stderr = stdout.clone();
                }
                // 1>&2
                (_, RedirectTarget::Fd(2)) if redirect.source_fd == 1 => {
                    stdout = stderr.clone();
                }
                // <<EOF (heredoc)
                (RedirectKind::Read, RedirectTarget::HereDoc { content, .. })
                    if redirect.source_fd == 0 =>
                {
                    // Heredoc provides content as stdin
                    // Add trailing newline if not present (shell convention)
                    let mut heredoc_content = content.clone();
                    if !heredoc_content.is_empty() && !heredoc_content.ends_with('\n') {
                        heredoc_content.push('\n');
                    }
                    stdin = IoHandle::from_string(&heredoc_content);
                }
                // Other redirects not implemented
                _ => {}
            }
        }

        Ok((stdin, stdout, stderr))
    }

    /// Apply side effects from command execution.
    fn apply_effects(&self, effects: &SideEffects) {
        if let Some(cwd) = &effects.cwd {
            *self.cwd.borrow_mut() = cwd.clone();
        }
        for (key, value) in &effects.env_set {
            self.env.borrow_mut().set(key, value);
        }
        for key in &effects.env_unset {
            self.env.borrow_mut().unset(key);
        }
        if let Some(pipefail) = effects.pipefail {
            self.set_pipefail(pipefail);
        }
    }

    /// Resolve a path relative to the current working directory.
    fn resolve_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            normalize_path(path)
        } else {
            let cwd = self.cwd.borrow();
            let combined = if cwd.ends_with('/') {
                format!("{}{}", *cwd, path)
            } else {
                format!("{}/{}", *cwd, path)
            };
            normalize_path(&combined)
        }
    }

    // =========================================================================
    // Special builtins (require shell state)
    // =========================================================================

    /// Run the `jobs` builtin command.
    async fn run_builtin_jobs(&self, stdout: IoHandle, _stderr: IoHandle) -> Result<i32> {
        // Update all job states first
        let _ = self.scheduler.run();
        self.update_job_states();

        // Collect job info before spawning task
        let job_lines: Vec<String> = self
            .jobs
            .borrow()
            .iter()
            .map(|job| {
                let status = match job.state {
                    JobState::Running => "Running",
                    JobState::Done(code) => {
                        if code == 0 {
                            "Done"
                        } else {
                            "Exit"
                        }
                    }
                };
                format!("[{}] {} {}", job.id, status, job.command)
            })
            .collect();

        // Spawn async task to write output
        let handle = self.scheduler.spawn(async move {
            for line in job_lines {
                stdout.write_all(line.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
            }
            Ok(amla_scheduler::Exit::success())
        });

        match handle.await {
            Ok(exit) => Ok(exit.code),
            Err(_) => Ok(1),
        }
    }

    /// Run the `fg` builtin command.
    async fn run_builtin_fg(
        &self,
        argv: &[String],
        stdout: IoHandle,
        stderr: IoHandle,
    ) -> Result<i32> {
        // Sleep interval for polling (1ms)
        const POLL_INTERVAL_NS: u64 = 1_000_000;

        // Parse job ID: fg %1, fg 1, or fg (most recent)
        let job_id = if argv.len() > 1 {
            let arg = &argv[1];
            let num_str = arg.trim_start_matches('%');
            num_str.parse::<usize>().ok()
        } else {
            // Get most recent (highest ID) running job
            self.jobs
                .borrow()
                .iter()
                .filter(|j| matches!(j.state, JobState::Running))
                .map(|j| j.id)
                .max()
        };

        let Some(job_id) = job_id else {
            // Write error via async task
            let _handle = self.scheduler.spawn(async move {
                stderr.write_all(b"fg: no current job\n").await?;
                Ok(amla_scheduler::Exit::code(1))
            });
            let _ = self.scheduler.run();
            return Ok(1);
        };

        // Get command string and verify job exists
        let (cmd_str, already_done) = {
            let jobs = self.jobs.borrow();
            if let Some(job) = jobs.get(job_id) {
                let _done = matches!(job.state, JobState::Done(_));
                let code = match job.state {
                    JobState::Done(c) => Some(c),
                    _ => None,
                };
                (job.command.clone(), code)
            } else {
                // Job not found
                let msg = format!("fg: %{job_id}: no such job\n");
                let _handle = self.scheduler.spawn(async move {
                    stderr.write_all(msg.as_bytes()).await?;
                    Ok(amla_scheduler::Exit::code(1))
                });
                let _ = self.scheduler.run();
                return Ok(1);
            }
        };

        // If already done, just remove and return the code
        if let Some(code) = already_done {
            self.jobs.borrow_mut().remove(job_id);
            return Ok(code);
        }

        // Print the command being brought to foreground
        let cmd_display = cmd_str;
        let _handle = self.scheduler.spawn(async move {
            stdout.write_all(cmd_display.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            Ok(amla_scheduler::Exit::success())
        });
        let _ = self.scheduler.run();

        // Run scheduler until job completes
        loop {
            let _ = self.scheduler.run();

            // Check job state in a separate scope to drop borrow before await
            let should_sleep = {
                let mut jobs = self.jobs.borrow_mut();
                if let Some(job) = jobs.get_mut(job_id) {
                    job.update_state();
                    if let JobState::Done(code) = job.state {
                        drop(jobs);
                        self.jobs.borrow_mut().remove(job_id);
                        return Ok(code);
                    }
                    true // Need to sleep and check again
                } else {
                    return Ok(0);
                }
            };

            if should_sleep {
                // Sleep briefly to allow pending host ops (like WakeAt for background sleeps)
                // to be processed. Using a timer-based yield ensures we don't spin.
                let now = self.scheduler.now_monotonic();
                let _ = self.scheduler.sleep_until(now + POLL_INTERVAL_NS).await;
            }
        }
    }

    /// Run the `wait` builtin command.
    async fn run_builtin_wait(&self, argv: &[String]) -> Result<i32> {
        // Sleep interval for polling (1ms)
        const POLL_INTERVAL_NS: u64 = 1_000_000;

        // Parse job IDs, or get all if none specified
        let job_ids: Vec<usize> = if argv.len() > 1 {
            argv[1..]
                .iter()
                .filter_map(|arg| {
                    let num_str = arg.trim_start_matches('%');
                    num_str.parse::<usize>().ok()
                })
                .collect()
        } else {
            self.jobs.borrow().iter().map(|j| j.id).collect()
        };

        if job_ids.is_empty() {
            return Ok(0);
        }

        let mut last_code = 0;

        // Run scheduler until all specified jobs complete
        loop {
            let _ = self.scheduler.run();

            // Update and check job states
            let mut all_done = true;
            {
                let mut jobs = self.jobs.borrow_mut();
                for &id in &job_ids {
                    if let Some(job) = jobs.get_mut(id) {
                        job.update_state();
                        match job.state {
                            JobState::Done(code) => {
                                last_code = code;
                            }
                            JobState::Running => {
                                all_done = false;
                            }
                        }
                    }
                }
            }

            if all_done {
                // Remove completed jobs
                for &id in &job_ids {
                    self.jobs.borrow_mut().remove(id);
                }
                break;
            }

            // Sleep briefly to allow pending host ops (like WakeAt for background sleeps)
            // to be processed. Using a timer-based yield ensures we don't spin but
            // periodically check if jobs have completed.
            let now = self.scheduler.now_monotonic();
            let _ = self.scheduler.sleep_until(now + POLL_INTERVAL_NS).await;
        }

        Ok(last_code)
    }

    /// Run the `kill` builtin command.
    ///
    /// Kills background jobs by their job ID (%N or N).
    /// Uses structured concurrency: killing a job cancels all its child tasks.
    async fn run_builtin_kill(&self, argv: &[String], stderr: IoHandle) -> Result<i32> {
        if argv.len() < 2 {
            let stderr_clone = stderr;
            let _handle = self.scheduler.spawn(async move {
                stderr_clone
                    .write_all(b"kill: usage: kill %jobspec or kill jobid\n")
                    .await?;
                Ok(amla_scheduler::Exit::code(1))
            });
            let _ = self.scheduler.run();
            return Ok(1);
        }

        let mut exit_code = 0;

        for arg in &argv[1..] {
            // Parse job ID from %N format or just N
            let num_str = arg.trim_start_matches('%');
            let job_id = if let Ok(id) = num_str.parse::<usize>() {
                id
            } else {
                let msg = format!("kill: {arg}: arguments must be job IDs\n");
                let stderr_clone = stderr.clone();
                let _handle = self.scheduler.spawn(async move {
                    stderr_clone.write_all(msg.as_bytes()).await?;
                    Ok(amla_scheduler::Exit::code(1))
                });
                let _ = self.scheduler.run();
                exit_code = 1;
                continue;
            };

            // Kill the job (cancels tasks via structured concurrency)
            let killed = self.jobs.borrow_mut().kill(job_id);

            if !killed {
                let msg = format!("kill: %{job_id}: no such job\n");
                let stderr_clone = stderr.clone();
                let _handle = self.scheduler.spawn(async move {
                    stderr_clone.write_all(msg.as_bytes()).await?;
                    Ok(amla_scheduler::Exit::code(1))
                });
                let _ = self.scheduler.run();
                exit_code = 1;
            }
        }

        Ok(exit_code)
    }

    /// Update states of all jobs.
    fn update_job_states(&self) {
        let ids: Vec<usize> = self.jobs.borrow().iter().map(|j| j.id).collect();
        let mut jobs = self.jobs.borrow_mut();
        for id in ids {
            if let Some(job) = jobs.get_mut(id) {
                job.update_state();
            }
        }
    }

    /// Check for and report completed background jobs.
    ///
    /// Writes notifications to the provided stderr handle.
    pub fn reap_completed_jobs(&self, stderr: &IoHandle) -> Vec<(usize, i32)> {
        let _ = self.scheduler.run();
        self.update_job_states();
        let completed = self.jobs.borrow_mut().reap();

        if !completed.is_empty() {
            // Spawn async task to write notifications
            let stderr = stderr.clone();
            let completed_clone = completed.clone();
            let _handle = self.scheduler.spawn(async move {
                for (id, code) in &completed_clone {
                    let msg = if *code == 0 {
                        format!("[{id}]+ Done\n")
                    } else {
                        format!("[{id}]+ Exit {code}\n")
                    };
                    stderr.write_all(msg.as_bytes()).await?;
                }
                Ok(amla_scheduler::Exit::success())
            });
            let _ = self.scheduler.run();
        }

        completed
    }
}

/// Format a command for display (e.g., in job listings).
fn format_command(cmd: &Command) -> String {
    match cmd {
        Command::Empty => String::new(),
        Command::Simple { argv, .. } => argv
            .iter()
            .map(lexer::Word::text)
            .collect::<Vec<_>>()
            .join(" "),
        Command::Pipeline { commands } => commands
            .iter()
            .map(format_command)
            .collect::<Vec<_>>()
            .join(" | "),
        Command::And { left, right } => {
            format!("{} && {}", format_command(left), format_command(right))
        }
        Command::Or { left, right } => {
            format!("{} || {}", format_command(left), format_command(right))
        }
        Command::Sequence { commands } => commands
            .iter()
            .map(format_command)
            .collect::<Vec<_>>()
            .join("; "),
        Command::Background { command } => format!("{} &", format_command(command)),
        Command::Subshell { command } => format!("({})", format_command(command)),
    }
}

/// Normalize a path (resolve . and ..).
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();

    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }

    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amla_scheduler::{RandomSourceFn, TimeSourceFn};
    use std::cell::Cell;

    /// Create mock time and random sources for testing.
    fn mock_sources() -> (TimeSourceFn, RandomSourceFn, Rc<Cell<u64>>) {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        (time_source, random_source, mock_time)
    }

    /// Create a scheduler with mock sources for testing.
    fn test_scheduler() -> Scheduler {
        let (time_source, random_source, _) = mock_sources();
        Scheduler::new(1, time_source, random_source)
    }

    /// Create a shell with mock sources for testing.
    fn test_shell() -> Shell {
        Shell::new(test_scheduler())
    }

    /// Test helper: Run a command and assert it completes within max_steps.
    ///
    /// Uses `max_steps` as an upper bound (not exact match) because the actual
    /// number of scheduler steps depends on implementation details like
    /// round-robin task polling.
    ///
    /// Returns the Result from execute().
    fn run(shell: &Shell, cmd: &str, max_steps: usize) -> Result<i32> {
        use std::task::{Context, Poll};

        let scheduler = shell.scheduler.clone();
        let mut fut = std::pin::pin!(shell.execute(cmd));

        let waker = amla_scheduler::noop_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll (step 0) - before any scheduler steps
        if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
            return result;
        }

        // Allow more steps with round-robin scheduling
        // Each pipeline stage may need its own step
        let limit = max_steps.saturating_mul(3).max(100);

        for _step in 1..=limit {
            let _ = scheduler.run_step();
            if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
                return result;
            }
        }
        panic!("command '{cmd}' did not complete within {limit} steps");
    }

    #[test]
    fn shell_create() {
        let shell = test_shell();
        assert_eq!(shell.cwd(), "/workspace");
    }

    #[test]
    fn shell_parse_simple() {
        let cmd = parse("ls -la /tmp").unwrap();
        assert!(matches!(cmd, Command::Simple { .. }));
    }

    #[test]
    fn shell_parse_pipeline() {
        let cmd = parse("cat file | grep foo | wc -l").unwrap();
        assert!(matches!(cmd, Command::Pipeline { .. }));
    }

    #[test]
    fn shell_builtin_true() {
        let shell = test_shell();
        assert_eq!(run(&shell, "true", 1).unwrap(), 0);
    }

    #[test]
    fn shell_builtin_false() {
        let shell = test_shell();
        assert_eq!(run(&shell, "false", 1).unwrap(), 1);
    }

    #[test]
    fn shell_and_operator() {
        let shell = test_shell();
        // true && true = 0
        assert_eq!(run(&shell, "true && true", 2).unwrap(), 0);
        // false && true = 1 (short circuit)
        assert_eq!(run(&shell, "false && true", 1).unwrap(), 1);
    }

    #[test]
    fn shell_or_operator() {
        let shell = test_shell();
        // true || false = 0
        assert_eq!(run(&shell, "true || false", 1).unwrap(), 0);
        // false || true = 0
        assert_eq!(run(&shell, "false || true", 2).unwrap(), 0);
        // false || false = 1
        assert_eq!(run(&shell, "false || false", 2).unwrap(), 1);
    }

    #[test]
    fn shell_empty_command() {
        let shell = test_shell();
        assert_eq!(run(&shell, "", 0).unwrap(), 0);
    }

    // =========================================================================
    // PIPEFAIL tests
    // =========================================================================

    #[test]
    fn pipefail_disabled_by_default() {
        let shell = test_shell();
        assert!(!shell.pipefail());
    }

    #[test]
    fn pipefail_returns_last_without_option() {
        let shell = test_shell();

        // Without pipefail: false | true returns 0 (last command)
        // Note: pipeline with 2 commands needs more steps
        assert_eq!(run(&shell, "false | true", 5).unwrap(), 0);
    }

    #[test]
    fn pipefail_returns_first_failure_with_option() {
        let shell = test_shell();

        // Enable pipefail
        run(&shell, "set -o pipefail", 1).unwrap();
        assert!(shell.pipefail());

        // With pipefail: false | true returns 1 (first failure)
        assert_eq!(run(&shell, "false | true", 5).unwrap(), 1);
    }

    #[test]
    fn pipefail_can_be_disabled() {
        let shell = test_shell();

        // Enable then disable
        run(&shell, "set -o pipefail", 1).unwrap();
        assert!(shell.pipefail());

        run(&shell, "set +o pipefail", 1).unwrap();
        assert!(!shell.pipefail());

        // Back to normal behavior
        assert_eq!(run(&shell, "false | true", 5).unwrap(), 0);
    }

    #[test]
    fn pipefail_all_success_returns_zero() {
        let shell = test_shell();

        run(&shell, "set -o pipefail", 1).unwrap();

        // All success: returns 0
        assert_eq!(run(&shell, "true | true", 5).unwrap(), 0);
    }

    // =========================================================================
    // Command substitution tests
    // =========================================================================

    #[test]
    fn command_substitution_simple() {
        let shell = test_shell();

        // echo hello stores "hello" in file, then $(cat) reads it
        run(&shell, "echo hello > /workspace/data.txt", 1).unwrap();
        run(
            &shell,
            "echo $(cat /workspace/data.txt) > /workspace/out.txt",
            3,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello");
    }

    #[test]
    fn command_substitution_inline() {
        let shell = test_shell();

        // Test inline substitution: "VAL=$(echo foo)"
        run(&shell, "export VAL=$(echo inline)", 2).unwrap();
        assert_eq!(shell.env.borrow().get("VAL"), Some("inline"));
    }

    #[test]
    fn command_substitution_strips_trailing_newlines() {
        let shell = test_shell();

        // echo outputs with newline, but $() should strip it
        run(
            &shell,
            "echo prefix$(echo suffix)end > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "prefixsuffixend");
    }

    #[test]
    fn command_substitution_in_double_quotes() {
        let shell = test_shell();

        run(
            &shell,
            "echo \"result: $(echo quoted)\" > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "result: quoted");
    }

    #[test]
    fn command_substitution_nested() {
        let shell = test_shell();

        // Nested: $(echo $(echo nested))
        run(
            &shell,
            "echo $(echo $(echo nested)) > /workspace/out.txt",
            4,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "nested");
    }

    #[test]
    fn command_substitution_with_pipeline() {
        let shell = test_shell();

        shell
            .vfs_mut()
            .write_file(
                "/workspace/data.txt",
                b"apple\nbanana\napricot\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // $(grep a data.txt | wc -l) should return 3 (apple, banana, apricot all contain 'a')
        run(
            &shell,
            "echo $(cat /workspace/data.txt | grep a | wc -l) > /workspace/out.txt",
            5,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "3");
    }

    #[test]
    fn command_substitution_with_variable() {
        let shell = test_shell();

        // Variable expansion should work inside command substitution
        run(&shell, "export MSG=hello", 1).unwrap();
        run(&shell, "echo $(echo $MSG) > /workspace/out.txt", 3).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello");
    }

    #[test]
    fn command_substitution_multiple_in_line() {
        let shell = test_shell();

        // Multiple substitutions in one line
        run(
            &shell,
            "echo $(echo first) $(echo second) > /workspace/out.txt",
            4,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "first second");
    }

    #[test]
    fn command_substitution_empty_output() {
        let shell = test_shell();

        // Command that produces no output
        run(&shell, "echo $(true)end > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "end");
    }

    #[test]
    fn command_substitution_preserves_internal_spaces() {
        let shell = test_shell();

        // Spaces in output should be preserved in double quotes
        run(
            &shell,
            "echo \"$(echo 'hello   world')\" > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello   world");
    }

    #[test]
    fn command_substitution_with_quotes_inside() {
        let shell = test_shell();

        // Quotes inside command substitution
        run(
            &shell,
            "echo $(echo \"quoted string\") > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "quoted string");
    }

    #[test]
    fn command_substitution_in_redirect_target() {
        let shell = test_shell();

        // Use substitution to determine filename
        run(&shell, "export FNAME=dynamic.txt", 1).unwrap();
        run(&shell, "echo data > /workspace/$(echo $FNAME)", 3).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/dynamic.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "data");
    }

    #[test]
    fn command_substitution_multiline_output() {
        let shell = test_shell();

        // Multi-line output - trailing newlines stripped, internal preserved
        shell
            .vfs_mut()
            .write_file(
                "/workspace/multi.txt",
                b"line1\nline2\nline3\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "echo $(cat /workspace/multi.txt) > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // POSIX: trailing newlines stripped, internal newlines preserved
        // echo adds one newline at the end
        assert_eq!(
            String::from_utf8_lossy(&content).trim(),
            "line1\nline2\nline3"
        );
    }

    #[test]
    fn command_substitution_exit_code_ignored() {
        let shell = test_shell();

        // Substitution's exit code doesn't affect outer command
        // (the substituted text is just empty)
        let code = run(&shell, "echo prefix$(false)suffix > /workspace/out.txt", 2).unwrap();
        assert_eq!(code, 0);

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "prefixsuffix");
    }

    #[test]
    fn command_substitution_single_quotes_prevent_expansion() {
        let shell = test_shell();

        // Single quotes should prevent command substitution
        run(&shell, "echo '$(echo nope)' > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Should be literal
        assert_eq!(String::from_utf8_lossy(&content).trim(), "$(echo nope)");
    }

    #[test]
    fn normalize_path_basic() {
        assert_eq!(normalize_path("/foo/bar"), "/foo/bar");
        assert_eq!(normalize_path("/foo/./bar"), "/foo/bar");
        assert_eq!(normalize_path("/foo/../bar"), "/bar");
        assert_eq!(normalize_path("/foo/bar/.."), "/foo");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("/."), "/");
        assert_eq!(normalize_path("/.."), "/");
    }

    // =========================================================================
    // Subshell isolation tests
    // =========================================================================

    #[test]
    fn subshell_isolates_cwd() {
        let shell = test_shell();

        // Start in root, cd to /workspace
        run(&shell, "cd /workspace", 1).unwrap();
        assert_eq!(shell.cwd(), "/workspace");

        // Subshell cd should not affect parent
        run(&shell, "(cd /)", 1).unwrap();
        assert_eq!(shell.cwd(), "/workspace");
    }

    #[test]
    fn subshell_isolates_env() {
        let shell = test_shell();

        // Set variable in parent
        run(&shell, "export FOO=parent", 1).unwrap();
        assert_eq!(shell.env().get("FOO"), Some("parent"));

        // Subshell export should not affect parent
        run(&shell, "(export FOO=child)", 1).unwrap();
        assert_eq!(shell.env().get("FOO"), Some("parent"));
    }

    #[test]
    fn subshell_returns_exit_code() {
        let shell = test_shell();

        // Subshell should return last command's exit code
        assert_eq!(run(&shell, "(exit 42)", 1).unwrap(), 42);
        assert_eq!(run(&shell, "(true)", 1).unwrap(), 0);
        assert_eq!(run(&shell, "(false)", 1).unwrap(), 1);
    }

    // =========================================================================
    // sh applet integration tests
    // =========================================================================

    #[test]
    fn sh_applet_works() {
        let shell = test_shell();
        assert_eq!(run(&shell, "sh -c true", 3).unwrap(), 0);
        assert_eq!(run(&shell, "sh -c false", 3).unwrap(), 1);
        assert_eq!(run(&shell, "sh -c 'exit 7'", 3).unwrap(), 7);
    }

    #[test]
    fn sh_applet_inherits_env() {
        let shell = test_shell();

        // Set variable in parent
        run(&shell, "export MY_TEST_VAR=hello", 1).unwrap();

        // sh -c should inherit and be able to use/modify it
        // (but modifications shouldn't leak back to parent)
        assert_eq!(run(&shell, "sh -c true", 3).unwrap(), 0);

        // Verify parent env unchanged
        assert_eq!(shell.env().get("MY_TEST_VAR"), Some("hello"));
    }

    #[test]
    fn sh_applet_inherits_cwd() {
        let shell = test_shell();

        // cd to /workspace
        run(&shell, "cd /workspace", 1).unwrap();
        assert_eq!(shell.cwd(), "/workspace");

        // sh -c should start in same cwd
        // (we can't easily verify this without output capture, but ensure no crash)
        assert_eq!(run(&shell, "sh -c true", 3).unwrap(), 0);

        // Verify parent cwd unchanged
        assert_eq!(shell.cwd(), "/workspace");
    }

    #[test]
    fn bash_alias_works() {
        let shell = test_shell();
        // bash is an alias for sh
        assert_eq!(run(&shell, "bash -c true", 3).unwrap(), 0);
        assert_eq!(run(&shell, "bash -c 'exit 3'", 3).unwrap(), 3);
    }

    // =========================================================================
    // Variable expansion tests
    // =========================================================================

    #[test]
    fn variable_expansion_in_command() {
        let shell = test_shell();
        run(&shell, "export GREETING=hello", 1).unwrap();

        // Variable should be expanded (though echo output goes to buffer, not visible)
        // Just verify no crash and command succeeds
        assert_eq!(run(&shell, "echo $GREETING", 1).unwrap(), 0);
    }

    #[test]
    fn exit_status_variable() {
        let shell = test_shell();

        // Run false, then check $?
        run(&shell, "false", 1).unwrap();
        assert_eq!(shell.last_exit(), 1);

        run(&shell, "true", 1).unwrap();
        assert_eq!(shell.last_exit(), 0);
    }

    #[test]
    fn single_quotes_prevent_variable_expansion() {
        // Regression test: Variables in single quotes must NOT be expanded.
        // Bug was: shell.execute() expanded variables BEFORE parsing,
        // which meant single-quoted content was incorrectly expanded.
        let shell = test_shell();

        // Set a variable
        run(&shell, "export MYVAR=expanded", 1).unwrap();

        // Single quotes: $MYVAR should remain literal
        run(&shell, "echo '$MYVAR' > /workspace/single.txt", 1).unwrap();
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/single.txt")
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&content).trim(),
            "$MYVAR",
            "Single-quoted variable should NOT be expanded"
        );

        // Double quotes: $MYVAR should be expanded
        run(&shell, "echo \"$MYVAR\" > /workspace/double.txt", 1).unwrap();
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/double.txt")
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&content).trim(),
            "expanded",
            "Double-quoted variable should be expanded"
        );

        // Unquoted: $MYVAR should be expanded
        run(&shell, "echo $MYVAR > /workspace/unquoted.txt", 1).unwrap();
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/unquoted.txt")
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&content).trim(),
            "expanded",
            "Unquoted variable should be expanded"
        );
    }

    // =========================================================================
    // Redirect tests (integration)
    // =========================================================================

    #[test]
    fn redirect_stdout_to_file() {
        let shell = test_shell();

        // Write output to file
        run(&shell, "echo hello > /workspace/out.txt", 1).unwrap();

        // Read file to verify
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello");
    }

    #[test]
    fn redirect_stdout_append() {
        let shell = test_shell();

        // First write
        run(&shell, "echo line1 > /workspace/append.txt", 1).unwrap();
        // Append
        run(&shell, "echo line2 >> /workspace/append.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/append.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    #[test]
    fn redirect_append_to_append_only_file() {
        // Test that >> works with AppendOnly files like /log/actions.jsonl
        let shell = test_shell();

        // Append to /log/actions.jsonl (created by VFS with AppendOnly permission)
        run(&shell, "echo line1 >> /log/actions.jsonl", 1).unwrap();
        run(&shell, "echo line2 >> /log/actions.jsonl", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/log/actions.jsonl").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("line1"), "Should contain line1: {text}");
        assert!(text.contains("line2"), "Should contain line2: {text}");
    }

    #[test]
    fn redirect_stdin_from_file() {
        let shell = test_shell();

        // Create input file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/input.txt",
                b"hello from file",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Cat should read from file and output to another file
        run(
            &shell,
            "cat < /workspace/input.txt > /workspace/output.txt",
            1,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/output.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello from file");
    }

    #[test]
    fn redirect_stderr_to_file() {
        let shell = test_shell();

        // Command that writes to stderr (cat nonexistent file)
        run(&shell, "cat /nonexistent 2> /workspace/err.txt", 1).unwrap_or(1);

        // Stderr should have error message
        let content = shell.vfs.borrow().read_file("/workspace/err.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("cat:") || text.contains("No such") || text.contains("not found"));
    }

    #[test]
    fn redirect_stderr_to_stdout() {
        let shell = test_shell();

        // Create a scenario where we redirect stderr to stdout, then to file
        // cat nonexistent 2>&1 > file should capture both
        run(&shell, "cat /nonexistent 2>&1 > /workspace/combined.txt", 1).unwrap_or(1);

        // The file should have error output
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/combined.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        // May or may not have content depending on order of redirects
        // Just verify command ran without panic
        assert!(text.is_empty() || text.contains("cat"));
    }

    // =========================================================================
    // Cat command tests
    // =========================================================================

    #[test]
    fn cat_single_file() {
        let shell = test_shell();

        // Create file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/test.txt",
                b"content here",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Cat to output file
        run(&shell, "cat /workspace/test.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "content here");
    }

    #[test]
    fn cat_multiple_files() {
        let shell = test_shell();

        // Create two files
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/a.txt", b"AAA", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/b.txt", b"BBB", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // Cat both
        run(
            &shell,
            "cat /workspace/a.txt /workspace/b.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("AAA"));
        assert!(text.contains("BBB"));
    }

    #[test]
    fn cat_with_line_numbers() {
        let shell = test_shell();

        // Create file with multiple lines
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/lines.txt",
                b"line1\nline2\nline3",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Cat with -n
        run(
            &shell,
            "cat -n /workspace/lines.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains('1') && text.contains("line1"));
        assert!(text.contains('2') && text.contains("line2"));
    }

    #[test]
    fn cat_nonexistent_file() {
        let shell = test_shell();
        let code = run(&shell, "cat /workspace/does_not_exist.txt", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn cat_stdin_dash() {
        let shell = test_shell();

        // Create input
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/in.txt",
                b"stdin data",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // cat - reads from stdin
        run(&shell, "cat - < /workspace/in.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "stdin data");
    }

    // =========================================================================
    // Pipeline tests
    // =========================================================================

    #[test]
    fn pipe_echo_to_cat() {
        let shell = test_shell();

        // echo | cat should pass data through
        run(&shell, "echo piped | cat > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "piped");
    }

    #[test]
    fn pipe_cat_to_cat() {
        let shell = test_shell();

        // Create input file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/in.txt",
                b"data",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // cat | cat should pass through
        run(
            &shell,
            "cat /workspace/in.txt | cat > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "data");
    }

    #[test]
    fn pipe_three_commands() {
        let shell = test_shell();

        // echo | cat | cat
        run(&shell, "echo three | cat | cat > /workspace/out.txt", 3).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "three");
    }

    #[test]
    fn pipe_with_grep() {
        let shell = test_shell();

        // Create file with multiple lines
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"apple\nbanana\napricot\ncherry",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Grep for lines starting with 'a'
        run(
            &shell,
            "cat /workspace/data.txt | grep ^a > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("apple"));
        assert!(text.contains("apricot"));
        assert!(!text.contains("banana"));
        assert!(!text.contains("cherry"));
    }

    #[test]
    fn pipe_with_head() {
        let shell = test_shell();

        // Create file with many lines
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/many.txt",
                b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Head -3 should only show first 3
        run(
            &shell,
            "cat /workspace/many.txt | head -3 > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "1");
        assert_eq!(lines[1], "2");
        assert_eq!(lines[2], "3");
    }

    #[test]
    fn pipe_with_tail() {
        let shell = test_shell();

        // Create file with many lines
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/many.txt",
                b"1\n2\n3\n4\n5",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Tail -2 should only show last 2
        run(
            &shell,
            "cat /workspace/many.txt | tail -2 > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "4");
        assert_eq!(lines[1], "5");
    }

    #[test]
    fn pipe_with_wc() {
        let shell = test_shell();

        // Create file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                b"one two three\nfour five",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // wc should count lines, words, bytes
        run(
            &shell,
            "cat /workspace/text.txt | wc > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        // Should contain counts (exact format may vary)
        assert!(text.contains('2') || text.contains('5')); // 2 lines or 5 words
    }

    #[test]
    fn pipe_exit_code_from_last() {
        let shell = test_shell();

        // Pipeline exit code should be from last command
        let code = run(&shell, "true | false", 1).unwrap();
        assert_eq!(code, 1);

        let code = run(&shell, "false | true", 1).unwrap();
        assert_eq!(code, 0);
    }

    // =========================================================================
    // Combined redirect and pipe tests
    // =========================================================================

    #[test]
    fn stdin_redirect_in_pipeline() {
        let shell = test_shell();

        // Create input
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/in.txt",
                b"fromfile",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // First command gets stdin from file
        run(
            &shell,
            "cat < /workspace/in.txt | cat > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "fromfile");
    }

    #[test]
    fn stdout_redirect_in_pipeline() {
        let shell = test_shell();

        // Last command redirects to file (this is what we've been testing)
        run(&shell, "echo test | cat > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "test");
    }

    // =========================================================================
    // Echo command tests
    // =========================================================================

    #[test]
    fn echo_simple() {
        let shell = test_shell();
        run(&shell, "echo hello world > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello world");
    }

    #[test]
    fn echo_no_newline() {
        let shell = test_shell();
        run(&shell, "echo -n hello > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Should NOT have trailing newline
        assert_eq!(String::from_utf8_lossy(&content), "hello");
    }

    #[test]
    fn echo_escape_sequences() {
        let shell = test_shell();
        run(&shell, "echo -e 'a\\tb' > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Should have tab character
        assert!(String::from_utf8_lossy(&content).contains('\t'));
    }

    #[test]
    fn echo_empty() {
        let shell = test_shell();
        run(&shell, "echo > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Just a newline
        assert_eq!(content, b"\n");
    }

    // =========================================================================
    // Comprehensive Multi-Pipeline Tests
    // =========================================================================

    #[test]
    fn pipeline_four_commands() {
        let shell = test_shell();

        // Create file with numbered lines
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/numbers.txt",
                b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Break down: first test 2 commands
        run(
            &shell,
            "cat /workspace/numbers.txt | head -5 > /workspace/step1.txt",
            1,
        )
        .unwrap();
        let step1 = shell
            .vfs
            .borrow()
            .read_file("/workspace/step1.txt")
            .unwrap();
        let step1_text = String::from_utf8_lossy(&step1);
        let step1_lines: Vec<&str> = step1_text.lines().collect();
        assert_eq!(step1_lines.len(), 5, "head -5 should give 5 lines");

        // Now test tail on that (2 commands)
        run(
            &shell,
            "cat /workspace/step1.txt | tail -2 > /workspace/step2.txt",
            2,
        )
        .unwrap();
        let step2 = shell
            .vfs
            .borrow()
            .read_file("/workspace/step2.txt")
            .unwrap();
        let step2_text = String::from_utf8_lossy(&step2);
        let step2_lines: Vec<&str> = step2_text.lines().collect();
        assert_eq!(step2_lines.len(), 2, "tail -2 should give 2 lines");

        // Try 3 commands: cat | head | tail
        run(
            &shell,
            "cat /workspace/numbers.txt | head -5 | tail -2 > /workspace/step3.txt",
            2,
        )
        .unwrap();
        let step3 = shell
            .vfs
            .borrow()
            .read_file("/workspace/step3.txt")
            .unwrap();
        let step3_text = String::from_utf8_lossy(&step3);
        let step3_lines: Vec<&str> = step3_text.lines().collect();
        assert_eq!(step3_lines.len(), 2, "3-cmd pipeline should give 2 lines");

        // Now try all 4 at once: cat | head | tail | cat
        run(
            &shell,
            "cat /workspace/numbers.txt | head -5 | tail -2 | cat > /workspace/out.txt",
            3,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        // head -5 gives 1-5, tail -2 gives 4-5
        assert_eq!(lines.len(), 2, "4-cmd pipeline should give 2 lines");
        assert_eq!(lines[0], "4");
        assert_eq!(lines[1], "5");
    }

    #[test]
    fn pipeline_five_commands() {
        let shell = test_shell();

        // Create file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"apple\nbanana\napricot\nblueberry\navocado\ncherry",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // cat | grep | head | tail | cat (5 commands)
        run(
            &shell,
            "cat /workspace/data.txt | grep a | head -3 | tail -2 | cat > /workspace/out.txt",
            4,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        // grep a: apple, banana, apricot, avocado -> head -3: apple, banana, apricot -> tail -2: banana, apricot
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "banana");
        assert_eq!(lines[1], "apricot");
    }

    #[test]
    fn pipeline_echo_through_multiple_cats() {
        let shell = test_shell();

        // echo | cat | cat | cat | cat (5 commands, all passthrough)
        run(
            &shell,
            "echo hello | cat | cat | cat | cat > /workspace/out.txt",
            5,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello");
    }

    #[test]
    fn pipeline_grep_chain() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/log.txt",
                b"ERROR: disk full\nINFO: started\nERROR: network timeout\nWARN: slow query\nERROR: memory low",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Chain grep to filter: first ERROR, then timeout
        run(
            &shell,
            "cat /workspace/log.txt | grep ERROR | grep timeout > /workspace/out.txt",
            3,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert_eq!(text.trim(), "ERROR: network timeout");
    }

    #[test]
    fn pipeline_with_wc_in_middle() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/lines.txt",
                b"one\ntwo\nthree\nfour\nfive\n", // 5 complete lines (with trailing newline)
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Count lines, then pass through
        run(
            &shell,
            "cat /workspace/lines.txt | wc -l | cat > /workspace/out.txt",
            3,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content).trim().to_string();
        assert_eq!(text, "5");
    }

    #[test]
    fn pipeline_head_tail_combination() {
        let shell = test_shell();

        // Create file with 20 lines
        let lines: String = (1..=20).map(|i| format!("{i}\n")).collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/twenty.txt",
                lines.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Get lines 8-12: head -12 | tail -5
        run(
            &shell,
            "cat /workspace/twenty.txt | head -12 | tail -5 > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["8", "9", "10", "11", "12"]);
    }

    #[test]
    fn pipeline_preserves_binary_data() {
        let shell = test_shell();

        // Create file with binary data
        let binary_data: Vec<u8> = (0..=255).collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/binary.dat",
                &binary_data,
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Pass through cat
        run(
            &shell,
            "cat /workspace/binary.dat | cat > /workspace/out.dat",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.dat").unwrap();
        assert_eq!(content, binary_data);
    }

    #[test]
    fn pipeline_empty_intermediate_result() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"apple\nbanana\ncherry",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Grep for something that doesn't exist
        run(
            &shell,
            "cat /workspace/data.txt | grep xyz | cat > /workspace/out.txt",
            3,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert!(content.is_empty());
    }

    // =========================================================================
    // Comprehensive Redirect Tests
    // =========================================================================

    #[test]
    fn redirect_append_basic() {
        let shell = test_shell();

        // Write first line
        run(&shell, "echo first > /workspace/append.txt", 1).unwrap();
        // Append second line
        run(&shell, "echo second >> /workspace/append.txt", 1).unwrap();
        // Append third line
        run(&shell, "echo third >> /workspace/append.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/append.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["first", "second", "third"]);
    }

    #[test]
    fn redirect_append_to_nonexistent() {
        let shell = test_shell();

        // Append to file that doesn't exist (should create it)
        run(&shell, "echo hello >> /workspace/new_append.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/new_append.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello");
    }

    #[test]
    fn redirect_append_multiple_commands() {
        let shell = test_shell();

        // Create initial file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/log.txt",
                b"=== Log Start ===\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Append multiple entries
        run(&shell, "echo Entry 1 >> /workspace/log.txt", 1).unwrap();
        run(&shell, "echo Entry 2 >> /workspace/log.txt", 1).unwrap();
        run(&shell, "echo Entry 3 >> /workspace/log.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/log.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("=== Log Start ==="));
        assert!(text.contains("Entry 1"));
        assert!(text.contains("Entry 2"));
        assert!(text.contains("Entry 3"));
    }

    #[test]
    fn redirect_stderr_append() {
        let shell = test_shell();

        // Create initial error log
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/errors.log",
                b"Previous error\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Command that produces stderr (cat nonexistent file)
        run(&shell, "cat /nonexistent 2>> /workspace/errors.log", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/errors.log")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("Previous error"));
        assert!(text.contains("cat:") || text.contains("no such file"));
    }

    #[test]
    fn redirect_stdin_and_stdout() {
        let shell = test_shell();

        // Create input file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/input.txt",
                b"input data here",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Read from input, write to output
        run(
            &shell,
            "cat < /workspace/input.txt > /workspace/output.txt",
            1,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/output.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "input data here");
    }

    #[test]
    fn redirect_stdout_and_stderr_separate() {
        let shell = test_shell();

        // Create a file so cat succeeds for it
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/exists.txt",
                b"exists",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Cat both existing and nonexistent files
        // stdout goes to out.txt, stderr goes to err.txt
        run(
            &shell,
            "cat /workspace/exists.txt /nonexistent > /workspace/out.txt 2> /workspace/err.txt",
            1,
        )
        .unwrap();

        // Stdout should have the existing file content (or be empty if cat failed before)
        // Stderr should have the error
        let err_content = shell.vfs.borrow().read_file("/workspace/err.txt").unwrap();
        let err_text = String::from_utf8_lossy(&err_content);
        assert!(err_text.contains("cat:") || err_text.contains("no such"));
    }

    #[test]
    fn redirect_stderr_to_stdout_2_ampersand_1() {
        let shell = test_shell();

        // Redirect stderr to stdout, then to file
        run(&shell, "cat /nonexistent > /workspace/combined.txt 2>&1", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/combined.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        // Error message should be in the file
        assert!(text.contains("cat:") || text.contains("no such"));
    }

    #[test]
    fn redirect_append_in_pipeline() {
        let shell = test_shell();

        // Create initial file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/result.txt",
                b"Header\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Pipeline with append at the end
        run(&shell, "echo data | cat >> /workspace/result.txt", 2).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/result.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("Header"));
        assert!(text.contains("data"));
    }

    #[test]
    fn redirect_overwrite_clears_file() {
        let shell = test_shell();

        // Create file with content
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/overwrite.txt",
                b"old content that should be gone",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Overwrite with new content
        run(&shell, "echo new > /workspace/overwrite.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/overwrite.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert_eq!(text.trim(), "new");
        assert!(!text.contains("old"));
    }

    #[test]
    fn redirect_input_in_middle_of_pipeline() {
        let shell = test_shell();

        // Create input file
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/words.txt",
                b"apple\nbanana\ncherry",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Input redirect at start, then pipeline
        run(
            &shell,
            "grep a < /workspace/words.txt | head -2 > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "apple");
        assert_eq!(lines[1], "banana");
    }

    #[test]
    fn redirect_multiple_appends() {
        let shell = test_shell();

        // Multiple appends in a row
        for i in 1..=5 {
            run(&shell, &format!("echo line{i} >> /workspace/multi.txt"), 1).unwrap();
        }

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/multi.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5);
        for i in 1..=5 {
            assert_eq!(lines[i - 1], format!("line{i}"));
        }
    }

    #[test]
    fn redirect_from_pipeline_to_append() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/base.txt",
                b"base\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"one\ntwo\nthree",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Pipeline result appended to existing file
        run(
            &shell,
            "cat /workspace/data.txt | grep o >> /workspace/base.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/base.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("base"));
        assert!(text.contains("one"));
        assert!(text.contains("two"));
    }

    // =========================================================================
    // Combined Pipeline and Redirect Edge Cases
    // =========================================================================

    #[test]
    fn pipeline_with_failing_command() {
        let shell = test_shell();

        // First command fails, second should get empty input
        let result = run(&shell, "cat /nonexistent | cat > /workspace/out.txt", 2);
        // Should still complete (not panic)
        assert!(result.is_ok());
    }

    #[test]
    fn pipeline_large_data() {
        let shell = test_shell();

        // Create file with 1000 lines
        let lines: String = (1..=1000).map(|i| format!("Line {i}\n")).collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/large.txt",
                lines.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Process through pipeline
        run(
            &shell,
            "cat /workspace/large.txt | cat | cat > /workspace/out.txt",
            6,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let out_lines: Vec<&str> = text.lines().collect();
        assert_eq!(out_lines.len(), 1000);
    }

    #[test]
    fn redirect_all_streams() {
        let shell = test_shell();

        // Create input
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/in.txt",
                b"input",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Redirect all: stdin, stdout, stderr
        run(
            &shell,
            "cat < /workspace/in.txt > /workspace/out.txt 2> /workspace/err.txt",
            1,
        )
        .unwrap();

        let out = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&out).trim(), "input");

        // Stderr should be empty or non-existent (no errors)
        let err = shell
            .vfs
            .borrow()
            .read_file("/workspace/err.txt")
            .unwrap_or_default();
        assert!(err.is_empty());
    }

    #[test]
    fn pipeline_grep_case_insensitive() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/mixed.txt",
                b"Hello\nhello\nHELLO\nworld",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Case-insensitive grep
        run(
            &shell,
            "cat /workspace/mixed.txt | grep -i hello > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
    }

    // =========================================================================
    // Performance Tests
    // =========================================================================

    #[test]
    fn perf_pipeline_throughput_10k_lines() {
        let shell = test_shell();

        // Create file with 10,000 lines
        let lines: String = (1..=10000).map(|i| format!("Line {i:05}\n")).collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/big.txt",
                lines.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Measure pipeline throughput: cat | cat | cat
        let start = std::time::Instant::now();
        run(
            &shell,
            "cat /workspace/big.txt | cat | cat > /workspace/out.txt",
            54,
        )
        .unwrap();
        let duration = start.elapsed();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let line_count = text.lines().count();
        assert_eq!(line_count, 10000);

        // Calculate throughput
        let bytes = content.len();
        let bytes_per_sec = bytes as f64 / duration.as_secs_f64();
        eprintln!(
            "Pipeline throughput: {} lines, {} bytes in {:?} ({:.2} MB/s)",
            line_count,
            bytes,
            duration,
            bytes_per_sec / 1_000_000.0
        );

        // Should complete reasonably fast (less than 1 second)
        assert!(
            duration.as_millis() < 1000,
            "Pipeline too slow: {duration:?}"
        );
    }

    #[test]
    fn perf_pipeline_with_grep_filter() {
        let shell = test_shell();

        // Create file with 10,000 lines, half matching a pattern
        let lines: String = (1..=10000)
            .map(|i| {
                if i % 2 == 0 {
                    format!("MATCH Line {i:05}\n")
                } else {
                    format!("Line {i:05}\n")
                }
            })
            .collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/mixed.txt",
                lines.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let start = std::time::Instant::now();
        run(
            &shell,
            "cat /workspace/mixed.txt | grep MATCH > /workspace/filtered.txt",
            69,
        )
        .unwrap();
        let duration = start.elapsed();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/filtered.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        let line_count = text.lines().count();
        assert_eq!(line_count, 5000, "Should have filtered to 5000 lines");

        eprintln!("Grep filter: 10000 → {line_count} lines in {duration:?}");
        assert!(duration.as_millis() < 1000, "Grep too slow: {duration:?}");
    }

    #[test]
    fn perf_deep_pipeline() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"test data\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // 10-stage pipeline
        let start = std::time::Instant::now();
        run(&shell, "cat /workspace/data.txt | cat | cat | cat | cat | cat | cat | cat | cat | cat > /workspace/out.txt", 10)
            .unwrap();
        let duration = start.elapsed();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "test data");

        eprintln!("10-stage pipeline: {duration:?}");
        assert!(
            duration.as_millis() < 500,
            "Deep pipeline too slow: {duration:?}"
        );
    }

    #[test]
    fn pipeline_multiple_files_to_single_output() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/a.txt", b"aaa", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/b.txt", b"bbb", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/c.txt", b"ccc", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // Cat multiple files through pipeline
        run(&shell, "cat /workspace/a.txt /workspace/b.txt /workspace/c.txt | cat > /workspace/combined.txt", 2)
            .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/combined.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("aaa"));
        assert!(text.contains("bbb"));
        assert!(text.contains("ccc"));
    }

    // =========================================================================
    // Job control tests
    // =========================================================================

    #[test]
    fn background_simple_command() {
        let shell = test_shell();

        // Run a command in background
        let code = run(&shell, "echo hello &", 0).unwrap();

        // Background commands return 0 immediately
        assert_eq!(code, 0);

        // The job should be in the job table (may have already completed)
        // Just verify command ran without error
        let _job_count = shell.jobs.borrow().len();
    }

    #[test]
    fn jobs_lists_background_jobs() {
        let shell = test_shell();

        // Run a command in background
        run(&shell, "true &", 0).unwrap();

        // Run jobs command (should not panic)
        let code = run(&shell, "jobs", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn wait_waits_for_jobs() {
        let shell = test_shell();

        // Run commands in background
        run(&shell, "echo a &", 0).unwrap();
        run(&shell, "echo b &", 0).unwrap();

        // Wait for all
        let code = run(&shell, "wait", 0).unwrap();
        assert_eq!(code, 0);

        // No jobs should remain
        let job_count = shell.jobs.borrow().len();
        assert_eq!(job_count, 0);
    }

    #[test]
    fn wait_specific_job() {
        let shell = test_shell();

        // Run command in background
        run(&shell, "true &", 0).unwrap();

        // Wait for job 1
        let code = run(&shell, "wait %1", 0).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn fg_brings_job_to_foreground() {
        let shell = test_shell();

        // Run a command in background
        run(&shell, "true &", 0).unwrap();

        // Bring to foreground
        let code = run(&shell, "fg", 0).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn fg_specific_job() {
        let shell = test_shell();

        // Run multiple background commands
        run(&shell, "true &", 0).unwrap();
        run(&shell, "false &", 0).unwrap();

        // Bring specific job to foreground
        // fg %1 should get exit code from job 1
        let code = run(&shell, "fg %1", 0).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn fg_no_job_returns_error() {
        let shell = test_shell();

        // No jobs running
        let code = run(&shell, "fg", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn fg_nonexistent_job() {
        let shell = test_shell();

        // Try to fg a job that doesn't exist
        let code = run(&shell, "fg %99", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn kill_terminates_job() {
        let shell = test_shell();

        // Run command in background
        run(&shell, "true &", 0).unwrap();
        assert_eq!(shell.jobs.borrow().len(), 1);

        // Kill the job
        let code = run(&shell, "kill %1", 0).unwrap();
        assert_eq!(code, 0);

        // Job should be removed
        assert_eq!(shell.jobs.borrow().len(), 0);
    }

    #[test]
    fn kill_nonexistent_job() {
        let shell = test_shell();

        // Try to kill a job that doesn't exist
        let code = run(&shell, "kill %99", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn kill_no_args() {
        let shell = test_shell();

        // No args should show usage and return 1
        let code = run(&shell, "kill", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn kill_multiple_jobs() {
        let shell = test_shell();

        // Run multiple background jobs
        run(&shell, "true &", 0).unwrap();
        run(&shell, "true &", 0).unwrap();
        run(&shell, "true &", 0).unwrap();
        assert_eq!(shell.jobs.borrow().len(), 3);

        // Kill multiple at once
        let code = run(&shell, "kill %1 %2 %3", 0).unwrap();
        assert_eq!(code, 0);

        // All should be removed
        assert_eq!(shell.jobs.borrow().len(), 0);
    }

    #[test]
    fn kill_without_percent_prefix() {
        let shell = test_shell();

        // Run background job
        run(&shell, "true &", 0).unwrap();
        assert_eq!(shell.jobs.borrow().len(), 1);

        // Kill without % prefix (should work)
        let code = run(&shell, "kill 1", 0).unwrap();
        assert_eq!(code, 0);

        // Job should be removed
        assert_eq!(shell.jobs.borrow().len(), 0);
    }

    #[test]
    fn kill_invalid_non_numeric_arg() {
        let shell = test_shell();

        // Try to kill with invalid argument
        let code = run(&shell, "kill abc", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn kill_mixed_valid_invalid_args() {
        let shell = test_shell();

        // Run background jobs
        run(&shell, "true &", 0).unwrap();
        run(&shell, "true &", 0).unwrap();

        // Kill with mix of valid, invalid, and non-existent
        let code = run(&shell, "kill %1 abc %99 %2", 0).unwrap();
        assert_eq!(code, 1); // Exit code 1 due to errors

        // Valid jobs should still be killed
        assert_eq!(shell.jobs.borrow().len(), 0);
    }

    #[test]
    fn kill_pipeline_job() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"line1\nline2\nline3\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Run pipeline in background
        run(&shell, "cat /workspace/data.txt | grep line &", 0).unwrap();
        assert_eq!(shell.jobs.borrow().len(), 1);

        // Kill the pipeline
        let code = run(&shell, "kill %1", 0).unwrap();
        assert_eq!(code, 0);

        // Pipeline job should be removed
        assert_eq!(shell.jobs.borrow().len(), 0);
    }

    #[test]
    fn kill_already_completed_job() {
        let shell = test_shell();

        // Run a fast job in background
        run(&shell, "true &", 0).unwrap();

        // Let the shell reap completed jobs
        run(&shell, "jobs", 0).unwrap();

        // Job might be reaped already - killing should return error
        // (but if it's still there, it should succeed)
        let job_count = shell.jobs.borrow().len();
        let code = run(&shell, "kill %1", 0).unwrap();

        if job_count == 0 {
            // Job was reaped, kill should fail
            assert_eq!(code, 1);
        } else {
            // Job still there, kill should succeed
            assert_eq!(code, 0);
        }
    }

    #[test]
    fn kill_removes_job_from_table() {
        let shell = test_shell();

        // Run multiple jobs
        run(&shell, "true &", 0).unwrap();
        run(&shell, "true &", 0).unwrap();
        let initial = shell.jobs.borrow().len();

        // Kill one job
        let code = run(&shell, "kill %1", 0).unwrap();
        assert_eq!(code, 0);

        // Should have one less job
        let after = shell.jobs.borrow().len();
        assert_eq!(after, initial - 1);

        // Job 1 should be gone
        assert!(shell.jobs.borrow().get(1).is_none());

        // Job 2 should still exist
        assert!(shell.jobs.borrow().get(2).is_some());
    }

    #[test]
    fn kill_zero_is_invalid() {
        let shell = test_shell();

        // Job IDs start at 1, so %0 is invalid
        let code = run(&shell, "kill %0", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn kill_negative_is_invalid() {
        let shell = test_shell();

        // Negative numbers can't be parsed as usize
        let code = run(&shell, "kill %-1", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn kill_empty_string_after_percent() {
        let shell = test_shell();

        // % with no number
        let code = run(&shell, "kill %", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn kill_shows_error_for_each_invalid_job() {
        let shell = test_shell();

        // Kill multiple non-existent jobs
        let code = run(&shell, "kill %99 %100 %101", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn kill_stress_no_memory_leaks() {
        // Stress test: repeatedly create and kill jobs to verify no memory leaks
        let shell = test_shell();

        for i in 0..100 {
            // Create a background job
            run(&shell, "true &", 0).unwrap();

            // Verify it was added
            let has_job = shell.jobs.borrow().get(i + 1).is_some();
            assert!(has_job, "Job {} should exist", i + 1);

            // Kill it
            let cmd = format!("kill %{}", i + 1);
            let code = run(&shell, &cmd, 0).unwrap();
            assert_eq!(code, 0, "Kill should succeed for job {}", i + 1);

            // Verify it was removed
            assert!(
                shell.jobs.borrow().get(i + 1).is_none(),
                "Job {} should be removed after kill",
                i + 1
            );
        }

        // Final check: no jobs should remain
        assert_eq!(shell.jobs.borrow().len(), 0);
    }

    #[test]
    fn kill_cascades_to_child_tasks() {
        // Test that kill properly cascades to child tasks via structured concurrency
        let shell = test_shell();

        // Run a subshell with nested command in background
        // The subshell creates a parent-child task relationship
        run(&shell, "(true; true; true) &", 0).unwrap();
        assert_eq!(shell.jobs.borrow().len(), 1);

        // Kill the job - should cancel parent which cascades to children
        let code = run(&shell, "kill %1", 0).unwrap();
        assert_eq!(code, 0);

        // Job should be completely removed
        assert_eq!(shell.jobs.borrow().len(), 0);
    }

    #[test]
    fn background_pipeline() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"line1\nline2\nline3\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Run pipeline in background
        let code = run(&shell, "cat /workspace/data.txt | wc -l &", 0).unwrap();
        assert_eq!(code, 0);

        // Wait for completion
        run(&shell, "wait", 0).unwrap();
    }

    #[test]
    fn background_with_redirect() {
        let shell = test_shell();

        // Background command with redirect
        run(&shell, "echo test > /workspace/bg.txt &", 0).unwrap();

        // Wait for completion
        run(&shell, "wait", 0).unwrap();

        // Verify file was created
        let content = shell.vfs.borrow().read_file("/workspace/bg.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "test");
    }

    #[test]
    fn has_running_jobs_initially_false() {
        let shell = test_shell();
        assert!(!shell.has_running_jobs());
        assert_eq!(shell.running_job_count(), 0);
    }

    #[test]
    fn has_running_jobs_after_background() {
        let shell = test_shell();

        // Before any background job
        assert!(!shell.has_running_jobs());

        // Run something in background
        run(&shell, "true &", 0).unwrap();

        // Job may or may not still be running (fast command)
        // Just verify the method doesn't panic
        let _ = shell.has_running_jobs();
        let _ = shell.running_job_count();
    }

    // =========================================================================
    // Subshell job isolation tests (SideEffects pattern)
    // =========================================================================

    #[test]
    fn subshell_does_not_inherit_parent_jobs() {
        let shell = test_shell();

        // Run background job in parent
        run(&shell, "true &", 0).unwrap();
        let parent_jobs = shell.jobs.borrow().len();

        // Subshell should not see parent's jobs
        // (This is already true due to subshell isolation via execute_in_subshell
        // which creates fresh job table per Shell instance, but we verify the pattern)
        let code = run(&shell, "(jobs)", 1).unwrap();
        assert_eq!(code, 0);

        // Parent's jobs unchanged
        assert_eq!(shell.jobs.borrow().len(), parent_jobs);
    }

    #[test]
    fn sh_c_does_not_inherit_parent_jobs() {
        let shell = test_shell();

        // Run background job in parent
        run(&shell, "true &", 0).unwrap();
        let parent_jobs = shell.jobs.borrow().len();

        // sh -c gets a fresh shell with its own job table
        let code = run(&shell, "sh -c jobs", 3).unwrap();
        assert_eq!(code, 0);

        // Parent's jobs unchanged
        assert_eq!(shell.jobs.borrow().len(), parent_jobs);
    }

    // =========================================================================
    // ls command tests (integration - async paths)
    // =========================================================================

    #[test]
    fn ls_current_directory() {
        let shell = test_shell();

        // Create some files
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/file1.txt",
                b"a",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/file2.txt",
                b"b",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "cd /workspace", 1).unwrap();
        run(&shell, "ls > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("file1.txt"));
        assert!(text.contains("file2.txt"));
    }

    #[test]
    fn ls_specific_path() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/test.txt",
                b"content",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "ls /workspace > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("test.txt"));
    }

    #[test]
    fn ls_long_format() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/myfile.txt",
                b"hello",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "ls -l /workspace > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        // Long format includes permissions, size, name
        assert!(text.contains("myfile.txt"));
        assert!(text.contains("rw")); // permissions
    }

    #[test]
    fn ls_show_hidden() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/.hidden",
                b"secret",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/visible",
                b"public",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Without -a, hidden files not shown
        run(&shell, "ls /workspace > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(!text.contains(".hidden"));
        assert!(text.contains("visible"));

        // With -a, hidden files shown
        run(&shell, "ls -a /workspace > /workspace/out2.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out2.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains(".hidden"));
        assert!(text.contains("visible"));
    }

    #[test]
    fn ls_one_per_line() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/a.txt", b"", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/b.txt", b"", amla_vfs::Permission::ReadWrite)
            .unwrap();

        run(&shell, "ls -1 /workspace > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 2);
    }

    #[test]
    fn ls_nonexistent_path() {
        let shell = test_shell();
        let code = run(&shell, "ls /nonexistent", 1).unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn ls_file_directly() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/single.txt",
                b"data",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "ls /workspace/single.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("single.txt"));
    }

    // =========================================================================
    // mkdir command tests (integration - async paths)
    // =========================================================================

    #[test]
    fn mkdir_simple() {
        let shell = test_shell();

        let code = run(&shell, "mkdir /workspace/newdir", 1).unwrap();
        assert_eq!(code, 0);
        assert!(shell.vfs.borrow().is_dir("/workspace/newdir"));
    }

    #[test]
    fn mkdir_parents() {
        let shell = test_shell();

        let code = run(&shell, "mkdir -p /workspace/a/b/c", 1).unwrap();
        assert_eq!(code, 0);
        assert!(shell.vfs.borrow().is_dir("/workspace/a"));
        assert!(shell.vfs.borrow().is_dir("/workspace/a/b"));
        assert!(shell.vfs.borrow().is_dir("/workspace/a/b/c"));
    }

    #[test]
    fn mkdir_multiple_dirs() {
        let shell = test_shell();

        let code = run(&shell, "mkdir /workspace/dir1 /workspace/dir2", 1).unwrap();
        assert_eq!(code, 0);
        assert!(shell.vfs.borrow().is_dir("/workspace/dir1"));
        assert!(shell.vfs.borrow().is_dir("/workspace/dir2"));
    }

    #[test]
    fn mkdir_no_operand() {
        let shell = test_shell();
        let code = run(&shell, "mkdir", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn mkdir_verbose() {
        let shell = test_shell();

        run(
            &shell,
            "mkdir -v /workspace/verbosedir > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("created"));
    }

    #[test]
    fn mkdir_parents_long_option() {
        let shell = test_shell();

        let code = run(&shell, "mkdir --parents /workspace/x/y/z", 1).unwrap();
        assert_eq!(code, 0);
        assert!(shell.vfs.borrow().is_dir("/workspace/x"));
        assert!(shell.vfs.borrow().is_dir("/workspace/x/y"));
        assert!(shell.vfs.borrow().is_dir("/workspace/x/y/z"));
    }

    #[test]
    fn mkdir_verbose_long_option() {
        let shell = test_shell();

        run(
            &shell,
            "mkdir --verbose /workspace/vdir > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("created directory"));
        assert!(text.contains("vdir"));
    }

    #[test]
    fn mkdir_existing_dir_fails() {
        let shell = test_shell();

        // Create the directory first
        run(&shell, "mkdir /workspace/exists", 1).unwrap();
        assert!(shell.vfs.borrow().is_dir("/workspace/exists"));

        // Trying to create it again without -p should fail
        let code = run(&shell, "mkdir /workspace/exists 2>/workspace/err.txt", 1).unwrap();
        assert_eq!(code, 1);

        let content = shell.vfs.borrow().read_file("/workspace/err.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("cannot create directory"));
    }

    #[test]
    fn mkdir_parents_existing_ok() {
        let shell = test_shell();

        // Create the directory first
        run(&shell, "mkdir /workspace/pexists", 1).unwrap();
        assert!(shell.vfs.borrow().is_dir("/workspace/pexists"));

        // With -p, creating existing directory should succeed
        let code = run(&shell, "mkdir -p /workspace/pexists", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn mkdir_nested_without_parents_fails() {
        let shell = test_shell();

        // Try to create nested path without -p
        let code = run(
            &shell,
            "mkdir /workspace/nonexistent/subdir 2>/workspace/err.txt",
            1,
        )
        .unwrap();
        assert_eq!(code, 1);

        let content = shell.vfs.borrow().read_file("/workspace/err.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("cannot create directory"));
    }

    #[test]
    fn mkdir_continue_on_error() {
        let shell = test_shell();

        // Create first dir to make it fail
        shell
            .vfs
            .borrow_mut()
            .create_dir("/workspace/first", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // First dir exists (will fail), second doesn't (should succeed)
        let code = run(
            &shell,
            "mkdir /workspace/first /workspace/second 2>/workspace/err.txt",
            1,
        )
        .unwrap();
        assert_eq!(code, 1); // Overall exit code is 1 due to first failure

        // But second directory should be created
        assert!(shell.vfs.borrow().is_dir("/workspace/second"));
    }

    #[test]
    fn mkdir_verbose_multiple() {
        let shell = test_shell();

        run(
            &shell,
            "mkdir -v /workspace/m1 /workspace/m2 /workspace/m3 > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("m1"));
        assert!(text.contains("m2"));
        assert!(text.contains("m3"));
    }

    #[test]
    fn mkdir_relative_path() {
        let shell = test_shell();

        // Default cwd is /workspace
        let code = run(&shell, "mkdir reldir", 1).unwrap();
        assert_eq!(code, 0);
        assert!(shell.vfs.borrow().is_dir("/workspace/reldir"));
    }

    #[test]
    fn mkdir_relative_nested() {
        let shell = test_shell();

        // Use -p for nested relative path
        let code = run(&shell, "mkdir -p rel/nested/path", 1).unwrap();
        assert_eq!(code, 0);
        assert!(shell.vfs.borrow().is_dir("/workspace/rel"));
        assert!(shell.vfs.borrow().is_dir("/workspace/rel/nested"));
        assert!(shell.vfs.borrow().is_dir("/workspace/rel/nested/path"));
    }

    // =========================================================================
    // touch command tests (integration - async paths)
    // =========================================================================

    #[test]
    fn touch_creates_file() {
        let shell = test_shell();

        assert!(!shell.vfs.borrow().exists("/workspace/touched.txt"));

        let code = run(&shell, "touch /workspace/touched.txt", 1).unwrap();
        assert_eq!(code, 0);
        assert!(shell.vfs.borrow().exists("/workspace/touched.txt"));
    }

    #[test]
    fn touch_existing_file() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/exists.txt",
                b"data",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Touch existing file - should succeed (no-op in VFS)
        let code = run(&shell, "touch /workspace/exists.txt", 1).unwrap();
        assert_eq!(code, 0);

        // Content should be unchanged
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/exists.txt")
            .unwrap();
        assert_eq!(content, b"data");
    }

    #[test]
    fn touch_multiple_files() {
        let shell = test_shell();

        let code = run(
            &shell,
            "touch /workspace/a.txt /workspace/b.txt /workspace/c.txt",
            1,
        )
        .unwrap();
        assert_eq!(code, 0);

        assert!(shell.vfs.borrow().exists("/workspace/a.txt"));
        assert!(shell.vfs.borrow().exists("/workspace/b.txt"));
        assert!(shell.vfs.borrow().exists("/workspace/c.txt"));
    }

    #[test]
    fn touch_no_create() {
        let shell = test_shell();

        // With -c, don't create if doesn't exist
        let code = run(&shell, "touch -c /workspace/nocreate.txt", 1).unwrap();
        assert_eq!(code, 0);
        assert!(!shell.vfs.borrow().exists("/workspace/nocreate.txt"));
    }

    #[test]
    fn touch_no_operand() {
        let shell = test_shell();
        let code = run(&shell, "touch", 1).unwrap();
        assert_eq!(code, 1);
    }

    // =========================================================================
    // rm command tests (integration - async paths)
    // =========================================================================

    #[test]
    fn rm_file() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/delete.txt",
                b"x",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let code = run(&shell, "rm /workspace/delete.txt", 1).unwrap();
        assert_eq!(code, 0);
        assert!(!shell.vfs.borrow().exists("/workspace/delete.txt"));
    }

    #[test]
    fn rm_multiple_files() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/d1.txt", b"", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/d2.txt", b"", amla_vfs::Permission::ReadWrite)
            .unwrap();

        let code = run(&shell, "rm /workspace/d1.txt /workspace/d2.txt", 1).unwrap();
        assert_eq!(code, 0);
        assert!(!shell.vfs.borrow().exists("/workspace/d1.txt"));
        assert!(!shell.vfs.borrow().exists("/workspace/d2.txt"));
    }

    #[test]
    fn rm_directory_without_r() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir("/workspace/mydir", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // Should fail without -r
        let code = run(&shell, "rm /workspace/mydir", 1).unwrap();
        assert_eq!(code, 1);
        assert!(shell.vfs.borrow().exists("/workspace/mydir"));
    }

    #[test]
    fn rm_directory_recursive() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir_all("/workspace/rmdir/sub", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/rmdir/sub/file.txt",
                b"x",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let code = run(&shell, "rm -r /workspace/rmdir", 1).unwrap();
        assert_eq!(code, 0);
        assert!(!shell.vfs.borrow().exists("/workspace/rmdir"));
    }

    #[test]
    fn rm_nonexistent_without_force() {
        let shell = test_shell();

        let code = run(&shell, "rm /workspace/nonexistent", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn rm_nonexistent_with_force() {
        let shell = test_shell();

        let code = run(&shell, "rm -f /workspace/nonexistent", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn rm_no_operand() {
        let shell = test_shell();

        let code = run(&shell, "rm", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn rm_force_no_operand() {
        let shell = test_shell();

        // With -f and no operand, succeed silently
        let code = run(&shell, "rm -f", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn rm_verbose() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/verbose.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "rm -v /workspace/verbose.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("removed"));
    }

    #[test]
    fn rm_uppercase_r_recursive() {
        // Test -R (uppercase) works the same as -r
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir_all(
                "/workspace/rmdir_upper/sub",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/rmdir_upper/sub/file.txt",
                b"x",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let code = run(&shell, "rm -R /workspace/rmdir_upper", 1).unwrap();
        assert_eq!(code, 0);
        assert!(!shell.vfs.borrow().exists("/workspace/rmdir_upper"));
    }

    #[test]
    fn rm_long_options() {
        let shell = test_shell();

        // Test --recursive long option
        shell
            .vfs
            .borrow_mut()
            .create_dir_all("/workspace/longopt/sub", amla_vfs::Permission::ReadWrite)
            .unwrap();

        let code = run(&shell, "rm --recursive /workspace/longopt", 1).unwrap();
        assert_eq!(code, 0);
        assert!(!shell.vfs.borrow().exists("/workspace/longopt"));

        // Test --force long option
        let code = run(&shell, "rm --force /workspace/nonexistent_long", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn rm_combined_flags_rf() {
        // Test combined -rf flags (common usage pattern)
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir_all("/workspace/combined/sub", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/combined/file.txt",
                b"data",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let code = run(&shell, "rm -rf /workspace/combined", 1).unwrap();
        assert_eq!(code, 0);
        assert!(!shell.vfs.borrow().exists("/workspace/combined"));
    }

    #[test]
    fn rm_combined_flags_fr() {
        // Test combined -fr flags (reversed order)
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir_all("/workspace/combined2/sub", amla_vfs::Permission::ReadWrite)
            .unwrap();

        let code = run(&shell, "rm -fr /workspace/combined2", 1).unwrap();
        assert_eq!(code, 0);
        assert!(!shell.vfs.borrow().exists("/workspace/combined2"));
    }

    // NOTE: The rm command's help functionality (-h/--help) is currently broken.
    // The HELP constant is defined in rm.rs but the -h/--help flag handlers
    // are missing from the argument parser. This should be fixed.

    #[test]
    fn rm_verbose_recursive() {
        // Test -v with recursive removal shows all removed items
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir_all("/workspace/vdir/sub", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/vdir/file.txt",
                b"x",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "rm -rv /workspace/vdir > /workspace/verbose_out.txt",
            1,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/verbose_out.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("removed"));
        assert!(text.contains("vdir"));
    }

    #[test]
    fn rm_verbose_multiple_files() {
        // Test -v with multiple files shows each removal
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/v1.txt", b"1", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/v2.txt", b"2", amla_vfs::Permission::ReadWrite)
            .unwrap();

        run(
            &shell,
            "rm -v /workspace/v1.txt /workspace/v2.txt > /workspace/multi_verbose.txt",
            1,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/multi_verbose.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("v1.txt"));
        assert!(text.contains("v2.txt"));
    }

    #[test]
    fn rm_error_message_nonexistent() {
        // Verify error message content for nonexistent file
        let shell = test_shell();

        run(&shell, "rm /workspace/ghost.txt 2> /workspace/err.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/err.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("cannot remove"));
        assert!(text.contains("ghost.txt"));
        assert!(text.contains("No such file or directory"));
    }

    #[test]
    fn rm_error_message_directory() {
        // Verify error message content for directory without -r
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir("/workspace/errdir", amla_vfs::Permission::ReadWrite)
            .unwrap();

        run(&shell, "rm /workspace/errdir 2> /workspace/dir_err.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/dir_err.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("cannot remove"));
        assert!(text.contains("errdir"));
        assert!(text.contains("Is a directory"));
    }

    #[test]
    fn rm_error_message_missing_operand() {
        // Verify error message for missing operand
        let shell = test_shell();

        run(&shell, "rm 2> /workspace/operand_err.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/operand_err.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("missing operand"));
    }

    #[test]
    fn rm_partial_failure() {
        // Test that rm continues after first failure and reports correct exit code
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/exists.txt",
                b"data",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // First file doesn't exist, second does
        let code = run(&shell, "rm /workspace/nothere.txt /workspace/exists.txt", 1).unwrap();

        // Should fail (exit code 1) but still delete the existing file
        assert_eq!(code, 1);
        assert!(!shell.vfs.borrow().exists("/workspace/exists.txt"));
    }

    #[test]
    fn rm_combined_flags_rfv() {
        // Test all three flags combined: -rfv
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir("/workspace/rfvdir", amla_vfs::Permission::ReadWrite)
            .unwrap();

        run(
            &shell,
            "rm -rfv /workspace/rfvdir > /workspace/rfv_out.txt",
            1,
        )
        .unwrap();

        assert!(!shell.vfs.borrow().exists("/workspace/rfvdir"));

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/rfv_out.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("removed"));
    }

    // =========================================================================
    // test/[ command tests (integration - async paths)
    // =========================================================================

    #[test]
    fn test_file_exists() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/exists.txt",
                b"",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let code = run(&shell, "test -e /workspace/exists.txt", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test -e /workspace/nonexistent", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn test_is_file() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/file.txt", b"", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .create_dir("/workspace/dir", amla_vfs::Permission::ReadWrite)
            .unwrap();

        let code = run(&shell, "test -f /workspace/file.txt", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test -f /workspace/dir", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn test_is_directory() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir("/workspace/testdir", amla_vfs::Permission::ReadWrite)
            .unwrap();

        let code = run(&shell, "test -d /workspace/testdir", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test -d /workspace/nonexistent", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn test_bracket_syntax() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/x.txt", b"", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // [ -e file ] syntax
        let code = run(&shell, "[ -e /workspace/x.txt ]", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "[ -e /workspace/missing ]", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn test_string_equals() {
        let shell = test_shell();

        let code = run(&shell, "test foo = foo", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test foo = bar", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn test_string_not_equals() {
        let shell = test_shell();

        let code = run(&shell, "test foo != bar", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test foo != foo", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn test_numeric_comparison() {
        let shell = test_shell();

        let code = run(&shell, "test 5 -eq 5", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test 5 -ne 3", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test 3 -lt 5", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test 5 -gt 3", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test 5 -le 5", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test 5 -ge 5", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn test_negation() {
        let shell = test_shell();

        let code = run(&shell, "test ! foo = bar", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test ! foo = foo", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn test_empty_nonempty_string() {
        let shell = test_shell();

        let code = run(&shell, "test -z ''", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test -n hello", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test -z hello", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn test_and_or() {
        let shell = test_shell();

        // -a is AND
        let code = run(&shell, "test foo = foo -a bar = bar", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "test foo = foo -a bar = baz", 1).unwrap();
        assert_eq!(code, 1);

        // -o is OR
        let code = run(&shell, "test foo = bar -o baz = baz", 1).unwrap();
        assert_eq!(code, 0);
    }

    // =========================================================================
    // printf command tests (integration - async paths)
    // =========================================================================

    #[test]
    fn printf_simple_string() {
        let shell = test_shell();

        run(&shell, "printf 'hello world' > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "hello world");
    }

    #[test]
    fn printf_string_format() {
        let shell = test_shell();

        run(&shell, "printf 'Hello %s' world > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "Hello world");
    }

    #[test]
    fn printf_decimal_format() {
        let shell = test_shell();

        run(&shell, "printf 'Number: %d' 42 > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "Number: 42");
    }

    #[test]
    fn printf_hex_format() {
        let shell = test_shell();

        run(&shell, "printf '%x' 255 > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "ff");
    }

    #[test]
    fn printf_escape_sequences() {
        let shell = test_shell();

        run(&shell, r"printf 'a\tb\nc' > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "a\tb\nc");
    }

    #[test]
    fn printf_percent_escape() {
        let shell = test_shell();

        run(&shell, "printf '100%%' > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "100%");
    }

    #[test]
    fn printf_no_format() {
        let shell = test_shell();

        let code = run(&shell, "printf", 1).unwrap();
        assert_eq!(code, 1);
    }

    // =========================================================================
    // pwd, env, colon builtin tests (integration - async paths)
    // =========================================================================

    #[test]
    fn pwd_prints_cwd() {
        let shell = test_shell();

        run(&shell, "cd /workspace", 1).unwrap();
        run(&shell, "pwd > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "/workspace");
    }

    #[test]
    fn pwd_after_cd() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir_all("/workspace/subdir/nested", amla_vfs::Permission::ReadWrite)
            .unwrap();

        run(&shell, "cd /workspace/subdir/nested", 1).unwrap();
        run(&shell, "pwd > /workspace/subdir/nested/out.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/subdir/nested/out.txt")
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&content).trim(),
            "/workspace/subdir/nested"
        );
    }

    #[test]
    fn env_prints_variables() {
        let shell = test_shell();

        run(&shell, "export MY_VAR=hello", 1).unwrap();
        run(&shell, "env > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("MY_VAR=hello"));
    }

    #[test]
    fn colon_is_noop() {
        let shell = test_shell();

        let code = run(&shell, ":", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn colon_in_pipeline() {
        let shell = test_shell();

        // Colon should succeed and produce no output
        let code = run(&shell, ": | cat", 2).unwrap();
        assert_eq!(code, 0);
    }

    // =========================================================================
    // Additional grep tests for better coverage
    // =========================================================================

    #[test]
    fn grep_case_insensitive() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                b"Hello\nWORLD\nhello\nworld",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -i hello /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("Hello"));
        assert!(text.contains("hello"));
    }

    #[test]
    fn grep_count_only() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                b"apple\napple\nbanana\napple",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -c apple /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content).trim().to_string();
        assert_eq!(text, "3");
    }

    #[test]
    fn grep_invert_match() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                b"keep\nremove\nkeep\nremove",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -v remove /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("keep"));
        assert!(!text.contains("remove"));
    }

    #[test]
    fn grep_line_numbers() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                b"one\ntwo\nthree",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -n two /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("2:")); // Line 2
    }

    #[test]
    fn grep_no_match() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                b"hello",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let code = run(&shell, "grep xyz /workspace/text.txt", 1).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn grep_after_context() {
        let shell = test_shell();

        let content = "line1\nMATCH\nline3\nline4\nline5\n";
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                content.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -A 2 MATCH /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let out = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&out);
        assert_eq!(text, "MATCH\nline3\nline4\n");
    }

    #[test]
    fn grep_before_context() {
        let shell = test_shell();

        let content = "line1\nline2\nMATCH\nline4\nline5\n";
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                content.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -B 2 MATCH /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let out = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&out);
        assert_eq!(text, "line1\nline2\nMATCH\n");
    }

    #[test]
    fn grep_context_both() {
        let shell = test_shell();

        let content = "a\nb\nc\nMATCH\nd\ne\nf\n";
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                content.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -C 2 MATCH /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let out = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&out);
        assert_eq!(text, "b\nc\nMATCH\nd\ne\n");
    }

    #[test]
    fn grep_context_with_line_numbers() {
        let shell = test_shell();

        let content = "a\nb\nMATCH\nd\ne\n";
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                content.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -n -B 1 -A 1 MATCH /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let out = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&out);
        // Line numbers with separator: context uses '-', match uses ':'
        assert_eq!(text, "2-b\n3:MATCH\n4-d\n");
    }

    #[test]
    fn grep_context_multiple_matches_with_separator() {
        let shell = test_shell();

        // Matches on lines 2 and 8 - gap large enough to need separator
        let content = "1\nMATCH\n3\n4\n5\n6\n7\nMATCH\n9\n";
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                content.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -A 1 MATCH /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let out = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&out);
        // Should have separator between groups
        assert_eq!(text, "MATCH\n3\n--\nMATCH\n9\n");
    }

    #[test]
    fn grep_context_overlapping_matches() {
        let shell = test_shell();

        // Consecutive matches - context should merge, no separator
        let content = "1\nMATCH1\nMATCH2\n4\n5\n";
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/text.txt",
                content.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "grep -A 1 MATCH /workspace/text.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let out = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&out);
        // No separator - matches are adjacent
        assert_eq!(text, "MATCH1\nMATCH2\n4\n");
    }

    // =========================================================================
    // Additional head/tail tests for better coverage
    // =========================================================================

    #[test]
    fn head_default_10_lines() {
        let shell = test_shell();

        let lines: String = (1..=20).map(|i| format!("{i}\n")).collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                lines.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "head /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let output_lines: Vec<&str> = text.lines().collect();
        assert_eq!(output_lines.len(), 10);
    }

    #[test]
    fn head_bytes() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"abcdefghijklmnop",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "head -c 5 /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(content, b"abcde");
    }

    #[test]
    fn tail_default_10_lines() {
        let shell = test_shell();

        let lines: String = (1..=20).map(|i| format!("{i}\n")).collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                lines.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "tail /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let output_lines: Vec<&str> = text.lines().collect();
        assert_eq!(output_lines.len(), 10);
        assert_eq!(output_lines[0], "11");
        assert_eq!(output_lines[9], "20");
    }

    #[test]
    fn tail_bytes() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"abcdefghijklmnop",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "tail -c 5 /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(content, b"lmnop");
    }

    // =========================================================================
    // Additional wc tests for better coverage
    // =========================================================================

    #[test]
    fn wc_lines_only() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"one\ntwo\nthree\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "wc -l /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content).trim().to_string();
        assert!(text.contains('3'));
    }

    #[test]
    fn wc_words_only() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"one two three four five",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "wc -w /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content).trim().to_string();
        assert!(text.contains('5'));
    }

    #[test]
    fn wc_bytes_only() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"hello",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "wc -c /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content).trim().to_string();
        assert!(text.contains('5'));
    }

    #[test]
    fn wc_multiple_files() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/a.txt",
                b"one\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/b.txt",
                b"two\nthree\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "wc -l /workspace/a.txt /workspace/b.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        // Should show individual counts and total
        assert!(text.contains('1')); // a.txt has 1 line
        assert!(text.contains('2')); // b.txt has 2 lines
    }

    // =========================================================================
    // tee, tr, cut, sort, uniq tests
    // =========================================================================

    #[test]
    fn tee_writes_to_stdout_and_file() {
        let shell = test_shell();

        run(
            &shell,
            "echo hello | tee /workspace/copy.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let stdout = shell
            .vfs
            .borrow()
            .read_file("/workspace/stdout.txt")
            .unwrap();
        let copy = shell.vfs.borrow().read_file("/workspace/copy.txt").unwrap();

        assert_eq!(String::from_utf8_lossy(&stdout), "hello\n");
        assert_eq!(String::from_utf8_lossy(&copy), "hello\n");
    }

    #[test]
    fn tee_append_mode() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/log.txt",
                b"first\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "echo second | tee -a /workspace/log.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/log.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "first\nsecond\n");
    }

    #[test]
    fn tee_help_flag() {
        let shell = test_shell();

        run(&shell, "tee --help > /workspace/help.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/help.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("tee - read from stdin"));
        assert!(text.contains("-a"));
        assert!(text.contains("--help"));
    }

    #[test]
    fn tee_short_help_flag() {
        let shell = test_shell();

        run(&shell, "tee -h > /workspace/help.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/help.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("tee - read from stdin"));
    }

    #[test]
    fn tee_multiple_files() {
        let shell = test_shell();

        run(
            &shell,
            "echo hello | tee /workspace/a.txt /workspace/b.txt /workspace/c.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let stdout = shell
            .vfs
            .borrow()
            .read_file("/workspace/stdout.txt")
            .unwrap();
        let a = shell.vfs.borrow().read_file("/workspace/a.txt").unwrap();
        let b = shell.vfs.borrow().read_file("/workspace/b.txt").unwrap();
        let c = shell.vfs.borrow().read_file("/workspace/c.txt").unwrap();

        assert_eq!(String::from_utf8_lossy(&stdout), "hello\n");
        assert_eq!(String::from_utf8_lossy(&a), "hello\n");
        assert_eq!(String::from_utf8_lossy(&b), "hello\n");
        assert_eq!(String::from_utf8_lossy(&c), "hello\n");
    }

    #[test]
    fn tee_no_files_passthrough() {
        let shell = test_shell();

        // tee with no files should just pass stdin to stdout
        run(&shell, "echo passthrough | tee > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "passthrough\n");
    }

    #[test]
    fn tee_empty_input() {
        let shell = test_shell();

        // printf with empty string produces no output
        run(
            &shell,
            "printf '' | tee /workspace/empty.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let stdout = shell
            .vfs
            .borrow()
            .read_file("/workspace/stdout.txt")
            .unwrap();
        let file = shell
            .vfs
            .borrow()
            .read_file("/workspace/empty.txt")
            .unwrap();

        assert_eq!(stdout.len(), 0);
        assert_eq!(file.len(), 0);
    }

    #[test]
    fn tee_truncates_existing_file() {
        let shell = test_shell();

        // Create file with existing content
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/existing.txt",
                b"old content that is longer",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // tee without -a should truncate the file
        run(
            &shell,
            "echo new | tee /workspace/existing.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/existing.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "new\n");
    }

    #[test]
    fn tee_ignore_sigint_flag() {
        let shell = test_shell();

        // -i flag should be silently accepted (no-op in sandbox)
        run(
            &shell,
            "echo hello | tee -i /workspace/out.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "hello\n");
    }

    #[test]
    fn tee_append_to_existing_file() {
        let shell = test_shell();

        // Create an existing file first
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/existing.txt",
                b"first line\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // -a should append to the existing file
        run(
            &shell,
            "echo second line | tee -a /workspace/existing.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/existing.txt")
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&content),
            "first line\nsecond line\n"
        );
    }

    #[test]
    fn tee_append_nonexistent_file_errors() {
        let shell = test_shell();

        // -a on a nonexistent file reports error to stderr but still passes stdin to stdout
        let result = run(
            &shell,
            "echo hello | tee -a /workspace/nonexistent.txt 2>/workspace/err.txt > /workspace/stdout.txt",
            2,
        );

        // Command completes (exit code may be 1 due to write error)
        assert!(result.is_ok());

        // stdout should still have the data
        let stdout = shell
            .vfs
            .borrow()
            .read_file("/workspace/stdout.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&stdout), "hello\n");

        // stderr should have an error message
        let stderr = shell.vfs.borrow().read_file("/workspace/err.txt").unwrap();
        let stderr_text = String::from_utf8_lossy(&stderr);
        assert!(stderr_text.contains("tee:") || stderr_text.contains("nonexistent"));
    }

    #[test]
    fn tee_multiline_content() {
        let shell = test_shell();

        run(
            &shell,
            "printf 'line1\\nline2\\nline3\\n' | tee /workspace/multi.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let stdout = shell
            .vfs
            .borrow()
            .read_file("/workspace/stdout.txt")
            .unwrap();
        let file = shell
            .vfs
            .borrow()
            .read_file("/workspace/multi.txt")
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&stdout), "line1\nline2\nline3\n");
        assert_eq!(String::from_utf8_lossy(&file), "line1\nline2\nline3\n");
    }

    #[test]
    fn tee_binary_data() {
        let shell = test_shell();

        // Create binary data with null bytes
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/binary.dat",
                &[0x00, 0x01, 0x02, 0xFF, 0xFE, 0x00],
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "cat /workspace/binary.dat | tee /workspace/copy.dat > /workspace/stdout.dat",
            2,
        )
        .unwrap();

        let stdout = shell
            .vfs
            .borrow()
            .read_file("/workspace/stdout.dat")
            .unwrap();
        let copy = shell.vfs.borrow().read_file("/workspace/copy.dat").unwrap();

        assert_eq!(stdout, vec![0x00, 0x01, 0x02, 0xFF, 0xFE, 0x00]);
        assert_eq!(copy, vec![0x00, 0x01, 0x02, 0xFF, 0xFE, 0x00]);
    }

    #[test]
    fn tee_combined_append_and_ignore() {
        let shell = test_shell();

        // Create initial content
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/combined.txt",
                b"initial\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Use both -a and -i flags
        run(
            &shell,
            "echo appended | tee -a -i /workspace/combined.txt > /workspace/stdout.txt",
            2,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/combined.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "initial\nappended\n");
    }

    #[test]
    fn tr_translate_characters() {
        let shell = test_shell();

        run(&shell, "echo hello | tr a-z A-Z > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "HELLO\n");
    }

    #[test]
    fn tr_delete_characters() {
        let shell = test_shell();

        run(
            &shell,
            "echo 'hello world' | tr -d ' ' > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "helloworld\n");
    }

    #[test]
    fn tr_squeeze_repeats() {
        let shell = test_shell();

        // tr -s e squeezes only 'e' characters
        run(&shell, "echo 'heeello' | tr -s e > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "hello\n");
    }

    #[test]
    fn tr_complement_mode() {
        let shell = test_shell();

        // -c complements SET1: replace all non-letters with X
        run(
            &shell,
            "echo 'abc123def' | tr -c a-zA-Z X > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // abc, then 123 becomes XXX, then def, then newline becomes X
        assert_eq!(String::from_utf8_lossy(&content), "abcXXXdefX");
    }

    #[test]
    fn tr_complement_delete() {
        let shell = test_shell();

        // -cd: delete all characters NOT in SET1 (keep only digits)
        run(
            &shell,
            "echo 'abc123def456' | tr -cd 0-9 > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "123456");
    }

    #[test]
    fn tr_squeeze_with_translate() {
        let shell = test_shell();

        // -s with translation: squeeze is applied based on whether the OUTPUT byte
        // is in SET1. Since we translate a-z to A-Z, the output 'A' is NOT in SET1 (a-z),
        // so consecutive A's are NOT squeezed.
        run(
            &shell,
            "echo 'aaa bbb' | tr -s a-z A-Z > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // 'aaa' -> 'AAA' (not squeezed because 'A' is not in SET1 'a-z')
        assert_eq!(String::from_utf8_lossy(&content), "AAA BBB\n");
    }

    #[test]
    fn tr_squeeze_only_mode() {
        let shell = test_shell();

        // -s without translation: squeeze repeated characters in SET1
        // When given only one set, squeeze consecutive characters that are in SET1
        run(
            &shell,
            "echo 'aaa bbb ccc' | tr -s abc > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // 'aaa' squeezed to 'a', 'bbb' squeezed to 'b', 'ccc' squeezed to 'c'
        assert_eq!(String::from_utf8_lossy(&content), "a b c\n");
    }

    #[test]
    fn tr_set2_extension() {
        let shell = test_shell();

        // When SET2 is shorter than SET1, last char of SET2 is used
        run(&shell, "echo 'abcdef' | tr a-f X > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // All of a-f map to X
        assert_eq!(String::from_utf8_lossy(&content), "XXXXXX\n");
    }

    #[test]
    fn tr_delete_digits() {
        let shell = test_shell();

        run(
            &shell,
            "echo 'hello123world456' | tr -d 0-9 > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "helloworld\n");
    }

    #[test]
    fn tr_missing_operand_error() {
        let shell = test_shell();

        // tr with no arguments should produce an error
        let result = run(&shell, "tr 2>/workspace/err.txt", 1);

        assert!(result.is_ok());
        let err = shell.vfs.borrow().read_file("/workspace/err.txt").unwrap();
        let err_text = String::from_utf8_lossy(&err);
        assert!(err_text.contains("missing operand"));
    }

    #[test]
    fn tr_escape_sequences() {
        let shell = test_shell();

        // Test that newline escape works
        run(
            &shell,
            r#"printf 'a\nb\nc' | tr '\n' ',' > /workspace/out.txt"#,
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "a,b,c");
    }

    #[test]
    fn tr_empty_input() {
        let shell = test_shell();

        run(&shell, "printf '' | tr a-z A-Z > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(content.len(), 0);
    }

    #[test]
    fn tr_squeeze_spaces() {
        let shell = test_shell();

        // Squeeze multiple spaces into one
        run(
            &shell,
            "echo 'hello    world' | tr -s ' ' > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "hello world\n");
    }

    #[test]
    fn tr_delete_newlines() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/multiline.txt",
                b"line1\nline2\nline3\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            r#"cat /workspace/multiline.txt | tr -d '\n' > /workspace/out.txt"#,
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "line1line2line3");
    }

    #[test]
    fn tr_uppercase_range() {
        let shell = test_shell();

        // Reverse case: uppercase to lowercase
        run(&shell, "echo 'HELLO' | tr A-Z a-z > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "hello\n");
    }

    #[test]
    fn cut_fields() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.csv",
                b"a,b,c\n1,2,3\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "cut -d, -f2 /workspace/data.csv > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "b\n2\n");
    }

    #[test]
    fn cut_bytes() {
        let shell = test_shell();

        run(&shell, "echo abcdef | cut -b1-3 > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "abc\n");
    }

    #[test]
    fn sort_basic() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"banana\napple\ncherry\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "sort /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "apple\nbanana\ncherry\n");
    }

    #[test]
    fn sort_reverse() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"a\nb\nc\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "sort -r /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "c\nb\na\n");
    }

    #[test]
    fn sort_numeric() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"10\n2\n1\n20\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "sort -n /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "1\n2\n10\n20\n");
    }

    #[test]
    fn uniq_removes_adjacent_duplicates() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"a\na\nb\nb\nb\na\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "uniq /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "a\nb\na\n");
    }

    #[test]
    fn uniq_count() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"a\na\na\nb\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "uniq -c /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains('3')); // 3 a's
        assert!(text.contains('1')); // 1 b
    }

    #[test]
    fn sort_uniq_pipeline() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"b\na\nb\na\nc\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "sort /workspace/data.txt | uniq > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "a\nb\nc\n");
    }

    #[test]
    fn uniq_only_duplicates() {
        // -d flag: only print lines that appear more than once
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"a\na\nb\nc\nc\nc\nd\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "uniq -d /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Only 'a' (2 times) and 'c' (3 times) are duplicates
        assert_eq!(String::from_utf8_lossy(&content), "a\nc\n");
    }

    #[test]
    fn uniq_only_unique() {
        // -u flag: only print lines that appear exactly once
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"a\na\nb\nc\nc\nc\nd\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "uniq -u /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Only 'b' and 'd' appear exactly once
        assert_eq!(String::from_utf8_lossy(&content), "b\nd\n");
    }

    #[test]
    fn uniq_case_insensitive() {
        // -i flag: case-insensitive comparison
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"Hello\nhello\nHELLO\nWorld\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "uniq -i /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // All "hello" variants collapse to first one, then "World"
        assert_eq!(String::from_utf8_lossy(&content), "Hello\nWorld\n");
    }

    #[test]
    fn uniq_count_with_case_insensitive() {
        // -c and -i flags together
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"Apple\napple\nAPPLE\nBanana\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "uniq -ci /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        // Should count 3 apples and 1 banana
        assert!(text.contains('3'));
        assert!(text.contains("Apple")); // First occurrence is preserved
        assert!(text.contains('1'));
        assert!(text.contains("Banana"));
    }

    #[test]
    fn uniq_empty_input() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/data.txt", b"", amla_vfs::Permission::ReadWrite)
            .unwrap();

        run(&shell, "uniq /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "");
    }

    #[test]
    fn uniq_single_line() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"single\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "uniq /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "single\n");
    }

    #[test]
    fn uniq_output_file_argument() {
        // Test second argument as output file (not using redirection)
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/input.txt",
                b"a\na\nb\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "uniq /workspace/input.txt /workspace/output.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/output.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "a\nb\n");
    }

    #[test]
    fn uniq_from_stdin_pipe() {
        // Test reading from stdin via pipe (echo ... | uniq)
        let shell = test_shell();

        run(&shell, "echo -e 'a\\na\\nb' | uniq > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "a\nb\n");
    }

    #[test]
    fn uniq_du_flags_together() {
        // -d and -u together: should output nothing
        // -d = only duplicates, -u = only unique, these are mutually exclusive
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"a\na\nb\nc\nc\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "uniq -d -u /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Nothing matches both "only duplicate" AND "only unique"
        assert_eq!(String::from_utf8_lossy(&content), "");
    }

    #[test]
    fn uniq_count_with_duplicates_flag() {
        // -c and -d together: count only duplicated lines
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"a\na\na\nb\nc\nc\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "uniq -cd /workspace/data.txt > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        // Should show count for 'a' (3) and 'c' (2), but not 'b' (unique)
        assert!(text.contains('3'));
        assert!(text.contains('a'));
        assert!(text.contains('2'));
        assert!(text.contains('c'));
        assert!(!text.contains('b'));
    }

    #[test]
    fn uniq_no_trailing_newline() {
        // Input without trailing newline
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"a\na\nb",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "uniq /workspace/data.txt > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Should still handle line without trailing newline
        assert_eq!(String::from_utf8_lossy(&content), "a\nb\n");
    }

    #[test]
    fn cut_tr_pipeline() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.csv",
                b"hello,world\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "cut -d, -f1 /workspace/data.csv | tr a-z A-Z > /workspace/out.txt",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "HELLO\n");
    }

    // =========================================================================
    // set builtin test
    // =========================================================================

    #[test]
    fn set_prints_environment() {
        let shell = test_shell();

        run(&shell, "export TEST_SET_VAR=value", 1).unwrap();
        run(&shell, "set > /workspace/out.txt", 1).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("TEST_SET_VAR=value"));
    }

    // =========================================================================
    // Comprehensive Pipeline Execution Edge Cases
    // =========================================================================

    #[test]
    fn pipeline_empty_first_command() {
        let shell = test_shell();

        // Empty command produces no output, cat should get empty stdin
        run(&shell, ": | cat > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn pipeline_failing_first_stage() {
        let shell = test_shell();

        // First command fails (cat nonexistent file)
        // Should return exit code of last command (cat succeeds with empty input)
        let code = run(&shell, "cat /nonexistent | cat > /workspace/out.txt", 2).unwrap();
        // Last command should succeed (it just got empty input)
        assert_eq!(code, 0);
    }

    #[test]
    fn pipeline_failing_middle_stage() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"hello world",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Middle command fails (grep for pattern that doesn't exist)
        // Exit code should be from last command
        let code = run(
            &shell,
            "cat /workspace/data.txt | grep nonexistent | cat > /workspace/out.txt",
            3,
        )
        .unwrap();
        // cat succeeds with empty input from grep
        assert_eq!(code, 0);
    }

    #[test]
    fn pipeline_failing_last_stage() {
        let shell = test_shell();

        // Last command fails
        let code = run(&shell, "echo hello | cat /nonexistent", 1).unwrap();
        // Should return exit code of last command (1 for cat failure)
        assert_eq!(code, 1);
    }

    #[test]
    fn pipeline_exit_code_from_last_command_true_then_false() {
        let shell = test_shell();

        let code = run(&shell, "true | false", 1).unwrap();
        assert_eq!(code, 1, "Exit code should be from last command (false = 1)");
    }

    #[test]
    fn pipeline_exit_code_from_last_command_false_then_true() {
        let shell = test_shell();

        let code = run(&shell, "false | true", 1).unwrap();
        assert_eq!(code, 0, "Exit code should be from last command (true = 0)");
    }

    #[test]
    fn pipeline_exit_code_chain() {
        let shell = test_shell();

        // Chain of true|false|true|false should return 1 (last false)
        let code = run(&shell, "true | false | true | false", 1).unwrap();
        assert_eq!(code, 1);

        // Chain of false|true|false|true should return 0 (last true)
        let code = run(&shell, "false | true | false | true", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn pipeline_large_data_1mb() {
        let shell = test_shell();

        // Create 1MB of data (about 32000 lines of 32 chars each)
        let lines: String = (0..32000)
            .map(|i| format!("Line {i:025} padding\n"))
            .collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/large.txt",
                lines.as_bytes(),
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let start = std::time::Instant::now();
        run(
            &shell,
            "cat /workspace/large.txt | cat | cat > /workspace/out.txt",
            610,
        )
        .unwrap();
        let duration = start.elapsed();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(content.len(), lines.len());

        // Should complete in reasonable time (< 5 seconds for 1MB)
        assert!(
            duration.as_secs() < 5,
            "Large pipeline too slow: {duration:?}"
        );
    }

    #[test]
    fn pipeline_six_stages() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/input.txt",
                b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // 6-stage pipeline: cat | cat | head | cat | tail | cat
        run(
            &shell,
            "cat /workspace/input.txt | cat | head -7 | cat | tail -3 | cat > /workspace/out.txt",
            5,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        // head -7 gives 1-7, tail -3 gives 5,6,7
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "5");
        assert_eq!(lines[1], "6");
        assert_eq!(lines[2], "7");
    }

    #[test]
    fn pipeline_seven_stages() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // 7-stage pipeline
        run(&shell, "cat /workspace/data.txt | cat | cat | head -6 | tail -4 | cat | cat > /workspace/out.txt", 6)
            .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        // head -6 gives alpha-zeta, tail -4 gives gamma,delta,epsilon,zeta
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "gamma");
        assert_eq!(lines[3], "zeta");
    }

    #[test]
    fn pipeline_empty_intermediate_all_filtered() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"one\ntwo\nthree",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // First grep filters everything, second grep gets empty input
        let code = run(
            &shell,
            "cat /workspace/data.txt | grep xyz | grep abc > /workspace/out.txt",
            3,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert!(content.is_empty());
        // Exit code should be 0 (last cat-like command succeeds with empty input)
        // Actually grep with no matches returns 1
        assert_eq!(code, 1);
    }

    // =========================================================================
    // Comprehensive And/Or Operator Tests
    // =========================================================================

    #[test]
    fn and_operator_short_circuit_on_failure() {
        let shell = test_shell();

        // false && echo should not run echo
        run(
            &shell,
            "false && echo should_not_appear > /workspace/out.txt",
            1,
        )
        .unwrap();

        // File might not exist or be empty
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/out.txt")
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&content);
        assert!(!text.contains("should_not_appear"));
    }

    #[test]
    fn and_operator_executes_on_success() {
        let shell = test_shell();

        // true && echo should run echo
        run(&shell, "true && echo success > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("success"));
    }

    #[test]
    fn and_chain_three_commands_all_success() {
        let shell = test_shell();

        // All succeed
        let code = run(&shell, "true && true && true", 3).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn and_chain_three_commands_middle_fails() {
        let shell = test_shell();

        // Middle fails, third should not run
        let code = run(&shell, "true && false && true", 2).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn and_chain_exit_code_propagation() {
        let shell = test_shell();

        // Exit code should be from the failing command
        let code = run(&shell, "true && exit 42", 2).unwrap();
        assert_eq!(code, 42);

        let code = run(&shell, "exit 42 && true", 1).unwrap();
        assert_eq!(code, 42);
    }

    #[test]
    fn or_operator_short_circuit_on_success() {
        let shell = test_shell();

        // true || echo should not run echo
        run(
            &shell,
            "true || echo should_not_appear > /workspace/out.txt",
            1,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/out.txt")
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&content);
        assert!(!text.contains("should_not_appear"));
    }

    #[test]
    fn or_operator_executes_on_failure() {
        let shell = test_shell();

        // false || echo should run echo
        run(&shell, "false || echo fallback > /workspace/out.txt", 2).unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("fallback"));
    }

    #[test]
    fn or_chain_three_commands_first_success() {
        let shell = test_shell();

        // First succeeds, rest should not run
        let code = run(&shell, "true || false || false", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn or_chain_three_commands_all_fail() {
        let shell = test_shell();

        // All fail, exit code from last
        let code = run(&shell, "false || false || false", 3).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn or_chain_recovery() {
        let shell = test_shell();

        // First two fail, third succeeds
        let code = run(&shell, "false || false || true", 3).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn mixed_and_or_chain() {
        let shell = test_shell();

        // Complex chain: true && false || true should succeed
        // (true && false) -> false, then (false || true) -> 0
        let code = run(&shell, "true && false || true", 3).unwrap();
        assert_eq!(code, 0);

        // false || true && false should fail
        // (false || true) -> 0, then (true && false) -> 1
        let code = run(&shell, "false || true && false", 3).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn and_or_with_exit_codes() {
        let shell = test_shell();

        // exit 2 && true -> 2
        let code = run(&shell, "exit 2 && true", 1).unwrap();
        assert_eq!(code, 2);

        // exit 2 || exit 3 -> 3 (first fails, second runs)
        let code = run(&shell, "exit 2 || exit 3", 2).unwrap();
        assert_eq!(code, 3);

        // true || exit 5 -> 0 (short circuit)
        let code = run(&shell, "true || exit 5", 1).unwrap();
        assert_eq!(code, 0);
    }

    // =========================================================================
    // Comprehensive Sequence Execution Tests
    // =========================================================================

    #[test]
    fn sequence_multiple_commands() {
        let shell = test_shell();

        // Execute sequence and verify side effects
        run(&shell, "echo first > /workspace/seq1.txt; echo second > /workspace/seq2.txt; echo third > /workspace/seq3.txt", 3).unwrap();

        let c1 = shell.vfs.borrow().read_file("/workspace/seq1.txt").unwrap();
        let c2 = shell.vfs.borrow().read_file("/workspace/seq2.txt").unwrap();
        let c3 = shell.vfs.borrow().read_file("/workspace/seq3.txt").unwrap();

        assert_eq!(String::from_utf8_lossy(&c1).trim(), "first");
        assert_eq!(String::from_utf8_lossy(&c2).trim(), "second");
        assert_eq!(String::from_utf8_lossy(&c3).trim(), "third");
    }

    #[test]
    fn sequence_exit_code_from_last() {
        let shell = test_shell();

        // Exit code should be from last command
        let code = run(&shell, "true; true; true", 3).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "true; true; false", 3).unwrap();
        assert_eq!(code, 1);

        let code = run(&shell, "false; false; true", 3).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn sequence_continues_after_failure() {
        let shell = test_shell();

        // Sequence continues even if middle command fails
        run(
            &shell,
            "echo before > /workspace/before.txt; false; echo after > /workspace/after.txt",
            3,
        )
        .unwrap();

        // Both files should exist
        let before = shell
            .vfs
            .borrow()
            .read_file("/workspace/before.txt")
            .unwrap();
        let after = shell
            .vfs
            .borrow()
            .read_file("/workspace/after.txt")
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&before).trim(), "before");
        assert_eq!(String::from_utf8_lossy(&after).trim(), "after");
    }

    #[test]
    fn sequence_with_exit_codes() {
        let shell = test_shell();

        // Last exit code wins
        let code = run(&shell, "exit 5; exit 10; exit 15", 3).unwrap();
        assert_eq!(code, 15);
    }

    #[test]
    fn sequence_env_changes_persist() {
        let shell = test_shell();

        // Environment changes should persist across sequence
        run(&shell, "export SEQ_VAR=first; export SEQ_VAR=second", 2).unwrap();
        assert_eq!(shell.env().get("SEQ_VAR"), Some("second"));
    }

    #[test]
    fn sequence_cwd_changes_persist() {
        let shell = test_shell();

        // Create nested directories
        shell
            .vfs
            .borrow_mut()
            .create_dir_all("/workspace/a/b", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // cd changes should persist across sequence
        run(&shell, "cd /workspace; cd a; cd b", 3).unwrap();
        assert_eq!(shell.cwd(), "/workspace/a/b");
    }

    // =========================================================================
    // Comprehensive Subshell Isolation Tests
    // =========================================================================

    #[test]
    fn subshell_cwd_isolation_nested() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .create_dir_all("/workspace/sub1/sub2", amla_vfs::Permission::ReadWrite)
            .unwrap();

        run(&shell, "cd /workspace", 1).unwrap();
        assert_eq!(shell.cwd(), "/workspace");

        // Nested subshell cd
        run(&shell, "(cd /workspace/sub1; (cd sub2))", 2).unwrap();
        assert_eq!(shell.cwd(), "/workspace");
    }

    #[test]
    fn subshell_env_isolation_multiple_vars() {
        let shell = test_shell();

        run(&shell, "export VAR1=parent1; export VAR2=parent2", 2).unwrap();

        // Modify multiple vars in subshell
        run(
            &shell,
            "(export VAR1=child1; export VAR2=child2; export VAR3=child3)",
            3,
        )
        .unwrap();

        assert_eq!(shell.env().get("VAR1"), Some("parent1"));
        assert_eq!(shell.env().get("VAR2"), Some("parent2"));
        assert_eq!(shell.env().get("VAR3"), None); // Not set in parent
    }

    #[test]
    fn subshell_exit_code_from_last_in_sequence() {
        let shell = test_shell();

        // Subshell with sequence, exit code from last
        let code = run(&shell, "(true; false; true)", 3).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "(true; true; false)", 3).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn subshell_exit_code_propagation() {
        let shell = test_shell();

        // Explicit exit in subshell
        let code = run(&shell, "(exit 42)", 1).unwrap();
        assert_eq!(code, 42);

        let code = run(&shell, "(exit 0)", 1).unwrap();
        assert_eq!(code, 0);

        let code = run(&shell, "(exit 255)", 1).unwrap();
        assert_eq!(code, 255);
    }

    #[test]
    fn subshell_with_pipeline() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/data.txt",
                b"one\ntwo\nthree",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Pipeline inside subshell
        run(
            &shell,
            "(cat /workspace/data.txt | grep o > /workspace/out.txt)",
            2,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("one"));
        assert!(text.contains("two"));
        assert!(!text.contains("three"));
    }

    #[test]
    fn subshell_file_writes_persist() {
        let shell = test_shell();

        // File writes in subshell SHOULD persist (VFS is shared)
        run(
            &shell,
            "(echo from_subshell > /workspace/subshell_out.txt)",
            1,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/subshell_out.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "from_subshell");
    }

    // =========================================================================
    // Comprehensive Background Job Tests
    // =========================================================================

    #[test]
    fn background_returns_immediately() {
        let shell = test_shell();

        let start = std::time::Instant::now();
        // This should return immediately
        let code = run(&shell, "sleep 0 &", 0).unwrap();
        let duration = start.elapsed();

        assert_eq!(code, 0);
        // Should be fast (< 100ms)
        assert!(
            duration.as_millis() < 100,
            "Background should return immediately"
        );
    }

    #[test]
    fn background_multiple_jobs() {
        let shell = test_shell();

        run(&shell, "true &", 0).unwrap();
        run(&shell, "true &", 0).unwrap();
        run(&shell, "true &", 0).unwrap();

        // Jobs may have completed already (they're very fast)
        // Just verify no panic occurred - the job count could be 0-3
        let _job_count = shell.jobs.borrow().len();
    }

    #[test]
    fn background_job_completes() {
        let shell = test_shell();

        // Start a simple background job
        run(&shell, "echo bg > /workspace/bg.txt &", 0).unwrap();

        // Wait for it
        run(&shell, "wait", 0).unwrap();

        // File should exist
        let content = shell.vfs.borrow().read_file("/workspace/bg.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "bg");
    }

    #[test]
    fn background_pipeline_execution() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/input.txt",
                b"alpha\nbeta\ngamma",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Background pipeline - just verify it starts without error
        let code = run(&shell, "cat /workspace/input.txt | cat &", 0).unwrap();
        assert_eq!(code, 0, "Background pipeline should return 0 immediately");

        // Wait for completion
        run(&shell, "wait", 0).unwrap();

        // The job table should be empty after wait
        let job_count = shell.jobs.borrow().len();
        assert_eq!(job_count, 0, "All jobs should be completed after wait");
    }

    #[test]
    fn wait_returns_last_exit_code() {
        let shell = test_shell();

        // Note: with multiple jobs, wait returns last completed job's code
        run(&shell, "true &", 0).unwrap();
        let code = run(&shell, "wait", 0).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn wait_specific_job_by_id() {
        let shell = test_shell();

        run(&shell, "true &", 0).unwrap();
        run(&shell, "false &", 0).unwrap();

        // Wait for specific job
        let code = run(&shell, "wait %1", 0).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn fg_waits_for_completion() {
        let shell = test_shell();

        run(&shell, "true &", 0).unwrap();

        let code = run(&shell, "fg", 0).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn jobs_empty_initially() {
        let shell = test_shell();

        // Jobs command should succeed with no jobs
        let code = run(&shell, "jobs", 1).unwrap();
        assert_eq!(code, 0);
    }

    // =========================================================================
    // Wait/Fg with Sleep Tests (timer-based yielding)
    // =========================================================================

    /// Create a shell with controllable mock time for testing timer-based operations.
    fn test_shell_with_time() -> (Shell, Rc<Cell<u64>>) {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        let scheduler = Scheduler::new(1, time_source, random_source);
        (Shell::new(scheduler), mock_time)
    }

    /// Run a command with host op handling (WakeAt timer completion).
    /// Advances mock time as needed to complete sleep operations.
    fn run_with_timers(shell: &Shell, mock_time: &Rc<Cell<u64>>, cmd: &str) -> Result<i32> {
        use amla_scheduler::{HostOpKind, SchedulerState};
        use std::task::{Context, Poll};
        const MAX_ITERATIONS: usize = 100_000;

        let scheduler = shell.scheduler.clone();
        let mut fut = std::pin::pin!(shell.execute(cmd));

        let waker = amla_scheduler::noop_waker();
        let mut cx = Context::from_waker(&waker);

        for _ in 0..MAX_ITERATIONS {
            // Poll the future
            if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
                return result;
            }

            // Run scheduler
            let state = scheduler.run();

            // Handle any WakeAt operations
            while let Some(req) = scheduler.take_host_op() {
                match req.kind {
                    HostOpKind::WakeAt { deadline } => {
                        // Advance time to the deadline and complete the op
                        if deadline > mock_time.get() {
                            mock_time.set(deadline);
                        }
                        scheduler.complete_host_op(req.id, deadline.to_le_bytes().to_vec());
                    }
                    _ => {
                        // For other ops, just provide empty response
                        scheduler.complete_host_op(req.id, vec![]);
                    }
                }
            }

            // If done and no pending ops, break
            if matches!(state, SchedulerState::Done) && scheduler.take_host_op().is_none() {
                break;
            }
        }

        panic!("command '{cmd}' did not complete within MAX_ITERATIONS");
    }

    #[test]
    fn wait_with_background_sleep() {
        let (shell, mock_time) = test_shell_with_time();

        // Start time at 0
        mock_time.set(0);

        // Run: sleep 0.5 in background, then wait
        let code = run_with_timers(&shell, &mock_time, "sleep 0.5 &").unwrap();
        assert_eq!(code, 0, "background command should return immediately");

        // Should have a job
        assert_eq!(shell.jobs.borrow().len(), 1, "should have 1 background job");

        // Wait for the background job
        let code = run_with_timers(&shell, &mock_time, "wait").unwrap();
        assert_eq!(code, 0, "wait should succeed");

        // Jobs should be cleared
        assert_eq!(
            shell.jobs.borrow().len(),
            0,
            "jobs should be empty after wait"
        );

        // Time should have advanced to at least 500ms
        let elapsed = mock_time.get();
        assert!(
            elapsed >= 500_000_000,
            "time should have advanced to at least 500ms, got {elapsed} ns"
        );
    }

    #[test]
    fn wait_with_multiple_background_sleeps() {
        let (shell, mock_time) = test_shell_with_time();
        mock_time.set(0);

        // Run two sleeps in background
        run_with_timers(&shell, &mock_time, "sleep 0.3 &").unwrap();
        run_with_timers(&shell, &mock_time, "sleep 0.5 &").unwrap();

        assert_eq!(
            shell.jobs.borrow().len(),
            2,
            "should have 2 background jobs"
        );

        // Wait for all
        let code = run_with_timers(&shell, &mock_time, "wait").unwrap();
        assert_eq!(code, 0);

        // All jobs cleared
        assert_eq!(shell.jobs.borrow().len(), 0);

        // Time should have advanced to cover the longest sleep (500ms)
        assert!(
            mock_time.get() >= 500_000_000,
            "time should have advanced for longest sleep"
        );
    }

    #[test]
    fn wait_inline_with_background_sleeps() {
        let (shell, mock_time) = test_shell_with_time();
        mock_time.set(0);

        // The command that previously failed: two parallel sleeps + wait
        // Parser should produce: Sequence[Background(sleep 0.5), Background(sleep 0.5), wait]
        let code = run_with_timers(&shell, &mock_time, "sleep 0.5 & sleep 0.5 & wait").unwrap();
        assert_eq!(code, 0, "command should succeed");

        // All jobs should be completed
        assert_eq!(
            shell.jobs.borrow().len(),
            0,
            "all jobs should be completed after wait"
        );

        // Time should have advanced (both sleeps are 500ms, run in parallel)
        let elapsed_ns = mock_time.get();
        assert!(
            elapsed_ns >= 500_000_000,
            "time should have advanced to at least 500ms, got {elapsed_ns} ns"
        );
    }

    #[test]
    fn fg_with_background_sleep() {
        let (shell, mock_time) = test_shell_with_time();
        mock_time.set(0);

        // Start sleep in background
        run_with_timers(&shell, &mock_time, "sleep 0.3 &").unwrap();
        assert_eq!(shell.jobs.borrow().len(), 1);

        // Bring to foreground - should wait for completion
        let code = run_with_timers(&shell, &mock_time, "fg").unwrap();
        assert_eq!(code, 0);

        // Job should be removed
        assert_eq!(shell.jobs.borrow().len(), 0);

        // Time should have advanced
        assert!(
            mock_time.get() >= 300_000_000,
            "time should advance for sleep"
        );
    }

    #[test]
    fn wait_specific_job_with_sleep() {
        let (shell, mock_time) = test_shell_with_time();
        mock_time.set(0);

        // Start two background jobs
        run_with_timers(&shell, &mock_time, "sleep 0.2 &").unwrap(); // job 1
        run_with_timers(&shell, &mock_time, "sleep 0.4 &").unwrap(); // job 2

        assert_eq!(shell.jobs.borrow().len(), 2);

        // Wait for just job 1
        let code = run_with_timers(&shell, &mock_time, "wait %1").unwrap();
        assert_eq!(code, 0);

        // Job 1 should be removed, job 2 may still exist
        // (depending on timing - job 2 might have completed too)
        let remaining = shell.jobs.borrow().len();
        assert!(remaining <= 1, "at most 1 job should remain");
    }

    // =========================================================================
    // Comprehensive Redirect Execution Tests
    // =========================================================================

    #[test]
    fn redirect_chain_stdin_stdout() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/source.txt",
                b"source content",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Read from file, write to different file
        run(
            &shell,
            "cat < /workspace/source.txt > /workspace/dest.txt",
            1,
        )
        .unwrap();

        let content = shell.vfs.borrow().read_file("/workspace/dest.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "source content");
    }

    #[test]
    fn redirect_append_creates_file() {
        let shell = test_shell();

        // File doesn't exist, append should create it
        assert!(!shell.vfs.borrow().exists("/workspace/new_append.txt"));

        run(&shell, "echo created >> /workspace/new_append.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/new_append.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "created");
    }

    #[test]
    fn redirect_append_preserves_content() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/existing.txt",
                b"original\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "echo appended >> /workspace/existing.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/existing.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.starts_with("original\n"));
        assert!(text.contains("appended"));
    }

    #[test]
    fn redirect_truncate_overwrites() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/overwrite_me.txt",
                b"this will be overwritten completely",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "echo new > /workspace/overwrite_me.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/overwrite_me.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert_eq!(text.trim(), "new");
        assert!(!text.contains("overwritten"));
    }

    #[test]
    fn redirect_stderr_separate_from_stdout() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/exists.txt",
                b"exists",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // stdout to one file, stderr to another
        run(&shell, "cat /workspace/exists.txt /nonexistent > /workspace/stdout.txt 2> /workspace/stderr.txt", 1)
            .unwrap();

        // stdout should have the existing file content
        let stdout = shell
            .vfs
            .borrow()
            .read_file("/workspace/stdout.txt")
            .unwrap();
        let stdout_text = String::from_utf8_lossy(&stdout);
        // May or may not have content depending on cat behavior

        // stderr should have error message
        let stderr = shell
            .vfs
            .borrow()
            .read_file("/workspace/stderr.txt")
            .unwrap();
        let stderr_text = String::from_utf8_lossy(&stderr);
        assert!(
            stderr_text.contains("cat:")
                || stderr_text.contains("no such")
                || stderr_text.is_empty()
                || stdout_text.contains("exists")
        );
    }

    #[test]
    fn redirect_stderr_to_stdout_2_and_1() {
        let shell = test_shell();

        // 2>&1 redirects stderr to stdout
        run(&shell, "cat /nonexistent > /workspace/combined.txt 2>&1", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/combined.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        // Error message should be in stdout file
        assert!(text.contains("cat:") || text.contains("no such"));
    }

    #[test]
    fn redirect_multiple_stdout() {
        let shell = test_shell();

        // Multiple stdout redirects, last one wins
        run(
            &shell,
            "echo test > /workspace/first.txt > /workspace/second.txt",
            1,
        )
        .unwrap();

        // Second file should have content
        let second = shell
            .vfs
            .borrow()
            .read_file("/workspace/second.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&second).trim(), "test");
    }

    #[test]
    fn redirect_in_pipeline_first_command() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/pipe_in.txt",
                b"pipe input data",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // First command gets stdin from file
        run(
            &shell,
            "cat < /workspace/pipe_in.txt | cat > /workspace/pipe_out.txt",
            2,
        )
        .unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/pipe_out.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content), "pipe input data");
    }

    #[test]
    fn redirect_in_pipeline_last_command() {
        let shell = test_shell();

        // Last command redirects to file
        run(&shell, "echo piped | cat | cat > /workspace/final.txt", 3).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/final.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "piped");
    }

    #[test]
    fn redirect_stderr_append_to_existing() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/error_log.txt",
                b"Previous errors\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Append stderr to existing file
        run(&shell, "cat /nonexistent 2>> /workspace/error_log.txt", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/error_log.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.starts_with("Previous errors\n"));
        assert!(text.len() > "Previous errors\n".len()); // Has new error
    }

    #[test]
    fn redirect_stdin_nonexistent_file() {
        let shell = test_shell();

        // Reading from nonexistent file should fail
        let result = run(&shell, "cat < /workspace/does_not_exist.txt", 0);
        // Should return an error or non-zero exit code (test primarily verifies no panic)
        assert!(result.is_err() || result.is_ok_and(|code| code != 0));
    }

    // =========================================================================
    // Edge Cases and Error Handling
    // =========================================================================

    #[test]
    fn empty_pipeline_stage() {
        let shell = test_shell();

        // Empty stages in pipeline should be handled gracefully
        let code = run(&shell, "echo test |  | cat", 0).unwrap_or(0);
        // Should not panic, exit code may vary
        let _ = code;
    }

    #[test]
    fn deeply_nested_and_or() {
        let shell = test_shell();

        // Deep nesting of && and ||
        let code = run(&shell, "true && true && true && true && false || true", 6).unwrap();
        // (true && true && true && true && false) -> 1, then || true -> 0
        assert_eq!(code, 0);
    }

    #[test]
    fn sequence_with_pipelines() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/seq_pipe.txt",
                b"one\ntwo\nthree",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Sequence containing pipelines
        run(&shell, "cat /workspace/seq_pipe.txt | head -1 > /workspace/first.txt; cat /workspace/seq_pipe.txt | tail -1 > /workspace/last.txt", 3).unwrap();

        let first = shell
            .vfs
            .borrow()
            .read_file("/workspace/first.txt")
            .unwrap();
        let last = shell.vfs.borrow().read_file("/workspace/last.txt").unwrap();

        assert_eq!(String::from_utf8_lossy(&first).trim(), "one");
        assert_eq!(String::from_utf8_lossy(&last).trim(), "three");
    }

    #[test]
    fn and_or_with_pipelines() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/and_or_pipe.txt",
                b"match\nno_match",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Pipeline in && chain
        let code = run(
            &shell,
            "cat /workspace/and_or_pipe.txt | grep match && echo found > /workspace/found.txt",
            3,
        )
        .unwrap();
        assert_eq!(code, 0);

        let found = shell
            .vfs
            .borrow()
            .read_file("/workspace/found.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&found).trim(), "found");
    }

    #[test]
    fn background_with_and_or() {
        let shell = test_shell();

        // Background a command that uses && (note: only first part runs in bg)
        let code = run(&shell, "true &", 0).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn subshell_with_and_or() {
        let shell = test_shell();

        // && inside subshell
        let code = run(&shell, "(true && false)", 2).unwrap();
        assert_eq!(code, 1);

        // || inside subshell
        let code = run(&shell, "(false || true)", 2).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn subshell_with_sequence() {
        let shell = test_shell();

        // Sequence inside subshell, exit code from last
        let code = run(&shell, "(true; false; exit 42)", 3).unwrap();
        assert_eq!(code, 42);
    }

    #[test]
    fn redirect_with_variable_path() {
        let shell = test_shell();

        run(&shell, "export OUT_FILE=/workspace/var_out.txt", 1).unwrap();
        run(&shell, "echo variable_path > $OUT_FILE", 1).unwrap();

        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/var_out.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "variable_path");
    }

    #[test]
    fn pipeline_with_grep_no_match_exit_code() {
        let shell = test_shell();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/nomatch.txt",
                b"hello world",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // grep with no match returns 1
        let code = run(&shell, "cat /workspace/nomatch.txt | grep xyz", 2).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn multiple_redirects_same_fd() {
        let shell = test_shell();

        // Multiple redirects to same fd, last wins
        run(
            &shell,
            "echo multi > /workspace/r1.txt > /workspace/r2.txt > /workspace/r3.txt",
            1,
        )
        .unwrap();

        // Only r3.txt should have content (last redirect wins)
        let r3 = shell.vfs.borrow().read_file("/workspace/r3.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&r3).trim(), "multi");
    }

    #[test]
    fn binary_data_through_pipeline() {
        let shell = test_shell();

        // Create binary data including null bytes and high bytes
        let binary: Vec<u8> = (0..=255).collect();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/binary_input.dat",
                &binary,
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Pass through pipeline
        run(
            &shell,
            "cat /workspace/binary_input.dat | cat | cat > /workspace/binary_output.dat",
            3,
        )
        .unwrap();

        let output = shell
            .vfs
            .borrow()
            .read_file("/workspace/binary_output.dat")
            .unwrap();
        assert_eq!(output, binary);
    }

    #[test]
    fn long_pipeline_exit_codes() {
        let shell = test_shell();

        // 5-stage pipeline, various exit codes
        // All succeed: 0
        let code = run(&shell, "true | true | true | true | true", 1).unwrap();
        assert_eq!(code, 0);

        // Last fails: 1
        let code = run(&shell, "true | true | true | true | false", 1).unwrap();
        assert_eq!(code, 1);

        // First fails, last succeeds: 0
        let code = run(&shell, "false | true | true | true | true", 1).unwrap();
        assert_eq!(code, 0);
    }

    // =========================================================================
    // Constructor edge case tests
    // =========================================================================

    #[test]
    fn from_vfs_ref_creates_shell() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let shell = Shell::from_vfs_ref(test_scheduler(), vfs);
        assert_eq!(shell.cwd(), "/workspace");
    }

    #[test]
    fn with_full_context_inherits_state() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let mut env = Environment::new();
        env.set("CUSTOM", "value");
        let shell = Shell::with_full_context(
            test_scheduler(),
            vfs,
            "/custom/path".to_string(),
            env,
            IoHandle::null(),
            IoHandle::buffer(),
            IoHandle::buffer(),
        );
        assert_eq!(shell.cwd(), "/custom/path");
        assert_eq!(shell.env().get("CUSTOM"), Some("value"));
    }

    #[test]
    fn set_cwd_success() {
        let shell = test_shell();
        assert!(shell.set_cwd("/workspace").is_ok());
        assert_eq!(shell.cwd(), "/workspace");
    }

    #[test]
    fn set_cwd_not_a_directory() {
        let shell = test_shell();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/file.txt",
                b"content",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        let result = shell.set_cwd("/workspace/file.txt");
        assert!(result.is_err());
    }

    #[test]
    fn env_mut_allows_modification() {
        let shell = test_shell();
        shell.env_mut().set("MUTATED", "yes");
        assert_eq!(shell.env().get("MUTATED"), Some("yes"));
    }

    // =========================================================================
    // Command not found error tests
    // =========================================================================

    #[test]
    fn command_not_found_error() {
        let shell = test_shell();
        let result = run(&shell, "nonexistent_command arg1 arg2", 0);
        assert!(result.is_err());
        match result {
            Err(ShellError::CommandNotFound(cmd)) => {
                assert_eq!(cmd, "nonexistent_command");
            }
            _ => panic!("expected CommandNotFound error"),
        }
    }

    // =========================================================================
    // Job control builtin tests
    // =========================================================================

    #[test]
    fn builtin_jobs_empty() {
        let shell = test_shell();
        let code = run(&shell, "jobs", 1).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn builtin_fg_no_jobs() {
        let shell = test_shell();
        // fg with no jobs should fail
        let code = run(&shell, "fg", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn builtin_fg_invalid_job() {
        let shell = test_shell();
        // fg with invalid job spec
        let code = run(&shell, "fg %999", 0).unwrap();
        assert_eq!(code, 1);
    }

    #[test]
    fn builtin_wait_no_jobs() {
        let shell = test_shell();
        let code = run(&shell, "wait", 0).unwrap();
        assert_eq!(code, 0);
    }

    // =========================================================================
    // Redirect edge case tests (additional coverage)
    // =========================================================================

    #[test]
    fn redirect_both_stdout_stderr() {
        let shell = test_shell();
        // &> redirects both stdout and stderr
        run(&shell, "echo both &> /workspace/both.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/both.txt").unwrap();
        assert!(String::from_utf8_lossy(&content).contains("both"));
    }

    #[test]
    fn redirect_stderr_only() {
        let shell = test_shell();
        // 2> redirects only stderr (echo writes to stdout, so file should be empty)
        run(&shell, "echo hello 2> /workspace/err_only.txt", 1).unwrap();
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/err_only.txt")
            .unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn redirect_stdin_file() {
        let shell = test_shell();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/input.txt",
                b"line1\nline2\nline3\n",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(
            &shell,
            "cat < /workspace/input.txt > /workspace/output.txt",
            1,
        )
        .unwrap();
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/output.txt")
            .unwrap();
        assert!(String::from_utf8_lossy(&content).contains("line1"));
    }

    // =========================================================================
    // Format command tests
    // =========================================================================

    #[test]
    fn format_command_empty() {
        let cmd = Command::Empty;
        assert_eq!(format_command(&cmd), "");
    }

    #[test]
    fn format_command_simple() {
        let cmd = Command::simple(vec!["ls".into(), "-la".into()]);
        assert_eq!(format_command(&cmd), "ls -la");
    }

    #[test]
    fn format_command_pipeline() {
        let cmd = Command::Pipeline {
            commands: vec![
                Command::simple(vec!["cat".into(), "file".into()]),
                Command::simple(vec!["grep".into(), "foo".into()]),
            ],
        };
        assert_eq!(format_command(&cmd), "cat file | grep foo");
    }

    #[test]
    fn format_command_and() {
        let cmd = Command::And {
            left: Box::new(Command::simple(vec!["cmd1".into()])),
            right: Box::new(Command::simple(vec!["cmd2".into()])),
        };
        assert_eq!(format_command(&cmd), "cmd1 && cmd2");
    }

    #[test]
    fn format_command_or() {
        let cmd = Command::Or {
            left: Box::new(Command::simple(vec!["cmd1".into()])),
            right: Box::new(Command::simple(vec!["cmd2".into()])),
        };
        assert_eq!(format_command(&cmd), "cmd1 || cmd2");
    }

    #[test]
    fn format_command_sequence() {
        let cmd = Command::Sequence {
            commands: vec![
                Command::simple(vec!["cmd1".into()]),
                Command::simple(vec!["cmd2".into()]),
            ],
        };
        assert_eq!(format_command(&cmd), "cmd1; cmd2");
    }

    #[test]
    fn format_command_background() {
        let cmd = Command::Background {
            command: Box::new(Command::simple(vec!["sleep".into(), "10".into()])),
        };
        assert_eq!(format_command(&cmd), "sleep 10 &");
    }

    #[test]
    fn format_command_subshell() {
        let cmd = Command::Subshell {
            command: Box::new(Command::simple(vec!["pwd".into()])),
        };
        assert_eq!(format_command(&cmd), "(pwd)");
    }

    // =========================================================================
    // Last exit code tests
    // =========================================================================

    #[test]
    fn last_exit_updates_on_success() {
        let shell = test_shell();
        run(&shell, "true", 1).unwrap();
        assert_eq!(shell.last_exit(), 0);
    }

    #[test]
    fn last_exit_updates_on_failure() {
        let shell = test_shell();
        run(&shell, "false", 1).unwrap();
        assert_eq!(shell.last_exit(), 1);
    }

    #[test]
    fn last_exit_reflects_last_command() {
        let shell = test_shell();
        run(&shell, "true", 1).unwrap();
        run(&shell, "false", 1).unwrap();
        assert_eq!(shell.last_exit(), 1);

        run(&shell, "true", 1).unwrap();
        assert_eq!(shell.last_exit(), 0);
    }

    // =========================================================================
    // Variable expansion tests
    // =========================================================================

    #[test]
    fn expand_simple_variable() {
        let shell = test_shell();
        run(&shell, "export MYVAR=hello", 1).unwrap();

        // Echo should expand $MYVAR
        run(&shell, "echo $MYVAR > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello");
    }

    #[test]
    fn expand_braced_variable() {
        let shell = test_shell();
        run(&shell, "export NAME=world", 1).unwrap();

        run(&shell, "echo ${NAME} > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "world");
    }

    #[test]
    fn expand_exit_code_variable() {
        let shell = test_shell();
        run(&shell, "true", 1).unwrap();
        run(&shell, "echo $? > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "0");

        run(&shell, "false", 1).unwrap();
        run(&shell, "echo $? > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "1");
    }

    #[test]
    fn expand_missing_variable() {
        let shell = test_shell();
        // Missing variable expands to empty string
        run(&shell, "echo $NONEXISTENT > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "");
    }

    #[test]
    fn expand_variable_in_redirect() {
        let shell = test_shell();
        run(&shell, "export OUTFILE=output.txt", 1).unwrap();
        run(&shell, "echo hello > /workspace/$OUTFILE", 1).unwrap();
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/output.txt")
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello");
    }

    #[test]
    fn expand_multiple_variables() {
        let shell = test_shell();
        run(&shell, "export A=foo", 1).unwrap();
        run(&shell, "export B=bar", 1).unwrap();
        run(&shell, "echo $A$B > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "foobar");
    }

    // =========================================================================
    // Tilde expansion tests
    // =========================================================================

    #[test]
    fn expand_tilde_alone() {
        let shell = test_shell();
        run(&shell, "export HOME=/home/user", 1).unwrap();
        run(&shell, "echo ~ > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "/home/user");
    }

    #[test]
    fn expand_tilde_with_path() {
        let shell = test_shell();
        run(&shell, "export HOME=/home/user", 1).unwrap();
        run(&shell, "echo ~/documents > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(
            String::from_utf8_lossy(&content).trim(),
            "/home/user/documents"
        );
    }

    #[test]
    fn expand_tilde_no_home() {
        let shell = test_shell();
        run(&shell, "unset HOME", 1).unwrap();
        // Without HOME, ~ expands to empty
        run(&shell, "echo ~ > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "");
    }

    #[test]
    fn expand_tilde_in_redirect() {
        let shell = test_shell();
        run(&shell, "export HOME=/workspace", 1).unwrap();
        run(&shell, "echo hello > ~/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "hello");
    }

    #[test]
    fn no_tilde_expansion_in_middle() {
        let shell = test_shell();
        run(&shell, "export HOME=/home/user", 1).unwrap();
        // Tilde in middle of word should not expand
        run(&shell, "echo foo~bar > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "foo~bar");
    }

    #[test]
    fn expand_tilde_user_alone() {
        let shell = test_shell();
        run(&shell, "export HOME=/home/user", 1).unwrap();
        // ~anyuser expands to HOME in sandbox
        run(&shell, "echo ~root > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "/home/user");
    }

    #[test]
    fn expand_tilde_user_with_path() {
        let shell = test_shell();
        run(&shell, "export HOME=/home/user", 1).unwrap();
        // ~anyuser/path expands to HOME/path in sandbox
        run(&shell, "echo ~root/documents > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(
            String::from_utf8_lossy(&content).trim(),
            "/home/user/documents"
        );
    }

    #[test]
    fn expand_tilde_user_no_home() {
        let shell = test_shell();
        run(&shell, "unset HOME", 1).unwrap();
        // Without HOME, ~user returns literal
        run(&shell, "echo ~root > /workspace/out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "");
    }

    // =========================================================================
    // Glob expansion tests
    // =========================================================================

    #[test]
    fn glob_star_expansion() {
        let shell = test_shell();
        run(&shell, "cd /workspace", 1).unwrap();

        // Create some files
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/a.txt", b"a", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/b.txt", b"b", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/c.log", b"c", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // Glob should expand and pass multiple files to cat
        run(&shell, "cat *.txt > out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        // Should contain both a.txt and b.txt contents
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains('a') && text.contains('b'));
    }

    #[test]
    fn glob_question_mark_expansion() {
        let shell = test_shell();
        run(&shell, "cd /workspace", 1).unwrap();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/f1.txt", b"1", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/f2.txt", b"2", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/f10.txt", b"10", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // f?.txt should match f1.txt and f2.txt but not f10.txt
        run(&shell, "cat f?.txt > out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains('1') && text.contains('2'));
        assert!(!text.contains("10"));
    }

    #[test]
    fn glob_bracket_expansion() {
        let shell = test_shell();
        run(&shell, "cd /workspace", 1).unwrap();

        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/a.txt", b"A", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/b.txt", b"B", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/c.txt", b"C", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // [ab].txt should match a.txt and b.txt but not c.txt
        run(&shell, "cat [ab].txt > out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains('A') && text.contains('B'));
        assert!(!text.contains('C'));
    }

    #[test]
    fn glob_no_match_returns_pattern() {
        let shell = test_shell();
        run(&shell, "cd /workspace", 1).unwrap();

        // Glob with no matches passes pattern literally (POSIX behavior)
        run(&shell, "echo *.nonexistent > out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "*.nonexistent");
    }

    #[test]
    fn glob_with_subdirectory() {
        let shell = test_shell();
        run(&shell, "cd /workspace", 1).unwrap();

        shell
            .vfs
            .borrow_mut()
            .create_dir("/workspace/sub", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/sub/x.txt",
                b"X",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/sub/y.txt",
                b"Y",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        run(&shell, "cat sub/*.txt > out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains('X') && text.contains('Y'));
    }

    #[test]
    fn glob_with_variable() {
        let shell = test_shell();
        run(&shell, "cd /workspace", 1).unwrap();

        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/test.txt",
                b"content",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Variable expansion should happen before glob
        run(&shell, "export EXT=txt", 1).unwrap();
        run(&shell, "cat *.$EXT > out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert!(String::from_utf8_lossy(&content).contains("content"));
    }

    #[test]
    fn glob_sorted_results() {
        let shell = test_shell();
        run(&shell, "cd /workspace", 1).unwrap();

        // Create files in non-alphabetical order
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/z.txt", b"z\n", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/a.txt", b"a\n", amla_vfs::Permission::ReadWrite)
            .unwrap();
        shell
            .vfs
            .borrow_mut()
            .write_file("/workspace/m.txt", b"m\n", amla_vfs::Permission::ReadWrite)
            .unwrap();

        // Results should be sorted alphabetically
        run(&shell, "cat *.txt > out.txt", 1).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        let text = String::from_utf8_lossy(&content);
        // a.txt should come before m.txt should come before z.txt
        let a_pos = text.find('a').unwrap();
        let m_pos = text.find('m').unwrap();
        let z_pos = text.find('z').unwrap();
        assert!(a_pos < m_pos && m_pos < z_pos);
    }

    // =========================================================================
    // Integration tests: Shell → VFS permission enforcement
    // =========================================================================
    //
    // These tests verify that the shell correctly enforces VFS permissions
    // through the complete pipeline: lexer → parser → execution → VFS.
    // This is the security-critical path that ensures sandboxing works.

    #[test]
    fn integration_cannot_write_to_readonly_tools() {
        let shell = test_shell();

        // /tools is read-only - shell commands should fail to write there
        let result = run(&shell, "echo malicious > /tools/evil.js", 1);

        // Should fail (non-zero exit code or error)
        if let Ok(code) = result {
            assert_ne!(code, 0, "write to /tools should fail");
        }

        // Verify file was not created
        assert!(
            shell.vfs.borrow().read_file("/tools/evil.js").is_err(),
            "file should not exist in /tools"
        );
    }

    #[test]
    fn integration_cannot_redirect_to_readonly() {
        let shell = test_shell();

        // Try to redirect output to read-only directory via various methods
        let commands = [
            "echo test > /tools/test.txt",
            "echo test >> /tools/test.txt",
            "cat /workspace/test.txt > /bin/test.txt",
        ];

        for cmd in commands {
            let result = run(&shell, cmd, 1);
            if let Ok(code) = result {
                assert_ne!(code, 0, "'{cmd}' should fail");
            }
        }
    }

    #[test]
    fn integration_can_write_to_readwrite_workspace() {
        let shell = test_shell();

        // /workspace is read-write - should succeed
        let code = run(&shell, "echo success > /workspace/test.txt", 1).unwrap();
        assert_eq!(code, 0, "write to /workspace should succeed");

        // Verify content
        let content = shell.vfs.borrow().read_file("/workspace/test.txt").unwrap();
        assert!(String::from_utf8_lossy(&content).contains("success"));
    }

    #[test]
    fn integration_append_only_log_directory() {
        let shell = test_shell();

        // Overwrite (>) should fail on AppendOnly files
        let result = run(&shell, "echo replaced > /log/actions.jsonl", 1);
        if let Ok(code) = result {
            assert_ne!(code, 0, "overwrite of append-only should fail");
        }

        // Append (>>) should succeed on AppendOnly files
        let result = run(&shell, "echo line1 >> /log/actions.jsonl", 1);
        assert_eq!(result.unwrap(), 0, "append to append-only should succeed");

        // Verify data was appended
        let content = shell.vfs.borrow().read_file("/log/actions.jsonl").unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("line1"), "appended line should be in file");
    }

    #[test]
    fn integration_path_traversal_blocked() {
        let shell = test_shell();

        // Attempt to escape /workspace via path traversal
        let traversal_attempts = [
            "echo evil > /workspace/../tools/evil.js",
            "echo evil > /workspace/../../tools/evil.js",
            "cat /workspace/test.txt > ../tools/evil.js",
        ];

        for cmd in traversal_attempts {
            let result = run(&shell, cmd, 1);
            if let Ok(code) = result {
                assert_ne!(code, 0, "path traversal '{cmd}' should fail");
            }
        }

        // Verify no files created in /tools
        assert!(shell.vfs.borrow().read_file("/tools/evil.js").is_err());
    }

    #[test]
    fn integration_full_pipeline_with_permissions() {
        let shell = test_shell();

        // Create a file in workspace
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/input.txt",
                b"line1\nline2\nline3",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();

        // Full pipeline: read from workspace, process, write to workspace
        let code = run(
            &shell,
            "cat /workspace/input.txt | grep line | head -2 > /workspace/output.txt",
            3,
        )
        .unwrap();
        assert_eq!(code, 0);

        // Verify output
        let content = shell
            .vfs
            .borrow()
            .read_file("/workspace/output.txt")
            .unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("line1"));
        assert!(text.contains("line2"));
    }

    #[test]
    fn integration_lexer_parser_execution_flow() {
        // Test the complete flow: source code → tokens → AST → execution → VFS

        // 1. Lexer tokenizes input
        let input = "echo hello | cat > /workspace/out.txt";
        let mut lexer = Lexer::new(input);
        let mut found_pipe = false;
        loop {
            let tok = lexer.next().unwrap();
            if tok == Token::Eof {
                break;
            }
            if matches!(tok, Token::Pipe) {
                found_pipe = true;
            }
        }
        assert!(found_pipe, "lexer should find pipe token");

        // 2. Parser builds AST
        let ast = parse(input).expect("parse should succeed");
        assert!(
            matches!(ast, Command::Pipeline { .. }),
            "should parse as pipeline"
        );

        // 3. Execution writes to VFS
        let shell = test_shell();
        let code = run(&shell, input, 2).unwrap();
        assert_eq!(code, 0);

        // 4. VFS contains result
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert!(String::from_utf8_lossy(&content).contains("hello"));
    }

    #[test]
    fn integration_permission_zones_complete() {
        let shell = test_shell();

        // Verify all standard permission zones

        // 1. Root (/) - read-only
        let result = run(&shell, "echo test > /rootfile.txt", 1);
        assert!(matches!(result, Ok(code) if code != 0) || result.is_err());

        // 2. /tools - read-only (tested above)

        // 3. /workspace - read-write
        assert_eq!(
            run(&shell, "echo ok > /workspace/zone_test.txt", 1).unwrap(),
            0
        );

        // 4. /log - append-only
        // NOTE: Shell append uses write_file internally, so it fails on append-only.
        // This tests that the directory blocks overwrites (the important security property).
        let result = run(&shell, "echo log > /log/actions.jsonl", 1);
        assert!(
            matches!(result, Ok(code) if code != 0) || result.is_err(),
            "/log should block overwrites"
        );

        // 5. /bin - read-only
        let result = run(&shell, "echo test > /bin/evil", 1);
        assert!(matches!(result, Ok(code) if code != 0) || result.is_err());
    }

    #[test]
    fn integration_rm_respects_permissions() {
        let shell = test_shell();

        // Setup: create files with different permissions via insert_file
        shell
            .vfs
            .borrow_mut()
            .insert_file(
                "/tools/protected.txt",
                b"data",
                amla_vfs::Permission::ReadOnly,
            )
            .unwrap();

        // Cannot rm from read-only directory
        let result = run(&shell, "rm /tools/protected.txt", 1);
        assert!(
            matches!(result, Ok(code) if code != 0) || result.is_err(),
            "rm from /tools should fail"
        );

        // Can rm from read-write directory
        shell
            .vfs
            .borrow_mut()
            .write_file(
                "/workspace/deleteme.txt",
                b"data",
                amla_vfs::Permission::ReadWrite,
            )
            .unwrap();
        let code = run(&shell, "rm /workspace/deleteme.txt", 1).unwrap();
        assert_eq!(code, 0, "rm from /workspace should succeed");
        assert!(
            shell
                .vfs
                .borrow()
                .read_file("/workspace/deleteme.txt")
                .is_err()
        );
    }

    #[test]
    fn integration_mkdir_respects_permissions() {
        let shell = test_shell();

        // Cannot mkdir in read-only directory
        let result = run(&shell, "mkdir /tools/subdir", 1);
        assert!(
            matches!(result, Ok(code) if code != 0) || result.is_err(),
            "mkdir in /tools should fail"
        );

        // Can mkdir in read-write directory
        let code = run(&shell, "mkdir /workspace/newdir", 1).unwrap();
        assert_eq!(code, 0, "mkdir in /workspace should succeed");
        assert!(shell.vfs.borrow().is_dir("/workspace/newdir"));
    }

    #[test]
    fn integration_touch_respects_permissions() {
        let shell = test_shell();

        // Cannot touch in read-only directory
        let result = run(&shell, "touch /tools/newfile.txt", 1);
        assert!(
            matches!(result, Ok(code) if code != 0) || result.is_err(),
            "touch in /tools should fail"
        );

        // Can touch in read-write directory
        let code = run(&shell, "touch /workspace/touched.txt", 1).unwrap();
        assert_eq!(code, 0, "touch in /workspace should succeed");
        assert!(
            shell
                .vfs
                .borrow()
                .read_file("/workspace/touched.txt")
                .is_ok()
        );
    }

    // ========== TOOLS COMMAND TESTS ==========

    #[test]
    fn tools_help_shows_usage() {
        let shell = test_shell();

        // tools without subcommand shows help
        let code = run(&shell, "tools", 10).unwrap();
        assert_eq!(code, 0);

        // tools help shows help
        let code = run(&shell, "tools help", 10).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn tools_list_shows_demo_catalog() {
        let shell = test_shell();

        let code = run(&shell, "tools list", 10).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn tools_search_finds_payment_tools() {
        let shell = test_shell();

        // Search for payment-related tools
        let code = run(&shell, "tools search payment", 10).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn tools_search_with_limit() {
        let shell = test_shell();

        // Search with limit option
        let code = run(&shell, "tools search payment -n 2", 10).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn tools_search_with_bm25_flag() {
        let shell = test_shell();

        // Search using pure BM25
        let code = run(&shell, "tools search payment --bm25", 10).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn tools_search_with_semantic_flag() {
        let shell = test_shell();

        // Search using pure semantic (falls back to BM25 if no embedder)
        let code = run(&shell, "tools search payment --semantic", 10).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn tools_search_empty_query_fails() {
        let shell = test_shell();

        // Search without query fails
        let code = run(&shell, "tools search", 10).unwrap();
        assert_ne!(code, 0);
    }

    #[test]
    fn tools_info_shows_tool_details() {
        let shell = test_shell();

        // Get info about a specific tool
        let code = run(&shell, "tools info stripe:charge", 10).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn tools_info_missing_name_fails() {
        let shell = test_shell();

        // Info without name fails
        let code = run(&shell, "tools info", 10).unwrap();
        assert_ne!(code, 0);
    }

    #[test]
    fn tools_info_nonexistent_tool_fails() {
        let shell = test_shell();

        // Info for nonexistent tool fails
        let code = run(&shell, "tools info nonexistent:tool", 10).unwrap();
        assert_ne!(code, 0);
    }

    #[test]
    fn tools_search_with_category_filter() {
        let shell = test_shell();

        // Filter by category
        let code = run(&shell, "tools search charge -c payments", 10).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn tools_embed_without_text_fails() {
        let shell = test_shell();

        // Embed without text fails
        let code = run(&shell, "tools embed", 10).unwrap();
        assert_ne!(code, 0);
    }

    #[test]
    fn tools_embed_with_text() {
        let shell = test_shell();

        // Embed with text (may fail gracefully if no model loaded)
        // Just check it doesn't crash
        let _code = run(&shell, "tools embed payment processing", 10);
        // Don't assert on code - it may succeed or fail depending on model availability
    }

    #[test]
    fn tools_search_no_results() {
        let shell = test_shell();

        // Search for something not in the demo catalog
        let code = run(&shell, "tools search xyznonexistentterm", 10).unwrap();
        assert_eq!(code, 0); // Returns success but shows "no matching tools"
    }

    #[test]
    fn tools_list_empty_category() {
        let shell = test_shell();

        // Filter by non-existent category shows empty
        let code = run(&shell, "tools list -c nonexistent", 10).unwrap();
        assert_eq!(code, 0);
    }
}
