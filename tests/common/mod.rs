//! Shared subprocess setup for the integration tests.

use std::process::Command;

/// The briefing binary with every `BRIEFING_*` variable dropped from the inherited environment,
/// so a developer's own settings, hooks, or hub never reach a test subprocess. Callers add back
/// the variables they are exercising.
pub fn briefing_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_briefing"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("BRIEFING_") {
            command.env_remove(key);
        }
    }
    command
}
