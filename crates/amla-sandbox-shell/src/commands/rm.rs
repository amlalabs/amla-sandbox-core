//! rm - remove files or directories

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;

use super::CommandResult;

const HELP: &str = "\
rm - remove files or directories

Usage: rm [OPTIONS] FILE...

Options:
  -h, --help       Show this help message
  -r, -R, --recursive
                   Remove directories and their contents recursively
  -f, --force      Ignore nonexistent files, never prompt
  -v, --verbose    Explain what is being done

Examples:
  rm file.txt           Remove a single file
  rm -r directory       Remove a directory and its contents
  rm -rf temp           Force remove without errors for missing files
";

pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut recursive = false;
    let mut force = false;
    let mut verbose = false;
    let mut files: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('h'))) | Ok(Some(lexopt::Arg::Long("help"))) => {
                ctx.stdout_write_all(HELP.as_bytes()).await?;
                return Ok(Exit::success());
            }
            Ok(Some(lexopt::Arg::Short('r' | 'R'))) | Ok(Some(lexopt::Arg::Long("recursive"))) => {
                recursive = true;
            }
            Ok(Some(lexopt::Arg::Short('f'))) | Ok(Some(lexopt::Arg::Long("force"))) => {
                force = true;
            }
            Ok(Some(lexopt::Arg::Short('v'))) | Ok(Some(lexopt::Arg::Long("verbose"))) => {
                verbose = true;
            }
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if files.is_empty() {
        if !force {
            ctx.eprintln("rm: missing operand").await?;
            return Ok(Exit::code(1));
        }
        return Ok(Exit::success());
    }

    let mut exit_code = 0;

    for file_path in &files {
        if !ctx.exists(file_path) {
            if !force {
                ctx.eprintln(&format!(
                    "rm: cannot remove '{file_path}': No such file or directory"
                ))
                .await?;
                exit_code = 1;
            }
            continue;
        }

        if ctx.is_dir(file_path) {
            if !recursive {
                ctx.eprintln(&format!("rm: cannot remove '{file_path}': Is a directory"))
                    .await?;
                exit_code = 1;
                continue;
            }

            if let Err(e) = ctx.remove_recursive(file_path) {
                ctx.eprintln(&format!("rm: cannot remove '{file_path}': {e}"))
                    .await?;
                exit_code = 1;
                continue;
            }
        } else if let Err(e) = ctx.remove(file_path) {
            ctx.eprintln(&format!("rm: cannot remove '{file_path}': {e}"))
                .await?;
            exit_code = 1;
            continue;
        }

        if verbose {
            ctx.println(&format!("removed '{file_path}'")).await?;
        }
    }

    Ok(Exit::code(exit_code))
}
