//! grep - search for patterns in files

use std::collections::VecDeque;

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::{IoError, IoHandle};

use super::CommandResult;

/// grep [-i] [-v] [-n] [-c] [-l] [-q] [-F] [-A num] [-B num] [-C num] pattern [file...]
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut case_insensitive = false;
    let mut invert = false;
    let mut line_numbers = false;
    let mut count_only = false;
    let mut files_only = false;
    let mut quiet = false;
    let mut fixed_string = false;
    let mut after_context = 0usize;
    let mut before_context = 0usize;
    let mut pattern: Option<String> = None;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('i'))) => case_insensitive = true,
            Ok(Some(lexopt::Arg::Short('v'))) => invert = true,
            Ok(Some(lexopt::Arg::Short('n'))) => line_numbers = true,
            Ok(Some(lexopt::Arg::Short('c'))) => count_only = true,
            Ok(Some(lexopt::Arg::Short('l'))) => files_only = true,
            Ok(Some(lexopt::Arg::Short('q'))) => quiet = true,
            Ok(Some(lexopt::Arg::Short('E'))) => {}
            Ok(Some(lexopt::Arg::Short('F'))) => fixed_string = true,
            Ok(Some(lexopt::Arg::Short('A'))) => {
                if let Ok(val) = parser.value() {
                    after_context = val.to_string_lossy().parse().unwrap_or(0);
                }
            }
            Ok(Some(lexopt::Arg::Short('B'))) => {
                if let Ok(val) = parser.value() {
                    before_context = val.to_string_lossy().parse().unwrap_or(0);
                }
            }
            Ok(Some(lexopt::Arg::Short('C'))) => {
                if let Ok(val) = parser.value() {
                    let n = val.to_string_lossy().parse().unwrap_or(0);
                    before_context = n;
                    after_context = n;
                }
            }
            Ok(Some(lexopt::Arg::Short('e'))) => {
                if let Ok(val) = parser.value() {
                    pattern = Some(val.to_string_lossy().into_owned());
                }
            }
            Ok(Some(lexopt::Arg::Value(val))) => {
                let s = val.to_string_lossy().into_owned();
                if pattern.is_none() {
                    pattern = Some(s);
                } else {
                    files.push(s);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let pattern = if let Some(p) = pattern {
        p
    } else {
        ctx.eprintln("grep: missing pattern").await?;
        return Ok(Exit::code(2));
    };

    // Use Cow to avoid clone when not case_insensitive
    let pattern_lower: std::borrow::Cow<'_, str> = if case_insensitive {
        std::borrow::Cow::Owned(pattern.to_lowercase())
    } else {
        std::borrow::Cow::Borrowed(&pattern)
    };

    let opts = GrepOptions {
        case_insensitive,
        invert,
        line_numbers,
        count_only,
        files_only,
        quiet,
        fixed_string,
        before_context,
        after_context,
    };

    let mut match_found = false;
    let print_filename = files.len() > 1;

    if files.is_empty() {
        // Read from stdin
        let result = grep_from_handle(&ctx, &ctx.stdin, &pattern_lower, &opts, None).await?;
        if result.found {
            match_found = true;
        }
        if opts.count_only && !opts.quiet {
            ctx.println(&result.count.to_string()).await?;
        }
    } else {
        for file_path in &files {
            let filename = if print_filename {
                Some(file_path.as_str())
            } else {
                None
            };

            let handle = match ctx.open_or_stdin(file_path).await {
                Ok(h) => h,
                Err(e) => {
                    ctx.eprintln(&format!("grep: {file_path}: {e}")).await?;
                    continue;
                }
            };

            let result = grep_from_handle(&ctx, &handle, &pattern_lower, &opts, filename).await?;

            if opts.count_only && !opts.quiet {
                if print_filename {
                    ctx.println(&format!("{}:{}", file_path, result.count))
                        .await?;
                } else {
                    ctx.println(&result.count.to_string()).await?;
                }
            }

            if result.found {
                match_found = true;
                if opts.quiet {
                    ctx.stdout.flush().await?;
                    return Ok(Exit::success());
                }
            }
        }
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::code(i32::from(!match_found)))
}

struct GrepOptions {
    case_insensitive: bool,
    invert: bool,
    line_numbers: bool,
    count_only: bool,
    files_only: bool,
    quiet: bool,
    fixed_string: bool,
    before_context: usize,
    after_context: usize,
}

struct GrepResult {
    found: bool,
    count: usize,
}

/// A line with its number and content.
#[derive(Clone)]
struct Line {
    num: usize,
    content: String,
}

/// Stream grep from an I/O handle with context support.
async fn grep_from_handle(
    ctx: &CmdContext,
    handle: &IoHandle,
    pattern: &str,
    opts: &GrepOptions,
    filename: Option<&str>,
) -> Result<GrepResult, IoError> {
    let mut buffer = [0u8; 4096];
    let mut pending = Vec::new();
    let mut line_num = 0usize;
    let mut match_found = false;
    let mut count = 0usize;

    // Ring buffer for before-context
    let mut before_ring: VecDeque<Line> = VecDeque::with_capacity(opts.before_context + 1);

    // Track after-context: how many more lines to print after a match
    let mut after_remaining = 0usize;

    // Track last printed line to avoid duplicates and for separator logic
    let mut last_printed_line = 0usize;
    let mut any_output = false;

    loop {
        let n = handle.read(&mut buffer).await?;
        if n == 0 {
            // EOF - process any remaining partial line
            if !pending.is_empty() {
                line_num += 1;
                let line_content = String::from_utf8_lossy(&pending).into_owned();
                let line = Line {
                    num: line_num,
                    content: line_content,
                };

                let result = process_line_with_context(
                    ctx,
                    &line,
                    pattern,
                    opts,
                    filename,
                    &mut before_ring,
                    &mut after_remaining,
                    &mut last_printed_line,
                    &mut any_output,
                )
                .await?;

                if let Some(r) = result {
                    match_found = true;
                    count += 1;
                    if r.early_exit {
                        return Ok(GrepResult { found: true, count });
                    }
                }
            }
            break;
        }

        pending.extend_from_slice(&buffer[..n]);

        // Process complete lines
        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            line_num += 1;
            let line_bytes = &pending[..pos];
            let line_content = String::from_utf8_lossy(line_bytes).into_owned();
            let line = Line {
                num: line_num,
                content: line_content,
            };

            let result = process_line_with_context(
                ctx,
                &line,
                pattern,
                opts,
                filename,
                &mut before_ring,
                &mut after_remaining,
                &mut last_printed_line,
                &mut any_output,
            )
            .await?;

            if let Some(r) = result {
                match_found = true;
                count += 1;
                if r.early_exit {
                    return Ok(GrepResult { found: true, count });
                }
            }

            pending.drain(..=pos);
        }
    }

    Ok(GrepResult {
        found: match_found,
        count,
    })
}

struct LineResult {
    early_exit: bool,
}

/// Process a line with context support.
async fn process_line_with_context(
    ctx: &CmdContext,
    line: &Line,
    pattern: &str,
    opts: &GrepOptions,
    filename: Option<&str>,
    before_ring: &mut VecDeque<Line>,
    after_remaining: &mut usize,
    last_printed_line: &mut usize,
    any_output: &mut bool,
) -> Result<Option<LineResult>, IoError> {
    // Use Cow to avoid clone when not case_insensitive
    let line_to_check: std::borrow::Cow<'_, str> = if opts.case_insensitive {
        std::borrow::Cow::Owned(line.content.to_lowercase())
    } else {
        std::borrow::Cow::Borrowed(&line.content)
    };

    let matches = if opts.fixed_string {
        line_to_check.contains(pattern)
    } else {
        simple_pattern_match(&line_to_check, pattern)
    };

    let is_match = if opts.invert { !matches } else { matches };

    if is_match {
        // We have a match
        if opts.quiet {
            return Ok(Some(LineResult { early_exit: true }));
        }

        if opts.files_only {
            if let Some(name) = filename {
                ctx.println(name).await?;
            }
            return Ok(Some(LineResult { early_exit: true }));
        }

        if !opts.count_only {
            // Print separator if there's a gap (only when context is enabled)
            let has_context = opts.before_context > 0 || opts.after_context > 0;
            if has_context
                && *any_output
                && *last_printed_line > 0
                && line.num > *last_printed_line + 1
            {
                ctx.println("--").await?;
            }

            // Print before-context (lines we haven't printed yet)
            for ctx_line in before_ring.iter() {
                if ctx_line.num > *last_printed_line {
                    print_line(ctx, ctx_line, filename, opts.line_numbers, '-').await?;
                    *last_printed_line = ctx_line.num;
                }
            }

            // Print the matching line
            print_line(ctx, line, filename, opts.line_numbers, ':').await?;
            *last_printed_line = line.num;
            *any_output = true;

            // Reset after-context counter
            *after_remaining = opts.after_context;
        }

        // Clear before ring since we've used it
        before_ring.clear();

        return Ok(Some(LineResult { early_exit: false }));
    }

    // Not a match - handle context
    if !opts.count_only {
        if *after_remaining > 0 {
            // Print as after-context
            if line.num > *last_printed_line {
                print_line(ctx, line, filename, opts.line_numbers, '-').await?;
                *last_printed_line = line.num;
            }
            *after_remaining -= 1;
        } else if opts.before_context > 0 {
            // Add to before-context ring buffer
            if before_ring.len() >= opts.before_context {
                before_ring.pop_front();
            }
            before_ring.push_back(line.clone());
        }
    }

    Ok(None)
}

/// Print a line with optional filename and line number.
async fn print_line(
    ctx: &CmdContext,
    line: &Line,
    filename: Option<&str>,
    show_line_num: bool,
    sep: char,
) -> Result<(), IoError> {
    let output = match (filename, show_line_num) {
        (Some(f), true) => format!("{}{}{}{}{}", f, sep, line.num, sep, line.content),
        (Some(f), false) => format!("{}{}{}", f, sep, line.content),
        (None, true) => format!("{}{}{}", line.num, sep, line.content),
        (None, false) => line.content.clone(),
    };
    ctx.println(&output).await?;
    Ok(())
}

fn simple_pattern_match(text: &str, pattern: &str) -> bool {
    // Handle ^...$ (exact match)
    if let Some(inner) = pattern.strip_prefix('^') {
        if let Some(inner) = inner.strip_suffix('$') {
            return text == inner;
        }
        return text.starts_with(inner);
    }
    // Handle ...$ (ends with)
    if let Some(prefix) = pattern.strip_suffix('$') {
        return text.ends_with(prefix);
    }
    if pattern.contains(".*") {
        let parts: Vec<&str> = pattern.split(".*").collect();
        let mut pos = 0;
        for part in parts {
            if part.is_empty() {
                continue;
            }
            if let Some(idx) = text[pos..].find(part) {
                pos += idx + part.len();
            } else {
                return false;
            }
        }
        return true;
    }
    text.contains(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_pattern_match() {
        assert!(simple_pattern_match("hello world", "world"));
        assert!(simple_pattern_match("hello world", "^hello"));
        assert!(simple_pattern_match("hello world", "world$"));
        assert!(simple_pattern_match("hello world", "^hello world$"));
        assert!(simple_pattern_match("hello world", "hel.*rld"));
        assert!(!simple_pattern_match("hello world", "^world"));
        assert!(!simple_pattern_match("hello world", "hello$"));
    }
}
