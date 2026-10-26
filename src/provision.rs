//! First-boot provisioning for harness presets.
//!
//! The upstream cloud image stays untouched on the host. Instead, the NoCloud
//! seed carries a small, static install script that runs once inside the guest
//! as root. It installs the preset's packages with apt, upgrades Node.js when
//! the preset needs it, and then installs the harness itself.
//!
//! Only validated package names and static command strings reach the script.
//! User-supplied packages are data, never shell fragments.

use anyhow::{Result, bail};

use crate::config::{Harness, safe_package_name};

/// Everything the guest provisioner needs for one sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionPlan {
    pub harness: Harness,
    pub packages: Vec<String>,
    pub commands: Vec<String>,
    pub needs_node: bool,
}

impl ProvisionPlan {
    /// Build a plan from a harness preset plus extra package selections.
    ///
    /// The preset packages always win: selecting `git` for OpenCode cannot
    /// accidentally omit Node.js or the CLI itself.
    pub fn new(harness: Harness, extra_packages: &[String]) -> Result<Self> {
        for package in extra_packages {
            if !safe_package_name(package) {
                bail!("invalid package name: {package:?}");
            }
        }
        let mut packages: Vec<String> = harness
            .packages()
            .iter()
            .map(|package| package.to_string())
            .collect();
        for package in extra_packages {
            if !packages.iter().any(|existing| existing == package) {
                packages.push(package.clone());
            }
        }
        Ok(Self {
            harness,
            packages,
            commands: harness
                .install_commands()
                .iter()
                .map(|command| command.to_string())
                .collect(),
            needs_node: harness.needs_node(),
        })
    }

    /// Marker written by the guest after a successful provision run.
    pub fn marker() -> &'static str {
        "/var/lib/hegn/provisioned"
    }

    /// The complete first-boot script. Kept as plain shell so it can be
    /// inspected in the seed and re-run manually when debugging a guest.
    pub fn script(&self) -> String {
        let mut script = String::from(
            "#!/bin/bash\n\
             set -euo pipefail\n\
             export DEBIAN_FRONTEND=noninteractive\n\
             export PATH=\"/root/.local/bin:/usr/local/bin:$PATH\"\n\
             MARKER=/var/lib/hegn/provisioned\n\
             if [ -e \"$MARKER\" ]; then\n\
               echo \"hegn: preset already provisioned\"\n\
               exit 0\n\
             fi\n\
             echo \"hegn: installing preset packages\"\n\
             echo 'Acquire::ForceIPv4 \"true\";' >/etc/apt/apt.conf.d/99hegn-ipv4\n\
             apt-get update\n\
             apt-get install -y --no-install-recommends ",
        );
        script.push_str(&self.packages.join(" "));
        script.push('\n');

        if self.needs_node {
            script.push_str(
                "if ! command -v node >/dev/null 2>&1 || [ \"$(node -p 'process.versions.node.split(\".\")[0]')\" -lt 22 ]; then\n\
                   echo \"hegn: installing Node.js 22 from NodeSource\"\n\
                   curl -fsSL --proto '=https' --proto-redir '=https' --retry 3 https://deb.nodesource.com/setup_22.x -o /tmp/hegn-nodesource.sh\n\
                   bash /tmp/hegn-nodesource.sh\n\
                   apt-get install -y --no-install-recommends nodejs\n\
                 fi\n",
            );
        }

        if !self.commands.is_empty() {
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
            "install -d -m 0755 /var/lib/hegn\n\
             install -d -m 0755 /usr/local/bin\n\
             # Installers often use ~/.local/bin. Make those tools available to\n\
             # the serial root login and the hegn user without a re-login.\n\
             if [ -d /root/.local/bin ]; then\n\
               for tool in /root/.local/bin/*; do\n\
                 [ -e \"$tool\" ] || continue\n\
                 ln -sf \"$tool\" \"/usr/local/bin/$(basename \"$tool\")\"\n\
               done\n\
             fi\n\
             cat >/etc/profile.d/hegn-path.sh <<'HEGN_PATH'\n\
             export PATH=\"/root/.local/bin:/usr/local/bin:$PATH\"\n\
             HEGN_PATH\n\
             chmod 0644 /etc/profile.d/hegn-path.sh\n\
             touch \"$MARKER\"\n\
             echo \"hegn: preset ready\"\n",
        );
        script
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_packages_cannot_be_dropped_by_extra_selection() {
        let plan = ProvisionPlan::new(Harness::Opencode, &["git".into()]).unwrap();
        assert!(plan.packages.iter().any(|package| package == "nodejs"));
        assert!(plan.needs_node);
        assert!(
            plan.commands
                .iter()
                .any(|command| command.contains("opencode-ai"))
        );
    }

    #[test]
    fn shell_preset_has_no_harness_command() {
        let plan = ProvisionPlan::new(Harness::Shell, &[]).unwrap();
        assert!(plan.commands.is_empty());
        assert!(!plan.needs_node);
        assert!(!plan.script().contains("npm install"));
    }

    #[test]
    fn extra_packages_are_union_deduped() {
        let plan = ProvisionPlan::new(Harness::Aider, &["git".into(), "jq".into()]).unwrap();
        assert_eq!(
            plan.packages
                .iter()
                .filter(|package| *package == "git")
                .count(),
            1
        );
        assert_eq!(
            plan.packages
                .iter()
                .filter(|package| *package == "jq")
                .count(),
            1
        );
    }

    #[test]
    fn rejects_shell_metacharacters_in_package_names() {
        for bad in ["git;reboot", "$(id)", "git\napt", "-oops", "a b"] {
            assert!(
                ProvisionPlan::new(Harness::Shell, &[bad.to_string()]).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn script_is_idempotent_and_writes_a_marker() {
        let script = ProvisionPlan::new(Harness::ClaudeCode, &[])
            .unwrap()
            .script();
        assert!(script.contains(ProvisionPlan::marker()));
        assert!(script.contains("if [ -e \"$MARKER\" ]"));
        assert!(script.contains("@anthropic-ai/claude-code"));
        assert!(script.contains("apt-get install -y --no-install-recommends"));
    }
}
