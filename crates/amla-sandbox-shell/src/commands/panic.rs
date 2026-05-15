//! Test-only panic command.
//!
//! This command intentionally panics to test panic recovery.
//! Only available in test builds or when `test-panic` feature is enabled.

use amla_scheduler::{Error, Exit};

use crate::CmdContext;

/// Intentionally panic with an optional message.
///
/// Usage: `panic [message]`
///
/// This command is only available in test builds and is used to verify
/// that the runtime correctly handles panics without affecting other runtimes.
pub async fn run(ctx: CmdContext) -> Result<Exit, Error> {
    let message = if ctx.argv.len() > 1 {
        ctx.argv[1..].join(" ")
    } else {
        "test panic".to_string()
    };

    panic!("{message}");
}
