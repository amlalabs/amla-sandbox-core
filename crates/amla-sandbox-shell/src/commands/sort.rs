//! sort - sort lines of text
//!
//! Note: Sorting requires all input in memory - cannot stream.

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::{IoError, IoHandle};

use super::CommandResult;

/// sort [-r] [-n] [-u] [-f] [-k field] [file...]
///
/// Sort lines of text.
/// Note: Requires all input in memory to sort.
///
/// Options:
///   -r    Reverse sort order
///   -n    Numeric sort
///   -u    Output only unique lines
///   -f    Fold lowercase to uppercase (case-insensitive)
///   -k N  Sort by field N (1-indexed)
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut reverse = false;
    let mut numeric = false;
    let mut unique = false;
    let mut ignore_case = false;
    let mut key_field: Option<usize> = None;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('r'))) => reverse = true,
            Ok(Some(lexopt::Arg::Short('n'))) => numeric = true,
            Ok(Some(lexopt::Arg::Short('u'))) => unique = true,
            Ok(Some(lexopt::Arg::Short('f'))) => ignore_case = true,
            Ok(Some(lexopt::Arg::Short('k'))) => {
                if let Ok(val) = parser.value() {
                    // Parse key spec - just take the field number
                    let spec = val.to_string_lossy();
                    // Handle "1" or "1,1" or "1.1" format
                    let field_str = spec.split(&[',', '.'][..]).next().unwrap_or("1");
                    key_field = field_str.parse().ok();
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

    let opts = SortOptions {
        reverse,
        numeric,
        unique,
        ignore_case,
        key_field,
    };

    // Collect all lines
    let mut lines = Vec::new();

    if files.is_empty() {
        collect_lines(&ctx.stdin, &mut lines).await?;
    } else {
        for file_path in &files {
            let handle = match ctx.open_or_stdin(file_path).await {
                Ok(h) => h,
                Err(e) => {
                    ctx.eprintln(&format!("sort: {file_path}: {e}")).await?;
                    continue;
                }
            };
            collect_lines(&handle, &mut lines).await?;
        }
    }

    // Sort lines
    sort_lines(&mut lines, &opts);

    // Output
    let mut prev: Option<&str> = None;
    for line in &lines {
        if opts.unique {
            if prev == Some(line.as_str()) {
                continue;
            }
            prev = Some(line);
        }
        ctx.println(line).await?;
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

struct SortOptions {
    reverse: bool,
    numeric: bool,
    unique: bool,
    ignore_case: bool,
    key_field: Option<usize>,
}

async fn collect_lines(handle: &IoHandle, lines: &mut Vec<String>) -> Result<(), IoError> {
    let mut buffer = [0u8; 4096];
    let mut pending = Vec::new();

    loop {
        let n = handle.read(&mut buffer).await?;
        if n == 0 {
            if !pending.is_empty() {
                lines.push(String::from_utf8_lossy(&pending).into_owned());
            }
            break;
        }

        pending.extend_from_slice(&buffer[..n]);

        while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&pending[..pos]).into_owned();
            lines.push(line);
            pending.drain(..=pos);
        }
    }

    Ok(())
}

fn sort_lines(lines: &mut [String], opts: &SortOptions) {
    lines.sort_by(|a, b| {
        let key_a = get_sort_key(a, opts);
        let key_b = get_sort_key(b, opts);

        let cmp = if opts.numeric {
            // Numeric comparison with GNU sort behavior:
            // - Parse leading numeric prefix
            // - Numbers sort before non-numbers
            // - Non-numbers fall back to lexical comparison
            match (parse_numeric_prefix(&key_a), parse_numeric_prefix(&key_b)) {
                (Some(num_a), Some(num_b)) => num_a
                    .partial_cmp(&num_b)
                    .unwrap_or(std::cmp::Ordering::Equal),
                (Some(_), None) => std::cmp::Ordering::Less, // Numbers before non-numbers
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => key_a.cmp(&key_b), // Lexical fallback
            }
        } else if opts.ignore_case {
            key_a.to_lowercase().cmp(&key_b.to_lowercase())
        } else {
            key_a.cmp(&key_b)
        };

        if opts.reverse { cmp.reverse() } else { cmp }
    });
}

/// Parse leading numeric prefix from a string (GNU sort behavior).
/// Handles integers and floats including negative numbers.
/// Returns None if the string doesn't start with a number.
fn parse_numeric_prefix(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Find the end of the numeric portion
    let mut chars = s.chars().peekable();
    let mut num_end = 0;
    let mut has_digit = false;
    let mut has_decimal = false;

    // Handle optional leading sign
    if matches!(chars.peek(), Some('-') | Some('+')) {
        chars.next();
        num_end += 1;
    }

    // Consume digits and at most one decimal point
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            has_digit = true;
            chars.next();
            num_end += 1;
        } else if c == '.' && !has_decimal {
            has_decimal = true;
            chars.next();
            num_end += 1;
        } else {
            break;
        }
    }

    if !has_digit {
        return None;
    }

    s[..num_end].parse().ok()
}

fn get_sort_key<'a>(line: &'a str, opts: &SortOptions) -> std::borrow::Cow<'a, str> {
    // Extract field (1-indexed, whitespace-separated)
    // Use nth() directly instead of collect() to avoid Vec allocation per comparison
    if let Some(field) = opts.key_field
        && field > 0
        && let Some(key) = line.split_whitespace().nth(field - 1)
    {
        return std::borrow::Cow::Borrowed(key);
    }
    std::borrow::Cow::Borrowed(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // parse_numeric_prefix tests
    // =========================================================================

    #[test]
    fn parse_numeric_prefix_integer() {
        assert_eq!(parse_numeric_prefix("123"), Some(123.0));
        assert_eq!(parse_numeric_prefix("0"), Some(0.0));
        assert_eq!(parse_numeric_prefix("42"), Some(42.0));
    }

    #[test]
    fn parse_numeric_prefix_negative() {
        assert_eq!(parse_numeric_prefix("-5"), Some(-5.0));
        assert_eq!(parse_numeric_prefix("-123"), Some(-123.0));
        assert_eq!(parse_numeric_prefix("+42"), Some(42.0));
    }

    #[test]
    fn parse_numeric_prefix_float() {
        assert_eq!(parse_numeric_prefix("3.25"), Some(3.25));
        assert_eq!(parse_numeric_prefix("-2.5"), Some(-2.5));
        assert_eq!(parse_numeric_prefix(".5"), Some(0.5));
    }

    #[test]
    fn parse_numeric_prefix_with_suffix() {
        assert_eq!(parse_numeric_prefix("123abc"), Some(123.0));
        assert_eq!(parse_numeric_prefix("42.5foo"), Some(42.5));
        assert_eq!(parse_numeric_prefix("10 hello"), Some(10.0));
    }

    #[test]
    fn parse_numeric_prefix_non_numeric() {
        assert_eq!(parse_numeric_prefix("abc"), None);
        assert_eq!(parse_numeric_prefix("hello123"), None);
        assert_eq!(parse_numeric_prefix(""), None);
        assert_eq!(parse_numeric_prefix("   "), None);
    }

    #[test]
    fn parse_numeric_prefix_whitespace() {
        assert_eq!(parse_numeric_prefix("  42"), Some(42.0));
        assert_eq!(parse_numeric_prefix("  -5.5  "), Some(-5.5));
    }

    #[test]
    fn parse_numeric_prefix_sign_only() {
        assert_eq!(parse_numeric_prefix("-"), None);
        assert_eq!(parse_numeric_prefix("+"), None);
        assert_eq!(parse_numeric_prefix("-abc"), None);
    }

    // =========================================================================
    // sort_lines tests for numeric sorting
    // =========================================================================

    fn make_opts(numeric: bool) -> SortOptions {
        SortOptions {
            reverse: false,
            numeric,
            unique: false,
            ignore_case: false,
            key_field: None,
        }
    }

    #[test]
    fn sort_numeric_basic() {
        let mut lines: Vec<String> = vec!["10", "2", "1", "20"]
            .into_iter()
            .map(String::from)
            .collect();
        sort_lines(&mut lines, &make_opts(true));
        assert_eq!(lines, vec!["1", "2", "10", "20"]);
    }

    #[test]
    fn sort_numeric_with_text() {
        // Numbers should come before non-numeric strings
        let mut lines: Vec<String> = vec!["banana", "10", "apple", "2"]
            .into_iter()
            .map(String::from)
            .collect();
        sort_lines(&mut lines, &make_opts(true));
        assert_eq!(lines, vec!["2", "10", "apple", "banana"]);
    }

    #[test]
    fn sort_numeric_prefix_behavior() {
        // "123abc" should be treated as 123
        let mut lines: Vec<String> = vec!["5xyz", "10abc", "2def"]
            .into_iter()
            .map(String::from)
            .collect();
        sort_lines(&mut lines, &make_opts(true));
        assert_eq!(lines, vec!["2def", "5xyz", "10abc"]);
    }

    #[test]
    fn sort_non_numeric_lexical_fallback() {
        // Non-numeric lines should be sorted lexically
        let mut lines: Vec<String> = vec!["cherry", "apple", "banana"]
            .into_iter()
            .map(String::from)
            .collect();
        sort_lines(&mut lines, &make_opts(true));
        assert_eq!(lines, vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn sort_numeric_negative_numbers() {
        let mut lines: Vec<String> = vec!["5", "-10", "0", "-3"]
            .into_iter()
            .map(String::from)
            .collect();
        sort_lines(&mut lines, &make_opts(true));
        assert_eq!(lines, vec!["-10", "-3", "0", "5"]);
    }

    #[test]
    fn sort_numeric_floats() {
        let mut lines: Vec<String> = vec!["3.14", "2.71", "1.41"]
            .into_iter()
            .map(String::from)
            .collect();
        sort_lines(&mut lines, &make_opts(true));
        assert_eq!(lines, vec!["1.41", "2.71", "3.14"]);
    }
}
