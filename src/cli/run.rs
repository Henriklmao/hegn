//! Top-level entry point and subcommand dispatch. [`run`] normalises global
//! flags before handing the parsed command to the matching handler.

use anyhow::Result;

use crate::config::Config;
use crate::diagnostics::benchmark;
use crate::project;

use super::args::{Cli, Commands, LogCommands};
use super::commands::config::config_command;
use super::commands::doctor::{check, doctor};
use super::commands::image::image_command;
use super::commands::instance::{attach, launch_command, legacy_mapping_command, open};
use super::commands::logs::logs_command;
use super::commands::network::network_command;
use super::commands::profile::profile_command;
use super::commands::project::project_command;
use super::commands::skill::skill_command;
use super::commands::snapshot::snapshot_command;
use super::commands::vm::vm_command;
use super::output::{print_projects, project_rows};

/// Run the CLI. Returns the process exit code.
pub fn run(cli: Cli) -> Result<i32> {
    // Launch integrations re-execute `hegn attach` in tmux/herdr panes. Pass
    // the same config through the environment so a custom `--config` path is
    // not silently replaced by the default one in the child.
    if let Some(path) = &cli.config {
        let path = path.canonicalize().unwrap_or_else(|_| path.clone());
        // Safety: this runs before any worker thread or terminal event loop is
        // started, so no other thread can access the environment concurrently.
        unsafe { std::env::set_var("HEGN_CONFIG", path) };
    }
    let config = load_config(&cli)?;
    match cli.command {
        None | Some(Commands::Tui) => {
            crate::tui::run(config)?;
            Ok(0)
        }
        Some(command) => dispatch(command, config, cli.json),
    }
}

pub fn load_config(cli: &Cli) -> Result<Config> {
    let config = match &cli.config {
        Some(path) => Config::load_from(path)?,
        None => Config::load()?,
    };
    config.validate()?;
    Ok(config)
}

fn dispatch(command: Commands, config: Config, json: bool) -> Result<i32> {
    match command {
        Commands::Tui => {
            crate::tui::run(config)?;
            Ok(0)
        }
        Commands::Projects { query } => {
            let store =
                crate::session::state::StateStore::load(config.state_dir.join("state.json"));
            let projects =
                project::discover_protected(&config.projects_root, &config.protected_paths())?
                    .into_iter()
                    .filter(|project| !store.is_hidden(&project.path))
                    .collect::<Vec<_>>();
            let rows = project_rows(&config, &projects, query.as_deref().unwrap_or(""))?;
            if rows.is_empty() && !json {
                println!(
                    "No projects in {}.\nCreate one with: hegn project create <name>",
                    config.projects_root.display()
                );
                return Ok(0);
            }
            print_projects(&rows, json)?;
            Ok(0)
        }
        Commands::Project { command } => project_command(command, config, json),
        Commands::Open {
            project,
            editor,
            lazygit,
            no_branch,
            serial,
            no_bar,
            command,
        } => open(
            project, editor, lazygit, no_branch, serial, no_bar, command, config,
        ),
        Commands::Attach {
            project,
            serial,
            no_bar,
            command,
        } => attach(project, serial, no_bar, command, config),
        Commands::Launch {
            selector,
            command,
            integration,
            serial,
            no_bar,
        } => launch_command(selector, command, integration, serial, no_bar, config),
        Commands::Logs { command } => logs_command(
            command.unwrap_or(LogCommands::List { vm: None }),
            config,
            json,
        ),
        Commands::Vm { command } => vm_command(command, config, json),
        Commands::LegacyMapping { command } => legacy_mapping_command(command, config, json),
        Commands::Snapshot { command } => snapshot_command(command, config, json),
        Commands::Check { command, cwd } => check(&command.join(" "), cwd.as_deref(), config, json),
        Commands::Config { command } => config_command(command, config, json),
        Commands::Profile { command } => profile_command(command, config, json),
        Commands::Skills { command } => skill_command(command, config, json),
        Commands::Images { command } => image_command(command, config, json),
        Commands::Network { command } => network_command(command, config, json),
        Commands::Doctor { fix } => doctor(config, json, fix),
        Commands::Benchmark { project, clean } => {
            benchmark::run(&config, project.as_deref(), clean, json)
        }
    }
}
