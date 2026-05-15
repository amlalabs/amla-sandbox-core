//! Command execution context.
//!
//! Each command task gets its own context with its own I/O handles.
//! No shared mutable state between tasks.

use std::cell::RefCell;
use std::rc::Rc;

use amla_scheduler::Scheduler;
use amla_tools::ToolCatalog;
use amla_vfs::{Permission, Vfs};

use crate::env::Environment;
use crate::io_handle::{DirEntry, IoError, IoHandle, Stat};

/// Execution context for a command.
///
/// Each task gets its own context with its own I/O handles.
/// No shared mutable state between tasks.
pub struct CmdContext {
    /// Command arguments (argv\[0\] is command name).
    pub argv: Vec<String>,

    /// Standard input.
    pub stdin: IoHandle,

    /// Standard output.
    pub stdout: IoHandle,

    /// Standard error.
    pub stderr: IoHandle,

    /// Current working directory (for this context).
    cwd: String,

    /// Environment variables (clone for this context).
    env: Environment,

    /// VFS reference (for file operations).
    vfs: Rc<RefCell<Vfs>>,

    /// Scheduler for timer operations (sleep, now).
    scheduler: Scheduler,

    /// Tool catalog for the `tools` command (optional).
    tool_catalog: Option<Rc<ToolCatalog>>,
}

impl CmdContext {
    /// Create a new command context.
    pub fn new(
        argv: Vec<String>,
        stdin: IoHandle,
        stdout: IoHandle,
        stderr: IoHandle,
        cwd: String,
        env: Environment,
        vfs: Rc<RefCell<Vfs>>,
        scheduler: Scheduler,
        tool_catalog: Option<Rc<ToolCatalog>>,
    ) -> Self {
        Self {
            argv,
            stdin,
            stdout,
            stderr,
            cwd,
            env,
            vfs,
            scheduler,
            tool_catalog,
        }
    }

    /// Get the tool catalog (if set).
    pub fn tool_catalog(&self) -> Option<Rc<ToolCatalog>> {
        self.tool_catalog.clone()
    }

    // =========================================================================
    // Argument helpers
    // =========================================================================

    /// Get the command name.
    pub fn command_name(&self) -> &str {
        self.argv.first().map_or("", std::string::String::as_str)
    }

    /// Get arguments (excluding command name).
    pub fn args(&self) -> &[String] {
        if self.argv.is_empty() {
            &[]
        } else {
            &self.argv[1..]
        }
    }

    /// Create a lexopt parser for the arguments.
    ///
    /// Note: Uses `from_iter` which treats argv\[0\] as the binary name and skips it.
    pub fn arg_parser(&self) -> lexopt::Parser {
        lexopt::Parser::from_iter(self.argv.iter().map(std::string::String::as_str))
    }

    // =========================================================================
    // Environment helpers
    // =========================================================================

    /// Get the current working directory.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Get an environment variable.
    pub fn getenv(&self, name: &str) -> Option<&str> {
        self.env.get(name)
    }

    /// Iterate over environment variables.
    pub fn env_iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.env.iter()
    }

    /// Clone the environment (for creating subshells).
    pub fn env_clone(&self) -> Environment {
        self.env.clone()
    }

    // =========================================================================
    // Path helpers
    // =========================================================================

    /// Resolve a path relative to the current working directory.
    pub fn resolve_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            normalize_path(path)
        } else {
            let combined = if self.cwd.ends_with('/') {
                format!("{}{}", self.cwd, path)
            } else {
                format!("{}/{}", self.cwd, path)
            };
            normalize_path(&combined)
        }
    }

    // =========================================================================
    // I/O helpers
    // =========================================================================

    /// Read from stdin.
    pub async fn read_stdin(&self, buf: &mut [u8]) -> Result<usize, IoError> {
        self.stdin.read(buf).await
    }

    /// Write to stdout.
    pub async fn write_stdout(&self, buf: &[u8]) -> Result<usize, IoError> {
        self.stdout.write(buf).await
    }

    /// Write to stderr.
    pub async fn write_stderr(&self, buf: &[u8]) -> Result<usize, IoError> {
        self.stderr.write(buf).await
    }

    /// Write all bytes to stdout.
    pub async fn stdout_write_all(&self, buf: &[u8]) -> Result<(), IoError> {
        self.stdout.write_all(buf).await
    }

    /// Write all bytes to stderr.
    pub async fn stderr_write_all(&self, buf: &[u8]) -> Result<(), IoError> {
        self.stderr.write_all(buf).await
    }

    /// Print a line to stdout.
    pub async fn println(&self, s: &str) -> Result<(), IoError> {
        self.stdout_write_all(s.as_bytes()).await?;
        self.stdout_write_all(b"\n").await?;
        Ok(())
    }

    /// Print an error message to stderr.
    pub async fn eprintln(&self, s: &str) -> Result<(), IoError> {
        self.stderr_write_all(s.as_bytes()).await?;
        self.stderr_write_all(b"\n").await?;
        Ok(())
    }

    // =========================================================================
    // VFS helpers (for commands like cat, ls)
    // =========================================================================

    /// Open a file for reading.
    ///
    /// For mounted files (external host files), this fetches the content via a
    /// `FileRead` host op and returns a buffer-backed handle. For regular VFS
    /// files, this returns a VFS-backed handle.
    pub async fn open_read(&self, path: &str) -> Result<IoHandle, IoError> {
        let path = self.resolve_path(path);

        // Check if this path is mounted (points to a host file)
        // Extract host_path before await to avoid holding RefCell borrow across await
        let host_path = self.vfs.borrow().get_host_path(&path).map(String::from);

        if let Some(host_path) = host_path {
            // Mounted file - fetch via host op and return as buffer
            let data = self
                .scheduler
                .host()
                .file_read(&host_path)
                .await
                .map_err(|e| IoError::Io(format!("host read failed: {e}")))?;
            Ok(IoHandle::from_bytes(data))
        } else {
            // Regular VFS file
            IoHandle::open_read(Rc::clone(&self.vfs), path)
        }
    }

    /// Open a file for reading, or return stdin if path is "-".
    ///
    /// This implements the POSIX convention where "-" represents stdin.
    /// Centralizes the common pattern found in many commands like cat, head, grep, etc.
    pub async fn open_or_stdin(&self, path: &str) -> Result<IoHandle, IoError> {
        if path == "-" {
            Ok(self.stdin.clone())
        } else {
            self.open_read(path).await
        }
    }

    /// Open a file for writing (creates/truncates).
    pub fn open_write(&self, path: &str) -> Result<IoHandle, IoError> {
        let path = self.resolve_path(path);
        IoHandle::open_write(Rc::clone(&self.vfs), path)
    }

    /// Open a file for appending.
    pub fn open_append(&self, path: &str) -> Result<IoHandle, IoError> {
        let path = self.resolve_path(path);
        IoHandle::open_append(Rc::clone(&self.vfs), path)
    }

    /// Read entire file into bytes.
    ///
    /// For mounted files (external host files), this issues a `FileRead` host op
    /// to fetch the content from the host filesystem. For regular VFS files,
    /// this reads directly from the in-memory VFS.
    pub async fn read_file(&self, path: &str) -> Result<Vec<u8>, IoError> {
        let path = self.resolve_path(path);

        // Check if this path is mounted (points to a host file)
        // Extract host_path before await to avoid holding RefCell borrow across await
        let host_path = self.vfs.borrow().get_host_path(&path).map(String::from);

        if let Some(host_path) = host_path {
            // Mounted file - fetch via host op
            let data = self
                .scheduler
                .host()
                .file_read(&host_path)
                .await
                .map_err(|e| IoError::Io(format!("host read failed: {e}")))?;
            Ok(data)
        } else {
            // Regular VFS file - read directly
            Ok(self.vfs.borrow().read_file(&path)?)
        }
    }

    /// Write bytes to file (create/truncate).
    pub fn write_file(&self, path: &str, content: &[u8]) -> Result<(), IoError> {
        let path = self.resolve_path(path);
        self.vfs
            .borrow_mut()
            .write_file(&path, content, Permission::ReadWrite)?;
        Ok(())
    }

    /// Append bytes to file (create if not exists).
    pub fn append_file(&self, path: &str, content: &[u8]) -> Result<(), IoError> {
        let path = self.resolve_path(path);
        self.vfs.borrow_mut().append_file(&path, content)?;
        Ok(())
    }

    /// List directory contents.
    pub fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, IoError> {
        let path = self.resolve_path(path);
        let vfs = self.vfs.borrow();

        if !vfs.exists(&path) {
            return Err(IoError::NotFound(path));
        }
        if !vfs.is_dir(&path) {
            return Err(IoError::NotDir(path));
        }

        let entries = vfs.list_dir(&path)?;
        Ok(entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                is_dir: e.is_dir,
            })
            .collect())
    }

    /// Get file status.
    ///
    /// For mounted files, size is reported as 0 (actual size requires async read).
    pub fn stat(&self, path: &str) -> Result<Stat, IoError> {
        let path = self.resolve_path(path);
        let vfs = self.vfs.borrow();

        // Check if it's a mounted file first
        if vfs.is_mounted(&path) {
            // Mounted files: size unknown without async read
            return Ok(Stat {
                size: 0, // Size unknown for mounted files without reading
                is_dir: false,
                is_file: true,
            });
        }

        if !vfs.exists(&path) {
            return Err(IoError::NotFound(path));
        }

        let is_dir = vfs.is_dir(&path);
        let is_file = vfs.is_file(&path);
        let size = if is_file {
            vfs.read_file(&path).map(|c| c.len() as u64).unwrap_or(0)
        } else {
            0
        };

        Ok(Stat {
            size,
            is_dir,
            is_file,
        })
    }

    /// Check if path exists.
    ///
    /// Returns true for both regular VFS files and mounted host files.
    pub fn exists(&self, path: &str) -> bool {
        let path = self.resolve_path(path);
        let vfs = self.vfs.borrow();
        // Check VFS first, then check if it's a mount point
        vfs.exists(&path) || vfs.is_mounted(&path)
    }

    /// Check if path is a directory.
    pub fn is_dir(&self, path: &str) -> bool {
        let path = self.resolve_path(path);
        self.vfs.borrow().is_dir(&path)
    }

    /// Check if path is a file.
    ///
    /// Returns true for both regular VFS files and mounted host files.
    pub fn is_file(&self, path: &str) -> bool {
        let path = self.resolve_path(path);
        let vfs = self.vfs.borrow();
        // Check VFS first, then check if it's a mount point (mounts are file-to-file)
        vfs.is_file(&path) || vfs.is_mounted(&path)
    }

    /// Create a directory.
    pub fn mkdir(&self, path: &str) -> Result<(), IoError> {
        let path = self.resolve_path(path);
        self.vfs
            .borrow_mut()
            .create_dir(&path, Permission::ReadWrite)?;
        Ok(())
    }

    /// Create directory and parents.
    pub fn mkdir_p(&self, path: &str) -> Result<(), IoError> {
        let path = self.resolve_path(path);
        self.vfs
            .borrow_mut()
            .create_dir_all(&path, Permission::ReadWrite)?;
        Ok(())
    }

    /// Remove a file or empty directory.
    pub fn remove(&self, path: &str) -> Result<(), IoError> {
        let path = self.resolve_path(path);
        self.vfs.borrow_mut().remove(&path)?;
        Ok(())
    }

    /// Remove file or directory recursively.
    pub fn remove_recursive(&self, path: &str) -> Result<(), IoError> {
        let path = self.resolve_path(path);
        self.vfs.borrow_mut().remove_recursive(&path)?;
        Ok(())
    }

    /// Get VFS reference (for advanced operations).
    pub fn vfs(&self) -> Rc<RefCell<Vfs>> {
        Rc::clone(&self.vfs)
    }

    // =========================================================================
    // Scheduler helpers (for timer operations)
    // =========================================================================

    /// Get the current monotonic time (synchronous - no host op required).
    ///
    /// Monotonic time is suitable for measuring durations and scheduling.
    pub fn now(&self) -> amla_scheduler::TimeNanos {
        self.scheduler.now_monotonic()
    }

    /// Get the current realtime (wall clock) time.
    ///
    /// Realtime is suitable for displaying timestamps to users.
    pub fn now_realtime(&self) -> amla_scheduler::TimeNanos {
        self.scheduler.now_realtime()
    }

    /// Sleep until the specified deadline.
    pub async fn sleep_until(&self, deadline: amla_scheduler::TimeNanos) -> Result<(), IoError> {
        self.scheduler
            .sleep_until(deadline)
            .await
            .map_err(|e| IoError::Io(e.to_string()))
    }

    /// Sleep for the specified duration.
    pub async fn sleep(&self, duration: std::time::Duration) -> Result<(), IoError> {
        let now = self.scheduler.now_monotonic();
        let deadline = now + amla_scheduler::duration_to_nanos(duration);
        self.scheduler
            .sleep_until(deadline)
            .await
            .map_err(|e| IoError::Io(e.to_string()))
    }

    /// Get a clone of the scheduler (for spawning sub-tasks).
    pub fn scheduler(&self) -> Scheduler {
        self.scheduler.clone()
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

    fn test_scheduler() -> Scheduler {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    #[test]
    fn resolve_path_absolute() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let scheduler = test_scheduler();
        let ctx = CmdContext::new(
            vec!["test".to_string()],
            IoHandle::null(),
            IoHandle::null(),
            IoHandle::null(),
            "/home/user".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        );

        assert_eq!(ctx.resolve_path("/etc/passwd"), "/etc/passwd");
    }

    #[test]
    fn resolve_path_relative() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let scheduler = test_scheduler();
        let ctx = CmdContext::new(
            vec!["test".to_string()],
            IoHandle::null(),
            IoHandle::null(),
            IoHandle::null(),
            "/home/user".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        );

        assert_eq!(ctx.resolve_path("file.txt"), "/home/user/file.txt");
    }

    #[test]
    fn resolve_path_dot() {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let scheduler = test_scheduler();
        let ctx = CmdContext::new(
            vec!["test".to_string()],
            IoHandle::null(),
            IoHandle::null(),
            IoHandle::null(),
            "/home/user".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        );

        assert_eq!(ctx.resolve_path("."), "/home/user");
        assert_eq!(ctx.resolve_path(".."), "/home");
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
}
