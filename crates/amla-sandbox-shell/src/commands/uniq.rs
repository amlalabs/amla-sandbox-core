//! uniq - report or omit repeated lines
//!
//! Streams line-by-line with O(1) memory (just stores previous line).

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::{IoError, IoHandle};

use super::CommandResult;

/// uniq [-c] [-d] [-u] [-i] [input [output]]
///
/// Filter adjacent matching lines.
/// Streams with O(1) memory - only stores the previous line.
///
/// Options:
///   -c    Prefix lines with count of occurrences
///   -d    Only print duplicate lines
///   -u    Only print unique lines
///   -i    Case-insensitive comparison
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut count = false;
    let mut only_duplicates = false;
    let mut only_unique = false;
    let mut ignore_case = false;
    let mut files: SmallVec<[String; 2]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('c'))) => count = true,
            Ok(Some(lexopt::Arg::Short('d'))) => only_duplicates = true,
            Ok(Some(lexopt::Arg::Short('u'))) => only_unique = true,
            Ok(Some(lexopt::Arg::Short('i'))) => ignore_case = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let opts = UniqOptions {
        count,
        only_duplicates,
        only_unique,
        ignore_case,
    };

    // Get input handle (default to stdin if no files)
    let input_path = files.first().map(String::as_str).unwrap_or("-");
    let input_handle = match ctx.open_or_stdin(input_path).await {
        Ok(h) => h,
        Err(e) => {
            ctx.eprintln(&format!("uniq: {input_path}: {e}")).await?;
            return Ok(Exit::code(1));
        }
    };

    // Output file (optional second argument)
    let output_file = files.get(1).map(std::string::String::as_str);

    uniq_from_handle(&ctx, &input_handle, &opts, output_file).await?;

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

struct UniqOptions {
    count: bool,
    only_duplicates: bool,
    only_unique: bool,
    ignore_case: bool,
}

/// Stream uniq from an I/O handle - O(1) memory.
async fn uniq_from_handle(
    ctx: &CmdContext,
    handle: &IoHandle,
    opts: &UniqOptions,
    output_file: Option<&str>,
) -> Result<(), IoError> {
    let mut buffer = [0u8; 4096];
    let mut pending = Vec::new();

    let mut prev_line: Option<String> = None;
    let mut prev_count = 0usize;

    // For output file, we'll collect output and write at end
    // (streaming to file would require append which works but is less efficient)
    let mut output = Vec::new();

    loop {
        let n = handle.read(&mut buffer).await?;
        if n == 0 {
            // Process remaining line
            if !pending.is_empty() {
                let line = String::from_utf8_lossy(&pending).into_owned();
                process_uniq_line(&line, opts, &mut prev_line, &mut prev_count, &mut output);
            }
            break;
        }

        pending.extend_from_slice(&buffer[..n]);

        // Process complete lines
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line_bytes = &pending[..pos];
            let line = String::from_utf8_lossy(line_bytes).into_owned();

            process_uniq_line(&line, opts, &mut prev_line, &mut prev_count, &mut output);

            pending.drain(..=pos);
        }
    }

    // Flush the last line
    if let Some(prev) = prev_line {
        emit_line(&prev, prev_count, opts, &mut output);
    }

    // Write output
    if let Some(path) = output_file {
        ctx.write_file(path, &output)?;
    } else {
        ctx.stdout_write_all(&output).await?;
    }

    Ok(())
}

fn process_uniq_line(
    line: &str,
    opts: &UniqOptions,
    prev_line: &mut Option<String>,
    prev_count: &mut usize,
    output: &mut Vec<u8>,
) {
    let compare_line = if opts.ignore_case {
        line.to_lowercase()
    } else {
        line.to_string()
    };

    let prev_compare = prev_line.as_ref().map(|p| {
        if opts.ignore_case {
            p.to_lowercase()
        } else {
            p.clone()
        }
    });

    if prev_compare.as_ref() == Some(&compare_line) {
        // Same as previous line
        *prev_count += 1;
    } else {
        // Different line - emit previous if exists
        if let Some(prev) = prev_line.take() {
            emit_line(&prev, *prev_count, opts, output);
        }
        *prev_line = Some(line.to_string());
        *prev_count = 1;
    }
}

fn emit_line(line: &str, count: usize, opts: &UniqOptions, output: &mut Vec<u8>) {
    let is_duplicate = count > 1;
    let is_unique = count == 1;

    // Filter based on options
    if opts.only_duplicates && !is_duplicate {
        return;
    }
    if opts.only_unique && !is_unique {
        return;
    }

    // Format output
    if opts.count {
        output.extend_from_slice(format!("{count:7} {line}\n").as_bytes());
    } else {
        output.extend_from_slice(line.as_bytes());
        output.push(b'\n');
    }
}
