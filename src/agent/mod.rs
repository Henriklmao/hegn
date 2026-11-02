//! Agent-facing configuration: launch commands, harness profiles and
//! first-boot provisioning.
//!
//! - [`launch`] — named agent commands and terminal integrations
//! - [`profile`] — per-harness guest profiles and copy-only skill sync
//! - [`provision`] — first-boot harness preset installers

pub mod launch;
pub mod profile;
pub mod provision;
