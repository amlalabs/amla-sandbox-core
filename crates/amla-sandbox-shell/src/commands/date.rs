//! date - print or set the system date and time
//!
//! Usage: date [+FORMAT]
//!
//! Displays the current date and time from the scheduler's timer.
//! Format strings are simplified (not full strftime support):
//!   %Y - year (4 digits)
//!   %m - month (01-12)
//!   %d - day of month (01-31)
//!   %H - hour (00-23)
//!   %M - minute (00-59)
//!   %S - second (00-59)
//!   %s - seconds since epoch (Unix timestamp)
//!   %N - nanoseconds (9 digits)
//!   %% - literal %

use amla_scheduler::{Exit, nanos_to_duration};

use crate::CmdContext;

use super::CommandResult;

/// Convert nanoseconds since epoch to date components.
/// Returns (year, month, day, hour, minute, second, nanos).
#[allow(clippy::cast_possible_truncation)] // Date/time values are bounded
fn nanos_to_date(nanos: u64) -> (i32, u8, u8, u8, u8, u8, u32) {
    let duration = nanos_to_duration(nanos);
    let secs = duration.as_secs();
    let nanos_part = duration.subsec_nanos();

    // Simple calculation from Unix epoch (1970-01-01 00:00:00 UTC)
    // This is a simplified implementation - not handling leap seconds, etc.

    let seconds_per_day = 86400u64;
    let mut days = secs / seconds_per_day;
    let day_secs = (secs % seconds_per_day) as u32;

    let hour = (day_secs / 3600) as u8;
    let minute = ((day_secs % 3600) / 60) as u8;
    let second = (day_secs % 60) as u8;

    // Calculate year, month, day from days since epoch
    // Using a simplified algorithm
    let mut year = 1970i32;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    // Now find month and day
    let days_in_months = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u8;
    for days_in_month in &days_in_months {
        if days < *days_in_month as u64 {
            break;
        }
        days -= *days_in_month as u64;
        month += 1;
    }

    let day = (days + 1) as u8; // Days are 1-indexed

    (year, month, day, hour, minute, second, nanos_part)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// Format a date using the given format string.
fn format_date(format: &str, nanos: u64) -> String {
    use std::fmt::Write;

    let (year, month, day, hour, minute, second, nanos_part) = nanos_to_date(nanos);
    let secs = nanos / 1_000_000_000;

    let mut result = String::new();
    let mut chars = format.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&next) = chars.peek() {
                match next {
                    'Y' => {
                        chars.next();
                        let _ = write!(result, "{year:04}");
                    }
                    'm' => {
                        chars.next();
                        let _ = write!(result, "{month:02}");
                    }
                    'd' => {
                        chars.next();
                        let _ = write!(result, "{day:02}");
                    }
                    'H' => {
                        chars.next();
                        let _ = write!(result, "{hour:02}");
                    }
                    'M' => {
                        chars.next();
                        let _ = write!(result, "{minute:02}");
                    }
                    'S' => {
                        chars.next();
                        let _ = write!(result, "{second:02}");
                    }
                    's' => {
                        chars.next();
                        let _ = write!(result, "{secs}");
                    }
                    'N' => {
                        chars.next();
                        let _ = write!(result, "{nanos_part:09}");
                    }
                    '%' => {
                        chars.next();
                        result.push('%');
                    }
                    _ => {
                        // Unknown format specifier, keep as-is
                        result.push('%');
                    }
                }
            } else {
                result.push('%');
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// date [+FORMAT] - print the system date and time
pub async fn run(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    // Validate arguments before any async operations
    let format = if args.is_empty() {
        // Default format: similar to "date" output
        // e.g., "2024-01-15 14:30:00"
        "%Y-%m-%d %H:%M:%S".to_string()
    } else if let Some(fmt) = args.first().and_then(|s| s.strip_prefix('+')) {
        // Custom format
        fmt.to_string()
    } else {
        // Unknown argument - fail immediately without host ops
        ctx.eprintln(&format!("date: invalid option -- '{}'", args[0]))
            .await?;
        return Ok(Exit::code(1));
    };

    // Get current realtime (wall clock) from scheduler (only after validation)
    let now = ctx.now_realtime();

    let output = format_date(&format, now);
    ctx.println(&output).await?;

    Ok(Exit::success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leap_year() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2023));
    }

    #[test]
    fn format_epoch() {
        // Unix epoch: 1970-01-01 00:00:00
        let epoch = 0u64;
        assert_eq!(format_date("%Y-%m-%d", epoch), "1970-01-01");
        assert_eq!(format_date("%H:%M:%S", epoch), "00:00:00");
        assert_eq!(format_date("%s", epoch), "0");
    }

    #[test]
    fn format_known_date() {
        // 2024-01-01 00:00:00 UTC
        // Unix timestamp: 1704067200 seconds (verified externally)
        let secs = 1704067200u64;
        let nanos = secs * 1_000_000_000;
        assert_eq!(format_date("%Y-%m-%d", nanos), "2024-01-01");
        assert_eq!(format_date("%H:%M:%S", nanos), "00:00:00");
    }

    #[test]
    fn format_specifiers() {
        let nanos = 1_000_000_000u64; // 1 second after epoch
        assert_eq!(format_date("%s", nanos), "1");
        assert_eq!(format_date("%%", nanos), "%");
        assert_eq!(format_date("%N", nanos), "000000000");
    }

    // =========================================================================
    // Integration tests with scheduler
    // =========================================================================

    use crate::CmdContext;
    use crate::Environment;
    use crate::io_handle::IoHandle;
    use amla_scheduler::{ClockType, RandomSourceFn, Scheduler, SchedulerState, TimeSourceFn};
    use amla_vfs::Vfs;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Create a test scheduler with controllable mock time.
    fn test_scheduler_with_time(time_nanos: u64) -> Scheduler {
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_nanos);
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    fn make_ctx(argv: Vec<&str>, scheduler: Scheduler) -> (CmdContext, IoHandle) {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let stdout = IoHandle::buffer();
        let ctx = CmdContext::new(
            argv.into_iter()
                .map(std::string::ToString::to_string)
                .collect(),
            IoHandle::null(),
            stdout.clone(),
            IoHandle::buffer(),
            "/workspace".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        );
        (ctx, stdout)
    }

    #[test]
    fn date_no_host_ops_for_time() {
        // now() is sync - no host ops required for getting current time
        // Use 2024-01-01 00:00:00 UTC in nanoseconds
        let scheduler = test_scheduler_with_time(1704067200_000_000_000);
        let (ctx, _stdout) = make_ctx(vec!["date"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Should complete immediately without blocking (now() is sync)
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(!scheduler.has_pending_host_ops());
        assert!(handle.is_complete());
    }

    #[test]
    fn date_completes_immediately() {
        // Use 2024-01-01 00:00:00 UTC in nanoseconds
        let scheduler = test_scheduler_with_time(1704067200_000_000_000);
        let (ctx, stdout) = make_ctx(vec!["date"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // date command completes in a single run (sync time)
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(handle.is_complete());

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Verify output contains a valid date format
        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        // Should be in format "YYYY-MM-DD HH:MM:SS"
        assert!(output.contains('-'), "output should contain date: {output}");
    }

    #[test]
    fn date_custom_format() {
        // Use 2024-01-01 00:00:00 UTC in nanoseconds
        let scheduler = test_scheduler_with_time(1704067200_000_000_000);
        let (ctx, stdout) = make_ctx(vec!["date", "+%Y"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Output should be just the year (2024)
        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        let year: i32 = output.trim().parse().unwrap();
        assert_eq!(year, 2024, "year should be 2024: {year}");
    }

    #[test]
    fn date_unix_timestamp() {
        // Use 2024-01-01 00:00:00 UTC in nanoseconds
        let scheduler = test_scheduler_with_time(1704067200_000_000_000);
        let (ctx, stdout) = make_ctx(vec!["date", "+%s"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Output should be the Unix timestamp we set
        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        let timestamp: u64 = output.trim().parse().unwrap();
        assert_eq!(timestamp, 1704067200, "timestamp should be 1704067200");
    }

    #[test]
    fn date_invalid_option() {
        let scheduler = test_scheduler_with_time(0);
        let (ctx, _stdout) = make_ctx(vec!["date", "-x"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Invalid option should fail immediately, no host ops
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(!scheduler.has_pending_host_ops());

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn date_no_host_ops_on_invalid() {
        let scheduler = test_scheduler_with_time(0);
        let (ctx, _stdout) = make_ctx(vec!["date", "--invalid"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Run should complete immediately without blocking
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(!scheduler.has_pending_host_ops());

        assert!(handle.is_complete());
        assert_eq!(handle.try_get().unwrap().unwrap().code, 1);
    }

    // =========================================================================
    // Clock type tests - verify date uses realtime, not monotonic
    // =========================================================================

    /// Create a scheduler where realtime and monotonic return different values.
    fn test_scheduler_with_distinct_clocks(realtime_nanos: u64, monotonic_nanos: u64) -> Scheduler {
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, clock| match clock {
            ClockType::Realtime => realtime_nanos,
            ClockType::Monotonic => monotonic_nanos,
        });
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    #[test]
    fn date_uses_realtime_not_monotonic() {
        // Realtime: 2024-01-15 12:30:00 UTC
        let realtime_nanos = 1705321800_000_000_000u64;
        // Monotonic: 2020-01-01 00:00:00 UTC (different year to make it obvious)
        let monotonic_nanos = 1577836800_000_000_000u64;

        let scheduler = test_scheduler_with_distinct_clocks(realtime_nanos, monotonic_nanos);
        let (ctx, stdout) = make_ctx(vec!["date", "+%Y"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        let year: i32 = output.trim().parse().unwrap();

        // Should be 2024 (realtime), NOT 2020 (monotonic)
        assert_eq!(
            year, 2024,
            "date should use realtime clock, got year {year}"
        );
    }

    #[test]
    fn date_unix_timestamp_uses_realtime() {
        // Realtime: known timestamp
        let realtime_nanos = 1705321800_000_000_000u64; // 2024-01-15 12:30:00 UTC
        // Monotonic: completely different value
        let monotonic_nanos = 1000_000_000_000u64; // ~31 years after epoch

        let scheduler = test_scheduler_with_distinct_clocks(realtime_nanos, monotonic_nanos);
        let (ctx, stdout) = make_ctx(vec!["date", "+%s"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        let timestamp: u64 = output.trim().parse().unwrap();

        // Should be realtime seconds (1705321800), NOT monotonic seconds (1000)
        assert_eq!(
            timestamp, 1705321800,
            "date %s should use realtime clock, got {timestamp}"
        );
    }

    #[test]
    fn date_full_format_uses_realtime() {
        // Realtime: 2024-06-15 (mid-year)
        let realtime_nanos = 1718441400_000_000_000u64; // 2024-06-15 10:30:00 UTC
        // Monotonic: 2019-12-01 (different year and month)
        let monotonic_nanos = 1575158400_000_000_000u64; // 2019-12-01 00:00:00 UTC

        let scheduler = test_scheduler_with_distinct_clocks(realtime_nanos, monotonic_nanos);
        let (ctx, stdout) = make_ctx(vec!["date", "+%Y-%m-%d"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        let date = output.trim();

        // Should be 2024-06-15 (realtime), NOT 2019-12-01 (monotonic)
        assert_eq!(
            date, "2024-06-15",
            "date should use realtime clock, got {date}"
        );
    }

    // =========================================================================
    // Additional unit tests for format_date edge cases
    // =========================================================================

    #[test]
    fn format_all_specifiers() {
        // 2024-07-15 14:40:45.123456789 UTC
        // Unix timestamp: 1721054445 seconds
        let secs = 1721054445u64;
        let nanos = secs * 1_000_000_000 + 123_456_789;

        assert_eq!(format_date("%Y", nanos), "2024");
        assert_eq!(format_date("%m", nanos), "07");
        assert_eq!(format_date("%d", nanos), "15");
        assert_eq!(format_date("%H", nanos), "14");
        assert_eq!(format_date("%M", nanos), "40");
        assert_eq!(format_date("%S", nanos), "45");
        assert_eq!(format_date("%s", nanos), "1721054445");
        assert_eq!(format_date("%N", nanos), "123456789");
        assert_eq!(format_date("%%", nanos), "%");
    }

    #[test]
    fn format_nanoseconds_nonzero() {
        // Test %N with various nanosecond values
        let base_secs = 1704067200u64 * 1_000_000_000; // 2024-01-01 00:00:00

        // Small nanoseconds (should be zero-padded to 9 digits)
        assert_eq!(format_date("%N", base_secs + 1), "000000001");
        assert_eq!(format_date("%N", base_secs + 123), "000000123");
        assert_eq!(format_date("%N", base_secs + 1_000_000), "001000000");
        assert_eq!(format_date("%N", base_secs + 999_999_999), "999999999");
    }

    #[test]
    fn format_unknown_specifier() {
        // Unknown specifiers should preserve the % but not consume the character
        let nanos = 0u64;
        assert_eq!(format_date("%x", nanos), "%x");
        assert_eq!(format_date("%z", nanos), "%z");
        assert_eq!(format_date("%1", nanos), "%1");
        assert_eq!(format_date("%!", nanos), "%!");
    }

    #[test]
    fn format_trailing_percent() {
        // Trailing % should be preserved
        let nanos = 0u64;
        assert_eq!(format_date("test%", nanos), "test%");
        assert_eq!(format_date("%", nanos), "%");
    }

    #[test]
    fn format_empty_string() {
        let nanos = 0u64;
        assert_eq!(format_date("", nanos), "");
    }

    #[test]
    fn format_no_specifiers() {
        let nanos = 0u64;
        assert_eq!(format_date("hello world", nanos), "hello world");
    }

    #[test]
    fn format_mixed_specifiers_and_literals() {
        // 2024-01-01 00:00:00 UTC
        let nanos = 1704067200_000_000_000u64;
        assert_eq!(
            format_date("Year: %Y, Month: %m, Day: %d", nanos),
            "Year: 2024, Month: 01, Day: 01"
        );
        assert_eq!(
            format_date("100%% complete at %H:%M:%S", nanos),
            "100% complete at 00:00:00"
        );
    }

    #[test]
    fn format_consecutive_specifiers() {
        let nanos = 1704067200_000_000_000u64; // 2024-01-01 00:00:00
        assert_eq!(format_date("%Y%m%d", nanos), "20240101");
        assert_eq!(format_date("%H%M%S", nanos), "000000");
        assert_eq!(format_date("%%%%", nanos), "%%"); // Two literal percents
    }

    #[test]
    fn format_leap_day() {
        // 2024-02-29 12:00:00 UTC (leap day)
        // Unix timestamp: 1709208000 seconds
        let nanos = 1709208000_000_000_000u64;
        assert_eq!(format_date("%Y-%m-%d", nanos), "2024-02-29");
    }

    #[test]
    fn format_end_of_year() {
        // 2024-12-31 23:59:59 UTC
        // Unix timestamp: 1735689599 seconds
        let nanos = 1735689599_000_000_000u64;
        assert_eq!(format_date("%Y-%m-%d", nanos), "2024-12-31");
        assert_eq!(format_date("%H:%M:%S", nanos), "23:59:59");
    }

    #[test]
    fn format_various_months() {
        // Test each month to verify correct day-of-month handling
        // Using noon on the 15th of each month in 2024

        // 2024-03-15 12:00:00 UTC = 1710504000
        assert_eq!(
            format_date("%Y-%m-%d", 1710504000_000_000_000u64),
            "2024-03-15"
        );

        // 2024-06-15 12:00:00 UTC = 1718452800
        assert_eq!(
            format_date("%Y-%m-%d", 1718452800_000_000_000u64),
            "2024-06-15"
        );

        // 2024-09-15 12:00:00 UTC = 1726401600
        assert_eq!(
            format_date("%Y-%m-%d", 1726401600_000_000_000u64),
            "2024-09-15"
        );

        // 2024-11-15 12:00:00 UTC = 1731672000
        assert_eq!(
            format_date("%Y-%m-%d", 1731672000_000_000_000u64),
            "2024-11-15"
        );
    }

    // =========================================================================
    // Additional leap year tests
    // =========================================================================

    #[test]
    fn leap_year_comprehensive() {
        // Century years not divisible by 400 are NOT leap years
        assert!(!is_leap_year(1700));
        assert!(!is_leap_year(1800));
        assert!(!is_leap_year(2100));
        assert!(!is_leap_year(2200));
        assert!(!is_leap_year(2300));

        // Century years divisible by 400 ARE leap years
        assert!(is_leap_year(1600));
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2400));

        // Regular divisible by 4 years ARE leap years
        assert!(is_leap_year(2004));
        assert!(is_leap_year(2008));
        assert!(is_leap_year(2012));
        assert!(is_leap_year(2016));
        assert!(is_leap_year(2020));

        // Years not divisible by 4 are NOT leap years
        assert!(!is_leap_year(2001));
        assert!(!is_leap_year(2002));
        assert!(!is_leap_year(2003));
        assert!(!is_leap_year(2019));
        assert!(!is_leap_year(2021));
    }

    // =========================================================================
    // Additional integration tests
    // =========================================================================

    /// Helper to create context with access to stderr
    fn make_ctx_with_stderr(
        argv: Vec<&str>,
        scheduler: Scheduler,
    ) -> (CmdContext, IoHandle, IoHandle) {
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
    fn date_empty_format() {
        // `date +` should produce empty output (just newline)
        let scheduler = test_scheduler_with_time(1704067200_000_000_000);
        let (ctx, stdout, _stderr) = make_ctx_with_stderr(vec!["date", "+"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        // Should just be a newline (from println)
        assert_eq!(output, "\n");
    }

    #[test]
    fn date_invalid_option_error_message() {
        let scheduler = test_scheduler_with_time(0);
        let (ctx, _stdout, stderr) = make_ctx_with_stderr(vec!["date", "-x"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);

        let error = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(
            error.contains("date: invalid option -- '-x'"),
            "error message should mention invalid option: {error}"
        );
    }

    #[test]
    fn date_default_format_output() {
        // 2024-07-15 14:40:45 UTC
        let nanos = 1721054445_000_000_000u64;
        let scheduler = test_scheduler_with_time(nanos);
        let (ctx, stdout, _stderr) = make_ctx_with_stderr(vec!["date"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "2024-07-15 14:40:45");
    }

    #[test]
    fn date_complex_format() {
        // 2024-03-14 09:26:53 UTC (approximate Pi Day + Time)
        let nanos = 1710408413_000_000_000u64;
        let scheduler = test_scheduler_with_time(nanos);
        let (ctx, stdout, _stderr) = make_ctx_with_stderr(
            vec![
                "date",
                "+Today is %Y-%m-%d and time is %H:%M:%S (epoch: %s)",
            ],
            scheduler.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(
            output.trim(),
            "Today is 2024-03-14 and time is 09:26:53 (epoch: 1710408413)"
        );
    }

    #[test]
    fn date_nanoseconds_format() {
        // 2024-01-01 00:00:00.123456789 UTC
        let nanos = 1704067200_000_000_000u64 + 123_456_789;
        let scheduler = test_scheduler_with_time(nanos);
        let (ctx, stdout, _stderr) =
            make_ctx_with_stderr(vec!["date", "+%s.%N"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "1704067200.123456789");
    }

    #[test]
    fn date_multiple_percent_literals() {
        let scheduler = test_scheduler_with_time(0);
        let (ctx, stdout, _stderr) =
            make_ctx_with_stderr(vec!["date", "+100%% done %%%% end"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert_eq!(output.trim(), "100% done %% end");
    }
}
