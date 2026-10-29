//! Modular launch integrations for the agent or maintenance command.
//!
//! The embedded project terminal is a scrollback, not a full-screen terminal
//! emulator. Agent TUIs therefore work best in a real terminal: hegn can hand
//! the serial console to `tmux`, `herdr`, or the current terminal, or keep the
//! command inside the embedded TUI terminal.
//!
//! The guest command itself is always executed inside the VM. Host-side
//! integrations only decide where the serial console is displayed; they never
//! run an agent on the host.
//!
//! Adding an integration means adding an [`Integration`] variant, mapping it to
//! a [`LaunchAction`] in [`action`], and executing that action in the TUI/CLI.
//! The command, configuration and validation stay shared.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::config::{Config, Harness};

/// Built-in selector that resolves to the instance's harness executable.
pub const HARNESS: &str = "harness";
/// Built-in selector that only opens a maintenance shell.
pub const SHELL: &str = "shell";

/// Where the serial console (and therefore the launched command) is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
pub enum Integration {
    /// Prefer herdr, then tmux, then the current terminal.
    #[default]
    Auto,
    /// Keep the command in the TUI's embedded terminal.
    Embedded,
    /// Suspend the TUI and hand the current terminal to `hegn attach`.
    External,
    /// Open a new tmux window running `hegn attach`.
    Tmux,
    /// Open a new herdr pane running `hegn attach`.
    Herdr,
}

impl Integration {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Embedded => "embedded",
            Self::External => "external",
            Self::Tmux => "tmux",
            Self::Herdr => "herdr",
        }
    }

    /// Resolve [`Integration::Auto`] for the current host session.
    pub fn resolve(self) -> Self {
        match self {
            Self::Auto if env::var("HERDR_ENV").is_ok_and(|value| value == "1") => Self::Herdr,
            Self::Auto if env::var_os("TMUX").is_some() => Self::Tmux,
            Self::Auto => Self::External,
            other => other,
        }
    }
}

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

/// A fully resolved launch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub name: String,
    pub command: String,
    pub integration: Integration,
    pub bar: bool,
}

impl LaunchSpec {
    pub fn is_shell(&self) -> bool {
        self.command.trim().is_empty()
    }
}

/// Resolve a named command from config, the project override, or the default.
pub fn resolve(
    config: &Config,
    harness: Harness,
    selector: Option<&str>,
    integration: Option<Integration>,
) -> Result<LaunchSpec> {
    let selected = selector
        .map(str::to_string)
        .or_else(|| config.launch.default.clone())
        .context("no launch command configured; set launch.default or pass --command")?;
    let mut spec = match selected.as_str() {
        SHELL => LaunchSpec {
            name: SHELL.to_string(),
            command: String::new(),
            integration: config.launch.integration,
            bar: true,
        },
        HARNESS => LaunchSpec {
            name: harness.label().to_string(),
            command: if harness == Harness::Shell {
                String::new()
            } else {
                harness.command().to_string()
            },
            integration: config.launch.integration,
            bar: harness == Harness::Shell,
        },
        name => {
            let command = config
                .launch
                .commands
                .iter()
                .find(|command| command.name == name)
                .with_context(|| {
                    let available: Vec<&str> = config
                        .launch
                        .commands
                        .iter()
                        .map(|command| command.name.as_str())
                        .collect();
                    format!(
                        "unknown launch command '{name}' (available: {}, {HARNESS}, {SHELL})",
                        if available.is_empty() {
                            "none".to_string()
                        } else {
                            available.join(", ")
                        }
                    )
                })?;
            LaunchSpec {
                name: command.name.clone(),
                command: command.command.trim().to_string(),
                integration: command.integration.unwrap_or(config.launch.integration),
                bar: command.bar || command.command.trim().is_empty(),
            }
        }
    };
    if let Some(integration) = integration {
        spec.integration = integration;
    }
    if spec.is_shell() {
        spec.bar = true;
    }
    Ok(spec)
}

/// Host-side action needed to show the launched command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchAction {
    /// Send the command to the embedded console.
    Embedded { command: String },
    /// Suspend the TUI and attach the current terminal.
    External {
        instance: String,
        command: Option<String>,
        bar: bool,
    },
    /// Run a host command that opens another terminal.
    Spawn { program: String, args: Vec<String> },
    /// Ask herdr for a pane, then run the attach command in it.
    Herdr { pane_command: String, cwd: PathBuf },
}

/// Translate a resolved spec into the action the TUI/CLI should perform.
pub fn action(
    spec: &LaunchSpec,
    instance: &str,
    project: &Path,
    hegn: &Path,
) -> Result<LaunchAction> {
    let integration = spec.integration.resolve();
    let command = (!spec.is_shell()).then(|| spec.command.clone());
    match integration {
        Integration::Embedded => Ok(LaunchAction::Embedded {
            command: spec.command.clone(),
        }),
        Integration::External => Ok(LaunchAction::External {
            instance: instance.to_string(),
            command,
            bar: spec.bar,
        }),
        Integration::Tmux => {
            if env::var_os("TMUX").is_none() {
                bail!("tmux launch integration requires running hegn inside tmux");
            }
            let pane_command = attach_shell_command(hegn, instance, command.as_deref(), spec.bar);
            Ok(LaunchAction::Spawn {
                program: "tmux".into(),
                args: vec![
                    "new-window".into(),
                    "-n".into(),
                    format!("hegn-{instance}"),
                    "-c".into(),
                    project.display().to_string(),
                    pane_command,
                ],
            })
        }
        Integration::Herdr => {
            if env::var("HERDR_ENV").ok().as_deref() != Some("1") {
                bail!("herdr launch integration requires running hegn inside herdr");
            }
            Ok(LaunchAction::Herdr {
                pane_command: attach_shell_command(hegn, instance, command.as_deref(), spec.bar),
                cwd: project.to_path_buf(),
            })
        }
        Integration::Auto => unreachable!("Auto is resolved before building an action"),
    }
}

/// Build the `hegn attach` command used inside tmux/herdr panes.
pub fn attach_shell_command(
    hegn: &Path,
    instance: &str,
    command: Option<&str>,
    bar: bool,
) -> String {
    let mut argv = vec![hegn.to_string_lossy().into_owned()];
    // tmux/herdr panes start from their own server environment, so pass the
    // config explicitly instead of relying on environment inheritance.
    if let Some(config) = env::var_os("HEGN_CONFIG") {
        argv.push("--config".into());
        argv.push(config.to_string_lossy().into_owned());
    }
    argv.push("attach".into());
    argv.push(instance.to_string());
    if let Some(command) = command {
        argv.push("--command".into());
        argv.push(command.to_string());
    }
    if !bar {
        argv.push("--no-bar".into());
    }
    argv.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_./:=@%+".contains(ch))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn safe_selector(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("launch command names must be 1-64 characters");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        bail!("launch command names may only contain letters, digits, '-' and '_'");
    }
    Ok(())
}

/// Create a herdr pane and run the attach command in it.
pub fn spawn_herdr(pane_command: &str, cwd: &Path) -> Result<()> {
    let output = Command::new("herdr")
        .args([
            "pane",
            "split",
            "--current",
            "--direction",
            "right",
            "--focus",
        ])
        .arg("--cwd")
        .arg(cwd)
        .output()
        .context("failed to run herdr; is it installed?")?;
    if !output.status.success() {
        bail!(
            "herdr pane split failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("herdr returned invalid JSON for the new pane")?;
    let pane = value
        .pointer("/result/pane/pane_id")
        .and_then(serde_json::Value::as_str)
        .context("herdr did not return a pane id")?;
    let output = Command::new("herdr")
        .args(["pane", "run", pane, pane_command])
        .output()
        .context("failed to run the launch command in herdr")?;
    if !output.status.success() {
        bail!(
            "herdr pane run failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn harness_resolves_to_the_project_agent() {
        let spec = resolve(&test_config(), Harness::Opencode, None, None).unwrap();
        assert_eq!(spec.command, "opencode");
        assert!(!spec.bar);
        assert_eq!(spec.integration, Integration::Auto);
    }

    #[test]
    fn shell_resolves_to_an_empty_maintenance_command() {
        let spec = resolve(&test_config(), Harness::Shell, None, None).unwrap();
        assert!(spec.is_shell());
        assert!(spec.bar);
    }

    #[test]
    fn named_commands_can_override_the_integration_and_bar() {
        let mut config = test_config();
        config.launch.commands.push(LaunchCommand {
            name: "review".into(),
            command: "opencode --model test".into(),
            integration: Some(Integration::Tmux),
            bar: true,
        });
        let spec = resolve(&config, Harness::Opencode, Some("review"), None).unwrap();
        assert_eq!(spec.name, "review");
        assert_eq!(spec.command, "opencode --model test");
        assert_eq!(spec.integration, Integration::Tmux);
        assert!(spec.bar);
    }

    #[test]
    fn unknown_commands_are_rejected() {
        let err = resolve(&test_config(), Harness::Opencode, Some("missing"), None).unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown launch command"),
            "{err:#}"
        );
    }

    #[test]
    fn invalid_command_names_and_newlines_are_rejected() {
        let mut config = test_config();
        config.launch.commands.push(LaunchCommand {
            name: "bad name".into(),
            command: "true".into(),
            ..LaunchCommand::default()
        });
        assert!(config.launch.validate().is_err());

        let mut config = test_config();
        config.launch.commands.push(LaunchCommand {
            name: "bad".into(),
            command: "echo\nrm -rf /".into(),
            ..LaunchCommand::default()
        });
        assert!(config.launch.validate().is_err());
    }

    #[test]
    fn external_actions_carry_the_command_and_bar_setting() {
        let spec = LaunchSpec {
            name: "opencode".into(),
            command: "opencode".into(),
            integration: Integration::External,
            bar: false,
        };
        let action = action(
            &spec,
            "demo",
            Path::new("/tmp/demo"),
            Path::new("/bin/hegn"),
        )
        .unwrap();
        assert_eq!(
            action,
            LaunchAction::External {
                instance: "demo".into(),
                command: Some("opencode".into()),
                bar: false
            }
        );
    }

    #[test]
    fn attach_shell_commands_are_quoted() {
        let command = attach_shell_command(
            Path::new("/usr/bin/hegn"),
            "demo",
            Some("opencode --model 'x y'"),
            false,
        );
        assert!(command.contains("'/usr/bin/hegn'") || command.contains("/usr/bin/hegn"));
        assert!(command.contains("--no-bar"));
        assert!(command.contains("opencode --model"));
        assert!(command.contains("'\"'\"'"));
    }
}
