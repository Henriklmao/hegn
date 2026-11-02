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

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::{Harness, safe_package_name};

/// Prefix used by the guest for machine-readable progress lines.
pub const PROGRESS_PREFIX: &str = "hegn-progress:";

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

    fn parse(value: &str) -> Option<Self> {
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

/// One event parsed from the guest's serial output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionEvent {
    Stage(Stage),
    Ready,
    Failed {
        code: Option<i64>,
        message: Option<String>,
    },
}

/// Guest progress lines plus the local start/end markers.
///
/// Parsing is intentionally tolerant: systemd prefixes every line with a
/// timestamp and the service name, and the serial console may interleave other
/// output, so only the JSON object after [`PROGRESS_PREFIX`] matters.
pub fn parse_progress(line: &str) -> Option<ProvisionEvent> {
    if line.contains("hegn: preset ready") || line.contains("preset already provisioned") {
        return Some(ProvisionEvent::Ready);
    }
    if line.contains("Failed to start hegn harness provisioning") {
        return Some(ProvisionEvent::Failed {
            code: None,
            message: Some("hegn-provision.service failed".into()),
        });
    }
    let rest = &line[line.find(PROGRESS_PREFIX)? + PROGRESS_PREFIX.len()..];
    let start = rest.find('{')?;
    let end = rest.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&rest[start..=end]).ok()?;
    match value.get("stage").and_then(serde_json::Value::as_str)? {
        "ready" => Some(ProvisionEvent::Ready),
        "failed" => Some(ProvisionEvent::Failed {
            code: value.get("code").and_then(serde_json::Value::as_i64),
            message: value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(sanitize_message),
        }),
        stage => Stage::parse(stage).map(ProvisionEvent::Stage),
    }
}

/// Serial output is untrusted. Keep diagnostic text short and strip control
/// characters so a compromised guest cannot inject terminal escapes into a
/// notification.
fn sanitize_message(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(160)
        .collect()
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

/// Estimated wall-clock cost of one provisioning stage.
fn stage_estimate(plan: &ProvisionPlan) -> Vec<(Stage, Duration)> {
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

/// Interpolated progress for the TUI and CLI notifications.
#[derive(Debug, Clone)]
pub struct ProvisionProgress {
    harness: Harness,
    stages: Vec<(Stage, Duration)>,
    current: usize,
    started: Instant,
    stage_started: Instant,
    ready: bool,
}

impl ProvisionProgress {
    pub fn new(plan: &ProvisionPlan) -> Self {
        Self {
            harness: plan.harness,
            stages: stage_estimate(plan),
            current: 0,
            started: Instant::now(),
            stage_started: Instant::now(),
            ready: false,
        }
    }

    pub fn harness_label(&self) -> &'static str {
        self.harness.label()
    }

    pub fn advance(&mut self, stage: Stage) {
        if let Some(index) = self.stages.iter().position(|(known, _)| *known == stage)
            && index >= self.current
        {
            self.current = index;
            self.stage_started = Instant::now();
        }
    }

    pub fn finish(&mut self) {
        self.ready = true;
    }

    pub fn stage_label(&self) -> &'static str {
        if self.ready {
            return "ready";
        }
        self.stages
            .get(self.current)
            .map(|(stage, _)| stage.label())
            .unwrap_or("finishing up")
    }

    /// Completion in `0.0..=1.0`, interpolated inside the current stage.
    pub fn fraction(&self) -> f64 {
        if self.ready {
            return 1.0;
        }
        let total: f64 = self
            .stages
            .iter()
            .map(|(_, duration)| duration.as_secs_f64())
            .sum();
        if total <= 0.0 {
            return 0.0;
        }
        let completed: f64 = self.stages[..self.current]
            .iter()
            .map(|(_, duration)| duration.as_secs_f64())
            .sum();
        let within = self
            .stage_started
            .elapsed()
            .as_secs_f64()
            .min(self.stages[self.current].1.as_secs_f64());
        ((completed + within) / total).clamp(0.0, 0.99)
    }

    /// Rough remaining time, based on the per-stage estimates.
    pub fn remaining(&self) -> Duration {
        if self.ready {
            return Duration::ZERO;
        }
        let total: f64 = self
            .stages
            .iter()
            .map(|(_, duration)| duration.as_secs_f64())
            .sum();
        let completed: f64 = self.stages[..self.current]
            .iter()
            .map(|(_, duration)| duration.as_secs_f64())
            .sum();
        let within = self
            .stage_started
            .elapsed()
            .as_secs_f64()
            .min(self.stages[self.current].1.as_secs_f64());
        Duration::from_secs_f64((total - completed - within).max(0.0))
    }

    pub fn eta_label(&self) -> String {
        if self.ready {
            return "done".into();
        }
        let seconds = self.remaining().as_secs();
        if seconds == 0 {
            return "almost done".into();
        }
        if seconds < 60 {
            return format!("~{seconds}s left");
        }
        let minutes = seconds.div_ceil(60);
        format!("~{minutes}m left")
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Incremental reader for a session's serial log.
///
/// The relay appends to the log; polling keeps the progress bar live without
/// taking the console socket away from the attach client.
pub struct ProvisionWatcher {
    file: fs::File,
    offset: u64,
    carry: String,
    progress: ProvisionProgress,
    finished: bool,
    saw_progress: bool,
}

impl ProvisionWatcher {
    pub fn open(path: &Path, plan: &ProvisionPlan) -> Result<Self> {
        let file =
            fs::File::open(path).with_context(|| format!("failed to watch {}", path.display()))?;
        Ok(Self {
            file,
            offset: 0,
            carry: String::new(),
            progress: ProvisionProgress::new(plan),
            finished: false,
            saw_progress: false,
        })
    }

    pub fn progress(&self) -> &ProvisionProgress {
        &self.progress
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    pub fn saw_progress(&self) -> bool {
        self.saw_progress
    }

    /// Read whatever has been appended since the previous poll.
    pub fn poll(&mut self) -> Result<Vec<ProvisionEvent>> {
        let length = self.file.metadata()?.len();
        if length < self.offset {
            self.offset = 0;
            self.carry.clear();
        }
        self.file.seek(SeekFrom::Start(self.offset))?;
        let mut raw = Vec::new();
        self.file.read_to_end(&mut raw)?;
        self.offset += raw.len() as u64;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let mut text = std::mem::take(&mut self.carry);
        text.push_str(&String::from_utf8_lossy(&raw));
        let mut lines: Vec<&str> = text.split('\n').collect();
        self.carry = lines.pop().unwrap_or_default().to_string();
        let mut events = Vec::new();
        for line in lines {
            if let Some(event) = parse_progress(line) {
                self.saw_progress = true;
                match &event {
                    ProvisionEvent::Stage(stage) => self.progress.advance(*stage),
                    ProvisionEvent::Ready => {
                        self.progress.finish();
                        self.finished = true;
                    }
                    ProvisionEvent::Failed { .. } => self.finished = true,
                }
                events.push(event);
            }
        }
        Ok(events)
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
        assert!(script.contains(ProvisionPlan::package_marker()));
        assert!(script.contains("if [ -e \"$MARKER\" ]"));
        assert!(script.contains("@anthropic-ai/claude-code"));
        assert!(script.contains("install -y --no-install-recommends"));
        assert!(script.contains("grow_rootfs"));
        assert!(script.contains("apt_network_ready"));
    }

    #[test]
    fn progress_lines_are_parsed_through_the_systemd_prefix() {
        let line = "[   12.345678] hegn-provision[123]: hegn-progress: {\"stage\":\"packages\"}";
        assert_eq!(
            parse_progress(line),
            Some(ProvisionEvent::Stage(Stage::Packages))
        );
        assert_eq!(
            parse_progress("hegn: preset ready"),
            Some(ProvisionEvent::Ready)
        );
        assert_eq!(
            parse_progress("hegn-progress: {\"stage\":\"failed\",\"code\":2}"),
            Some(ProvisionEvent::Failed {
                code: Some(2),
                message: None
            })
        );
        // A malformed line from the guest must never panic the watcher.
        assert_eq!(parse_progress("hegn-progress: } not json {"), None);
        assert_eq!(
            parse_progress("hegn-progress: {\"stage\":\"failed\",\"message\":\"a\\u001b[2Jb\"}"),
            Some(ProvisionEvent::Failed {
                code: None,
                message: Some("a[2Jb".into())
            })
        );
    }

    #[test]
    fn progress_is_interpolated_between_known_stages() {
        let plan = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let mut progress = ProvisionProgress::new(&plan);
        assert_eq!(progress.stage_label(), Stage::Prepare.label());
        progress.advance(Stage::Packages);
        assert_eq!(progress.stage_label(), Stage::Packages.label());
        assert!(progress.fraction() > 0.0 && progress.fraction() < 1.0);
        assert!(!progress.eta_label().is_empty());
        progress.finish();
        assert_eq!(progress.fraction(), 1.0);
    }

    #[test]
    fn watcher_reads_progress_appended_to_a_live_log() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        std::fs::write(&path, b"booting\n").unwrap();
        let plan = ProvisionPlan::new(Harness::Opencode, &[]).unwrap();
        let mut watcher = ProvisionWatcher::open(&path, &plan).unwrap();
        assert!(watcher.poll().unwrap().is_empty());

        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            log,
            "[  1.0] hegn-provision[1]: hegn-progress: {{\"stage\":\"packages\"}}"
        )
        .unwrap();
        let events = watcher.poll().unwrap();
        assert_eq!(events, vec![ProvisionEvent::Stage(Stage::Packages)]);
        assert!(watcher.saw_progress());

        writeln!(log, "hegn: preset ready").unwrap();
        let events = watcher.poll().unwrap();
        assert_eq!(events, vec![ProvisionEvent::Ready]);
        assert!(watcher.finished());
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
