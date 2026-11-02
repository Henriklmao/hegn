//! Serial-console integrations and the host-side actions that open them.
//!
//! Host-side integrations only decide where the serial console is displayed;
//! the launched command always runs inside the VM. `auto` prefers herdr, then
//! tmux, then the current terminal.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::command::LaunchSpec;

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
    /// Cyclable order for the project settings/launch pickers.
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Embedded,
        Self::External,
        Self::Tmux,
        Self::Herdr,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Embedded => "embedded",
            Self::External => "external",
            Self::Tmux => "tmux",
            Self::Herdr => "herdr",
        }
    }

    /// One-line explanation for menus and the help screen.
    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => "herdr → tmux → current terminal",
            Self::Embedded => "TUI terminal pane (scrollback)",
            Self::External => "suspend the TUI, attach this terminal",
            Self::Tmux => "new tmux window running hegn attach",
            Self::Herdr => "new herdr pane running hegn attach",
        }
    }

    /// Whether the host can honour this integration right now.
    pub fn available(self) -> bool {
        match self.resolve() {
            Self::Tmux => env::var_os("TMUX").is_some(),
            Self::Herdr => env::var("HERDR_ENV").is_ok_and(|value| value == "1"),
            _ => true,
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
    use crate::agent::launch::command::LaunchSpec;

    #[test]
    fn integration_menu_lists_every_choice() {
        assert_eq!(
            Integration::ALL.map(Integration::label),
            ["auto", "embedded", "external", "tmux", "herdr"]
        );
        for integration in Integration::ALL {
            assert!(!integration.description().is_empty());
        }
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
