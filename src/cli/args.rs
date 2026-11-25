//! Command-line surface: the root [`Cli`] parser and every clap subcommand
//! enum. Keeping the AST separate from the handlers makes the dispatch tree
//! readable and lets argument parsing be tested without touching a host.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::agent::launch::Integration;
use crate::config::Harness;
use crate::session::logs::DEFAULT_KEEP;

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
        /// Command to type into the guest after attaching (for example an agent).
        #[arg(long, value_name = "COMMAND")]
        command: Option<String>,
    },
    /// Attach to the serial console of the already running VM (tmux-style).
    ///
    /// Never boots or restarts a VM. The bottom line shows the project name,
    /// harness, VM pid and persistent serial log; Ctrl-B detaches.
    #[command(visible_alias = "a")]
    Attach {
        /// VM name, or a project that names its VM.
        project: Option<String>,
        /// Do not reserve a status bar line (for full-screen guest programs).
        #[arg(long)]
        no_bar: bool,
        /// Command to type into the guest after attaching.
        #[arg(long, value_name = "COMMAND")]
        command: Option<String>,
    },
    /// Launch the configured agent or maintenance command in a VM.
    Launch {
        /// VM name, project name or project path (defaults to the only
        /// running VM).
        selector: Option<String>,
        /// Named command from the `[launch]` configuration.
        #[arg(long, value_name = "NAME")]
        command: Option<String>,
        /// Override the terminal integration.
        #[arg(long, value_enum)]
        integration: Option<Integration>,
        /// Do not reserve a status bar line.
        #[arg(long)]
        no_bar: bool,
    },
    /// List, inspect and prune persistent serial-console logs.
    #[command(visible_alias = "log")]
    Logs {
        #[command(subcommand)]
        command: Option<LogCommands>,
    },
    /// Create and control Cloud Hypervisor VMs.
    Vm {
        #[command(subcommand)]
        command: VmCommands,
    },
    /// Removed v1 command group; prints the v2 mapping and exits non-zero.
    ///
    /// The clap tree is kept so an existing script gets a readable migration
    /// message instead of an "unknown command" usage error.
    #[command(alias = "sandbox", hide = true)]
    Instance {
        #[command(subcommand)]
        command: Option<InstanceCommands>,
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
        /// AGENTS.md template to copy.
        #[arg(long)]
        template: Option<PathBuf>,
        /// Assign the new project to an existing VM.
        #[arg(long, value_name = "VM", conflicts_with = "solo")]
        vm: Option<String>,
        /// Create a one-member VM named after the project without prompting.
        #[arg(long, conflicts_with = "vm")]
        solo: bool,
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
        /// Assign the imported project to an existing VM.
        #[arg(long, value_name = "VM", conflicts_with = "solo")]
        vm: Option<String>,
        /// Create a one-member VM named after the project without prompting.
        #[arg(long, conflicts_with = "vm")]
        solo: bool,
    },
    /// Print project metadata, VM membership and git status.
    Info { project: String },
    /// Assign an existing project to a VM.
    Assign {
        /// Project picker name or path.
        project: String,
        /// VM to assign the project to.
        #[arg(long, value_name = "VM")]
        vm: String,
        /// Export the project read-only (virtiofsd and the guest mount).
        #[arg(long)]
        read_only: bool,
    },
    /// Remove a project from its VM. The VM and all other members stay.
    Unassign { project: String },
    /// Fork a project into an isolated clone or a nested git worktree.
    Fork {
        /// Parent project picker name or path.
        parent: String,
        /// Name for the new workspace project.
        #[arg(long)]
        name: Option<String>,
        /// Fork strategy (default: clone).
        #[arg(long, value_enum, default_value_t = ForkModeArg::Clone)]
        mode: ForkModeArg,
        /// Optional worktree path inside the parent (worktree mode only).
        #[arg(long, value_name = "PATH")]
        target: Option<PathBuf>,
    },
    /// Remove a project from the picker or delete a managed workspace.
    ///
    /// Git status is printed first. `--delete-files` also removes the project
    /// directory; `--keep-files` only unregisters it. Imported projects are
    /// symlinks and their original directory is never deleted. A VM is never
    /// deleted by this command.
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

/// CLI mirror of [`crate::project::ForkMode`], kept local so the argument
/// surface can use clap's `ValueEnum` without changing the library type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ForkModeArg {
    Clone,
    Worktree,
}

#[derive(Debug, Subcommand)]
pub enum VmCommands {
    /// List every VM, its state and exported member count.
    #[command(visible_alias = "ls")]
    List,
    /// Create a VM profile without booting it.
    Create {
        name: String,
        /// Harness tool profile installed into this VM.
        #[arg(long, value_enum)]
        harness: Option<Harness>,
        /// Number of virtual CPUs (1–32).
        #[arg(long, value_name = "N")]
        vcpus: Option<u8>,
        /// Guest memory in MiB (128–262144).
        #[arg(long, value_name = "MIB")]
        memory: Option<u32>,
        /// Extra apt package on top of the harness preset (repeatable).
        #[arg(long = "package", value_name = "PACKAGE", value_delimiter = ',')]
        packages: Vec<String>,
    },
    /// Show one VM, its state and its project members.
    Show { name: String },
    /// Print or edit a VM's vm.toml.
    Config {
        name: String,
        /// Open the config in $EDITOR.
        #[arg(long)]
        edit: bool,
        /// Print only the config path.
        #[arg(long)]
        path: bool,
    },
    /// Boot a VM and every project it exports.
    Start { name: String },
    /// Stop a VM, its relay and every member virtiofsd.
    Stop { name: String },
    /// Stop a running VM and start it again.
    Restart { name: String },
    /// Restart a VM so a changed config or membership takes effect.
    Apply { name: String },
    /// Delete the private rootfs; the next start copies a fresh one.
    Rebuild {
        name: String,
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Choose the member that /workspace points at.
    Primary { name: String, project: String },
    /// List a VM's project members and guest paths.
    Projects { name: String },
    /// Attach the console and start a shell or harness in a project directory.
    Shell {
        name: String,
        /// Member project to start in (defaults to the VM primary).
        #[arg(long, value_name = "PROJECT")]
        project: Option<String>,
    },
    /// Delete a VM after unassigning its projects.
    Delete {
        name: String,
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
        /// Delete the VM directory, including hegn-managed clone workspaces.
        #[arg(long)]
        delete_files: bool,
    },
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
    List {
        /// Only show logs from this VM's member projects.
        #[arg(long, alias = "instance", value_name = "VM")]
        vm: Option<String>,
    },
    /// Print the path of a log (defaults to the newest).
    Path {
        name: Option<String>,
        /// Only resolve logs from this VM's member projects.
        #[arg(long, alias = "instance", value_name = "VM")]
        vm: Option<String>,
    },
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
        /// Only resolve logs from this VM's member projects.
        #[arg(long, alias = "instance", value_name = "VM")]
        vm: Option<String>,
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
    /// Reinstall the nftables rules and endpoint DNAT without touching the TAP
    /// pool (requires root).
    Reload {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sandbox_alias_still_parses_as_instance() {
        let cli = Cli::try_parse_from(["hegn", "sandbox", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Instance {
                command: Some(InstanceCommands::List)
            })
        ));
    }

    #[test]
    fn attach_alias_parses_and_accepts_an_optional_project() {
        let cli = Cli::try_parse_from(["hegn", "a", "demo"]).unwrap();
        match cli.command {
            Some(Commands::Attach {
                project,
                no_bar,
                command,
            }) => {
                assert_eq!(project.as_deref(), Some("demo"));
                assert!(!no_bar);
                assert!(command.is_none());
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
    fn launch_parses_a_named_command_and_integration() {
        let cli = Cli::try_parse_from([
            "hegn",
            "launch",
            "demo",
            "--command",
            "review",
            "--integration",
            "herdr",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Launch {
                selector,
                command,
                integration,
                no_bar,
            }) => {
                assert_eq!(selector.as_deref(), Some("demo"));
                assert_eq!(command.as_deref(), Some("review"));
                assert_eq!(integration, Some(Integration::Herdr));
                assert!(!no_bar);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sanitizes_harness_value_enum() {
        let cli =
            Cli::try_parse_from(["hegn", "vm", "create", "x", "--harness", "opencode"]).unwrap();
        match cli.command {
            Some(Commands::Vm {
                command: VmCommands::Create { harness, .. },
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

    #[test]
    fn visible_aliases_map_to_their_commands() {
        let cli = Cli::try_parse_from(["hegn", "ls"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Projects { query: None })
        ));

        let cli = Cli::try_parse_from(["hegn", "log"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Logs { command: None })
        ));

        let cli = Cli::try_parse_from(["hegn", "instance", "ls"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Instance {
                command: Some(InstanceCommands::List)
            })
        ));

        let cli = Cli::try_parse_from(["hegn", "snapshot", "ls"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Snapshot {
                command: SnapshotCommands::List
            })
        ));

        let cli = Cli::try_parse_from(["hegn", "logs", "ls"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Logs {
                command: Some(LogCommands::List { vm: None })
            })
        ));
    }

    #[test]
    fn missing_required_arguments_are_parse_errors() {
        assert!(Cli::try_parse_from(["hegn", "open"]).is_err());
        assert!(Cli::try_parse_from(["hegn", "project"]).is_err());
        assert!(Cli::try_parse_from(["hegn", "project", "import"]).is_err());
        assert!(Cli::try_parse_from(["hegn", "check"]).is_err());
        // The hidden v1 command parses and is handled as a migration error;
        // every `hegn vm` command still requires its subcommand.
        assert!(Cli::try_parse_from(["hegn", "instance"]).is_ok());
        assert!(Cli::try_parse_from(["hegn", "vm"]).is_err());
        assert!(Cli::try_parse_from(["hegn", "logs", "show", "--lines"]).is_err());
    }

    #[test]
    fn conflicting_flags_are_parse_errors() {
        assert!(Cli::try_parse_from(["hegn", "open", "demo", "--editor", "--lazygit"]).is_err());
        assert!(
            Cli::try_parse_from([
                "hegn",
                "project",
                "delete",
                "demo",
                "--delete-files",
                "--keep-files",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["hegn", "benchmark", "demo", "--clean"]).is_err());
        assert!(
            Cli::try_parse_from([
                "hegn",
                "instance",
                "delete",
                "demo",
                "--delete-files",
                "--keep-files",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["hegn", "vm", "create", "demo", "--harness", "unknown"]).is_err()
        );
        // Harness and packages belong to `hegn vm create`; project creation
        // rejects the old project-level flags.
        assert!(
            Cli::try_parse_from(["hegn", "project", "create", "demo", "--harness", "opencode"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["hegn", "project", "create", "demo", "--package", "git"]).is_err()
        );
    }

    #[test]
    fn global_flags_are_accepted_before_and_after_a_subcommand() {
        let cli = Cli::try_parse_from([
            "hegn",
            "--config",
            "/tmp/hegn.toml",
            "--json",
            "config",
            "show",
        ])
        .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/hegn.toml")));
        assert!(cli.json);
        assert!(matches!(
            cli.command,
            Some(Commands::Config {
                command: ConfigCommands::Show
            })
        ));

        let cli = Cli::try_parse_from(["hegn", "config", "show", "--json"]).unwrap();
        assert!(cli.json);
        assert!(cli.config.is_none());
    }

    #[test]
    fn optional_arguments_default_to_safe_values() {
        let cli = Cli::try_parse_from(["hegn", "instance", "clone", "demo"]).unwrap();
        match cli.command {
            Some(Commands::Instance {
                command:
                    Some(InstanceCommands::Clone {
                        source,
                        name,
                        count,
                        harness,
                        no_start,
                    }),
            }) => {
                assert_eq!(source, "demo");
                assert!(name.is_none());
                assert_eq!(count, 1);
                assert!(harness.is_none());
                assert!(!no_start);
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from(["hegn", "logs", "show"]).unwrap();
        match cli.command {
            Some(Commands::Logs {
                command: Some(LogCommands::Show { lines, raw, .. }),
            }) => {
                assert_eq!(lines, 80);
                assert!(!raw);
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from(["hegn", "logs", "prune"]).unwrap();
        match cli.command {
            Some(Commands::Logs {
                command: Some(LogCommands::Prune { keep }),
            }) => assert_eq!(keep, DEFAULT_KEEP),
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from(["hegn", "snapshot", "create"]).unwrap();
        match cli.command {
            Some(Commands::Snapshot {
                command: SnapshotCommands::Create { name, instance },
            }) => {
                assert!(name.is_none());
                assert!(instance.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_command_specific_options() {
        let cli = Cli::try_parse_from(["hegn", "check", "--cwd", "/tmp", "echo", "hi"]).unwrap();
        match cli.command {
            Some(Commands::Check { command, cwd }) => {
                assert_eq!(command.join(" "), "echo hi");
                assert_eq!(cwd, Some(PathBuf::from("/tmp")));
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "hegn",
            "project",
            "create",
            "demo",
            "--template",
            "/tmp/AGENTS.md",
            "--vm",
            "rust",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Project {
                command:
                    ProjectCommands::Create {
                        name,
                        template,
                        vm,
                        solo,
                    },
            }) => {
                assert_eq!(name, "demo");
                assert_eq!(template, Some(PathBuf::from("/tmp/AGENTS.md")));
                assert_eq!(vm.as_deref(), Some("rust"));
                assert!(!solo);
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "hegn",
            "images",
            "download",
            "--force",
            "--allow-unverified",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Images {
                command: ImageCommands::Download {
                    force: true,
                    allow_unverified: true
                }
            })
        ));

        let cli = Cli::try_parse_from(["hegn", "network", "setup", "--user", "alice", "--dry-run"])
            .unwrap();
        match cli.command {
            Some(Commands::Network {
                command: NetworkCommands::Setup { user, dry_run },
            }) => {
                assert_eq!(user.as_deref(), Some("alice"));
                assert!(dry_run);
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from(["hegn", "benchmark", "--clean"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Benchmark {
                project: None,
                clean: true
            })
        ));
    }

    #[test]
    fn parses_the_vm_control_plane() {
        let cli = Cli::try_parse_from(["hegn", "vm", "ls"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Vm {
                command: VmCommands::List
            })
        ));

        let cli = Cli::try_parse_from([
            "hegn",
            "vm",
            "create",
            "rust",
            "--harness",
            "opencode",
            "--vcpus",
            "8",
            "--memory",
            "16384",
            "--package",
            "rustup,ripgrep",
            "--package",
            "fd-find",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Vm {
                command:
                    VmCommands::Create {
                        name,
                        harness,
                        vcpus,
                        memory,
                        packages,
                    },
            }) => {
                assert_eq!(name, "rust");
                assert_eq!(harness, Some(Harness::Opencode));
                assert_eq!(vcpus, Some(8));
                assert_eq!(memory, Some(16_384));
                assert_eq!(
                    packages,
                    vec![
                        "rustup".to_string(),
                        "ripgrep".to_string(),
                        "fd-find".to_string()
                    ]
                );
            }
            other => panic!("unexpected {other:?}"),
        }

        let cli = Cli::try_parse_from(["hegn", "vm", "config", "rust", "--path"]).unwrap();
        match cli.command {
            Some(Commands::Vm {
                command: VmCommands::Config { name, edit, path },
            }) => {
                assert_eq!(name, "rust");
                assert!(!edit);
                assert!(path);
            }
            other => panic!("unexpected {other:?}"),
        }

        for (arguments, expected) in [
            (vec!["hegn", "vm", "show", "rust"], "show"),
            (vec!["hegn", "vm", "start", "rust"], "start"),
            (vec!["hegn", "vm", "stop", "rust"], "stop"),
            (vec!["hegn", "vm", "restart", "rust"], "restart"),
            (vec!["hegn", "vm", "apply", "rust"], "apply"),
            (vec!["hegn", "vm", "projects", "rust"], "projects"),
        ] {
            assert!(Cli::try_parse_from(arguments).is_ok(), "{expected}");
        }

        let cli = Cli::try_parse_from(["hegn", "vm", "rebuild", "rust", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Vm {
                command: VmCommands::Rebuild { name, yes: true }
            }) if name == "rust"
        ));

        let cli = Cli::try_parse_from(["hegn", "vm", "primary", "rust", "parser"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Vm {
                command: VmCommands::Primary { name, project }
            }) if name == "rust" && project == "parser"
        ));

        let cli =
            Cli::try_parse_from(["hegn", "vm", "shell", "rust", "--project", "parser"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Vm {
                command: VmCommands::Shell { name, project: Some(project) }
            }) if name == "rust" && project == "parser"
        ));

        let cli = Cli::try_parse_from(["hegn", "vm", "delete", "rust", "--yes", "--delete-files"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Vm {
                command: VmCommands::Delete {
                    name,
                    yes: true,
                    delete_files: true
                }
            }) if name == "rust"
        ));
    }

    #[test]
    fn parses_the_project_membership_and_fork_flags() {
        let cli =
            Cli::try_parse_from(["hegn", "project", "create", "demo", "--vm", "rust"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Project {
                command: ProjectCommands::Create { vm: Some(vm), solo: false, .. }
            }) if vm == "rust"
        ));

        let cli = Cli::try_parse_from(["hegn", "project", "create", "demo", "--solo"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Project {
                command: ProjectCommands::Create {
                    vm: None,
                    solo: true,
                    ..
                }
            })
        ));
        assert!(
            Cli::try_parse_from([
                "hegn", "project", "create", "demo", "--solo", "--vm", "rust"
            ])
            .is_err()
        );

        let cli = Cli::try_parse_from([
            "hegn",
            "project",
            "assign",
            "parser",
            "--vm",
            "rust",
            "--read-only",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Project {
                command: ProjectCommands::Assign {
                    project,
                    vm,
                    read_only: true
                }
            }) if project == "parser" && vm == "rust"
        ));

        let cli = Cli::try_parse_from(["hegn", "project", "unassign", "parser"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Project {
                command: ProjectCommands::Unassign { project }
            }) if project == "parser"
        ));

        let cli = Cli::try_parse_from(["hegn", "project", "fork", "parser"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Project {
                command: ProjectCommands::Fork {
                    parent,
                    name: None,
                    mode: ForkModeArg::Clone,
                    target: None
                }
            }) if parent == "parser"
        ));

        let cli = Cli::try_parse_from([
            "hegn",
            "project",
            "fork",
            "parser",
            "--mode",
            "worktree",
            "--name",
            "parser-experiment",
            "--target",
            "/tmp/parser/.hegn/worktrees/parser-experiment",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Project {
                command:
                    ProjectCommands::Fork {
                        parent,
                        name,
                        mode,
                        target,
                    },
            }) => {
                assert_eq!(parent, "parser");
                assert_eq!(name.as_deref(), Some("parser-experiment"));
                assert_eq!(mode, ForkModeArg::Worktree);
                assert_eq!(
                    target,
                    Some(PathBuf::from(
                        "/tmp/parser/.hegn/worktrees/parser-experiment"
                    ))
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hidden_instance_command_stays_out_of_help() {
        use clap::CommandFactory;
        let command = Cli::command();
        let instance = command
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "instance")
            .unwrap();
        assert!(instance.is_hide_set(), "instance must stay hidden");
    }
}
