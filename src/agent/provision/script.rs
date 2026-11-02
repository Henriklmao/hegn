//! The generated first-boot install script.
//!
//! The script is kept as plain shell so it can be inspected in the NoCloud seed
//! and re-run manually when debugging a guest. Only validated package names and
//! static command strings are interpolated.

use super::presets::ProvisionPlan;

impl ProvisionPlan {
    /// The complete first-boot script. Kept as plain shell so it can be
    /// inspected in the seed and re-run manually when debugging a guest.
    pub fn script(&self) -> String {
        let mut script = format!(
            r#"#!/bin/bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
export PATH="/root/.local/bin:/usr/local/bin:$PATH"
MARKER={marker}
PACKAGE_MARKER={package_marker}
PACKAGE_HASH={package_hash}

progress() {{
  printf 'hegn-progress: {{"stage":"%s"}}\n' "$1"
}}

provision_failed() {{
  code=$?
  if [ "$code" -ne 0 ]; then
    printf 'hegn-progress: {{"stage":"failed","code":%s}}\n' "$code"
  fi
}}
trap provision_failed EXIT

# The cloud image starts with a static guest route. systemd-resolved can still
# be warming up when the provisioner starts, so wait for DNS and fall back to
# the guarded public resolvers instead of letting apt burn minutes on retries.
apt_network_ready() {{
  attempt=0
  while [ "$attempt" -lt 12 ]; do
    if getent ahostsv4 archive.ubuntu.com >/dev/null 2>&1; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 2
  done
  if [ -L /etc/resolv.conf ] || ! grep -q '^nameserver 1.1.1.1' /etc/resolv.conf 2>/dev/null; then
    rm -f /etc/resolv.conf
    printf 'nameserver 1.1.1.1\nnameserver 9.9.9.9\n' > /etc/resolv.conf
  fi
  attempt=0
  while [ "$attempt" -lt 10 ]; do
    if getent ahostsv4 archive.ubuntu.com >/dev/null 2>&1; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 2
  done
  return 1
}}

apt_update() {{
  attempt=0
  while [ "$attempt" -lt 5 ]; do
    if apt-get -o Acquire::Retries=5 -o DPkg::Lock::Timeout=300 update; then
      return 0
    fi
    attempt=$((attempt + 1))
    sleep 5
  done
  return 1
}}

apt_install() {{
  attempt=0
  while [ "$attempt" -lt 3 ]; do
    if apt-get -o Acquire::Retries=5 -o DPkg::Lock::Timeout=300 \
      install -y --no-install-recommends "$@"; then
      return 0
    fi
    dpkg --configure -a || true
    attempt=$((attempt + 1))
    sleep 5
  done
  return 1
}}

grow_rootfs() {{
  root_source=$(findmnt -n -o SOURCE / 2>/dev/null || true)
  if [ -z "$root_source" ] || ! command -v growpart >/dev/null 2>&1; then
    return 0
  fi
  root_name=$(basename "$root_source")
  parent=$(lsblk -n -o PKNAME "$root_source" 2>/dev/null | head -n 1 || true)
  partnum=$(cat "/sys/class/block/$root_name/partition" 2>/dev/null || true)
  if [ -z "$parent" ] || [ -z "$partnum" ]; then
    return 0
  fi
  echo "hegn: growing the root filesystem"
  growpart "/dev/$parent" "$partnum" || true
  resize2fs "$root_source" || true
}}

install -d -m 0755 /var/lib/hegn
progress prepare
if [ -x /usr/local/sbin/hegn-apply-profile ]; then
  echo "hegn: applying harness profile"
  /usr/local/sbin/hegn-apply-profile || echo "hegn: profile apply failed" >&2
fi
if [ -e "$MARKER" ] && [ -f "$PACKAGE_MARKER" ] && [ "$(cat "$PACKAGE_MARKER")" = "$PACKAGE_HASH" ]; then
  echo "hegn: preset already provisioned"
  progress ready
  exit 0
fi
if [ -e "$MARKER" ] && [ ! -f "$PACKAGE_MARKER" ]; then
  # Upgrade from a hegn version without a package marker: keep the existing,
  # already provisioned harness instead of hitting the network again.
  echo "$PACKAGE_HASH" > "$PACKAGE_MARKER"
  echo "hegn: recorded the existing package set"
  progress ready
  exit 0
fi
progress grow
grow_rootfs
progress network
if ! apt_network_ready; then
  echo "hegn: warning: DNS is not ready yet" >&2
fi
echo "hegn: installing preset packages"
echo 'Acquire::ForceIPv4 "true";' >/etc/apt/apt.conf.d/99hegn-ipv4
progress apt-update
if ! apt_update; then
  echo "hegn: apt-get update failed" >&2
  exit 1
fi
dpkg --configure -a || true
progress packages
"#,
            marker = Self::marker(),
            package_marker = Self::package_marker(),
            package_hash = self.package_hash(),
        );
        if !self.packages.is_empty() {
            script.push_str("apt_install ");
            script.push_str(&self.packages.join(" "));
            script.push_str(" || { echo \"hegn: package installation failed\" >&2; exit 1; }\n");
        }
        script.push_str(
            r#"echo "$PACKAGE_HASH" > "$PACKAGE_MARKER"
apt-get clean || true
if [ -e "$MARKER" ]; then
  echo "hegn: harness already installed; package list updated"
  progress ready
  exit 0
fi
"#,
        );

        if self.needs_node {
            script.push_str(
                r#"progress node
if ! command -v node >/dev/null 2>&1 || [ "$(node -p 'process.versions.node.split(".")[0]')" -lt 22 ]; then
  echo "hegn: installing Node.js 22 from NodeSource"
  curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 https://deb.nodesource.com/setup_22.x -o /tmp/hegn-nodesource.sh
  bash /tmp/hegn-nodesource.sh
  apt_install nodejs || { echo "hegn: Node.js installation failed" >&2; exit 1; }
fi
"#,
            );
        }

        if !self.commands.is_empty() {
            script.push_str("progress harness\n");
            script.push_str("echo \"hegn: installing ");
            script.push_str(self.harness.label());
            script.push_str("\"\n");
            for command in &self.commands {
                script.push_str(command);
                script.push('\n');
            }
        }

        if self.harness.command() != "shell" {
            script.push_str(&format!(
                "if ! command -v {} >/dev/null 2>&1; then\n\
                   echo \"hegn: preset install finished but '{}' is not on PATH\" >&2\n\
                   exit 1\n\
                 fi\n",
                self.harness.command(),
                self.harness.command()
            ));
        }

        script.push_str(
            r#"install -d -m 0755 /usr/local/bin
# Installers often use ~/.local/bin. Make those tools available to the serial
# root login and the hegn user without a re-login.
if [ -d /root/.local/bin ]; then
  for tool in /root/.local/bin/*; do
    [ -e "$tool" ] || continue
    ln -sf "$tool" "/usr/local/bin/$(basename "$tool")"
  done
fi
cat >/etc/profile.d/hegn-path.sh <<'HEGN_PATH'
export PATH="/root/.local/bin:/usr/local/bin:$PATH"
HEGN_PATH
chmod 0644 /etc/profile.d/hegn-path.sh
touch "$MARKER"
progress ready
echo "hegn: preset ready"
"#,
        );
        script
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Harness;

    #[test]
    fn script_is_idempotent_and_writes_a_marker() {
        let script = ProvisionPlan::new(Harness::ClaudeCode, &[])
            .unwrap()
            .script();
        assert!(script.contains(ProvisionPlan::marker()));
        assert!(script.contains(ProvisionPlan::package_marker()));
        assert!(script.contains("if [ -e \"$MARKER\" ]"));
        assert!(script.contains("@anthropic-ai/claude-code"));
        assert!(script.contains("install -y --no-install-recommends"));
        assert!(script.contains("grow_rootfs"));
        assert!(script.contains("apt_network_ready"));
    }

    #[test]
    fn provision_script_is_valid_bash() {
        for harness in Harness::ALL {
            let script = ProvisionPlan::new(harness, &["jq".into()])
                .unwrap()
                .script();
            let output = std::process::Command::new("bash")
                .arg("-n")
                .arg("-c")
                .arg(&script)
                .output()
                .expect("bash is required for the provisioning script");
            assert!(
                output.status.success(),
                "{}: {}",
                harness.label(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
