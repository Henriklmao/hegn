//! Minimal NoCloud seed for the official Cloud Hypervisor cloud images.
//!
//! The upstream images have no usable local account until cloud-init runs. hegn
//! therefore writes a small FAT volume labelled `CIDATA` and attaches it as a
//! second disk. The seed configures an automatic root login on the serial
//! console and one first-boot provisioner for the selected harness preset.
//!
//! The seed contains no secrets and is regenerated on every boot.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};

use crate::config::LocalEndpoint;
use crate::network::Lease;
use crate::profile::{PROFILE_GUEST_MOUNT, PROFILE_SHARE_TAG, ProfileShare, apply_script};
use crate::provision::ProvisionPlan;

/// 4 MiB is enough for the seed and keeps it cheap.
const SEED_BYTES: u64 = 4 * 1024 * 1024;

/// Increment when the seed's user-data changes so existing instances re-run
/// cloud-init on their next boot.
pub const SEED_VERSION: u32 = 8;

fn meta_data(instance_id: &str) -> String {
    format!("instance-id: {instance_id}\nlocal-hostname: hegn\n")
}

fn user_data(
    shared_tag: &str,
    guest_mount: &str,
    provision: &ProvisionPlan,
    profile: Option<&ProfileShare>,
    endpoints: Option<&str>,
) -> String {
    let script = indent(&provision.script(), 6);
    let endpoint_file = match endpoints {
        Some(content) => format!(
            "  - path: /etc/profile.d/hegn-endpoints.sh\n\
             \x20   permissions: '0644'\n\
             \x20   content: |\n{}\n",
            indent(content, 6)
        ),
        None => String::new(),
    };
    let profile_mount = match profile {
        Some(_) => format!(
            "  - [ '{}', '{}', virtiofs, \"defaults,nofail\", \"0\", \"0\" ]\n",
            PROFILE_SHARE_TAG, PROFILE_GUEST_MOUNT
        ),
        None => String::new(),
    };
    let profile_file = match profile {
        Some(_) => {
            let apply = indent(&apply_script(), 6);
            format!(
                "  - path: /usr/local/sbin/hegn-apply-profile\n\
                 \x20   permissions: '0700'\n\
                 \x20   content: |\n{apply}\n"
            )
        }
        None => String::new(),
    };
    let profile_runcmd = match profile {
        Some(_) => format!(
            "  - mkdir -p '{}' || true\n\
             \x20 - mountpoint -q '{}' || mount -t virtiofs '{}' '{}' || true\n",
            PROFILE_GUEST_MOUNT, PROFILE_GUEST_MOUNT, PROFILE_SHARE_TAG, PROFILE_GUEST_MOUNT
        ),
        None => String::new(),
    };
    format!(
        r#"#cloud-config
users:
  - default
  - name: hegn
    groups: [sudo]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    lock_passwd: true
mounts:
  - [ '{shared_tag}', '{guest_mount}', virtiofs, "defaults,nofail", "0", "0" ]
{profile_mount}write_files:
  - path: /etc/gitconfig
    permissions: '0644'
    content: |
      [safe]
          directory = {guest_mount}
{endpoint_file}{profile_file}  - path: /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
    permissions: '0644'
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin root --keep-baud 115200,38400,9600 %I $TERM
  - path: /etc/systemd/system/serial-getty@ttyAMA0.service.d/autologin.conf
    permissions: '0644'
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin root --keep-baud 115200,38400,9600 %I $TERM
  - path: /usr/local/sbin/hegn-provision
    permissions: '0700'
    content: |
{script}
  - path: /etc/systemd/system/hegn-provision.service
    permissions: '0644'
    content: |
      [Unit]
      Description=hegn harness provisioning
      Wants=network-online.target
      After=network-online.target

      [Service]
      Type=oneshot
      ExecStart=/usr/local/sbin/hegn-provision
      RemainAfterExit=yes
      StandardOutput=journal+console
      StandardError=journal+console
      TimeoutStartSec=0

      [Install]
      WantedBy=multi-user.target
  - path: /usr/local/sbin/hegn-health
    permissions: '0700'
    content: |
      #!/bin/bash
      set -u
      export DEBIAN_FRONTEND=noninteractive
      checked=$(date +%s)
      count=unknown
      reason=ok
      if [ -d /var/lib/apt/lists ] && [ -z "$(find /var/lib/apt/lists -maxdepth 0 -mmin -360 2>/dev/null)" ]; then
        if ! apt-get update -qq >/tmp/hegn-health.log 2>&1; then
          reason=offline
        fi
      fi
      if [ "$reason" = ok ]; then
        count=$(apt-get -s upgrade 2>/dev/null | grep -c '^Inst ' || true)
        [ -n "$count" ] || count=unknown
      fi
      echo "hegn-health v1 apt=$count checked=$checked reason=$reason"
  - path: /etc/systemd/system/hegn-health.service
    permissions: '0644'
    content: |
      [Unit]
      Description=hegn apt health
      Wants=network-online.target
      After=network-online.target hegn-provision.service

      [Service]
      Type=oneshot
      ExecStart=/usr/local/sbin/hegn-health
      StandardOutput=journal+console
      StandardError=journal+console
      TimeoutStartSec=300

      [Install]
      WantedBy=multi-user.target
runcmd:
  - systemctl daemon-reload || true
  - systemctl enable serial-getty@ttyS0.service || true
  - systemctl restart serial-getty@ttyS0.service || true
  - systemctl enable serial-getty@ttyAMA0.service || true
  - systemctl restart serial-getty@ttyAMA0.service || true
  - mkdir -p '{guest_mount}' || true
  - mountpoint -q '{guest_mount}' || mount -t virtiofs '{shared_tag}' '{guest_mount}' || true
{profile_runcmd}  - systemctl enable --now hegn-provision.service || true
  - systemctl enable hegn-health.service || true
  - systemctl start --no-block hegn-health.service || true
"#
    )
}

fn indent(text: &str, spaces: usize) -> String {
    let padding = " ".repeat(spaces);
    text.lines()
        .map(|line| format!("{padding}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Shell snippet exported to login shells inside the guest.
///
/// `OLLAMA_HOST` is set as a convenience alias; every endpoint is also
/// available as `HEGN_ENDPOINT_<NAME>` for tools without a dedicated variable.
fn endpoint_env(lease: &Lease, endpoints: &[LocalEndpoint]) -> String {
    let mut lines = String::from("# Generated by hegn; do not edit.\n");
    for endpoint in endpoints {
        let url = endpoint.url(lease.host_ip);
        lines.push_str(&format!("export {}=\"{url}\"\n", endpoint.env_name()));
        if endpoint.name.eq_ignore_ascii_case("ollama") {
            lines.push_str(&format!("export OLLAMA_HOST=\"{url}\"\n"));
        }
    }
    lines
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
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
}

/// Write (or replace) the NoCloud seed at `path`.
///
/// The image is created with mode `0600` and `O_NOFOLLOW`; a planted symlink
/// cannot redirect the write.
#[allow(clippy::too_many_arguments)]
pub fn write_seed(
    path: &Path,
    shared_tag: &str,
    guest_mount: &str,
    instance_id: &str,
    provision: &ProvisionPlan,
    profile: Option<&ProfileShare>,
    network: Option<&Lease>,
    endpoints: &[LocalEndpoint],
) -> Result<()> {
    if !safe_token(shared_tag) {
        anyhow::bail!("unsafe virtio-fs tag for the cloud-init seed: {shared_tag:?}");
    }
    if !safe_mount(guest_mount) {
        anyhow::bail!("unsafe guest mount path for the cloud-init seed: {guest_mount:?}");
    }
    if !safe_token(instance_id) {
        anyhow::bail!("unsafe cloud-init instance id: {instance_id:?}");
    }
    let endpoint_env = network
        .filter(|_| !endpoints.is_empty())
        .map(|lease| endpoint_env(lease, endpoints));
    let user_data = user_data(
        shared_tag,
        guest_mount,
        provision,
        profile,
        endpoint_env.as_deref(),
    );
    let meta_data = meta_data(instance_id);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("failed to create cloud-init seed {}", path.display()))?;
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
        write_file(&root, "meta-data", &meta_data)?;
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

/// Stable id for this seed. It changes when the seed version, preset or
/// network configuration changes, which makes cloud-init re-apply the new
/// user-data/network-config even when the rootfs is reused.
pub fn instance_id(
    instance: &str,
    provision: &ProvisionPlan,
    profile: Option<&ProfileShare>,
    network: Option<&Lease>,
    endpoints: &[LocalEndpoint],
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(SEED_VERSION.to_le_bytes());
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
    format!("hegn-{instance}-v{SEED_VERSION}-{}", &digest[..12])
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
            "workspace",
            "/workspace",
            "hegn-demo",
            &plan,
            None,
            Some(&lease()),
            &[],
        )
        .unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let (user_data, meta_data, network) = read_seed(&path);
        assert!(user_data.contains("autologin root"), "{user_data}");
        assert!(
            user_data.contains("mount -t virtiofs 'workspace' '/workspace'"),
            "{user_data}"
        );
        assert!(user_data.contains("directory = /workspace"), "{user_data}");
        assert!(user_data.contains("/usr/local/sbin/hegn-provision"));
        assert!(user_data.contains("hegn-provision.service"));
        assert!(meta_data.contains("instance-id: hegn-demo"), "{meta_data}");
        let network = network.expect("network-config must be present");
        assert!(network.contains("10.213.0.2/30"), "{network}");
        assert!(network.contains("via: 10.213.0.1"), "{network}");
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
            "workspace",
            "/workspace",
            "hegn-demo",
            &plan,
            None,
            Some(&lease()),
            &endpoints,
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
        use std::path::PathBuf;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let profile = ProfileShare {
            bundle_dir: PathBuf::from("/tmp/profile-bundle"),
            manifest_id: "a".repeat(64),
        };
        write_seed(
            &path,
            "workspace",
            "/workspace",
            "hegn-demo",
            &plan,
            Some(&profile),
            None,
            &[],
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
    fn instance_id_changes_with_preset_and_network() {
        let shell = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        let opencode = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let a = instance_id("demo", &shell, None, None, &[]);
        assert_eq!(a, instance_id("demo", &shell, None, None, &[]));
        assert_ne!(a, instance_id("demo", &opencode, None, None, &[]));
        assert_ne!(a, instance_id("demo", &shell, None, Some(&lease()), &[]));
        assert!(a.starts_with("hegn-demo-v"));
    }

    #[test]
    fn off_network_omits_network_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        write_seed(
            &path,
            "workspace",
            "/workspace",
            "hegn-demo",
            &plan,
            None,
            None,
            &[],
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
                "workspace",
                "/workspace",
                "hegn-demo",
                &plan,
                None,
                None,
                &[]
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
        assert!(
            write_seed(
                &path,
                "bad tag",
                "/workspace",
                "hegn-demo",
                &plan,
                None,
                None,
                &[]
            )
            .is_err()
        );
        assert!(
            write_seed(
                &path,
                "workspace",
                "/work space",
                "hegn-demo",
                &plan,
                None,
                None,
                &[]
            )
            .is_err()
        );
        assert!(
            write_seed(
                &path,
                "workspace;reboot",
                "/workspace",
                "hegn-demo",
                &plan,
                None,
                None,
                &[]
            )
            .is_err()
        );
        assert!(
            write_seed(
                &path,
                "workspace",
                "/workspace;reboot",
                "hegn-demo",
                &plan,
                None,
                None,
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn replaces_an_existing_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-init.img");
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        write_seed(
            &path,
            "workspace",
            "/workspace",
            "hegn-demo",
            &plan,
            None,
            None,
            &[],
        )
        .unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() >= SEED_BYTES);
    }
}
