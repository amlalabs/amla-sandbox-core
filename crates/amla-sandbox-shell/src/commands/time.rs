//! time - time a command

use amla_scheduler::{Exit, nanos_to_duration};

use crate::CmdContext;

use super::CommandResult;

const HELP: &str = "\
time - time a command

Usage: time COMMAND [ARGS...]

Measures and reports the elapsed wall-clock time for a command.
The timing information is printed to stderr.

Note: In this sandboxed environment, only real (wall-clock) time is available.
User and system time are not reported.

Examples:
  time echo hello     Time the echo command
  time sleep 1        Time a 1-second sleep
";

/// Format nanoseconds as "Xm Y.ZZZs" or "Y.ZZZs"
#[allow(clippy::cast_possible_truncation)] // Minutes value is bounded
fn format_time(nanos: u64) -> String {
    let duration = nanos_to_duration(nanos);
    let total_secs = duration.as_secs_f64();

    if total_secs >= 60.0 {
        let mins = (total_secs / 60.0).floor() as u64;
        let secs = total_secs - (mins as f64 * 60.0);
        format!("{mins}m{secs:.3}s")
    } else {
        format!("{total_secs:.3}s")
    }
}

/// time COMMAND [ARGS...] - time a command
pub async fn run(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    // Check for --help
    if args.first().is_some_and(|a| *a == "--help" || *a == "-h") {
        ctx.stdout_write_all(HELP.as_bytes()).await?;
        return Ok(Exit::success());
    }

    if args.is_empty() {
        ctx.eprintln("time: missing command").await?;
        return Ok(Exit::code(1));
    }

    // Get start time
    let start = ctx.now();

    // Build the command to execute
    // We'll execute directly using the command lookup
    let cmd_name = &args[0];
    let cmd_args: Vec<String> = args.to_vec();

    // Look up and execute the command
    let exit_code = if let Some(cmd_fn) = super::get_command(cmd_name) {
        // Create a new context with the command's argv
        let vfs = ctx.vfs();
        let scheduler = ctx.scheduler();
        let sub_ctx = CmdContext::new(
            cmd_args,
            ctx.stdin.clone(),
            ctx.stdout.clone(),
            ctx.stderr.clone(),
            ctx.cwd().to_string(),
            ctx.env_clone(),
            vfs,
            scheduler,
            ctx.tool_catalog(),
        );

        match cmd_fn(sub_ctx).await {
            Ok(exit) => exit.code,
            Err(_) => 1,
        }
    } else {
        ctx.eprintln(&format!("time: {cmd_name}: command not found"))
            .await?;
        127
    };

    // Get end time
    let end = ctx.now();

    // Calculate elapsed time
    let elapsed = end.saturating_sub(start);
    let formatted = format_time(elapsed);

    // Print timing info to stderr (like real time command)
    ctx.eprintln(&format!("\nreal\t{formatted}")).await?;

    // In a sandboxed environment, we don't have user/sys time
    // We could report them as 0, but that's misleading
    // So we just report real time only

    Ok(Exit::code(exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Unit tests for format_time
    // =========================================================================

    #[test]
    fn format_subsecond() {
        assert_eq!(format_time(500_000_000), "0.500s");
        assert_eq!(format_time(1_500_000), "0.002s"); // rounds
        assert_eq!(format_time(0), "0.000s");
    }

    #[test]
    fn format_seconds() {
        assert_eq!(format_time(1_000_000_000), "1.000s");
        assert_eq!(format_time(5_500_000_000), "5.500s");
        assert_eq!(format_time(59_000_000_000), "59.000s");
    }

    #[test]
    fn format_minutes() {
        assert_eq!(format_time(60_000_000_000), "1m0.000s");
        assert_eq!(format_time(90_000_000_000), "1m30.000s");
        assert_eq!(format_time(125_500_000_000), "2m5.500s");
    }

    // =========================================================================
    // Integration tests with scheduler
    // =========================================================================

    use crate::Environment;
    use crate::io_handle::IoHandle;
    use amla_scheduler::{RandomSourceFn, Scheduler, SchedulerState, TimeSourceFn};
    use amla_vfs::Vfs;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn test_scheduler() -> Scheduler {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    fn make_ctx(argv: Vec<&str>, scheduler: Scheduler) -> (CmdContext, IoHandle, IoHandle) {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let stdout = IoHandle::buffer();
        let stderr = IoHandle::buffer();
        let ctx = CmdContext::new(
            argv.into_iter()
                .map(std::string::ToString::to_string)
                .collect(),
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

    #[test]
    fn time_missing_command() {
        let scheduler = test_scheduler();
        let (ctx, _stdout, _stderr) = make_ctx(vec!["time"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Should fail immediately, no host ops
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(!scheduler.has_pending_host_ops());

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn time_command_not_found() {
        let scheduler = test_scheduler();
        let (ctx, _stdout, _stderr) = make_ctx(vec!["time", "nonexistent_cmd"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // now() is sync, so should complete in one run
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 127);
    }

    #[test]
    fn time_true_command() {
        let scheduler = test_scheduler();
        let (ctx, _stdout, stderr) = make_ctx(vec!["time", "true"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // now() is sync - should complete in one run
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Check stderr contains timing info
        let stderr_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(stderr_output.contains("real"), "should contain 'real'");
    }

    #[test]
    fn time_false_command() {
        let scheduler = test_scheduler();
        let (ctx, _stdout, _stderr) = make_ctx(vec!["time", "false"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // now() is sync - completes in one run
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1); // false returns 1
    }

    #[test]
    fn time_no_host_ops_for_time() {
        // now() is sync - no host ops required for timing
        let scheduler = test_scheduler();
        let (ctx, _stdout, _stderr) = make_ctx(vec!["time", "true"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Should complete immediately without blocking
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(!scheduler.has_pending_host_ops());
        assert!(handle.is_complete());
    }

    #[test]
    fn time_with_echo() {
        let scheduler = test_scheduler();
        let (ctx, stdout, stderr) = make_ctx(vec!["time", "echo", "hello"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Completes in one run
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Echo should have produced output
        let stdout_output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(stdout_output.contains("hello"));

        // Timing info on stderr
        let stderr_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(stderr_output.contains("real"));
    }

    // =========================================================================
    // Additional tests for coverage gaps
    // =========================================================================

    #[test]
    fn time_help_long_flag() {
        let scheduler = test_scheduler();
        let (ctx, stdout, _stderr) = make_ctx(vec!["time", "--help"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Check stdout contains help text
        let stdout_output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(stdout_output.contains("Usage: time COMMAND"));
        assert!(stdout_output.contains("Measures and reports"));
    }

    #[test]
    fn time_help_short_flag() {
        let scheduler = test_scheduler();
        let (ctx, stdout, _stderr) = make_ctx(vec!["time", "-h"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Check stdout contains help text
        let stdout_output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(stdout_output.contains("Usage: time COMMAND"));
    }

    #[test]
    fn time_missing_command_stderr_message() {
        let scheduler = test_scheduler();
        let (ctx, _stdout, stderr) = make_ctx(vec!["time"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);

        // Verify the error message
        let stderr_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(stderr_output.contains("time: missing command"));
    }

    #[test]
    fn time_command_not_found_stderr_message() {
        let scheduler = test_scheduler();
        let (ctx, _stdout, stderr) = make_ctx(vec!["time", "nonexistent_cmd"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 127);

        // Verify the error message includes the command name
        let stderr_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(stderr_output.contains("time: nonexistent_cmd: command not found"));
    }

    #[test]
    fn time_stderr_format_with_tab() {
        // Verify the exact format of timing output: "\nreal\t{time}"
        let scheduler = test_scheduler();
        let (ctx, _stdout, stderr) = make_ctx(vec!["time", "true"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let stderr_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        // Should contain newline, "real", tab, and time ending in 's'
        assert!(stderr_output.contains("\nreal\t"));
        assert!(stderr_output.ends_with("s\n"));
    }

    #[test]
    fn time_measures_elapsed_time() {
        // Test that time actually measures elapsed time by advancing mock time
        // Create a time source that increments each time it's called
        // First call returns 0 (start time), second call returns 1 billion ns (1 second)
        let call_count = Rc::new(Cell::new(0u64));
        let call_count_clone = call_count.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| {
            let count = call_count_clone.get();
            call_count_clone.set(count + 1);
            // First call (start): 0, Second call (end): 1 second
            count * 1_000_000_000
        });
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        let scheduler = Scheduler::new(1, time_source, random_source);

        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let stdout = IoHandle::buffer();
        let stderr = IoHandle::buffer();
        let ctx = CmdContext::new(
            vec!["time".to_string(), "true".to_string()],
            IoHandle::null(),
            stdout.clone(),
            stderr.clone(),
            "/workspace".to_string(),
            Environment::new(),
            vfs,
            scheduler.clone(),
            None,
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let stderr_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        // Should show 1 second elapsed (second call - first call)
        assert!(
            stderr_output.contains("1.000s"),
            "Expected 1.000s in output: {stderr_output}"
        );
    }

    #[test]
    fn time_with_exit_command() {
        // Test timing a command that returns a specific exit code
        let scheduler = test_scheduler();
        let (ctx, _stdout, stderr) = make_ctx(vec!["time", "exit", "42"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        // Should propagate exit code from 'exit 42'
        assert_eq!(result.code, 42);

        // Should still have timing info
        let stderr_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(stderr_output.contains("real\t"));
    }

    // =========================================================================
    // Additional unit tests for format_time edge cases
    // =========================================================================

    #[test]
    fn format_time_boundary_59_999() {
        // Just under 60 seconds - should not show minutes
        assert_eq!(format_time(59_999_000_000), "59.999s");
    }

    #[test]
    fn format_time_boundary_60_exact() {
        // Exactly 60 seconds - should show 1m0.000s
        assert_eq!(format_time(60_000_000_000), "1m0.000s");
    }

    #[test]
    fn format_time_boundary_60_001() {
        // Just over 60 seconds
        assert_eq!(format_time(60_001_000_000), "1m0.001s");
    }

    #[test]
    fn format_time_large_values() {
        // 1 hour = 3600 seconds = 60 minutes
        assert_eq!(format_time(3600_000_000_000), "60m0.000s");
        // 1 hour 30 minutes 45.5 seconds
        assert_eq!(format_time(5445_500_000_000), "90m45.500s");
    }

    #[test]
    fn format_time_very_small() {
        // 1 microsecond
        assert_eq!(format_time(1_000), "0.000s");
        // 1 millisecond
        assert_eq!(format_time(1_000_000), "0.001s");
        // 999 microseconds (rounds to 0.001)
        assert_eq!(format_time(999_000), "0.001s");
    }

    #[test]
    fn format_time_rounding() {
        // Test rounding behavior
        // 1.5 ms = 0.0015s, rounds to 0.002s
        assert_eq!(format_time(1_500_000), "0.002s");
        // 1.4999 ms = 0.0014999s, rounds to 0.001s
        assert_eq!(format_time(1_499_900), "0.001s");
    }
}
