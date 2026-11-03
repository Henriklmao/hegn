//! Project discovery, removal, git summaries and fuzzy filtering for the picker.
//!
//! - [`model`] — picker entries, git summaries and per-project metadata
//! - [`discovery`] — scanning the projects root with symlink guardrails
//! - [`git`] — status summaries, checkpoints and session branches
//! - [`filter`] — fuzzy matching for the project picker
//! - [`meta`] — `hegn.toml` reading and validated writing
//! - [`create`] — creating a seeded project directory
//! - [`import`] — importing existing directories as symlinks
//! - [`delete`] — removing projects and their instances

mod create;
mod delete;
mod discovery;
mod filter;
mod git;
mod import;
mod meta;
mod model;

pub use create::create_project;
pub use delete::delete_project;
pub use discovery::{discover, discover_protected};
pub use filter::{filter_projects, fuzzy_score};
pub use git::{create_checkpoint, create_session_branch, parse_git_status, read_git_summary};
pub use import::{expand_tilde, import_project};
pub use meta::{PROJECT_META_FILE, read_meta, try_read_meta, validate_project_name, write_meta};
pub use model::{GitSummary, Project, ProjectDeleteOutcome, ProjectMeta};
