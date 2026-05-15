//! cut - remove sections from each line
//!
//! Streams line-by-line for memory efficiency.

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::{IoError, IoHandle};

use super::CommandResult;

/// cut -d delimiter -f fields [file...]
/// cut -c characters [file...]
/// cut -b bytes [file...]
///
/// Extract selected portions of each line.
/// Streams line-by-line for memory efficiency.
///
/// Options:
///   -d delim    Use delim as field delimiter (default: TAB)
///   -f fields   Select only these fields (1-indexed)
///   -c chars    Select only these characters
///   -b bytes    Select only these bytes
///   -s          Only print lines containing delimiter
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut delimiter = b'\t';
    let mut fields: Option<Vec<Range>> = None;
    let mut chars: Option<Vec<Range>> = None;
    let mut bytes: Option<Vec<Range>> = None;
    let mut only_delimited = false;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('d'))) => {
                if let Ok(val) = parser.value() {
                    let s = val.to_string_lossy();
                    if let Some(c) = s.bytes().next() {
                        delimiter = c;
                    }
                }
            }
            Ok(Some(lexopt::Arg::Short('f'))) => {
                if let Ok(val) = parser.value() {
                    fields = Some(parse_ranges(&val.to_string_lossy()));
                }
            }
            Ok(Some(lexopt::Arg::Short('c'))) => {
                if let Ok(val) = parser.value() {
                    chars = Some(parse_ranges(&val.to_string_lossy()));
                }
            }
            Ok(Some(lexopt::Arg::Short('b'))) => {
                if let Ok(val) = parser.value() {
                    bytes = Some(parse_ranges(&val.to_string_lossy()));
                }
            }
            Ok(Some(lexopt::Arg::Short('s'))) => only_delimited = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    // Must have exactly one of -f, -c, -b
    let mode = match (&fields, &chars, &bytes) {
        (Some(_), None, None) => CutMode::Fields,
        (None, Some(_), None) => CutMode::Chars,
        (None, None, Some(_)) => CutMode::Bytes,
        _ => {
            ctx.eprintln("cut: you must specify exactly one of -b, -c, or -f")
                .await?;
            return Ok(Exit::code(1));
        }
    };

    let ranges = fields.or(chars).or(bytes).unwrap();

    if files.is_empty() {
        cut_from_handle(&ctx, &ctx.stdin, &mode, &ranges, delimiter, only_delimited).await?;
    } else {
        for file_path in &files {
            let handle = match ctx.open_or_stdin(file_path).await {
                Ok(h) => h,
                Err(e) => {
                    ctx.eprintln(&format!("cut: {file_path}: {e}")).await?;
                    continue;
                }
            };
            cut_from_handle(&ctx, &handle, &mode, &ranges, delimiter, only_delimited).await?;
        }
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

#[derive(Clone, Copy)]
enum CutMode {
    Fields,
    Chars,
    Bytes,
}

/// A range like 1, 1-3, -3, 3-
#[derive(Clone, Copy)]
struct Range {
    start: Option<usize>, // None means from beginning
    end: Option<usize>,   // None means to end
}

impl Range {
    fn contains(&self, idx: usize) -> bool {
        let start = self.start.unwrap_or(1);
        match self.end {
            Some(end) => idx >= start && idx <= end,
            None => idx >= start,
        }
    }
}

fn parse_ranges(s: &str) -> Vec<Range> {
    let mut ranges = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some(pos) = part.find('-') {
            let (left, right) = part.split_at(pos);
            let right = &right[1..]; // Skip the '-'

            let start = if left.is_empty() {
                None
            } else {
                left.parse().ok()
            };
            let end = if right.is_empty() {
                None
            } else {
                right.parse().ok()
            };

            ranges.push(Range { start, end });
        } else if let Ok(n) = part.parse::<usize>() {
            ranges.push(Range {
                start: Some(n),
                end: Some(n),
            });
        }
    }
    ranges
}

fn in_ranges(idx: usize, ranges: &[Range]) -> bool {
    ranges.iter().any(|r| r.contains(idx))
}

/// Stream cut from an I/O handle.
async fn cut_from_handle(
    ctx: &CmdContext,
    handle: &IoHandle,
    mode: &CutMode,
    ranges: &[Range],
    delimiter: u8,
    only_delimited: bool,
) -> Result<(), IoError> {
    let mut buffer = [0u8; 4096];
    let mut pending = Vec::new();

    loop {
        let n = handle.read(&mut buffer).await?;
        if n == 0 {
            // Process remaining line
            if !pending.is_empty() {
                process_line(ctx, &pending, mode, ranges, delimiter, only_delimited).await?;
            }
            break;
        }

        pending.extend_from_slice(&buffer[..n]);

        // Process complete lines
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line = &pending[..pos];
            process_line(ctx, line, mode, ranges, delimiter, only_delimited).await?;
            pending.drain(..=pos);
        }
    }

    Ok(())
}

async fn process_line(
    ctx: &CmdContext,
    line: &[u8],
    mode: &CutMode,
    ranges: &[Range],
    delimiter: u8,
    only_delimited: bool,
) -> Result<(), IoError> {
    match mode {
        CutMode::Fields => {
            let fields: Vec<&[u8]> = line.split(|&b| b == delimiter).collect();

            // If only_delimited and line has no delimiter, skip
            if only_delimited && fields.len() == 1 {
                return Ok(());
            }

            let mut first = true;
            for (i, field) in fields.iter().enumerate() {
                if in_ranges(i + 1, ranges) {
                    if !first {
                        ctx.stdout_write_all(&[delimiter]).await?;
                    }
                    ctx.stdout_write_all(field).await?;
                    first = false;
                }
            }
            ctx.stdout_write_all(b"\n").await?;
        }
        CutMode::Chars | CutMode::Bytes => {
            // For simplicity, treat chars and bytes the same (byte mode)
            // A proper implementation would handle UTF-8 for -c
            let mut output = Vec::new();
            for (i, &byte) in line.iter().enumerate() {
                if in_ranges(i + 1, ranges) {
                    output.push(byte);
                }
            }
            ctx.stdout_write_all(&output).await?;
            ctx.stdout_write_all(b"\n").await?;
        }
    }

    Ok(())
}
