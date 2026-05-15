//! tr - translate or delete characters
//!
//! Streams character-by-character for memory efficiency.

use amla_scheduler::Exit;

use crate::CmdContext;
use crate::io_handle::IoError;

use super::CommandResult;

/// tr [-d] [-s] [-c] set1 [set2]
///
/// Translate, squeeze, or delete characters.
/// Streams byte-by-byte for memory efficiency.
///
/// Options:
///   -d    Delete characters in set1
///   -s    Squeeze repeated characters in set2
///   -c    Complement set1
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut delete = false;
    let mut squeeze = false;
    let mut complement = false;
    let mut sets: Vec<String> = Vec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('d'))) => delete = true,
            Ok(Some(lexopt::Arg::Short('s'))) => squeeze = true,
            Ok(Some(lexopt::Arg::Short('c' | 'C'))) => complement = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                sets.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if sets.is_empty() {
        ctx.eprintln("tr: missing operand").await?;
        return Ok(Exit::code(1));
    }

    // Parse character sets
    let set1 = expand_set(&sets[0]);
    let set2 = if sets.len() > 1 {
        expand_set(&sets[1])
    } else {
        Vec::new()
    };

    // Build translation table or delete set
    let set1_chars: std::collections::HashSet<u8> = if complement {
        // Complement: all bytes NOT in set1
        let in_set1: std::collections::HashSet<u8> = set1.iter().copied().collect();
        (0u8..=255).filter(|b| !in_set1.contains(b)).collect()
    } else {
        set1.iter().copied().collect()
    };

    // Build translation map (set1[i] -> set2[i])
    let mut trans_map = [0u8; 256];
    for i in 0u8..=255 {
        trans_map[i as usize] = i;
    }

    if !delete && !set2.is_empty() {
        let set1_vec: Vec<u8> = if complement {
            set1_chars.iter().copied().collect()
        } else {
            set1.clone()
        };

        for (i, &from) in set1_vec.iter().enumerate() {
            let to = if i < set2.len() {
                set2[i]
            } else {
                // Extend set2 with its last character
                *set2.last().unwrap_or(&from)
            };
            trans_map[from as usize] = to;
        }
    }

    // Stream processing
    let mut buffer = [0u8; 4096];
    let mut last_output: Option<u8> = None;

    loop {
        let n = ctx.stdin.read(&mut buffer).await?;
        if n == 0 {
            break;
        }

        for &byte in &buffer[..n] {
            if delete {
                // Delete mode: skip characters in set1
                if !set1_chars.contains(&byte) {
                    output_byte(&ctx, byte, squeeze, &set1_chars, &mut last_output).await?;
                }
            } else {
                // Translate mode
                let translated = trans_map[byte as usize];
                output_byte(&ctx, translated, squeeze, &set1_chars, &mut last_output).await?;
            }
        }
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

/// Output a byte, with optional squeeze.
async fn output_byte(
    ctx: &CmdContext,
    byte: u8,
    squeeze: bool,
    squeeze_set: &std::collections::HashSet<u8>,
    last_output: &mut Option<u8>,
) -> Result<(), IoError> {
    if squeeze && squeeze_set.contains(&byte) {
        // Squeeze: skip if same as last output
        if *last_output == Some(byte) {
            return Ok(());
        }
    }
    *last_output = Some(byte);
    ctx.stdout_write_all(&[byte]).await
}

/// Expand a character set string into bytes.
/// Supports: literals, ranges (a-z), escapes (\n, \t, etc.)
fn expand_set(s: &str) -> Vec<u8> {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            // Escape sequence
            i += 1;
            let escaped = match bytes[i] {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                b'\\' => b'\\',
                b'0' => b'\0',
                b'a' => 0x07, // bell
                b'b' => 0x08, // backspace
                b'f' => 0x0C, // form feed
                b'v' => 0x0B, // vertical tab
                c => c,
            };
            result.push(escaped);
            i += 1;
        } else if i + 2 < bytes.len() && bytes[i + 1] == b'-' {
            // Range: a-z
            let start = bytes[i];
            let end = bytes[i + 2];
            if start <= end {
                for c in start..=end {
                    result.push(c);
                }
            } else {
                // Descending range
                for c in (end..=start).rev() {
                    result.push(c);
                }
            }
            i += 3;
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_set_simple() {
        assert_eq!(expand_set("abc"), vec![b'a', b'b', b'c']);
    }

    #[test]
    fn test_expand_set_range() {
        assert_eq!(expand_set("a-d"), vec![b'a', b'b', b'c', b'd']);
    }

    #[test]
    fn test_expand_set_escapes() {
        assert_eq!(expand_set(r"\n\t"), vec![b'\n', b'\t']);
    }

    #[test]
    fn test_expand_set_mixed() {
        assert_eq!(
            expand_set("a-c0-2"),
            vec![b'a', b'b', b'c', b'0', b'1', b'2']
        );
    }

    #[test]
    fn test_expand_set_descending_range() {
        // z-a should produce z, y, x, ..., a in descending order
        assert_eq!(expand_set("d-a"), vec![b'd', b'c', b'b', b'a']);
    }

    #[test]
    fn test_expand_set_all_escapes() {
        // Test all supported escape sequences
        assert_eq!(expand_set(r"\n"), vec![b'\n']);
        assert_eq!(expand_set(r"\t"), vec![b'\t']);
        assert_eq!(expand_set(r"\r"), vec![b'\r']);
        assert_eq!(expand_set(r"\\"), vec![b'\\']);
        assert_eq!(expand_set(r"\0"), vec![b'\0']);
        assert_eq!(expand_set(r"\a"), vec![0x07]); // bell
        assert_eq!(expand_set(r"\b"), vec![0x08]); // backspace
        assert_eq!(expand_set(r"\f"), vec![0x0C]); // form feed
        assert_eq!(expand_set(r"\v"), vec![0x0B]); // vertical tab
    }

    #[test]
    fn test_expand_set_unknown_escape() {
        // Unknown escapes should pass through the character
        assert_eq!(expand_set(r"\x"), vec![b'x']);
        assert_eq!(expand_set(r"\z"), vec![b'z']);
    }

    #[test]
    fn test_expand_set_single_char() {
        assert_eq!(expand_set("x"), vec![b'x']);
    }

    #[test]
    fn test_expand_set_empty() {
        assert_eq!(expand_set(""), Vec::<u8>::new());
    }

    #[test]
    fn test_expand_set_digit_range() {
        assert_eq!(
            expand_set("0-9"),
            vec![b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9']
        );
    }

    #[test]
    fn test_expand_set_uppercase_range() {
        assert_eq!(expand_set("A-E"), vec![b'A', b'B', b'C', b'D', b'E']);
    }

    #[test]
    fn test_expand_set_trailing_hyphen() {
        // Hyphen at end should be literal since no third char for range
        assert_eq!(expand_set("ab-"), vec![b'a', b'b', b'-']);
    }

    #[test]
    fn test_expand_set_leading_hyphen() {
        // Leading hyphen followed by char, then hyphen - no range formed
        let result = expand_set("-a-");
        assert_eq!(result, vec![b'-', b'a', b'-']);
    }

    #[test]
    fn test_expand_set_multiple_ranges() {
        assert_eq!(
            expand_set("a-cA-C"),
            vec![b'a', b'b', b'c', b'A', b'B', b'C']
        );
    }

    #[test]
    fn test_expand_set_escape_in_range() {
        // Escape at start of potential range: \n (10) to 'p' (112)
        let result = expand_set(r"\n-p");
        assert!(result.len() > 2);
        assert_eq!(result[0], b'\n');
        assert_eq!(*result.last().unwrap(), b'p');
    }

    #[test]
    fn test_expand_set_escape_trailing() {
        // Trailing backslash with no character after is treated as literal
        assert_eq!(expand_set("a\\"), vec![b'a', b'\\']);
    }

    #[test]
    fn test_expand_set_same_char_range() {
        // Range where start equals end
        assert_eq!(expand_set("a-a"), vec![b'a']);
    }
}
