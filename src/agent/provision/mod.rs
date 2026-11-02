//! First-boot provisioning for harness presets.
//!
//! The upstream cloud image stays untouched on the host. Instead, the NoCloud
//! seed carries a small, static install script that runs once inside the guest
//! as root. It installs the preset's packages with apt, upgrades Node.js when
//! the preset needs it, and then installs the harness itself.
//!
//! Only validated package names and static command strings reach the script.
//! User-supplied packages are data, never shell fragments.
//!
//! The script also emits small `hegn-progress:` JSON lines on the serial
//! console. The host watches the session log and renders those stages as a
//! progress bar while the first boot installs the preset.

mod presets;
mod script;
mod watcher;

pub use presets::{ProvisionPlan, Stage};
pub use watcher::{
    PROGRESS_PREFIX, ProvisionEvent, ProvisionProgress, ProvisionWatcher, parse_progress,
};
