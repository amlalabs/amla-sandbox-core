//! test - check file types and compare values (also [ )

use amla_scheduler::Exit;

use crate::CmdContext;

use super::CommandResult;

/// test expression | [ expression ]
pub async fn run(ctx: CmdContext) -> CommandResult {
    let args: Vec<&str> = ctx.argv.iter().map(std::string::String::as_str).collect();
    let cmd_name = args.first().copied().unwrap_or("test");

    // Handle [ command - remove trailing ]
    let args = if cmd_name == "[" {
        if args.last() != Some(&"]") {
            ctx.eprintln("[: missing ']'").await?;
            return Ok(Exit::code(2));
        }
        &args[1..args.len() - 1]
    } else {
        &args[1..]
    };

    if args.is_empty() {
        return Ok(Exit::code(1));
    }

    let result = evaluate(&ctx, args);
    Ok(Exit::code(i32::from(!result)))
}

fn evaluate(ctx: &CmdContext, args: &[&str]) -> bool {
    if args.is_empty() {
        return false;
    }

    if args[0] == "!" {
        return !evaluate(ctx, &args[1..]);
    }

    if args[0] == "(" {
        let mut depth = 1;
        let mut end = 1;
        while end < args.len() && depth > 0 {
            if args[end] == "(" {
                depth += 1;
            } else if args[end] == ")" {
                depth -= 1;
            }
            if depth > 0 {
                end += 1;
            }
        }
        if depth == 0 {
            let inner = &args[1..end];
            let rest = &args[end + 1..];
            let inner_result = evaluate(ctx, inner);
            if rest.is_empty() {
                return inner_result;
            }
            if rest.len() >= 2 {
                if rest[0] == "-a" {
                    return inner_result && evaluate(ctx, &rest[1..]);
                } else if rest[0] == "-o" {
                    return inner_result || evaluate(ctx, &rest[1..]);
                }
            }
            return inner_result;
        }
    }

    if args.len() == 1 {
        return !args[0].is_empty();
    }

    if args.len() == 2 {
        return unary_test(ctx, args[0], args[1]);
    }

    if args.len() >= 3 {
        for i in 1..args.len() - 1 {
            if args[i] == "-a" {
                let left = evaluate(ctx, &args[..i]);
                let right = evaluate(ctx, &args[i + 1..]);
                return left && right;
            }
        }
        for i in 1..args.len() - 1 {
            if args[i] == "-o" {
                let left = evaluate(ctx, &args[..i]);
                let right = evaluate(ctx, &args[i + 1..]);
                return left || right;
            }
        }

        if args.len() == 3 {
            return binary_test(args[0], args[1], args[2]);
        }
    }

    false
}

fn unary_test(ctx: &CmdContext, op: &str, arg: &str) -> bool {
    match op {
        "-e" => ctx.exists(arg),
        "-f" => ctx.is_file(arg),
        "-d" => ctx.is_dir(arg),
        "-s" => ctx.stat(arg).map(|s| s.size > 0).unwrap_or(false),
        "-r" | "-w" => ctx.exists(arg),
        "-x" => false,
        "-z" => arg.is_empty(),
        "-n" => !arg.is_empty(),
        _ => !arg.is_empty(),
    }
}

fn binary_test(left: &str, op: &str, right: &str) -> bool {
    match op {
        "=" | "==" => left == right,
        "!=" => left != right,
        "-eq" => left.parse::<i64>().unwrap_or(0) == right.parse::<i64>().unwrap_or(0),
        "-ne" => left.parse::<i64>().unwrap_or(0) != right.parse::<i64>().unwrap_or(0),
        "-lt" => left.parse::<i64>().unwrap_or(0) < right.parse::<i64>().unwrap_or(0),
        "-le" => left.parse::<i64>().unwrap_or(0) <= right.parse::<i64>().unwrap_or(0),
        "-gt" => left.parse::<i64>().unwrap_or(0) > right.parse::<i64>().unwrap_or(0),
        "-ge" => left.parse::<i64>().unwrap_or(0) >= right.parse::<i64>().unwrap_or(0),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Binary Test - String Comparisons
    // =========================================================================

    #[test]
    fn binary_string_equal_single() {
        assert!(binary_test("hello", "=", "hello"));
        assert!(!binary_test("hello", "=", "world"));
    }

    #[test]
    fn binary_string_equal_double() {
        assert!(binary_test("foo", "==", "foo"));
        assert!(!binary_test("foo", "==", "bar"));
    }

    #[test]
    fn binary_string_not_equal() {
        assert!(binary_test("a", "!=", "b"));
        assert!(!binary_test("same", "!=", "same"));
    }

    #[test]
    fn binary_string_empty() {
        assert!(binary_test("", "=", ""));
        assert!(binary_test("", "!=", "x"));
        assert!(!binary_test("", "!=", ""));
    }

    // =========================================================================
    // Binary Test - Numeric Comparisons
    // =========================================================================

    #[test]
    fn binary_numeric_eq() {
        assert!(binary_test("42", "-eq", "42"));
        assert!(!binary_test("42", "-eq", "43"));
    }

    #[test]
    fn binary_numeric_ne() {
        assert!(binary_test("10", "-ne", "20"));
        assert!(!binary_test("10", "-ne", "10"));
    }

    #[test]
    fn binary_numeric_lt() {
        assert!(binary_test("5", "-lt", "10"));
        assert!(!binary_test("10", "-lt", "5"));
        assert!(!binary_test("10", "-lt", "10"));
    }

    #[test]
    fn binary_numeric_le() {
        assert!(binary_test("5", "-le", "10"));
        assert!(binary_test("10", "-le", "10"));
        assert!(!binary_test("15", "-le", "10"));
    }

    #[test]
    fn binary_numeric_gt() {
        assert!(binary_test("20", "-gt", "10"));
        assert!(!binary_test("10", "-gt", "20"));
        assert!(!binary_test("10", "-gt", "10"));
    }

    #[test]
    fn binary_numeric_ge() {
        assert!(binary_test("20", "-ge", "10"));
        assert!(binary_test("10", "-ge", "10"));
        assert!(!binary_test("5", "-ge", "10"));
    }

    #[test]
    fn binary_numeric_negative() {
        assert!(binary_test("-5", "-lt", "0"));
        assert!(binary_test("-10", "-lt", "-5"));
        assert!(binary_test("0", "-gt", "-1"));
    }

    #[test]
    fn binary_numeric_invalid_becomes_zero() {
        // Invalid numbers parse as 0
        assert!(binary_test("abc", "-eq", "0"));
        assert!(binary_test("abc", "-eq", "xyz"));
        assert!(binary_test("0", "-eq", "notanumber"));
    }

    #[test]
    fn binary_unknown_operator() {
        assert!(!binary_test("a", "-unknown", "b"));
    }

    // =========================================================================
    // Unary Test - String Tests (no VFS needed)
    // =========================================================================

    // Note: For -z and -n, we can test via evaluate() with mock-like behavior
    // since they don't actually need filesystem access

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

    fn create_test_ctx(argv: Vec<&str>) -> CmdContext {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        // Create test files/dirs under /workspace (which is ReadWrite)
        vfs.borrow_mut()
            .create_dir("/workspace/testdir", Permission::ReadWrite)
            .unwrap();
        vfs.borrow_mut()
            .write_file("/workspace/testfile", b"content", Permission::ReadWrite)
            .unwrap();
        vfs.borrow_mut()
            .write_file("/workspace/emptyfile", b"", Permission::ReadWrite)
            .unwrap();

        let scheduler = test_scheduler();
        CmdContext::new(
            argv.into_iter().map(String::from).collect(),
            IoHandle::null(),
            IoHandle::null(),
            IoHandle::null(),
            "/workspace".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        )
    }

    #[test]
    fn unary_exists() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(unary_test(&ctx, "-e", "testfile"));
        assert!(unary_test(&ctx, "-e", "testdir"));
        assert!(!unary_test(&ctx, "-e", "nonexistent"));
    }

    #[test]
    fn unary_is_file() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(unary_test(&ctx, "-f", "testfile"));
        assert!(!unary_test(&ctx, "-f", "testdir"));
        assert!(!unary_test(&ctx, "-f", "nonexistent"));
    }

    #[test]
    fn unary_is_dir() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(unary_test(&ctx, "-d", "testdir"));
        assert!(!unary_test(&ctx, "-d", "testfile"));
        assert!(!unary_test(&ctx, "-d", "nonexistent"));
    }

    #[test]
    fn unary_has_size() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(unary_test(&ctx, "-s", "testfile")); // has content
        assert!(!unary_test(&ctx, "-s", "emptyfile")); // empty
        assert!(!unary_test(&ctx, "-s", "nonexistent"));
    }

    #[test]
    fn unary_readable_writable() {
        let ctx = create_test_ctx(vec!["test"]);
        // -r and -w just check existence in this implementation
        assert!(unary_test(&ctx, "-r", "testfile"));
        assert!(unary_test(&ctx, "-w", "testfile"));
        assert!(!unary_test(&ctx, "-r", "nonexistent"));
    }

    #[test]
    fn unary_executable() {
        let ctx = create_test_ctx(vec!["test"]);
        // -x always returns false in this implementation
        assert!(!unary_test(&ctx, "-x", "testfile"));
    }

    #[test]
    fn unary_string_zero() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(unary_test(&ctx, "-z", ""));
        assert!(!unary_test(&ctx, "-z", "nonempty"));
    }

    #[test]
    fn unary_string_nonzero() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(unary_test(&ctx, "-n", "nonempty"));
        assert!(!unary_test(&ctx, "-n", ""));
    }

    #[test]
    fn unary_default_nonempty() {
        let ctx = create_test_ctx(vec!["test"]);
        // Unknown operator with non-empty string returns true
        assert!(unary_test(&ctx, "-unknown", "something"));
        assert!(!unary_test(&ctx, "-unknown", ""));
    }

    // =========================================================================
    // Evaluate - Logical Operations
    // =========================================================================

    #[test]
    fn evaluate_empty() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(!evaluate(&ctx, &[]));
    }

    #[test]
    fn evaluate_single_nonempty() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["hello"]));
        assert!(!evaluate(&ctx, &[""]));
    }

    #[test]
    fn evaluate_negation() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["!", ""]));
        assert!(!evaluate(&ctx, &["!", "hello"]));
    }

    #[test]
    fn evaluate_double_negation() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["!", "!", "hello"]));
    }

    #[test]
    fn evaluate_and_operator() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["hello", "-a", "world"]));
        assert!(!evaluate(&ctx, &["hello", "-a", ""]));
        assert!(!evaluate(&ctx, &["", "-a", "world"]));
    }

    #[test]
    fn evaluate_or_operator() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["hello", "-o", ""]));
        assert!(evaluate(&ctx, &["", "-o", "world"]));
        assert!(!evaluate(&ctx, &["", "-o", ""]));
    }

    #[test]
    fn evaluate_parentheses() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["(", "hello", ")"]));
        assert!(!evaluate(&ctx, &["(", "", ")"]));
    }

    #[test]
    fn evaluate_parentheses_with_and() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["(", "a", ")", "-a", "b"]));
        assert!(!evaluate(&ctx, &["(", "", ")", "-a", "b"]));
    }

    #[test]
    fn evaluate_parentheses_with_or() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["(", "", ")", "-o", "b"]));
    }

    #[test]
    fn evaluate_nested_parentheses() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["(", "(", "hello", ")", ")"]));
    }

    #[test]
    fn evaluate_binary_string() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["foo", "=", "foo"]));
        assert!(!evaluate(&ctx, &["foo", "=", "bar"]));
        assert!(evaluate(&ctx, &["foo", "!=", "bar"]));
    }

    #[test]
    fn evaluate_binary_numeric() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["5", "-lt", "10"]));
        assert!(evaluate(&ctx, &["10", "-eq", "10"]));
        assert!(evaluate(&ctx, &["20", "-gt", "10"]));
    }

    #[test]
    fn evaluate_unary_file() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["-e", "testfile"]));
        assert!(evaluate(&ctx, &["-f", "testfile"]));
        assert!(evaluate(&ctx, &["-d", "testdir"]));
        assert!(evaluate(&ctx, &["!", "-e", "nonexistent"]));
    }

    #[test]
    fn evaluate_complex() {
        let ctx = create_test_ctx(vec!["test"]);
        // "a" -a "b" -o ""  -> should be true (a && b is true, then || "" doesn't matter)
        assert!(evaluate(&ctx, &["a", "-a", "b"]));
    }

    // =========================================================================
    // Run function tests (async)
    // =========================================================================

    #[test]
    fn run_empty_args() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 1); // Empty args returns false (exit code 1)
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_bracket_command() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["[", "hello", "]"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0); // Non-empty string is true
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_bracket_missing_close() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["[", "hello"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 2); // Missing ] is error
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_true() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "nonempty"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_false() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", ""]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 1);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_file_exists() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-e", "testfile"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_file_not_exists() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-e", "nonexistent"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 1);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_negation() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "!", "-e", "nonexistent"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0); // ! false = true
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_bracket_string_compare() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["[", "foo", "=", "foo", "]"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_bracket_numeric_compare() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["[", "5", "-lt", "10", "]"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    // =========================================================================
    // Additional Coverage Tests - Help Flags
    // =========================================================================

    #[test]
    fn run_test_help_long() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "--help"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0); // Help should return success
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_help_short() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-h"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0); // Help should return success
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_bracket_no_help() {
        // [ command should NOT show help even with --help
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["[", "--help", "]"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            // --help is treated as a non-empty string, so returns true (exit 0)
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    // =========================================================================
    // Additional Coverage Tests - Bracket Command Edge Cases
    // =========================================================================

    #[test]
    fn run_bracket_empty_expression() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["[", "]"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 1); // Empty expression returns false
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_bracket_only_close() {
        // Just "]" without "[" - this would be called as "test ]"
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "]"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            // "]" is a non-empty string, so returns true
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    // =========================================================================
    // Additional Coverage Tests - Negation Edge Cases
    // =========================================================================

    #[test]
    fn evaluate_negation_empty_becomes_true() {
        let ctx = create_test_ctx(vec!["test"]);
        // "!" alone with nothing else - negates an empty evaluation
        assert!(evaluate(&ctx, &["!"]));
    }

    #[test]
    fn evaluate_triple_negation() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(!evaluate(&ctx, &["!", "!", "!", "hello"]));
    }

    #[test]
    fn evaluate_negation_with_file_test() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["!", "-f", "nonexistent"]));
        assert!(!evaluate(&ctx, &["!", "-f", "testfile"]));
    }

    // =========================================================================
    // Additional Coverage Tests - Complex Boolean Expressions
    // =========================================================================

    #[test]
    fn evaluate_multiple_and() {
        let ctx = create_test_ctx(vec!["test"]);
        // a -a b -a c should all be true
        assert!(evaluate(&ctx, &["a", "-a", "b", "-a", "c"]));
        // Empty in the middle should fail
        assert!(!evaluate(&ctx, &["a", "-a", "", "-a", "c"]));
    }

    #[test]
    fn evaluate_multiple_or() {
        let ctx = create_test_ctx(vec!["test"]);
        // At least one true should succeed
        assert!(evaluate(&ctx, &["", "-o", "", "-o", "c"]));
        // All empty should fail
        assert!(!evaluate(&ctx, &["", "-o", "", "-o", ""]));
    }

    #[test]
    fn evaluate_and_or_mixed() {
        let ctx = create_test_ctx(vec!["test"]);
        // In standard test, -a has higher precedence than -o
        // a -a b -o c means (a && b) || c
        // Per implementation: -a is found first and splits
        assert!(evaluate(&ctx, &["a", "-a", "b", "-o", "c"]));
        // "" -a b -o c: (-a splits first) left="" right="b -o c"
        // "" is false, so left is false; then right evaluates "b -o c"
        // But since -a split happened, it's: evaluate("") && evaluate("b -o c")
        // = false && true = false
        assert!(!evaluate(&ctx, &["", "-a", "b", "-o", "c"]));
        // a -o "" -a b: -a splits first: left="a -o """  right="b"
        // evaluate("a -o """) = true, evaluate("b") = true
        // = true && true = true
        assert!(evaluate(&ctx, &["a", "-o", "", "-a", "b"]));
    }

    #[test]
    fn evaluate_or_with_file_tests() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(
            &ctx,
            &["-e", "testfile", "-o", "-e", "nonexistent"]
        ));
        assert!(evaluate(
            &ctx,
            &["-e", "nonexistent", "-o", "-e", "testfile"]
        ));
        assert!(!evaluate(&ctx, &["-e", "nope1", "-o", "-e", "nope2"]));
    }

    #[test]
    fn evaluate_and_with_file_tests() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["-e", "testfile", "-a", "-d", "testdir"]));
        assert!(!evaluate(
            &ctx,
            &["-e", "testfile", "-a", "-e", "nonexistent"]
        ));
    }

    // =========================================================================
    // Additional Coverage Tests - Parentheses Edge Cases
    // =========================================================================

    #[test]
    fn evaluate_unmatched_open_paren() {
        let ctx = create_test_ctx(vec!["test"]);
        // Unmatched "(" - the parenthesis handling loop runs but depth never reaches 0.
        // When the loop ends without finding a matching ")", the code falls through
        // to the single-arg check: args.len() == 1, and "(" is non-empty, so returns true.
        assert!(evaluate(&ctx, &["("]));
    }

    #[test]
    fn evaluate_paren_with_negation_inside() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["(", "!", "", ")"]));
        assert!(!evaluate(&ctx, &["(", "!", "hello", ")"]));
    }

    #[test]
    fn evaluate_negation_of_paren() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["!", "(", "", ")"]));
        assert!(!evaluate(&ctx, &["!", "(", "hello", ")"]));
    }

    #[test]
    fn evaluate_paren_with_binary() {
        let ctx = create_test_ctx(vec!["test"]);
        assert!(evaluate(&ctx, &["(", "foo", "=", "foo", ")"]));
        assert!(!evaluate(&ctx, &["(", "foo", "=", "bar", ")"]));
    }

    #[test]
    fn evaluate_complex_nested_parens() {
        let ctx = create_test_ctx(vec!["test"]);
        // ( ( a ) -a ( b ) )
        assert!(evaluate(
            &ctx,
            &["(", "(", "a", ")", "-a", "(", "b", ")", ")"]
        ));
        assert!(!evaluate(
            &ctx,
            &["(", "(", "", ")", "-a", "(", "b", ")", ")"]
        ));
    }

    // =========================================================================
    // Additional Coverage Tests - Numeric Edge Cases
    // =========================================================================

    #[test]
    fn binary_numeric_large_numbers() {
        assert!(binary_test(
            "9223372036854775807",
            "-eq",
            "9223372036854775807"
        )); // i64::MAX
        assert!(binary_test("-9223372036854775808", "-lt", "0")); // i64::MIN
    }

    #[test]
    fn binary_numeric_overflow_becomes_zero() {
        // Numbers larger than i64::MAX should parse as 0
        assert!(binary_test("99999999999999999999999", "-eq", "0"));
    }

    #[test]
    fn binary_numeric_leading_zeros() {
        assert!(binary_test("007", "-eq", "7"));
        assert!(binary_test("0042", "-eq", "42"));
    }

    #[test]
    fn binary_numeric_with_plus_sign() {
        // Rust's parse::<i64>() accepts leading + sign
        assert!(binary_test("+5", "-eq", "5"));
        assert!(binary_test("+10", "-gt", "+5"));
    }

    #[test]
    fn binary_numeric_whitespace_invalid() {
        // Strings with whitespace should parse as 0
        assert!(binary_test(" 5", "-eq", "0"));
        assert!(binary_test("5 ", "-eq", "0"));
    }

    // =========================================================================
    // Additional Coverage Tests - String Edge Cases
    // =========================================================================

    #[test]
    fn binary_string_with_spaces() {
        assert!(binary_test("hello world", "=", "hello world"));
        assert!(binary_test(" ", "=", " "));
        assert!(binary_test("", "!=", " "));
    }

    #[test]
    fn binary_string_special_chars() {
        assert!(binary_test("foo=bar", "=", "foo=bar"));
        assert!(binary_test("-n", "=", "-n"));
        assert!(binary_test("(", "=", "("));
    }

    #[test]
    fn evaluate_string_that_looks_like_operator() {
        let ctx = create_test_ctx(vec!["test"]);
        // Single "-n" should be treated as non-empty string
        assert!(evaluate(&ctx, &["-n"]));
        // "-a" alone is also a non-empty string
        assert!(evaluate(&ctx, &["-a"]));
    }

    // =========================================================================
    // Additional Coverage Tests - Run Function Edge Cases
    // =========================================================================

    #[test]
    fn run_test_complex_expression() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec![
            "test", "(", "-e", "testfile", ")", "-a", "(", "-d", "testdir", ")",
        ]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_bracket_with_parentheses() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["[", "(", "hello", ")", "]"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_string_z_empty() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-z", ""]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0); // Empty string, -z is true
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_string_z_nonempty() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-z", "hello"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 1); // Non-empty string, -z is false
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_string_n_empty() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-n", ""]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 1); // Empty string, -n is false
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_string_n_nonempty() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-n", "hello"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0); // Non-empty string, -n is true
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_directory_check() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-d", "testdir"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0);
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_file_size() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-s", "testfile"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 0); // testfile has content
            Ok(Exit::success())
        });

        let _ = exec.run();
    }

    #[test]
    fn run_test_empty_file_size() {
        let exec = Executor::new();
        let ctx = create_test_ctx(vec!["test", "-s", "emptyfile"]);

        exec.spawn(async move {
            let result = run(ctx).await?;
            assert_eq!(result.code, 1); // emptyfile is empty
            Ok(Exit::success())
        });

        let _ = exec.run();
    }
}
