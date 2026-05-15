//! sh - shell command interpreter
//!
//! Executes shell commands from a string or script file.
//!
//! This is an applet that allows proper subshell support. When a
//! `(command)` subshell is executed, it runs through this applet,
//! ensuring correct isolation of environment changes.

use amla_scheduler::Exit;

use crate::{CmdContext, Shell};

use super::CommandResult;

/// sh [-c command] [script] [args...]
///
/// Options:
///   -c string    Execute commands from string
///
/// If neither -c nor script is provided, reads from stdin.
pub async fn run(ctx: CmdContext) -> CommandResult {
    let args: Vec<&str> = ctx
        .argv
        .iter()
        .skip(1)
        .map(std::string::String::as_str)
        .collect();

    // Parse options and positional parameters
    // sh -c 'script' [arg0 arg1 ...] sets $0, $1, $2, etc.
    let (command_string, script_file, positional_args) = if args.is_empty() {
        (None, None, Vec::new())
    } else if args[0] == "-c" {
        if args.len() < 2 {
            ctx.eprintln("sh: -c requires an argument").await?;
            return Ok(Exit::code(2));
        }
        // Extra args become $0, $1, $2, ...
        let positional = args.iter().skip(2).copied().collect();
        (Some(args[1]), None, positional)
    } else if args[0].starts_with('-') {
        ctx.eprintln(&format!("sh: unknown option: {}", args[0]))
            .await?;
        return Ok(Exit::code(2));
    } else {
        // Script file mode: args after script become positional
        let positional = args.iter().skip(1).copied().collect();
        (None, Some(args[0]), positional)
    };

    // Determine what to execute
    let script = if let Some(cmd) = command_string {
        cmd.to_string()
    } else if let Some(file) = script_file {
        // Read script from file (may be a mounted host file)
        match ctx.read_file(file).await {
            Ok(content) => {
                if let Ok(s) = String::from_utf8(content) {
                    s
                } else {
                    ctx.eprintln(&format!("sh: {file}: invalid UTF-8")).await?;
                    return Ok(Exit::code(1));
                }
            }
            Err(e) => {
                ctx.eprintln(&format!("sh: {file}: {e}")).await?;
                return Ok(Exit::code(1));
            }
        }
    } else {
        // Read from stdin
        let mut script = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = ctx.stdin.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            script.extend_from_slice(&buf[..n]);
        }
        if let Ok(s) = String::from_utf8(script) {
            s
        } else {
            ctx.eprintln("sh: stdin: invalid UTF-8").await?;
            return Ok(Exit::code(1));
        }
    };

    // Create a shell that inherits the caller's context
    // (Changes in the child don't affect the parent since cwd/env are cloned)
    // Uses parent's scheduler - caller must drive the scheduler.
    let vfs = ctx.vfs();
    let cwd = ctx.cwd().to_string();
    let mut env = ctx.env_clone();

    // Set positional parameters: $0, $1, $2, ..., $#, $@, $*
    if positional_args.is_empty() {
        env.set("0", "sh");
        env.set("#", "0");
        env.set("@", "");
        env.set("*", "");
    } else {
        env.set("0", positional_args[0]);
        for (i, arg) in positional_args.iter().skip(1).enumerate() {
            env.set((i + 1).to_string(), *arg);
        }
        // $# = number of positional parameters (excluding $0)
        env.set("#", (positional_args.len() - 1).to_string());
        // $@ and $* = all positional parameters (space-separated)
        let all_args = positional_args[1..].join(" ");
        env.set("@", &all_args);
        env.set("*", &all_args);
    }

    let stdin = ctx.stdin.clone();
    let stdout = ctx.stdout.clone();
    let stderr = ctx.stderr.clone();
    let shell = Shell::with_full_context(ctx.scheduler(), vfs, cwd, env, stdin, stdout, stderr);

    // Execute the script - parent scheduler drives execution
    let mut last_exit = 0;
    for line in script.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match Box::pin(shell.execute(line)).await {
            Ok(code) => last_exit = code,
            Err(e) => {
                ctx.eprintln(&format!("sh: {e}")).await?;
                return Ok(Exit::code(1));
            }
        }
    }

    Ok(Exit::code(last_exit))
}

#[cfg(test)]
mod tests {
    use crate::{Shell, error::Result};
    use amla_scheduler::{RandomSourceFn, Scheduler, TimeSourceFn};
    use amla_vfs::Permission;
    use std::cell::Cell;
    use std::rc::Rc;

    fn test_scheduler() -> Scheduler {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    fn test_shell() -> Shell {
        Shell::new(test_scheduler())
    }

    /// Test helper: Run a command and assert it completes in exactly expected_steps.
    fn run(shell: &Shell, cmd: &str, expected_steps: usize) -> Result<i32> {
        use std::task::{Context, Poll};

        let scheduler = shell.scheduler().clone();
        let mut fut = std::pin::pin!(shell.execute(cmd));

        let waker = amla_scheduler::noop_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll (step 0) - before any scheduler steps
        if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
            assert_eq!(0, expected_steps, "STEPS|{cmd}|0|expected {expected_steps}");
            return result;
        }

        for step in 1..=10000 {
            let _ = scheduler.run_step();
            if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
                assert_eq!(
                    step, expected_steps,
                    "STEPS|{cmd}|{step}|expected {expected_steps}"
                );
                return result;
            }
        }
        panic!("command '{cmd}' did not complete within 10000 steps");
    }

    #[test]
    fn sh_c_true() {
        let shell = test_shell();
        assert_eq!(run(&shell, "sh -c true", 3).unwrap(), 0);
    }

    #[test]
    fn sh_c_false() {
        let shell = test_shell();
        assert_eq!(run(&shell, "sh -c false", 3).unwrap(), 1);
    }

    #[test]
    fn sh_c_exit() {
        let shell = test_shell();
        assert_eq!(run(&shell, "sh -c 'exit 42'", 3).unwrap(), 42);
    }

    #[test]
    fn sh_c_missing_arg() {
        let shell = test_shell();
        // Error on first poll, no scheduler steps needed
        assert_eq!(run(&shell, "sh -c", 1).unwrap(), 2);
    }

    #[test]
    fn sh_script_file() {
        let shell = test_shell();
        shell
            .vfs_mut()
            .write_file(
                "/workspace/test.sh",
                b"true\ntrue\nexit 0",
                Permission::ReadWrite,
            )
            .unwrap();
        // Script with 3 lines: true, true, exit 0 = 7 steps
        assert_eq!(run(&shell, "sh /workspace/test.sh", 7).unwrap(), 0);
    }

    #[test]
    fn sh_inherits_environment() {
        let shell = test_shell();
        shell.env_mut().set("MY_VAR", "hello_world");
        // sh inherits the environment
        assert_eq!(run(&shell, "sh -c true", 3).unwrap(), 0);
    }

    #[test]
    fn sh_inherits_cwd() {
        let shell = test_shell();
        shell
            .vfs_mut()
            .create_dir_all("/workspace/subdir", Permission::ReadWrite)
            .unwrap();
        run(&shell, "cd /workspace/subdir", 1).unwrap();
        assert_eq!(run(&shell, "sh -c true", 3).unwrap(), 0);
    }

    #[test]
    fn sh_unknown_option() {
        let shell = test_shell();
        // Error on first step
        assert_eq!(run(&shell, "sh -x true", 1).unwrap(), 2);
    }

    #[test]
    fn sh_script_with_comments() {
        let shell = test_shell();
        shell
            .vfs_mut()
            .write_file(
                "/workspace/test.sh",
                b"# This is a comment\ntrue\n# Another comment\nexit 5",
                Permission::ReadWrite,
            )
            .unwrap();
        // Script with 2 actual commands: true, exit 5 = 5 steps
        assert_eq!(run(&shell, "sh /workspace/test.sh", 5).unwrap(), 5);
    }

    #[test]
    fn sh_multiline_script() {
        let shell = test_shell();
        shell
            .vfs_mut()
            .write_file(
                "/workspace/multi.sh",
                b"true\nfalse\ntrue",
                Permission::ReadWrite,
            )
            .unwrap();
        // Last command is 'true' so exit code should be 0, 3 commands = 7 steps
        assert_eq!(run(&shell, "sh /workspace/multi.sh", 7).unwrap(), 0);
    }

    #[test]
    fn sh_c_positional_args_dollar_1() {
        let shell = test_shell();
        // sh -c 'echo $1' foo bar -> $0=foo, $1=bar
        shell
            .vfs_mut()
            .write_file("/workspace/out.txt", b"", Permission::ReadWrite)
            .unwrap();
        run(&shell, "sh -c 'echo $1 > /workspace/out.txt' foo bar", 3).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "bar");
    }

    #[test]
    fn sh_c_positional_args_dollar_0() {
        let shell = test_shell();
        // sh -c 'echo $0' myscript -> $0=myscript
        shell
            .vfs_mut()
            .write_file("/workspace/out.txt", b"", Permission::ReadWrite)
            .unwrap();
        run(&shell, "sh -c 'echo $0 > /workspace/out.txt' myscript", 3).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "myscript");
    }

    #[test]
    fn sh_c_positional_args_dollar_hash() {
        let shell = test_shell();
        // sh -c 'echo $#' s0 a1 a2 a3 -> $0=s0, $1=a1, $2=a2, $3=a3, $#=3
        shell
            .vfs_mut()
            .write_file("/workspace/out.txt", b"", Permission::ReadWrite)
            .unwrap();
        run(
            &shell,
            "sh -c 'echo $# > /workspace/out.txt' s0 a1 a2 a3",
            3,
        )
        .unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "3");
    }

    #[test]
    fn sh_c_positional_args_dollar_at() {
        let shell = test_shell();
        // sh -c 'echo $@' s0 a1 a2 -> $@="a1 a2"
        shell
            .vfs_mut()
            .write_file("/workspace/out.txt", b"", Permission::ReadWrite)
            .unwrap();
        run(&shell, "sh -c 'echo $@ > /workspace/out.txt' s0 a1 a2", 3).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "a1 a2");
    }

    #[test]
    fn sh_c_no_positional_args() {
        let shell = test_shell();
        // sh -c 'echo $0' with no args -> $0=sh (default)
        shell
            .vfs_mut()
            .write_file("/workspace/out.txt", b"", Permission::ReadWrite)
            .unwrap();
        run(&shell, "sh -c 'echo $0 > /workspace/out.txt'", 3).unwrap();
        let content = shell.vfs.borrow().read_file("/workspace/out.txt").unwrap();
        assert_eq!(String::from_utf8_lossy(&content).trim(), "sh");
    }
}
