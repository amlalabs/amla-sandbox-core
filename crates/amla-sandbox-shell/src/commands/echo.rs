//! echo - display a line of text

use amla_scheduler::Exit;

use crate::CmdContext;

use super::CommandResult;

const HELP: &str = "\
echo - display a line of text

Usage: echo [OPTIONS] [STRING...]

Options:
  --help        Show this help message
  -n            Do not output trailing newline
  -e            Enable interpretation of backslash escapes
  -E            Disable interpretation of backslash escapes (default)

Escape sequences (with -e):
  \\\\    backslash
  \\n    newline
  \\t    tab
  \\r    carriage return
  \\0NNN octal byte value

Examples:
  echo hello world        Print 'hello world'
  echo -n no newline      Print without trailing newline
  echo -e 'a\\nb'          Print with newline between a and b
";

pub async fn run(ctx: CmdContext) -> CommandResult {
    // Check for --help first
    if ctx.argv.get(1).is_some_and(|a| a == "--help" || a == "-h") {
        ctx.stdout_write_all(HELP.as_bytes()).await?;
        return Ok(Exit::success());
    }

    let mut no_newline = false;
    let mut interpret_escapes = false;
    let mut args_start = 1;

    // Parse options
    'outer: for (i, arg) in ctx.argv.iter().enumerate().skip(1) {
        if arg.starts_with('-') && arg.len() > 1 {
            for c in arg.chars().skip(1) {
                match c {
                    'n' => no_newline = true,
                    'e' => interpret_escapes = true,
                    'E' => interpret_escapes = false,
                    _ => {
                        // Unknown option char - treat whole arg as regular arg
                        args_start = i;
                        break 'outer;
                    }
                }
            }
            args_start = i + 1;
        } else {
            args_start = i;
            break;
        }
    }

    let args: Vec<&str> = ctx.argv[args_start..]
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let mut output = args.join(" ");

    if interpret_escapes {
        output = interpret_escape_sequences(&output);
    }

    if !no_newline {
        output.push('\n');
    }

    ctx.stdout_write_all(output.as_bytes()).await?;

    Ok(Exit::success())
}

fn interpret_escape_sequences(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\\') => result.push('\\'),
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
                        result.push('\0');
                    } else if let Ok(n) = u8::from_str_radix(&octal, 8) {
                        result.push(n as char);
                    }
                }
                Some(c) => {
                    result.push('\\');
                    result.push(c);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // interpret_escape_sequences unit tests
    // =========================================================================

    #[test]
    fn escape_newline() {
        assert_eq!(interpret_escape_sequences(r"a\nb"), "a\nb");
    }

    #[test]
    fn escape_tab() {
        assert_eq!(interpret_escape_sequences(r"a\tb"), "a\tb");
    }

    #[test]
    fn escape_carriage_return() {
        assert_eq!(interpret_escape_sequences(r"a\rb"), "a\rb");
    }

    #[test]
    fn escape_backslash() {
        assert_eq!(interpret_escape_sequences(r"a\\b"), "a\\b");
    }

    #[test]
    fn escape_null() {
        assert_eq!(interpret_escape_sequences(r"a\0b"), "a\0b");
    }

    #[test]
    fn escape_octal_simple() {
        // \0101 = 'A' (65 in decimal, 101 in octal)
        assert_eq!(interpret_escape_sequences(r"\0101"), "A");
    }

    #[test]
    fn escape_octal_partial() {
        // \012 = newline (10 in decimal, 12 in octal)
        assert_eq!(interpret_escape_sequences(r"\012"), "\n");
    }

    #[test]
    fn escape_octal_one_digit() {
        // \07 = bell character (7 in octal)
        assert_eq!(interpret_escape_sequences(r"\07"), "\x07");
    }

    #[test]
    fn escape_octal_stops_at_non_octal() {
        // \0189 - only '1' is valid octal (8 and 9 are not valid octal digits)
        // \01 = 1 decimal, then '89' remains literal
        assert_eq!(interpret_escape_sequences(r"\0189"), "\x0189");
    }

    #[test]
    fn escape_octal_stops_at_three_digits() {
        // \01234 - only first 3 octal digits read (123), then 4 is literal
        // \0123 = 83 decimal = 'S'
        assert_eq!(interpret_escape_sequences(r"\01234"), "S4");
    }

    #[test]
    fn escape_unknown_preserved() {
        // Unknown escape sequences are preserved literally
        assert_eq!(interpret_escape_sequences(r"\x"), "\\x");
        assert_eq!(interpret_escape_sequences(r"\z"), "\\z");
        assert_eq!(interpret_escape_sequences(r"\a"), "\\a");
    }

    #[test]
    fn escape_trailing_backslash() {
        // Trailing backslash is preserved
        assert_eq!(interpret_escape_sequences("hello\\"), "hello\\");
    }

    #[test]
    fn escape_multiple() {
        assert_eq!(
            interpret_escape_sequences(r"line1\nline2\ttab\r\n"),
            "line1\nline2\ttab\r\n"
        );
    }

    #[test]
    fn escape_no_escapes() {
        assert_eq!(interpret_escape_sequences("hello world"), "hello world");
    }

    #[test]
    fn escape_empty_string() {
        assert_eq!(interpret_escape_sequences(""), "");
    }

    #[test]
    fn escape_only_backslash() {
        assert_eq!(interpret_escape_sequences("\\"), "\\");
    }

    #[test]
    fn escape_double_backslash() {
        assert_eq!(interpret_escape_sequences(r"\\\\"), "\\\\");
    }

    // =========================================================================
    // Integration tests with CmdContext
    // =========================================================================

    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use amla_scheduler::{Executor, Exit, RandomSourceFn, Scheduler, TimeSourceFn};
    use amla_vfs::{Permission, Vfs};

    use crate::env::Environment;
    use crate::io_handle::IoHandle;

    fn test_scheduler() -> Scheduler {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    fn create_test_ctx(argv: Vec<&str>) -> (CmdContext, IoHandle) {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir("/workspace", Permission::ReadWrite)
            .ok();

        let scheduler = test_scheduler();
        let stdout = IoHandle::buffer();

        let ctx = CmdContext::new(
            argv.into_iter().map(String::from).collect(),
            IoHandle::null(),
            stdout.clone(),
            IoHandle::null(),
            "/workspace".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        );
        (ctx, stdout)
    }

    fn run_echo(argv: Vec<&str>) -> (Exit, String) {
        let exec = Executor::new();
        let (ctx, stdout) = create_test_ctx(argv);

        let result = Rc::new(RefCell::new(None));
        let result_clone = result.clone();

        exec.spawn(async move {
            let exit = run(ctx).await?;
            *result_clone.borrow_mut() = Some(exit);
            Ok(Exit::success())
        });

        let _ = exec.run();

        let exit = result.borrow().clone().unwrap();
        let output = String::from_utf8_lossy(&stdout.take_buffer()).to_string();
        (exit, output)
    }

    // =========================================================================
    // Help tests
    // =========================================================================

    #[test]
    fn echo_help_long() {
        let (exit, output) = run_echo(vec!["echo", "--help"]);
        assert_eq!(exit.code, 0);
        assert!(output.contains("echo - display a line of text"));
        assert!(output.contains("-n"));
        assert!(output.contains("-e"));
    }

    #[test]
    fn echo_help_short() {
        let (exit, output) = run_echo(vec!["echo", "-h"]);
        assert_eq!(exit.code, 0);
        assert!(output.contains("echo - display a line of text"));
    }

    // =========================================================================
    // Basic output tests
    // =========================================================================

    #[test]
    fn echo_simple_word() {
        let (exit, output) = run_echo(vec!["echo", "hello"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "hello\n");
    }

    #[test]
    fn echo_multiple_words() {
        let (exit, output) = run_echo(vec!["echo", "hello", "world"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "hello world\n");
    }

    #[test]
    fn echo_no_args() {
        let (exit, output) = run_echo(vec!["echo"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "\n");
    }

    #[test]
    fn echo_empty_string_arg() {
        let (exit, output) = run_echo(vec!["echo", ""]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "\n");
    }

    #[test]
    fn echo_multiple_empty_args() {
        let (exit, output) = run_echo(vec!["echo", "", "", ""]);
        assert_eq!(exit.code, 0);
        // Three empty strings joined by spaces = "  " (two spaces)
        assert_eq!(output, "  \n");
    }

    // =========================================================================
    // -n option tests (no newline)
    // =========================================================================

    #[test]
    fn echo_n_option() {
        let (exit, output) = run_echo(vec!["echo", "-n", "hello"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "hello");
    }

    #[test]
    fn echo_n_no_args() {
        let (exit, output) = run_echo(vec!["echo", "-n"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "");
    }

    #[test]
    fn echo_n_multiple_words() {
        let (exit, output) = run_echo(vec!["echo", "-n", "hello", "world"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "hello world");
    }

    // =========================================================================
    // -e option tests (escape sequences)
    // =========================================================================

    #[test]
    fn echo_e_newline() {
        let (exit, output) = run_echo(vec!["echo", "-e", r"a\nb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\nb\n");
    }

    #[test]
    fn echo_e_tab() {
        let (exit, output) = run_echo(vec!["echo", "-e", r"a\tb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\tb\n");
    }

    #[test]
    fn echo_e_carriage_return() {
        let (exit, output) = run_echo(vec!["echo", "-e", r"a\rb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\rb\n");
    }

    #[test]
    fn echo_e_backslash() {
        let (exit, output) = run_echo(vec!["echo", "-e", r"a\\b"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\\b\n");
    }

    #[test]
    fn echo_e_null() {
        let (exit, output) = run_echo(vec!["echo", "-e", r"a\0b"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\0b\n");
    }

    #[test]
    fn echo_e_octal() {
        // \0101 = 'A'
        let (exit, output) = run_echo(vec!["echo", "-e", r"\0101"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "A\n");
    }

    // =========================================================================
    // -E option tests (disable escapes, default)
    // =========================================================================

    #[test]
    fn echo_e_uppercase_disables() {
        // -E should disable escape interpretation
        let (exit, output) = run_echo(vec!["echo", "-E", r"a\nb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\\nb\n");
    }

    #[test]
    fn echo_default_no_escapes() {
        // Default behavior (no -e) should not interpret escapes
        let (exit, output) = run_echo(vec!["echo", r"a\nb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\\nb\n");
    }

    // =========================================================================
    // Combined options tests
    // =========================================================================

    #[test]
    fn echo_ne_combined() {
        let (exit, output) = run_echo(vec!["echo", "-ne", r"a\nb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\nb");
    }

    #[test]
    fn echo_en_combined() {
        let (exit, output) = run_echo(vec!["echo", "-en", r"a\tb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\tb");
    }

    #[test]
    fn echo_e_then_uppercase_e_last_wins() {
        // -eE: -e enables escapes, then -E disables
        let (exit, output) = run_echo(vec!["echo", "-eE", r"a\nb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\\nb\n");
    }

    #[test]
    fn echo_uppercase_e_then_e_last_wins() {
        // -Ee: -E disables, then -e enables
        let (exit, output) = run_echo(vec!["echo", "-Ee", r"a\nb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\nb\n");
    }

    #[test]
    fn echo_multiple_option_args() {
        // Multiple separate option arguments
        let (exit, output) = run_echo(vec!["echo", "-n", "-e", r"a\tb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\tb");
    }

    #[test]
    fn echo_n_e_uppercase_e_complex() {
        // -n -e -E: no newline, escapes enabled then disabled
        let (exit, output) = run_echo(vec!["echo", "-n", "-e", "-E", r"a\nb"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "a\\nb");
    }

    // =========================================================================
    // Unknown option handling tests
    // =========================================================================

    #[test]
    fn echo_unknown_option_single() {
        // -x is unknown, so the whole arg is treated as text
        let (exit, output) = run_echo(vec!["echo", "-x"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "-x\n");
    }

    #[test]
    fn echo_unknown_option_with_valid() {
        // -nx: n is parsed (sets no_newline), x is unknown so -nx becomes text
        // But since n was already parsed, no_newline is true
        let (exit, output) = run_echo(vec!["echo", "-nx"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "-nx"); // No newline because -n was parsed before x
    }

    #[test]
    fn echo_unknown_option_after_valid() {
        // -n is valid (parsed), -x is unknown (treated as text)
        let (exit, output) = run_echo(vec!["echo", "-n", "-x"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "-x");
    }

    #[test]
    fn echo_dash_alone() {
        // Single dash is not an option, treated as text
        let (exit, output) = run_echo(vec!["echo", "-"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "-\n");
    }

    #[test]
    fn echo_double_dash() {
        // -- is treated as unknown option (not special end-of-options marker)
        let (exit, output) = run_echo(vec!["echo", "--"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "--\n");
    }

    #[test]
    fn echo_option_after_non_option() {
        // Once a non-option arg is seen, subsequent -n is text
        let (exit, output) = run_echo(vec!["echo", "hello", "-n"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "hello -n\n");
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn echo_spaces_preserved() {
        // Multiple arguments with their own spaces
        let (exit, output) = run_echo(vec!["echo", "a  b", "c  d"]);
        assert_eq!(exit.code, 0);
        // Arguments are joined with single space
        assert_eq!(output, "a  b c  d\n");
    }

    #[test]
    fn echo_special_chars() {
        let (exit, output) = run_echo(vec!["echo", "!@#$%^&*()"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "!@#$%^&*()\n");
    }

    #[test]
    fn echo_unicode() {
        let (exit, output) = run_echo(vec!["echo", "Hello World!"]);
        assert_eq!(exit.code, 0);
        assert_eq!(output, "Hello World!\n");
    }

    #[test]
    fn echo_very_long_string() {
        let long_str = "x".repeat(10000);
        let (exit, output) = run_echo(vec!["echo", &long_str]);
        assert_eq!(exit.code, 0);
        assert_eq!(output.len(), 10001); // 10000 chars + newline
    }

    #[test]
    fn echo_many_arguments() {
        let args: Vec<&str> = (0..100).map(|_| "word").collect();
        let mut argv = vec!["echo"];
        argv.extend(args.iter());
        let (exit, output) = run_echo(argv);
        assert_eq!(exit.code, 0);
        // 100 "word"s joined by 99 spaces + newline
        let expected = (0..100).map(|_| "word").collect::<Vec<_>>().join(" ") + "\n";
        assert_eq!(output, expected);
    }
}
