//! `hegn doctor` and `hegn check`: host prerequisite reporting and
//! non-interactive tirith guardrail inspection.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::Config;
use crate::diagnostics::doctor;
use crate::security::{Decision, SecurityEngine};
use crate::vm::images;

pub(crate) fn check(command: &str, cwd: Option<&Path>, config: Config, json: bool) -> Result<i32> {
    let engine = SecurityEngine::new(config.security);
    let inspection = engine.inspect(command, cwd);
    if json {
        #[derive(Serialize)]
        struct Out<'a> {
            command: &'a str,
            decision: &'a str,
            destructive: bool,
            checkpoint_suggested: bool,
            findings: &'a [crate::security::Finding],
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&Out {
                command,
                decision: match inspection.decision {
                    Decision::Allow => "allow",
                    Decision::Warn => "warn",
                    Decision::Block => "block",
                },
                destructive: inspection.destructive,
                checkpoint_suggested: inspection.suggest_checkpoint,
                findings: &inspection.findings,
            })?
        );
    } else {
        match inspection.decision {
            Decision::Allow => {
                println!("ALLOW  {command}");
                if inspection.destructive {
                    println!("note: workspace guard flagged this command; checkpoint suggested");
                }
            }
            Decision::Warn => {
                println!("WARN   {command}");
                print_findings(&inspection);
            }
            Decision::Block => {
                println!("BLOCK  {command}");
                print_findings(&inspection);
            }
        }
    }
    Ok(inspection.decision.exit_code())
}

pub(crate) fn doctor(config: Config, json: bool, fix: bool) -> Result<i32> {
    if fix {
        let actions = doctor::setup_actions(&config);
        if actions.any() {
            if !json {
                eprintln!("hegn: preparing this host");
            }
            if actions.create_dirs {
                config.ensure_dirs()?;
            }
            if actions.init_config && config.write_default_if_missing()? && !json {
                eprintln!("hegn: wrote {}", Config::config_path().display());
            }
            if actions.init_profiles {
                let written =
                    crate::agent::profile::ProfileStore::new(&config).write_templates()?;
                if !json {
                    for path in &written {
                        eprintln!("hegn: wrote {}", path.display());
                    }
                }
            }
            if actions.download_images {
                images::download_missing(&config, false)?;
            }
            if actions.configure_network {
                let user = crate::network::invoking_user()
                    .context("could not determine the current user for TAP ownership")?;
                crate::network::setup_with_privilege(&user, &config.endpoint_ports())?;
            }
        }
    }
    let report = doctor::run(&config);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for check in &report.checks {
            println!(
                "{} {:<18} {}",
                if check.ok { "✓" } else { "✗" },
                check.name,
                check.detail
            );
        }
        if report.can_download() {
            println!("\nMissing VM images. Download with: hegn images download");
        }
        if report.can_setup() {
            println!(
                "Automatic setup available. Run: hegn doctor --fix (privileged steps use sudo)"
            );
        }
        println!("\nConfig: {}", Config::config_path().display());
    }
    Ok(if report.ok { 0 } else { 1 })
}

fn print_findings(inspection: &crate::security::Inspection) {
    for finding in &inspection.findings {
        println!(
            "  [{}] {}: {}",
            finding.severity.label(),
            finding.rule,
            finding.title
        );
        if !finding.description.is_empty() {
            println!("         {}", finding.description);
        }
    }
    if inspection.suggest_checkpoint {
        println!("  suggestion: create a checkpoint before running (`c` in the TUI)");
    }
}

/// Helper used by tests: run the security check and return the printed exit
/// code without touching a terminal.
pub fn check_command_for_test(config: &Config, command: &str) -> Result<i32> {
    check(command, None, config.clone(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_support::config;

    #[test]
    fn check_returns_tirith_exit_codes() {
        let (_dir, config) = config();
        assert_eq!(check_command_for_test(&config, "ls -la").unwrap(), 0);
        assert_eq!(
            check_command_for_test(&config, "rm -rf /workspace").unwrap(),
            1
        );
    }
}
