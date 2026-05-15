//! jq - JSON processor using jaq (pure Rust implementation)
//!
//! Processes JSON data using jq filter expressions.
//! Uses the jaq library which is a pure Rust implementation of jq.

use amla_scheduler::Exit;
use jaq_interpret::{Ctx, FilterT, ParseCtx, RcIter, Val};
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::IoError;

use super::CommandResult;

/// jq [-r] [-c] [-e] [-s] [-n] filter [file...]
///
/// Process JSON with jq filter expressions.
///
/// Options:
///   -r    Output raw strings (no quotes)
///   -c    Compact output (no pretty printing)
///   -e    Exit with non-zero if last output is false or null
///   -s    Slurp: read entire input into array
///   -n    Null input: don't read any input
///   -j    No newline after each output
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut raw_output = false;
    let mut compact = false;
    let mut exit_status = false;
    let mut slurp = false;
    let mut null_input = false;
    let mut no_newline = false;
    let mut show_help = false;
    let mut filter: Option<String> = None;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    // Parse arguments
    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('h'))) | Ok(Some(lexopt::Arg::Long("help"))) => {
                show_help = true;
            }
            Ok(Some(lexopt::Arg::Short('r'))) => raw_output = true,
            Ok(Some(lexopt::Arg::Short('c'))) => compact = true,
            Ok(Some(lexopt::Arg::Short('e'))) => exit_status = true,
            Ok(Some(lexopt::Arg::Short('s'))) => slurp = true,
            Ok(Some(lexopt::Arg::Short('n'))) => null_input = true,
            Ok(Some(lexopt::Arg::Short('j'))) => no_newline = true,
            Ok(Some(lexopt::Arg::Long("raw-output"))) => raw_output = true,
            Ok(Some(lexopt::Arg::Long("compact-output"))) => compact = true,
            Ok(Some(lexopt::Arg::Long("exit-status"))) => exit_status = true,
            Ok(Some(lexopt::Arg::Long("slurp"))) => slurp = true,
            Ok(Some(lexopt::Arg::Long("null-input"))) => null_input = true,
            Ok(Some(lexopt::Arg::Long("join-output"))) => no_newline = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                let s = val.to_string_lossy().into_owned();
                if filter.is_none() {
                    filter = Some(s);
                } else {
                    files.push(s);
                }
            }
            Ok(Some(_)) => {} // Ignore unknown options
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if show_help {
        print_help(&ctx).await?;
        return Ok(Exit::success());
    }

    // Default filter is identity
    let filter_str = filter.unwrap_or_else(|| ".".to_string());

    // Compile the filter
    let compiled = match compile_filter(&filter_str) {
        Ok(f) => f,
        Err(e) => {
            ctx.eprintln(&format!("jq: compile error: {e}")).await?;
            return Ok(Exit::code(3));
        }
    };

    let opts = JqOptions {
        raw_output,
        compact,
        slurp,
        no_newline,
    };

    // Process input
    let last_value = if null_input {
        // -n: don't read any input, use null
        process_value(&ctx, &compiled, Val::Null, &opts).await?
    } else if files.is_empty() {
        // Read from stdin
        process_handle(&ctx, &ctx.stdin, &compiled, &opts).await?
    } else {
        let mut last = None;
        for file_path in &files {
            let handle = match ctx.open_or_stdin(file_path).await {
                Ok(h) => h,
                Err(e) => {
                    ctx.eprintln(&format!("jq: {file_path}: {e}")).await?;
                    return Ok(Exit::code(2));
                }
            };
            last = process_handle(&ctx, &handle, &compiled, &opts).await?;
        }
        last
    };

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    // Exit status based on last value
    if exit_status {
        match last_value {
            Some(v) if is_falsy(&v) => return Ok(Exit::code(1)),
            None => return Ok(Exit::code(1)),
            _ => {}
        }
    }

    Ok(Exit::success())
}

struct JqOptions {
    raw_output: bool,
    compact: bool,
    slurp: bool,
    no_newline: bool,
}

const HELP_TEXT: &str = r#"jq - JSON processor (using jaq)

Usage: jq [OPTIONS] FILTER [FILE...]

Options:
  -h, --help            Show this help message
  -r, --raw-output      Output raw strings without quotes
  -c, --compact-output  Compact output (no pretty printing)
  -s, --slurp           Read all inputs into an array
  -n, --null-input      Don't read any input, use null
  -e, --exit-status     Exit 1 if last output is false or null
  -j, --join-output     No newline after each output

Examples:
  echo '{"name":"foo"}' | jq .name
  echo '[1,2,3]' | jq '.[] | . * 2'
  jq -n '{hello: "world"}'
  echo '{"a":1}' | jq -c .
  cat file.json | jq -r '.items[].name'

Filter syntax:
  .               Identity (return input unchanged)
  .foo            Access field "foo"
  .[0]            Access array index 0
  .[]             Iterate over array/object values
  .foo.bar        Chained field access
  .foo | .bar     Pipe output to next filter
  select(cond)    Keep values where condition is true
  map(f)          Apply filter to each array element
  keys            Get object keys as array
  length          Get length of string/array/object
  sort            Sort array
  unique          Remove duplicates
  group_by(.f)    Group array by field
  if c then t else f end   Conditional
"#;

/// Print help text.
async fn print_help(ctx: &CmdContext) -> Result<(), IoError> {
    ctx.println(HELP_TEXT).await
}

/// Compiled jq filter ready for execution.
type CompiledFilter = jaq_interpret::Filter;

/// Compile a jq filter string.
fn compile_filter(filter_str: &str) -> Result<CompiledFilter, String> {
    // Create parsing context with no global variables
    let mut defs = ParseCtx::new(Vec::new());

    // Add standard library definitions
    defs.insert_defs(jaq_std::std());

    // Add core native filters
    defs.insert_natives(jaq_core::core());

    // Parse the filter
    let (parsed, errs) = jaq_parse::parse(filter_str, jaq_parse::main());
    if !errs.is_empty() {
        return Err(errs
            .into_iter()
            .map(|e| format!("{e:?}"))
            .collect::<Vec<_>>()
            .join(", "));
    }

    let parsed = parsed.ok_or_else(|| "failed to parse filter".to_string())?;

    // Compile the filter
    let filter = defs.compile(parsed);
    if !defs.errs.is_empty() {
        return Err(defs
            .errs
            .into_iter()
            .map(|(e, _span)| format!("{e}"))
            .collect::<Vec<_>>()
            .join(", "));
    }

    Ok(filter)
}

/// Read all data from a handle into a byte vector.
async fn read_all(handle: &crate::io_handle::IoHandle) -> Result<Vec<u8>, IoError> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = handle.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    Ok(data)
}

/// Process input from an I/O handle.
async fn process_handle(
    ctx: &CmdContext,
    handle: &crate::io_handle::IoHandle,
    compiled: &CompiledFilter,
    opts: &JqOptions,
) -> Result<Option<Val>, IoError> {
    // Read all input
    let data = read_all(handle).await?;
    let input_str = String::from_utf8_lossy(&data);

    if opts.slurp {
        // Parse all values into an array
        let mut values = Vec::new();
        for line in input_str.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(json) => values.push(Val::from(json)),
                Err(e) => {
                    ctx.eprintln(&format!("jq: parse error: {e}")).await?;
                    return Err(IoError::Io(format!("JSON parse error: {e}")));
                }
            }
        }
        let arr = values.into_iter().collect::<Val>();
        process_value(ctx, compiled, arr, opts).await
    } else {
        // Process each JSON value separately (streaming)
        let mut last_value = None;

        // Try parsing as a single JSON document first
        let trimmed = input_str.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        // Try to parse as a single value or multiple values
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(json) => {
                let val = Val::from(json);
                last_value = process_value(ctx, compiled, val, opts).await?;
            }
            Err(_) => {
                // Try line-by-line parsing (NDJSON)
                for line in input_str.lines() {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(trimmed) {
                        Ok(json) => {
                            let val = Val::from(json);
                            last_value = process_value(ctx, compiled, val, opts).await?;
                        }
                        Err(e) => {
                            ctx.eprintln(&format!("jq: parse error: {e}")).await?;
                            return Err(IoError::Io(format!("JSON parse error: {e}")));
                        }
                    }
                }
            }
        }

        Ok(last_value)
    }
}

/// Process a single value through the filter and output results.
async fn process_value(
    ctx: &CmdContext,
    compiled: &CompiledFilter,
    input: Val,
    opts: &JqOptions,
) -> Result<Option<Val>, IoError> {
    // Create empty inputs iterator
    let inputs = RcIter::new(core::iter::empty());
    let ctx_jaq = Ctx::new([], &inputs);

    let mut last_value: Option<Val> = None;

    // Run the filter
    for result in compiled.run((ctx_jaq.clone(), input)) {
        match result {
            Ok(val) => {
                last_value = Some(val.clone());
                output_value(ctx, &val, opts).await?;
            }
            Err(e) => {
                ctx.eprintln(&format!("jq: error: {e}")).await?;
                return Err(IoError::Io(format!("jq error: {e}")));
            }
        }
    }

    Ok(last_value)
}

/// Output a single value.
async fn output_value(ctx: &CmdContext, val: &Val, opts: &JqOptions) -> Result<(), IoError> {
    let output = if opts.raw_output {
        // Raw output: strings without quotes
        match val {
            Val::Str(s) => s.to_string(),
            other => format_val(other, opts.compact),
        }
    } else {
        format_val(val, opts.compact)
    };

    if opts.no_newline {
        ctx.stdout_write_all(output.as_bytes()).await?;
    } else {
        ctx.println(&output).await?;
    }

    Ok(())
}

/// Format a value as JSON string.
fn format_val(val: &Val, compact: bool) -> String {
    let json: serde_json::Value = val.clone().into();
    if compact {
        serde_json::to_string(&json).unwrap_or_else(|_| "null".to_string())
    } else {
        serde_json::to_string_pretty(&json).unwrap_or_else(|_| "null".to_string())
    }
}

/// Check if a value is "falsy" (false or null).
fn is_falsy(val: &Val) -> bool {
    matches!(val, Val::Null | Val::Bool(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Environment;
    use crate::io_handle::IoHandle;
    use amla_scheduler::{RandomSourceFn, Scheduler, TimeSourceFn};
    use amla_vfs::{Permission, Vfs};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    // =========================================================================
    // Unit tests for helper functions
    // =========================================================================

    #[test]
    fn test_is_falsy() {
        assert!(is_falsy(&Val::Null));
        assert!(is_falsy(&Val::Bool(false)));
        assert!(!is_falsy(&Val::Bool(true)));
        assert!(!is_falsy(&Val::Int(0)));
        assert!(!is_falsy(&Val::from(String::new())));
    }

    #[test]
    fn test_is_falsy_numbers() {
        // 0 is NOT falsy in jq (unlike JavaScript)
        assert!(!is_falsy(&Val::Int(0)));
        assert!(!is_falsy(&Val::Int(-1)));
        assert!(!is_falsy(&Val::Int(1)));
    }

    #[test]
    fn test_is_falsy_empty_values() {
        // Empty string/array/object are NOT falsy in jq
        assert!(!is_falsy(&Val::from(String::new())));
        assert!(!is_falsy(&Val::from(serde_json::json!([]))));
        assert!(!is_falsy(&Val::from(serde_json::json!({}))));
    }

    #[test]
    fn test_format_val_compact() {
        let val = Val::from(serde_json::json!({"a": 1, "b": 2}));
        let output = format_val(&val, true);
        // Compact should have no newlines
        assert!(!output.contains('\n'));
        assert!(output.contains("\"a\""));
        assert!(output.contains("\"b\""));
    }

    #[test]
    fn test_format_val_pretty() {
        let val = Val::from(serde_json::json!({"a": 1}));
        let output = format_val(&val, false);
        // Pretty should have newlines and indentation
        assert!(output.contains('\n'));
        assert!(output.contains("  ")); // indentation
    }

    #[test]
    fn test_format_val_null() {
        let val = Val::Null;
        assert_eq!(format_val(&val, true), "null");
        assert_eq!(format_val(&val, false), "null");
    }

    #[test]
    fn test_format_val_string() {
        let val = Val::from("hello".to_string());
        assert_eq!(format_val(&val, true), "\"hello\"");
    }

    #[test]
    fn test_format_val_number() {
        let val = Val::Int(42);
        assert_eq!(format_val(&val, true), "42");
    }

    #[test]
    fn test_format_val_array() {
        let val = Val::from(serde_json::json!([1, 2, 3]));
        let output = format_val(&val, true);
        assert_eq!(output, "[1,2,3]");
    }

    // =========================================================================
    // Compile filter tests
    // =========================================================================

    #[test]
    fn test_compile_identity_filter() {
        let result = compile_filter(".");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_field_access() {
        let result = compile_filter(".foo");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_array_access() {
        let result = compile_filter(".[0]");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_pipe() {
        let result = compile_filter(".foo | .bar");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_map() {
        let result = compile_filter("map(. + 1)");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_select() {
        let result = compile_filter("select(. > 0)");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_invalid() {
        let result = compile_filter("[[[");
        assert!(result.is_err());
    }

    #[test]
    fn test_compile_keys() {
        let result = compile_filter("keys");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_length() {
        let result = compile_filter("length");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_sort() {
        let result = compile_filter("sort");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_unique() {
        let result = compile_filter("unique");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_group_by() {
        let result = compile_filter("group_by(.name)");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_conditional() {
        let result = compile_filter("if .x > 0 then \"positive\" else \"non-positive\" end");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_object_construction() {
        let result = compile_filter("{name: .foo, value: .bar}");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_array_construction() {
        let result = compile_filter("[.a, .b, .c]");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_recursive_descent() {
        let result = compile_filter("..");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_add() {
        let result = compile_filter("add");
        assert!(result.is_ok());
    }

    #[test]
    fn test_compile_first_last() {
        assert!(compile_filter("first").is_ok());
        assert!(compile_filter("last").is_ok());
    }

    #[test]
    fn test_compile_type() {
        let result = compile_filter("type");
        assert!(result.is_ok());
    }

    // =========================================================================
    // Filter execution tests
    // =========================================================================

    #[test]
    fn test_run_identity() {
        let filter = compile_filter(".").unwrap();
        let input = Val::from(serde_json::json!({"a": 1}));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
    }

    #[test]
    fn test_run_field_access() {
        let filter = compile_filter(".a").unwrap();
        let input = Val::from(serde_json::json!({"a": 42}));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        let val = results[0].as_ref().unwrap();
        assert_eq!(*val, Val::Int(42));
    }

    #[test]
    fn test_run_array_iteration() {
        let filter = compile_filter(".[]").unwrap();
        let input = Val::from(serde_json::json!([1, 2, 3]));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_run_nested_field_access() {
        let filter = compile_filter(".a.b.c").unwrap();
        let input = Val::from(serde_json::json!({"a": {"b": {"c": "deep"}}}));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        let val = results[0].as_ref().unwrap();
        assert_eq!(*val, Val::from("deep".to_string()));
    }

    #[test]
    fn test_run_map() {
        let filter = compile_filter("map(. * 2)").unwrap();
        let input = Val::from(serde_json::json!([1, 2, 3]));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        let arr: serde_json::Value = results[0].as_ref().unwrap().clone().into();
        assert_eq!(arr, serde_json::json!([2, 4, 6]));
    }

    #[test]
    fn test_run_select() {
        let filter = compile_filter(".[] | select(. > 2)").unwrap();
        let input = Val::from(serde_json::json!([1, 2, 3, 4]));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 2); // 3 and 4
    }

    #[test]
    fn test_run_keys() {
        let filter = compile_filter("keys").unwrap();
        let input = Val::from(serde_json::json!({"z": 1, "a": 2}));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        let arr: serde_json::Value = results[0].as_ref().unwrap().clone().into();
        // Keys are sorted alphabetically
        assert_eq!(arr, serde_json::json!(["a", "z"]));
    }

    #[test]
    fn test_run_length_array() {
        let filter = compile_filter("length").unwrap();
        let input = Val::from(serde_json::json!([1, 2, 3, 4, 5]));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].as_ref().unwrap(), Val::Int(5));
    }

    #[test]
    fn test_run_length_string() {
        let filter = compile_filter("length").unwrap();
        let input = Val::from("hello".to_string());
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].as_ref().unwrap(), Val::Int(5));
    }

    #[test]
    fn test_run_sort() {
        let filter = compile_filter("sort").unwrap();
        let input = Val::from(serde_json::json!([3, 1, 2]));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        let arr: serde_json::Value = results[0].as_ref().unwrap().clone().into();
        assert_eq!(arr, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn test_run_add() {
        let filter = compile_filter("add").unwrap();
        let input = Val::from(serde_json::json!([1, 2, 3]));
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);

        let results: Vec<_> = filter.run((ctx, input)).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(*results[0].as_ref().unwrap(), Val::Int(6));
    }

    #[test]
    fn test_run_type() {
        let filter = compile_filter("type").unwrap();

        // Test string
        let inputs = RcIter::new(core::iter::empty());
        let ctx = Ctx::new([], &inputs);
        let results: Vec<_> = filter
            .run((ctx.clone(), Val::from("hello".to_string())))
            .collect();
        assert_eq!(
            *results[0].as_ref().unwrap(),
            Val::from("string".to_string())
        );

        // Test number
        let inputs2 = RcIter::new(core::iter::empty());
        let ctx2 = Ctx::new([], &inputs2);
        let results2: Vec<_> = filter.run((ctx2, Val::Int(42))).collect();
        assert_eq!(
            *results2[0].as_ref().unwrap(),
            Val::from("number".to_string())
        );

        // Test array
        let inputs3 = RcIter::new(core::iter::empty());
        let ctx3 = Ctx::new([], &inputs3);
        let results3: Vec<_> = filter
            .run((ctx3, Val::from(serde_json::json!([]))))
            .collect();
        assert_eq!(
            *results3[0].as_ref().unwrap(),
            Val::from("array".to_string())
        );
    }

    // =========================================================================
    // Integration test helpers
    // =========================================================================

    fn test_scheduler() -> Scheduler {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    fn make_ctx_with_stdin(
        argv: Vec<&str>,
        scheduler: Scheduler,
        vfs: Rc<RefCell<Vfs>>,
        stdin_content: &str,
    ) -> (crate::CmdContext, IoHandle, IoHandle) {
        let stdin = IoHandle::from_string(stdin_content);
        let stdout = IoHandle::buffer();
        let stderr = IoHandle::buffer();
        let ctx = crate::CmdContext::new(
            argv.into_iter().map(ToString::to_string).collect(),
            stdin,
            stdout.clone(),
            stderr.clone(),
            "/workspace".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        );
        (ctx, stdout, stderr)
    }

    fn make_ctx_with_vfs(
        argv: Vec<&str>,
        scheduler: Scheduler,
        vfs: Rc<RefCell<Vfs>>,
    ) -> (crate::CmdContext, IoHandle, IoHandle) {
        let stdout = IoHandle::buffer();
        let stderr = IoHandle::buffer();
        let ctx = crate::CmdContext::new(
            argv.into_iter().map(ToString::to_string).collect(),
            IoHandle::null(),
            stdout.clone(),
            stderr.clone(),
            "/workspace".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        );
        (ctx, stdout, stderr)
    }

    // =========================================================================
    // Help tests
    // =========================================================================

    #[test]
    fn jq_help_short() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_vfs(vec!["jq", "-h"], scheduler.clone(), vfs);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("jq - JSON processor"));
        assert!(output.contains("--raw-output"));
        assert!(output.contains("--slurp"));
    }

    #[test]
    fn jq_help_long() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_vfs(vec!["jq", "--help"], scheduler.clone(), vfs);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("jq - JSON processor"));
    }

    // =========================================================================
    // Basic stdin tests
    // =========================================================================

    #[test]
    fn jq_identity_from_stdin() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "."], scheduler.clone(), vfs, r#"{"a":1}"#);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("\"a\""));
        assert!(output.contains('1'));
    }

    #[test]
    fn jq_field_access_from_stdin() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", ".name"],
            scheduler.clone(),
            vfs,
            r#"{"name":"foo"}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("\"foo\""));
    }

    #[test]
    fn jq_default_identity_filter() {
        // When no filter is provided, identity (.) is used
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq"], scheduler.clone(), vfs, r#"{"x":1}"#);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("\"x\""));
        assert!(output.contains('1'));
    }

    // =========================================================================
    // Raw output (-r) tests
    // =========================================================================

    #[test]
    fn jq_raw_output_string() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-r", ".name"],
            scheduler.clone(),
            vfs,
            r#"{"name":"hello"}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        // Raw output should not have quotes
        assert_eq!(output.trim(), "hello");
        assert!(!output.contains('"'));
    }

    #[test]
    fn jq_raw_output_long_option() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "--raw-output", ".name"],
            scheduler.clone(),
            vfs,
            r#"{"name":"test"}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "test");
    }

    #[test]
    fn jq_raw_output_number() {
        // -r with non-string values should still format normally
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-r", ".x"],
            scheduler.clone(),
            vfs,
            r#"{"x":42}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "42");
    }

    // =========================================================================
    // Compact output (-c) tests
    // =========================================================================

    #[test]
    fn jq_compact_output() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-c", "."],
            scheduler.clone(),
            vfs,
            r#"{"a":1,"b":2}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        // Compact output should be on one line (no extra newlines within)
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(!output.contains("  ")); // No indentation
    }

    #[test]
    fn jq_compact_output_long_option() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "--compact-output", "."],
            scheduler.clone(),
            vfs,
            r#"{"x":1}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), r#"{"x":1}"#);
    }

    // =========================================================================
    // Null input (-n) tests
    // =========================================================================

    #[test]
    fn jq_null_input() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-n", "42"],
            scheduler.clone(),
            vfs,
            "ignored input",
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "42");
    }

    #[test]
    fn jq_null_input_object_construction() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-n", "-c", r#"{hello: "world"}"#],
            scheduler.clone(),
            vfs,
            "",
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
    }

    #[test]
    fn jq_null_input_long_option() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "--null-input", "null"],
            scheduler.clone(),
            vfs,
            "",
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "null");
    }

    // =========================================================================
    // Slurp mode (-s) tests
    // =========================================================================

    #[test]
    fn jq_slurp_multiple_values() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-s", "-c", "."],
            scheduler.clone(),
            vfs,
            "1\n2\n3",
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "[1,2,3]");
    }

    #[test]
    fn jq_slurp_sum() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-s", "add"],
            scheduler.clone(),
            vfs,
            "1\n2\n3\n4",
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "10");
    }

    #[test]
    fn jq_slurp_long_option() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "--slurp", "-c", "."],
            scheduler.clone(),
            vfs,
            r#"{"a":1}
{"b":2}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains('['));
        assert!(output.contains(']'));
    }

    // =========================================================================
    // Exit status (-e) tests
    // =========================================================================

    #[test]
    fn jq_exit_status_null() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-e", ".missing"],
            scheduler.clone(),
            vfs,
            r#"{}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        // Missing field returns null, which is falsy, so exit code 1
        assert_eq!(result.code, 1);
    }

    #[test]
    fn jq_exit_status_false() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "-e", "."], scheduler.clone(), vfs, "false");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn jq_exit_status_true() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "-e", "."], scheduler.clone(), vfs, "true");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn jq_exit_status_number() {
        // Numbers (including 0) are NOT falsy
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "-e", "."], scheduler.clone(), vfs, "0");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn jq_exit_status_empty_string() {
        // Empty string is NOT falsy
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "-e", "."], scheduler.clone(), vfs, r#""""#);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn jq_exit_status_long_option() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "--exit-status", "."],
            scheduler.clone(),
            vfs,
            "null",
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);
    }

    // =========================================================================
    // No newline (-j) tests
    // =========================================================================

    #[test]
    fn jq_no_newline() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "-j", "."], scheduler.clone(), vfs, "42");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output, "42"); // No trailing newline
    }

    #[test]
    fn jq_no_newline_long_option() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "--join-output", "-r", "."],
            scheduler.clone(),
            vfs,
            r#""hello""#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output, "hello"); // No trailing newline, raw output
    }

    // =========================================================================
    // File input tests
    // =========================================================================

    #[test]
    fn jq_read_from_file() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file(
                "/workspace/data.json",
                br#"{"name":"from file"}"#,
                Permission::ReadWrite,
            )
            .unwrap();

        let (ctx, stdout, _stderr) = make_ctx_with_vfs(
            vec!["jq", "-r", ".name", "/workspace/data.json"],
            scheduler.clone(),
            vfs,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "from file");
    }

    #[test]
    fn jq_read_multiple_files() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file("/workspace/a.json", br#"{"x":1}"#, Permission::ReadWrite)
            .unwrap();
        vfs.borrow_mut()
            .write_file("/workspace/b.json", br#"{"x":2}"#, Permission::ReadWrite)
            .unwrap();

        let (ctx, stdout, _stderr) = make_ctx_with_vfs(
            vec!["jq", ".x", "/workspace/a.json", "/workspace/b.json"],
            scheduler.clone(),
            vfs,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains('1'));
        assert!(output.contains('2'));
    }

    #[test]
    fn jq_nonexistent_file() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));

        let (ctx, _stdout, stderr) = make_ctx_with_vfs(
            vec!["jq", ".", "/workspace/missing.json"],
            scheduler.clone(),
            vfs,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 2);

        let err_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(err_output.contains("jq:"));
        assert!(err_output.contains("missing.json"));
    }

    // =========================================================================
    // Error handling tests
    // =========================================================================

    #[test]
    fn jq_compile_error() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, stderr) =
            make_ctx_with_stdin(vec!["jq", "[[["], scheduler.clone(), vfs, "{}");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 3);

        let err_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(err_output.contains("jq: compile error"));
    }

    #[test]
    fn jq_parse_error() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, stderr) =
            make_ctx_with_stdin(vec!["jq", "."], scheduler.clone(), vfs, "not valid json");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        // Parse errors should result in an error
        let err_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(err_output.contains("jq: parse error"));
    }

    #[test]
    fn jq_empty_input() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "."], scheduler.clone(), vfs, "");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        // Empty input should succeed with no output
        assert_eq!(result.code, 0);

        let output = stdout.get_buffer().unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn jq_whitespace_only_input() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "."], scheduler.clone(), vfs, "   \n  \n  ");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = stdout.get_buffer().unwrap();
        assert!(output.is_empty());
    }

    // =========================================================================
    // NDJSON (newline-delimited JSON) tests
    // =========================================================================

    #[test]
    fn jq_ndjson_processing() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let ndjson = r#"{"a":1}
{"a":2}
{"a":3}"#;
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", ".a"], scheduler.clone(), vfs, ndjson);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains('1'));
        assert!(output.contains('2'));
        assert!(output.contains('3'));
    }

    #[test]
    fn jq_ndjson_with_blank_lines() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let ndjson = r#"{"x":1}

{"x":2}
"#;
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", ".x"], scheduler.clone(), vfs, ndjson);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains('1'));
        assert!(output.contains('2'));
    }

    // =========================================================================
    // Array iteration tests
    // =========================================================================

    #[test]
    fn jq_array_iteration() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", ".[]"],
            scheduler.clone(),
            vfs,
            r#"["a","b","c"]"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("\"a\""));
        assert!(output.contains("\"b\""));
        assert!(output.contains("\"c\""));
    }

    #[test]
    fn jq_array_element_access() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", ".[1]"], scheduler.clone(), vfs, r#"[10,20,30]"#);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "20");
    }

    // =========================================================================
    // Complex filter tests
    // =========================================================================

    #[test]
    fn jq_pipe_chain() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", ".items | .[] | .name"],
            scheduler.clone(),
            vfs,
            r#"{"items":[{"name":"foo"},{"name":"bar"}]}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("\"foo\""));
        assert!(output.contains("\"bar\""));
    }

    #[test]
    fn jq_select_filter() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", ".[] | select(.active)"],
            scheduler.clone(),
            vfs,
            r#"[{"name":"a","active":true},{"name":"b","active":false},{"name":"c","active":true}]"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("\"a\""));
        assert!(!output.contains("\"b\""));
        assert!(output.contains("\"c\""));
    }

    #[test]
    fn jq_map_filter() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-c", "map(. + 10)"],
            scheduler.clone(),
            vfs,
            r#"[1,2,3]"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "[11,12,13]");
    }

    #[test]
    fn jq_object_construction() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-c", "{x: .a, y: .b}"],
            scheduler.clone(),
            vfs,
            r#"{"a":1,"b":2,"c":3}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("\"x\""));
        assert!(output.contains("\"y\""));
        assert!(output.contains('1'));
        assert!(output.contains('2'));
    }

    // =========================================================================
    // Combined options tests
    // =========================================================================

    #[test]
    fn jq_raw_and_compact() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-r", "-c", ".name"],
            scheduler.clone(),
            vfs,
            r#"{"name":"test"}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "test");
    }

    #[test]
    fn jq_slurp_and_compact() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-s", "-c", "map(.x)"],
            scheduler.clone(),
            vfs,
            r#"{"x":1}
{"x":2}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "[1,2]");
    }

    #[test]
    fn jq_null_input_and_exit_status() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "-n", "-e", "null"], scheduler.clone(), vfs, "");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1); // null is falsy
    }

    // =========================================================================
    // Special JSON values tests
    // =========================================================================

    #[test]
    fn jq_boolean_values() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-c", "."],
            scheduler.clone(),
            vfs,
            r#"{"t":true,"f":false}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("true"));
        assert!(output.contains("false"));
    }

    #[test]
    fn jq_null_value() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", "."], scheduler.clone(), vfs, "null");

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "null");
    }

    #[test]
    fn jq_nested_arrays_and_objects() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", ".a[0].b"],
            scheduler.clone(),
            vfs,
            r#"{"a":[{"b":"deep"}]}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("\"deep\""));
    }

    #[test]
    fn jq_unicode_strings() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-r", ".msg"],
            scheduler.clone(),
            vfs,
            r#"{"msg":"Hello \u4e16\u754c"}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        // Should contain the decoded unicode characters
        assert!(output.contains("Hello"));
    }

    #[test]
    fn jq_escaped_strings() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "-r", "."],
            scheduler.clone(),
            vfs,
            r#""line1\nline2""#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        // Raw output should show actual newline
        assert!(output.contains("line1\nline2"));
    }

    // =========================================================================
    // Stdin with - argument tests
    // =========================================================================

    #[test]
    fn jq_dash_for_stdin() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", ".", "-"], scheduler.clone(), vfs, r#"{"x":99}"#);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("99"));
    }

    // =========================================================================
    // Large number handling tests
    // =========================================================================

    #[test]
    fn jq_large_integers() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) = make_ctx_with_stdin(
            vec!["jq", "."],
            scheduler.clone(),
            vfs,
            r#"{"big":9007199254740993}"#,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        // The number should be preserved (though exact representation depends on jaq)
        assert!(output.contains("900719925474099"));
    }

    #[test]
    fn jq_floating_point() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, stdout, _stderr) =
            make_ctx_with_stdin(vec!["jq", ".x"], scheduler.clone(), vfs, r#"{"x":3.14159}"#);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("3.14"));
    }
}
