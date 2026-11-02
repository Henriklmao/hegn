//! Discovery of the external tool binaries the VM backend depends on.

use std::path::{Path, PathBuf};

/// Locations where distributions install `virtiofsd` outside `PATH`.
///
/// Arch ships `/usr/lib/virtiofsd`, Fedora/RHEL use `/usr/libexec/virtiofsd`,
/// Debian/Ubuntu keep it in `/usr/lib/qemu/`, and openSUSE uses
/// `/usr/libexec/qemu/`. `hegn doctor` and the instance preflight fall back to
/// these after `PATH` so a plain package install works without editing
/// `virtiofsd_bin`.
pub const VIRTIOFSD_FALLBACKS: &[&str] = &[
    "/usr/lib/virtiofsd",
    "/usr/libexec/virtiofsd",
    "/usr/lib/qemu/virtiofsd",
    "/usr/libexec/qemu/virtiofsd",
];

/// The AUR package `cloud-hypervisor-bin` installs only the static binary.
pub const CLOUD_HYPERVISOR_STATIC: &str = "cloud-hypervisor-static";

/// Common install locations for Cloud Hypervisor.
pub const CLOUD_HYPERVISOR_FALLBACKS: &[&str] = &[
    "/usr/bin/cloud-hypervisor",
    "/usr/bin/cloud-hypervisor-static",
    "/usr/lib/cloud-hypervisor/cloud-hypervisor",
];

/// Map `cloud-hypervisor` to its static AUR variant and back.
pub(super) fn alternate_cloud_hypervisor_name(binary: &Path) -> Option<&'static str> {
    if binary == Path::new("cloud-hypervisor") {
        Some(CLOUD_HYPERVISOR_STATIC)
    } else if binary == Path::new(CLOUD_HYPERVISOR_STATIC) {
        Some("cloud-hypervisor")
    } else {
        None
    }
}

/// Resolve a configured tool to an executable file.
///
/// A configured path (anything with a directory component) is used verbatim so
/// an explicit `*_bin` always wins. A bare command name is looked up on `PATH`
/// first and then in `fallbacks`; distribution helper daemons often live in a
/// private lib directory instead of being symlinked into `PATH`.
pub(super) fn resolve_binary(binary: &Path, fallbacks: &[&str]) -> Option<PathBuf> {
    if binary.components().count() > 1 {
        return binary.is_file().then(|| binary.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    fallbacks
        .iter()
        .map(Path::new)
        .find(|candidate| candidate.is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn cloud_hypervisor_static_is_an_accepted_alias() {
        assert_eq!(
            alternate_cloud_hypervisor_name(Path::new("cloud-hypervisor")),
            Some("cloud-hypervisor-static")
        );
        assert_eq!(
            alternate_cloud_hypervisor_name(Path::new("cloud-hypervisor-static")),
            Some("cloud-hypervisor")
        );
        assert_eq!(
            alternate_cloud_hypervisor_name(Path::new("/opt/custom/ch")),
            None
        );
    }

    #[test]
    fn resolve_binary_prefers_an_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let explicit = dir.path().join("virtiofsd");
        fs::write(&explicit, b"").unwrap();
        assert_eq!(resolve_binary(&explicit, &[]), Some(explicit.clone()));
        assert_eq!(
            resolve_binary(&dir.path().join("missing-virtiofsd"), &[]),
            None
        );
    }

    #[test]
    fn resolve_binary_falls_back_to_install_directories() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("virtiofsd");
        fs::write(&fallback, b"").unwrap();
        let fallback_str = fallback.to_str().unwrap();
        assert_eq!(
            resolve_binary(Path::new("hegn-test-missing-virtiofsd"), &[fallback_str]),
            Some(fallback)
        );
    }
}
