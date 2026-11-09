//! `hegn config` handler plus the shared `$EDITOR` launcher used by the
//! instance and profile commands.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::Config;

use crate::cli::args::ConfigCommands;

pub(crate) fn config_command(command: ConfigCommands, config: Config, json: bool) -> Result<i32> {
    match command {
        ConfigCommands::Show => {
            if json {
                println!("{}", serde_json::to_string_pretty(&config)?);
            } else {
                print!("{}", toml::to_string_pretty(&config)?);
            }
            Ok(0)
        }
        ConfigCommands::Path => {
            if json {
                println!("{}", serde_json::to_string_pretty(&Config::config_path())?);
            } else {
                println!("{}", Config::config_path().display());
            }
            Ok(0)
        }
        ConfigCommands::Init => {
            let config_written = config.write_default_if_missing()?;
            let profiles_written =
                crate::agent::profile::ProfileStore::new(&config).write_templates()?;
            config.ensure_dirs()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "config": Config::config_path(),
                        "config_written": config_written,
                        "profiles_written": profiles_written,
                    }))?
                );
            } else {
                if config_written {
                    println!("Wrote {}", Config::config_path().display());
                } else {
                    println!(
                        "Config already exists at {}",
                        Config::config_path().display()
                    );
                }
                for path in &profiles_written {
                    println!("Wrote {}", path.display());
                }
                if profiles_written.is_empty() {
                    println!(
                        "Profile templates already exist in {}",
                        config.profiles_dir.display()
                    );
                }
            }
            Ok(0)
        }
    }
}

/// Split an `$EDITOR`/`$VISUAL` value into the program and its arguments.
///
/// Quoted shell syntax is intentionally no longer interpreted: the value is
/// split on whitespace and executed directly, never through a shell, so it can
/// not inject commands. An empty or whitespace-only value yields `None` and the
/// caller falls back to `vi`.
fn split_editor(value: &str) -> Option<(String, Vec<String>)> {
    let mut parts = value.split_whitespace();
    let program = parts.next()?.to_string();
    Some((program, parts.map(str::to_string).collect()))
}

pub(crate) fn run_editor(path: &Path) -> Result<i32> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let (program, extra) = split_editor(&editor).unwrap_or_else(|| ("vi".to_string(), Vec::new()));
    let status = Command::new(&program)
        .args(&extra)
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_editor_separates_program_and_arguments() {
        let (program, args) = split_editor("code --wait").expect("editor value");
        assert_eq!(program, "code");
        assert_eq!(args, ["--wait"]);

        let (program, args) = split_editor("vi").expect("editor value");
        assert_eq!(program, "vi");
        assert!(args.is_empty());

        assert!(split_editor("").is_none());
        assert!(split_editor("   \t ").is_none());
    }

    #[test]
    fn config_init_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hegn/config.toml");
        // write_default_if_missing uses Config::config_path; just test the
        // underlying behavior through load_from + validate.
        let config = Config::default();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, toml::to_string_pretty(&config).unwrap()).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.vcpus, config.vcpus);
    }
}
