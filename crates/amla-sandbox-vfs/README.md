# amla-vfs

In-memory virtual filesystem for AI agent sandboxing.

## Why a Virtual Filesystem

Agents need to read and write files, but we can't give them real filesystem access:

- **Isolation**: Agent code can't read `/etc/passwd` or `~/.ssh/`
- **Portability**: Works in WASM where there's no filesystem
- **Determinism**: Same inputs produce same file state
- **Audit**: All file operations are observable

This VFS provides a permission-controlled filesystem that exists only in memory.

## Permission Model

Three permission levels control what operations are allowed:

| Permission | Read | Write | Append | Delete |
|------------|------|-------|--------|--------|
| `ReadOnly` | Yes | No | No | No |
| `ReadWrite` | Yes | Yes | Yes | Yes |
| `AppendOnly` | Yes | No | Yes | No |

Permissions apply to both files and directories:

```
/                   ReadOnly   (cannot create files in root)
├── /tools/         ReadOnly   (auto-generated tool stubs)
├── /bin/           ReadOnly   (shell command metadata)
├── /workspace/     ReadWrite  (agent scratch space)
└── /log/           AppendOnly (audit logs, can only append)
    └── actions.jsonl
```

## Security Architecture

**All mutations go through a single permission chokepoint.**

```rust
// The ONLY place permission checks happen
fn check_mutation(&self, path: &str, op: MutationOp) -> Result<(), VfsError> {
    match op {
        MutationOp::Create => {
            // Parent must be writable
            self.check_parent_writable(path)?;
        }
        MutationOp::Overwrite => {
            // Existing file must be ReadWrite, or parent must be writable
        }
        MutationOp::Append => {
            // File must be ReadWrite or AppendOnly
        }
        MutationOp::Delete => {
            // File must be ReadWrite, parent must be writable
        }
    }
    Ok(())
}
```

Every mutating method (`write_file`, `create_dir`, `remove`, `append_file`) calls `check_mutation()` before modifying state.

## Path Normalization

All paths are normalized to prevent traversal attacks:

```rust
// These all normalize to /workspace/file.txt
"/workspace/./file.txt"
"/workspace/subdir/../file.txt"
"/workspace//file.txt"

// These are REJECTED (escape root)
"/../etc/passwd"           // Error: path escapes root
"/workspace/../../etc"     // Error: path escapes root
"/workspace/file\0.txt"    // Error: null byte
```

The VFS uses `std::path::Path::components()` to properly handle:

- `.` (current directory) - collapsed
- `..` (parent directory) - navigates up, errors if escapes root
- Multiple slashes - collapsed
- Null bytes - rejected

## API

### Basic Operations

```rust
use amla_vfs::{Vfs, Permission};

let mut vfs = Vfs::new();  // Creates standard directory structure

// Write file
vfs.write_file("/workspace/data.txt", b"hello", Permission::ReadWrite)?;

// Read file
let content = vfs.read_file("/workspace/data.txt")?;
let text = vfs.read_file_string("/workspace/data.txt")?;

// Check existence
assert!(vfs.exists("/workspace/data.txt"));
assert!(vfs.is_file("/workspace/data.txt"));
assert!(vfs.is_dir("/workspace"));

// Get metadata
let entry = vfs.stat("/workspace/data.txt")?;
```

### Directory Operations

```rust
// Create directory
vfs.create_dir("/workspace/subdir", Permission::ReadWrite)?;

// Create nested directories
vfs.create_dir_all("/workspace/a/b/c", Permission::ReadWrite)?;

// List directory
let entries = vfs.list_dir("/workspace")?;
for entry in entries {
    println!("{}: {}", entry.name, if entry.is_dir { "dir" } else { "file" });
}
```

### Append-Only Files

```rust
// /log/actions.jsonl is initialized as append-only
vfs.append_file("/log/actions.jsonl", b"{\"action\": \"start\"}\n")?;
vfs.append_file("/log/actions.jsonl", b"{\"action\": \"stop\"}\n")?;

// Cannot overwrite
assert!(vfs.write_file("/log/actions.jsonl", b"replaced", Permission::AppendOnly).is_err());

// Cannot delete
assert!(vfs.remove("/log/actions.jsonl").is_err());
```

### Glob Patterns

```rust
vfs.write_file("/workspace/test.txt", b"", Permission::ReadWrite)?;
vfs.write_file("/workspace/test.js", b"", Permission::ReadWrite)?;
vfs.create_dir_all("/workspace/a/b", Permission::ReadWrite)?;
vfs.write_file("/workspace/a/b/deep.txt", b"", Permission::ReadWrite)?;

// Single star: matches within directory
let matches = vfs.glob("/workspace/*.txt");
// ["/workspace/test.txt"]

// Double star: matches any depth
let matches = vfs.glob("/workspace/**/*.txt");
// ["/workspace/test.txt", "/workspace/a/b/deep.txt"]

// Question mark: single character
let matches = vfs.glob("/workspace/test.???");
// ["/workspace/test.txt"]
```

### Delete

```rust
// Delete file
vfs.remove("/workspace/file.txt")?;

// Delete empty directory
vfs.remove("/workspace/empty_dir")?;

// Delete recursively (rm -rf)
vfs.remove_recursive("/workspace/dir_with_contents")?;
```

### Host Mounts

Map external paths to sandbox paths (read-only):

```rust
vfs.setup_mounts([
    ("/host/data/config.json".into(), "/data/config.json".into()),
    ("/host/input.csv".into(), "/input.csv".into()),
])?;

// Content loaded lazily via HostChannel when accessed
assert!(vfs.is_mounted("/data/config.json"));
assert_eq!(vfs.get_host_path("/data/config.json"), Some("/host/data/config.json"));
```

## Bootstrap vs Runtime Methods

**Bootstrap methods** bypass permission checks for system initialization:

```rust
// Use ONLY during runtime setup, before any agent code runs
vfs.insert_file("/tools/stripe/charge.js", content, Permission::ReadOnly)?;
vfs.insert_dir("/tools/stripe", Permission::ReadOnly)?;
vfs.insert_dir_all("/tools/provider/subtool", Permission::ReadOnly)?;
```

**Runtime methods** enforce permissions:

```rust
// Use during agent execution
vfs.write_file("/workspace/file.txt", content, Permission::ReadWrite)?;
vfs.create_dir("/workspace/mydir", Permission::ReadWrite)?;
```

## Error Types

```rust
pub enum VfsError {
    NotFound(String),        // Path doesn't exist
    PermissionDenied(String), // Operation not allowed
    NotADirectory(String),   // Expected directory, found file
    NotAFile(String),        // Expected file, found directory
    AlreadyExists(String),   // Path already exists (for create)
    InvalidPath(String),     // Path normalization failed
}
```

## Async VFS

For integration with the async scheduler:

```rust
use amla_vfs::{AsyncVfs, Fd, OpenMode};

let async_vfs = AsyncVfs::new(vfs, host_channel);

// Open file handle
let fd: Fd = async_vfs.open("/workspace/data.txt", OpenMode::Read).await?;

// Read
let data = async_vfs.read(fd, 1024).await?;

// Write
async_vfs.write(fd, b"content").await?;

// Close
async_vfs.close(fd).await?;
```

The `AsyncVfs` wraps the synchronous `Vfs` and uses the `HostChannel` for lazy-loaded mounted files.

## Building

```bash
cargo build -p amla-vfs
cargo test -p amla-vfs
```

## License

AGPL-3.0-or-later OR BUSL-1.1
