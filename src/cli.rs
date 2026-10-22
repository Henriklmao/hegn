//! Non-interactive CLI. Every TUI capability has a CLI counterpart so hegn can
//! be scripted in CI, used over SSH, or driven by an agent.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::app::PACKAGE_CHOICES;
use crate::config::{Config, Harness};
use crate::project::{self, ProjectMeta};
use crate::sandbox::{SandboxManager, SandboxReport, process_alive};
use crate::security::{Decision, SecurityEngine};
use crate::session::harness_command;
use crate::snapshot::{SnapshotMeta, SnapshotStore};

#[derive(Debug, Parser)]
#[command(
    name = "hegn",
    version,
    about = "Firecracker MicroVM sandbox manager and TUI for coding agents",
    long_about = "hegn launches hardware-isolated Firecracker MicroVMs with a shared \
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
    /// Launch a project harness (or editor / lazygit) from the CLI.
    Open {
        /// Project name (under projects_root) or path.
        project: String,
        /// Open in $EDITOR instead of the coding harness.
        #[arg(long, conflicts_with = "lazygit")]
        editor: bool,
        /// Open in lazygit instead of the coding harness.
        #[arg(long)]
        lazygit: bool,
        /// Skip creating a transient git session branch.
        #[arg(long)]
        no_branch: bool,
    },
    /// Manage the Firecracker sandbox lifecycle.
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
    /// Check host prerequisites (KVM, Firecracker, virtiofsd, assets, tools).
    Doctor,
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
    /// Print project metadata and git status.
    Info { project: String },
}

#[derive(Debug, Subcommand)]
pub enum SandboxCommands {
    /// Boot virtiofsd + Firecracker for a project.
    Start {
        project: String,
        /// Harness recorded in the session metadata.
        #[arg(long, value_enum)]
        harness: Option<Harness>,
    },
    /// Stop Firecracker and virtiofsd.
    Stop,
    /// Show VM, virtiofs and pid status.
    Status,
    /// Pause the VM.
    Pause,
    /// Resume the VM.
    Resume,
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
        /// Project path recorded in metadata (defaults to the session project).
        #[arg(long)]
        project: Option<String>,
    },
    /// Load a snapshot (boots a fresh Firecracker process).
    Restore { name: String },
    /// Delete a snapshot directory.
    Delete { name: String },
    /// Show snapshot metadata.
    Show { name: String },
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
            let projects = project::discover(&config.projects_root)?;
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
        } => open(project, editor, lazygit, no_branch, config),
        Commands::Sandbox { command } => sandbox_command(command, config, json),
        Commands::Snapshot { command } => snapshot_command(command, config, json),
        Commands::Check { command, cwd } => check(&command.join(" "), cwd.as_deref(), config, json),
        Commands::Config { command } => config_command(command, config, json),
        Commands::Doctor => doctor(config, json),
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
    let manager = SandboxManager::new(config.clone());
    match command {
        SandboxCommands::Start { project, harness } => {
            let path = resolve_project(&config, &project)?;
            let meta = project::read_meta(&path);
            let harness = harness.or(meta.harness).unwrap_or(config.harness);
            let report = manager.start(&path, harness)?;
            print_report(&report, json)?;
            Ok(0)
        }
        SandboxCommands::Stop => {
            manager.stop()?;
            if json {
                println!("{}", serde_json::json!({"stopped": true}));
            } else {
                println!("Sandbox stopped.");
            }
            Ok(0)
        }
        SandboxCommands::Status => {
            let report = manager.status()?;
            print_report(&report, json)?;
            Ok(0)
        }
        SandboxCommands::Pause => {
            runtime()?.block_on(manager.pause())?;
            if json {
                println!("{}", serde_json::json!({"paused": true}));
            } else {
                println!("Sandbox paused.");
            }
            Ok(0)
        }
        SandboxCommands::Resume => {
            runtime()?.block_on(manager.resume())?;
            if json {
                println!("{}", serde_json::json!({"resumed": true}));
            } else {
                println!("Sandbox resumed.");
            }
            Ok(0)
        }
    }
}

fn snapshot_command(command: SnapshotCommands, config: Config, json: bool) -> Result<i32> {
    let store = SnapshotStore::new(&config.snapshots_dir);
    let manager = SandboxManager::new(config.clone());
    match command {
        SnapshotCommands::List => {
            let snapshots = store.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else if snapshots.is_empty() {
                println!("No snapshots in {}.", store.root().display());
            } else {
                println!("{:<32} {:<22} {:>10}", "NAME", "CREATED", "SIZE");
                for snapshot in snapshots {
                    println!(
                        "{:<32} {:<22} {:>10}",
                        snapshot.name,
                        snapshot.created_at.format("%Y-%m-%d %H:%M:%S"),
                        snapshot.size_label()
                    );
                }
            }
            Ok(0)
        }
        SnapshotCommands::Create { name, project } => {
            if !manager.running() {
                bail!("the sandbox is not running; start it with `hegn sandbox start <project>`");
            }
            let project = match project {
                Some(project) => resolve_project(&config, &project)?,
                None => manager
                    .session()
                    .map(|session| session.project)
                    .context("no session project; pass --project")?,
            };
            let suggested = SnapshotStore::suggested_name(
                project
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("snapshot"),
            );
            let name = name.unwrap_or(suggested);
            let meta = runtime()?.block_on(manager.create_snapshot(&project, &name))?;
            print_snapshot(&meta, json)?;
            Ok(0)
        }
        SnapshotCommands::Restore { name } => {
            let meta = store.load(&name)?;
            runtime()?.block_on(manager.restore_snapshot(&meta))?;
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

fn doctor(config: Config, json: bool) -> Result<i32> {
    let mut checks = Vec::new();
    checks.push(check_tool(
        "firecracker",
        &config.firecracker_bin,
        &["--version"],
    ));
    checks.push(check_tool(
        "virtiofsd",
        &config.virtiofsd_bin,
        &["--version"],
    ));
    checks.push(check_tool("git", Path::new("git"), &["--version"]));
    checks.push(check_tool("lazygit", Path::new("lazygit"), &["--version"]));
    checks.push(Check {
        name: "kvm".into(),
        ok: Path::new("/dev/kvm").exists(),
        detail: if Path::new("/dev/kvm").exists() {
            "/dev/kvm available".into()
        } else {
            "/dev/kvm missing (enable KVM / add user to kvm group)".into()
        },
    });
    checks.push(Check {
        name: "kernel image".into(),
        ok: config.kernel_image.is_file(),
        detail: config.kernel_image.display().to_string(),
    });
    checks.push(Check {
        name: "rootfs image".into(),
        ok: config.rootfs_image.is_file(),
        detail: config.rootfs_image.display().to_string(),
    });
    checks.push(Check {
        name: "projects root".into(),
        ok: config.projects_root.is_dir(),
        detail: config.projects_root.display().to_string(),
    });
    checks.push(Check {
        name: "backend".into(),
        ok: config.effective_backend() != crate::config::BackendKind::Firecracker
            || (config.kernel_image.is_file() && config.rootfs_image.is_file()),
        detail: format!(
            "{} (configured {})",
            config.effective_backend().label(),
            config.backend.label()
        ),
    });
    checks.push(Check {
        name: "editor".into(),
        ok: std::env::var_os("EDITOR").is_some() || std::env::var_os("VISUAL").is_some(),
        detail: std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "$EDITOR is unset".into()),
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&checks)?);
    } else {
        for check in &checks {
            println!(
                "{} {:<18} {}",
                if check.ok { "✓" } else { "✗" },
                check.name,
                check.detail
            );
        }
        println!("\nConfig: {}", Config::config_path().display());
    }
    Ok(0)
}

#[derive(Debug, Serialize)]
struct Check {
    name: String,
    ok: bool,
    detail: String,
}

fn check_tool(name: &str, binary: &Path, args: &[&str]) -> Check {
    match Command::new(binary).args(args).output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let version = stdout.lines().next().unwrap_or("").trim();
            Check {
                name: name.into(),
                ok: true,
                detail: if version.is_empty() {
                    output.status.to_string()
                } else {
                    version.to_string()
                },
            }
        }
        Ok(output) => Check {
            name: name.into(),
            ok: false,
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        },
        Err(err) => Check {
            name: name.into(),
            ok: false,
            detail: err.to_string(),
        },
    }
}

fn open(
    project: String,
    editor: bool,
    lazygit: bool,
    no_branch: bool,
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

    let git = project::read_git_summary(&path);
    if config.auto_branch && git.is_repo && !no_branch {
        match project::create_session_branch(&path) {
            Ok(branch) => println!("Session branch: {branch}"),
            Err(err) => eprintln!("warning: could not create session branch: {err}"),
        }
    }

    let meta = project::read_meta(&path);
    let harness = meta.harness.unwrap_or(config.harness);
    let (program, args) = harness_command(harness);
    let status = Command::new(&program)
        .args(&args)
        .current_dir(&path)
        .env("HEGN", "1")
        .env("HEGN_WORKSPACE", &path)
        .status()
        .with_context(|| format!("failed to launch harness '{}' ({program})", harness.label()))?;
    Ok(status.code().unwrap_or(1))
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

fn print_report(report: &SandboxReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!("state        {}", report.state);
    println!("backend      {}", report.backend);
    if let Some(project) = &report.project {
        println!("project      {}", project.display());
    }
    println!(
        "resources    {} vCPU · {} MiB",
        report.vcpus, report.memory_mib
    );
    println!("api socket   {}", report.api_socket.display());
    println!(
        "virtiofs     {} ({})",
        if report.virtiofs_running {
            "running"
        } else {
            "stopped"
        },
        report.virtiofs_socket.display()
    );
    if let Some(pid) = report.firecracker_pid {
        println!(
            "firecracker  pid {pid} ({})",
            if process_alive(pid) { "alive" } else { "dead" }
        );
    }
    if let Some(pid) = report.virtiofs_pid {
        println!(
            "virtiofsd    pid {pid} ({})",
            if process_alive(pid) { "alive" } else { "dead" }
        );
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
        println!("  snapshot {}", meta.snapshot_path.display());
        println!("  memory   {}", meta.mem_file_path.display());
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
        config.templates_dir = dir.path().join("templates");
        config.backend = BackendKind::Pty;
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
        let code = sandbox_command(SandboxCommands::Status, config, true).unwrap();
        assert_eq!(code, 0);
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
