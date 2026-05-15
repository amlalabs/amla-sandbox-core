//! tail - output the last part of files

use std::collections::VecDeque;

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::{IoError, IoHandle};

use super::CommandResult;

/// tail [-n lines] [-c bytes] [-NUMBER] [file...]
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut num_lines: Option<usize> = None;
    let mut num_bytes: Option<usize> = None;
    let mut quiet = false;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('n'))) => {
                if let Ok(val) = parser.value() {
                    let s = val.to_string_lossy();
                    if let Some(rest) = s.strip_prefix('+') {
                        if let Ok(n) = rest.parse::<usize>() {
                            num_lines = Some(n);
                        }
                    } else if let Ok(n) = s.parse::<usize>() {
                        num_lines = Some(n);
                    }
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
                // Handle -NUMBER shorthand (e.g., -2 means -n 2)
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
        tail_from_handle(&ctx, &ctx.stdin, num_lines, num_bytes).await?;
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
                    tail_from_handle(&ctx, &handle, num_lines, num_bytes).await?;
                }
                Err(e) => {
                    ctx.eprintln(&format!("tail: {file_path}: {e}")).await?;
                    return Ok(Exit::code(1));
                }
            }
        }
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

/// Stream tail from an I/O handle using a ring buffer for bounded memory.
async fn tail_from_handle(
    ctx: &CmdContext,
    handle: &IoHandle,
    num_lines: Option<usize>,
    num_bytes: Option<usize>,
) -> Result<(), IoError> {
    let mut buffer = [0u8; 4096];

    if let Some(n) = num_bytes {
        // Byte mode: keep ring buffer of last n bytes
        let mut ring: VecDeque<u8> = VecDeque::with_capacity(n);

        loop {
            let bytes_read = handle.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }

            for &b in &buffer[..bytes_read] {
                if ring.len() >= n {
                    ring.pop_front();
                }
                ring.push_back(b);
            }
        }

        // Output the ring buffer
        let bytes: Vec<u8> = ring.into_iter().collect();
        ctx.stdout_write_all(&bytes).await?;
    } else if let Some(n) = num_lines {
        // Line mode: keep ring buffer of last n lines
        let mut ring: VecDeque<Vec<u8>> = VecDeque::with_capacity(n);
        let mut pending = Vec::new();

        loop {
            let bytes_read = handle.read(&mut buffer).await?;
            if bytes_read == 0 {
                // EOF - add any remaining partial line
                if !pending.is_empty() {
                    if ring.len() >= n {
                        ring.pop_front();
                    }
                    ring.push_back(pending);
                }
                break;
            }

            pending.extend_from_slice(&buffer[..bytes_read]);

            // Process complete lines
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=pos).collect();
                if ring.len() >= n {
                    ring.pop_front();
                }
                ring.push_back(line);
            }
        }

        // Output the ring buffer
        for line in ring {
            ctx.stdout_write_all(&line).await?;
            // Add newline if line doesn't end with one (partial final line)
            if !line.ends_with(b"\n") {
                ctx.stdout_write_all(b"\n").await?;
            }
        }
    }

    Ok(())
}
