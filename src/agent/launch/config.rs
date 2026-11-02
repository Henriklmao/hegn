//! Launch configuration schema: the global `[launch]` table and its named
//! commands.
//!
//! Validation rejects reserved or duplicate selectors and refuses command
//! strings that could not be typed into the guest safely.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::command::safe_selector;
use super::integration::Integration;

/// Built-in selector that resolves to the instance's harness executable.
pub const HARNESS: &str = "harness";
/// Built-in selector that only opens a maintenance shell.
pub const SHELL: &str = "shell";

/// A named command that can be selected from the launcher or CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct LaunchCommand {
    pub name: String,
    /// Shell command typed into the guest. Empty means a maintenance shell.
    pub command: String,
    /// Per-command integration override.
    pub integration: Option<Integration>,
    /// Keep the attach status bar for this command.
    pub bar: bool,
}

/// Global `[launch]` configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LaunchConfig {
    /// Command name launched after Enter boots a VM. `harness` and `shell` are
    /// always available; other names refer to `commands`.
    pub default: Option<String>,
    /// Default integration for commands that do not set one.
    pub integration: Integration,
    /// Launch the default command after the TUI starts a VM with Enter.
    pub auto_start: bool,
    pub commands: Vec<LaunchCommand>,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            default: Some(HARNESS.to_string()),
            integration: Integration::Auto,
            auto_start: true,
            commands: Vec::new(),
        }
    }
}

impl LaunchConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = &self.default {
            safe_selector(name)?;
            if !self.commands.iter().any(|command| &command.name == name)
                && !matches!(name.as_str(), HARNESS | SHELL)
            {
                bail!("launch.default '{name}' is not a defined launch command");
            }
        }
        let mut seen = std::collections::HashSet::new();
        for command in &self.commands {
            safe_selector(&command.name)?;
            if matches!(command.name.as_str(), HARNESS | SHELL) {
                bail!(
                    "launch command name '{}' is reserved; use '{}' or '{}' instead",
                    command.name,
                    HARNESS,
                    SHELL
                );
            }
            if !seen.insert(command.name.as_str()) {
                bail!("duplicate launch command '{}'", command.name);
            }
            if command.command.len() > 4096 {
                bail!("launch command '{}' is too long", command.name);
            }
            if command.command.chars().any(char::is_control) {
                bail!(
                    "launch command '{}' must not contain control characters or newlines",
                    command.name
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn invalid_command_names_and_newlines_are_rejected() {
        let mut config = Config::default();
        config.launch.commands.push(LaunchCommand {
            name: "bad name".into(),
            command: "true".into(),
            ..LaunchCommand::default()
        });
        assert!(config.launch.validate().is_err());

        let mut config = Config::default();
        config.launch.commands.push(LaunchCommand {
            name: "bad".into(),
            command: "echo\nrm -rf /".into(),
            ..LaunchCommand::default()
        });
        assert!(config.launch.validate().is_err());
    }
}
