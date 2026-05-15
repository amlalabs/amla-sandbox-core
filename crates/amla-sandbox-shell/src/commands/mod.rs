//! Built-in shell commands.
//!
//! Each command is an async function that takes a [`CmdContext`] and returns
//! an exit code via [`Exit`].
//!
//! # Adding a New Command
//!
//! 1. Create a new module file (e.g., `mycommand.rs`) with a `pub async fn run(ctx: CmdContext)`
//! 2. Add `mod mycommand;` to the module list below
//! 3. Add an entry to the `register_commands!` macro invocation
//!
//! That's it! The command will automatically appear in `help` output.

mod builtins;
mod cat;
mod cut;
mod date;
mod echo;
mod find;
mod grep;
mod head;
mod help;
mod jq;
mod ls;
mod mkdir;
mod node;
#[cfg(any(test, feature = "test-panic"))]
mod panic;
mod printf;
mod rm;
mod sh;
mod sleep;
mod sort;
mod tail;
mod tee;
mod test_cmd;
mod time;
mod tool;
mod tools;
mod touch;
mod tr;
mod uniq;
mod wc;
mod xxd;

use std::future::Future;
use std::pin::Pin;

use amla_scheduler::{Error, Exit};

use crate::CmdContext;

/// Type alias for command function return type.
pub type CommandResult = Result<Exit, Error>;

/// Type alias for async command functions.
pub type CommandFn = fn(CmdContext) -> Pin<Box<dyn Future<Output = CommandResult> + 'static>>;

/// Command metadata for help and registration.
#[derive(Debug, Clone, Copy)]
pub struct CommandInfo {
    /// Primary command name.
    pub name: &'static str,
    /// Optional aliases (e.g., "bash" for "sh").
    pub aliases: &'static [&'static str],
    /// Short description for help output.
    pub description: &'static str,
}

/// Macro to register all commands in one place.
///
/// Each entry has the format:
///   "name" [| "alias1" | "alias2"] => module::func, "description"
///
/// This generates:
/// - `get_command(name)` - looks up command by name or alias
/// - `all_commands()` - returns slice of CommandInfo for help
///
/// Note: Commands with conditional compilation (like test-only commands)
/// should be handled separately in `get_command_conditional()`.
macro_rules! register_commands {
    (
        $(
            $name:literal $(| $alias:literal)* => $module:ident :: $func:ident, $desc:literal
        );* $(;)?
    ) => {
        /// Look up a command by name (main registry).
        fn get_command_main(name: &str) -> Option<CommandFn> {
            match name {
                $(
                    $name $(| $alias)* => Some(|ctx| Box::pin($module::$func(ctx))),
                )*
                _ => None,
            }
        }

        /// Get metadata for all registered commands.
        pub fn all_commands() -> &'static [CommandInfo] {
            const COMMANDS: &[CommandInfo] = &[
                $(
                    CommandInfo {
                        name: $name,
                        aliases: &[$($alias),*],
                        description: $desc,
                    },
                )*
            ];
            COMMANDS
        }
    };
}

/// Look up conditionally-compiled commands.
#[inline]
fn get_command_conditional(name: &str) -> Option<CommandFn> {
    #[cfg(any(test, feature = "test-panic"))]
    if name == "panic" {
        return Some(|ctx| Box::pin(panic::run(ctx)));
    }
    let _ = name; // Suppress unused warning when no conditional commands
    None
}

/// Look up a command by name.
pub fn get_command(name: &str) -> Option<CommandFn> {
    get_command_main(name).or_else(|| get_command_conditional(name))
}

// =============================================================================
// COMMAND REGISTRY
// =============================================================================
// To add a new command:
// 1. Add `mod mycommand;` above
// 2. Add an entry here: "mycommand" => mycommand::run, "Description"
// =============================================================================

register_commands! {
    // -------------------------------------------------------------------------
    // Shell Builtins
    // -------------------------------------------------------------------------
    ":"       => builtins::colon,    "No-op command (always succeeds)";
    "cd"      => builtins::cd,       "Change working directory";
    "env"     => builtins::env,      "Print environment variables";
    "exit"    => builtins::exit,     "Exit the shell with optional code";
    "export"  => builtins::export,   "Set environment variable";
    "false"   => builtins::cmd_false, "Exit with failure status";
    "pwd"     => builtins::pwd,      "Print working directory";
    "set"     => builtins::set,      "Set shell options";
    "true"    => builtins::cmd_true, "Exit with success status";
    "unset"   => builtins::unset,    "Remove environment variable";

    // -------------------------------------------------------------------------
    // File Operations
    // -------------------------------------------------------------------------
    "cat"     => cat::run,           "Concatenate and print files";
    "find"    => find::run,          "Search for files in directory hierarchy";
    "ls"      => ls::run,            "List directory contents";
    "mkdir"   => mkdir::run,         "Create directories";
    "rm"      => rm::run,            "Remove files or directories";
    "touch"   => touch::run,         "Create empty file or update timestamp";

    // -------------------------------------------------------------------------
    // Text Processing
    // -------------------------------------------------------------------------
    "cut"     => cut::run,           "Extract fields from lines";
    "echo"    => echo::run,          "Print arguments to stdout";
    "grep"    => grep::run,          "Search for patterns in text";
    "head"    => head::run,          "Output first lines of files";
    "jq"      => jq::run,            "Process JSON with jq expressions";
    "printf"  => printf::run,        "Format and print data";
    "sort"    => sort::run,          "Sort lines of text";
    "tail"    => tail::run,          "Output last lines of files";
    "tee"     => tee::run,           "Read stdin, write to stdout and files";
    "tr"      => tr::run,            "Translate or delete characters";
    "uniq"    => uniq::run,          "Report or filter repeated lines";
    "wc"      => wc::run,            "Count lines, words, and bytes";
    "xxd"     => xxd::run,           "Hex dump or reverse";

    // -------------------------------------------------------------------------
    // Conditionals & Testing
    // -------------------------------------------------------------------------
    "test" | "[" => test_cmd::run,   "Evaluate conditional expression";

    // -------------------------------------------------------------------------
    // Shell & Scripting
    // -------------------------------------------------------------------------
    "sh" | "bash" => sh::run,        "Execute shell commands";

    // -------------------------------------------------------------------------
    // Time & Scheduling
    // -------------------------------------------------------------------------
    "date"    => date::run,          "Print or set system date/time";
    "sleep"   => sleep::run,         "Pause for specified duration";
    "time"    => time::run,          "Measure command execution time";

    // -------------------------------------------------------------------------
    // Runtime & Tools
    // -------------------------------------------------------------------------
    "help"    => help::run,          "Show available commands";
    "node"    => node::run,          "Execute JavaScript code";
    "tool"    => tool::run,          "Invoke external tools";
    "tools"   => tools::run,         "Search and discover tools"

    // Note: Test-only commands (like "panic") are registered in
    // get_command_conditional() since the macro doesn't support #[cfg]
}

/// List all available command names (for compatibility).
pub fn available_commands() -> impl Iterator<Item = &'static str> {
    all_commands()
        .iter()
        .flat_map(|cmd| std::iter::once(cmd.name).chain(cmd.aliases.iter().copied()))
}
