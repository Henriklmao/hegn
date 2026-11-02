//! Harness presets: the provision plan, its markers and per-stage estimates.
//!
//! The preset packages always win: selecting `git` for OpenCode cannot
//! accidentally omit Node.js or the CLI itself. User-supplied packages are
//! data: they are validated before they reach the generated script.

use std::time::Duration;

use anyhow::{Result, bail};

use crate::config::{Harness, safe_package_name};

/// Stages emitted by the guest provisioner, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Prepare,
    Grow,
    Network,
    AptUpdate,
    Packages,
    Node,
    Harness,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Prepare => "preparing the guest",
            Self::Grow => "growing the disk",
            Self::Network => "waiting for the network",
            Self::AptUpdate => "updating package lists",
            Self::Packages => "installing system packages",
            Self::Node => "installing Node.js 22",
            Self::Harness => "installing the harness",
        }
    }

    pub(super) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "prepare" => Self::Prepare,
            "grow" => Self::Grow,
            "network" => Self::Network,
            "apt-update" => Self::AptUpdate,
            "packages" => Self::Packages,
            "node" => Self::Node,
            "harness" => Self::Harness,
            _ => return None,
        })
    }
}

/// Everything the guest provisioner needs for one instance.
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

    /// Marker for the exact package set, so adding a profile package later
    /// installs it without re-running the harness installer.
    pub fn package_marker() -> &'static str {
        "/var/lib/hegn/provisioned-packages"
    }

    /// Stable digest of the apt package list.
    pub fn package_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.packages.join("\n").as_bytes());
        hex::encode(&hasher.finalize()[..8])
    }
}

/// Estimated wall-clock cost of one provisioning stage.
pub(super) fn stage_estimate(plan: &ProvisionPlan) -> Vec<(Stage, Duration)> {
    let mut stages = vec![
        (Stage::Prepare, Duration::from_secs(5)),
        (Stage::Grow, Duration::from_secs(8)),
        (Stage::Network, Duration::from_secs(15)),
        (Stage::AptUpdate, Duration::from_secs(60)),
        (
            Stage::Packages,
            Duration::from_secs(20 + 3 * plan.packages.len() as u64),
        ),
    ];
    if plan.needs_node {
        stages.push((Stage::Node, Duration::from_secs(60)));
    }
    if !plan.commands.is_empty() {
        stages.push((
            Stage::Harness,
            Duration::from_secs(45 + 20 * plan.commands.len() as u64),
        ));
    }
    stages
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
    fn package_changes_update_the_provisioned_package_set() {
        let base = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let extra = ProvisionPlan::new(Harness::Opencode, &["tmux".into()]).unwrap();
        assert_ne!(base.package_hash(), extra.package_hash());
        let script = extra.script();
        assert!(script.contains(&extra.package_hash()), "{script}");
        assert!(
            script.contains("harness already installed; package list updated"),
            "{script}"
        );
    }
}
