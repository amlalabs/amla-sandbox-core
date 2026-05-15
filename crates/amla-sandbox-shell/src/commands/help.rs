//! help - Display available commands and their descriptions.

use amla_scheduler::Exit;

use super::{CommandResult, all_commands};
use crate::CmdContext;

/// help [command]
///
/// Display help information.
///
/// With no arguments, lists all available commands.
/// With a command name, shows detailed help for that command.
pub async fn run(ctx: CmdContext) -> CommandResult {
    let args: Vec<&str> = ctx.argv.iter().skip(1).map(String::as_str).collect();

    if args.is_empty() {
        // List all commands
        print_all_commands(&ctx).await?;
    } else {
        // Show help for specific command
        let cmd_name = args[0];
        print_command_help(&ctx, cmd_name).await?;
    }

    Ok(Exit::success())
}

/// Print a list of all available commands grouped by category.
async fn print_all_commands(ctx: &CmdContext) -> Result<(), crate::io_handle::IoError> {
    ctx.println("Available commands:\n").await?;

    // Group commands by category based on their position in the registry
    let commands = all_commands();

    // Find the longest command name for alignment
    let max_name_len = commands
        .iter()
        .map(|c| {
            if c.aliases.is_empty() {
                c.name.len()
            } else {
                // "name, alias1, alias2"
                c.name.len() + c.aliases.iter().map(|a| a.len() + 2).sum::<usize>()
            }
        })
        .max()
        .unwrap_or(10);

    // Print each command with description
    for cmd in commands {
        let name_with_aliases = if cmd.aliases.is_empty() {
            cmd.name.to_string()
        } else {
            format!("{}, {}", cmd.name, cmd.aliases.join(", "))
        };

        // Pad to align descriptions
        let padding = max_name_len.saturating_sub(name_with_aliases.len()) + 2;
        let spaces = " ".repeat(padding);

        ctx.println(&format!("  {name_with_aliases}{spaces}{}", cmd.description))
            .await?;
    }

    ctx.println("\nUse 'help <command>' for more information on a specific command.")
        .await?;
    ctx.println("Use '<command> --help' for detailed usage of most commands.")
        .await?;

    Ok(())
}

/// Print help for a specific command.
async fn print_command_help(ctx: &CmdContext, name: &str) -> Result<(), crate::io_handle::IoError> {
    let commands = all_commands();

    // Find the command by name or alias
    let cmd = commands
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name));

    match cmd {
        Some(info) => {
            ctx.println(&format!("{}: {}", info.name, info.description))
                .await?;

            if !info.aliases.is_empty() {
                ctx.println(&format!("Aliases: {}", info.aliases.join(", ")))
                    .await?;
            }

            ctx.println(&format!("\nRun '{} --help' for detailed usage.", info.name))
                .await?;
        }
        None => {
            ctx.eprintln(&format!("help: unknown command: {name}"))
                .await?;
            ctx.eprintln("Run 'help' for a list of available commands.")
                .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_commands_returns_entries() {
        let cmds = all_commands();
        assert!(!cmds.is_empty());

        // Should have help command
        assert!(cmds.iter().any(|c| c.name == "help"));

        // Should have echo command
        assert!(cmds.iter().any(|c| c.name == "echo"));

        // sh should have bash alias
        let sh = cmds.iter().find(|c| c.name == "sh").unwrap();
        assert!(sh.aliases.contains(&"bash"));
    }

    #[test]
    fn all_commands_have_descriptions() {
        for cmd in all_commands() {
            assert!(
                !cmd.description.is_empty(),
                "Command '{}' has empty description",
                cmd.name
            );
        }
    }
}
