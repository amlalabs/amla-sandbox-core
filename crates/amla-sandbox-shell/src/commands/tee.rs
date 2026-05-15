//! tee - read from stdin and write to stdout and files
//!
//! Streams data chunk-by-chunk for memory efficiency.

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;

use super::CommandResult;

const HELP: &str = "\
tee - read from stdin and write to stdout and files

Usage: tee [OPTIONS] [FILE...]

Options:
  -h, --help    Show this help message
  -a            Append to files instead of overwriting
  -i            Ignore SIGINT (no-op in sandbox)

Reads from stdin and writes to both stdout and the specified files.
Streams in 4KB chunks for memory efficiency.
";

pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut append = false;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('h'))) | Ok(Some(lexopt::Arg::Long("help"))) => {
                ctx.stdout_write_all(HELP.as_bytes()).await?;
                return Ok(Exit::success());
            }
            Ok(Some(lexopt::Arg::Short('a'))) | Ok(Some(lexopt::Arg::Long("append"))) => {
                append = true;
            }
            Ok(Some(lexopt::Arg::Short('i'))) => {} // Ignore SIGINT (no-op in sandbox)
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // If not appending, truncate/create files first
    if !append {
        for file_path in &files {
            if let Err(e) = ctx.write_file(file_path, b"") {
                ctx.eprintln(&format!("tee: {file_path}: {e}")).await?;
                // Continue anyway - we'll try to write
            }
        }
    }

    // Stream stdin to stdout and files
    let mut buffer = [0u8; 4096];
    let mut exit_code = 0;

    loop {
        let n = ctx.stdin.read(&mut buffer).await?;
        if n == 0 {
            break;
        }

        let chunk = &buffer[..n];

        // Write to stdout
        ctx.stdout_write_all(chunk).await?;

        // Write to each file
        for file_path in &files {
            if let Err(e) = ctx.append_file(file_path, chunk) {
                ctx.eprintln(&format!("tee: {file_path}: {e}")).await?;
                exit_code = 1;
            }
        }
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::code(exit_code))
}
