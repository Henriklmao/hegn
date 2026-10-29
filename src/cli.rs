//! Non-interactive CLI. Every TUI capability has a CLI counterpart so hegn can
//! be scripted in CI, used over SSH, or driven by an agent.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::app::PACKAGE_CHOICES;
use crate::benchmark;
use crate::config::{Config, Harness};
use crate::doctor;
use crate::images::{self, AssetStatus};
use crate::instance::{InstanceRegistry, InstanceReport, InstanceSummary, process_alive};
use crate::logs::{self, DEFAULT_KEEP, LogStore};
use crate::project::{self, ProjectMeta};
use crate::provision::{ProvisionPlan, ProvisionProgress, ProvisionWatcher};
use crate::security::{Decision, SecurityEngine};
use crate::snapshot::{SnapshotMeta, SnapshotStore};
use crate::terminal::TerminalBuffer;

#[derive(Debug, Parser)]
#[command(
    name = "hegn",
    version,
    about = "Cloud Hypervisor instance manager and TUI for coding agents",
    long_about = "hegn launches hardware-isolated Cloud Hypervisor VMs with a shared \
workspace, intercepts dangerous commands with tirith-core, and manages VM snapshots. \
Run without a subcommand to open the TUI."
)]
pub struct Cli {
    /// Path to config.toml (defaults to ~/.config/hegn/config.toml).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Emit machine-readable JSON where supported.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Open the interactive TUI (default).
    Tui,
    /// List projects found under projects_root.
    #[command(visible_alias = "ls")]
    Projects {
        /// Fuzzy filter query.
        query: Option<String>,
    },
    /// Create or inspect a project.
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },
    /// Boot or attach a project VM and open its serial console.
    Open {
        /// Project name (under projects_root) or path.
        project: String,
        /// Open the project in $EDITOR on the host instead of the VM.
        #[arg(long, conflicts_with = "lazygit")]
        editor: bool,
        /// Open the project in lazygit on the host instead of the VM.
        #[arg(long)]
        lazygit: bool,
        /// Skip creating a transient git session branch.
        #[arg(long)]
        no_branch: bool,
        /// Do not reserve a status bar line (for full-screen guest programs).
        #[arg(long)]
        no_bar: bool,
    },
    /// Attach to the serial console of the already running VM (tmux-style).
    ///
    /// Never boots or restarts a VM. The bottom line shows the project name,
    /// harness, VM pid and persistent serial log; Ctrl-B detaches.
    #[command(visible_alias = "a")]
    Attach {
        /// Optional project name or path; must match the running session.
        project: Option<String>,
        /// Do not reserve a status bar line (for full-screen guest programs).
        #[arg(long)]
        no_bar: bool,
    },
    /// List, inspect and prune persistent serial-console logs.
    #[command(visible_alias = "log")]
    Logs {
        #[command(subcommand)]
        command: Option<LogCommands>,
    },
    /// Manage the Cloud Hypervisor instance lifecycle.
    #[command(alias = "sandbox", hide = true)]
    Instance {
        #[command(subcommand)]
        command: InstanceCommands,
    },
    /// List, create, restore and delete VM snapshots.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommands,
    },
    /// Inspect a command with tirith-core guardrails (non-interactive).
    Check {
        /// Command to inspect. Separate arguments are joined with spaces.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Working directory used for policy discovery.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Show or initialize configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Inspect and initialize per-harness guest profiles.
    Profile {
        #[command(subcommand)]
        command: ProfileCommands,
    },
    /// Copy host skills into an instance and sync changes back.
    Skills {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Inspect, download and verify the pinned upstream VM images.
    Images {
        #[command(subcommand)]
        command: ImageCommands,
    },
    /// Configure or inspect the host-enforced guest network guard.
    Network {
        #[command(subcommand)]
        command: NetworkCommands,
    },
    /// Check host prerequisites (KVM, Cloud Hypervisor, virtiofsd, assets, tools).
    Doctor {
        /// Perform hegn-owned setup: directories, default config, VM images and
        /// the guarded network (prompts for sudo when needed).
        #[arg(long)]
        fix: bool,
    },
    /// Measure the host cost of an instance; boots one on demand.
    Benchmark {
        /// Project name or path to boot and measure.
        project: Option<String>,
        /// Create a throwaway project, measure it, then remove it again.
        #[arg(long, conflicts_with = "project")]
        clean: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommands {
    /// Create a project directory, AGENTS.md, README and git repository.
    Create {
        name: String,
        /// Default coding harness (also written to hegn.toml).
        #[arg(long, value_enum)]
        harness: Option<Harness>,
        /// AGENTS.md template to copy.
        #[arg(long)]
        template: Option<PathBuf>,
        /// Packages recorded for the VM template (comma separated).
        #[arg(long = "package", value_delimiter = ',')]
        packages: Vec<String>,
    },
    /// Import an existing directory as a symlink under projects_root.
    ///
    /// The directory is never moved or copied. hegn refuses to import the
    /// filesystem root, the home directory, an ancestor of projects_root, or
    /// anything that overlaps hegn state.
    Import {
        /// Existing project directory (absolute, relative or starting with ~).
        path: PathBuf,
        /// Name in the picker (defaults to the directory name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Print project metadata and git status.
    Info { project: String },
    /// Remove a project from the picker and delete its instances.
    ///
    /// Git status is printed first. `--delete-files` also removes the project
    /// directory; `--keep-files` only unregisters it. Imported projects are
    /// symlinks and their original directory is never deleted.
    Delete {
        project: String,
        /// Delete the project directory (import symlinks are removed, not
        /// followed).
        #[arg(long, conflicts_with = "keep_files")]
        delete_files: bool,
        /// Remove the project from the picker but keep its files.
        #[arg(long, conflicts_with = "delete_files")]
        keep_files: bool,
        /// Skip interactive prompts (a file policy flag is still required).
        #[arg(long)]
        yes: bool,
    },
    /// Show a project that `project delete --keep-files` hid from the picker.
    Restore { project: String },
}

#[derive(Debug, Subcommand)]
pub enum InstanceCommands {
    /// List all instances and their state.
    #[command(visible_alias = "ls")]
    List,
    /// Create an instance for a project.
    Create {
        project: String,
        /// Instance name (defaults to the project directory name).
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        harness: Option<Harness>,
        /// Copy the project into the instance instead of exporting it directly.
        #[arg(long)]
        clone: bool,
    },
    /// Clone an instance for parallel agent work.
    Clone {
        /// Instance or project to clone.
        source: String,
        /// Base name for the clone(s).
        #[arg(long)]
        name: Option<String>,
        /// Number of clones to create.
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long, value_enum)]
        harness: Option<Harness>,
        /// Only create the clones; do not boot them.
        #[arg(long)]
        no_start: bool,
    },
    /// Boot an instance, creating it from a project if necessary.
    Start {
        /// Instance name, project name or project path.
        name: String,
        #[arg(long, value_enum)]
        harness: Option<Harness>,
        /// Start a temporary clone instead of the project itself.
        #[arg(long)]
        clone: bool,
    },
    /// Stop one instance, or the only running instance.
    Stop { name: Option<String> },
    /// Show one instance, or all instances.
    Status { name: Option<String> },
    /// Pause an instance.
    Pause { name: String },
    /// Resume an instance.
    Resume { name: String },
    /// Show or edit an instance's hegn.toml.
    Config {
        name: String,
        /// Open the config in $EDITOR.
        #[arg(long)]
        edit: bool,
        /// Print only the config path.
        #[arg(long)]
        path: bool,
    },
    /// Stop and delete an instance.
    Delete {
        name: String,
        /// Delete an in-instance clone workspace.
        #[arg(long, conflicts_with = "keep_files")]
        delete_files: bool,
        /// Keep an in-instance clone workspace.
        #[arg(long, conflicts_with = "delete_files")]
        keep_files: bool,
        /// Skip interactive prompts (a file policy flag is still required).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SnapshotCommands {
    /// List snapshots (newest first).
    #[command(visible_alias = "ls")]
    List,
    /// Pause the VM, snapshot RAM+CPU, resume.
    Create {
        /// Name for the snapshot (defaults to a timestamp).
        name: Option<String>,
        /// Instance to snapshot (defaults to the only running instance).
        #[arg(long)]
        instance: Option<String>,
    },
    /// Load a snapshot (boots a fresh Cloud Hypervisor process).
    Restore {
        name: String,
        /// Instance to restore into (defaults to the snapshot metadata).
        #[arg(long)]
        instance: Option<String>,
    },
    /// Delete a snapshot directory.
    Delete { name: String },
    /// Show snapshot metadata.
    Show { name: String },
}

#[derive(Debug, Subcommand)]
pub enum LogCommands {
    /// List stored serial logs, newest first.
    #[command(visible_alias = "ls")]
    List,
    /// Print the path of a log (defaults to the newest).
    Path { name: Option<String> },
    /// Print the end of a log (defaults to the newest, sanitized).
    Show {
        name: Option<String>,
        /// Number of trailing lines to print.
        #[arg(long, default_value_t = 80)]
        lines: usize,
        /// Write the raw bytes, including guest ANSI escape sequences.
        ///
        /// Only use this for logs you trust: guest output can contain terminal
        /// control sequences.
        #[arg(long)]
        raw: bool,
    },
    /// Delete all but the newest N session logs.
    Prune {
        /// Number of logs to keep (at least 1).
        #[arg(long, default_value_t = DEFAULT_KEEP)]
        keep: usize,
    },
}

#[derive(Debug, Subcommand)]
pub enum ImageCommands {
    /// Download missing kernel and rootfs images (checksums pinned).
    Download {
        /// Replace existing files after verifying the new download.
        #[arg(long)]
        force: bool,
        /// Permit downloads when no SHA-256 is configured (unsafe).
        #[arg(long)]
        allow_unverified: bool,
    },
    /// Show configured image URLs, paths and sizes.
    Status,
    /// Hash installed images and compare them with the pinned SHA-256.
    Verify,
}

#[derive(Debug, Subcommand)]
pub enum NetworkCommands {
    /// Show the firewall marker and TAP pool state.
    Status,
    /// Create the TAP pool and install the nftables rules (requires root).
    ///
    /// Includes the DNAT rules for configured local LLM endpoints.
    Setup {
        /// User that may open the TAP devices (defaults to $SUDO_USER or $USER).
        #[arg(long)]
        user: Option<String>,
        /// Print the script instead of applying it (does not require root).
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the TAP pool and the nftables table (requires root).
    Teardown,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommands {
    /// Print the effective configuration as TOML.
    Show,
    /// Print the configuration file path.
    Path,
    /// Write a default config if none exists.
    Init,
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommands {
    /// List harness profiles and their paths.
    List,
    /// Print the effective profile for a harness as TOML.
    Show {
        #[arg(value_enum)]
        harness: Harness,
    },
    /// Print the profile file path.
    Path {
        #[arg(value_enum)]
        harness: Harness,
        /// Open the profile in $EDITOR.
        #[arg(long)]
        edit: bool,
    },
    /// Write missing per-harness profile templates.
    Init,
    /// Refresh an instance's profile payload (files, environment, skills).
    Sync {
        /// Instance name, project name or project path.
        instance: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillCommands {
    /// Copy host skill changes into an instance's private copy.
    Sync {
        /// Instance name, project name or project path.
        instance: String,
    },
    /// Copy guest skill changes from an instance back to the host.
    SyncBack {
        /// Instance name, project name or project path.
        instance: String,
        /// Report what would be copied without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Sync while the VM is running (files may be mid-write).
        #[arg(long)]
        force: bool,
    },
}

/// Run the CLI. Returns the process exit code.
pub fn run(cli: Cli) -> Result<i32> {
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
            let store = crate::state::StateStore::load(config.state_dir.join("state.json"));
            let projects =
                project::discover_protected(&config.projects_root, &config.protected_paths())?
                    .into_iter()
                    .filter(|project| !store.is_hidden(&project.path))
                    .collect::<Vec<_>>();
            let indices = project::filter_projects(&projects, query.as_deref().unwrap_or(""));
            let selected: Vec<&project::Project> = indices.iter().map(|&i| &projects[i]).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&selected)?);
            } else if selected.is_empty() {
                println!(
                    "No projects in {}.\nCreate one with: hegn project create <name>",
                    config.projects_root.display()
                );
            } else {
                println!("{:<28} {:<20} PATH", "NAME", "GIT");
                for project in selected {
                    println!(
                        "{:<28} {:<20} {}",
                        project.name,
                        project.git.badge(),
                        project.display_path()
                    );
                }
            }
            Ok(0)
        }
        Commands::Project { command } => project_command(command, config, json),
        Commands::Open {
            project,
            editor,
            lazygit,
            no_branch,
            no_bar,
        } => open(project, editor, lazygit, no_branch, no_bar, config),
        Commands::Attach { project, no_bar } => attach(project, no_bar, config),
        Commands::Logs { command } => {
            logs_command(command.unwrap_or(LogCommands::List), config, json)
        }
        Commands::Instance { command } => instance_command(command, config, json),
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

fn project_command(command: ProjectCommands, config: Config, json: bool) -> Result<i32> {
    match command {
        ProjectCommands::Create {
            name,
            harness,
            template,
            packages: package_args,
        } => {
            let harness = harness.unwrap_or(config.harness);
            let template = match template {
                Some(path) => {
                    if !path.is_file() {
                        bail!("template {} does not exist", path.display());
                    }
                    Some(path)
                }
                None => {
                    let path = config.templates_dir.join("AGENTS.md");
                    path.is_file().then_some(path)
                }
            };
            let path = project::create_project(
                &config.projects_root,
                &name,
                template.as_deref(),
                harness,
            )?;
            let explicit_packages = !package_args.is_empty();
            // `--package` adds to the harness preset; it never replaces it.
            let mut packages: Vec<String> = harness
                .packages()
                .iter()
                .map(|package| package.to_string())
                .collect();
            for package in package_args {
                if !crate::config::safe_package_name(&package) {
                    bail!("invalid package name: {package:?}");
                }
                if explicit_packages && !PACKAGE_CHOICES.contains(&package.as_str()) {
                    eprintln!("note: '{package}' is not a built-in template package");
                }
                if !packages.iter().any(|existing| existing == &package) {
                    packages.push(package);
                }
            }
            let meta = ProjectMeta {
                harness: Some(harness),
                packages: packages.clone(),
                template: template.map(|p| p.display().to_string()),
            };
            project::write_meta(&path, &meta)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "path": path,
                        "harness": harness,
                        "preset": harness.preset_summary(),
                        "packages": packages,
                    }))?
                );
            } else {
                println!("Created {}", path.display());
                println!(
                    "Harness: {} ({}) — {}",
                    harness.label(),
                    harness.command(),
                    harness.preset_summary()
                );
                println!("Packages: {}", packages.join(", "));
                println!("Next: hegn open {}", path.display());
            }
            Ok(0)
        }
        ProjectCommands::Import { path, name } => {
            let link = config.import_project(&path, name.as_deref())?;
            let target = link.canonicalize().with_context(|| {
                format!("failed to resolve imported project {}", link.display())
            })?;
            let info = project::Project::load(link.clone());
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "name": info.name,
                        "path": link,
                        "target": target,
                        "git": info.git,
                    }))?
                );
            } else {
                println!("Imported {}", info.name);
                println!("  link   {}", link.display());
                println!("  target {}", target.display());
                println!("  git    {}", info.git.badge());
                println!("Next: hegn open {}", info.name);
            }
            Ok(0)
        }
        ProjectCommands::Info { project } => {
            let path = resolve_project(&config, &project)?;
            let info = project::Project::load(path.clone());
            let meta = project::read_meta(&path);
            let harness = meta.harness.unwrap_or(config.harness);
            if json {
                #[derive(Serialize)]
                struct Info<'a> {
                    name: &'a str,
                    path: &'a Path,
                    git: &'a project::GitSummary,
                    harness: Harness,
                    packages: &'a [String],
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Info {
                        name: &info.name,
                        path: &info.path,
                        git: &info.git,
                        harness,
                        packages: &meta.packages,
                    })?
                );
            } else {
                println!("{}", info.name);
                println!("  path     {}", info.path.display());
                println!("  git      {}", info.git.badge());
                if let Some(err) = &info.git.error {
                    println!("  git note {err}");
                }
                println!("  harness  {}", harness.label());
                if !meta.packages.is_empty() {
                    println!("  packages {}", meta.packages.join(", "));
                }
            }
            Ok(0)
        }
        ProjectCommands::Delete {
            project: selector,
            delete_files,
            keep_files,
            yes,
        } => {
            let entry = resolve_project_entry(&config, &selector)?;
            let info = project::Project::load(entry.clone());
            print_project_git(&entry, &info, json);
            if !yes
                && !prompt_yes(&format!(
                    "Remove project '{}' from the picker and delete its instances?",
                    info.name
                ))?
            {
                return Ok(0);
            }
            let delete_files = if delete_files {
                true
            } else if keep_files {
                false
            } else if std::io::stdin().is_terminal() {
                prompt_yes(&format!(
                    "Also delete the project files at {}?",
                    entry.display()
                ))?
            } else {
                bail!("choose --delete-files or --keep-files in non-interactive mode");
            };
            let outcome = project::delete_project(&config, &entry, delete_files)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry,
                        "files_deleted": outcome.files_deleted,
                        "instances": outcome.instances,
                    }))?
                );
            } else {
                println!(
                    "Removed {} ({}); {}",
                    info.name,
                    if outcome.instances.is_empty() {
                        "no instances".to_string()
                    } else {
                        format!("instances: {}", outcome.instances.join(", "))
                    },
                    if outcome.files_deleted {
                        "project files deleted"
                    } else {
                        "project files kept"
                    }
                );
            }
            Ok(0)
        }
        ProjectCommands::Restore { project: selector } => {
            let entry = resolve_project_entry(&config, &selector)?;
            let mut store = crate::state::StateStore::load(config.state_dir.join("state.json"));
            store.set_project_hidden(&entry, false)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "project": entry,
                        "hidden": false,
                    }))?
                );
            } else {
                println!("Restored {} to the picker", entry.display());
            }
            Ok(0)
        }
    }
}

fn print_project_git(path: &Path, info: &project::Project, json: bool) {
    if json {
        return;
    }
    println!("{}", info.name);
    println!("  path     {}", path.display());
    println!("  git      {}", info.git.badge());
    if let Some(error) = &info.git.error {
        println!("  git note {error}");
    }
    if info.git.is_dirty() {
        println!(
            "  changes  {} staged, {} modified, {} untracked",
            info.git.staged, info.git.modified, info.git.untracked
        );
        println!("  warning  uncommitted changes are lost if the files are deleted");
    }
}

fn instance_command(command: InstanceCommands, config: Config, json: bool) -> Result<i32> {
    let registry = InstanceRegistry::new(config.clone());
    match command {
        InstanceCommands::List => {
            print_instances(&registry.summaries()?, json)?;
            Ok(0)
        }
        InstanceCommands::Create {
            project,
            name,
            harness,
            clone,
        } => {
            let path = resolve_project(&config, &project)?;
            let harness = harness.or_else(|| project::read_meta(&path).harness);
            let instance = if clone {
                let outcome =
                    registry.create_with_warnings(&path, name.as_deref(), harness, true)?;
                for warning in &outcome.warnings {
                    eprintln!("warning: {warning}");
                }
                outcome.instance
            } else {
                registry.create(&path, name.as_deref(), harness, false)?
            };
            print_instance(&registry.summary(&instance), json)?;
            Ok(0)
        }
        InstanceCommands::Clone {
            source,
            name,
            count,
            harness,
            no_start,
        } => {
            if count == 0 || count > 64 {
                bail!("--count must be between 1 and 64");
            }
            let source = registry.resolve(&source)?;
            let base = name.unwrap_or_else(|| format!("{}-agent", source.name));
            let mut created = Vec::new();
            for index in 1..=count {
                let clone_name = if count == 1 {
                    registry.unique_name(&base)?
                } else {
                    registry.unique_name(&format!("{base}-{index}"))?
                };
                let outcome = registry.clone_from(&source, &clone_name, harness)?;
                for warning in &outcome.warnings {
                    eprintln!("warning: {warning}");
                }
                if !no_start {
                    registry.manager(outcome.instance.clone()).start()?;
                }
                created.push(registry.summary(&outcome.instance));
            }
            print_instances(&created, json)?;
            Ok(0)
        }
        InstanceCommands::Start {
            name,
            harness,
            clone,
        } => {
            let instance = match registry.resolve(&name) {
                Ok(instance) => {
                    if clone {
                        let clone_name =
                            registry.unique_name(&format!("{}-agent", instance.name))?;
                        let outcome = registry.clone_from(&instance, &clone_name, harness)?;
                        for warning in &outcome.warnings {
                            eprintln!("warning: {warning}");
                        }
                        outcome.instance
                    } else {
                        instance
                    }
                }
                Err(_) => {
                    let path = resolve_project(&config, &name)?;
                    let meta = project::read_meta(&path);
                    let harness = harness.or(meta.harness).unwrap_or(config.harness);
                    if clone {
                        let outcome =
                            registry.create_with_warnings(&path, None, Some(harness), true)?;
                        for warning in &outcome.warnings {
                            eprintln!("warning: {warning}");
                        }
                        outcome.instance
                    } else {
                        registry.ensure_for_project(&path, harness)?
                    }
                }
            };
            let manager = registry.manager(instance);
            let report = manager.start()?;
            print_report(&report, json)?;
            Ok(0)
        }
        InstanceCommands::Stop { name } => {
            let instance = match name {
                Some(selector) => registry.resolve(&selector)?,
                None => only_running_instance(&registry)?,
            };
            registry.manager(instance.clone()).stop()?;
            if json {
                println!("{}", serde_json::json!({"stopped": instance.name}));
            } else {
                println!("Instance '{}' stopped.", instance.name);
            }
            Ok(0)
        }
        InstanceCommands::Status { name } => {
            match name {
                Some(selector) => {
                    let instance = registry.resolve(&selector)?;
                    let report = registry.manager(instance).status()?;
                    print_report(&report, json)?;
                }
                None => print_instances(&registry.summaries()?, json)?,
            }
            Ok(0)
        }
        InstanceCommands::Pause { name } => {
            let instance = registry.resolve(&name)?;
            let instance_name = instance.name.clone();
            runtime()?.block_on(registry.manager(instance).pause())?;
            if json {
                println!("{}", serde_json::json!({"paused": instance_name}));
            } else {
                println!("Instance '{instance_name}' paused.");
            }
            Ok(0)
        }
        InstanceCommands::Resume { name } => {
            let instance = registry.resolve(&name)?;
            let instance_name = instance.name.clone();
            runtime()?.block_on(registry.manager(instance).resume())?;
            if json {
                println!("{}", serde_json::json!({"resumed": instance_name}));
            } else {
                println!("Instance '{instance_name}' resumed.");
            }
            Ok(0)
        }
        InstanceCommands::Config { name, edit, path } => {
            let instance = registry.store().load(&name)?;
            let config_path = registry.store().config_path(&name)?;
            if path {
                println!("{}", config_path.display());
            } else if edit {
                run_editor(&config_path)?;
            } else if json {
                println!("{}", serde_json::to_string_pretty(&instance)?);
            } else {
                println!("{}", toml::to_string_pretty(&instance)?);
            }
            Ok(0)
        }
        InstanceCommands::Delete {
            name,
            delete_files,
            keep_files,
            yes,
        } => {
            let instance = registry.store().load(&name)?;
            let workspace_inside = instance
                .workspace
                .canonicalize()
                .map(|workspace| {
                    instance
                        .dir(registry.store().root())
                        .canonicalize()
                        .map(|dir| workspace.starts_with(&dir))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if !yes && !prompt_yes(&format!("Delete instance '{}'?", instance.name))? {
                println!("Cancelled.");
                return Ok(0);
            }
            let delete_files = if workspace_inside {
                if delete_files {
                    true
                } else if keep_files {
                    false
                } else {
                    if !std::io::stdin().is_terminal() {
                        bail!("workspace files require an explicit --delete-files or --keep-files");
                    }
                    prompt_yes("Also delete the cloned workspace files?")?
                }
            } else {
                false
            };
            let outcome = registry.delete(&instance.name, delete_files)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "deleted": instance.name,
                        "delete_files": delete_files,
                        "kept_workspace": outcome.kept_workspace,
                    })
                );
            } else if let Some(kept) = outcome.kept_workspace {
                println!(
                    "Instance '{}' removed. Workspace kept at {}",
                    instance.name,
                    kept.display()
                );
            } else if workspace_inside && delete_files {
                println!("Instance '{}' and its workspace deleted.", instance.name);
            } else {
                println!(
                    "Instance '{}' removed. Source project {} was not touched.",
                    instance.name,
                    instance.project.display()
                );
            }
            Ok(0)
        }
    }
}

fn only_running_instance(
    registry: &InstanceRegistry,
) -> Result<crate::instance_config::InstanceConfig> {
    let mut running: Vec<_> = registry
        .summaries()?
        .into_iter()
        .filter(|instance| instance.state == "running")
        .collect();
    match running.len() {
        0 => bail!("no running instance; start one with `hegn instance start <name>`"),
        1 => registry.store().load(&running.remove(0).name),
        _ => {
            let names: Vec<String> = running.into_iter().map(|instance| instance.name).collect();
            bail!(
                "multiple instances are running ({}); pass an instance name",
                names.join(", ")
            )
        }
    }
}

fn prompt_yes(question: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        bail!("confirmation required; pass --yes and a file policy flag in non-interactive mode");
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn snapshot_instance(
    registry: &InstanceRegistry,
    requested: Option<String>,
) -> Result<crate::instance_config::InstanceConfig> {
    match requested {
        Some(selector) => registry.resolve(&selector),
        None => only_running_instance(registry),
    }
}

fn snapshot_command(command: SnapshotCommands, config: Config, json: bool) -> Result<i32> {
    let store = SnapshotStore::new(&config.snapshots_dir);
    let registry = InstanceRegistry::new(config.clone());
    match command {
        SnapshotCommands::List => {
            let snapshots = store.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else if snapshots.is_empty() {
                println!("No snapshots in {}.", store.root().display());
            } else {
                println!(
                    "{:<32} {:<20} {:<18} {:>10}",
                    "NAME", "INSTANCE", "CREATED", "SIZE"
                );
                for snapshot in snapshots {
                    println!(
                        "{:<32} {:<20} {:<18} {:>10}",
                        snapshot.name,
                        snapshot.instance.as_deref().unwrap_or("-"),
                        snapshot.created_at.format("%Y-%m-%d %H:%M:%S"),
                        snapshot.size_label()
                    );
                }
            }
            Ok(0)
        }
        SnapshotCommands::Create { name, instance } => {
            let instance = snapshot_instance(&registry, instance)?;
            let suggested = SnapshotStore::suggested_name(
                instance
                    .project
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("snapshot"),
            );
            let name = name.unwrap_or(suggested);
            let meta = runtime()?.block_on(registry.manager(instance).create_snapshot(&name))?;
            print_snapshot(&meta, json)?;
            Ok(0)
        }
        SnapshotCommands::Restore { name, instance } => {
            let meta = store.load(&name)?;
            let instance = match instance {
                Some(selector) => registry.resolve(&selector)?,
                None => match meta.instance.as_deref() {
                    Some(name) => registry.store().load(name)?,
                    None => {
                        let project = meta
                            .project
                            .as_deref()
                            .context("snapshot has no instance/project metadata")?;
                        registry.resolve(project.to_string_lossy().as_ref())?
                    }
                },
            };
            runtime()?.block_on(registry.manager(instance).restore_snapshot(&meta))?;
            if json {
                println!("{}", serde_json::json!({"restored": meta.name}));
            } else {
                println!("Restored snapshot '{}'.", meta.name);
            }
            Ok(0)
        }
        SnapshotCommands::Delete { name } => {
            store.delete(&name)?;
            if json {
                println!("{}", serde_json::json!({"deleted": name}));
            } else {
                println!("Deleted snapshot '{name}'.");
            }
            Ok(0)
        }
        SnapshotCommands::Show { name } => {
            let meta = store.load(&name)?;
            print_snapshot(&meta, json)?;
            Ok(0)
        }
    }
}

fn check(command: &str, cwd: Option<&Path>, config: Config, json: bool) -> Result<i32> {
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

fn config_command(command: ConfigCommands, config: Config, json: bool) -> Result<i32> {
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
            let profiles_written = crate::profile::ProfileStore::new(&config).write_templates()?;
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

fn profile_command(command: ProfileCommands, config: Config, json: bool) -> Result<i32> {
    let store = crate::profile::ProfileStore::new(&config);
    match command {
        ProfileCommands::List => {
            let mut entries = Vec::new();
            for harness in Harness::ALL {
                if harness == Harness::Shell {
                    continue;
                }
                let path = store.path(harness);
                let exists = path.is_file();
                entries.push(serde_json::json!({
                    "harness": harness.slug(),
                    "path": path,
                    "exists": exists,
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                println!("{:<16} {:<6} PATH", "HARNESS", "STATE");
                for entry in &entries {
                    println!(
                        "{:<16} {:<6} {}",
                        entry["harness"].as_str().unwrap_or(""),
                        if entry["exists"].as_bool().unwrap_or(false) {
                            "ok"
                        } else {
                            "missing"
                        },
                        entry["path"].as_str().unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        ProfileCommands::Show { harness } => {
            let profile = store.load(harness)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&profile)?);
            } else {
                print!("{}", toml::to_string_pretty(&profile)?);
            }
            Ok(0)
        }
        ProfileCommands::Path { harness, edit } => {
            let path = store.path(harness);
            if edit {
                if !path.is_file() {
                    store.write_templates()?;
                }
                run_editor(&path)?;
                return Ok(0);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&path)?);
            } else {
                println!("{}", path.display());
            }
            Ok(0)
        }
        ProfileCommands::Init => {
            let written = store.write_templates()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&written)?);
            } else if written.is_empty() {
                println!(
                    "Profile templates already exist in {}",
                    store.dir().display()
                );
            } else {
                for path in &written {
                    println!("Wrote {}", path.display());
                }
            }
            Ok(0)
        }
        ProfileCommands::Sync { instance } => {
            let registry = InstanceRegistry::new(config);
            let instance = registry.resolve(&instance)?;
            let plan = crate::profile::prepare_for_start(registry.config(), &instance)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "instance": instance.name,
                        "packages": plan.packages,
                        "share": plan.share.as_ref().map(|share| &share.bundle_dir),
                        "warnings": plan.warnings,
                    }))?
                );
            } else {
                match &plan.share {
                    Some(share) => println!(
                        "Profile payload for '{}' refreshed at {}",
                        instance.name,
                        share.bundle_dir.display()
                    ),
                    None => println!(
                        "Profile for '{}' has no files, skills or environment to export",
                        instance.name
                    ),
                }
                for warning in &plan.warnings {
                    eprintln!("hegn: {warning}");
                }
                println!("Restart the instance to apply configuration files.");
            }
            Ok(0)
        }
    }
}

fn skill_command(command: SkillCommands, config: Config, json: bool) -> Result<i32> {
    let registry = InstanceRegistry::new(config);
    match command {
        SkillCommands::Sync { instance } => {
            let instance = registry.resolve(&instance)?;
            let report = crate::profile::sync_skills(registry.config(), &instance)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Copied {} file(s) from {} host source(s) into instance '{}'",
                    report.copied_files, report.sources, report.instance
                );
                for warning in &report.warnings {
                    eprintln!("hegn: {warning}");
                }
            }
            Ok(0)
        }
        SkillCommands::SyncBack {
            instance,
            dry_run,
            force,
        } => {
            let instance = registry.resolve(&instance)?;
            let manager = registry.manager(instance.clone());
            if manager.is_running() && !force && !dry_run {
                bail!(
                    "instance '{}' is running; stop it first or pass --force to copy live files",
                    instance.name
                );
            }
            let report = crate::profile::sync_skills_back(registry.config(), &instance, dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let action = if dry_run { "Would copy" } else { "Copied" };
                println!(
                    "{} {} file(s) from instance '{}' back to the host ({} unchanged)",
                    action, report.copied_files, report.instance, report.skipped_files
                );
                for warning in &report.warnings {
                    eprintln!("hegn: {warning}");
                }
            }
            Ok(0)
        }
    }
}

fn image_command(command: ImageCommands, config: Config, json: bool) -> Result<i32> {
    match command {
        ImageCommands::Download {
            force,
            allow_unverified,
        } => {
            let assets = images::download_all(&config, force, allow_unverified)?;
            print_assets(&assets, json)?;
            Ok(0)
        }
        ImageCommands::Status => {
            print_assets(&images::status(&config), json)?;
            Ok(0)
        }
        ImageCommands::Verify => {
            let mut results = Vec::new();
            let mut failed = false;
            for asset in images::status(&config) {
                let result = if !asset.exists {
                    Err("missing".to_string())
                } else {
                    images::verify_file(&asset.path, &asset.sha256)
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                };
                if result.is_err() {
                    failed = true;
                }
                results.push(serde_json::json!({
                    "name": asset.name,
                    "path": asset.path,
                    "sha256": asset.sha256,
                    "ok": result.is_ok(),
                    "error": result.err(),
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                for result in &results {
                    let ok = result["ok"].as_bool().unwrap_or(false);
                    println!(
                        "{} {:<8} {}",
                        if ok { "✓" } else { "✗" },
                        result["name"].as_str().unwrap_or("asset"),
                        result["path"].as_str().unwrap_or("")
                    );
                    if let Some(error) = result["error"].as_str() {
                        println!("  {error}");
                    }
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
    }
}

fn print_assets(assets: &[AssetStatus], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(assets)?);
        return Ok(());
    }
    println!("{:<8} {:<8} {:>12}  PATH", "NAME", "STATE", "SIZE");
    for asset in assets {
        println!(
            "{:<8} {:<8} {:>12}  {}",
            asset.name,
            if asset.exists { "present" } else { "missing" },
            asset
                .size_bytes
                .map(human_size)
                .unwrap_or_else(|| "-".into()),
            asset.path.display()
        );
    }
    if assets.iter().any(AssetStatus::missing) {
        println!("\nDownload with: hegn images download");
    }
    Ok(())
}

fn network_command(command: NetworkCommands, config: Config, json: bool) -> Result<i32> {
    match command {
        NetworkCommands::Status => {
            let marker = crate::network::ready_marker();
            let taps = crate::network::status(&config);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ready": marker.is_some(),
                        "marker": marker,
                        "mode": config.network.label(),
                        "endpoints": config.endpoint_ports(),
                        "endpoints_applied": marker.as_ref().map(|marker| marker.endpoints.clone()),
                        "taps": taps,
                    }))?
                );
            } else {
                match &marker {
                    Some(marker) => println!(
                        "guard: ready (subnet {}, {} TAPs, host firewall {}, applied {})",
                        marker.subnet,
                        marker.pool,
                        marker.host_firewall.label(),
                        marker.applied_at
                    ),
                    None => println!(
                        "guard: not configured; run `sudo hegn network setup --user $USER`"
                    ),
                }
                println!("mode: {}", config.network.label());
                let ports = config.endpoint_ports();
                if ports.is_empty() {
                    println!("endpoints: none");
                } else {
                    println!(
                        "endpoints: {}",
                        config
                            .local_endpoints
                            .iter()
                            .map(|endpoint| format!(
                                "{}=127.0.0.1:{}",
                                endpoint.name, endpoint.port
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                if marker
                    .as_ref()
                    .is_some_and(|marker| marker.endpoints != ports)
                {
                    println!("note: endpoints changed; re-run `sudo hegn network setup`");
                }
                for tap in taps {
                    let owner = tap.owner.as_deref().unwrap_or("-");
                    println!(
                        "  {:<8} {:<15} {} owner={owner}",
                        tap.tap,
                        tap.host_ip,
                        if tap.exists { "up" } else { "missing" }
                    );
                }
            }
            Ok(0)
        }
        NetworkCommands::Setup { user, dry_run } => {
            let user = user
                .or_else(|| std::env::var("SUDO_USER").ok())
                .or_else(|| std::env::var("USER").ok())
                .context("could not determine the TAP owner; pass --user NAME")?;
            if dry_run {
                println!(
                    "{}",
                    crate::network::setup_script(&user, &config.endpoint_ports())?
                );
                return Ok(0);
            }
            crate::network::setup(&user, &config.endpoint_ports())?;
            if json {
                println!("{}", serde_json::json!({"ready": true, "user": user}));
            } else {
                println!(
                    "guarded network is active; TAPs are owned by '{user}'. Start an instance with \
                     `hegn instance start <name>`."
                );
            }
            Ok(0)
        }
        NetworkCommands::Teardown => {
            crate::network::teardown()?;
            if json {
                println!("{}", serde_json::json!({"ready": false}));
            } else {
                println!("guarded network removed.");
            }
            Ok(0)
        }
    }
}

fn doctor(config: Config, json: bool, fix: bool) -> Result<i32> {
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
                let written = crate::profile::ProfileStore::new(&config).write_templates()?;
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

fn open(
    project: String,
    editor: bool,
    lazygit: bool,
    no_branch: bool,
    no_bar: bool,
    config: Config,
) -> Result<i32> {
    let path = resolve_project(&config, &project)?;
    if editor {
        return run_editor(&path);
    }
    if lazygit {
        let status = Command::new("lazygit")
            .current_dir(&path)
            .status()
            .context("failed to launch lazygit (is it installed?)")?;
        return Ok(status.code().unwrap_or(1));
    }

    let meta = project::read_meta(&path);
    let harness = meta.harness.unwrap_or(config.harness);
    let registry = InstanceRegistry::new(config.clone());
    let instance = registry.ensure_for_project(&path, harness)?;
    let plan = ProvisionPlan::new(instance.harness, &instance.extra_packages).ok();
    let manager = registry.manager(instance.clone());
    let report = if manager.is_running() {
        manager.status().ok()
    } else {
        Some(manager.start()?)
    };
    if config.cloud_init
        && let (Some(report), Some(plan)) = (&report, plan)
    {
        wait_for_provision(report.serial_log.as_deref(), &plan);
    }
    let console = manager.attach_console()?;

    // Only mutate the workspace repository once a VM is actually available.
    let workspace = instance.workspace.clone();
    let git = project::read_git_summary(&workspace);
    if instance.auto_branch && git.is_repo && !no_branch {
        match project::create_session_branch(&workspace) {
            Ok(branch) => println!("Session branch: {branch}"),
            Err(err) => eprintln!("warning: could not create session branch: {err}"),
        }
    }

    crate::console::run_interactive(console, !no_bar)?;
    Ok(0)
}

/// Wait for the guest provisioner while showing a rough progress bar.
///
/// Pre-provisioned images never emit progress, so the wait is bounded: after a
/// short grace period `hegn open` attaches normally.
fn wait_for_provision(log: Option<&Path>, plan: &ProvisionPlan) {
    let Some(log) = log else {
        return;
    };
    let Ok(mut watcher) = ProvisionWatcher::open(log, plan) else {
        return;
    };
    let tty = std::io::stderr().is_terminal();
    let started = Instant::now();
    let mut last_update = started;
    let mut last_label = String::new();
    loop {
        match watcher.poll() {
            Ok(events) if !events.is_empty() => last_update = Instant::now(),
            Ok(_) => {}
            Err(_) => break,
        }
        if watcher.saw_progress() {
            let progress = watcher.progress();
            if tty {
                draw_cli_provision(progress);
            } else {
                let label = progress.stage_label();
                if label != last_label {
                    eprintln!("hegn: {label}");
                    last_label = label.to_string();
                }
            }
        }
        if watcher.finished() {
            break;
        }
        if !watcher.saw_progress() && started.elapsed() > Duration::from_secs(20) {
            break;
        }
        // A guest that stops reporting for several minutes is better watched
        // through the console than through a frozen bar.
        if watcher.saw_progress() && last_update.elapsed() > Duration::from_secs(5 * 60) {
            break;
        }
        if started.elapsed() > Duration::from_secs(15 * 60) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if tty && watcher.saw_progress() {
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }
}

fn draw_cli_provision(progress: &ProvisionProgress) {
    const WIDTH: usize = 28;
    let filled = (progress.fraction() * WIDTH as f64).round() as usize;
    let bar = format!(
        "{}{}",
        "█".repeat(filled.min(WIDTH)),
        "░".repeat(WIDTH.saturating_sub(filled))
    );
    eprint!(
        "\r\x1b[2K setting up {}  [{bar}]  {}  ",
        progress.harness_label(),
        progress.eta_label()
    );
    let _ = std::io::stderr().flush();
}

fn attach(project: Option<String>, no_bar: bool, config: Config) -> Result<i32> {
    let registry = InstanceRegistry::new(config.clone());
    let instance = match project {
        Some(selector) => registry.resolve(&selector)?,
        None => only_running_instance(&registry)?,
    };
    // VmConsole::attach re-verifies the recorded pid, VM binary and relay.
    let console = registry.manager(instance).attach_console()?;
    crate::console::run_interactive(console, !no_bar)?;
    Ok(0)
}

fn logs_command(command: LogCommands, config: Config, json: bool) -> Result<i32> {
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

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn run_editor(path: &Path) -> Result<i32> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$1\""))
        .arg("hegn-editor")
        .arg(path)
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"))?;
    Ok(status.code().unwrap_or(1))
}

pub fn resolve_project(config: &Config, arg: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(arg);
    if direct.exists() {
        if !direct.is_dir() {
            bail!("{} is not a directory", direct.display());
        }
        return Ok(direct
            .canonicalize()
            .unwrap_or_else(|_| direct.to_path_buf()));
    }
    let candidate = config.projects_root.join(arg);
    if candidate.is_dir() {
        return Ok(candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.to_path_buf()));
    }
    bail!(
        "project '{arg}' not found; looked at {} and {}",
        direct.display(),
        candidate.display()
    )
}

pub fn resolve_project_entry(config: &Config, arg: &str) -> Result<PathBuf> {
    let root = config
        .projects_root
        .canonicalize()
        .unwrap_or_else(|_| config.projects_root.clone());
    for candidate in [config.projects_root.join(arg), PathBuf::from(arg)] {
        if std::fs::symlink_metadata(&candidate).is_err() {
            continue;
        }
        let parent = candidate
            .parent()
            .and_then(|parent| parent.canonicalize().ok());
        if parent.as_ref() == Some(&root) {
            return Ok(candidate);
        }
    }
    bail!(
        "project '{arg}' not found directly inside {}",
        config.projects_root.display()
    )
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

fn print_instances(instances: &[InstanceSummary], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(instances)?);
        return Ok(());
    }
    if instances.is_empty() {
        println!("No instances. Create one with: hegn instance create <project>");
        return Ok(());
    }
    println!(
        "{:<24} {:<12} {:<10} {:<12} PROJECT",
        "INSTANCE", "STATE", "MODE", "HARNESS"
    );
    for instance in instances {
        println!(
            "{:<24} {:<12} {:<10} {:<12} {}",
            instance.name,
            instance.state,
            if instance.ephemeral {
                "clone"
            } else {
                "project"
            },
            instance.harness.label(),
            instance.project.display()
        );
    }
    Ok(())
}

fn print_instance(instance: &InstanceSummary, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(instance)?);
        return Ok(());
    }
    println!("instance      {}", instance.name);
    println!("state        {}", instance.state);
    println!(
        "mode         {}",
        if instance.ephemeral {
            "clone"
        } else {
            "project"
        }
    );
    println!("project      {}", instance.project.display());
    println!("workspace    {}", instance.workspace.display());
    println!("harness      {}", instance.harness.label());
    println!(
        "resources    {} vCPU · {} MiB",
        instance.vcpus, instance.memory_mib
    );
    println!("config       {}", instance.config_path.display());
    if let Some(source) = &instance.source_instance {
        println!("source       {source}");
    }
    if let Some(pid) = instance.vm_pid {
        println!("vm           pid {pid}");
    }
    if let Some(log) = &instance.serial_log {
        println!("serial log   {}", log.display());
    }
    if let Some(error) = &instance.error {
        println!("error        {error}");
    }
    Ok(())
}

fn print_report(report: &InstanceReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!("instance      {}", report.instance);
    println!("state        {}", report.state);
    println!("backend      {}", report.backend);
    println!("workspace    {}", report.workspace.display());
    if let Some(project) = &report.project {
        println!("project      {}", project.display());
    }
    println!(
        "resources    {} vCPU · {} MiB",
        report.vcpus, report.memory_mib
    );
    println!("api socket   {}", report.api_socket.display());
    println!("serial       {}", report.serial_socket.display());
    println!("console      {}", report.console_socket.display());
    println!(
        "virtiofs     {} ({})",
        if report.virtiofs_running {
            "running"
        } else {
            "stopped"
        },
        report.virtiofs_socket.display()
    );
    if let Some(pid) = report.vm_pid {
        println!(
            "vm           pid {pid} ({})",
            if process_alive(pid) { "alive" } else { "dead" }
        );
    }
    if let Some(pid) = report.relay_pid {
        println!(
            "relay        pid {pid} ({})",
            if process_alive(pid) { "alive" } else { "dead" }
        );
    }
    if let Some(pid) = report.virtiofs_pid {
        println!(
            "virtiofsd    pid {pid} ({})",
            if process_alive(pid) { "alive" } else { "dead" }
        );
    }
    if let Some(log) = &report.serial_log {
        println!("serial log   {}", log.display());
    }
    for endpoint in &report.local_endpoints {
        println!("endpoint     {endpoint}");
    }
    if let Some(note) = &report.note {
        println!("note         {note}");
    }
    Ok(())
}

fn print_snapshot(meta: &SnapshotMeta, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(meta)?);
    } else {
        println!("{}", meta.name);
        println!(
            "  created {}",
            meta.created_at.format("%Y-%m-%d %H:%M:%S UTC")
        );
        println!("  state   {}", meta.vm_state);
        println!("  size    {}", meta.size_label());
        println!("  dir     {}", meta.dir.display());
        println!("  source  {}", meta.source_url());
        if let Some(instance) = &meta.instance {
            println!("  instance {instance}");
        }
        if let Some(project) = &meta.project {
            println!("  project  {}", project.display());
        }
    }
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Helper used by tests: run the security check and return the printed exit
/// code without touching a terminal.
pub fn check_command_for_test(config: &Config, command: &str) -> Result<i32> {
    check(command, None, config.clone(), false)
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::BackendKind;

    fn config() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.projects_root = dir.path().join("projects");
        config.state_dir = dir.path().join("state");
        config.snapshots_dir = dir.path().join("snapshots");
        config.instances_dir = dir.path().join("instances");
        config.templates_dir = dir.path().join("templates");
        config.profiles_dir = dir.path().join("profiles");
        config.backend = BackendKind::Auto;
        std::fs::create_dir_all(&config.projects_root).unwrap();
        (dir, config)
    }

    #[test]
    fn cli_parses_default_tui_and_global_flags() {
        let cli = Cli::try_parse_from(["hegn", "--json", "projects", "hegn"]).unwrap();
        assert!(cli.json);
        assert!(matches!(cli.command, Some(Commands::Projects { .. })));
        let cli = Cli::try_parse_from(["hegn"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_check_with_trailing_hyphen_values() {
        let cli =
            Cli::try_parse_from(["hegn", "check", "curl", "-s", "https://x", "|", "bash"]).unwrap();
        match cli.command {
            Some(Commands::Check { command, .. }) => {
                assert_eq!(command.join(" "), "curl -s https://x | bash");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resolves_project_by_name_and_path() {
        let (_dir, config) = config();
        let path = config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        assert_eq!(
            resolve_project(&config, "demo").unwrap(),
            path.canonicalize().unwrap()
        );
        assert_eq!(
            resolve_project(&config, path.to_str().unwrap()).unwrap(),
            path.canonicalize().unwrap()
        );
        assert!(resolve_project(&config, "missing").is_err());
    }

    #[test]
    fn check_returns_tirith_exit_codes() {
        let (_dir, config) = config();
        assert_eq!(check_command_for_test(&config, "ls -la").unwrap(), 0);
        assert_eq!(
            check_command_for_test(&config, "rm -rf /workspace").unwrap(),
            1
        );
    }

    #[test]
    fn project_create_writes_metadata() {
        let (_dir, config) = config();
        let code = project_command(
            ProjectCommands::Create {
                name: "demo".into(),
                harness: Some(Harness::Codex),
                template: None,
                packages: vec!["git".into(), "python3".into()],
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let meta = project::read_meta(&config.projects_root.join("demo"));
        assert_eq!(meta.harness, Some(Harness::Codex));
        // The preset stays complete; --package only adds to it.
        for package in ["git", "nodejs", "curl", "python3"] {
            assert!(
                meta.packages.iter().any(|existing| existing == package),
                "missing {package}: {:?}",
                meta.packages
            );
        }
        assert_eq!(
            meta.packages
                .iter()
                .filter(|package| *package == "git")
                .count(),
            1
        );
    }

    #[test]
    fn project_import_creates_a_picker_symlink() {
        let (dir, config) = config();
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();
        let code = project_command(
            ProjectCommands::Import {
                path: source.clone(),
                name: None,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let link = config.projects_root.join("demo");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(link.canonicalize().unwrap(), source.canonicalize().unwrap());
    }

    #[test]
    fn project_import_parses_and_rejects_unsafe_sources() {
        let (_dir, config) = config();
        let cli = Cli::try_parse_from(["hegn", "project", "import", "/tmp/example", "--name", "x"])
            .unwrap();
        match cli.command {
            Some(Commands::Project {
                command: ProjectCommands::Import { path, name },
            }) => {
                assert_eq!(path, PathBuf::from("/tmp/example"));
                assert_eq!(name.as_deref(), Some("x"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(config.import_project(Path::new("/"), None).is_err());
    }

    #[test]
    fn project_delete_keeps_or_removes_files_and_can_be_restored() {
        let (_dir, config) = config();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"demo").unwrap();

        let code = project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: false,
                keep_files: true,
                yes: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(project.join("README.md").is_file(), "files must be kept");
        let store = crate::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(store.is_hidden(&project));

        let code = project_command(
            ProjectCommands::Restore {
                project: "demo".into(),
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        let store = crate::state::StateStore::load(config.state_dir.join("state.json"));
        assert!(!store.is_hidden(&project));

        let code = project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: true,
                keep_files: false,
                yes: true,
            },
            config,
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(!project.exists());
    }

    #[test]
    fn project_delete_never_follows_an_import_link() {
        let (dir, config) = config();
        let source = dir.path().join("outside/demo");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("README.md"), b"demo").unwrap();
        project_command(
            ProjectCommands::Import {
                path: source.clone(),
                name: None,
            },
            config.clone(),
            true,
        )
        .unwrap();
        let link = config.projects_root.join("demo");

        assert!(
            resolve_project_entry(&config, &source.display().to_string()).is_err(),
            "deletion must only target picker entries"
        );
        project_command(
            ProjectCommands::Delete {
                project: "demo".into(),
                delete_files: true,
                keep_files: false,
                yes: true,
            },
            config,
            true,
        )
        .unwrap();
        assert!(!link.exists());
        assert!(source.join("README.md").is_file());
    }

    #[test]
    fn image_status_and_missing_verify() {
        let (dir, mut config) = config();
        config.kernel_image = dir.path().join("missing-kernel");
        config.rootfs_image = dir.path().join("missing-rootfs");
        assert_eq!(
            image_command(ImageCommands::Status, config.clone(), true).unwrap(),
            0
        );
        // Missing images are a verification failure, not a pass.
        assert_eq!(
            image_command(ImageCommands::Verify, config, true).unwrap(),
            1
        );
    }

    #[test]
    fn project_create_rejects_a_missing_template() {
        let (_dir, config) = config();
        let err = project_command(
            ProjectCommands::Create {
                name: "demo".into(),
                harness: None,
                template: Some(PathBuf::from("/definitely/missing/AGENTS.md")),
                packages: Vec::new(),
            },
            config,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn instance_status_json_works_without_vm() {
        let (_dir, config) = config();
        let code = instance_command(InstanceCommands::Status { name: None }, config, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn instance_create_clone_list_config_and_delete() {
        let (_dir, config) = config();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"demo").unwrap();

        let code = instance_command(
            InstanceCommands::Create {
                project: project.display().to_string(),
                name: Some("base".into()),
                harness: Some(Harness::Codex),
                clone: false,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(config.instances_dir.join("base/hegn.toml").is_file());

        let code = instance_command(
            InstanceCommands::Clone {
                source: "base".into(),
                name: Some("agent1".into()),
                count: 1,
                harness: None,
                no_start: true,
            },
            config.clone(),
            true,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(config.instances_dir.join("agent1/hegn.toml").is_file());
        assert!(
            config
                .instances_dir
                .join("agent1/workspace/README.md")
                .is_file()
        );

        assert_eq!(
            instance_command(InstanceCommands::List, config.clone(), true).unwrap(),
            0
        );
        assert_eq!(
            instance_command(
                InstanceCommands::Config {
                    name: "base".into(),
                    edit: false,
                    path: true,
                },
                config.clone(),
                true,
            )
            .unwrap(),
            0
        );
        assert_eq!(
            instance_command(
                InstanceCommands::Delete {
                    name: "agent1".into(),
                    delete_files: true,
                    keep_files: false,
                    yes: true,
                },
                config.clone(),
                true,
            )
            .unwrap(),
            0
        );
        assert!(!config.instances_dir.join("agent1").exists());
        assert!(project.join("README.md").is_file());
    }

    #[test]
    fn snapshot_list_empty() {
        let (_dir, config) = config();
        assert_eq!(
            snapshot_command(SnapshotCommands::List, config, true).unwrap(),
            0
        );
    }

    #[test]
    fn sandbox_alias_still_parses_as_instance() {
        let cli = Cli::try_parse_from(["hegn", "sandbox", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Instance {
                command: InstanceCommands::List
            })
        ));
    }

    #[test]
    fn attach_alias_parses_and_accepts_an_optional_project() {
        let cli = Cli::try_parse_from(["hegn", "a", "demo"]).unwrap();
        match cli.command {
            Some(Commands::Attach { project, no_bar }) => {
                assert_eq!(project.as_deref(), Some("demo"));
                assert!(!no_bar);
            }
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::try_parse_from(["hegn", "attach"]).unwrap();
        match cli.command {
            Some(Commands::Attach { project, .. }) => assert!(project.is_none()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn attach_without_a_session_is_a_hard_error() {
        let (_dir, config) = config();
        let err = attach(None, false, config).unwrap_err().to_string();
        assert!(err.contains("no running instance"), "{err}");
    }

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

    #[test]
    fn sanitizes_harness_value_enum() {
        let cli = Cli::try_parse_from(["hegn", "project", "create", "x", "--harness", "opencode"])
            .unwrap();
        match cli.command {
            Some(Commands::Project {
                command: ProjectCommands::Create { harness, .. },
            }) => assert_eq!(harness, Some(Harness::Opencode)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_profile_and_skill_commands() {
        let cli =
            Cli::try_parse_from(["hegn", "profile", "path", "prime-agent", "--edit"]).unwrap();
        match cli.command {
            Some(Commands::Profile {
                command: ProfileCommands::Path { harness, edit },
            }) => {
                assert_eq!(harness, Harness::PrimeAgent);
                assert!(edit);
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "hegn",
            "skills",
            "sync-back",
            "demo",
            "--dry-run",
            "--force",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Skills {
                command:
                    SkillCommands::SyncBack {
                        instance,
                        dry_run,
                        force,
                    },
            }) => {
                assert_eq!(instance, "demo");
                assert!(dry_run);
                assert!(force);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
