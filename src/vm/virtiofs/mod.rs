//! Hardened `virtiofsd` process management.
//!
//! Every session gets a dedicated virtiofsd instance whose `--shared-dir` is
//! exactly one project directory. The daemon always runs with a sandbox
//! (`namespace` by default, `chroot` available) so a guest cannot traverse out
//! of the exported tree.

mod config;
mod process;

pub use config::VirtiofsConfig;
pub use process::VirtiofsProcess;
