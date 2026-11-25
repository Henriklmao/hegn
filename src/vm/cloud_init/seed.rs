//! Writing the `CIDATA` FAT seed image.
//!
//! [`write_seed`] validates every value that ends up in the cloud-init shell
//! commands, formats a 4 MiB FAT volume with mode `0600` and `O_NOFOLLOW`, and
//! writes `user-data`, `meta-data` and (when networking is enabled)
//! `network-config` into it.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};

use crate::agent::profile::ProfileShare;
use crate::agent::provision::ProvisionPlan;
use crate::config::LocalEndpoint;
use crate::network::Lease;

use super::network::endpoint_env;
use super::user_data::{meta_data, user_data};

/// 4 MiB is enough for the seed and keeps it cheap.
const SEED_BYTES: u64 = 4 * 1024 * 1024;

/// Increment when the seed's user-data changes so existing VMs re-run
/// cloud-init on their next boot. Version 12 introduced multiple mounts and
/// the `/workspace` symlink.
pub const SEED_VERSION: u32 = 12;

/// One exported project mounted by cloud-init.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    /// virtio-fs device tag (`hegn-prj-<index>`).
    pub tag: String,
    /// Absolute guest path below `guest_projects_base`.
    pub guest_path: String,
    pub read_only: bool,
}

/// Reject values that could break out of the cloud-init shell command.
pub(crate) fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub(crate) fn safe_mount(value: &str) -> bool {
    value.starts_with('/')
        && value.len() > 1
        && !value.ends_with('/')
        && !value.contains("//")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        && value
            .split('/')
            .skip(1)
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Write (or replace) the NoCloud seed at `path`.
///
/// The image is created with mode `0600` and `O_NOFOLLOW`; a planted symlink
/// cannot redirect the write. Every mount is validated before the file is
/// touched.
#[allow(clippy::too_many_arguments)]
pub fn write_seed(
    path: &Path,
    mounts: &[MountSpec],
    primary_guest: &str,
    instance_id: &str,
    provision: &ProvisionPlan,
    profile: Option<&ProfileShare>,
    network: Option<&Lease>,
    endpoints: &[LocalEndpoint],
    credential_hardening: bool,
) -> Result<()> {
    if mounts.is_empty() {
        anyhow::bail!("cloud-init seed needs at least one project mount");
    }
    for mount in mounts {
        if !safe_token(&mount.tag) {
            anyhow::bail!(
                "unsafe virtio-fs tag for the cloud-init seed: {tag:?}",
                tag = mount.tag
            );
        }
        if !safe_mount(&mount.guest_path) {
            anyhow::bail!(
                "unsafe guest mount path for the cloud-init seed: {path:?}",
                path = mount.guest_path
            );
        }
    }
    if !safe_mount(primary_guest) {
        anyhow::bail!("unsafe primary guest path for the cloud-init seed: {primary_guest:?}");
    }
    if !mounts.iter().any(|mount| mount.guest_path == primary_guest) {
        anyhow::bail!("primary guest path {primary_guest:?} is not one of the mounted projects");
    }
    if !safe_token(instance_id) {
        anyhow::bail!("unsafe cloud-init instance id: {instance_id:?}");
    }
    let endpoint_env = network
        .filter(|_| !endpoints.is_empty())
        .map(|lease| endpoint_env(lease, endpoints));
    let user_data = user_data(
        mounts,
        primary_guest,
        provision,
        profile,
        endpoint_env.as_deref(),
        credential_hardening,
    );
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to create cloud-init seed {}", path.display()))?;
    // An existing seed may predate the private-file invariant. Tighten it on
    // the open descriptor, so a concurrently replaced path cannot redirect it.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure cloud-init seed {}", path.display()))?;
    file.set_len(SEED_BYTES)
        .with_context(|| format!("failed to size cloud-init seed {}", path.display()))?;
    file.seek(SeekFrom::Start(0))?;

    fatfs::format_volume(
        &mut file,
        FormatVolumeOptions::new().volume_label(*b"CIDATA     "),
    )
    .with_context(|| format!("failed to format cloud-init seed {}", path.display()))?;

    {
        let fs = FileSystem::new(&mut file, FsOptions::new())
            .with_context(|| format!("failed to open cloud-init seed {}", path.display()))?;
        let root = fs.root_dir();
        write_file(&root, "user-data", &user_data)?;
        write_file(&root, "meta-data", &meta_data(instance_id))?;
        if let Some(network) = network {
            write_file(&root, "network-config", &network.guest_network_config())?;
        }
    }
    file.sync_all()?;
    Ok(())
}

fn write_file<IO: std::io::Read + std::io::Write + std::io::Seek>(
    root: &fatfs::Dir<'_, IO>,
    name: &str,
    contents: &str,
) -> Result<()> {
    let mut file = root
        .create_file(name)
        .with_context(|| format!("failed to create {name} in the cloud-init seed"))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {name} in the cloud-init seed"))?;
    file.flush()?;
    Ok(())
}

/// Stable id for this seed. It changes when the seed version, membership,
/// preset or network configuration changes, which makes cloud-init re-apply
/// the new user-data/network-config even when the rootfs is reused.
#[allow(clippy::too_many_arguments)]
pub fn instance_id(
    vm: &str,
    mounts: &[MountSpec],
    primary_guest: &str,
    provision: &ProvisionPlan,
    profile: Option<&ProfileShare>,
    network: Option<&Lease>,
    endpoints: &[LocalEndpoint],
    credential_hardening: bool,
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(SEED_VERSION.to_le_bytes());
    hasher.update([credential_hardening as u8]);
    hasher.update(vm.as_bytes());
    hasher.update(primary_guest.as_bytes());
    for mount in mounts {
        hasher.update(mount.tag.as_bytes());
        hasher.update(mount.guest_path.as_bytes());
        hasher.update([mount.read_only as u8]);
    }
    hasher.update(provision.harness.label().as_bytes());
    hasher.update(provision.script().as_bytes());
    if let Some(profile) = profile {
        hasher.update(profile.manifest_id.as_bytes());
    }
    if let Some(lease) = network {
        hasher.update(lease.guest_network_config().as_bytes());
        hasher.update(endpoint_env(lease, endpoints).as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    format!("hegn-{vm}-v{SEED_VERSION}-{}", &digest[..12])
}

/// The exact `--disk` value for the seed.
pub fn disk_arg(path: &Path) -> String {
    format!("path={},image_type=raw", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;
    use crate::network::{Lease, guest_ip, host_ip, mac, tap_name};
    use std::io::Read;
    use std::path::PathBuf;

    fn lease() -> Lease {
        Lease {
            index: 0,
            instance: "demo".into(),
            tap: tap_name(0),
            mac: mac(0),
            host_ip: host_ip(0),
            guest_ip: guest_ip(0),
        }
    }

    fn mounts() -> Vec<MountSpec> {
        vec![
            MountSpec {
                tag: "hegn-prj-0".into(),
                guest_path: "/projects/parser".into(),
                read_only: false,
            },
            MountSpec {
                tag: "hegn-prj-1".into(),
                guest_path: "/projects/cli".into(),
                read_only: true,
            },
        ]
    }

    fn read_seed(path: &Path) -> (String, String, Option<String>) {
        let file = OpenOptions::new().read(true).open(path).unwrap();
        let fs = FileSystem::new(file, FsOptions::new()).unwrap();
        let root = fs.root_dir();
        let mut user_data = String::new();
        root.open_file("user-data")
            .unwrap()
            .read_to_string(&mut user_data)
            .unwrap();
        let mut meta_data = String::new();
        root.open_file("meta-data")
            .unwrap()
            .read_to_string(&mut meta_data)
            .unwrap();
        let network = root.open_file("network-config").ok().map(|mut file| {
            let mut raw = String::new();
            file.read_to_string(&mut raw).unwrap();
            raw
        });
        (user_data, meta_data, network)
    }

    #[test]
    fn writes_a_readable_cidata_volume_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        write_seed(
            &path,
            &mounts(),
            "/projects/parser",
            "hegn-demo",
            &plan,
            None,
            Some(&lease()),
            &[],
            false,
        )
        .unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let (user_data, meta_data, network) = read_seed(&path);
        assert!(user_data.contains("autologin root"), "{user_data}");
        assert!(
            user_data.contains("mount -t virtiofs 'hegn-prj-0' '/projects/parser'"),
            "{user_data}"
        );
        assert!(
            user_data.contains("mount -t virtiofs -o ro 'hegn-prj-1' '/projects/cli'"),
            "{user_data}"
        );
        assert!(
            user_data.contains("ln -sfn '/projects/parser' '/workspace'"),
            "{user_data}"
        );
        assert!(
            user_data.contains("directory = /projects/parser"),
            "{user_data}"
        );
        assert!(
            user_data.contains("directory = /projects/cli"),
            "{user_data}"
        );
        assert!(user_data.contains("directory = /workspace"), "{user_data}");
        assert!(user_data.contains("hegn-health v3"), "{user_data}");
        assert!(
            user_data.contains("mounts=$mounts_ok/$mounts_total"),
            "{user_data}"
        );
        assert!(user_data.contains("/usr/local/sbin/hegn-provision"));
        assert!(user_data.contains("hegn-provision.service"));
        assert!(meta_data.contains("instance-id: hegn-demo"), "{meta_data}");
        let network = network.expect("network-config must be present");
        assert!(network.contains("10.213.0.2/30"), "{network}");
        assert!(network.contains("via: 10.213.0.1"), "{network}");
    }

    #[test]
    fn login_shell_starts_in_the_primary_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        write_seed(
            &path,
            &mounts(),
            "/projects/parser",
            "hegn-demo",
            &plan,
            None,
            None,
            &[],
            false,
        )
        .unwrap();
        let (user_data, _, _) = read_seed(&path);
        assert!(
            user_data.contains("  - path: /etc/profile.d/hegn-workspace.sh"),
            "{user_data}"
        );
        assert!(user_data.contains("cd '/workspace'"), "{user_data}");
    }

    #[test]
    fn health_counts_every_expected_mount() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        write_seed(
            &path,
            &mounts(),
            "/projects/parser",
            "hegn-demo",
            &plan,
            None,
            None,
            &[],
            false,
        )
        .unwrap();
        let (user_data, _, _) = read_seed(&path);
        assert!(user_data.contains("mounts_total=2"), "{user_data}");
        assert!(
            user_data.contains("for target in '/projects/parser' '/projects/cli'"),
            "{user_data}"
        );
        assert!(
            user_data.contains("mounts=$mounts_ok/$mounts_total"),
            "{user_data}"
        );
        assert!(!user_data.contains("hegn-health v2"), "{user_data}");
    }

    #[test]
    fn endpoints_are_exported_for_the_guest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        let endpoints = vec![
            LocalEndpoint {
                name: "ollama".into(),
                port: 11434,
            },
            LocalEndpoint {
                name: "llm-local".into(),
                port: 1234,
            },
        ];
        write_seed(
            &path,
            &mounts(),
            "/projects/parser",
            "hegn-demo",
            &plan,
            None,
            Some(&lease()),
            &endpoints,
            false,
        )
        .unwrap();
        let (user_data, _, _) = read_seed(&path);
        assert!(
            user_data.contains("/etc/profile.d/hegn-endpoints.sh"),
            "{user_data}"
        );
        assert!(
            user_data.contains("export HEGN_ENDPOINT_OLLAMA=\"http://10.213.0.1:11434\""),
            "{user_data}"
        );
        assert!(
            user_data.contains("export OLLAMA_HOST=\"http://10.213.0.1:11434\""),
            "{user_data}"
        );
        assert!(
            user_data.contains("export HEGN_ENDPOINT_LLM_LOCAL=\"http://10.213.0.1:1234\""),
            "{user_data}"
        );
    }

    #[test]
    fn profile_share_mounts_and_applies_on_every_boot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let profile = ProfileShare {
            bundle_dir: PathBuf::from("/tmp/profile-bundle"),
            manifest_id: "a".repeat(64),
        };
        write_seed(
            &path,
            &mounts(),
            "/projects/parser",
            "hegn-demo",
            &plan,
            Some(&profile),
            None,
            &[],
            false,
        )
        .unwrap();
        let (user_data, _, _) = read_seed(&path);
        assert!(
            user_data.contains("'hegn-profile', '/run/hegn/profile'"),
            "{user_data}"
        );
        assert!(
            user_data.contains("/usr/local/sbin/hegn-apply-profile"),
            "{user_data}"
        );
        assert!(
            user_data.contains("hegn: applying harness profile"),
            "{user_data}"
        );
        assert!(
            !user_data.contains("ConditionPathExists=!/var/lib/hegn/provisioned"),
            "the provisioner must run every boot so profiles are re-applied: {user_data}"
        );
    }

    #[test]
    fn instance_id_changes_with_membership_preset_and_network() {
        let shell = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        let opencode = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let one = mounts();
        let a = instance_id(
            "demo",
            &one,
            "/projects/parser",
            &shell,
            None,
            None,
            &[],
            false,
        );
        assert_eq!(
            a,
            instance_id(
                "demo",
                &one,
                "/projects/parser",
                &shell,
                None,
                None,
                &[],
                false
            )
        );
        assert_ne!(
            a,
            instance_id(
                "demo",
                &one,
                "/projects/parser",
                &opencode,
                None,
                None,
                &[],
                false
            )
        );
        assert_ne!(
            a,
            instance_id(
                "demo",
                &one,
                "/projects/parser",
                &shell,
                None,
                Some(&lease()),
                &[],
                false
            )
        );
        assert_ne!(
            a,
            instance_id(
                "demo",
                &one,
                "/projects/parser",
                &shell,
                None,
                None,
                &[],
                true
            ),
            "toggling credential hardening must re-run cloud-init"
        );
        let mut two = one.clone();
        two.pop();
        assert_ne!(
            a,
            instance_id(
                "demo",
                &two,
                "/projects/parser",
                &shell,
                None,
                None,
                &[],
                false
            ),
            "membership changes must re-run cloud-init"
        );
        assert!(a.starts_with("hegn-demo-v"));
    }

    #[test]
    fn off_network_omits_network_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        write_seed(
            &path,
            &mounts(),
            "/projects/parser",
            "hegn-demo",
            &plan,
            None,
            None,
            &[],
            false,
        )
        .unwrap();
        let (user_data, _, network) = read_seed(&path);
        assert!(network.is_none());
        assert!(!user_data.contains("network-config"));
    }

    #[test]
    fn refuses_to_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"do not touch").unwrap();
        let link = dir.path().join("cloud-init.img");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();

        assert!(
            write_seed(
                &link,
                &mounts(),
                "/projects/parser",
                "hegn-demo",
                &plan,
                None,
                None,
                &[],
                false
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"do not touch");
    }

    #[test]
    fn rejects_values_that_could_break_the_cloud_init_shell() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        for bad in ["bad tag", "workspace;reboot"] {
            let mounts = vec![MountSpec {
                tag: bad.into(),
                guest_path: "/projects/demo".into(),
                read_only: false,
            }];
            assert!(
                write_seed(
                    &path,
                    &mounts,
                    "/projects/demo",
                    "hegn-demo",
                    &plan,
                    None,
                    None,
                    &[],
                    false
                )
                .is_err(),
                "{bad:?}"
            );
        }
        for bad in ["/work space", "/workspace;reboot", "/workspace/../etc"] {
            let mounts = vec![MountSpec {
                tag: "hegn-prj-0".into(),
                guest_path: bad.into(),
                read_only: false,
            }];
            assert!(
                write_seed(
                    &path,
                    &mounts,
                    bad,
                    "hegn-demo",
                    &plan,
                    None,
                    None,
                    &[],
                    false
                )
                .is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rejects_an_empty_or_unmounted_primary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        assert!(
            write_seed(
                &path,
                &[],
                "/projects/demo",
                "hegn-demo",
                &plan,
                None,
                None,
                &[],
                false
            )
            .is_err()
        );
        assert!(
            write_seed(
                &path,
                &mounts(),
                "/projects/elsewhere",
                "hegn-demo",
                &plan,
                None,
                None,
                &[],
                false
            )
            .is_err()
        );
    }

    #[test]
    fn replaces_an_existing_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        write_seed(
            &path,
            &mounts(),
            "/projects/parser",
            "hegn-demo",
            &plan,
            None,
            None,
            &[],
            false,
        )
        .unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() >= SEED_BYTES);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "an existing seed must be tightened to 0600"
        );
    }

    #[test]
    fn safe_tokens_reject_shell_and_yaml_metacharacters() {
        for bad in [
            "",
            "line\nbreak",
            "carriage\rreturn",
            "tab\there",
            "single'quote",
            "double\"quote",
            "comma,separated",
            "space separated",
            "semi;colon",
            "pipe|operator",
            "amp&background",
            "redirect>file",
            "redirect<file",
            "dollar$(id)",
            "back`tick`",
            "back\\slash",
            "paren(thesis)",
            "colon:name",
        ] {
            assert!(!safe_token(bad), "{bad:?} must not be a safe token");
        }
        for good in ["workspace", "hegn-demo_1.2", "A1", "hegn-profile"] {
            assert!(safe_token(good), "{good:?} must be a safe token");
        }
    }

    #[test]
    fn safe_mounts_reject_metacharacters_and_relative_paths() {
        for good in ["/workspace", "/home/user/.config", "/a", "/opt/x-y_z"] {
            assert!(safe_mount(good), "{good:?} must be a safe mount");
        }
        for bad in [
            "",
            "/",
            "/..",
            "/.",
            "/workspace/",
            "/workspace//etc",
            "/workspace/..",
            "/workspace/../etc",
            "/workspace/.",
            "relative/path",
            "~/.config",
            "/work space",
            "/work\nspace",
            "/work'space",
            "/work\"space",
            "/work,space",
            "/work;space",
            "/work$(id)",
            "/work`id`",
            "/work|space",
            "/work&space",
        ] {
            assert!(!safe_mount(bad), "{bad:?} must not be a safe mount");
        }
    }

    #[test]
    fn write_seed_rejects_unsafe_instance_ids_before_creating_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        for bad in [
            "hegn demo",
            "hegn;reboot",
            "hegn\nid",
            "hegn'id",
            "hegn$(id)",
        ] {
            assert!(
                write_seed(
                    &path,
                    &mounts(),
                    "/projects/parser",
                    bad,
                    &plan,
                    None,
                    None,
                    &[],
                    false,
                )
                .is_err(),
                "{bad:?} must be rejected"
            );
            assert!(!path.exists(), "no seed may be written for {bad:?}");
        }
    }

    #[test]
    fn instance_id_is_stable_and_tracks_profile_and_endpoints() {
        let plan = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let mounts = mounts();
        let plain = instance_id(
            "demo",
            &mounts,
            "/projects/parser",
            &plan,
            None,
            None,
            &[],
            false,
        );
        assert_eq!(
            plain,
            instance_id(
                "demo",
                &mounts,
                "/projects/parser",
                &plan,
                None,
                None,
                &[],
                false
            )
        );
        assert_eq!(
            plain,
            format!("hegn-demo-v{SEED_VERSION}-{}", &plain[plain.len() - 12..])
        );

        let profile = ProfileShare {
            bundle_dir: PathBuf::from("/tmp/bundle"),
            manifest_id: "b".repeat(64),
        };
        let with_profile = instance_id(
            "demo",
            &mounts,
            "/projects/parser",
            &plan,
            Some(&profile),
            None,
            &[],
            false,
        );
        assert_ne!(plain, with_profile);

        let lease = lease();
        let no_endpoints = instance_id(
            "demo",
            &mounts,
            "/projects/parser",
            &plan,
            None,
            Some(&lease),
            &[],
            false,
        );
        let endpoint = LocalEndpoint {
            name: "ollama".into(),
            port: 11434,
        };
        let with_endpoint = instance_id(
            "demo",
            &mounts,
            "/projects/parser",
            &plan,
            None,
            Some(&lease),
            &[endpoint],
            false,
        );
        assert_ne!(no_endpoints, with_endpoint);
    }

    #[test]
    fn disk_arg_is_a_raw_cloud_hypervisor_path() {
        let path = Path::new("/var/lib/hegn/demo/cloud-init.img");
        assert_eq!(
            disk_arg(path),
            "path=/var/lib/hegn/demo/cloud-init.img,image_type=raw"
        );
        assert!(!disk_arg(path).contains(' '));
    }
}
