//! `hegn logs` handlers for the persistent serial-console log store.

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;

use crate::config::Config;
use crate::session::logs::{self, LogStore};
use crate::session::terminal::TerminalBuffer;
use crate::vm::instance::InstanceRegistry;

use crate::cli::args::LogCommands;
use crate::cli::output::human_size;

pub(crate) fn logs_command(command: LogCommands, config: Config, json: bool) -> Result<i32> {
    let store = LogStore::new(config.logs_dir());
    match command {
        LogCommands::List => {
            let entries = store.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("No serial logs in {}.", store.root().display());
            } else {
                println!("{:<64} {:>12}  MODIFIED", "NAME", "SIZE");
                for entry in entries {
                    let modified = entry
                        .modified
                        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "{:<64} {:>12}  {}",
                        entry.name,
                        human_size(entry.size_bytes),
                        modified
                    );
                }
            }
            Ok(0)
        }
        LogCommands::Path { name } => {
            let path = store.resolve(name.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&path)?);
            } else {
                println!("{}", path.display());
            }
            Ok(0)
        }
        LogCommands::Show { name, lines, raw } => {
            let path = store.resolve(name.as_deref())?;
            let bytes = logs::tail_bytes(&path, lines)?;
            let mut out = std::io::stdout();
            if raw {
                out.write_all(&bytes)?;
            } else {
                // The default path strips ANSI/control sequences from guest
                // output so a log cannot reprogram or exploit the terminal.
                let mut buffer = TerminalBuffer::default();
                buffer.push_bytes(&bytes);
                let mut lines = buffer.lines();
                if lines.last().is_some_and(String::is_empty) {
                    lines.pop();
                }
                for line in lines {
                    // `TerminalBuffer` removed ANSI/control sequences; also
                    // neutralize bidi overrides so logs cannot spoof prompts.
                    writeln!(out, "{}", logs::sanitize_label(&line))?;
                }
            }
            out.flush()?;
            Ok(0)
        }
        LogCommands::Prune { keep } => {
            // Never unlink a log a running instance is still writing to.
            let registry = InstanceRegistry::new(config.clone());
            let protected: Vec<PathBuf> = registry
                .summaries()?
                .into_iter()
                .filter_map(|instance| instance.serial_log)
                .collect();
            let removed = store.prune_except_many(keep, &protected)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"removed": removed, "kept": keep.max(1)})
                );
            } else {
                println!("Removed {removed} log(s); kept the newest {}.", keep.max(1));
            }
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;
    use crate::cli::{Cli, Commands};
    use clap::Parser;
    use std::path::Path;

    #[test]
    fn logs_defaults_to_list_and_rejects_traversal() {
        let (_dir, config) = config();
        let cli = Cli::try_parse_from(["hegn", "logs"]).unwrap();
        match cli.command {
            Some(Commands::Logs { command: None }) => {}
            other => panic!("unexpected {other:?}"),
        }

        let store = LogStore::new(config.logs_dir());
        let log = store
            .create_session(Path::new("/projects/demo"), chrono::Utc::now(), 7)
            .unwrap();
        assert_eq!(
            logs_command(LogCommands::List, config.clone(), true).unwrap(),
            0
        );
        assert_eq!(
            logs_command(
                LogCommands::Path {
                    name: Some(log.file_name().unwrap().to_str().unwrap().to_string()),
                },
                config.clone(),
                true,
            )
            .unwrap(),
            0
        );
        assert!(
            logs_command(
                LogCommands::Path {
                    name: Some("../session-evil.log".into()),
                },
                config,
                true,
            )
            .is_err()
        );
    }
}
