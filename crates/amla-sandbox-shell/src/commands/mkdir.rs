//! mkdir - make directories

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;

use super::CommandResult;

/// mkdir [-p] [-v] dir...
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut parents = false;
    let mut verbose = false;
    let mut dirs: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('p'))) => parents = true,
            Ok(Some(lexopt::Arg::Long("parents"))) => parents = true,
            Ok(Some(lexopt::Arg::Short('v'))) => verbose = true,
            Ok(Some(lexopt::Arg::Long("verbose"))) => verbose = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                dirs.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if dirs.is_empty() {
        ctx.eprintln("mkdir: missing operand").await?;
        return Ok(Exit::code(1));
    }

    let mut exit_code = 0;

    for dir_path in &dirs {
        let result = if parents {
            ctx.mkdir_p(dir_path)
        } else {
            ctx.mkdir(dir_path)
        };

        if let Err(e) = result {
            ctx.eprintln(&format!("mkdir: cannot create directory '{dir_path}': {e}"))
                .await?;
            exit_code = 1;
            continue;
        }

        if verbose {
            ctx.println(&format!("mkdir: created directory '{dir_path}'"))
                .await?;
        }
    }

    Ok(Exit::code(exit_code))
}
