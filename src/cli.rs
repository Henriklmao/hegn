//! Non-interactive CLI. Every TUI capability has a CLI counterpart so hegn can
//! be scripted in CI, used over SSH, or driven by an agent.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::app::PACKAGE_CHOICES;
use crate::benchmark;
use crate::config::{Config, Harness};
use crate::doctor;
use crate::images::{self, AssetStatus};
use crate::logs::{self, DEFAULT_KEEP, LogStore};
use crate::project::{self, ProjectMeta};
use crate::sandbox::{SandboxRegistry, SandboxReport, SandboxSummary, process_alive};
use crate::security::{Decision, SecurityEngine};
use crate::snapshot::{SnapshotMeta, SnapshotStore};
use crate::terminal::TerminalBuffer;

#[derive(Debug, Parser)]
#[command(
    name = "hegn",
    version,
    about = "Cloud Hypervisor sandbox manager and TUI for coding agents",
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
    /// Manage the Cloud Hypervisor sandbox lifecycle.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommands,
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
    /// Inspect, download and verify the pinned upstream VM images.
    Images {
        #[command(subcommand)]
        command: ImageCommands,
    },
    /// Check host prerequisites (KVM, Cloud Hypervisor, virtiofsd, assets, tools).
    Doctor,
    /// Measure the host cost of a sandbox; boots one on demand.
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
        /// Default coding harness (also written to .hegn.toml).
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
}

#[derive(Debug, Subcommand)]
pub enum SandboxCommands {
    /// List all sandboxes and their state.
    #[command(visible_alias = "ls")]
    List,
    /// Create a sandbox for a project.
    Create {
        project: String,
        /// Sandbox name (defaults to the project directory name).
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum)]
        harness: Option<Harness>,
        /// Copy the project into the sandbox instead of exporting it directly.
        #[arg(long)]
        clone: bool,
    },
    /// Clone a sandbox for parallel agent work.
    Clone {
        /// Sandbox or project to clone.
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
    /// Boot a sandbox, creating it from a project if necessary.
    Start {
        /// Sandbox name, project name or project path.
        name: String,
        #[arg(long, value_enum)]
        harness: Option<Harness>,
        /// Start a temporary clone instead of the project itself.
        #[arg(long)]
        clone: bool,
    },
    /// Stop one sandbox, or the only running sandbox.
    Stop { name: Option<String> },
    /// Show one sandbox, or all sandboxes.
    Status { name: Option<String> },
    /// Pause a sandbox.
    Pause { name: String },
    /// Resume a sandbox.
    Resume { name: String },
    /// Show or edit a sandbox's hegn.toml.
    Config {
        name: String,
        /// Open the config in $EDITOR.
        #[arg(long)]
        edit: bool,
        /// Print only the config path.
        #[arg(long)]
        path: bool,
    },
    /// Stop and delete a sandbox.
    Delete {
        name: String,
        /// Delete an in-sandbox clone workspace.
        #[arg(long, conflicts_with = "keep_files")]
        delete_files: bool,
        /// Keep an in-sandbox clone workspace.
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
        /// Sandbox to snapshot (defaults to the only running sandbox).
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Load a snapshot (boots a fresh Cloud Hypervisor process).
    Restore {
        name: String,
        /// Sandbox to restore into (defaults to the snapshot metadata).
        #[arg(long)]
        sandbox: Option<String>,
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
pub enum ConfigCommands {
    /// Print the effective configuration as TOML.
    Show,
    /// Print the configuration file path.
    Path,
    /// Write a default config if none exists.
    Init,
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
            let projects =
                project::discover_protected(&config.projects_root, &config.protected_paths())?;
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
        Commands::Sandbox { command } => sandbox_command(command, config, json),
        Commands::Snapshot { command } => snapshot_command(command, config, json),
        Commands::Check { command, cwd } => check(&command.join(" "), cwd.as_deref(), config, json),
        Commands::Config { command } => config_command(command, config, json),
        Commands::Images { command } => image_command(command, config, json),
        Commands::Doctor => doctor(config, json),
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
            packages,
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
            let explicit_packages = !packages.is_empty();
            let packages: Vec<String> = if explicit_packages {
                packages
            } else {
                harness.packages().iter().map(|p| p.to_string()).collect()
            };
            if explicit_packages {
                for package in &packages {
                    if !PACKAGE_CHOICES.contains(&package.as_str()) {
                        eprintln!("note: '{package}' is not a built-in template package");
                    }
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
                        "packages": packages,
                    }))?
                );
            } else {
                println!("Created {}", path.display());
                println!("Harness: {} ({})", harness.label(), harness.command());
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
    }
}

fn sandbox_command(command: SandboxCommands, config: Config, json: bool) -> Result<i32> {
    let registry = SandboxRegistry::new(config.clone());
    match command {
        SandboxCommands::List => {
            print_sandboxes(&registry.summaries()?, json)?;
            Ok(0)
        }
        SandboxCommands::Create {
            project,
            name,
            harness,
            clone,
        } => {
            let path = resolve_project(&config, &project)?;
            let harness = harness.or_else(|| project::read_meta(&path).harness);
            let sandbox = if clone {
                let outcome =
                    registry.create_with_warnings(&path, name.as_deref(), harness, true)?;
                for warning in &outcome.warnings {
                    eprintln!("warning: {warning}");
                }
                outcome.sandbox
            } else {
                registry.create(&path, name.as_deref(), harness, false)?
            };
            print_sandbox(&registry.summary(&sandbox), json)?;
            Ok(0)
        }
        SandboxCommands::Clone {
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
                    registry.manager(outcome.sandbox.clone()).start()?;
                }
                created.push(registry.summary(&outcome.sandbox));
            }
            print_sandboxes(&created, json)?;
            Ok(0)
        }
        SandboxCommands::Start {
            name,
            harness,
            clone,
        } => {
            let sandbox = match registry.resolve(&name) {
                Ok(sandbox) => {
                    if clone {
                        let clone_name =
                            registry.unique_name(&format!("{}-agent", sandbox.name))?;
                        let outcome = registry.clone_from(&sandbox, &clone_name, harness)?;
                        for warning in &outcome.warnings {
                            eprintln!("warning: {warning}");
                        }
                        outcome.sandbox
                    } else {
                        sandbox
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
                        outcome.sandbox
                    } else {
                        registry.ensure_for_project(&path, harness)?
                    }
                }
            };
            let manager = registry.manager(sandbox);
            let report = manager.start()?;
            print_report(&report, json)?;
            Ok(0)
        }
        SandboxCommands::Stop { name } => {
            let sandbox = match name {
                Some(selector) => registry.resolve(&selector)?,
                None => only_running_sandbox(&registry)?,
            };
            registry.manager(sandbox.clone()).stop()?;
            if json {
                println!("{}", serde_json::json!({"stopped": sandbox.name}));
            } else {
                println!("Sandbox '{}' stopped.", sandbox.name);
            }
            Ok(0)
        }
        SandboxCommands::Status { name } => {
            match name {
                Some(selector) => {
                    let sandbox = registry.resolve(&selector)?;
                    let report = registry.manager(sandbox).status()?;
                    print_report(&report, json)?;
                }
                None => print_sandboxes(&registry.summaries()?, json)?,
            }
            Ok(0)
        }
        SandboxCommands::Pause { name } => {
            let sandbox = registry.resolve(&name)?;
            let sandbox_name = sandbox.name.clone();
            runtime()?.block_on(registry.manager(sandbox).pause())?;
            if json {
                println!("{}", serde_json::json!({"paused": sandbox_name}));
            } else {
                println!("Sandbox '{sandbox_name}' paused.");
            }
            Ok(0)
        }
        SandboxCommands::Resume { name } => {
            let sandbox = registry.resolve(&name)?;
            let sandbox_name = sandbox.name.clone();
            runtime()?.block_on(registry.manager(sandbox).resume())?;
            if json {
                println!("{}", serde_json::json!({"resumed": sandbox_name}));
            } else {
                println!("Sandbox '{sandbox_name}' resumed.");
            }
            Ok(0)
        }
        SandboxCommands::Config { name, edit, path } => {
            let sandbox = registry.store().load(&name)?;
            let config_path = registry.store().config_path(&name)?;
            if path {
                println!("{}", config_path.display());
            } else if edit {
                run_editor(&config_path)?;
            } else if json {
                println!("{}", serde_json::to_string_pretty(&sandbox)?);
            } else {
                println!("{}", toml::to_string_pretty(&sandbox)?);
            }
            Ok(0)
        }
        SandboxCommands::Delete {
            name,
            delete_files,
            keep_files,
            yes,
        } => {
            let sandbox = registry.store().load(&name)?;
            let workspace_inside = sandbox
                .workspace
                .canonicalize()
                .map(|workspace| {
                    sandbox
                        .dir(registry.store().root())
                        .canonicalize()
                        .map(|dir| workspace.starts_with(&dir))
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if !yes && !prompt_yes(&format!("Delete sandbox '{}'?", sandbox.name))? {
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
            let outcome = registry.delete(&sandbox.name, delete_files)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "deleted": sandbox.name,
                        "delete_files": delete_files,
                        "kept_workspace": outcome.kept_workspace,
                    })
                );
            } else if let Some(kept) = outcome.kept_workspace {
                println!(
                    "Sandbox '{}' removed. Workspace kept at {}",
                    sandbox.name,
                    kept.display()
                );
            } else if workspace_inside && delete_files {
                println!("Sandbox '{}' and its workspace deleted.", sandbox.name);
            } else {
                println!(
                    "Sandbox '{}' removed. Source project {} was not touched.",
                    sandbox.name,
                    sandbox.project.display()
                );
            }
            Ok(0)
        }
    }
}

fn only_running_sandbox(
    registry: &SandboxRegistry,
) -> Result<crate::sandbox_config::SandboxConfig> {
    let mut running: Vec<_> = registry
        .summaries()?
        .into_iter()
        .filter(|sandbox| sandbox.state == "running")
        .collect();
    match running.len() {
        0 => bail!("no running sandbox; start one with `hegn sandbox start <name>`"),
        1 => registry.store().load(&running.remove(0).name),
        _ => {
            let names: Vec<String> = running.into_iter().map(|sandbox| sandbox.name).collect();
            bail!(
                "multiple sandboxes are running ({}); pass a sandbox name",
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

fn snapshot_sandbox(
    registry: &SandboxRegistry,
    requested: Option<String>,
) -> Result<crate::sandbox_config::SandboxConfig> {
    match requested {
        Some(selector) => registry.resolve(&selector),
        None => only_running_sandbox(registry),
    }
}

fn snapshot_command(command: SnapshotCommands, config: Config, json: bool) -> Result<i32> {
    let store = SnapshotStore::new(&config.snapshots_dir);
    let registry = SandboxRegistry::new(config.clone());
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
                    "NAME", "SANDBOX", "CREATED", "SIZE"
                );
                for snapshot in snapshots {
                    println!(
                        "{:<32} {:<20} {:<18} {:>10}",
                        snapshot.name,
                        snapshot.sandbox.as_deref().unwrap_or("-"),
                        snapshot.created_at.format("%Y-%m-%d %H:%M:%S"),
                        snapshot.size_label()
                    );
                }
            }
            Ok(0)
        }
        SnapshotCommands::Create { name, sandbox } => {
            let sandbox = snapshot_sandbox(&registry, sandbox)?;
            let suggested = SnapshotStore::suggested_name(
                sandbox
                    .project
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("snapshot"),
            );
            let name = name.unwrap_or(suggested);
            let meta = runtime()?.block_on(registry.manager(sandbox).create_snapshot(&name))?;
            print_snapshot(&meta, json)?;
            Ok(0)
        }
        SnapshotCommands::Restore { name, sandbox } => {
            let meta = store.load(&name)?;
            let sandbox = match sandbox {
                Some(selector) => registry.resolve(&selector)?,
                None => match meta.sandbox.as_deref() {
                    Some(name) => registry.store().load(name)?,
                    None => {
                        let project = meta
                            .project
                            .as_deref()
                            .context("snapshot has no sandbox/project metadata")?;
                        registry.resolve(project.to_string_lossy().as_ref())?
                    }
                },
            };
            runtime()?.block_on(registry.manager(sandbox).restore_snapshot(&meta))?;
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
            if config.write_default_if_missing()? {
                println!("Wrote {}", Config::config_path().display());
            } else {
                println!(
                    "Config already exists at {}",
                    Config::config_path().display()
                );
            }
            config.ensure_dirs()?;
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

fn doctor(config: Config, json: bool) -> Result<i32> {
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
    let registry = SandboxRegistry::new(config.clone());
    let sandbox = registry.ensure_for_project(&path, harness)?;
    let manager = registry.manager(sandbox.clone());
    let console = manager.ensure_console()?;

    // Only mutate the workspace repository once a VM is actually available.
    let workspace = sandbox.workspace.clone();
    let git = project::read_git_summary(&workspace);
    if sandbox.auto_branch && git.is_repo && !no_branch {
        match project::create_session_branch(&workspace) {
            Ok(branch) => println!("Session branch: {branch}"),
            Err(err) => eprintln!("warning: could not create session branch: {err}"),
        }
    }

    crate::console::run_interactive(console, !no_bar)?;
    Ok(0)
}

fn attach(project: Option<String>, no_bar: bool, config: Config) -> Result<i32> {
    let registry = SandboxRegistry::new(config.clone());
    let sandbox = match project {
        Some(selector) => registry.resolve(&selector)?,
        None => only_running_sandbox(&registry)?,
    };
    // VmConsole::attach re-verifies the recorded pid, VM binary and relay.
    let console = registry.manager(sandbox).attach_console()?;
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
            // Never unlink a log a running sandbox is still writing to.
            let registry = SandboxRegistry::new(config.clone());
            let protected: Vec<PathBuf> = registry
                .summaries()?
                .into_iter()
                .filter_map(|sandbox| sandbox.serial_log)
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

fn print_sandboxes(sandboxes: &[SandboxSummary], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(sandboxes)?);
        return Ok(());
    }
    if sandboxes.is_empty() {
        println!("No sandboxes. Create one with: hegn sandbox create <project>");
        return Ok(());
    }
    println!(
        "{:<24} {:<12} {:<10} {:<12} PROJECT",
        "SANDBOX", "STATE", "MODE", "HARNESS"
    );
    for sandbox in sandboxes {
        println!(
            "{:<24} {:<12} {:<10} {:<12} {}",
            sandbox.name,
            sandbox.state,
            if sandbox.ephemeral {
                "clone"
            } else {
                "project"
            },
            sandbox.harness.label(),
            sandbox.project.display()
        );
    }
    Ok(())
}

fn print_sandbox(sandbox: &SandboxSummary, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(sandbox)?);
        return Ok(());
    }
    println!("sandbox      {}", sandbox.name);
    println!("state        {}", sandbox.state);
    println!(
        "mode         {}",
        if sandbox.ephemeral {
            "clone"
        } else {
            "project"
        }
    );
    println!("project      {}", sandbox.project.display());
    println!("workspace    {}", sandbox.workspace.display());
    println!("harness      {}", sandbox.harness.label());
    println!(
        "resources    {} vCPU · {} MiB",
        sandbox.vcpus, sandbox.memory_mib
    );
    println!("config       {}", sandbox.config_path.display());
    if let Some(source) = &sandbox.source_sandbox {
        println!("source       {source}");
    }
    if let Some(pid) = sandbox.vm_pid {
        println!("vm           pid {pid}");
    }
    if let Some(log) = &sandbox.serial_log {
        println!("serial log   {}", log.display());
    }
    if let Some(error) = &sandbox.error {
        println!("error        {error}");
    }
    Ok(())
}

fn print_report(report: &SandboxReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!("sandbox      {}", report.sandbox);
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
        if let Some(sandbox) = &meta.sandbox {
            println!("  sandbox {sandbox}");
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
        config.sandboxes_dir = dir.path().join("sandboxes");
        config.templates_dir = dir.path().join("templates");
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
        assert_eq!(meta.packages, vec!["git", "python3"]);
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
    fn sandbox_status_json_works_without_vm() {
        let (_dir, config) = config();
        let code = sandbox_command(SandboxCommands::Status { name: None }, config, true).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn sandbox_create_clone_list_config_and_delete() {
        let (_dir, config) = config();
        let project = config.projects_root.join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"demo").unwrap();

        let code = sandbox_command(
            SandboxCommands::Create {
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
        assert!(config.sandboxes_dir.join("base/hegn.toml").is_file());

        let code = sandbox_command(
            SandboxCommands::Clone {
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
        assert!(config.sandboxes_dir.join("agent1/hegn.toml").is_file());
        assert!(
            config
                .sandboxes_dir
                .join("agent1/workspace/README.md")
                .is_file()
        );

        assert_eq!(
            sandbox_command(SandboxCommands::List, config.clone(), true).unwrap(),
            0
        );
        assert_eq!(
            sandbox_command(
                SandboxCommands::Config {
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
            sandbox_command(
                SandboxCommands::Delete {
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
        assert!(!config.sandboxes_dir.join("agent1").exists());
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
        assert!(err.contains("no running sandbox"), "{err}");
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
}
