# amla-shell

Unix shell implementation for AI agent sandboxes. No syscalls, pure Rust, WASM-compatible.

## Why a Custom Shell

Agents need to process data: filter JSON, extract fields, search text. Real shells (`bash`, `zsh`) can't run in WASM. We implement the subset agents actually use:

- **Text processing**: `grep`, `cut`, `sort`, `uniq`, `tr`, `head`, `tail`
- **File operations**: `ls`, `cat`, `mkdir`, `touch`, `rm`
- **Utilities**: `echo`, `printf`, `wc`, `tee`, `xxd`
- **Control**: `test`, `true`, `false`, `exit`
- **Integration**: `node` (JavaScript), `tools` (tool discovery)

Full pipeline support: `cat data.json | grep "error" | cut -d'"' -f4 | sort | uniq -c`

## Commands

### Text Processing

| Command | Usage | Description |
|---------|-------|-------------|
| `grep` | `grep [-ivnclqF] [-A n] [-B n] [-C n] [-e pattern] pattern [file...]` | Search for patterns |
| `cut` | `cut -d delim -f fields [file...]` or `cut -c chars` or `cut -b bytes` | Extract columns |
| `sort` | `sort [-rnuf] [-k field] [file...]` | Sort lines |
| `uniq` | `uniq [-c] [-d] [-u] [file]` | Deduplicate adjacent lines |
| `tr` | `tr [-d] [-s] set1 [set2]` | Translate characters |
| `head` | `head [-n lines] [file...]` | First N lines |
| `tail` | `tail [-n lines] [file...]` | Last N lines |
| `wc` | `wc [-lwc] [file...]` | Count lines/words/chars |

### File Operations

| Command | Usage | Description |
|---------|-------|-------------|
| `cat` | `cat [file...]` | Concatenate files |
| `ls` | `ls [-la1h] [path...]` | List directory |
| `mkdir` | `mkdir [-p] dir...` | Create directories |
| `touch` | `touch file...` | Create empty files |
| `rm` | `rm [-rf] path...` | Remove files/dirs |
| `tee` | `tee [-a] file...` | Copy stdin to files and stdout |

### Utilities

| Command | Usage | Description |
|---------|-------|-------------|
| `echo` | `echo [-n] [-e] [text...]` | Print text |
| `printf` | `printf format [args...]` | Formatted output |
| `xxd` | `xxd [-r] [-p] [file]` | Hex dump |
| `test` / `[` | `test expr` or `[ expr ]` | Conditionals |
| `date` | `date [+format]` | Current date/time |
| `sleep` | `sleep seconds` | Pause execution |
| `time` | `time command` | Measure execution time |

### Builtins

| Command | Usage | Description |
|---------|-------|-------------|
| `cd` | `cd [dir]` | Change directory |
| `pwd` | `pwd` | Print working directory |
| `export` | `export VAR=value` | Set environment variable |
| `unset` | `unset VAR` | Remove environment variable |
| `env` | `env` | Print environment |
| `set` | `set VAR=value` | Set shell variable |
| `exit` | `exit [code]` | Exit with status |
| `true` | `true` | Exit 0 |
| `false` | `false` | Exit 1 |
| `:` | `:` | No-op (always succeeds) |

### Integration

| Command | Usage | Description |
|---------|-------|-------------|
| `node` | `node [-e code] [file.js]` | Execute JavaScript |
| `sh` / `bash` | `sh -c "command"` | Execute subshell |
| `tools` | `tools [search query]` | Search available tools |

## Command Options

### grep

```bash
grep [-ivnclqFE] [-A num] [-B num] [-C num] [-e pattern] pattern [file...]

-i          Case insensitive
-v          Invert match (print non-matching lines)
-n          Show line numbers
-c          Count matches only
-l          Print filenames only
-q          Quiet (exit status only)
-F          Fixed string (no regex)
-E          Extended regex (ignored, default)
-A num      Print num lines after match
-B num      Print num lines before match
-C num      Print num lines before and after match
-e pattern  Specify pattern (allows pattern starting with -)
```

### cut

```bash
cut -d delim -f fields [file...]   # Field mode
cut -c chars [file...]              # Character mode
cut -b bytes [file...]              # Byte mode

-d char     Field delimiter (default: TAB)
-f list     Select fields (1-indexed): 1,3 or 1-3 or 2-
-c list     Select characters
-b list     Select bytes
-s          Only print lines containing delimiter
```

### sort

```bash
sort [-rnuf] [-k field] [file...]

-r          Reverse order
-n          Numeric sort
-u          Unique (remove duplicates)
-f          Case insensitive
-k N        Sort by field N (1-indexed, whitespace-separated)
```

### ls

```bash
ls [-la1h] [path...]

-l          Long format (permissions, size, date)
-a          Show hidden files (starting with .)
-1          One entry per line
-h          Human-readable sizes (K, M, G)
```

### head / tail

```bash
head [-n lines] [file...]
tail [-n lines] [file...]

-n num      Number of lines (default: 10)
```

### wc

```bash
wc [-lwc] [file...]

-l          Count lines
-w          Count words
-c          Count characters/bytes
(no flags)  All three counts
```

## Pipelines and Redirects

Full shell pipeline support:

```bash
# Pipes
cat data.json | grep "error" | head -5

# Boolean operators
test -f config.json && cat config.json
command1 || echo "fallback"

# Sequential execution
echo "start" ; process ; echo "done"

# Output redirect
grep "TODO" *.js > todos.txt
echo "append" >> log.txt

# Input redirect
sort < unsorted.txt

# Stderr redirect
command 2> errors.txt
command 2>&1           # stderr to stdout
command &> all.txt     # both to file

# Background (returns immediately)
long_running_task &

# Job control
jobs                   # list background jobs
fg %1                  # bring job to foreground
bg %1                  # continue job in background
```

## Variable Expansion

```bash
export API_KEY=secret123
echo "Key is: $API_KEY"
echo "Path: ${HOME}/data"

# Shell variables (not exported)
set count=42
echo $count
```

## Async Architecture

Commands are async functions that yield to the scheduler:

```rust
pub async fn run(ctx: CmdContext) -> CommandResult {
    // Read from stdin
    let mut buffer = [0u8; 4096];
    let n = ctx.stdin.read(&mut buffer).await?;

    // Write to stdout
    ctx.println("output").await?;

    // Access VFS
    let content = ctx.read_file("/workspace/data.txt").await?;

    Ok(Exit::success())
}
```

No blocking I/O. All reads/writes yield to the scheduler, enabling:

- Pipeline parallelism (commands in a pipe run concurrently)
- Host-controlled stepping
- Deterministic execution

## Side Effects Model

Commands don't mutate shell state directly. They return `SideEffects`:

```rust
pub struct SideEffects {
    pub cwd: Option<String>,           // cd changed directory
    pub env_set: SmallVec<[(String, String); 2]>,  // export
    pub env_unset: SmallVec<[String; 2]>,          // unset
    pub pipefail: Option<bool>,        // set -o pipefail
}
```

The shell applies effects after command completion. This ensures consistent state even with concurrent pipeline execution.

## Building

```bash
# Native (for testing)
cargo build -p amla-shell
cargo test -p amla-shell

# WASM
cargo build -p amla-shell --target wasm32-wasip1
```

## Adding Commands

1. Create `src/commands/mycommand.rs`:

   ```rust
   use amla_scheduler::Exit;
   use crate::CmdContext;
   use super::CommandResult;

   /// mycommand [-f] [file...]
   pub async fn run(ctx: CmdContext) -> CommandResult {
       let mut flag = false;
       let mut files = Vec::new();

       let mut parser = ctx.arg_parser();
       loop {
           match parser.next() {
               Ok(Some(lexopt::Arg::Short('f'))) => flag = true,
               Ok(Some(lexopt::Arg::Value(val))) => {
                   files.push(val.to_string_lossy().into_owned());
               }
               Ok(None) => break,
               _ => {}
           }
       }

       // Implementation...

       Ok(Exit::success())
   }
   ```

2. Register in `src/commands/mod.rs`:

   ```rust
   mod mycommand;

   pub fn get_command(name: &str) -> Option<CommandFn> {
       match name {
           // ...
           "mycommand" => Some(|ctx| Box::pin(mycommand::run(ctx))),
           _ => None,
       }
   }
   ```

## License

AGPL-3.0-or-later OR BUSL-1.1
