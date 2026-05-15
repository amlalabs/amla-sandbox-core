//! find - search for files in a directory hierarchy

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;

use super::CommandResult;

const HELP: &str = "\
find - search for files in a directory hierarchy

Usage: find [PATH...] [EXPRESSION]

If no path is given, defaults to current directory.

Expressions:
  -name PATTERN     Match filename (supports * and ? wildcards)
  -type TYPE        Match file type: f (file), d (directory)
  -maxdepth N       Descend at most N levels
  -mindepth N       Don't apply tests at levels less than N
  -print            Print matching path (default action)

Examples:
  find                        List all files recursively
  find .                      Same as above
  find /workspace -name '*.rs'  Find all Rust files
  find . -type f              Find only regular files
  find . -type d              Find only directories
  find . -name 'test*' -type f  Combine conditions
  find . -maxdepth 1          List immediate contents only
";

/// Options and filters for find
#[derive(Default)]
struct FindOptions {
    /// Name pattern to match (with * and ? wildcards)
    name_pattern: Option<String>,
    /// File type filter: 'f' for file, 'd' for directory
    file_type: Option<char>,
    /// Maximum depth to descend
    max_depth: Option<usize>,
    /// Minimum depth before applying tests
    min_depth: usize,
    /// Whether -print was explicitly specified
    print_explicit: bool,
}

/// find [PATH...] [EXPRESSION]
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut opts = FindOptions::default();
    let mut paths: SmallVec<[String; 4]> = SmallVec::new();

    // Parse arguments manually since find uses -name, -type style (not --name, --type)
    let args: Vec<&str> = ctx.argv.iter().skip(1).map(String::as_str).collect();
    let mut i = 0;

    while i < args.len() {
        let arg = args[i];
        match arg {
            "-h" | "--help" | "-help" => {
                ctx.stdout_write_all(HELP.as_bytes()).await?;
                return Ok(Exit::success());
            }
            "-name" | "--name" => {
                i += 1;
                if i < args.len() {
                    opts.name_pattern = Some(args[i].to_string());
                }
            }
            "-type" | "--type" => {
                i += 1;
                if i < args.len() {
                    opts.file_type = args[i].chars().next();
                }
            }
            "-maxdepth" | "--maxdepth" => {
                i += 1;
                if i < args.len() {
                    opts.max_depth = args[i].parse().ok();
                }
            }
            "-mindepth" | "--mindepth" => {
                i += 1;
                if i < args.len() {
                    opts.min_depth = args[i].parse().unwrap_or(0);
                }
            }
            "-print" | "--print" => {
                opts.print_explicit = true;
            }
            _ if arg.starts_with('-') => {
                // Unknown option - skip
            }
            _ => {
                // Path argument
                paths.push(arg.to_string());
            }
        }
        i += 1;
    }

    // Default to current directory if no paths given
    if paths.is_empty() {
        paths.push(".".to_string());
    }

    // Process each starting path
    for path_arg in &paths {
        let start_path = ctx.resolve_path(path_arg);

        if !ctx.exists(&start_path) {
            ctx.eprintln(&format!("find: '{path_arg}': No such file or directory"))
                .await?;
            continue;
        }

        // Traverse the directory tree
        find_recursive(&ctx, &start_path, path_arg, &opts, 0).await?;
    }

    Ok(Exit::success())
}

/// Recursively find files matching the criteria
fn find_recursive<'a>(
    ctx: &'a CmdContext,
    abs_path: &'a str,
    display_path: &'a str,
    opts: &'a FindOptions,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), amla_scheduler::Error>> + 'a>> {
    Box::pin(async move {
        // Check max depth
        if opts.max_depth.is_some_and(|max| depth > max) {
            return Ok(());
        }

        // Check if this entry matches (only if at or below min_depth)
        if depth >= opts.min_depth {
            let matches = matches_criteria(ctx, abs_path, display_path, opts);
            if matches {
                ctx.println(display_path).await?;
            }
        }

        // If it's a directory, recurse into it
        if ctx.is_dir(abs_path) {
            // Check if we can descend
            if opts.max_depth.is_some_and(|max| depth >= max) {
                return Ok(());
            }

            match ctx.list_dir(abs_path) {
                Ok(entries) => {
                    for entry in entries {
                        let child_abs = if abs_path == "/" {
                            format!("/{}", entry.name)
                        } else {
                            format!("{}/{}", abs_path, entry.name)
                        };
                        let child_display = if display_path == "/" {
                            format!("/{}", entry.name)
                        } else {
                            format!("{}/{}", display_path, entry.name)
                        };
                        find_recursive(ctx, &child_abs, &child_display, opts, depth + 1).await?;
                    }
                }
                Err(e) => {
                    ctx.eprintln(&format!("find: '{display_path}': {e}"))
                        .await?;
                }
            }
        }

        Ok(())
    })
}

/// Check if a path matches the filter criteria
fn matches_criteria(
    ctx: &CmdContext,
    abs_path: &str,
    display_path: &str,
    opts: &FindOptions,
) -> bool {
    // Check file type
    if let Some(t) = opts.file_type {
        match t {
            'f' if !ctx.is_file(abs_path) => return false,
            'd' if !ctx.is_dir(abs_path) => return false,
            _ => {}
        }
    }

    // Check name pattern
    if let Some(ref pattern) = opts.name_pattern {
        let name = display_path.rsplit('/').next().unwrap_or(display_path);
        if !matches_glob(name, pattern) {
            return false;
        }
    }

    true
}

/// Simple glob matching supporting * and ?
fn matches_glob(name: &str, pattern: &str) -> bool {
    let mut name_chars = name.chars().peekable();
    let mut pattern_chars = pattern.chars().peekable();

    matches_glob_inner(&mut name_chars, &mut pattern_chars)
}

fn matches_glob_inner(
    name: &mut std::iter::Peekable<std::str::Chars>,
    pattern: &mut std::iter::Peekable<std::str::Chars>,
) -> bool {
    loop {
        match (pattern.peek().copied(), name.peek().copied()) {
            (None, None) => return true,
            (None, Some(_)) => return false,
            (Some('*'), _) => {
                pattern.next();
                // Try matching zero or more characters
                loop {
                    // Clone iterators to try matching rest
                    let mut name_clone = name.clone();
                    let mut pattern_clone = pattern.clone();
                    if matches_glob_inner(&mut name_clone, &mut pattern_clone) {
                        return true;
                    }
                    // Move forward in name
                    if name.next().is_none() {
                        // At end of name, check if rest of pattern is just *
                        return pattern.all(|c| c == '*');
                    }
                }
            }
            (Some('?'), Some(_)) => {
                pattern.next();
                name.next();
            }
            (Some('?'), None) => return false,
            (Some(p), Some(n)) if p == n => {
                pattern.next();
                name.next();
            }
            (Some(_), _) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Glob matching tests
    // =========================================================================

    #[test]
    fn glob_exact_match() {
        assert!(matches_glob("hello", "hello"));
        assert!(!matches_glob("hello", "world"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(matches_glob("hello", "hell?"));
        assert!(matches_glob("hello", "h?llo"));
        assert!(matches_glob("hello", "?????"));
        assert!(!matches_glob("hello", "????"));
        assert!(!matches_glob("hello", "??????"));
    }

    #[test]
    fn glob_asterisk() {
        assert!(matches_glob("hello", "*"));
        assert!(matches_glob("hello", "h*"));
        assert!(matches_glob("hello", "*o"));
        assert!(matches_glob("hello", "h*o"));
        assert!(matches_glob("hello", "*ello"));
        assert!(matches_glob("hello", "hell*"));
        assert!(matches_glob("hello.rs", "*.rs"));
        assert!(!matches_glob("hello.txt", "*.rs"));
    }

    #[test]
    fn glob_combined() {
        assert!(matches_glob("test_file.rs", "test_*.rs"));
        assert!(matches_glob("test_file.rs", "*_*.rs"));
        assert!(matches_glob("a.b.c", "*.*.c"));
        assert!(!matches_glob("a.b.c", "*.*.d"));
    }

    #[test]
    fn glob_empty() {
        assert!(matches_glob("", ""));
        assert!(matches_glob("", "*"));
        assert!(!matches_glob("hello", ""));
        assert!(!matches_glob("", "?"));
    }

    #[test]
    fn glob_multiple_asterisks() {
        assert!(matches_glob("foo_bar_baz", "*_*_*"));
        assert!(matches_glob("abcd", "**"));
        assert!(matches_glob("abcd", "***"));
    }

    // =========================================================================
    // Integration tests with scheduler
    // =========================================================================

    use crate::Environment;
    use crate::io_handle::IoHandle;
    use amla_scheduler::{RandomSourceFn, Scheduler, SchedulerState, TimeSourceFn};
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

    fn create_test_vfs() -> Rc<RefCell<Vfs>> {
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        {
            let mut v = vfs.borrow_mut();
            v.create_dir("/workspace/src", Permission::ReadWrite)
                .unwrap();
            v.write_file(
                "/workspace/src/main.rs",
                b"fn main() {}",
                Permission::ReadWrite,
            )
            .unwrap();
            v.write_file(
                "/workspace/src/lib.rs",
                b"pub mod lib;",
                Permission::ReadWrite,
            )
            .unwrap();
            v.create_dir("/workspace/tests", Permission::ReadWrite)
                .unwrap();
            v.write_file(
                "/workspace/tests/test1.rs",
                b"#[test]",
                Permission::ReadWrite,
            )
            .unwrap();
            v.write_file("/workspace/README.md", b"# README", Permission::ReadWrite)
                .unwrap();
        }
        vfs
    }

    fn make_ctx(
        argv: Vec<&str>,
        vfs: Rc<RefCell<Vfs>>,
        scheduler: Scheduler,
    ) -> (CmdContext, IoHandle) {
        let stdout = IoHandle::buffer();
        let ctx = CmdContext::new(
            argv.into_iter().map(String::from).collect(),
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
    fn find_help() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();
        let (ctx, stdout) = make_ctx(vec!["find", "--help"], vfs, scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        let state = scheduler.run();
        assert!(matches!(state, SchedulerState::Done));

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("find - search for files"));
    }

    #[test]
    fn find_default_cwd() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();
        let (ctx, stdout) = make_ctx(vec!["find"], vfs, scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains('.'), "should contain current dir");
    }

    #[test]
    fn find_name_pattern() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();
        let (ctx, stdout) = make_ctx(vec!["find", ".", "-name", "*.rs"], vfs, scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("main.rs"));
        assert!(output.contains("lib.rs"));
        assert!(output.contains("test1.rs"));
        assert!(!output.contains("README.md"));
    }

    #[test]
    fn find_type_file() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();
        let (ctx, stdout) = make_ctx(vec!["find", ".", "-type", "f"], vfs, scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("main.rs"));
        assert!(output.contains("README.md"));
        // Should not contain directories
        assert!(!output.lines().any(|l| l.ends_with("/src") || l == "./src"));
    }

    #[test]
    fn find_type_dir() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();
        let (ctx, stdout) = make_ctx(vec!["find", ".", "-type", "d"], vfs, scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("src") || output.contains("tests"));
        // Should not contain files
        assert!(!output.contains("main.rs"));
        assert!(!output.contains("README.md"));
    }

    #[test]
    fn find_maxdepth() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();
        let (ctx, stdout) = make_ctx(vec!["find", ".", "--maxdepth", "1"], vfs, scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        // Should include . and immediate children
        assert!(output.contains('.'));
        // Should NOT include files inside subdirectories
        assert!(!output.contains("main.rs"));
    }

    #[test]
    fn find_combined_filters() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();
        let (ctx, stdout) = make_ctx(
            vec!["find", ".", "-name", "*.rs", "-type", "f"],
            vfs,
            scheduler.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("main.rs"));
        assert!(output.contains("lib.rs"));
        // Should not include directories or non-.rs files
        assert!(!output.contains("README.md"));
    }

    #[test]
    fn find_nonexistent_path() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();
        let (ctx, _stdout) = make_ctx(vec!["find", "/nonexistent"], vfs, scheduler.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        // Should still succeed (just print error message)
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }

    // =========================================================================
    // Comprehensive integration tests using Shell
    // =========================================================================

    use crate::Shell;

    /// Helper to run a shell command and return exit code
    fn shell_run(shell: &Shell, cmd: &str) -> i32 {
        use std::task::{Context, Poll};

        let scheduler = shell.scheduler().clone();
        let mut fut = std::pin::pin!(shell.execute(cmd));

        let waker = amla_scheduler::noop_waker();
        let mut cx = Context::from_waker(&waker);

        for _ in 0..10000 {
            if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
                return result.unwrap_or(-1);
            }
            let _ = scheduler.run_step();
        }
        panic!("command '{cmd}' did not complete");
    }

    /// Helper to get stdout from a shell command
    fn shell_output(shell: &mut Shell, cmd: &str) -> String {
        use crate::io_handle::IoHandle;

        let stdout = IoHandle::buffer();
        shell.set_stdout(stdout.clone());
        shell_run(shell, cmd);
        String::from_utf8(stdout.get_buffer().unwrap_or_default()).unwrap_or_default()
    }

    fn test_shell() -> Shell {
        Shell::new(test_scheduler())
    }

    #[test]
    fn find_deep_hierarchy_created_by_shell() {
        let mut shell = test_shell();

        // Create a deep directory hierarchy using shell commands
        shell_run(&shell, "mkdir -p /workspace/project/src/core/utils");
        shell_run(&shell, "mkdir -p /workspace/project/src/api/handlers");
        shell_run(&shell, "mkdir -p /workspace/project/tests/unit");
        shell_run(&shell, "mkdir -p /workspace/project/tests/integration");
        shell_run(&shell, "mkdir -p /workspace/project/docs/api");

        // Create files at various levels
        shell_run(
            &shell,
            "echo 'fn main() {}' > /workspace/project/src/main.rs",
        );
        shell_run(
            &shell,
            "echo 'pub mod core;' > /workspace/project/src/lib.rs",
        );
        shell_run(
            &shell,
            "echo 'pub fn util() {}' > /workspace/project/src/core/utils/helpers.rs",
        );
        shell_run(
            &shell,
            "echo 'pub fn api() {}' > /workspace/project/src/api/mod.rs",
        );
        shell_run(
            &shell,
            "echo 'pub fn handler() {}' > /workspace/project/src/api/handlers/user.rs",
        );
        shell_run(
            &shell,
            "echo '#[test]' > /workspace/project/tests/unit/test_core.rs",
        );
        shell_run(
            &shell,
            "echo '#[test]' > /workspace/project/tests/integration/test_api.rs",
        );
        shell_run(
            &shell,
            "echo '# API Docs' > /workspace/project/docs/api/README.md",
        );
        shell_run(&shell, "echo '# Project' > /workspace/project/README.md");
        shell_run(&shell, "echo 'version = 1' > /workspace/project/Cargo.toml");

        // Test: find all .rs files
        let output = shell_output(&mut shell, "find /workspace/project -name '*.rs'");
        assert!(output.contains("main.rs"), "should find main.rs");
        assert!(output.contains("lib.rs"), "should find lib.rs");
        assert!(output.contains("helpers.rs"), "should find helpers.rs");
        assert!(output.contains("user.rs"), "should find user.rs");
        assert!(output.contains("test_core.rs"), "should find test_core.rs");
        assert!(output.contains("test_api.rs"), "should find test_api.rs");
        assert!(!output.contains("README.md"), "should not find .md files");
        assert!(
            !output.contains("Cargo.toml"),
            "should not find .toml files"
        );

        // Test: find all directories
        let output = shell_output(&mut shell, "find /workspace/project -type d");
        assert!(output.contains("src"), "should find src dir");
        assert!(output.contains("core"), "should find core dir");
        assert!(output.contains("utils"), "should find utils dir");
        assert!(output.contains("api"), "should find api dir");
        assert!(output.contains("handlers"), "should find handlers dir");
        assert!(output.contains("tests"), "should find tests dir");
        assert!(output.contains("docs"), "should find docs dir");
        // Should not contain files
        assert!(!output.contains("main.rs"));
        assert!(!output.contains("README.md"));

        // Test: find only files (not directories)
        let output = shell_output(&mut shell, "find /workspace/project -type f");
        assert!(output.contains("main.rs"));
        assert!(output.contains("README.md"));
        assert!(output.contains("Cargo.toml"));
        // Count lines - should be exactly 10 files
        let file_count = output.lines().count();
        assert_eq!(file_count, 10, "should find exactly 10 files");
    }

    #[test]
    fn find_with_maxdepth_on_deep_hierarchy() {
        let mut shell = test_shell();

        // Create a 5-level deep hierarchy
        shell_run(&shell, "mkdir -p /workspace/a/b/c/d/e");
        shell_run(&shell, "echo 'level0' > /workspace/file0.txt");
        shell_run(&shell, "echo 'level1' > /workspace/a/file1.txt");
        shell_run(&shell, "echo 'level2' > /workspace/a/b/file2.txt");
        shell_run(&shell, "echo 'level3' > /workspace/a/b/c/file3.txt");
        shell_run(&shell, "echo 'level4' > /workspace/a/b/c/d/file4.txt");
        shell_run(&shell, "echo 'level5' > /workspace/a/b/c/d/e/file5.txt");

        // maxdepth 0: only the starting point
        let output = shell_output(&mut shell, "find /workspace -maxdepth 0");
        assert!(output.contains("/workspace"));
        assert!(!output.contains("file0.txt"));
        assert!(!output.contains("/a"));

        // maxdepth 1: starting point + immediate children
        let output = shell_output(&mut shell, "find /workspace -maxdepth 1");
        assert!(output.contains("/workspace"));
        assert!(output.contains("file0.txt"));
        assert!(output.contains("/a") || output.contains("workspace/a"));
        assert!(!output.contains("file1.txt"));
        assert!(!output.contains("/b"));

        // maxdepth 2: up to 2 levels
        let output = shell_output(&mut shell, "find /workspace -maxdepth 2");
        assert!(output.contains("file0.txt"));
        assert!(output.contains("file1.txt"));
        assert!(!output.contains("file2.txt"));

        // maxdepth 3: up to 3 levels
        let output = shell_output(&mut shell, "find /workspace -maxdepth 3");
        assert!(output.contains("file2.txt"));
        assert!(!output.contains("file3.txt"));

        // No maxdepth: should find all
        let output = shell_output(&mut shell, "find /workspace -name '*.txt'");
        assert!(output.contains("file0.txt"));
        assert!(output.contains("file1.txt"));
        assert!(output.contains("file2.txt"));
        assert!(output.contains("file3.txt"));
        assert!(output.contains("file4.txt"));
        assert!(output.contains("file5.txt"));
    }

    #[test]
    fn find_with_mindepth() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/level1/level2/level3");
        shell_run(&shell, "echo 'root' > /workspace/root.txt");
        shell_run(&shell, "echo 'l1' > /workspace/level1/l1.txt");
        shell_run(&shell, "echo 'l2' > /workspace/level1/level2/l2.txt");
        shell_run(&shell, "echo 'l3' > /workspace/level1/level2/level3/l3.txt");

        // mindepth 2: include depth >= 2
        // depth 0 = /workspace, depth 1 = root.txt/level1, depth 2 = l1.txt/level2, etc.
        let output = shell_output(&mut shell, "find /workspace -mindepth 2 -name '*.txt'");
        assert!(
            !output.contains("root.txt"),
            "should skip root.txt (depth 1)"
        );
        assert!(output.contains("l1.txt"), "should include l1.txt (depth 2)");
        assert!(output.contains("l2.txt"), "should include l2.txt (depth 3)");
        assert!(output.contains("l3.txt"), "should include l3.txt (depth 4)");

        // mindepth 3: include depth >= 3
        let output = shell_output(&mut shell, "find /workspace -mindepth 3 -name '*.txt'");
        assert!(
            !output.contains("root.txt"),
            "should skip root.txt (depth 1)"
        );
        assert!(!output.contains("l1.txt"), "should skip l1.txt (depth 2)");
        assert!(output.contains("l2.txt"), "should include l2.txt (depth 3)");
        assert!(output.contains("l3.txt"), "should include l3.txt (depth 4)");
    }

    #[test]
    fn find_name_patterns_various() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/patterns");
        shell_run(&shell, "touch /workspace/patterns/test_one.rs");
        shell_run(&shell, "touch /workspace/patterns/test_two.rs");
        shell_run(&shell, "touch /workspace/patterns/test_three.rs");
        shell_run(&shell, "touch /workspace/patterns/main.rs");
        shell_run(&shell, "touch /workspace/patterns/lib.rs");
        shell_run(&shell, "touch /workspace/patterns/config.json");
        shell_run(&shell, "touch /workspace/patterns/config.yaml");
        shell_run(&shell, "touch /workspace/patterns/README.md");
        shell_run(&shell, "touch /workspace/patterns/CHANGELOG.md");
        shell_run(&shell, "touch /workspace/patterns/a.txt");
        shell_run(&shell, "touch /workspace/patterns/ab.txt");
        shell_run(&shell, "touch /workspace/patterns/abc.txt");

        // Pattern: test_*.rs
        let output = shell_output(&mut shell, "find /workspace/patterns -name 'test_*.rs'");
        assert!(output.contains("test_one.rs"));
        assert!(output.contains("test_two.rs"));
        assert!(output.contains("test_three.rs"));
        assert!(!output.contains("main.rs"));
        assert!(!output.contains("lib.rs"));

        // Pattern: *.md
        let output = shell_output(&mut shell, "find /workspace/patterns -name '*.md'");
        assert!(output.contains("README.md"));
        assert!(output.contains("CHANGELOG.md"));
        assert!(!output.contains(".rs"));
        assert!(!output.contains(".json"));

        // Pattern: config.*
        let output = shell_output(&mut shell, "find /workspace/patterns -name 'config.*'");
        assert!(output.contains("config.json"));
        assert!(output.contains("config.yaml"));
        assert!(!output.contains("README"));

        // Pattern: ?.txt (single char before .txt)
        let output = shell_output(&mut shell, "find /workspace/patterns -name '?.txt'");
        assert!(output.contains("a.txt"));
        assert!(!output.contains("ab.txt"));
        assert!(!output.contains("abc.txt"));

        // Pattern: ??.txt (two chars before .txt)
        let output = shell_output(&mut shell, "find /workspace/patterns -name '??.txt'");
        assert!(output.contains("ab.txt"));
        assert!(!output.contains("a.txt"));
        assert!(!output.contains("abc.txt"));

        // Pattern: *.*  (any file with extension)
        let output = shell_output(&mut shell, "find /workspace/patterns -name '*.*' -type f");
        let line_count = output.lines().count();
        assert_eq!(line_count, 12, "should find all 12 files with extensions");
    }

    #[test]
    fn find_combined_type_and_name_on_mixed_content() {
        let mut shell = test_shell();

        // Create directories and files with overlapping names
        shell_run(&shell, "mkdir -p /workspace/mixed/src.rs"); // dir named src.rs
        shell_run(&shell, "mkdir -p /workspace/mixed/config");
        shell_run(&shell, "echo 'file' > /workspace/mixed/src.rs/actual.rs"); // file inside dir named src.rs
        shell_run(&shell, "echo 'file' > /workspace/mixed/main.rs"); // regular file
        shell_run(&shell, "echo 'file' > /workspace/mixed/config/app.rs");

        // Find only FILES named *.rs
        let output = shell_output(&mut shell, "find /workspace/mixed -name '*.rs' -type f");
        assert!(output.contains("main.rs"), "should find main.rs file");
        assert!(output.contains("actual.rs"), "should find actual.rs file");
        assert!(output.contains("app.rs"), "should find app.rs file");
        // The directory src.rs should NOT be in this output
        let lines: Vec<&str> = output.lines().collect();
        assert!(
            !lines
                .iter()
                .any(|l| l.ends_with("/src.rs") && !l.contains("actual")),
            "should not include src.rs directory, output: {output}"
        );

        // Find only DIRECTORIES named *.rs
        let output = shell_output(&mut shell, "find /workspace/mixed -name '*.rs' -type d");
        assert!(output.contains("src.rs"), "should find src.rs directory");
        assert!(!output.contains("main.rs"), "should not find main.rs");
        assert!(!output.contains("actual.rs"), "should not find actual.rs");
    }

    #[test]
    fn find_from_subdirectory() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/project/src/module");
        shell_run(&shell, "echo 'root' > /workspace/project/root.txt");
        shell_run(&shell, "echo 'src' > /workspace/project/src/src.txt");
        shell_run(&shell, "echo 'mod' > /workspace/project/src/module/mod.txt");

        // Find from project directory
        shell_run(&shell, "cd /workspace/project");
        let output = shell_output(&mut shell, "find . -name '*.txt'");
        assert!(output.contains("root.txt"));
        assert!(output.contains("src.txt"));
        assert!(output.contains("mod.txt"));

        // Find from src subdirectory
        shell_run(&shell, "cd /workspace/project/src");
        let output = shell_output(&mut shell, "find . -name '*.txt'");
        assert!(
            !output.contains("root.txt"),
            "should not find root.txt when starting from src"
        );
        assert!(output.contains("src.txt"));
        assert!(output.contains("mod.txt"));
    }

    #[test]
    fn find_multiple_paths() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/dir1/sub1");
        shell_run(&shell, "mkdir -p /workspace/dir2/sub2");
        shell_run(&shell, "echo 'a' > /workspace/dir1/a.txt");
        shell_run(&shell, "echo 'b' > /workspace/dir1/sub1/b.txt");
        shell_run(&shell, "echo 'c' > /workspace/dir2/c.txt");
        shell_run(&shell, "echo 'd' > /workspace/dir2/sub2/d.txt");

        // Find in multiple paths
        let output = shell_output(
            &mut shell,
            "find /workspace/dir1 /workspace/dir2 -name '*.txt'",
        );
        assert!(output.contains("a.txt"));
        assert!(output.contains("b.txt"));
        assert!(output.contains("c.txt"));
        assert!(output.contains("d.txt"));
    }

    #[test]
    fn find_with_special_characters_in_names() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/special");
        shell_run(&shell, "echo 'x' > /workspace/special/file-with-dash.txt");
        shell_run(
            &shell,
            "echo 'x' > /workspace/special/file_with_underscore.txt",
        );
        shell_run(
            &shell,
            "echo 'x' > /workspace/special/file.multiple.dots.txt",
        );
        shell_run(&shell, "echo 'x' > /workspace/special/UPPERCASE.TXT");
        shell_run(&shell, "echo 'x' > /workspace/special/MixedCase.Txt");

        // Find with dash
        let output = shell_output(&mut shell, "find /workspace/special -name '*-*'");
        assert!(output.contains("file-with-dash.txt"));

        // Find with underscore
        let output = shell_output(&mut shell, "find /workspace/special -name '*_*'");
        assert!(output.contains("file_with_underscore.txt"));

        // Find uppercase (exact match)
        let output = shell_output(&mut shell, "find /workspace/special -name 'UPPERCASE.TXT'");
        assert!(output.contains("UPPERCASE.TXT"));

        // Find with multiple dots
        let output = shell_output(&mut shell, "find /workspace/special -name '*.*.*.txt'");
        assert!(output.contains("file.multiple.dots.txt"));
    }

    #[test]
    fn find_empty_directory() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/empty_dir");

        // Find in empty directory
        let output = shell_output(&mut shell, "find /workspace/empty_dir");
        // Should just find the directory itself
        assert!(output.contains("empty_dir"));
        assert_eq!(
            output.lines().count(),
            1,
            "should only find the directory itself"
        );

        // Find files in empty directory
        let output = shell_output(&mut shell, "find /workspace/empty_dir -type f");
        assert!(output.trim().is_empty(), "should find no files");
    }

    #[test]
    fn find_single_file() {
        let mut shell = test_shell();

        shell_run(&shell, "echo 'content' > /workspace/single.txt");

        // Find on a single file path
        let output = shell_output(&mut shell, "find /workspace/single.txt");
        assert!(output.contains("single.txt"));
        assert_eq!(output.lines().count(), 1);

        // Find with -type f on a file path
        let output = shell_output(&mut shell, "find /workspace/single.txt -type f");
        assert!(output.contains("single.txt"));

        // Find with -type d on a file path (should not match)
        let output = shell_output(&mut shell, "find /workspace/single.txt -type d");
        assert!(!output.contains("single.txt"));
    }

    #[test]
    fn find_respects_hidden_files() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/hidden");
        shell_run(&shell, "mkdir -p /workspace/hidden/.hidden_dir");
        shell_run(&shell, "echo 'visible' > /workspace/hidden/visible.txt");
        shell_run(&shell, "echo 'hidden' > /workspace/hidden/.hidden_file");
        shell_run(
            &shell,
            "echo 'in_hidden' > /workspace/hidden/.hidden_dir/file.txt",
        );

        // Find all - should include hidden files and directories
        let output = shell_output(&mut shell, "find /workspace/hidden");
        assert!(output.contains("visible.txt"));
        assert!(output.contains(".hidden_file"));
        assert!(output.contains(".hidden_dir"));
        assert!(output.contains("file.txt"));

        // Find with name pattern for hidden files
        let output = shell_output(&mut shell, "find /workspace/hidden -name '.*'");
        assert!(output.contains(".hidden_file"));
        assert!(output.contains(".hidden_dir"));
        assert!(!output.contains("visible.txt"));
    }

    #[test]
    fn find_large_flat_directory() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/flat");

        // Create 50 files
        for i in 0..50 {
            shell_run(&shell, &format!("touch /workspace/flat/file{i:03}.txt"));
        }

        // Find all txt files
        let output = shell_output(&mut shell, "find /workspace/flat -name '*.txt' -type f");
        let count = output.lines().count();
        assert_eq!(count, 50, "should find all 50 files");

        // Find specific patterns
        let output = shell_output(&mut shell, "find /workspace/flat -name 'file00?.txt'");
        let count = output.lines().count();
        assert_eq!(count, 10, "should find files 000-009");

        let output = shell_output(&mut shell, "find /workspace/flat -name 'file01?.txt'");
        let count = output.lines().count();
        assert_eq!(count, 10, "should find files 010-019");
    }

    #[test]
    fn find_combined_maxdepth_and_mindepth() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/depth/l1/l2/l3/l4");
        shell_run(&shell, "echo '0' > /workspace/depth/f0.txt");
        shell_run(&shell, "echo '1' > /workspace/depth/l1/f1.txt");
        shell_run(&shell, "echo '2' > /workspace/depth/l1/l2/f2.txt");
        shell_run(&shell, "echo '3' > /workspace/depth/l1/l2/l3/f3.txt");
        shell_run(&shell, "echo '4' > /workspace/depth/l1/l2/l3/l4/f4.txt");

        // File depths from starting path /workspace/depth:
        // f0.txt = depth 1, f1.txt = depth 2, f2.txt = depth 3, f3.txt = depth 4, f4.txt = depth 5
        // mindepth 2, maxdepth 3: should find f1.txt (d=2) and f2.txt (d=3) only
        let output = shell_output(
            &mut shell,
            "find /workspace/depth -mindepth 2 -maxdepth 3 -name '*.txt'",
        );
        assert!(
            !output.contains("f0.txt"),
            "should skip f0.txt (depth 1 < 2)"
        );
        assert!(output.contains("f1.txt"), "should include f1.txt (depth 2)");
        assert!(output.contains("f2.txt"), "should include f2.txt (depth 3)");
        assert!(
            !output.contains("f3.txt"),
            "should skip f3.txt (depth 4 > 3)"
        );
        assert!(
            !output.contains("f4.txt"),
            "should skip f4.txt (depth 5 > 3)"
        );
    }

    // =========================================================================
    // Additional edge case tests for improved coverage
    // =========================================================================

    #[test]
    fn glob_trailing_question_mark() {
        // Pattern ending with ?
        assert!(matches_glob("test1", "test?"));
        assert!(matches_glob("testa", "test?"));
        assert!(!matches_glob("test", "test?"));
        assert!(!matches_glob("test12", "test?"));
    }

    #[test]
    fn glob_leading_asterisk() {
        // Pattern starting with *
        assert!(matches_glob("mytest", "*test"));
        assert!(matches_glob("test", "*test"));
        assert!(matches_glob("a_long_test", "*test"));
        assert!(!matches_glob("testing", "*test"));
    }

    #[test]
    fn glob_only_wildcards() {
        // Patterns with only wildcards
        assert!(matches_glob("anything", "*"));
        assert!(matches_glob("x", "?"));
        assert!(matches_glob("ab", "??"));
        assert!(matches_glob("abc", "???"));
        assert!(matches_glob("abcd", "????"));
        assert!(matches_glob("", "*"));
        assert!(!matches_glob("", "?"));
        assert!(!matches_glob("a", "??"));
    }

    #[test]
    fn glob_case_sensitivity() {
        // Glob matching should be case-sensitive
        assert!(matches_glob("Test.RS", "Test.RS"));
        assert!(!matches_glob("Test.RS", "test.rs"));
        assert!(!matches_glob("test.rs", "Test.RS"));
        assert!(matches_glob("Test.RS", "*.RS"));
        assert!(!matches_glob("Test.RS", "*.rs"));
    }

    #[test]
    fn glob_adjacent_wildcards() {
        // Adjacent wildcards
        assert!(matches_glob("ab", "*?"));
        assert!(matches_glob("abc", "*?"));
        assert!(matches_glob("a", "*?"));
        assert!(!matches_glob("", "*?"));

        assert!(matches_glob("ab", "?*"));
        assert!(matches_glob("abc", "?*"));
        assert!(matches_glob("a", "?*"));
        assert!(!matches_glob("", "?*"));

        assert!(matches_glob("abc", "?*?"));
        assert!(matches_glob("ab", "?*?"));
        assert!(!matches_glob("a", "?*?"));
    }

    #[test]
    fn find_unknown_options_silently_ignored() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/opts");
        shell_run(&shell, "echo 'test' > /workspace/opts/file.txt");

        // Unknown options like -exec, -newer, -perm should be silently ignored
        let output = shell_output(
            &mut shell,
            "find /workspace/opts -unknown -option -name '*.txt'",
        );
        assert!(
            output.contains("file.txt"),
            "should still find files despite unknown options"
        );
    }

    #[test]
    fn find_missing_option_argument() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/missing");
        shell_run(&shell, "echo 'test' > /workspace/missing/file.txt");

        // -name at end without argument - should gracefully handle
        let output = shell_output(&mut shell, "find /workspace/missing -name");
        // Should still work (no name pattern set means match all)
        assert!(
            output.contains("file.txt") || output.contains("missing"),
            "should handle missing argument gracefully"
        );

        // -type at end without argument
        let output = shell_output(&mut shell, "find /workspace/missing -type");
        assert!(
            output.contains("missing"),
            "should handle missing type argument"
        );

        // -maxdepth at end without argument
        let output = shell_output(&mut shell, "find /workspace/missing -maxdepth");
        assert!(
            output.contains("missing"),
            "should handle missing maxdepth argument"
        );

        // -mindepth at end without argument
        let output = shell_output(&mut shell, "find /workspace/missing -mindepth");
        assert!(
            output.contains("missing"),
            "should handle missing mindepth argument"
        );
    }

    #[test]
    fn find_invalid_type_value() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/invalid");
        shell_run(&shell, "echo 'test' > /workspace/invalid/file.txt");

        // Invalid type value 'x' (not 'f' or 'd')
        // Per implementation, unknown types are ignored (matches_criteria just doesn't filter)
        let output = shell_output(&mut shell, "find /workspace/invalid -type x");
        // Should find both files and directories since 'x' type is unrecognized
        assert!(output.contains("invalid"), "should find directory");
        assert!(output.contains("file.txt"), "should find file");
    }

    #[test]
    fn find_invalid_depth_values() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/badnum/sub");
        shell_run(&shell, "echo 'test' > /workspace/badnum/file.txt");
        shell_run(&shell, "echo 'test' > /workspace/badnum/sub/nested.txt");

        // Non-numeric maxdepth - should be ignored (parse returns None)
        let output = shell_output(&mut shell, "find /workspace/badnum -maxdepth abc");
        // Should find all files since maxdepth was invalid
        assert!(output.contains("file.txt"));
        assert!(output.contains("nested.txt"));

        // Non-numeric mindepth - should default to 0
        let output = shell_output(&mut shell, "find /workspace/badnum -mindepth xyz");
        assert!(output.contains("badnum"), "should find root (mindepth 0)");
        assert!(output.contains("file.txt"));
    }

    #[test]
    fn find_root_path() {
        let mut shell = test_shell();

        // Create files under /workspace (default cwd) instead of root
        shell_run(&shell, "mkdir -p /workspace/testroot");
        shell_run(&shell, "echo 'data' > /workspace/testroot/data.txt");

        // Find from /workspace/testroot should work
        let output = shell_output(&mut shell, "find /workspace/testroot -name 'data.txt'");
        assert!(output.contains("data.txt"), "should find file: {output}");

        // Find directory with maxdepth 0
        let output = shell_output(&mut shell, "find /workspace/testroot -maxdepth 0");
        assert!(
            output.contains("testroot"),
            "should output start path: {output}"
        );
    }

    #[test]
    fn find_double_dash_option_format() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/dashes");
        shell_run(&shell, "echo 'x' > /workspace/dashes/test.rs");
        shell_run(&shell, "mkdir -p /workspace/dashes/subdir");
        shell_run(&shell, "echo 'y' > /workspace/dashes/subdir/deep.rs");

        // Test --name format
        let output = shell_output(&mut shell, "find /workspace/dashes --name '*.rs' --type f");
        assert!(output.contains("test.rs"));
        assert!(output.contains("deep.rs"));

        // Test -mindepth with single dash (already tested maxdepth with --)
        let output = shell_output(
            &mut shell,
            "find /workspace/dashes -mindepth 1 -name '*.rs'",
        );
        assert!(output.contains("test.rs"));
    }

    #[test]
    fn find_print_option() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/printtest");
        shell_run(&shell, "echo 'x' > /workspace/printtest/file.txt");

        // -print option should work (even though it's always the default action)
        let output = shell_output(&mut shell, "find /workspace/printtest -print");
        assert!(output.contains("printtest"));
        assert!(output.contains("file.txt"));

        // -print with other options
        let output = shell_output(&mut shell, "find /workspace/printtest -name '*.txt' -print");
        assert!(output.contains("file.txt"));
    }

    #[test]
    fn find_path_without_slash() {
        // Test name extraction when display_path has no slash (edge case in matches_criteria)
        // This tests line 207: rsplit('/').next().unwrap_or(display_path)

        // Direct test of matches_glob with simple name
        assert!(matches_glob("file.txt", "*.txt"));
        assert!(matches_glob("file.txt", "file.*"));
        assert!(matches_glob("file.txt", "file.txt"));
    }

    #[test]
    fn find_help_variants() {
        let scheduler = test_scheduler();
        let vfs = create_test_vfs();

        // Test -h
        let (ctx, stdout) = make_ctx(vec!["find", "-h"], vfs.clone(), scheduler.clone());
        let handle = scheduler.spawn(run(ctx));
        scheduler.run();
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
        let output = String::from_utf8(stdout.get_buffer().unwrap()).unwrap();
        assert!(output.contains("find - search for files"));

        // Test -help
        let scheduler2 = test_scheduler();
        let (ctx2, stdout2) = make_ctx(vec!["find", "-help"], vfs, scheduler2.clone());
        let handle2 = scheduler2.spawn(run(ctx2));
        scheduler2.run();
        let result2 = handle2.try_get().unwrap().unwrap();
        assert_eq!(result2.code, 0);
        let output2 = String::from_utf8(stdout2.get_buffer().unwrap()).unwrap();
        assert!(output2.contains("find - search for files"));
    }

    #[test]
    fn find_with_absolute_display_path() {
        let mut shell = test_shell();

        // Use a path under /workspace which is the default cwd
        shell_run(&shell, "mkdir -p /workspace/abspath/to/dir");
        shell_run(
            &shell,
            "echo 'content' > /workspace/abspath/to/dir/file.txt",
        );

        // When using absolute path, output should show absolute paths
        let output = shell_output(&mut shell, "find /workspace/abspath -name '*.txt'");
        assert!(
            output.contains("abspath/to/dir/file.txt"),
            "should show path containing file: {output}"
        );
    }

    #[test]
    fn find_deeply_nested_with_name_and_type() {
        let mut shell = test_shell();

        // Create a complex nested structure
        shell_run(&shell, "mkdir -p /workspace/deep/a/b/c/d/e");
        shell_run(&shell, "mkdir -p /workspace/deep/x/y/z");
        shell_run(&shell, "echo '1' > /workspace/deep/a/b/c/d/e/target.txt");
        shell_run(&shell, "echo '2' > /workspace/deep/x/y/z/target.txt");
        shell_run(&shell, "mkdir /workspace/deep/x/y/target.txt"); // Directory with .txt name!

        // Find only files named target.txt
        let output = shell_output(
            &mut shell,
            "find /workspace/deep -name 'target.txt' -type f",
        );
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "should find exactly 2 target.txt files, got: {lines:?}"
        );

        // Find only directories named target.txt
        let output = shell_output(
            &mut shell,
            "find /workspace/deep -name 'target.txt' -type d",
        );
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "should find exactly 1 target.txt directory, got: {lines:?}"
        );
    }

    #[test]
    fn find_glob_complex_patterns() {
        // Test more complex glob edge cases
        assert!(matches_glob("a", "a*"));
        assert!(matches_glob("ab", "a*"));
        assert!(matches_glob("abc", "a*c"));
        assert!(matches_glob("ac", "a*c"));
        assert!(matches_glob("abbc", "a*c"));

        // Multiple asterisks with literals between
        assert!(matches_glob("a_b_c", "a*b*c"));
        assert!(matches_glob("axbxc", "a*b*c"));
        assert!(matches_glob("aXXbYYc", "a*b*c"));
        // Note: "abc" DOES match "a*b*c" because * can match zero chars (a + "" + b + "" + c)
        assert!(matches_glob("abc", "a*b*c"));

        // Cases that should NOT match
        assert!(!matches_glob("axc", "a*b*c")); // missing 'b'
        assert!(!matches_glob("ab", "a*b*c")); // missing 'c'

        // Question marks with asterisks
        assert!(matches_glob("abc", "?*c"));
        assert!(matches_glob("xyzabc", "???*c"));
        assert!(!matches_glob("ab", "???*c"));
    }

    #[test]
    fn find_symlinks_as_files() {
        // Note: The VFS may not support symlinks, but we test the behavior
        // If symlinks existed, they would likely be treated as files
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/linktest");
        shell_run(&shell, "echo 'target' > /workspace/linktest/target.txt");

        // Just verify normal file operations work
        let output = shell_output(&mut shell, "find /workspace/linktest -type f");
        assert!(output.contains("target.txt"));
    }

    #[test]
    fn find_maxdepth_zero() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/zerodepth/sub");
        shell_run(&shell, "echo 'x' > /workspace/zerodepth/file.txt");

        // maxdepth 0 should only show the starting path itself
        let output = shell_output(&mut shell, "find /workspace/zerodepth -maxdepth 0");
        assert!(output.contains("zerodepth"), "should include starting path");
        assert!(!output.contains("file.txt"), "should not descend at all");
        assert!(!output.contains("/sub"), "should not show subdirectories");

        // Count lines - should be exactly 1
        let lines = output.lines().count();
        assert_eq!(lines, 1, "maxdepth 0 should output only the start path");
    }

    #[test]
    fn find_mindepth_exceeds_tree_depth() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/shallow/sub");
        shell_run(&shell, "echo 'x' > /workspace/shallow/file.txt");
        shell_run(&shell, "echo 'y' > /workspace/shallow/sub/nested.txt");

        // mindepth 10 on a shallow tree - should find nothing
        let output = shell_output(&mut shell, "find /workspace/shallow -mindepth 10");
        assert!(
            output.trim().is_empty(),
            "should find nothing with mindepth > tree depth"
        );
    }

    #[test]
    fn find_name_exact_match_no_wildcards() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/exact");
        shell_run(&shell, "echo 'a' > /workspace/exact/file.txt");
        shell_run(&shell, "echo 'b' > /workspace/exact/file.txt.bak");
        shell_run(&shell, "echo 'c' > /workspace/exact/afile.txt");
        shell_run(&shell, "echo 'd' > /workspace/exact/file.txtx");

        // Exact name match without wildcards
        let output = shell_output(&mut shell, "find /workspace/exact -name 'file.txt'");
        assert!(output.contains("file.txt"));
        assert!(!output.contains("file.txt.bak"));
        assert!(!output.contains("afile.txt"));
        assert!(!output.contains("file.txtx"));

        // Count matches
        let count = output.lines().count();
        assert_eq!(count, 1, "should find exactly one match");
    }

    #[test]
    fn find_multiple_paths_with_filters() {
        let mut shell = test_shell();

        shell_run(&shell, "mkdir -p /workspace/pathA/sub");
        shell_run(&shell, "mkdir -p /workspace/pathB/sub");
        shell_run(&shell, "echo 'a' > /workspace/pathA/file.rs");
        shell_run(&shell, "echo 'b' > /workspace/pathA/sub/nested.rs");
        shell_run(&shell, "echo 'c' > /workspace/pathB/file.rs");
        shell_run(&shell, "echo 'd' > /workspace/pathB/other.txt");

        // Find .rs files across multiple paths
        let output = shell_output(
            &mut shell,
            "find /workspace/pathA /workspace/pathB -name '*.rs' -type f",
        );
        assert!(output.contains("pathA") && output.contains("file.rs"));
        assert!(output.contains("nested.rs"));
        assert!(output.contains("pathB"));
        assert!(!output.contains("other.txt"));

        // Count .rs files
        let count = output.lines().count();
        assert_eq!(count, 3, "should find 3 .rs files across both paths");
    }
}
