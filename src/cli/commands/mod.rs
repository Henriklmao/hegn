//! One handler module per CLI command family. Each handler receives the parsed
//! command plus the loaded [`Config`] and returns a process exit code.
//!
//! [`Config`]: crate::config::Config

pub(crate) mod config;
pub(crate) mod doctor;
pub(crate) mod image;
pub(crate) mod instance;
pub(crate) mod logs;
pub(crate) mod network;
pub(crate) mod profile;
pub(crate) mod project;
pub(crate) mod skill;
pub(crate) mod snapshot;
