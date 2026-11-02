//! Per-harness guest profiles and the copy-only skill sync.
//!
//! A profile is a TOML file in `profiles_dir` (`~/.config/hegn/profiles/` by
//! default). `hegn config init` writes one template per harness so the user can
//! add packages, configuration files, API keys and skill directories without
//! editing the global config.
//!
//! The host is always the source of truth for configuration files. Skills are
//! **copied** into a private per-instance bundle and exported through a second
//! `virtiofsd`; the host's real skill directories are never mounted into the
//! guest. Changes made in the guest can be copied back explicitly with
//! `hegn skills sync-back`.
//!
//! ```text
//! <instances_dir>/<instance>/profile/
//!   local.json          host-side mapping (never exported)
//!   bundle/             exported to the guest at /run/hegn/profile
//!     manifest          tab-separated copy plan
//!     env.sh            exported environment
//!     files/<n>/…       copied configuration files
//!     skills/<n>/…      copied skills (guest edits land here)
//! ```

mod fs;
mod model;
mod store;
mod sync;
mod template;
mod validate;

pub use model::{
    Profile, ProfileFile, ProfilePlan, ProfileShare, ProfileSkill, SyncBackReport, SyncReport,
};
pub use store::ProfileStore;
pub use sync::{prepare_for_start, sync_skills, sync_skills_back};
pub use template::apply_script;
pub use validate::expand_home;

/// virtio-fs tag of the profile share. Reserved in `config.shared_tag`.
pub const PROFILE_SHARE_TAG: &str = "hegn-profile";
/// Mount point of the profile share inside the guest.
pub const PROFILE_GUEST_MOUNT: &str = "/run/hegn/profile";
/// Current profile schema version.
pub const PROFILE_VERSION: u32 = 1;

const BUNDLE_DIR: &str = "bundle";
const LOCAL_STATE: &str = "local.json";
const FILES_DIR: &str = "files";
const SKILLS_DIR: &str = "skills";
const MANIFEST: &str = "manifest";
const ENV_FILE: &str = "env.sh";
const MAX_ENV_VALUE: usize = 8192;
const MAX_TARGET_LEN: usize = 512;
