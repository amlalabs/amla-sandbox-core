//! node - Execute JavaScript code using QuickJS
//!
//! This provides a Node.js-like CLI for running JavaScript files.

use amla_scheduler::Exit;

use crate::CmdContext;

use super::CommandResult;

/// Path to the tool prelude that defines global functions like `charge()`
const PRELUDE_PATH: &str = "/tools/prelude.js";

/// `node [options] [script.js] [arguments]`
///
/// Execute JavaScript code.
///
/// # Options
///
/// - `-e, --eval <code>` - Evaluate code instead of running a file
/// - `-p, --print <code>` - Evaluate and print result
/// - `--max-old-space-size=<MB>` - Set memory limit in megabytes
/// - `-` - Read script from stdin
///
/// # Examples
///
/// ```text
/// node script.js              # Run a JavaScript file
/// node -e "console.log('hi')" # Evaluate inline code
/// echo "1+2" | node -         # Read from stdin
/// node --max-old-space-size=128 script.js  # Limit to 128MB
/// ```
pub async fn run(ctx: CmdContext) -> CommandResult {
    // Parse arguments
    let mut eval_code: Option<String> = None;
    let mut print_code: Option<String> = None;
    let mut script_path: Option<String> = None;
    let mut script_args: Vec<String> = Vec::new();
    let mut memory_limit_mb: Option<usize> = None;

    let mut args = ctx.argv.iter().skip(1).peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-e" | "--eval" => {
                if let Some(code) = args.next() {
                    eval_code = Some(code.clone());
                } else {
                    ctx.eprintln("node: -e requires an argument").await?;
                    return Ok(Exit::code(1));
                }
            }
            "-p" | "--print" => {
                if let Some(code) = args.next() {
                    print_code = Some(code.clone());
                } else {
                    ctx.eprintln("node: -p requires an argument").await?;
                    return Ok(Exit::code(1));
                }
            }
            "-h" | "--help" => {
                ctx.println(HELP_TEXT).await?;
                return Ok(Exit::success());
            }
            "-v" | "--version" => {
                ctx.println("node v0.1.0 (amla-shell QuickJS backend)")
                    .await?;
                return Ok(Exit::success());
            }
            arg if arg.starts_with("--max-old-space-size=") => {
                let val = &arg["--max-old-space-size=".len()..];
                match val.parse::<usize>() {
                    Ok(mb) => memory_limit_mb = Some(mb),
                    Err(_) => {
                        ctx.eprintln(&format!("node: invalid memory limit: {val}"))
                            .await?;
                        return Ok(Exit::code(1));
                    }
                }
            }
            arg if arg.starts_with('-') && arg != "-" => {
                ctx.eprintln(&format!("node: unknown option: {arg}"))
                    .await?;
                return Ok(Exit::code(1));
            }
            _ => {
                // First non-option is the script, rest are arguments
                script_path = Some(arg.clone());
                script_args = args.cloned().collect();
                break;
            }
        }
    }

    // Determine what code to run
    let code = if let Some(code) = eval_code {
        code
    } else if let Some(code) = print_code.clone() {
        code
    } else if let Some(ref path) = script_path {
        if path == "-" {
            // Read from stdin
            read_stdin_to_string(&ctx).await?
        } else {
            // Read from file
            match ctx.read_file(path).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) => {
                    ctx.eprintln(&format!("node: cannot open '{path}': {e}"))
                        .await?;
                    return Ok(Exit::code(1));
                }
            }
        }
    } else {
        // No script or code provided - show help
        ctx.println(HELP_TEXT).await?;
        return Ok(Exit::success());
    };

    // Execute the code
    execute_quickjs(
        &ctx,
        &code,
        print_code.is_some(),
        script_path.as_ref(),
        &script_args,
        memory_limit_mb,
    )
    .await
}

async fn execute_quickjs(
    ctx: &CmdContext,
    code: &str,
    print_result: bool,
    _script_path: Option<&String>,
    _script_args: &[String],
    memory_limit_mb: Option<usize>,
) -> CommandResult {
    use amla_js::{EngineConfig, JsRuntime};

    // Create runtime with optional memory limit
    let mut runtime = if let Some(mb) = memory_limit_mb {
        let config = EngineConfig {
            memory_limit: mb * 1024 * 1024, // Convert MB to bytes
            ..Default::default()
        };
        match JsRuntime::with_config(config) {
            Ok(rt) => rt,
            Err(e) => {
                ctx.eprintln(&format!("node: failed to create runtime: {e}"))
                    .await?;
                return Ok(Exit::code(1));
            }
        }
    } else {
        JsRuntime::new()
    };

    // Auto-load tool prelude if it exists
    // This makes tool functions like `charge()` available automatically
    if let Ok(prelude_bytes) = ctx.read_file(PRELUDE_PATH).await {
        let prelude_code = String::from_utf8_lossy(&prelude_bytes);
        if let Err(e) = runtime.execute(&prelude_code) {
            ctx.eprintln(&format!("node: failed to load prelude: {e}"))
                .await?;
            // Continue anyway - prelude failure shouldn't block user code
        }
    }

    // Wrap code in async IIFE to allow top-level await.
    // Skip wrapping for -p (print) since we need the direct result value.
    // Example: `const x = await fs.readFile("/f"); console.log(x);`
    // becomes: `(async () => { ... })().catch(e => { /* format error */ });`
    //
    // IMPORTANT: The .catch() handler ensures unhandled rejections from the async
    // IIFE are reported to stderr. Without this, errors like `x.foo.bar` where x
    // is undefined would silently fail (the promise rejects but nobody catches it).
    // We format the error with "ErrorName: message\nstack" for readable output.
    let wrapped_code = if print_result {
        code.to_string()
    } else {
        format!(
            "(async () => {{\n{code}\n}})().catch(e => {{\
                const msg = (e.name ? e.name + ': ' : '') + (e.message || String(e));\
                console.error(e.stack ? msg + '\\n' + e.stack : msg);\
            }});"
        )
    };

    // Execute initial code
    let result = match runtime.execute(&wrapped_code) {
        Ok(r) => r,
        Err(e) => {
            ctx.eprintln(&format!("node: {e}")).await?;
            return Ok(Exit::code(1));
        }
    };

    // Flush initial console output
    flush_console(ctx, &result.console_output).await?;

    // If -p flag, print the result
    if print_result {
        let output = match &result.value {
            serde_json::Value::Null => "undefined".to_string(),
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_else(|_| "undefined".to_string()),
        };
        ctx.println(&output).await?;
    }

    // Track if any operation returned an error (for exit code)
    // Note: This tracks all op errors, even those caught by JS .catch() handlers.
    // True "unhandled rejection" tracking would require inspecting JS promise state.
    let mut had_op_error = false;

    // Event loop: process pending ops until none remain
    // Note: We process ops one at a time to properly handle chains like:
    // sleep -> callback creates another sleep -> etc.
    //
    // IMPORTANT: The first batch of pending ops comes from execute()'s result,
    // not from take_pending_ops(). The execute() method takes pending ops and
    // returns them in result.pending_ops.
    let mut pending_ops = result.pending_ops;
    loop {
        if pending_ops.is_empty() {
            break;
        }

        for op in pending_ops.drain(..) {
            let result = process_op(ctx, &op.op_type).await;

            match result {
                Ok(value) => {
                    if let Err(e) = runtime.resolve(&op.id, &value) {
                        ctx.eprintln(&format!("node: failed to resolve promise: {e}"))
                            .await?;
                    }
                }
                Err(error) => {
                    had_op_error = true;
                    if let Err(e) = runtime.reject(&op.id, &error) {
                        ctx.eprintln(&format!("node: failed to reject promise: {e}"))
                            .await?;
                    }
                }
            }

            // Flush any console output from promise handlers
            let console_output = runtime.take_console_output();
            flush_console(ctx, &console_output).await?;
        }

        // Check for new pending ops (created during promise resolution)
        pending_ops = runtime.take_pending_ops();
    }

    // Return non-zero exit code if any operation failed
    if had_op_error {
        Ok(Exit::code(1))
    } else {
        Ok(Exit::success())
    }
}

/// Flush console output to appropriate streams.
async fn flush_console(
    ctx: &CmdContext,
    output: &[amla_js::ConsoleOutput],
) -> Result<(), crate::io_handle::IoError> {
    for entry in output {
        let msg = format!("{}\n", entry.message);
        match entry.level.as_str() {
            "error" | "warn" => {
                ctx.stderr_write_all(msg.as_bytes()).await?;
            }
            _ => {
                ctx.stdout_write_all(msg.as_bytes()).await?;
            }
        }
    }
    Ok(())
}

/// Convert a VFS error to appropriate errno code.
fn vfs_error_to_errno(err: &amla_vfs::VfsError) -> &'static str {
    use amla_vfs::VfsError;
    match err {
        VfsError::NotFound(_) => "ENOENT",
        VfsError::PermissionDenied(_) => "EACCES",
        VfsError::NotADirectory(_) => "ENOTDIR",
        VfsError::NotAFile(_) => "EISDIR",
        VfsError::AlreadyExists(_) => "EEXIST",
        VfsError::InvalidPath(_) => "EINVAL",
    }
}

/// Convert an IoError to appropriate errno code.
fn io_error_to_errno(err: &crate::io_handle::IoError) -> &'static str {
    // IoError is typically from VFS operations
    // Default to EIO for generic I/O errors
    let msg = err.to_string().to_lowercase();
    if msg.contains("not found") {
        "ENOENT"
    } else if msg.contains("permission") {
        "EACCES"
    } else if msg.contains("not a directory") {
        "ENOTDIR"
    } else if msg.contains("not a file") || msg.contains("is a directory") {
        "EISDIR"
    } else if msg.contains("exists") {
        "EEXIST"
    } else {
        "EIO"
    }
}

/// Process a pending operation and return the result or error.
async fn process_op(
    ctx: &CmdContext,
    op_type: &amla_js::OpType,
) -> Result<serde_json::Value, String> {
    use amla_js::OpType;
    use base64::Engine;
    use std::time::Duration;

    match op_type {
        OpType::Sleep { delay_ms } => {
            // Use scheduler timer via ctx.sleep()
            ctx.sleep(Duration::from_millis(*delay_ms))
                .await
                .map_err(|e| format!("sleep failed: {e}"))?;
            Ok(serde_json::Value::Null)
        }

        OpType::FsRead { path, options } => {
            let resolved = ctx.resolve_path(path);
            match ctx.read_file(&resolved).await {
                Ok(bytes) => {
                    // Handle encoding option
                    let encoding = options.encoding.as_deref().unwrap_or("utf-8");
                    match encoding {
                        "base64" => {
                            // Return as base64 for binary data
                            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            Ok(serde_json::Value::String(encoded))
                        }
                        "buffer" | "binary" => {
                            // Return as array of bytes
                            let arr: Vec<serde_json::Value> =
                                bytes.iter().map(|&b| serde_json::json!(b)).collect();
                            Ok(serde_json::Value::Array(arr))
                        }
                        _ => {
                            // Default: utf-8 with lossy conversion
                            // Try conversion (consumes bytes without cloning)
                            match String::from_utf8(bytes) {
                                Ok(s) => Ok(serde_json::Value::String(s)),
                                Err(e) => {
                                    // Not valid UTF-8 - recover bytes and use lossy conversion
                                    let bytes = e.into_bytes();
                                    let content = String::from_utf8_lossy(&bytes).into_owned();
                                    Ok(serde_json::Value::String(content))
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let errno = io_error_to_errno(&e);
                    Err(format!("{errno}: {e}"))
                }
            }
        }

        OpType::FsWrite {
            path,
            data,
            options,
        } => {
            let resolved = ctx.resolve_path(path);

            // Handle create_dirs option
            if options.create_dirs
                && let Some(parent) = std::path::Path::new(&resolved).parent()
            {
                let parent_str = parent.to_string_lossy();
                if !parent_str.is_empty() && parent_str != "/" {
                    let _ = ctx.mkdir_p(parent_str.as_ref());
                }
            }

            // Handle append option
            let write_result = if options.append {
                // Read existing content and append
                let existing = ctx.read_file(&resolved).await.unwrap_or_default();
                let mut combined = existing;
                combined.extend_from_slice(data.as_bytes());
                ctx.write_file(&resolved, &combined)
            } else {
                ctx.write_file(&resolved, data.as_bytes())
            };

            write_result.map_err(|e| {
                let errno = io_error_to_errno(&e);
                format!("{errno}: {e}")
            })?;
            Ok(serde_json::Value::Null)
        }

        OpType::FsReadDir { path } => {
            let resolved = ctx.resolve_path(path);
            let vfs = ctx.vfs();
            let entries = vfs.borrow().list_dir(&resolved).map_err(|e| {
                let errno = vfs_error_to_errno(&e);
                format!("{errno}: {e}")
            })?;
            let names: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| serde_json::Value::String(e.name.clone()))
                .collect();
            Ok(serde_json::Value::Array(names))
        }

        OpType::FsStat { path } => {
            let resolved = ctx.resolve_path(path);
            let vfs = ctx.vfs();
            let vfs_ref = vfs.borrow();

            // Check if path exists and get entry
            let entry = vfs_ref.stat(&resolved).map_err(|e| {
                let errno = vfs_error_to_errno(&e);
                format!("{errno}: {e}")
            })?;

            let is_dir = entry.is_dir();
            let is_file = entry.is_file();

            // Get size directly from entry (no separate read needed!)
            let size = match &entry {
                amla_vfs::Entry::File { content, .. } => content.len(),
                amla_vfs::Entry::Directory { .. } => 0,
            };

            Ok(serde_json::json!({
                "size": size,
                "isDirectory": is_dir,
                "isFile": is_file,
                "isSymbolicLink": false,  // VFS doesn't support symlinks
                "modifiedAt": 0
            }))
        }

        OpType::FsExists { path } => {
            let resolved = ctx.resolve_path(path);
            let vfs = ctx.vfs();
            let exists = vfs.borrow().exists(&resolved);
            Ok(serde_json::Value::Bool(exists))
        }

        OpType::FsUnlink { path } => {
            let resolved = ctx.resolve_path(path);
            ctx.remove(&resolved).map_err(|e| {
                let errno = io_error_to_errno(&e);
                format!("{errno}: {e}")
            })?;
            Ok(serde_json::Value::Null)
        }

        OpType::FsMkdir { path, recursive } => {
            let resolved = ctx.resolve_path(path);
            let result = if *recursive {
                ctx.mkdir_p(&resolved)
            } else {
                ctx.mkdir(&resolved)
            };
            result.map_err(|e| {
                let errno = io_error_to_errno(&e);
                format!("{errno}: {e}")
            })?;
            Ok(serde_json::Value::Null)
        }

        // Tool calls are routed to the host via the scheduler's host channel.
        // The host channel creates HostOpKind::Custom which becomes HostOpRequest::ToolCall.
        OpType::ToolCall { tool, params } => {
            let params_bytes = serde_json::to_vec(&params).unwrap_or_default();
            // Request the tool call via host channel
            let result_bytes = ctx
                .scheduler()
                .host()
                .custom(tool.clone(), params_bytes)
                .await
                .map_err(|e| format!("tool call '{tool}' failed: {e}"))?;
            // Parse result as JSON value
            serde_json::from_slice(&result_bytes)
                .map_err(|e| format!("tool result parse error: {e}"))
        }
        OpType::Fetch { url, .. } => Err(format!("fetch '{url}' not supported in standalone mode")),
        OpType::MemoryRead { key } => Err(format!(
            "memory read '{key}' not supported in standalone mode"
        )),
        OpType::MemoryWrite { key, .. } => Err(format!(
            "memory write '{key}' not supported in standalone mode"
        )),
        OpType::MemoryDelete { key } => Err(format!(
            "memory delete '{key}' not supported in standalone mode"
        )),
        OpType::Spawn { .. } => Err("spawn not supported in standalone mode".to_string()),
        OpType::LlmCall { model, .. } => Err(format!(
            "LLM call '{model}' not supported in standalone mode"
        )),
        OpType::Shell { command } => {
            // Execute shell command in a NEW isolated Shell instance.
            // This shell is a child of the current node task - if node is
            // cancelled, this shell task will also be cancelled (structured concurrency).

            // Create buffer handles to capture output
            let stdout_handle = crate::io_handle::IoHandle::buffer();
            let stderr_handle = crate::io_handle::IoHandle::buffer();

            // Create a new Shell instance with:
            // - Same scheduler (for proper task hierarchy and cancellation)
            // - Shared VFS reference (file changes are visible to all)
            // - Fresh environment, job table, last_exit (isolated state)
            // - Buffer handles for output capture
            let mut shell = crate::Shell::from_vfs_ref(ctx.scheduler(), ctx.vfs());

            // Set streams for output capture
            shell.set_stdout(stdout_handle.clone());
            shell.set_stderr(stderr_handle.clone());

            // Set cwd to match current context (ignore error if dir doesn't exist)
            let _ = shell.set_cwd(ctx.cwd());

            // Execute the command
            // Note: shell.execute() runs synchronously in this task context,
            // but any spawned child tasks (pipelines, etc.) are children of
            // the current node task and will be cancelled if node is cancelled.
            let exit_code = shell.execute(command).await.map_err(|e| e.to_string())?;

            // Capture output
            let stdout = stdout_handle
                .get_buffer()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let stderr = stderr_handle
                .get_buffer()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();

            Ok(serde_json::json!({
                "stdout": stdout,
                "stderr": stderr,
                "exitCode": exit_code
            }))
        }
    }
}

async fn read_stdin_to_string(ctx: &CmdContext) -> Result<String, crate::io_handle::IoError> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        match ctx.read_stdin(&mut chunk).await {
            Ok(0) => break, // EOF
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e),
        }
    }

    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

const HELP_TEXT: &str = r#"Usage: node [options] [script.js] [arguments]

Execute JavaScript code.

Options:
  -e, --eval <code>          Evaluate code instead of running a file
  -p, --print <code>         Evaluate and print result
  --max-old-space-size=<MB>  Set memory limit in megabytes (default: 64)
  -h, --help                 Show this help message
  -v, --version              Show version
  -                          Read script from stdin

Examples:
  node script.js                         Run a JavaScript file
  node -e "console.log('hi')"            Evaluate inline code
  node -p "1 + 2"                        Evaluate and print result
  echo "1+2" | node -                    Read from stdin
  node --max-old-space-size=128 app.js   Limit memory to 128MB"#;
