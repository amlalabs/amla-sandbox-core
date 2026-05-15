//! sleep - suspend execution for an interval
//!
//! Usage: sleep NUMBER[SUFFIX]...
//!
//! SUFFIX may be 's' for seconds (default), 'm' for minutes,
//! 'h' for hours, or 'd' for days.

use std::time::Duration;

use amla_scheduler::Exit;

use crate::CmdContext;

use super::CommandResult;

/// Parse a sleep duration string (e.g., "1.5", "2m", "1h30m").
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Check for suffix
    let (num_part, multiplier) = if let Some(num) = s.strip_suffix('s') {
        (num, 1.0)
    } else if let Some(num) = s.strip_suffix('m') {
        (num, 60.0)
    } else if let Some(num) = s.strip_suffix('h') {
        (num, 3600.0)
    } else if let Some(num) = s.strip_suffix('d') {
        (num, 86400.0)
    } else {
        (s, 1.0) // Default to seconds
    };

    let value: f64 = num_part.parse().ok()?;
    if value < 0.0 || !value.is_finite() {
        return None;
    }

    let total_secs = value * multiplier;
    Some(Duration::from_secs_f64(total_secs))
}

/// sleep NUMBER[SUFFIX]... - suspend execution for interval of time
pub async fn run(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    if args.is_empty() {
        ctx.eprintln("sleep: missing operand").await?;
        return Ok(Exit::code(1));
    }

    // Sum all durations
    let mut total = Duration::ZERO;
    for arg in args {
        if let Some(dur) = parse_duration(arg) {
            total += dur;
        } else {
            ctx.eprintln(&format!("sleep: invalid time interval '{arg}'"))
                .await?;
            return Ok(Exit::code(1));
        }
    }

    // Sleep using the scheduler
    ctx.sleep(total).await?;

    Ok(Exit::success())
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Unit tests for parse_duration
    // =========================================================================

    #[test]
    fn parse_seconds() {
        assert_eq!(parse_duration("5"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("5s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("0.5"), Some(Duration::from_millis(500)));
        assert_eq!(parse_duration("0.5s"), Some(Duration::from_millis(500)));
    }

    #[test]
    fn parse_minutes() {
        assert_eq!(parse_duration("1m"), Some(Duration::from_mins(1)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_mins(2)));
        assert_eq!(parse_duration("0.5m"), Some(Duration::from_secs(30)));
    }

    #[test]
    fn parse_hours() {
        assert_eq!(parse_duration("1h"), Some(Duration::from_hours(1)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_hours(2)));
    }

    #[test]
    fn parse_days() {
        assert_eq!(parse_duration("1d"), Some(Duration::from_hours(24)));
    }

    #[test]
    fn parse_invalid() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("-5"), None);
    }

    #[test]
    fn parse_fractional_all_units() {
        assert_eq!(parse_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(parse_duration("1.5m"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("1.5h"), Some(Duration::from_mins(90)));
        assert_eq!(parse_duration("1.5d"), Some(Duration::from_hours(36)));
    }

    #[test]
    fn parse_zero() {
        assert_eq!(parse_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_duration("0s"), Some(Duration::ZERO));
        assert_eq!(parse_duration("0m"), Some(Duration::ZERO));
    }

    #[test]
    fn parse_whitespace() {
        assert_eq!(parse_duration("  5  "), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration("  5s  "), Some(Duration::from_secs(5)));
    }

    #[test]
    fn parse_large_values() {
        assert_eq!(parse_duration("86400"), Some(Duration::from_hours(24)));
        assert_eq!(parse_duration("365d"), Some(Duration::from_hours(8760)));
    }

    #[test]
    fn parse_very_small() {
        assert_eq!(parse_duration("0.001"), Some(Duration::from_millis(1)));
        assert_eq!(parse_duration("0.0001"), Some(Duration::from_micros(100)));
    }

    #[test]
    fn parse_infinity_nan() {
        assert_eq!(parse_duration("inf"), None);
        assert_eq!(parse_duration("NaN"), None);
    }

    // =========================================================================
    // Integration tests with scheduler
    // =========================================================================

    use crate::CmdContext;
    use crate::Environment;
    use crate::io_handle::IoHandle;
    use amla_scheduler::{HostOpKind, RandomSourceFn, Scheduler, SchedulerState, TimeSourceFn};
    use amla_vfs::Vfs;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn test_scheduler() -> Scheduler {
        let (sched, _) = test_scheduler_with_time();
        sched
    }

    /// Create a scheduler with controllable mock time.
    /// Returns (scheduler, mock_time) where mock_time can be advanced.
    fn test_scheduler_with_time() -> (Scheduler, Rc<Cell<u64>>) {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        (Scheduler::new(1, time_source, random_source), mock_time)
    }

    /// Helper to complete a `WakeAt` host op and advance time past the deadline.
    fn complete_wake_at(sched: &Scheduler, mock_time: &Rc<Cell<u64>>) {
        let req = sched.take_host_op().expect("expected WakeAt");
        if let HostOpKind::WakeAt { deadline } = req.kind {
            // Advance mock time past the deadline
            mock_time.set(deadline);
            // WakeAt completion includes current time as 8 bytes LE u64
            sched.complete_host_op(req.id, deadline.to_le_bytes().to_vec());
        } else {
            panic!("expected WakeAt, got {:?}", req.kind);
        }
    }

    fn make_ctx(argv: Vec<&str>, scheduler: Scheduler) -> CmdContext {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        CmdContext::new(
            argv.into_iter()
                .map(std::string::ToString::to_string)
                .collect(),
            IoHandle::null(),
            IoHandle::buffer(),
            IoHandle::buffer(),
            "/workspace".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        )
    }

    #[test]
    fn sleep_missing_operand() {
        let scheduler = test_scheduler();
        let ctx = make_ctx(vec!["sleep"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn sleep_invalid_duration() {
        let scheduler = test_scheduler();
        let ctx = make_ctx(vec!["sleep", "abc"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn sleep_negative_duration() {
        let scheduler = test_scheduler();
        let ctx = make_ctx(vec!["sleep", "-5"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn sleep_zero() {
        let (scheduler, mock_time) = test_scheduler_with_time();
        let ctx = make_ctx(vec!["sleep", "0"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Run - should block on WakeAt (now() is sync, no Now host op)
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Complete the WakeAt request (advances time to deadline)
        complete_wake_at(&scheduler, &mock_time);

        // Run to completion
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn sleep_one_second() {
        let (scheduler, mock_time) = test_scheduler_with_time();
        let ctx = make_ctx(vec!["sleep", "1"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Run - should block on WakeAt (now() is sync)
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));
        assert!(!handle.is_complete());

        // Complete the WakeAt (advances time to deadline)
        complete_wake_at(&scheduler, &mock_time);

        // Run to completion
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(handle.is_complete());

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn sleep_fractional_seconds() {
        let (scheduler, mock_time) = test_scheduler_with_time();
        let ctx = make_ctx(vec!["sleep", "0.5"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Run - should block on WakeAt
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // Complete WakeAt (advances time to deadline)
        complete_wake_at(&scheduler, &mock_time);

        scheduler.run();
        assert!(handle.is_complete());

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn sleep_with_suffix() {
        // Test 1 minute
        let (scheduler, mock_time) = test_scheduler_with_time();
        let ctx = make_ctx(vec!["sleep", "1m"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        scheduler.run();

        // Complete WakeAt (advances time to deadline)
        complete_wake_at(&scheduler, &mock_time);
        scheduler.run();

        assert!(handle.is_complete());
        assert_eq!(handle.try_get().unwrap().unwrap().code, 0);
    }

    #[test]
    fn sleep_multiple_durations() {
        // sleep 1 2 should sleep for 3 seconds total
        let (scheduler, mock_time) = test_scheduler_with_time();
        let ctx = make_ctx(vec!["sleep", "1", "2"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        scheduler.run();

        // Complete WakeAt (advances time to deadline)
        complete_wake_at(&scheduler, &mock_time);
        scheduler.run();

        assert!(handle.is_complete());
        assert_eq!(handle.try_get().unwrap().unwrap().code, 0);
    }

    #[test]
    fn sleep_mixed_units() {
        // sleep 1h 30m should sleep for 5400 seconds
        let (scheduler, mock_time) = test_scheduler_with_time();
        let ctx = make_ctx(vec!["sleep", "1h", "30m"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        scheduler.run();

        // Complete WakeAt (advances time to deadline)
        complete_wake_at(&scheduler, &mock_time);
        scheduler.run();

        assert!(handle.is_complete());
        assert_eq!(handle.try_get().unwrap().unwrap().code, 0);
    }

    #[test]
    fn sleep_partial_invalid() {
        // If any duration is invalid, should fail
        let scheduler = test_scheduler();
        let ctx = make_ctx(vec!["sleep", "1", "abc", "2"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        assert_eq!(handle.try_get().unwrap().unwrap().code, 1);
    }

    // =========================================================================
    // Host ops state machine tests
    // =========================================================================

    #[test]
    fn sleep_host_ops_sequence() {
        // Verify the sequence: now() is sync, only WakeAt is a host op
        let (scheduler, mock_time) = test_scheduler_with_time();
        let ctx = make_ctx(vec!["sleep", "1"], scheduler.clone());

        let _handle = scheduler.spawn(run(ctx));

        // Initially no host ops pending
        assert!(!scheduler.has_pending_host_ops());

        // After first run, should have WakeAt pending (no Now - it's sync)
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));
        assert!(scheduler.has_pending_host_ops());

        // Complete WakeAt (advances time to deadline)
        complete_wake_at(&scheduler, &mock_time);
        assert!(!scheduler.has_pending_host_ops());

        // Should complete now
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
    }

    #[test]
    fn sleep_no_host_ops_on_error() {
        // Invalid args should fail without any host ops
        let scheduler = test_scheduler();
        let ctx = make_ctx(vec!["sleep", "invalid"], scheduler.clone());

        let _handle = scheduler.spawn(run(ctx));

        // Run should complete immediately without blocking
        let state = scheduler.run();
        assert!(
            matches!(state, SchedulerState::Done),
            "should not block on invalid args"
        );
        assert!(!scheduler.has_pending_host_ops());
    }

    #[test]
    fn sleep_multiple_concurrent() {
        // Test that multiple sleeps share the scheduler correctly
        let (scheduler, mock_time) = test_scheduler_with_time();

        let ctx1 = make_ctx(vec!["sleep", "1"], scheduler.clone());
        let ctx2 = make_ctx(vec!["sleep", "2"], scheduler.clone());

        let handle1 = scheduler.spawn(run(ctx1));
        let handle2 = scheduler.spawn(run(ctx2));

        // Run - both should block on WakeAt (now() is sync)
        scheduler.run();

        // The scheduler should coalesce to a single WakeAt for the earliest deadline
        // Complete first WakeAt (advances time to first deadline)
        complete_wake_at(&scheduler, &mock_time);
        scheduler.run();

        // First task should complete
        assert!(handle1.is_complete());
        assert!(!handle2.is_complete());

        // Continue for second task - advance time to second deadline
        while scheduler.has_pending_host_ops() {
            complete_wake_at(&scheduler, &mock_time);
            scheduler.run();
        }

        scheduler.run();
        assert!(handle2.is_complete());
    }

    #[test]
    fn sleep_state_transitions() {
        let (scheduler, mock_time) = test_scheduler_with_time();
        let ctx = make_ctx(vec!["sleep", "1"], scheduler.clone());

        let handle = scheduler.spawn(run(ctx));

        // Initial state: task spawned but not run
        assert!(!handle.is_complete());

        // State 1: Running -> Blocked (waiting for WakeAt, now() is sync)
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Blocked));

        // State 2: Complete WakeAt -> Done
        complete_wake_at(&scheduler, &mock_time);
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));
        assert!(handle.is_complete());
    }
}
