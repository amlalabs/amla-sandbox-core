//! wc - word, line, character, and byte count

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::{IoError, IoHandle};

use super::CommandResult;

/// wc [-l] [-w] [-c] [-m] [file...]
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut count_lines = false;
    let mut count_words = false;
    let mut count_bytes = false;
    let mut count_chars = false;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('l'))) => count_lines = true,
            Ok(Some(lexopt::Arg::Short('w'))) => count_words = true,
            Ok(Some(lexopt::Arg::Short('c'))) => count_bytes = true,
            Ok(Some(lexopt::Arg::Short('m'))) => count_chars = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if !count_lines && !count_words && !count_bytes && !count_chars {
        count_lines = true;
        count_words = true;
        count_bytes = true;
    }

    let mut total = Counts::default();

    if files.is_empty() {
        let counts = count_from_handle(&ctx.stdin).await?;
        output_counts(
            &ctx,
            &counts,
            None,
            count_lines,
            count_words,
            count_bytes,
            count_chars,
        )
        .await?;
    } else {
        for file_path in &files {
            match ctx.open_or_stdin(file_path).await {
                Ok(handle) => {
                    let counts = count_from_handle(&handle).await?;
                    output_counts(
                        &ctx,
                        &counts,
                        Some(file_path),
                        count_lines,
                        count_words,
                        count_bytes,
                        count_chars,
                    )
                    .await?;
                    total.add(&counts);
                }
                Err(e) => {
                    ctx.eprintln(&format!("wc: {file_path}: {e}")).await?;
                    return Ok(Exit::code(1));
                }
            }
        }

        if files.len() > 1 {
            output_counts(
                &ctx,
                &total,
                Some("total"),
                count_lines,
                count_words,
                count_bytes,
                count_chars,
            )
            .await?;
        }
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

#[derive(Default)]
struct Counts {
    lines: usize,
    words: usize,
    bytes: usize,
    chars: usize,
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        self.lines += other.lines;
        self.words += other.words;
        self.bytes += other.bytes;
        self.chars += other.chars;
    }
}

/// Stream count from an I/O handle - accumulates counts in constant memory.
async fn count_from_handle(handle: &IoHandle) -> Result<Counts, IoError> {
    let mut counts = Counts::default();
    let mut buffer = [0u8; 4096];
    let mut in_word = false;

    loop {
        let n = handle.read(&mut buffer).await?;
        if n == 0 {
            break;
        }

        counts.bytes += n;

        for &b in &buffer[..n] {
            // Count newlines
            if b == b'\n' {
                counts.lines += 1;
            }

            // Count UTF-8 characters (count bytes that are NOT continuation bytes)
            // Continuation bytes match pattern 10xxxxxx (0x80..0xBF)
            if (b & 0xC0) != 0x80 {
                counts.chars += 1;
            }

            // Count words (transitions from whitespace to non-whitespace)
            let is_whitespace = b.is_ascii_whitespace();
            if in_word && is_whitespace {
                in_word = false;
            } else if !in_word && !is_whitespace {
                in_word = true;
                counts.words += 1;
            }
        }
    }

    Ok(counts)
}

async fn output_counts(
    ctx: &CmdContext,
    counts: &Counts,
    filename: Option<&str>,
    count_lines: bool,
    count_words: bool,
    count_bytes: bool,
    count_chars: bool,
) -> Result<(), IoError> {
    let mut parts = Vec::new();

    if count_lines {
        parts.push(format!("{:>7}", counts.lines));
    }
    if count_words {
        parts.push(format!("{:>7}", counts.words));
    }
    if count_chars {
        parts.push(format!("{:>7}", counts.chars));
    }
    if count_bytes {
        parts.push(format!("{:>7}", counts.bytes));
    }

    let mut output = parts.join(" ");
    if let Some(name) = filename {
        output.push(' ');
        output.push_str(name);
    }

    ctx.println(&output).await?;
    Ok(())
}
