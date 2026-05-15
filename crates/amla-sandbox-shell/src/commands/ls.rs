//! ls - list directory contents

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;

use super::CommandResult;

/// ls [-l] [-a] [-1] [-h] [path...]
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut long_format = false;
    let mut show_all = false;
    let mut one_per_line = false;
    let mut human_readable = false;
    let mut paths: SmallVec<[String; 4]> = SmallVec::new();

    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('l'))) => long_format = true,
            Ok(Some(lexopt::Arg::Short('a'))) => show_all = true,
            Ok(Some(lexopt::Arg::Short('1'))) => one_per_line = true,
            Ok(Some(lexopt::Arg::Short('h'))) => human_readable = true,
            Ok(Some(lexopt::Arg::Value(val))) => {
                paths.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if paths.is_empty() {
        paths.push(".".to_string());
    }

    let multiple_paths = paths.len() > 1;
    let mut first = true;

    for path_arg in &paths {
        let path = ctx.resolve_path(path_arg);

        if multiple_paths {
            if !first {
                ctx.stdout_write_all(b"\n").await?;
            }
            ctx.println(&format!("{path_arg}:")).await?;
            first = false;
        }

        if !ctx.exists(&path) {
            ctx.eprintln(&format!(
                "ls: cannot access '{path_arg}': No such file or directory"
            ))
            .await?;
            return Ok(Exit::code(2));
        }

        if ctx.is_file(&path) {
            let name = path_arg.rsplit('/').next().unwrap_or(path_arg);
            if long_format {
                let line = format_long_entry(&ctx, &path, name, human_readable);
                ctx.println(&line).await?;
            } else {
                ctx.println(name).await?;
            }
            continue;
        }

        match ctx.list_dir(&path) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .into_iter()
                    .filter(|e| show_all || !e.name.starts_with('.'))
                    .map(|e| e.name)
                    .collect();
                names.sort();

                if long_format {
                    for name in &names {
                        let entry_path = if path == "/" {
                            format!("/{name}")
                        } else {
                            format!("{path}/{name}")
                        };
                        let line = format_long_entry(&ctx, &entry_path, name, human_readable);
                        ctx.println(&line).await?;
                    }
                } else if one_per_line {
                    for name in &names {
                        ctx.println(name).await?;
                    }
                } else {
                    let output = names.join("  ");
                    if !output.is_empty() {
                        ctx.println(&output).await?;
                    }
                }
            }
            Err(e) => {
                ctx.eprintln(&format!("ls: cannot open directory '{path_arg}': {e}"))
                    .await?;
                return Ok(Exit::code(1));
            }
        }
    }

    Ok(Exit::success())
}

fn format_long_entry(ctx: &CmdContext, path: &str, name: &str, human_readable: bool) -> String {
    let stat = ctx.stat(path).ok();
    let is_dir = stat.as_ref().is_some_and(|s| s.is_dir);
    let size = stat.as_ref().map_or(0, |s| s.size);

    let type_char = if is_dir { 'd' } else { '-' };
    let perms = if is_dir { "rwxr-xr-x" } else { "rw-r--r--" };

    let size_str = if human_readable {
        format_human_size(size)
    } else {
        format!("{size:>8}")
    };

    format!("{type_char}{perms} 1 user user {size_str} Jan  1 00:00 {name}")
}

fn format_human_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T"];

    let mut size_f = size as f64;
    let mut unit_idx = 0;

    while size_f >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size_f /= 1024.0;
        unit_idx += 1;
    }

    if unit_idx == 0 {
        format!("{:>4}{}", size, UNITS[unit_idx])
    } else {
        format!("{:>4.1}{}", size_f, UNITS[unit_idx])
    }
}
