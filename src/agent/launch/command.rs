//! Resolve a named launch selector into a fully expanded [`LaunchSpec`].
//!
//! Selectors are either the built-in `harness`/`shell` names or a validated
//! command from the `[launch]` configuration. Command strings are trimmed but
//! otherwise passed through unchanged; they run inside the VM.

use anyhow::{Context, Result, bail};

use crate::config::{Config, Harness};

use super::config::{HARNESS, SHELL};
use super::integration::Integration;

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

pub(super) fn safe_selector(name: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::launch::config::LaunchCommand;
    use crate::agent::launch::integration::Integration;
    use crate::config::{Config, Harness};

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
    fn selectors_reject_newlines_shell_metacharacters_and_overlong_names() {
        for bad in [
            "",
            "bad name",
            "bad\nname",
            "bad\rname",
            "bad;reboot",
            "bad$(id)",
            "bad`id`",
            "bad|pipe",
            "bad&background",
            "bad/path",
            "bad.name",
            "bad:name",
        ] {
            assert!(safe_selector(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(safe_selector(&"a".repeat(65)).is_err());
    }

    #[test]
    fn selectors_accept_letters_digits_dashes_and_underscores() {
        for good in ["a", "A1", "review-2", "my_command", &"a".repeat(64)] {
            assert!(safe_selector(good).is_ok(), "{good:?} must be accepted");
        }
    }

    #[test]
    fn named_commands_are_trimmed_but_inner_spacing_is_preserved() {
        let mut config = test_config();
        config.launch.commands.push(LaunchCommand {
            name: "review".into(),
            command: "  opencode   --model test  ".into(),
            ..LaunchCommand::default()
        });
        let spec = resolve(&config, Harness::Opencode, Some("review"), None).unwrap();
        assert_eq!(spec.command, "opencode   --model test");
        assert!(!spec.is_shell());
        assert!(!spec.bar);
    }

    #[test]
    fn a_whitespace_only_named_command_is_a_maintenance_shell() {
        let mut config = test_config();
        config.launch.commands.push(LaunchCommand {
            name: "blank".into(),
            command: "   ".into(),
            ..LaunchCommand::default()
        });
        let spec = resolve(&config, Harness::Opencode, Some("blank"), None).unwrap();
        assert!(spec.is_shell());
        assert!(spec.bar);
    }
}
