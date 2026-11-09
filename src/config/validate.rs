//! Schema validation for [`Config`], plus the small validators shared with the
//! CLI and the interactive settings form.

use anyhow::{Result, bail};

use super::Config;
use super::types::{LocalEndpoint, NetworkMode};

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.vcpus == 0 || self.vcpus > 32 {
            bail!("vcpus must be between 1 and 32 (got {})", self.vcpus);
        }
        if self.memory_mib < 128 {
            bail!("memory_mib must be at least 128 (got {})", self.memory_mib);
        }
        if !(16..=1024).contains(&self.max_rootfs_gib) {
            bail!(
                "max_rootfs_gib must be between 16 and 1024 (got {})",
                self.max_rootfs_gib
            );
        }
        if self.disk_usage_warn_percent == 0 || self.disk_usage_warn_percent >= 100 {
            bail!(
                "disk_usage_warn_percent must be between 1 and 99 (got {})",
                self.disk_usage_warn_percent
            );
        }
        // A snapshot pauses the guest while memory is written, so an interval
        // below 30 seconds would spend most of the VM's life paused.
        if !(30..=86_400).contains(&self.snapshot_interval_secs) {
            bail!(
                "snapshot_interval_secs must be between 30 and 86400 (got {})",
                self.snapshot_interval_secs
            );
        }
        if self.snapshot_keep == 0 || self.snapshot_keep > 32 {
            bail!(
                "snapshot_keep must be between 1 and 32 (got {})",
                self.snapshot_keep
            );
        }
        if !crate::vm::cloud_init::safe_token(&self.shared_tag) {
            bail!("shared_tag may only contain ASCII letters, digits, '-', '_' and '.'");
        }
        if self.shared_tag == crate::agent::profile::PROFILE_SHARE_TAG {
            bail!(
                "shared_tag '{}' is reserved for the harness profile share",
                crate::agent::profile::PROFILE_SHARE_TAG
            );
        }
        // virtio-fs tags are limited to 36 bytes including the terminator.
        if self.shared_tag.len() > 36 {
            bail!(
                "shared_tag must be at most 36 bytes (got {})",
                self.shared_tag.len()
            );
        }
        if !crate::vm::cloud_init::safe_mount(&self.guest_mount) {
            bail!(
                "guest_mount must be an absolute path with simple components \
                 (letters, digits, '/', '-', '_' and '.')"
            );
        }
        if !self.root_device.starts_with("/dev/")
            || !self.root_device.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-')
            })
        {
            bail!("root_device must be a guest device path such as /dev/vda1");
        }
        // Cloud Hypervisor's CLI packs paths into comma-separated option
        // values. A comma in one of these paths would silently change the
        // device configuration, so refuse it up front.
        for (name, path) in [
            ("state_dir", &self.state_dir),
            ("snapshots_dir", &self.snapshots_dir),
            ("instances_dir", &self.instances_dir),
            ("kernel_image", &self.kernel_image),
            ("rootfs_image", &self.rootfs_image),
        ] {
            if path.to_string_lossy().contains(',') {
                bail!("{name} must not contain a comma");
            }
        }
        for (name, sha) in [
            ("kernel_sha256", &self.kernel_sha256),
            ("rootfs_sha256", &self.rootfs_sha256),
        ] {
            if !sha.trim().is_empty() && !crate::vm::images::is_sha256(sha.trim()) {
                bail!("{name} must be a 64 character SHA-256 hex digest");
            }
        }
        for (name, url) in [
            ("kernel_url", &self.kernel_url),
            ("rootfs_url", &self.rootfs_url),
        ] {
            if !url.trim().is_empty() && !url.starts_with("https://") {
                bail!("{name} must use https://");
            }
        }
        for package in &self.extra_packages {
            if !safe_package_name(package) {
                bail!("invalid package name in extra_packages: {package:?}");
            }
        }
        self.launch.validate()?;
        validate_local_endpoints(&self.local_endpoints)?;
        if !self.local_endpoints.is_empty() && self.network != NetworkMode::Guarded {
            bail!("local_endpoints need guarded networking; set network = \"guarded\"");
        }
        Ok(())
    }

    /// Deduplicated, sorted ports of the configured local endpoints.
    pub fn endpoint_ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self
            .local_endpoints
            .iter()
            .map(|endpoint| endpoint.port)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// Rootfs growth ceiling in bytes.
    pub fn max_rootfs_bytes(&self) -> u64 {
        u64::from(self.max_rootfs_gib) * 1024 * 1024 * 1024
    }
}

/// Why a [`LocalEndpoint`] is invalid.
pub fn validate_local_endpoints(endpoints: &[LocalEndpoint]) -> Result<()> {
    let mut seen = Vec::new();
    for endpoint in endpoints {
        if !crate::vm::cloud_init::safe_token(&endpoint.name) || endpoint.name.len() > 32 {
            bail!(
                "invalid local endpoint name {:?}: use up to 32 ASCII letters, digits, '-', '_' or '.'",
                endpoint.name
            );
        }
        // Ports below 1024 are reserved for privileged host services (SSH,
        // DNS, …). Exposing those to an instance is never an LLM passthrough.
        if endpoint.port < 1024 {
            bail!(
                "local endpoint '{}' uses port {}; ports below 1024 are not exposed",
                endpoint.name,
                endpoint.port
            );
        }
        let key = endpoint.name.to_ascii_lowercase();
        if seen.contains(&key) {
            bail!("duplicate local endpoint name '{}'", endpoint.name);
        }
        seen.push(key);
    }
    Ok(())
}

/// Validate a Debian package name before it reaches a root shell or YAML file.
pub fn safe_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '_' | '-'))
        && !name.starts_with('-')
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rejects_invalid_image_pins() {
        let mut config = Config::default();
        config.kernel_sha256 = "not-a-digest".into();
        assert!(config.validate().is_err());
        config.kernel_sha256 = crate::vm::images::KERNEL_SHA256.into();
        config.rootfs_url = "http://example.com/rootfs".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_values() {
        let mut config = Config::default();
        config.vcpus = 0;
        assert!(config.validate().is_err());
        config.vcpus = 2;
        config.memory_mib = 12;
        assert!(config.validate().is_err());
        config.memory_mib = 1024;
        config.guest_mount = "workspace".into();
        assert!(config.validate().is_err());
        config.guest_mount = "/workspace".into();
        config.root_device = "vda1".into();
        assert!(config.validate().is_err());
        config.root_device = "/dev/vda1".into();
        config.shared_tag = "x".repeat(37);
        assert!(config.validate().is_err());
        config.shared_tag = "work space".into();
        assert!(config.validate().is_err());
        config.shared_tag = "workspace".into();
        config.guest_mount = "/work space".into();
        assert!(config.validate().is_err());
        config.guest_mount = "/workspace".into();
        config.kernel_image = PathBuf::from("/images/vmlinux,broken");
        assert!(config.validate().is_err());
        config.kernel_image = PathBuf::from("/images/vmlinux");
        config.shared_tag = crate::agent::profile::PROFILE_SHARE_TAG.into();
        assert!(
            config.validate().is_err(),
            "the profile share tag is reserved"
        );
        config.shared_tag = "workspace".into();
        config.snapshot_interval_secs = 29;
        assert!(config.validate().is_err());
        config.snapshot_interval_secs = 300;
        config.snapshot_keep = 0;
        assert!(config.validate().is_err());
        config.snapshot_keep = 2;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn local_endpoints_are_validated() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());

        config.local_endpoints = vec![LocalEndpoint {
            name: "ollama".into(),
            port: 11434,
        }];
        assert!(config.validate().is_ok());
        assert_eq!(config.endpoint_ports(), vec![11434]);

        config.local_endpoints = vec![LocalEndpoint {
            name: "bad name".into(),
            port: 11434,
        }];
        assert!(config.validate().is_err(), "unsafe names are rejected");

        config.local_endpoints = vec![LocalEndpoint {
            name: "ssh".into(),
            port: 22,
        }];
        assert!(config.validate().is_err(), "privileged ports are rejected");

        config.local_endpoints = vec![
            LocalEndpoint {
                name: "ollama".into(),
                port: 11434,
            },
            LocalEndpoint {
                name: "OLLAMA".into(),
                port: 1234,
            },
        ];
        assert!(config.validate().is_err(), "names are unique");

        config.local_endpoints = vec![LocalEndpoint {
            name: "ollama".into(),
            port: 11434,
        }];
        config.network = NetworkMode::Off;
        assert!(config.validate().is_err(), "endpoints need a network");
    }

    fn endpoint(name: &str, port: u16) -> LocalEndpoint {
        LocalEndpoint {
            name: name.into(),
            port,
        }
    }

    #[test]
    fn endpoint_ports_must_be_unprivileged() {
        assert!(validate_local_endpoints(&[endpoint("lowest", 1024)]).is_ok());
        assert!(validate_local_endpoints(&[endpoint("highest", 65535)]).is_ok());
        assert!(validate_local_endpoints(&[endpoint("zero", 0)]).is_err());
        assert!(validate_local_endpoints(&[endpoint("reserved", 1023)]).is_err());
    }

    #[test]
    fn endpoint_names_are_bounded_and_case_insensitively_unique() {
        let longest = "a".repeat(32);
        assert!(validate_local_endpoints(&[endpoint(&longest, 11434)]).is_ok());
        assert!(validate_local_endpoints(&[endpoint(&"a".repeat(33), 11434)]).is_err());
        assert!(validate_local_endpoints(&[endpoint("", 11434)]).is_err());
        assert!(validate_local_endpoints(&[endpoint("bad name", 11434)]).is_err());
        assert!(
            validate_local_endpoints(&[endpoint("ollama", 11434), endpoint("OLLAMA", 8080)])
                .is_err()
        );
    }

    #[test]
    fn endpoint_ports_are_sorted_and_deduplicated() {
        let mut config = Config::default();
        config.local_endpoints = vec![
            endpoint("beta", 11434),
            endpoint("alpha", 8080),
            endpoint("gamma", 11434),
        ];
        assert!(config.validate().is_ok(), "distinct names may share a port");
        assert_eq!(config.endpoint_ports(), vec![8080, 11434]);
    }

    #[test]
    fn package_names_reject_shell_and_empty_values() {
        let longest = "x".repeat(64);
        for name in [
            "ripgrep",
            "fd-find",
            "g++",
            "python3.12",
            "libfoo_bar",
            longest.as_str(),
        ] {
            assert!(safe_package_name(name), "{name:?} must be valid");
        }
        let too_long = "x".repeat(65);
        for name in ["", "-rm", "a b", "pkg\nname", "ümlaut", too_long.as_str()] {
            assert!(!safe_package_name(name), "{name:?} must be rejected");
        }
    }

    #[test]
    fn resource_and_guest_path_boundaries_are_enforced() {
        let mut config = Config::default();
        config.vcpus = 32;
        config.memory_mib = 128;
        config.shared_tag = "a".repeat(36);
        config.guest_mount = "/w".into();
        config.root_device = "/dev/vda".into();
        assert!(config.validate().is_ok());

        config.vcpus = 33;
        assert!(config.validate().is_err(), "vcpus above 32 are rejected");
        config.vcpus = 2;
        config.memory_mib = 127;
        assert!(
            config.validate().is_err(),
            "memory below 128 MiB is rejected"
        );
        config.memory_mib = 2048;
        config.shared_tag = "a".repeat(37);
        assert!(
            config.validate().is_err(),
            "tags above 36 bytes are rejected"
        );
        config.shared_tag = "workspace".into();
        config.guest_mount = "/".into();
        assert!(
            config.validate().is_err(),
            "the filesystem root is rejected"
        );
    }

    #[test]
    fn resource_caps_are_bounded() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.max_rootfs_bytes(), 64 * 1024 * 1024 * 1024);

        config.max_rootfs_gib = 15;
        assert!(config.validate().is_err(), "below the 16 GiB floor");
        config.max_rootfs_gib = 1025;
        assert!(config.validate().is_err(), "unreasonably large ceiling");

        config.max_rootfs_gib = 64;
        config.disk_usage_warn_percent = 0;
        assert!(config.validate().is_err());
        config.disk_usage_warn_percent = 100;
        assert!(config.validate().is_err());
        config.disk_usage_warn_percent = 80;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn comma_paths_and_insecure_image_sources_are_rejected() {
        let base = Config::default();

        let mut config = base.clone();
        config.state_dir = base.state_dir.join("a,b");
        assert!(config.validate().is_err(), "state_dir commas are rejected");

        let mut config = base.clone();
        config.snapshots_dir = base.snapshots_dir.join("a,b");
        assert!(
            config.validate().is_err(),
            "snapshots_dir commas are rejected"
        );

        let mut config = base.clone();
        config.instances_dir = base.instances_dir.join("a,b");
        assert!(
            config.validate().is_err(),
            "instances_dir commas are rejected"
        );

        let mut config = base.clone();
        config.rootfs_image = base.rootfs_image.join("a,b");
        assert!(
            config.validate().is_err(),
            "rootfs_image commas are rejected"
        );

        let mut config = base.clone();
        config.rootfs_sha256 = "not-a-digest".into();
        assert!(config.validate().is_err(), "invalid rootfs digests fail");

        let mut config = base.clone();
        config.rootfs_url = "http://example.test/rootfs".into();
        assert!(config.validate().is_err(), "rootfs URLs must use https");

        let mut config = base.clone();
        config.root_device = "/dev/vda,readonly".into();
        assert!(
            config.validate().is_err(),
            "root_device commas are rejected"
        );

        // Whitespace and shell/kernel metacharacters would append arguments to
        // the guest kernel command line.
        for injected in [
            "/dev/vda1 init=/bin/sh",
            "/dev/vda1\tro",
            "/dev/vda1;reboot",
            "/dev/vda1=",
            "/dev/vda1\"",
            "vda1",
        ] {
            let mut config = base.clone();
            config.root_device = injected.into();
            assert!(
                config.validate().is_err(),
                "root_device {injected:?} must be rejected"
            );
        }
        let mut config = base;
        config.root_device = "/dev/disk/by-label/hegn_root".into();
        assert!(
            config.validate().is_ok(),
            "dots, dashes, underscores and slashes stay valid"
        );
    }
}
