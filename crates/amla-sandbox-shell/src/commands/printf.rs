//! printf - format and print data

use amla_scheduler::Exit;

use crate::CmdContext;

use super::CommandResult;

/// printf format [arguments...]
pub async fn run(ctx: CmdContext) -> CommandResult {
    let args: Vec<&str> = ctx
        .argv
        .iter()
        .skip(1)
        .map(std::string::String::as_str)
        .collect();

    if args.is_empty() {
        ctx.eprintln("printf: missing format").await?;
        return Ok(Exit::code(1));
    }

    let format = args[0];
    let values = &args[1..];
    let mut value_idx = 0;

    let mut output = String::new();
    let mut chars = format.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => output.push('\n'),
                Some('t') => output.push('\t'),
                Some('r') => output.push('\r'),
                Some('\\') => output.push('\\'),
                Some('0') => {
                    let mut octal = String::new();
                    while octal.len() < 3 {
                        if let Some(&c) = chars.peek() {
                            if c.is_ascii_digit() && c < '8' {
                                octal.push(c);
                                chars.next();
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    if octal.is_empty() {
                        output.push('\0');
                    } else if let Ok(n) = u8::from_str_radix(&octal, 8) {
                        output.push(n as char);
                    }
                }
                Some(c) => {
                    output.push('\\');
                    output.push(c);
                }
                None => output.push('\\'),
            }
        } else if c == '%' {
            match chars.next() {
                Some('%') => output.push('%'),
                Some('s') => {
                    if value_idx < values.len() {
                        output.push_str(values[value_idx]);
                        value_idx += 1;
                    }
                }
                Some('d') => {
                    if value_idx < values.len() {
                        let n: i64 = values[value_idx].parse().unwrap_or(0);
                        output.push_str(&n.to_string());
                        value_idx += 1;
                    } else {
                        output.push('0');
                    }
                }
                Some('x') => {
                    if value_idx < values.len() {
                        let n: i64 = values[value_idx].parse().unwrap_or(0);
                        output.push_str(&format!("{n:x}"));
                        value_idx += 1;
                    } else {
                        output.push('0');
                    }
                }
                Some('X') => {
                    if value_idx < values.len() {
                        let n: i64 = values[value_idx].parse().unwrap_or(0);
                        output.push_str(&format!("{n:X}"));
                        value_idx += 1;
                    } else {
                        output.push('0');
                    }
                }
                Some('o') => {
                    if value_idx < values.len() {
                        let n: i64 = values[value_idx].parse().unwrap_or(0);
                        output.push_str(&format!("{n:o}"));
                        value_idx += 1;
                    } else {
                        output.push('0');
                    }
                }
                Some('c') => {
                    if value_idx < values.len() {
                        if let Some(c) = values[value_idx].chars().next() {
                            output.push(c);
                        }
                        value_idx += 1;
                    }
                }
                Some(c) => {
                    output.push('%');
                    output.push(c);
                }
                None => output.push('%'),
            }
        } else {
            output.push(c);
        }
    }

    ctx.stdout_write_all(output.as_bytes()).await?;

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

/// Format a printf format string with the given arguments.
///
/// Exposed for testing.
#[cfg(test)]
fn format_printf(format: &str, values: &[&str]) -> String {
    let mut value_idx = 0;
    let mut output = String::new();
    let mut chars = format.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => output.push('\n'),
                Some('t') => output.push('\t'),
                Some('r') => output.push('\r'),
                Some('\\') => output.push('\\'),
                Some('0') => {
                    let mut octal = String::new();
                    while octal.len() < 3 {
                        if let Some(&c) = chars.peek() {
                            if c.is_ascii_digit() && c < '8' {
                                octal.push(c);
                                chars.next();
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    if octal.is_empty() {
                        output.push('\0');
                    } else if let Ok(n) = u8::from_str_radix(&octal, 8) {
                        output.push(n as char);
                    }
                }
                Some(c) => {
                    output.push('\\');
                    output.push(c);
                }
                None => output.push('\\'),
            }
        } else if c == '%' {
            match chars.next() {
                Some('%') => output.push('%'),
                Some('s') => {
                    if value_idx < values.len() {
                        output.push_str(values[value_idx]);
                        value_idx += 1;
                    }
                }
                Some('d') => {
                    if value_idx < values.len() {
                        let n: i64 = values[value_idx].parse().unwrap_or(0);
                        output.push_str(&n.to_string());
                        value_idx += 1;
                    } else {
                        output.push('0');
                    }
                }
                Some('x') => {
                    if value_idx < values.len() {
                        let n: i64 = values[value_idx].parse().unwrap_or(0);
                        output.push_str(&format!("{n:x}"));
                        value_idx += 1;
                    } else {
                        output.push('0');
                    }
                }
                Some('X') => {
                    if value_idx < values.len() {
                        let n: i64 = values[value_idx].parse().unwrap_or(0);
                        output.push_str(&format!("{n:X}"));
                        value_idx += 1;
                    } else {
                        output.push('0');
                    }
                }
                Some('o') => {
                    if value_idx < values.len() {
                        let n: i64 = values[value_idx].parse().unwrap_or(0);
                        output.push_str(&format!("{n:o}"));
                        value_idx += 1;
                    } else {
                        output.push('0');
                    }
                }
                Some('c') => {
                    if value_idx < values.len() {
                        if let Some(c) = values[value_idx].chars().next() {
                            output.push(c);
                        }
                        value_idx += 1;
                    }
                }
                Some(c) => {
                    output.push('%');
                    output.push(c);
                }
                None => output.push('%'),
            }
        } else {
            output.push(c);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Escape Sequence Tests
    // =========================================================================

    #[test]
    fn escape_newline() {
        assert_eq!(format_printf("hello\\nworld", &[]), "hello\nworld");
    }

    #[test]
    fn escape_tab() {
        assert_eq!(format_printf("col1\\tcol2", &[]), "col1\tcol2");
    }

    #[test]
    fn escape_carriage_return() {
        assert_eq!(format_printf("line\\roverwrite", &[]), "line\roverwrite");
    }

    #[test]
    fn escape_backslash() {
        assert_eq!(format_printf("path\\\\file", &[]), "path\\file");
    }

    #[test]
    fn escape_null() {
        let result = format_printf("before\\0after", &[]);
        assert_eq!(result, "before\0after");
    }

    #[test]
    fn escape_octal_simple() {
        // \0101 = 'A' (65 in decimal, 101 in octal)
        assert_eq!(format_printf("\\0101", &[]), "A");
    }

    #[test]
    fn escape_octal_partial() {
        // \012 = newline (10 in decimal, 12 in octal)
        assert_eq!(format_printf("\\012", &[]), "\n");
    }

    #[test]
    fn escape_unknown() {
        // Unknown escape sequences are kept as-is
        assert_eq!(format_printf("\\q", &[]), "\\q");
    }

    #[test]
    fn escape_trailing_backslash() {
        assert_eq!(format_printf("end\\", &[]), "end\\");
    }

    #[test]
    fn escape_multiple() {
        assert_eq!(format_printf("a\\nb\\tc\\\\d", &[]), "a\nb\tc\\d");
    }

    // =========================================================================
    // Format Specifier Tests
    // =========================================================================

    #[test]
    fn format_string_basic() {
        assert_eq!(format_printf("hello %s", &["world"]), "hello world");
    }

    #[test]
    fn format_string_multiple() {
        assert_eq!(format_printf("%s %s", &["hello", "world"]), "hello world");
    }

    #[test]
    fn format_string_missing_arg() {
        // Missing arg produces empty string
        assert_eq!(format_printf("%s %s", &["only"]), "only ");
    }

    #[test]
    fn format_decimal_basic() {
        assert_eq!(format_printf("count: %d", &["42"]), "count: 42");
    }

    #[test]
    fn format_decimal_negative() {
        assert_eq!(format_printf("num: %d", &["-123"]), "num: -123");
    }

    #[test]
    fn format_decimal_invalid() {
        // Invalid number becomes 0
        assert_eq!(format_printf("num: %d", &["notanumber"]), "num: 0");
    }

    #[test]
    fn format_decimal_missing_arg() {
        assert_eq!(format_printf("num: %d", &[]), "num: 0");
    }

    #[test]
    fn format_hex_lower() {
        assert_eq!(format_printf("hex: %x", &["255"]), "hex: ff");
    }

    #[test]
    fn format_hex_upper() {
        assert_eq!(format_printf("hex: %X", &["255"]), "hex: FF");
    }

    #[test]
    fn format_hex_missing_arg() {
        assert_eq!(format_printf("hex: %x", &[]), "hex: 0");
    }

    #[test]
    fn format_octal() {
        assert_eq!(format_printf("oct: %o", &["64"]), "oct: 100");
    }

    #[test]
    fn format_octal_missing_arg() {
        assert_eq!(format_printf("oct: %o", &[]), "oct: 0");
    }

    #[test]
    fn format_char() {
        assert_eq!(format_printf("char: %c", &["hello"]), "char: h");
    }

    #[test]
    fn format_char_empty() {
        assert_eq!(format_printf("char: %c", &[""]), "char: ");
    }

    #[test]
    fn format_char_missing_arg() {
        assert_eq!(format_printf("char: %c", &[]), "char: ");
    }

    #[test]
    fn format_percent() {
        assert_eq!(format_printf("100%%", &[]), "100%");
    }

    #[test]
    fn format_unknown_specifier() {
        assert_eq!(format_printf("%z", &[]), "%z");
    }

    #[test]
    fn format_trailing_percent() {
        assert_eq!(format_printf("end%", &[]), "end%");
    }

    // =========================================================================
    // Combined Tests
    // =========================================================================

    #[test]
    fn combined_escapes_and_formats() {
        assert_eq!(
            format_printf("Name: %s\\nAge: %d", &["Alice", "30"]),
            "Name: Alice\nAge: 30"
        );
    }

    #[test]
    fn complex_format() {
        let result = format_printf("User %s has %d items (0x%x)\\n", &["bob", "15", "255"]);
        assert_eq!(result, "User bob has 15 items (0xff)\n");
    }

    #[test]
    fn no_format_specifiers() {
        assert_eq!(format_printf("plain text", &[]), "plain text");
    }

    #[test]
    fn extra_args_ignored() {
        assert_eq!(format_printf("%s", &["used", "unused"]), "used");
    }
}
