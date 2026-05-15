//! touch - change file timestamps or create empty files

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;

use super::CommandResult;

/// touch [-c] file...
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut no_create = false;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('c'))) => no_create = true,
            Ok(Some(lexopt::Arg::Long("no-create"))) => no_create = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if files.is_empty() {
        ctx.eprintln("touch: missing file operand").await?;
        return Ok(Exit::code(1));
    }

    let mut exit_code = 0;

    for file_path in &files {
        if ctx.exists(file_path) {
            // File exists - VFS doesn't track timestamps, so this is a no-op
            continue;
        }

        if no_create {
            continue;
        }

        // Create empty file
        if let Err(e) = ctx.write_file(file_path, &[]) {
            ctx.eprintln(&format!("touch: cannot touch '{file_path}': {e}"))
                .await?;
            exit_code = 1;
        }
    }

    Ok(Exit::code(exit_code))
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

    fn make_ctx_with_vfs(
        argv: Vec<&str>,
        scheduler: Scheduler,
        vfs: Rc<RefCell<Vfs>>,
    ) -> (CmdContext, IoHandle, IoHandle) {
        let stdout = IoHandle::buffer();
        let stderr = IoHandle::buffer();
        let ctx = CmdContext::new(
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
    // Basic file creation tests
    // =========================================================================

    #[test]
    fn touch_creates_empty_file() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "/workspace/new.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        assert!(vfs.borrow().exists("/workspace/new.txt"));
        let content = vfs.borrow().read_file("/workspace/new.txt").unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn touch_existing_file_preserves_content() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file(
                "/workspace/existing.txt",
                b"original content",
                Permission::ReadWrite,
            )
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "/workspace/existing.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Content should be preserved
        let content = vfs.borrow().read_file("/workspace/existing.txt").unwrap();
        assert_eq!(content, b"original content");
    }

    #[test]
    fn touch_multiple_files() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec![
                "touch",
                "/workspace/a.txt",
                "/workspace/b.txt",
                "/workspace/c.txt",
            ],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        assert!(vfs.borrow().exists("/workspace/a.txt"));
        assert!(vfs.borrow().exists("/workspace/b.txt"));
        assert!(vfs.borrow().exists("/workspace/c.txt"));
    }

    #[test]
    fn touch_relative_path() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        // CmdContext has cwd="/workspace", so relative path should work
        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "relative.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // File should be created at /workspace/relative.txt
        assert!(vfs.borrow().exists("/workspace/relative.txt"));
    }

    // =========================================================================
    // No-create flag tests
    // =========================================================================

    #[test]
    fn touch_no_create_short_flag_nonexistent() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "-c", "/workspace/nofile.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // File should NOT be created
        assert!(!vfs.borrow().exists("/workspace/nofile.txt"));
    }

    #[test]
    fn touch_no_create_long_flag_nonexistent() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "--no-create", "/workspace/nofile.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // File should NOT be created
        assert!(!vfs.borrow().exists("/workspace/nofile.txt"));
    }

    #[test]
    fn touch_no_create_with_existing_file() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .write_file("/workspace/exists.txt", b"content", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "-c", "/workspace/exists.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // File should still exist with original content
        let content = vfs.borrow().read_file("/workspace/exists.txt").unwrap();
        assert_eq!(content, b"content");
    }

    // =========================================================================
    // Mixed scenarios
    // =========================================================================

    #[test]
    fn touch_mixed_existing_and_new_files() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();
        vfs.borrow_mut()
            .write_file("/workspace/old.txt", b"old", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "/workspace/old.txt", "/workspace/new.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Old file should be preserved
        let old_content = vfs.borrow().read_file("/workspace/old.txt").unwrap();
        assert_eq!(old_content, b"old");

        // New file should be created and empty
        let new_content = vfs.borrow().read_file("/workspace/new.txt").unwrap();
        assert!(new_content.is_empty());
    }

    #[test]
    fn touch_no_create_with_mixed_files() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();
        vfs.borrow_mut()
            .write_file("/workspace/exists.txt", b"exists", Permission::ReadWrite)
            .unwrap();

        // -c means only "touch" existing files, don't create new ones
        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "-c", "/workspace/exists.txt", "/workspace/new.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Existing file should still exist
        assert!(vfs.borrow().exists("/workspace/exists.txt"));
        // New file should NOT be created
        assert!(!vfs.borrow().exists("/workspace/new.txt"));
    }

    // =========================================================================
    // Error cases
    // =========================================================================

    #[test]
    fn touch_no_operand_error() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        let (ctx, _stdout, stderr) = make_ctx_with_vfs(vec!["touch"], scheduler.clone(), vfs);

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);

        let err_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(err_output.contains("touch: missing file operand"));
    }

    #[test]
    fn touch_parent_directory_missing() {
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        // /workspace exists but /workspace/subdir doesn't
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, stderr) = make_ctx_with_vfs(
            vec!["touch", "/workspace/subdir/file.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1);

        let err_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(err_output.contains("touch: cannot touch"));
        assert!(err_output.contains("/workspace/subdir/file.txt"));
    }

    #[test]
    fn touch_partial_failure() {
        // When touching multiple files and one fails, should return error but continue
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, stderr) = make_ctx_with_vfs(
            vec![
                "touch",
                "/workspace/good1.txt",
                "/nonexistent/bad.txt",
                "/workspace/good2.txt",
            ],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 1); // Should fail due to bad path

        // Good files should still be created
        assert!(vfs.borrow().exists("/workspace/good1.txt"));
        assert!(vfs.borrow().exists("/workspace/good2.txt"));

        // Error message should mention the bad file
        let err_output = String::from_utf8(stderr.get_buffer().unwrap()).unwrap();
        assert!(err_output.contains("/nonexistent/bad.txt"));
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn touch_with_unknown_option() {
        // Unknown options should be silently ignored per implementation
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "-x", "/workspace/file.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // File should be created despite unknown option
        assert!(vfs.borrow().exists("/workspace/file.txt"));
    }

    #[test]
    fn touch_flag_after_files() {
        // Test: flags encountered anywhere in args apply globally
        // Since lexopt parses all args first, then we process files,
        // -c applies to ALL files regardless of position
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) = make_ctx_with_vfs(
            vec!["touch", "/workspace/file.txt", "-c", "/workspace/other.txt"],
            scheduler.clone(),
            vfs.clone(),
        );

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);

        // Since -c is set during parsing (before file processing loop),
        // neither file should be created
        assert!(!vfs.borrow().exists("/workspace/file.txt"));
        assert!(!vfs.borrow().exists("/workspace/other.txt"));
    }

    #[test]
    fn touch_dot_path() {
        // "." is the current directory - touching it should be a no-op
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) =
            make_ctx_with_vfs(vec!["touch", "."], scheduler.clone(), vfs.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        // Since "." resolves to /workspace which exists, touch should be a no-op
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }

    #[test]
    fn touch_dotdot_path() {
        // ".." - parent directory
        let scheduler = test_scheduler();
        let vfs = Rc::new(RefCell::new(Vfs::new()));
        vfs.borrow_mut()
            .create_dir_all("/workspace", Permission::ReadWrite)
            .unwrap();

        let (ctx, _stdout, _stderr) =
            make_ctx_with_vfs(vec!["touch", ".."], scheduler.clone(), vfs.clone());

        let handle = scheduler.spawn(run(ctx));
        scheduler.run();

        assert!(handle.is_complete());
        // ".." from /workspace resolves to "/" which exists
        let result = handle.try_get().unwrap().unwrap();
        assert_eq!(result.code, 0);
    }
}
