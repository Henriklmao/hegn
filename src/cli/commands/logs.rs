//! `hegn logs` handlers for the persistent serial-console log store.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::project::ProjectRegistry;
use crate::session::logs::{self, LogEntry, LogStore, sanitize_segment};
use crate::session::terminal::TerminalBuffer;
use crate::vm::registry::VmRegistry;

use crate::cli::args::LogCommands;
use crate::cli::output::human_size;

const MAX_PROJECT_SEGMENT: usize = 64;

pub(crate) fn logs_command(command: LogCommands, config: Config, json: bool) -> Result<i32> {
    let store = LogStore::new(config.logs_dir());
    match command {
        LogCommands::List { vm } => {
            let mut entries = store.list()?;
            if let Some(vm) = &vm {
                entries = filter_by_vm(&config, entries, vm)?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                match &vm {
                    Some(vm) => println!("No serial logs for VM '{vm}'."),
                    None => println!("No serial logs in {}.", store.root().display()),
                }
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
        LogCommands::Path { name, vm } => {
            let path = resolve_log(&store, name.as_deref(), vm.as_deref(), &config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&path)?);
            } else {
                println!("{}", path.display());
            }
            Ok(0)
        }
        LogCommands::Show {
            name,
            lines,
            raw,
            vm,
        } => {
            let path = resolve_log(&store, name.as_deref(), vm.as_deref(), &config)?;
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
            let registry = VmRegistry::new(config.clone());
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

/// Sanitized project-name segments that identify a VM's logs.
///
/// Serial log names carry the project name that booted the VM, not the VM name
/// (the same rootfs can host several projects).
fn vm_segments(config: &Config, vm: &str) -> Result<Vec<String>> {
    let registry = ProjectRegistry::load(&config.state_dir)?;
    let members = registry.members(vm);
    if members.is_empty() {
        bail!("VM '{vm}' has no projects; there are no logs to match");
    }
    Ok(members
        .iter()
        .map(|entry| sanitize_segment(&entry.name, MAX_PROJECT_SEGMENT))
        .collect())
}

fn entry_matches(entry: &LogEntry, segments: &[String]) -> bool {
    segments
        .iter()
        .any(|segment| entry.name.contains(&format!("-{segment}-")))
}

fn filter_by_vm(config: &Config, entries: Vec<LogEntry>, vm: &str) -> Result<Vec<LogEntry>> {
    let segments = vm_segments(config, vm)?;
    Ok(entries
        .into_iter()
        .filter(|entry| entry_matches(entry, &segments))
        .collect())
}

fn resolve_log(
    store: &LogStore,
    name: Option<&str>,
    vm: Option<&str>,
    config: &Config,
) -> Result<PathBuf> {
    let Some(vm) = vm else {
        return store.resolve(name);
    };
    let segments = vm_segments(config, vm)?;
    match name {
        Some(name) => {
            let path = store.resolve(Some(name))?;
            let file = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if !segments
                .iter()
                .any(|segment| file.contains(&format!("-{segment}-")))
            {
                bail!("serial log '{file}' does not belong to VM '{vm}'");
            }
            Ok(path)
        }
        None => store
            .list()?
            .into_iter()
            .find(|entry| entry_matches(entry, &segments))
            .map(|entry| entry.path)
            .with_context(|| format!("no serial logs for VM '{vm}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;
    use crate::cli::{Cli, Commands};
    use crate::config::Harness;
    use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
    use crate::vm::config::VmConfig;
    use chrono::Utc;
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
            logs_command(LogCommands::List { vm: None }, config.clone(), true).unwrap(),
            0
        );
        assert_eq!(
            logs_command(
                LogCommands::Path {
                    name: Some(log.file_name().unwrap().to_str().unwrap().to_string()),
                    vm: None,
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
                    vm: None,
                },
                config,
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn vm_filter_matches_logs_for_the_vms_members_only() {
        let (dir, config) = config();
        let vm = VmConfig::from_config(&config, "rust", Some(Harness::Shell));
        crate::vm::config::VmStore::new(&config).save(&vm).unwrap();
        let parser = dir.path().join("projects/parser");
        let other = dir.path().join("projects/other");
        std::fs::create_dir_all(&parser).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        for (name, path) in [("parser", &parser), ("other", &other)] {
            registry
                .upsert(ProjectEntry {
                    name: name.into(),
                    path: path.canonicalize().unwrap(),
                    vm: None,
                    read_only: false,
                    kind: ProjectKind::External,
                    parent: None,
                    managed: false,
                    hidden: false,
                    created_at: Utc::now(),
                })
                .unwrap();
        }
        registry.assign("parser", "rust", false).unwrap();

        let store = LogStore::new(config.logs_dir());
        let parser_log = store
            .create_session(Path::new("/projects/parser"), Utc::now(), 1)
            .unwrap();
        let other_log = store
            .create_session(Path::new("/projects/other"), Utc::now(), 2)
            .unwrap();
        let filtered = filter_by_vm(&config, store.list().unwrap(), "rust").unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, parser_log);
        assert_ne!(filtered[0].path, other_log);

        let resolved = resolve_log(&store, None, Some("rust"), &config).unwrap();
        assert_eq!(resolved, parser_log);
        let err = resolve_log(
            &store,
            Some(other_log.file_name().unwrap().to_str().unwrap()),
            Some("rust"),
            &config,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not belong"), "{err}");
    }

    #[test]
    fn vm_flag_has_a_hidden_instance_alias() {
        let cli = Cli::try_parse_from(["hegn", "logs", "list", "--vm", "rust"]).unwrap();
        match cli.command {
            Some(Commands::Logs {
                command: Some(LogCommands::List { vm }),
            }) => assert_eq!(vm.as_deref(), Some("rust")),
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from(["hegn", "logs", "list", "--instance", "rust"]).unwrap();
        match cli.command {
            Some(Commands::Logs {
                command: Some(LogCommands::List { vm }),
            }) => assert_eq!(vm.as_deref(), Some("rust")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
