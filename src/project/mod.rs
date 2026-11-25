//! Project discovery, removal, git summaries and fuzzy filtering for the picker.
//!
//! - [`model`] — picker entries, git summaries and per-project metadata
//! - [`discovery`] — scanning the projects root with symlink guardrails
//! - [`git`] — status summaries, checkpoints and session branches
//! - [`filter`] — fuzzy matching for the project picker
//! - [`meta`] — `hegn.toml` reading and validated writing
//! - [`registry`] — the private project-to-VM registry
//! - [`create`] — creating a seeded project directory
//! - [`import`] — importing existing directories as symlinks
//! - [`delete`] — removing projects and their VMs
//! - [`workspace`] — clone and worktree forks of a project

mod assign;
mod create;
mod delete;
mod discovery;
mod filter;
mod git;
mod import;
mod meta;
mod model;
mod registry;
mod workspace;

pub use assign::{UnassignOutcome, assign_project, registered_entry, unassign_project};
pub use create::create_project;
pub use delete::{delete_project, delete_registered_project};
pub use discovery::{discover, discover_protected};
pub use filter::{filter_projects, fuzzy_score};
pub use git::{create_checkpoint, create_session_branch, parse_git_status, read_git_summary};
pub use import::{expand_tilde, import_project};
pub use meta::{
    PROJECT_META_FILE, deprecated_legacy_keys, read_meta, try_read_meta, validate_project_name,
    write_meta,
};
pub use model::{GitSummary, Project, ProjectDeleteOutcome, ProjectMeta};
pub use registry::{
    ProjectEntry, ProjectKind, ProjectRegistry, REGISTRY_FILE, validate_recorded_export,
};
pub use workspace::{ForkMode, WorkspaceService, default_worktree_path, is_real_worktree};
