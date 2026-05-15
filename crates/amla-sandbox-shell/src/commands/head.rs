//! head - output the first part of files

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::{IoError, IoHandle};

use super::CommandResult;

/// head [-n lines] [-c bytes] [-NUMBER] [file...]
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut num_lines: Option<usize> = None;
    let mut num_bytes: Option<usize> = None;
    let mut quiet = false;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('n'))) => {
                if let Ok(val) = parser.value()
                    && let Ok(n) = val.to_string_lossy().parse::<usize>()
                {
                    num_lines = Some(n);
                }
            }
            Ok(Some(lexopt::Arg::Short('c'))) => {
                if let Ok(val) = parser.value()
                    && let Ok(n) = val.to_string_lossy().parse::<usize>()
                {
                    num_bytes = Some(n);
                }
            }
            Ok(Some(lexopt::Arg::Short('q'))) => quiet = true,
            Ok(Some(lexopt::Arg::Short(c))) if c.is_ascii_digit() => {
                // Handle -NUMBER shorthand (e.g., -3 means -n 3)
                let mut num_str = String::new();
                num_str.push(c);
                // Get any remaining value attached to this option (e.g., -10 or -123)
                if let Some(v) = parser.optional_value() {
                    num_str.push_str(&v.to_string_lossy());
                }
                if let Ok(n) = num_str.parse::<usize>() {
                    num_lines = Some(n);
                }
            }
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if num_lines.is_none() && num_bytes.is_none() {
        num_lines = Some(10);
    }

    let print_headers = !quiet && files.len() > 1;

    if files.is_empty() {
        // Read from stdin
        head_from_handle(&ctx, &ctx.stdin, num_lines, num_bytes).await?;
    } else {
        let mut first = true;
        for file_path in &files {
            if print_headers {
                if !first {
                    ctx.stdout_write_all(b"\n").await?;
                }
                ctx.println(&format!("==> {file_path} <==")).await?;
                first = false;
            }

            match ctx.open_or_stdin(file_path).await {
                Ok(handle) => {
                    head_from_handle(&ctx, &handle, num_lines, num_bytes).await?;
                }
                Err(e) => {
                    ctx.eprintln(&format!("head: {file_path}: {e}")).await?;
                    return Ok(Exit::code(1));
                }
            }
        }
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

/// Stream head from an I/O handle - stops early once limit reached.
async fn head_from_handle(
    ctx: &CmdContext,
    handle: &IoHandle,
    num_lines: Option<usize>,
    num_bytes: Option<usize>,
) -> Result<(), IoError> {
    let mut buffer = [0u8; 4096];

    if let Some(n) = num_bytes {
        // Byte mode: output exactly n bytes then stop
        let mut remaining = n;
        while remaining > 0 {
            let to_read = remaining.min(buffer.len());
            let bytes_read = handle.read(&mut buffer[..to_read]).await?;
            if bytes_read == 0 {
                break;
            }
            ctx.stdout_write_all(&buffer[..bytes_read]).await?;
            remaining -= bytes_read;
        }
    } else if let Some(n) = num_lines {
        // Line mode: output n lines then stop
        let mut lines_output = 0;
        let mut pending = Vec::new();

        'outer: loop {
            let bytes_read = handle.read(&mut buffer).await?;
            if bytes_read == 0 {
                // EOF - output any remaining partial line
                if !pending.is_empty() && lines_output < n {
                    ctx.stdout_write_all(&pending).await?;
                    if !pending.ends_with(b"\n") {
                        ctx.stdout_write_all(b"\n").await?;
                    }
                }
                break;
            }

            pending.extend_from_slice(&buffer[..bytes_read]);

            // Process complete lines
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                if lines_output >= n {
                    break 'outer;
                }
                // Output line including newline
                ctx.stdout_write_all(&pending[..=pos]).await?;
                pending.drain(..=pos);
                lines_output += 1;
            }
        }
    }

    Ok(())
}
