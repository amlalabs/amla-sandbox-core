//! cat - concatenate and print files

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::IoError;

use super::CommandResult;

const HELP: &str = "\
cat - concatenate and print files

Usage: cat [OPTIONS] [FILE...]

Options:
  -h, --help    Show this help message
  -n            Number all output lines
  -b            Number non-blank lines only

If no files given, read from stdin. Use '-' for stdin explicitly.
";

pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut number_lines = false;
    let mut number_nonblank = false;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('h'))) | Ok(Some(lexopt::Arg::Long("help"))) => {
                ctx.stdout_write_all(HELP.as_bytes()).await?;
                return Ok(Exit::success());
            }
            Ok(Some(lexopt::Arg::Short('n'))) => number_lines = true,
            Ok(Some(lexopt::Arg::Short('b'))) => number_nonblank = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let mut line_number = 1u64;

    if files.is_empty() {
        // Read from stdin
        cat_handle(
            &ctx,
            &ctx.stdin,
            number_lines,
            number_nonblank,
            &mut line_number,
        )
        .await?;
    } else {
        for file_path in &files {
            let handle = match ctx.open_or_stdin(file_path).await {
                Ok(h) => h,
                Err(e) => {
                    ctx.eprintln(&format!("cat: {file_path}: {e}")).await?;
                    return Ok(Exit::code(1));
                }
            };

            cat_handle(
                &ctx,
                &handle,
                number_lines,
                number_nonblank,
                &mut line_number,
            )
            .await?;
        }
    }

    // Flush stdout to ensure any buffered output (from line-buffered HostStream) is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

/// Cat from an I/O handle.
async fn cat_handle(
    ctx: &CmdContext,
    handle: &crate::io_handle::IoHandle,
    number_lines: bool,
    number_nonblank: bool,
    line_number: &mut u64,
) -> Result<(), IoError> {
    let mut buffer = [0u8; 4096];

    if number_lines || number_nonblank {
        // Need to process line by line
        let mut pending = Vec::new();

        loop {
            let n = handle.read(&mut buffer).await?;
            if n == 0 {
                // Flush any remaining content
                if !pending.is_empty() {
                    let line = String::from_utf8_lossy(&pending);
                    if number_nonblank && line.trim().is_empty() {
                        ctx.stdout_write_all(b"\n").await?;
                    } else {
                        let output = format!("{line_number:6}\t{line}\n");
                        ctx.stdout_write_all(output.as_bytes()).await?;
                        *line_number += 1;
                    }
                }
                break;
            }

            pending.extend_from_slice(&buffer[..n]);

            // Process complete lines
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let line_bytes = pending.drain(..=pos).collect::<Vec<_>>();
                let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]); // Remove \n

                if number_nonblank && line.is_empty() {
                    ctx.stdout_write_all(b"\n").await?;
                } else {
                    let output = format!("{line_number:6}\t{line}\n");
                    ctx.stdout_write_all(output.as_bytes()).await?;
                    *line_number += 1;
                }
            }
        }
    } else {
        // Simple passthrough
        loop {
            let n = handle.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            ctx.stdout_write_all(&buffer[..n]).await?;
        }
    }

    Ok(())
}
