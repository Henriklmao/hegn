//! Rendering of the cloud-init `user-data` and `meta-data` documents.
//!
//! The user-data is a single YAML cloud-config that creates the `hegn` account,
//! mounts the virtio-fs export, installs the provisioner, health check and
//! serial autologin units, and optionally exports local endpoints and applies
//! the harness profile on every boot.

use crate::agent::profile::{PROFILE_GUEST_MOUNT, PROFILE_SHARE_TAG, ProfileShare, apply_script};
use crate::agent::provision::ProvisionPlan;

pub(super) fn meta_data(instance_id: &str) -> String {
    format!("instance-id: {instance_id}\nlocal-hostname: hegn\n")
}

pub(super) fn user_data(
    shared_tag: &str,
    guest_mount: &str,
    provision: &ProvisionPlan,
    profile: Option<&ProfileShare>,
    endpoints: Option<&str>,
    credential_hardening: bool,
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
    // Hardening is opt-in: the default keeps the passwordless root console.
    // Enabled, the console auto-logs in as the unprivileged user and sudo asks
    // for a password; system services still run as root.
    let (serial_user, sudo_rule) = if credential_hardening {
        ("hegn", "ALL=(ALL) ALL")
    } else {
        ("root", "ALL=(ALL) NOPASSWD:ALL")
    };
    format!(
        r#"#cloud-config
users:
  - default
  - name: hegn
    groups: [sudo]
    shell: /bin/bash
    sudo: {sudo_rule}
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
      ExecStart=-/sbin/agetty --autologin {serial_user} --keep-baud 115200,38400,9600 %I $TERM
  - path: /etc/systemd/system/serial-getty@ttyAMA0.service.d/autologin.conf
    permissions: '0644'
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin {serial_user} --keep-baud 115200,38400,9600 %I $TERM
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;
    use std::path::PathBuf;

    fn plan() -> ProvisionPlan {
        ProvisionPlan::new(Harness::Shell, &[]).unwrap()
    }

    #[test]
    fn meta_data_is_a_single_instance_block() {
        let meta = meta_data("hegn-demo-v8-0123456789ab");
        assert_eq!(
            meta,
            "instance-id: hegn-demo-v8-0123456789ab\nlocal-hostname: hegn\n"
        );
        assert_eq!(meta.lines().count(), 2);
        assert!(!meta.contains("instance-id: hegn-demo\n"));
    }

    #[test]
    fn mount_and_tag_are_single_quoted_everywhere() {
        let data = user_data("workspace", "/workspace", &plan(), None, None, false);
        assert!(
            data.contains("- [ 'workspace', '/workspace', virtiofs"),
            "{data}"
        );
        assert!(
            data.contains("mkdir -p '/workspace' || true"),
            "mkdir must quote the mount: {data}"
        );
        assert!(
            data.contains("mount -t virtiofs 'workspace' '/workspace' || true"),
            "mount must quote both values: {data}"
        );
        assert!(
            data.contains("directory = /workspace"),
            "git safe.directory must point at the guest mount: {data}"
        );
    }

    #[test]
    fn credential_hardening_is_opt_in() {
        let relaxed = user_data("workspace", "/workspace", &plan(), None, None, false);
        assert!(relaxed.contains("--autologin root"), "{relaxed}");
        assert!(
            relaxed.contains("sudo: ALL=(ALL) NOPASSWD:ALL"),
            "{relaxed}"
        );

        let hardened = user_data("workspace", "/workspace", &plan(), None, None, true);
        assert!(hardened.contains("--autologin hegn"), "{hardened}");
        assert!(!hardened.contains("--autologin root"), "{hardened}");
        assert!(hardened.contains("sudo: ALL=(ALL) ALL"), "{hardened}");
        assert!(!hardened.contains("NOPASSWD"), "{hardened}");
        assert!(hardened.contains("lock_passwd: true"), "{hardened}");
    }

    #[test]
    fn endpoint_snippets_are_indented_under_a_private_write_file() {
        let content =
            "# Generated by hegn; do not edit.\nexport HEGN_ENDPOINT_OLLAMA=\"http://10.213.0.1:11434\""
                .to_string();
        let data = user_data(
            "workspace",
            "/workspace",
            &plan(),
            None,
            Some(&content),
            false,
        );
        assert!(
            data.contains(
                "  - path: /etc/profile.d/hegn-endpoints.sh\n    permissions: '0644'\n    content: |\n"
            ),
            "{data}"
        );
        for line in content.lines() {
            assert!(
                data.contains(&format!("      {line}")),
                "endpoint line {line:?} must stay inside the YAML block: {data}"
            );
        }
    }

    #[test]
    fn profile_payload_uses_the_reserved_tag_and_a_private_script() {
        let profile = ProfileShare {
            bundle_dir: PathBuf::from("/tmp/bundle"),
            manifest_id: "c".repeat(64),
        };
        let data = user_data(
            "workspace",
            "/workspace",
            &plan(),
            Some(&profile),
            None,
            false,
        );
        assert!(
            data.contains("[ 'hegn-profile', '/run/hegn/profile', virtiofs"),
            "{data}"
        );
        assert!(
            data.contains(
                "  - path: /usr/local/sbin/hegn-apply-profile\n    permissions: '0700'\n"
            ),
            "{data}"
        );
        assert!(
            data.contains("mount -t virtiofs 'hegn-profile' '/run/hegn/profile' || true"),
            "{data}"
        );
    }

    #[test]
    fn the_embedded_provision_script_is_fully_indented() {
        let plan = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let data = user_data("workspace", "/workspace", &plan, None, None, false);
        assert!(
            data.contains("  - path: /usr/local/sbin/hegn-provision\n    permissions: '0700'\n"),
            "{data}"
        );
        for line in plan.script().lines() {
            assert!(
                data.contains(&format!("      {line}")),
                "script line {line:?} escaped the YAML block: {data}"
            );
        }
    }

    #[test]
    fn indent_pads_blank_lines_and_does_not_invent_content() {
        assert_eq!(indent("first\n\nlast\n", 2), "  first\n  \n  last");
        assert_eq!(indent("", 4), "");
        assert_eq!(indent("one line", 1), " one line");
    }
}
