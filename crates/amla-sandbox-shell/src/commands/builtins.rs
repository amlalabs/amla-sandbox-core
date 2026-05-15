//! Shell builtins.
//!
//! These commands can affect shell state via `SideEffects`.

use amla_scheduler::{Exit, SideEffects, SmallVec};

use crate::CmdContext;

use super::CommandResult;

/// cd [dir] - change directory
pub async fn cd(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    let target = if args.is_empty() {
        // Default to HOME
        ctx.getenv("HOME").unwrap_or("/").to_string()
    } else {
        args[0].clone()
    };

    // Resolve and validate path
    let resolved = ctx.resolve_path(&target);

    if !ctx.is_dir(&resolved) {
        ctx.eprintln(&format!("cd: {target}: No such directory"))
            .await?;
        return Ok(Exit::code(1));
    }

    // Return side effect to change cwd
    Ok(Exit {
        code: 0,
        effects: SideEffects {
            cwd: Some(resolved),
            ..Default::default()
        },
    })
}

/// pwd - print working directory
pub async fn pwd(ctx: CmdContext) -> CommandResult {
    ctx.println(ctx.cwd()).await?;
    Ok(Exit::success())
}

/// export [name=value...] - set environment variables
pub async fn export(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    let mut env_set: SmallVec<[(String, String); 2]> = SmallVec::new();

    for arg in args {
        if let Some((name, value)) = arg.split_once('=') {
            env_set.push((name.to_string(), value.to_string()));
        }
        // Just "export VAR" (without =value) marks for export (no-op here)
    }

    Ok(Exit {
        code: 0,
        effects: SideEffects {
            env_set,
            ..Default::default()
        },
    })
}

/// unset name... - unset environment variables
pub async fn unset(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    let env_unset: SmallVec<[String; 2]> = args.iter().cloned().collect();

    Ok(Exit {
        code: 0,
        effects: SideEffects {
            env_unset,
            ..Default::default()
        },
    })
}

/// exit [code] - exit the shell
pub async fn exit(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    let code = if args.is_empty() {
        0
    } else {
        args[0].parse().unwrap_or(0)
    };

    Ok(Exit::code(code))
}

/// true - return success
pub async fn cmd_true(_ctx: CmdContext) -> CommandResult {
    Ok(Exit::success())
}

/// false - return failure
pub async fn cmd_false(_ctx: CmdContext) -> CommandResult {
    Ok(Exit::code(1))
}

/// : - no-op (colon command)
pub async fn colon(_ctx: CmdContext) -> CommandResult {
    Ok(Exit::success())
}

/// env - print environment variables
pub async fn env(ctx: CmdContext) -> CommandResult {
    for (key, value) in ctx.env_iter() {
        ctx.println(&format!("{key}={value}")).await?;
    }
    Ok(Exit::success())
}

/// set - set shell options
///
/// Options:
///   -o pipefail    Pipeline returns first non-zero exit code
///   +o pipefail    Pipeline returns last exit code (default)
///
/// Without arguments: print environment variables.
pub async fn set(ctx: CmdContext) -> CommandResult {
    let args = ctx.args();

    // No arguments: print environment
    if args.is_empty() {
        for (key, value) in ctx.env_iter() {
            ctx.println(&format!("{key}={value}")).await?;
        }
        return Ok(Exit::success());
    }

    // Parse -o and +o options
    let mut i = 0;
    let mut pipefail: Option<bool> = None;

    while i < args.len() {
        let arg = &args[i];

        if arg == "-o" {
            // Enable option
            if i + 1 >= args.len() {
                ctx.eprintln("set: -o requires an option name").await?;
                return Ok(Exit::code(2));
            }
            let option = &args[i + 1];
            if option == "pipefail" {
                pipefail = Some(true);
            } else {
                ctx.eprintln(&format!("set: unknown option: {option}"))
                    .await?;
                return Ok(Exit::code(1));
            }
            i += 2;
        } else if arg == "+o" {
            // Disable option
            if i + 1 >= args.len() {
                ctx.eprintln("set: +o requires an option name").await?;
                return Ok(Exit::code(2));
            }
            let option = &args[i + 1];
            if option == "pipefail" {
                pipefail = Some(false);
            } else {
                ctx.eprintln(&format!("set: unknown option: {option}"))
                    .await?;
                return Ok(Exit::code(1));
            }
            i += 2;
        } else {
            // Unknown argument
            ctx.eprintln(&format!("set: unknown argument: {arg}"))
                .await?;
            return Ok(Exit::code(1));
        }
    }

    Ok(Exit {
        code: 0,
        effects: SideEffects {
            pipefail,
            ..Default::default()
        },
    })
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

    fn test_scheduler() -> Scheduler {
        let mock_time = Rc::new(Cell::new(0u64));
        let time_clone = mock_time.clone();
        let time_source: TimeSourceFn = Rc::new(move |_runtime_id, _clock| time_clone.get());
        let random_source: RandomSourceFn = Rc::new(|_runtime_id| 42);
        Scheduler::new(1, time_source, random_source)
    }

    fn make_ctx(argv: Vec<&str>, cwd: &str, env: Environment) -> CmdContext {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let scheduler = test_scheduler();
        // Create subdirectories under /workspace (the writable area)
        {
            let mut vfs_mut = vfs.borrow_mut();
            vfs_mut
                .create_dir_all("/workspace/home/user", Permission::ReadWrite)
                .unwrap();
            vfs_mut
                .create_dir_all("/workspace/subdir", Permission::ReadWrite)
                .unwrap();
        }
        CmdContext::new(
            argv.into_iter()
                .map(std::string::ToString::to_string)
                .collect(),
            IoHandle::null(),
            IoHandle::buffer(),
            IoHandle::buffer(),
            cwd.to_string(),
            env,
            vfs,
            scheduler,
            None,
        )
    }

    #[test]
    fn cd_to_home() {
        let mut env = Environment::new();
        env.set("HOME", "/workspace/home/user");
        let ctx = make_ctx(vec!["cd"], "/workspace", env);

        let result = pollster::block_on(cd(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.cwd, Some("/workspace/home/user".to_string()));
    }

    #[test]
    fn cd_to_dir() {
        let ctx = make_ctx(
            vec!["cd", "/workspace/subdir"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(cd(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.cwd, Some("/workspace/subdir".to_string()));
    }

    #[test]
    fn cd_not_exists() {
        let ctx = make_ctx(vec!["cd", "/nonexistent"], "/workspace", Environment::new());

        let result = pollster::block_on(cd(ctx)).unwrap();
        assert_eq!(result.code, 1);
        assert_eq!(result.effects.cwd, None);
    }

    #[test]
    fn pwd_prints_cwd() {
        let ctx = make_ctx(vec!["pwd"], "/workspace", Environment::new());

        let result = pollster::block_on(pwd(ctx)).unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn export_sets_env() {
        let ctx = make_ctx(
            vec!["export", "FOO=bar", "BAZ=qux"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(export(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.env_set.len(), 2);
        assert_eq!(
            result.effects.env_set[0],
            ("FOO".to_string(), "bar".to_string())
        );
        assert_eq!(
            result.effects.env_set[1],
            ("BAZ".to_string(), "qux".to_string())
        );
    }

    #[test]
    fn unset_removes_env() {
        let ctx = make_ctx(
            vec!["unset", "FOO", "BAR"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(unset(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.env_unset.len(), 2);
        assert!(result.effects.env_unset.contains(&"FOO".to_string()));
        assert!(result.effects.env_unset.contains(&"BAR".to_string()));
    }

    #[test]
    fn exit_default() {
        let ctx = make_ctx(vec!["exit"], "/workspace", Environment::new());

        let result = pollster::block_on(exit(ctx)).unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn exit_with_code() {
        let ctx = make_ctx(vec!["exit", "42"], "/workspace", Environment::new());

        let result = pollster::block_on(exit(ctx)).unwrap();
        assert_eq!(result.code, 42);
    }

    // =========================================================================
    // cd - additional edge cases
    // =========================================================================

    #[test]
    fn cd_no_home_defaults_to_root() {
        // When HOME is not set, cd should default to "/"
        let ctx = make_ctx(vec!["cd"], "/workspace", Environment::new());

        let result = pollster::block_on(cd(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.cwd, Some("/".to_string()));
    }

    #[test]
    fn cd_relative_path() {
        let ctx = make_ctx(vec!["cd", "subdir"], "/workspace", Environment::new());

        let result = pollster::block_on(cd(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.cwd, Some("/workspace/subdir".to_string()));
    }

    #[test]
    fn cd_to_file_fails() {
        // Create a file (not a directory)
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let scheduler = test_scheduler();
        {
            let mut vfs_mut = vfs.borrow_mut();
            vfs_mut
                .create_dir_all("/workspace", amla_vfs::Permission::ReadWrite)
                .unwrap();
            vfs_mut
                .write_file(
                    "/workspace/file.txt",
                    b"content",
                    amla_vfs::Permission::ReadWrite,
                )
                .unwrap();
        }

        let ctx = CmdContext::new(
            vec!["cd".to_string(), "/workspace/file.txt".to_string()],
            IoHandle::null(),
            IoHandle::buffer(),
            IoHandle::buffer(),
            "/workspace".to_string(),
            Environment::new(),
            vfs,
            scheduler,
            None,
        );

        let result = pollster::block_on(cd(ctx)).unwrap();
        assert_eq!(result.code, 1);
        assert_eq!(result.effects.cwd, None);
    }

    // =========================================================================
    // export - additional edge cases
    // =========================================================================

    #[test]
    fn export_no_args() {
        let ctx = make_ctx(vec!["export"], "/workspace", Environment::new());

        let result = pollster::block_on(export(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert!(result.effects.env_set.is_empty());
    }

    #[test]
    fn export_var_without_value() {
        // "export VAR" without = just marks for export (no-op here)
        let ctx = make_ctx(vec!["export", "VAR_ONLY"], "/workspace", Environment::new());

        let result = pollster::block_on(export(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert!(result.effects.env_set.is_empty());
    }

    #[test]
    fn export_empty_value() {
        let ctx = make_ctx(vec!["export", "EMPTY="], "/workspace", Environment::new());

        let result = pollster::block_on(export(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.env_set.len(), 1);
        assert_eq!(
            result.effects.env_set[0],
            ("EMPTY".to_string(), String::new())
        );
    }

    #[test]
    fn export_value_with_equals() {
        // Value containing = should work
        let ctx = make_ctx(
            vec!["export", "KEY=value=with=equals"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(export(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.env_set.len(), 1);
        assert_eq!(
            result.effects.env_set[0],
            ("KEY".to_string(), "value=with=equals".to_string())
        );
    }

    // =========================================================================
    // unset - additional edge cases
    // =========================================================================

    #[test]
    fn unset_no_args() {
        let ctx = make_ctx(vec!["unset"], "/workspace", Environment::new());

        let result = pollster::block_on(unset(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert!(result.effects.env_unset.is_empty());
    }

    #[test]
    fn unset_single_var() {
        let ctx = make_ctx(vec!["unset", "SINGLE"], "/workspace", Environment::new());

        let result = pollster::block_on(unset(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.env_unset.len(), 1);
        assert_eq!(result.effects.env_unset[0], "SINGLE".to_string());
    }

    // =========================================================================
    // exit - additional edge cases
    // =========================================================================

    #[test]
    fn exit_invalid_code() {
        // Invalid code should default to 0
        let ctx = make_ctx(vec!["exit", "notanumber"], "/workspace", Environment::new());

        let result = pollster::block_on(exit(ctx)).unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn exit_negative_code() {
        let ctx = make_ctx(vec!["exit", "-1"], "/workspace", Environment::new());

        let result = pollster::block_on(exit(ctx)).unwrap();
        assert_eq!(result.code, -1);
    }

    #[test]
    fn exit_large_code() {
        let ctx = make_ctx(vec!["exit", "255"], "/workspace", Environment::new());

        let result = pollster::block_on(exit(ctx)).unwrap();
        assert_eq!(result.code, 255);
    }

    // =========================================================================
    // cmd_true - tests
    // =========================================================================

    #[test]
    fn true_returns_success() {
        let ctx = make_ctx(vec!["true"], "/workspace", Environment::new());

        let result = pollster::block_on(cmd_true(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert!(result.effects.cwd.is_none());
        assert!(result.effects.env_set.is_empty());
        assert!(result.effects.env_unset.is_empty());
    }

    #[test]
    fn true_ignores_args() {
        // `true` should ignore any arguments
        let ctx = make_ctx(
            vec!["true", "arg1", "arg2"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(cmd_true(ctx)).unwrap();
        assert_eq!(result.code, 0);
    }

    // =========================================================================
    // cmd_false - tests
    // =========================================================================

    #[test]
    fn false_returns_failure() {
        let ctx = make_ctx(vec!["false"], "/workspace", Environment::new());

        let result = pollster::block_on(cmd_false(ctx)).unwrap();
        assert_eq!(result.code, 1);
        assert!(result.effects.cwd.is_none());
        assert!(result.effects.env_set.is_empty());
        assert!(result.effects.env_unset.is_empty());
    }

    #[test]
    fn false_ignores_args() {
        // `false` should ignore any arguments
        let ctx = make_ctx(
            vec!["false", "arg1", "arg2"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(cmd_false(ctx)).unwrap();
        assert_eq!(result.code, 1);
    }

    // =========================================================================
    // colon - tests
    // =========================================================================

    #[test]
    fn colon_returns_success() {
        let ctx = make_ctx(vec![":"], "/workspace", Environment::new());

        let result = pollster::block_on(colon(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert!(result.effects.cwd.is_none());
        assert!(result.effects.env_set.is_empty());
        assert!(result.effects.env_unset.is_empty());
    }

    #[test]
    fn colon_ignores_args() {
        // `:` should ignore any arguments
        let ctx = make_ctx(vec![":", "arg1", "arg2"], "/workspace", Environment::new());

        let result = pollster::block_on(colon(ctx)).unwrap();
        assert_eq!(result.code, 0);
    }

    // =========================================================================
    // env - tests
    // =========================================================================

    #[test]
    fn env_empty_environment() {
        let ctx = make_ctx(vec!["env"], "/workspace", Environment::new());

        let result = pollster::block_on(env(ctx)).unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn env_with_variables() {
        let mut environment = Environment::new();
        environment.set("FOO", "bar");
        environment.set("BAZ", "qux");

        let ctx = make_ctx(vec!["env"], "/workspace", environment);

        let result = pollster::block_on(env(ctx)).unwrap();
        assert_eq!(result.code, 0);
    }

    // =========================================================================
    // set - tests
    // =========================================================================

    #[test]
    fn set_no_args_prints_env() {
        let mut environment = Environment::new();
        environment.set("VAR", "value");

        let ctx = make_ctx(vec!["set"], "/workspace", environment);

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert!(result.effects.pipefail.is_none());
    }

    #[test]
    fn set_enable_pipefail() {
        let ctx = make_ctx(
            vec!["set", "-o", "pipefail"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.pipefail, Some(true));
    }

    #[test]
    fn set_disable_pipefail() {
        let ctx = make_ctx(
            vec!["set", "+o", "pipefail"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(result.effects.pipefail, Some(false));
    }

    #[test]
    fn set_o_missing_option_name() {
        let ctx = make_ctx(vec!["set", "-o"], "/workspace", Environment::new());

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 2);
    }

    #[test]
    fn set_plus_o_missing_option_name() {
        let ctx = make_ctx(vec!["set", "+o"], "/workspace", Environment::new());

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 2);
    }

    #[test]
    fn set_unknown_option() {
        let ctx = make_ctx(
            vec!["set", "-o", "unknownoption"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn set_unknown_option_plus_o() {
        let ctx = make_ctx(
            vec!["set", "+o", "unknownoption"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn set_unknown_argument() {
        let ctx = make_ctx(vec!["set", "--unknown"], "/workspace", Environment::new());

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 1);
    }

    #[test]
    fn set_multiple_options() {
        // Enable then disable pipefail (last one wins)
        let ctx = make_ctx(
            vec!["set", "-o", "pipefail", "+o", "pipefail"],
            "/workspace",
            Environment::new(),
        );

        let result = pollster::block_on(set(ctx)).unwrap();
        assert_eq!(result.code, 0);
        // Last option processed was +o pipefail, so pipefail should be Some(false)
        assert_eq!(result.effects.pipefail, Some(false));
    }
}
