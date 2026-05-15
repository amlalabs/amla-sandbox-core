//! # amla-vfs
//!
//! In-memory virtual filesystem for AI agent sandboxing.
//!
//! Provides a simple, permission-controlled filesystem abstraction with:
//! - **Read-only** areas (e.g., `/tools/`, `/bin/`)
//! - **Read-write** areas (e.g., `/workspace/`)
//! - **Append-only** areas (e.g., `/log/`)
//!
//! ## Features
//!
//! - Path normalization and validation
//! - Directory creation with automatic parent creation
//! - Glob pattern matching
//! - No async - designed for WASM/embedded use
//!
//! ## Security Architecture
//!
//! **CRITICAL: All mutations go through a single permission chokepoint.**
//!
//! The `check_mutation()` method is the **ONLY** place where permission checks
//! should be performed. This ensures consistent enforcement and prevents bypass.
//!
//! If you're adding a new mutation method:
//! 1. **ALWAYS** call `check_mutation()` before modifying `self.entries`
//! 2. **NEVER** inline permission checks - use the chokepoint
//! 3. Add tests that verify permission denial
//!
//! ## Example
//!
//! ```rust
//! use amla_vfs::{Vfs, Permission};
//!
//! // Create a new VFS with standard directory structure
//! let mut vfs = Vfs::new();
//!
//! // Write a file
//! vfs.write_file("/workspace/notes.txt", b"Hello, world!", Permission::ReadWrite).unwrap();
//!
//! // Read it back
//! let content = vfs.read_file_string("/workspace/notes.txt").unwrap();
//! assert_eq!(content, "Hello, world!");
//!
//! // List directory contents (includes README.md from bootstrap)
//! let entries = vfs.list_dir("/workspace").unwrap();
//! assert_eq!(entries.len(), 2);
//! assert!(entries.iter().any(|e| e.name == "notes.txt"));
//! ```
//!
//! ## Standard Directory Structure
//!
//! When created with `Vfs::new()`, the following directories are initialized:
//!
//! | Path | Permission | Purpose |
//! |------|------------|---------|
//! | `/` | ReadOnly | Root |
//! | `/tools/` | ReadOnly | Auto-generated tool stubs |
//! | `/workspace/` | ReadWrite | Agent scratch space |
//! | `/log/` | AppendOnly | Observability logs |
//! | `/bin/` | ReadOnly | Shell command metadata |

// missing_docs lint inherited from workspace
#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

mod async_vfs;
mod block_store;
mod file_handle;
mod lazy_block_store;
mod page_cache;

pub use async_vfs::{AsyncVfs, Fd};
pub use block_store::{BLOCK_SIZE, BlockStore, MemoryBlockStore};
pub use file_handle::{FileHandle, OpenMode};
pub use lazy_block_store::LazyBlockStore;
pub use page_cache::{FetchAction, PageCache};

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::path::{Component, Path};
use thiserror::Error;

/// VFS error types.
#[derive(Debug, Error)]
pub enum VfsError {
    /// File or directory not found.
    #[error("File not found: {0}")]
    NotFound(String),

    /// Permission denied for operation.
    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    /// Path is not a directory.
    #[error("Not a directory: {0}")]
    NotADirectory(String),

    /// Path is not a file.
    #[error("Not a file: {0}")]
    NotAFile(String),

    /// Path already exists.
    #[error("Path exists: {0}")]
    AlreadyExists(String),

    /// Invalid path format.
    #[error("Invalid path: {0}")]
    InvalidPath(String),
}

/// File permissions controlling allowed operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Permission {
    /// Read-only (e.g., /tools/, /bin/)
    ReadOnly,
    /// Read-write (e.g., /workspace/)
    ReadWrite,
    /// Append-only (e.g., /log/)
    AppendOnly,
}

/// File or directory entry in the VFS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Entry {
    /// A file with content and permission.
    File {
        /// The file's binary content.
        content: Vec<u8>,
        /// The file's permission level.
        permission: Permission,
    },
    /// A directory with permission.
    Directory {
        /// The directory's permission level.
        permission: Permission,
    },
}

impl Entry {
    /// Returns true if this entry is a file.
    pub fn is_file(&self) -> bool {
        matches!(self, Entry::File { .. })
    }

    /// Returns true if this entry is a directory.
    pub fn is_dir(&self) -> bool {
        matches!(self, Entry::Directory { .. })
    }

    /// Returns the permission for this entry.
    pub fn permission(&self) -> Permission {
        match self {
            Entry::File { permission, .. } | Entry::Directory { permission } => *permission,
        }
    }
}

/// Directory entry returned by `list_dir`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    /// Filename (not full path).
    pub name: String,
    /// Full path to the entry.
    pub path: String,
    /// True if this is a directory.
    pub is_dir: bool,
    /// Permission for this entry.
    pub permission: Permission,
}

// =============================================================================
// MUTATION OPERATION TYPES
// =============================================================================

/// Operation types for permission checking.
///
/// Used by the central `check_mutation()` chokepoint to determine
/// what kind of access is being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationOp {
    /// Creating a new file or directory
    Create,
    /// Overwriting an existing file
    Overwrite,
    /// Appending to an existing file
    Append,
    /// Deleting a file or directory
    Delete,
}

/// In-memory virtual filesystem.
///
/// Provides a simple filesystem abstraction with permission control,
/// designed for sandboxing AI agent code execution.
#[derive(Debug, Default)]
pub struct Vfs {
    /// Path -> Entry mapping
    entries: HashMap<String, Entry>,
    /// Mounted paths: `sandbox_path` -> `host_path`.
    /// When a mounted path is accessed, the runtime issues a `FileRead` host op.
    mounts: HashMap<String, String>,
}

impl Vfs {
    /// Create a new VFS with standard directory structure.
    ///
    /// Creates the following directories:
    /// - `/` (`ReadOnly`)
    /// - `/tools/` (`ReadOnly`)
    /// - `/workspace/` (`ReadWrite`)
    /// - `/log/` (`AppendOnly`)
    /// - `/bin/` (`ReadOnly`)
    ///
    /// Also initializes `/log/actions.jsonl` as an empty append-only file.
    pub fn new() -> Self {
        let mut vfs = Self {
            entries: HashMap::new(),
            mounts: HashMap::new(),
        };

        // Bootstrap: directly insert entries (bypasses permission checks)
        // This is safe because we're setting up the initial structure
        vfs.entries.insert(
            "/".to_string(),
            Entry::Directory {
                permission: Permission::ReadOnly,
            },
        );
        vfs.entries.insert(
            "/tools".to_string(),
            Entry::Directory {
                permission: Permission::ReadOnly,
            },
        );
        vfs.entries.insert(
            "/workspace".to_string(),
            Entry::Directory {
                permission: Permission::ReadWrite,
            },
        );
        vfs.entries.insert(
            "/tmp".to_string(),
            Entry::Directory {
                permission: Permission::ReadWrite,
            },
        );
        vfs.entries.insert(
            "/log".to_string(),
            Entry::Directory {
                permission: Permission::AppendOnly,
            },
        );
        vfs.entries.insert(
            "/bin".to_string(),
            Entry::Directory {
                permission: Permission::ReadOnly,
            },
        );

        // Initialize /log/actions.jsonl as empty
        vfs.entries.insert(
            "/log/actions.jsonl".to_string(),
            Entry::File {
                content: vec![],
                permission: Permission::AppendOnly,
            },
        );

        // Initialize /workspace/README.md with welcome content
        vfs.entries.insert(
            "/workspace/README.md".to_string(),
            Entry::File {
                content: b"# Workspace\n\nThis is your sandbox workspace. Files here persist for the session.\n\nTry:\n- `echo 'hello' > test.txt` - create a file\n- `cat README.md` - read this file\n- `ls -la` - list directory contents\n".to_vec(),
                permission: Permission::ReadWrite,
            },
        );

        vfs
    }

    /// Create an empty VFS without standard directories.
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            mounts: HashMap::new(),
        }
    }

    /// Set up path mounts from host filesystem to sandbox.
    ///
    /// This method:
    /// 1. Creates read-only parent directories for each mounted path
    /// 2. Stores the mount mapping (`sandbox_path` -> `host_path`)
    ///
    /// **Security**: All mounted paths are read-only. The actual file
    /// content is fetched via `HostChannel` when accessed through
    /// `LazyBlockStore`.
    ///
    /// # Arguments
    ///
    /// * `mounts` - Iterator of (`host_path`, `sandbox_path`) pairs
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mounts = [
    ///     ("/host/data/config.json", "/data/config.json"),
    ///     ("/host/data/input.txt", "/data/input.txt"),
    /// ];
    /// vfs.setup_mounts(mounts.iter().map(|(h, s)| (h.to_string(), s.to_string())));
    /// ```
    pub fn setup_mounts(
        &mut self,
        mounts: impl IntoIterator<Item = (String, String)>,
    ) -> Result<(), VfsError> {
        for (host_path, sandbox_path) in mounts {
            // Create parent directories if needed (read-only to prevent siblings)
            if let Some(parent) = Path::new(&sandbox_path).parent().and_then(|p| p.to_str())
                && !parent.is_empty()
                && parent != "/"
            {
                self.insert_dir_all(parent, Permission::ReadOnly)?;
            }

            // Store the mount mapping
            let normalized = Self::normalize(&sandbox_path)?;
            self.mounts.insert(normalized, host_path);
        }
        Ok(())
    }

    /// Check if a sandbox path has a host mount.
    pub fn is_mounted(&self, sandbox_path: &str) -> bool {
        Self::normalize(sandbox_path).is_ok_and(|p| self.mounts.contains_key(&p))
    }

    /// Get the host path for a mounted sandbox path.
    ///
    /// Returns `None` if the path is not mounted.
    pub fn get_host_path(&self, sandbox_path: &str) -> Option<&str> {
        Self::normalize(sandbox_path)
            .ok()
            .and_then(|p| self.mounts.get(&p))
            .map(String::as_str)
    }

    /// Get all mounted paths (`sandbox_path` -> `host_path`).
    pub fn mounts(&self) -> &HashMap<String, String> {
        &self.mounts
    }

    /// Insert a file directly, bypassing the permission chokepoint.
    ///
    /// # Security Warning
    ///
    /// This method bypasses `check_mutation()` and should **ONLY** be used for:
    /// - Runtime bootstrap (e.g., populating `/tools/` with generated stubs)
    /// - System initialization before any untrusted code runs
    ///
    /// **NEVER** call this with paths derived from user/agent input.
    /// Normal agent code should use `write_file()` which enforces permissions.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to insert (must be valid and normalized)
    /// * `content` - File content
    /// * `permission` - Permission for the new file
    pub fn insert_file(
        &mut self,
        path: &str,
        content: &[u8],
        permission: Permission,
    ) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;

        // Ensure parent exists (create as directory if needed)
        if let Some(parent) = Self::parent_of(&path)
            && !self.entries.contains_key(&parent)
        {
            // Auto-create parent directories with same permission
            self.insert_dir_all(&parent, permission)?;
        }

        self.entries.insert(
            path,
            Entry::File {
                content: content.to_vec(),
                permission,
            },
        );
        Ok(())
    }

    /// Insert a directory directly, bypassing the permission chokepoint.
    ///
    /// # Security Warning
    ///
    /// This method bypasses `check_mutation()`. See [`Vfs::insert_file`] for details.
    pub fn insert_dir(&mut self, path: &str, permission: Permission) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;
        self.entries.insert(path, Entry::Directory { permission });
        Ok(())
    }

    /// Insert a directory and all parents, bypassing the permission chokepoint.
    ///
    /// # Security Warning
    ///
    /// This method bypasses `check_mutation()`. See [`Vfs::insert_file`] for details.
    pub fn insert_dir_all(&mut self, path: &str, permission: Permission) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;
        if path == "/" {
            return Ok(());
        }

        let parts: SmallVec<[&str; 8]> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = String::new();

        for part in parts {
            current = format!("{current}/{part}");
            if !self.entries.contains_key(&current) {
                self.entries
                    .insert(current.clone(), Entry::Directory { permission });
            }
        }
        Ok(())
    }

    /// Normalize and canonicalize a path using `std::path`.
    ///
    /// - Ensures leading slash
    /// - Removes trailing slashes
    /// - Collapses `.` (current dir) and `..` (parent dir)
    /// - Prevents escaping the root directory
    fn normalize_path(path: &str) -> Result<String, VfsError> {
        let path = path.trim();
        if path.is_empty() || path == "/" {
            return Ok("/".to_string());
        }

        // Use std::path::Path for robust component handling
        let p = Path::new(path);
        let mut components: SmallVec<[&str; 8]> = SmallVec::new();

        for component in p.components() {
            match component {
                Component::RootDir | Component::CurDir | Component::Prefix(_) => {
                    // Skip root, current dir, and Windows prefixes
                }
                Component::ParentDir => {
                    // Go up one level, but not above root
                    if components.pop().is_none() {
                        return Err(VfsError::InvalidPath(format!("path escapes root: {path}")));
                    }
                }
                Component::Normal(s) => {
                    let s = s.to_str().ok_or_else(|| {
                        VfsError::InvalidPath(format!("invalid UTF-8 in path: {path}"))
                    })?;
                    // Reject null bytes
                    if s.contains('\0') {
                        return Err(VfsError::InvalidPath(format!(
                            "invalid characters in path: {path}"
                        )));
                    }
                    components.push(s);
                }
            }
        }

        if components.is_empty() {
            Ok("/".to_string())
        } else {
            // Always use forward slashes (VFS is Unix-style regardless of platform)
            Ok(format!("/{}", components.join("/")))
        }
    }

    /// Normalize path, returning error on invalid paths.
    fn normalize(path: &str) -> Result<String, VfsError> {
        Self::normalize_path(path)
    }

    // =========================================================================
    // PERMISSION CHOKEPOINT - All mutations go through here
    // =========================================================================
    //
    // ⚠️  SECURITY-CRITICAL: DO NOT BYPASS THIS CHOKEPOINT  ⚠️
    //
    // Every public method that modifies self.entries MUST call check_mutation()
    // BEFORE making any changes. This is the single point of enforcement for:
    //   - Read-only protection
    //   - Append-only protection
    //   - Parent directory permission checks
    //
    // If you're adding a new mutation:
    //   1. Call check_mutation() with the appropriate MutationOp
    //   2. Only modify self.entries if check_mutation() returns Ok(())
    //   3. Add a test that verifies permission denial
    //
    // The insert_* methods bypass this for bootstrap ONLY - they should never
    // be called with untrusted paths or during normal agent execution.
    // =========================================================================

    /// Check if a mutation is allowed at the given path.
    ///
    /// This is the **single chokepoint** for all permission checks.
    /// Every mutating operation MUST call this before modifying state.
    ///
    /// # Rules
    ///
    /// 1. **Parent must exist and be a writable directory** (for Create)
    /// 2. **Target permission determines allowed operations:**
    ///    - `ReadOnly`: no mutations allowed
    ///    - `AppendOnly`: only `Append` allowed
    ///    - `ReadWrite`: `Create`, `Overwrite`, `Append`, `Delete` allowed
    /// 3. **Cannot create over existing entry** (use Overwrite for that)
    /// 4. **Delete requires parent to be writable** (for directories)
    fn check_mutation(&self, path: &str, op: MutationOp) -> Result<(), VfsError> {
        // Check if entry already exists
        let existing = self.entries.get(path);

        match op {
            MutationOp::Create => {
                // Cannot create if already exists
                if existing.is_some() {
                    return Err(VfsError::AlreadyExists(path.to_string()));
                }
                // Parent must be a writable directory
                self.check_parent_writable(path)?;
            }

            MutationOp::Overwrite => {
                match existing {
                    Some(entry) => {
                        // Check target permission
                        match entry.permission() {
                            Permission::ReadOnly => {
                                return Err(VfsError::PermissionDenied(format!(
                                    "cannot overwrite read-only: {path}"
                                )));
                            }
                            Permission::AppendOnly => {
                                return Err(VfsError::PermissionDenied(format!(
                                    "cannot overwrite append-only file: {path}"
                                )));
                            }
                            Permission::ReadWrite => {}
                        }
                        // Cannot overwrite directory with file
                        if entry.is_dir() {
                            return Err(VfsError::NotAFile(path.to_string()));
                        }
                    }
                    None => {
                        // Doesn't exist - treat as Create
                        self.check_parent_writable(path)?;
                    }
                }
            }

            MutationOp::Append => match existing {
                Some(entry) => {
                    if entry.is_dir() {
                        return Err(VfsError::NotAFile(path.to_string()));
                    }
                    match entry.permission() {
                        Permission::ReadOnly => {
                            return Err(VfsError::PermissionDenied(format!(
                                "cannot append to read-only: {path}"
                            )));
                        }
                        Permission::AppendOnly | Permission::ReadWrite => {}
                    }
                }
                None => {
                    return Err(VfsError::NotFound(path.to_string()));
                }
            },

            MutationOp::Delete => {
                match existing {
                    Some(entry) => {
                        // Check entry permission - cannot delete read-only or append-only
                        match entry.permission() {
                            Permission::ReadOnly => {
                                return Err(VfsError::PermissionDenied(format!(
                                    "cannot delete read-only: {path}"
                                )));
                            }
                            Permission::AppendOnly => {
                                return Err(VfsError::PermissionDenied(format!(
                                    "cannot delete append-only: {path}"
                                )));
                            }
                            Permission::ReadWrite => {}
                        }
                        // Also check parent is writable
                        self.check_parent_writable(path)?;
                    }
                    None => {
                        return Err(VfsError::NotFound(path.to_string()));
                    }
                }
            }
        }

        Ok(())
    }

    /// Check that the parent directory exists and is writable.
    fn check_parent_writable(&self, path: &str) -> Result<(), VfsError> {
        if let Some(parent) = Self::parent_of(path) {
            match self.entries.get(&parent) {
                Some(Entry::Directory { permission }) => {
                    if *permission == Permission::ReadOnly {
                        return Err(VfsError::PermissionDenied(format!(
                            "parent directory is read-only: {parent}"
                        )));
                    }
                    // AppendOnly directories don't allow creating new files
                    if *permission == Permission::AppendOnly {
                        return Err(VfsError::PermissionDenied(format!(
                            "parent directory is append-only: {parent}"
                        )));
                    }
                }
                Some(_) => {
                    return Err(VfsError::NotADirectory(parent));
                }
                None => {
                    return Err(VfsError::NotFound(parent));
                }
            }
        }
        // Root path or "/" - always allowed
        Ok(())
    }

    /// Get parent directory of a path (assumes already normalized).
    fn parent_of(normalized: &str) -> Option<String> {
        if normalized == "/" {
            return None;
        }

        if let Some(idx) = normalized.rfind('/') {
            if idx == 0 {
                Some("/".to_string())
            } else {
                Some(normalized[..idx].to_string())
            }
        } else {
            None
        }
    }

    /// Create a directory.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path already exists
    /// - The parent directory doesn't exist or isn't a directory
    /// - The parent directory is read-only
    pub fn create_dir(&mut self, path: &str, permission: Permission) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;

        // All permission checks go through the chokepoint
        self.check_mutation(&path, MutationOp::Create)?;

        self.entries.insert(path, Entry::Directory { permission });
        Ok(())
    }

    /// Create a directory and all parent directories.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Any parent path is read-only
    /// - Any parent path exists but is not a directory
    pub fn create_dir_all(&mut self, path: &str, permission: Permission) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;
        if path == "/" {
            return Ok(());
        }

        let parts: SmallVec<[&str; 8]> = path.split('/').filter(|s| !s.is_empty()).collect();
        let mut current = String::new();

        for part in parts {
            current = format!("{current}/{part}");

            if self.entries.contains_key(&current) {
                // Exists - make sure it's a directory
                if let Some(entry) = self.entries.get(&current)
                    && !entry.is_dir()
                {
                    return Err(VfsError::NotADirectory(current));
                }
            } else {
                // All permission checks go through the chokepoint
                self.check_mutation(&current, MutationOp::Create)?;
                self.entries
                    .insert(current.clone(), Entry::Directory { permission });
            }
        }

        Ok(())
    }

    /// Write a file.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path is an existing read-only file
    /// - The path is a directory
    /// - The parent directory is read-only
    /// - The parent directory doesn't exist
    pub fn write_file(
        &mut self,
        path: &str,
        content: &[u8],
        permission: Permission,
    ) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;

        // All permission checks go through the chokepoint
        // Overwrite handles both new files (checks parent) and existing files (checks target)
        self.check_mutation(&path, MutationOp::Overwrite)?;

        self.entries.insert(
            path,
            Entry::File {
                content: content.to_vec(),
                permission,
            },
        );
        Ok(())
    }

    /// Append to a file (only for append-only or read-write files).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file doesn't exist
    /// - The file is read-only
    /// - The path is a directory
    pub fn append_file(&mut self, path: &str, content: &[u8]) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;

        // All permission checks go through the chokepoint
        self.check_mutation(&path, MutationOp::Append)?;

        // Safe to unwrap: check_mutation verified file exists and is appendable
        if let Some(Entry::File {
            content: existing, ..
        }) = self.entries.get_mut(&path)
        {
            existing.extend_from_slice(content);
        }
        Ok(())
    }

    /// Read a file.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file doesn't exist
    /// - The path is a directory
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, VfsError> {
        let path = Self::normalize(path)?;

        match self.entries.get(&path) {
            Some(Entry::File { content, .. }) => Ok(content.clone()),
            Some(Entry::Directory { .. }) => Err(VfsError::NotAFile(path)),
            None => Err(VfsError::NotFound(path)),
        }
    }

    /// Read a file as a UTF-8 string.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file doesn't exist
    /// - The path is a directory
    /// - The content is not valid UTF-8
    pub fn read_file_string(&self, path: &str) -> Result<String, VfsError> {
        let content = self.read_file(path)?;
        String::from_utf8(content).map_err(|_| VfsError::InvalidPath(path.to_string()))
    }

    /// Check if a path exists.
    pub fn exists(&self, path: &str) -> bool {
        Self::normalize(path).is_ok_and(|p| self.entries.contains_key(&p))
    }

    /// Check if a path is a file.
    pub fn is_file(&self, path: &str) -> bool {
        Self::normalize(path)
            .is_ok_and(|p| matches!(self.entries.get(&p), Some(Entry::File { .. })))
    }

    /// Check if a path is a directory.
    pub fn is_dir(&self, path: &str) -> bool {
        Self::normalize(path)
            .is_ok_and(|p| matches!(self.entries.get(&p), Some(Entry::Directory { .. })))
    }

    /// List directory contents.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The directory doesn't exist
    /// - The path is not a directory
    pub fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>, VfsError> {
        let path = Self::normalize(path)?;

        // Check directory exists
        match self.entries.get(&path) {
            Some(Entry::Directory { .. }) => {}
            Some(_) => return Err(VfsError::NotADirectory(path.clone())),
            None => return Err(VfsError::NotFound(path.clone())),
        }

        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{path}/")
        };

        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for (entry_path, entry) in &self.entries {
            if entry_path == &path {
                continue;
            }

            // Check if this entry is a direct child
            if entry_path.starts_with(&prefix) {
                let remainder = &entry_path[prefix.len()..];
                // Only direct children (no more slashes)
                if !remainder.contains('/') && !remainder.is_empty() {
                    // Convert to owned String once, reuse for HashSet check and DirEntry
                    let name = remainder.to_string();
                    if seen.insert(name.clone()) {
                        entries.push(DirEntry {
                            name,
                            path: entry_path.clone(),
                            is_dir: entry.is_dir(),
                            permission: entry.permission(),
                        });
                    }
                }
            }
        }

        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Get entry metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the path doesn't exist.
    pub fn stat(&self, path: &str) -> Result<Entry, VfsError> {
        let path = Self::normalize(path)?;
        self.entries
            .get(&path)
            .cloned()
            .ok_or(VfsError::NotFound(path))
    }

    /// Delete a file or empty directory.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path doesn't exist
    /// - The path is read-only
    /// - The path is a non-empty directory
    pub fn remove(&mut self, path: &str) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;

        // All permission checks go through the chokepoint
        self.check_mutation(&path, MutationOp::Delete)?;

        // Additional check: directory must be empty (not a permission check)
        if self.is_dir(&path) {
            let children = self.list_dir(&path)?;
            if !children.is_empty() {
                return Err(VfsError::PermissionDenied(format!(
                    "{path}: directory not empty"
                )));
            }
        }

        self.entries.remove(&path);
        Ok(())
    }

    /// Delete a file or directory recursively.
    ///
    /// This is equivalent to `rm -r` in Unix.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path doesn't exist
    /// - Any path in the tree is read-only or append-only
    pub fn remove_recursive(&mut self, path: &str) -> Result<(), VfsError> {
        let path = Self::normalize(path)?;

        // Check if path exists
        if !self.entries.contains_key(&path) {
            return Err(VfsError::NotFound(path));
        }

        // Collect all paths to delete (depth-first order for proper deletion)
        let prefix = format!("{path}/");
        let mut to_delete: Vec<String> = self
            .entries
            .keys()
            .filter(|p| *p == &path || p.starts_with(&prefix))
            .cloned()
            .collect();

        // Sort by depth descending (deepest first)
        to_delete.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));

        // Check all permissions first (fail fast)
        for p in &to_delete {
            self.check_mutation(p, MutationOp::Delete)?;
        }

        // Delete all entries
        for p in to_delete {
            self.entries.remove(&p);
        }

        Ok(())
    }

    /// Get all paths matching a glob pattern.
    ///
    /// Supports:
    /// - `*` - matches any characters except `/`
    /// - `**` - matches any characters including `/`
    /// - `?` - matches any single character except `/`
    pub fn glob(&self, pattern: &str) -> Vec<String> {
        let Ok(pattern) = Self::normalize(pattern) else {
            return vec![];
        };

        // Convert glob pattern to regex
        // Use peekable iterator to avoid Vec allocation from chars().collect()
        let mut regex_pattern = String::new();
        let mut chars = pattern.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '*' {
                // Check for **
                if chars.peek() == Some(&'*') {
                    chars.next(); // consume second *
                    regex_pattern.push_str(".*");
                } else {
                    regex_pattern.push_str("[^/]*");
                }
            } else if c == '?' {
                regex_pattern.push_str("[^/]");
            } else if c == '.' {
                regex_pattern.push_str("\\.");
            } else if c == '/' || c == '-' || c == '_' || c.is_alphanumeric() {
                regex_pattern.push(c);
            } else {
                regex_pattern.push('\\');
                regex_pattern.push(c);
            }
        }

        let Ok(re) = regex::Regex::new(&format!("^{regex_pattern}$")) else {
            return vec![];
        };

        let mut matches: Vec<String> = self
            .entries
            .keys()
            .filter(|p| re.is_match(p))
            .cloned()
            .collect();
        matches.sort();
        matches
    }

    // =========================================================================
    // Permission Query API
    // =========================================================================

    /// Check if a write operation would be permitted at the given path.
    ///
    /// This is a **query-only** method that does not modify the VFS. It checks
    /// whether a write/overwrite operation would succeed based on current permissions.
    ///
    /// # Rules
    ///
    /// 1. If the path exists, it must be writable (`ReadWrite` permission).
    /// 2. If the path doesn't exist, its parent directory must be writable.
    /// 3. Cannot write to directories (only files).
    ///
    /// # Use Case
    ///
    /// This is used by the **async VFS chokepoint** in the runtime to validate
    /// `VfsWrite` host operations before exposing them to the host.
    ///
    /// # Example
    ///
    /// ```
    /// use amla_vfs::{Vfs, Permission};
    ///
    /// let mut vfs = Vfs::new();
    /// vfs.create_dir_all("/workspace", Permission::ReadWrite).unwrap();
    ///
    /// // Can write to workspace (parent is writable)
    /// assert!(vfs.can_write_to("/workspace/file.txt"));
    ///
    /// // Cannot write to tools (read-only)
    /// assert!(!vfs.can_write_to("/tools/malicious.sh"));
    /// ```
    pub fn can_write_to(&self, path: &str) -> bool {
        // Normalize path first
        let Ok(path) = Self::normalize(path) else {
            return false;
        };

        // Use check_mutation with Overwrite operation
        // This handles both existing files and new files in writable directories
        self.check_mutation(&path, MutationOp::Overwrite).is_ok()
    }

    /// Check if an append operation would be permitted at the given path.
    ///
    /// Similar to `can_write_to()`, but checks for append permission.
    /// Allows both `AppendOnly` and `ReadWrite` files.
    pub fn can_append_to(&self, path: &str) -> bool {
        let Ok(path) = Self::normalize(path) else {
            return false;
        };

        self.check_mutation(&path, MutationOp::Append).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // SECURITY TESTS - Permission enforcement
    // =========================================================================
    //
    // These tests verify that the VFS permission model is correctly enforced.
    // Any failure here indicates a potential security vulnerability.
    // =========================================================================

    #[test]
    fn test_vfs_create_dir() {
        let vfs = Vfs::new();
        assert!(vfs.is_dir("/tools"));
        assert!(vfs.is_dir("/workspace"));
        assert!(vfs.is_dir("/log"));
        assert!(vfs.is_dir("/bin"));
    }

    #[test]
    fn test_vfs_write_read_file() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/test.txt", b"hello world", Permission::ReadWrite)
            .unwrap();
        assert_eq!(
            vfs.read_file_string("/workspace/test.txt").unwrap(),
            "hello world"
        );
    }

    #[test]
    fn test_vfs_list_dir() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/a.txt", b"a", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/b.txt", b"b", Permission::ReadWrite)
            .unwrap();
        vfs.create_dir("/workspace/subdir", Permission::ReadWrite)
            .unwrap();

        let entries = vfs.list_dir("/workspace").unwrap();
        // 4 entries: README.md (from bootstrap) + a.txt + b.txt + subdir
        assert_eq!(entries.len(), 4);
        assert!(entries.iter().any(|e| e.name == "README.md"));
        assert!(entries.iter().any(|e| e.name == "a.txt"));
        assert!(entries.iter().any(|e| e.name == "b.txt"));
        assert!(entries.iter().any(|e| e.name == "subdir" && e.is_dir));
    }

    #[test]
    fn test_vfs_append_only() {
        let mut vfs = Vfs::new();
        vfs.append_file("/log/actions.jsonl", b"line1\n").unwrap();
        vfs.append_file("/log/actions.jsonl", b"line2\n").unwrap();
        assert_eq!(
            vfs.read_file_string("/log/actions.jsonl").unwrap(),
            "line1\nline2\n"
        );
    }

    #[test]
    fn test_vfs_permission_denied_overwrite() {
        let mut vfs = Vfs::new();
        // Write to workspace (writable)
        vfs.write_file("/workspace/test.js", b"code", Permission::ReadOnly)
            .unwrap();
        // Cannot overwrite read-only file
        assert!(
            vfs.write_file("/workspace/test.js", b"new code", Permission::ReadOnly)
                .is_err()
        );
    }

    #[test]
    fn test_vfs_write_to_readonly_dir_blocked() {
        let mut vfs = Vfs::new();
        // /tools is read-only - cannot create files there
        let result = vfs.write_file("/tools/malicious.js", b"code", Permission::ReadWrite);
        assert!(result.is_err());
        match result.unwrap_err() {
            VfsError::PermissionDenied(msg) => {
                assert!(msg.contains("read-only"));
            }
            e => panic!("Expected PermissionDenied, got {e:?}"),
        }
    }

    #[test]
    fn test_vfs_create_dir_in_readonly_blocked() {
        let mut vfs = Vfs::new();
        // /tools is read-only - cannot create subdirs there
        let result = vfs.create_dir("/tools/subdir", Permission::ReadWrite);
        assert!(result.is_err());
    }

    #[test]
    fn test_vfs_path_traversal_blocked() {
        let _vfs = Vfs::new();
        // Trying to escape root with .. should fail
        assert!(Vfs::normalize_path("/../etc/passwd").is_err());
        assert!(Vfs::normalize_path("/workspace/../../etc/passwd").is_err());
        assert!(Vfs::normalize_path("/..").is_err());
    }

    #[test]
    fn test_vfs_path_canonicalization() {
        // . is collapsed
        assert_eq!(
            Vfs::normalize_path("/workspace/./file.txt").unwrap(),
            "/workspace/file.txt"
        );
        // .. navigates up
        assert_eq!(
            Vfs::normalize_path("/workspace/subdir/../file.txt").unwrap(),
            "/workspace/file.txt"
        );
        // Multiple slashes collapsed
        assert_eq!(
            Vfs::normalize_path("/workspace//file.txt").unwrap(),
            "/workspace/file.txt"
        );
        // Valid .. that doesn't escape
        assert_eq!(Vfs::normalize_path("/a/b/c/../../d").unwrap(), "/a/d");
    }

    #[test]
    fn test_vfs_glob() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/test.txt", b"", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/test.js", b"", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/other.txt", b"", Permission::ReadWrite)
            .unwrap();

        let matches = vfs.glob("/workspace/*.txt");
        assert_eq!(matches.len(), 2);
        assert!(matches.contains(&"/workspace/test.txt".to_string()));
        assert!(matches.contains(&"/workspace/other.txt".to_string()));
    }

    #[test]
    fn test_vfs_empty() {
        let vfs = Vfs::empty();
        assert!(!vfs.exists("/"));
        assert!(!vfs.exists("/workspace"));
    }

    // =========================================================================
    // SECURITY: Read-only directory protection
    // =========================================================================

    #[test]
    fn test_security_cannot_write_file_to_readonly_dir() {
        let mut vfs = Vfs::new();
        // /tools is ReadOnly - cannot create files there
        let result = vfs.write_file("/tools/evil.js", b"malicious", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_security_cannot_create_dir_in_readonly_dir() {
        let mut vfs = Vfs::new();
        // /tools is ReadOnly - cannot create subdirs
        let result = vfs.create_dir("/tools/subdir", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_security_cannot_create_dir_all_through_readonly() {
        let mut vfs = Vfs::new();
        // Cannot create nested dirs under read-only parent
        let result = vfs.create_dir_all("/tools/deep/nested/path", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_security_cannot_delete_from_readonly_dir() {
        let mut vfs = Vfs::new();
        // Bootstrap a file in /tools (bypasses permission)
        vfs.insert_file("/tools/tool.json", b"{}", Permission::ReadOnly)
            .unwrap();

        // Cannot delete from read-only directory
        let result = vfs.remove("/tools/tool.json");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    // =========================================================================
    // SECURITY: Append-only protection
    // =========================================================================

    #[test]
    fn test_security_cannot_write_new_file_to_appendonly_dir() {
        let mut vfs = Vfs::new();
        // /log is AppendOnly - cannot create NEW files
        let result = vfs.write_file("/log/new.txt", b"data", Permission::AppendOnly);
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_security_cannot_overwrite_appendonly_file() {
        let mut vfs = Vfs::new();
        // /log/actions.jsonl is AppendOnly - cannot overwrite
        vfs.append_file("/log/actions.jsonl", b"line1\n").unwrap();
        let result = vfs.write_file("/log/actions.jsonl", b"replaced", Permission::AppendOnly);
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_security_cannot_delete_appendonly_file() {
        let mut vfs = Vfs::new();
        // /log/actions.jsonl is AppendOnly - cannot delete
        let result = vfs.remove("/log/actions.jsonl");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_security_can_append_to_appendonly_file() {
        let mut vfs = Vfs::new();
        // CAN append to append-only file
        vfs.append_file("/log/actions.jsonl", b"line1\n").unwrap();
        vfs.append_file("/log/actions.jsonl", b"line2\n").unwrap();
        let content = vfs.read_file_string("/log/actions.jsonl").unwrap();
        assert_eq!(content, "line1\nline2\n");
    }

    #[test]
    fn test_security_cannot_create_dir_in_appendonly_dir() {
        let mut vfs = Vfs::new();
        // Cannot create subdirs in append-only directory
        let result = vfs.create_dir("/log/subdir", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    // =========================================================================
    // SECURITY: Path traversal prevention
    // =========================================================================

    #[test]
    fn test_security_path_traversal_blocked() {
        // Cannot escape root with ..
        assert!(Vfs::normalize_path("/../etc/passwd").is_err());
        assert!(Vfs::normalize_path("/workspace/../../etc/passwd").is_err());
        assert!(Vfs::normalize_path("/..").is_err());
        assert!(Vfs::normalize_path("/tools/../..").is_err());
    }

    #[test]
    fn test_security_path_traversal_via_write() {
        let mut vfs = Vfs::new();
        // Attempt path traversal via write_file
        let result = vfs.write_file("/workspace/../tools/evil.js", b"bad", Permission::ReadWrite);
        // Should either fail normalization OR fail permission check
        // (normalize to /tools/evil.js → parent is read-only)
        assert!(result.is_err());
    }

    #[test]
    fn test_security_path_traversal_via_create_dir() {
        let mut vfs = Vfs::new();
        // Attempt path traversal via create_dir_all
        let result = vfs.create_dir_all("/workspace/../tools/subdir", Permission::ReadWrite);
        assert!(result.is_err());
    }

    #[test]
    fn test_security_dot_segments_normalized() {
        // Single dots are collapsed
        assert_eq!(
            Vfs::normalize_path("/workspace/./file.txt").unwrap(),
            "/workspace/file.txt"
        );
        // Double dots navigate up (within bounds)
        assert_eq!(
            Vfs::normalize_path("/workspace/subdir/../file.txt").unwrap(),
            "/workspace/file.txt"
        );
        // Multiple segments
        assert_eq!(Vfs::normalize_path("/a/b/c/../../d").unwrap(), "/a/d");
    }

    #[test]
    fn test_security_null_bytes_rejected() {
        // Null bytes in paths should be rejected
        assert!(Vfs::normalize_path("/workspace/file\0.txt").is_err());
        assert!(Vfs::normalize_path("/work\0space/file.txt").is_err());
    }

    // =========================================================================
    // SECURITY: ReadWrite directory operations
    // =========================================================================

    #[test]
    fn test_security_can_write_to_readwrite_dir() {
        let mut vfs = Vfs::new();
        // /workspace is ReadWrite - can create files
        vfs.write_file("/workspace/file.txt", b"data", Permission::ReadWrite)
            .unwrap();
        assert_eq!(vfs.read_file("/workspace/file.txt").unwrap(), b"data");
    }

    #[test]
    fn test_security_can_delete_from_readwrite_dir() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/file.txt", b"data", Permission::ReadWrite)
            .unwrap();
        vfs.remove("/workspace/file.txt").unwrap();
        assert!(!vfs.exists("/workspace/file.txt"));
    }

    #[test]
    fn test_security_cannot_delete_readonly_file_in_readwrite_dir() {
        let mut vfs = Vfs::new();
        // Create a read-only file in a read-write directory
        vfs.write_file("/workspace/protected.txt", b"data", Permission::ReadOnly)
            .unwrap();
        // Cannot delete it (file permission takes precedence)
        let result = vfs.remove("/workspace/protected.txt");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_security_delete_requires_parent_writable() {
        let mut vfs = Vfs::new();
        // Create a read-write file in an append-only directory (via bootstrap)
        vfs.insert_file("/log/temp.txt", b"data", Permission::ReadWrite)
            .unwrap();
        // Cannot delete because parent is append-only
        let result = vfs.remove("/log/temp.txt");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    // =========================================================================
    // SECURITY: Edge cases
    // =========================================================================

    #[test]
    fn test_security_cannot_overwrite_directory_with_file() {
        let mut vfs = Vfs::new();
        vfs.create_dir("/workspace/mydir", Permission::ReadWrite)
            .unwrap();
        // Cannot overwrite directory with file
        let result = vfs.write_file("/workspace/mydir", b"data", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::NotAFile(_))));
    }

    #[test]
    fn test_security_cannot_append_to_directory() {
        let mut vfs = Vfs::new();
        let result = vfs.append_file("/workspace", b"data");
        assert!(matches!(result, Err(VfsError::NotAFile(_))));
    }

    #[test]
    fn test_security_cannot_create_over_existing() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/file.txt", b"data", Permission::ReadWrite)
            .unwrap();
        // create_dir on existing file fails
        let result = vfs.create_dir("/workspace/file.txt", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::AlreadyExists(_))));
    }

    #[test]
    fn test_security_parent_must_exist() {
        let mut vfs = Vfs::new();
        // Parent doesn't exist
        let result = vfs.write_file("/nonexistent/file.txt", b"data", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::NotFound(_))));
    }

    // =========================================================================
    // Coverage: Entry methods
    // =========================================================================

    #[test]
    fn test_entry_is_file() {
        let file = Entry::File {
            content: vec![1, 2, 3],
            permission: Permission::ReadWrite,
        };
        assert!(file.is_file());
        assert!(!file.is_dir());
    }

    #[test]
    fn test_entry_is_dir() {
        let dir = Entry::Directory {
            permission: Permission::ReadOnly,
        };
        assert!(dir.is_dir());
        assert!(!dir.is_file());
    }

    #[test]
    fn test_entry_permission() {
        let file = Entry::File {
            content: vec![],
            permission: Permission::AppendOnly,
        };
        assert_eq!(file.permission(), Permission::AppendOnly);

        let dir = Entry::Directory {
            permission: Permission::ReadWrite,
        };
        assert_eq!(dir.permission(), Permission::ReadWrite);
    }

    // =========================================================================
    // Coverage: Bootstrap insert_* methods
    // =========================================================================

    #[test]
    fn test_insert_file_creates_parents() {
        let mut vfs = Vfs::empty();
        // insert_file should auto-create parent directories
        vfs.insert_file("/a/b/c/file.txt", b"content", Permission::ReadWrite)
            .unwrap();

        assert!(vfs.is_dir("/a"));
        assert!(vfs.is_dir("/a/b"));
        assert!(vfs.is_dir("/a/b/c"));
        assert!(vfs.is_file("/a/b/c/file.txt"));
    }

    #[test]
    fn test_insert_dir() {
        let mut vfs = Vfs::empty();
        vfs.insert_dir("/mydir", Permission::ReadWrite).unwrap();
        assert!(vfs.is_dir("/mydir"));
    }

    #[test]
    fn test_insert_dir_all() {
        let mut vfs = Vfs::empty();
        vfs.insert_dir_all("/a/b/c/d", Permission::ReadOnly)
            .unwrap();

        assert!(vfs.is_dir("/a"));
        assert!(vfs.is_dir("/a/b"));
        assert!(vfs.is_dir("/a/b/c"));
        assert!(vfs.is_dir("/a/b/c/d"));
    }

    #[test]
    fn test_insert_dir_all_root() {
        let mut vfs = Vfs::empty();
        // insert_dir_all at root should be no-op
        vfs.insert_dir_all("/", Permission::ReadOnly).unwrap();
    }

    // =========================================================================
    // Coverage: Path normalization edge cases
    // =========================================================================

    #[test]
    fn test_normalize_empty_path() {
        assert_eq!(Vfs::normalize_path("").unwrap(), "/");
    }

    #[test]
    fn test_normalize_just_slash() {
        assert_eq!(Vfs::normalize_path("/").unwrap(), "/");
    }

    #[test]
    fn test_normalize_dot_only() {
        assert_eq!(Vfs::normalize_path(".").unwrap(), "/");
        assert_eq!(Vfs::normalize_path("/.").unwrap(), "/");
        assert_eq!(Vfs::normalize_path("/./").unwrap(), "/");
    }

    #[test]
    fn test_normalize_multiple_slashes() {
        assert_eq!(Vfs::normalize_path("//a//b//").unwrap(), "/a/b");
    }

    // =========================================================================
    // Coverage: Mutation error cases
    // =========================================================================

    #[test]
    fn test_append_to_readonly_file() {
        let mut vfs = Vfs::new();
        // Create a read-only file in workspace
        vfs.write_file("/workspace/readonly.txt", b"data", Permission::ReadOnly)
            .unwrap();

        // Cannot append to read-only file
        let result = vfs.append_file("/workspace/readonly.txt", b"more");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_append_to_nonexistent_file() {
        let mut vfs = Vfs::new();
        let result = vfs.append_file("/workspace/does_not_exist.txt", b"data");
        assert!(matches!(result, Err(VfsError::NotFound(_))));
    }

    #[test]
    fn test_delete_nonexistent_file() {
        let mut vfs = Vfs::new();
        let result = vfs.remove("/workspace/does_not_exist.txt");
        assert!(matches!(result, Err(VfsError::NotFound(_))));
    }

    #[test]
    fn test_write_when_parent_is_file() {
        let mut vfs = Vfs::new();
        // Create a file
        vfs.write_file("/workspace/file.txt", b"data", Permission::ReadWrite)
            .unwrap();

        // Try to write a "child" of that file
        let result = vfs.write_file(
            "/workspace/file.txt/child.txt",
            b"data",
            Permission::ReadWrite,
        );
        assert!(matches!(result, Err(VfsError::NotADirectory(_))));
    }

    #[test]
    fn test_create_dir_when_parent_is_file() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/file.txt", b"data", Permission::ReadWrite)
            .unwrap();

        let result = vfs.create_dir("/workspace/file.txt/subdir", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::NotADirectory(_))));
    }

    // =========================================================================
    // Coverage: create_dir_all edge cases
    // =========================================================================

    #[test]
    fn test_create_dir_all_root() {
        let mut vfs = Vfs::new();
        // create_dir_all at root should be no-op
        let result = vfs.create_dir_all("/", Permission::ReadWrite);
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_dir_all_over_file() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/file.txt", b"data", Permission::ReadWrite)
            .unwrap();

        // Try to create dir path that goes through a file
        let result = vfs.create_dir_all("/workspace/file.txt/subdir", Permission::ReadWrite);
        assert!(matches!(result, Err(VfsError::NotADirectory(_))));
    }

    #[test]
    fn test_create_dir_all_existing_partial() {
        let mut vfs = Vfs::new();
        vfs.create_dir("/workspace/a", Permission::ReadWrite)
            .unwrap();

        // create_dir_all should skip existing dirs and create new ones
        vfs.create_dir_all("/workspace/a/b/c", Permission::ReadWrite)
            .unwrap();
        assert!(vfs.is_dir("/workspace/a/b/c"));
    }

    // =========================================================================
    // Coverage: Read/stat error cases
    // =========================================================================

    #[test]
    fn test_read_file_on_directory() {
        let vfs = Vfs::new();
        let result = vfs.read_file("/workspace");
        assert!(matches!(result, Err(VfsError::NotAFile(_))));
    }

    #[test]
    fn test_read_file_string_invalid_utf8() {
        let mut vfs = Vfs::new();
        vfs.write_file(
            "/workspace/binary.bin",
            &[0xFF, 0xFE, 0x00, 0x01],
            Permission::ReadWrite,
        )
        .unwrap();

        let result = vfs.read_file_string("/workspace/binary.bin");
        assert!(result.is_err());
    }

    #[test]
    fn test_is_file_with_invalid_path() {
        let vfs = Vfs::new();
        // Path that escapes root - is_file should return false (not panic)
        assert!(!vfs.is_file("/../../../etc/passwd"));
    }

    #[test]
    fn test_is_dir_with_invalid_path() {
        let vfs = Vfs::new();
        assert!(!vfs.is_dir("/../../../etc"));
    }

    #[test]
    fn test_exists_with_invalid_path() {
        let vfs = Vfs::new();
        assert!(!vfs.exists("/../../../etc/passwd"));
    }

    #[test]
    fn test_stat() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/file.txt", b"hello", Permission::ReadWrite)
            .unwrap();

        let entry = vfs.stat("/workspace/file.txt").unwrap();
        assert!(entry.is_file());
        assert_eq!(entry.permission(), Permission::ReadWrite);
    }

    #[test]
    fn test_stat_not_found() {
        let vfs = Vfs::new();
        let result = vfs.stat("/workspace/nonexistent.txt");
        assert!(matches!(result, Err(VfsError::NotFound(_))));
    }

    // =========================================================================
    // Coverage: list_dir error cases
    // =========================================================================

    #[test]
    fn test_list_dir_on_file() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/file.txt", b"data", Permission::ReadWrite)
            .unwrap();

        let result = vfs.list_dir("/workspace/file.txt");
        assert!(matches!(result, Err(VfsError::NotADirectory(_))));
    }

    #[test]
    fn test_list_dir_not_found() {
        let vfs = Vfs::new();
        let result = vfs.list_dir("/nonexistent");
        assert!(matches!(result, Err(VfsError::NotFound(_))));
    }

    // =========================================================================
    // Coverage: remove edge cases
    // =========================================================================

    #[test]
    fn test_remove_non_empty_directory() {
        let mut vfs = Vfs::new();
        vfs.create_dir("/workspace/dir", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/dir/file.txt", b"data", Permission::ReadWrite)
            .unwrap();

        // Cannot remove non-empty directory
        let result = vfs.remove("/workspace/dir");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
        assert!(result.unwrap_err().to_string().contains("not empty"));
    }

    #[test]
    fn test_remove_empty_directory() {
        let mut vfs = Vfs::new();
        vfs.create_dir("/workspace/empty_dir", Permission::ReadWrite)
            .unwrap();

        // Can remove empty directory
        vfs.remove("/workspace/empty_dir").unwrap();
        assert!(!vfs.exists("/workspace/empty_dir"));
    }

    // =========================================================================
    // Coverage: glob patterns
    // =========================================================================

    #[test]
    fn test_glob_double_star() {
        let mut vfs = Vfs::new();
        // Create nested directories first
        vfs.create_dir_all("/workspace/a/b", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/a/b/c.txt", b"", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/a/d.txt", b"", Permission::ReadWrite)
            .unwrap();

        // ** matches any depth
        let matches = vfs.glob("/workspace/**/*.txt");
        assert!(matches.contains(&"/workspace/a/b/c.txt".to_string()));
        assert!(matches.contains(&"/workspace/a/d.txt".to_string()));
    }

    #[test]
    fn test_glob_question_mark() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/a1.txt", b"", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/a2.txt", b"", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/ab.txt", b"", Permission::ReadWrite)
            .unwrap();

        // ? matches single character
        let matches = vfs.glob("/workspace/a?.txt");
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn test_glob_special_chars() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/file[1].txt", b"", Permission::ReadWrite)
            .unwrap();

        // Special chars should be escaped
        let matches = vfs.glob("/workspace/file[1].txt");
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_glob_invalid_pattern() {
        let vfs = Vfs::new();
        // Invalid glob pattern that escapes root
        let matches = vfs.glob("/../../../etc/*");
        assert!(matches.is_empty());
    }

    #[test]
    fn test_glob_no_matches() {
        let vfs = Vfs::new();
        let matches = vfs.glob("/workspace/*.nonexistent");
        assert!(matches.is_empty());
    }

    // =========================================================================
    // Coverage: remove_recursive
    // =========================================================================

    #[test]
    fn test_remove_recursive_file() {
        let mut vfs = Vfs::new();
        vfs.write_file("/workspace/file.txt", b"data", Permission::ReadWrite)
            .unwrap();

        vfs.remove_recursive("/workspace/file.txt").unwrap();
        assert!(!vfs.exists("/workspace/file.txt"));
    }

    #[test]
    fn test_remove_recursive_directory() {
        let mut vfs = Vfs::new();
        vfs.create_dir_all("/workspace/a/b/c", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/a/b/c/file.txt", b"data", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/a/file2.txt", b"data", Permission::ReadWrite)
            .unwrap();

        // Remove everything under /workspace/a
        vfs.remove_recursive("/workspace/a").unwrap();

        assert!(!vfs.exists("/workspace/a"));
        assert!(!vfs.exists("/workspace/a/b"));
        assert!(!vfs.exists("/workspace/a/b/c"));
        assert!(!vfs.exists("/workspace/a/b/c/file.txt"));
        assert!(!vfs.exists("/workspace/a/file2.txt"));
    }

    #[test]
    fn test_remove_recursive_not_found() {
        let mut vfs = Vfs::new();
        let result = vfs.remove_recursive("/workspace/nonexistent");
        assert!(matches!(result, Err(VfsError::NotFound(_))));
    }

    #[test]
    fn test_remove_recursive_readonly_blocked() {
        let mut vfs = Vfs::new();
        // Try to recursively remove /tools (read-only)
        let result = vfs.remove_recursive("/tools");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_remove_recursive_with_readonly_child() {
        let mut vfs = Vfs::new();
        vfs.create_dir("/workspace/dir", Permission::ReadWrite)
            .unwrap();
        // Create a read-only file inside
        vfs.write_file("/workspace/dir/readonly.txt", b"data", Permission::ReadOnly)
            .unwrap();

        // Should fail because child is read-only
        let result = vfs.remove_recursive("/workspace/dir");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));

        // Directory should still exist (no partial deletion)
        assert!(vfs.exists("/workspace/dir"));
        assert!(vfs.exists("/workspace/dir/readonly.txt"));
    }

    #[test]
    fn test_remove_recursive_with_appendonly_child() {
        let mut vfs = Vfs::new();
        vfs.create_dir("/workspace/logs", Permission::ReadWrite)
            .unwrap();
        vfs.write_file("/workspace/logs/audit.log", b"data", Permission::AppendOnly)
            .unwrap();

        // Should fail because child is append-only
        let result = vfs.remove_recursive("/workspace/logs");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }

    #[test]
    fn test_remove_respects_entry_permission() {
        let mut vfs = Vfs::new();
        // ReadOnly file cannot be deleted
        vfs.write_file("/workspace/readonly.txt", b"data", Permission::ReadOnly)
            .unwrap();
        let result = vfs.remove("/workspace/readonly.txt");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));

        // AppendOnly file cannot be deleted
        vfs.write_file("/workspace/appendonly.txt", b"data", Permission::AppendOnly)
            .unwrap();
        let result = vfs.remove("/workspace/appendonly.txt");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));

        // ReadWrite file can be deleted
        vfs.write_file("/workspace/writable.txt", b"data", Permission::ReadWrite)
            .unwrap();
        vfs.remove("/workspace/writable.txt").unwrap();
        assert!(!vfs.exists("/workspace/writable.txt"));
    }

    #[test]
    fn test_remove_respects_parent_permission() {
        let mut vfs = Vfs::new();
        // File in read-only parent cannot be deleted (even if file is writable)
        // We need to use insert_file to bypass permission for setup
        vfs.insert_file("/tools/test.txt", b"data", Permission::ReadWrite)
            .unwrap();

        let result = vfs.remove("/tools/test.txt");
        assert!(matches!(result, Err(VfsError::PermissionDenied(_))));
    }
}
