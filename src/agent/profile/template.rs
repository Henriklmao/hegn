//! Profile templates, default skill locations and the guest apply script.

use std::path::PathBuf;

use crate::config::Harness;

use super::PROFILE_GUEST_MOUNT;
use super::PROFILE_VERSION;
use super::model::ProfileSkill;

/// The shell script installed in the guest to apply the profile bundle.
pub fn apply_script() -> String {
    format!(
        r#"#!/bin/bash
set -euo pipefail
PROFILE={mount}
[ -f "$PROFILE/manifest" ] || exit 0
while IFS=$'\t' read -r kind bundle target mode; do
  [ -n "$kind" ] || continue
  case "$kind" in
    f)
      src="$PROFILE/files/$bundle"
      [ -f "$src" ] || continue
      if [ -L "$target" ]; then rm -f "$target"; fi
      mkdir -p "$(dirname "$target")"
      install -m "${{mode:-0600}}" "$src" "$target"
      ;;
    d)
      src="$PROFILE/files/$bundle"
      [ -d "$src" ] || continue
      if [ -L "$target" ]; then rm -f "$target"; fi
      install -d -m "${{mode:-0700}}" "$target"
      cp -a "$src/." "$target/"
      ;;
    s)
      src="$PROFILE/skills/$bundle"
      [ -d "$src" ] || continue
      mkdir -p "$(dirname "$target")"
      if [ -L "$target" ] || [ -f "$target" ]; then rm -f "$target"; fi
      if [ -d "$target" ]; then rm -rf "$target"; fi
      ln -s "$src" "$target"
      ;;
  esac
done < "$PROFILE/manifest"
if [ -f "$PROFILE/env.sh" ]; then
  mkdir -p /etc/profile.d
  install -m 0600 "$PROFILE/env.sh" /etc/profile.d/hegn-profile.sh
fi
"#,
        mount = PROFILE_GUEST_MOUNT
    )
}

pub(super) fn default_skills(harness: Harness) -> Vec<ProfileSkill> {
    let pairs: &[(&str, &str)] = match harness {
        Harness::Shell => &[],
        Harness::Opencode => &[
            ("~/.config/opencode/skills", "/root/.config/opencode/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::ClaudeCode => &[
            ("~/.claude/skills", "/root/.claude/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::Codex => &[
            ("~/.codex/skills", "/root/.codex/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::Pi => &[
            ("~/.pi/agent/skills", "/root/.pi/agent/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::PrimeAgent => &[
            ("~/.prime/agent/skills", "/root/.prime/agent/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
        Harness::Aider => &[],
        Harness::GeminiCli => &[
            ("~/.gemini/skills", "/root/.gemini/skills"),
            ("~/.agents/skills", "/root/.agents/skills"),
        ],
    };
    pairs
        .iter()
        .map(|(source, target)| ProfileSkill {
            source: PathBuf::from(source),
            target: (*target).to_string(),
            sync_back: true,
        })
        .collect()
}

pub(super) fn template(harness: Harness) -> String {
    let label = harness.label();
    let home = harness_home(harness);
    let file_examples = file_examples(harness);
    let skill_blocks = default_skills(harness)
        .into_iter()
        .map(|skill| {
            format!(
                "[[skills]]\nsource = {:?}\ntarget = {:?}\nsync_back = true\n",
                skill.source.to_string_lossy(),
                skill.target
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"# hegn harness profile: {label}
#
# Written by `hegn config init`. Edit freely; this file may contain API keys.
# Host paths may start with `~`. Guest targets must be absolute and stay under
# /root, /home, /etc, /usr/local, /opt or /workspace.
#
# `hegn skills sync <instance>` copies host skill changes into the instance copy.
# `hegn skills sync-back <instance>` copies guest changes back to the host.
version = {PROFILE_VERSION}

# Extra apt packages installed on first boot, on top of the harness preset.
packages = []

# Environment variables exported to login shells and the agent process.
[environment]
# OPENAI_API_KEY = "sk-..."
# ANTHROPIC_API_KEY = "sk-..."

# Configuration files copied into the guest on every start. Add entries by
# removing the `#` markers. API keys belong in files such as auth.json or in
# the [environment] table above.
{file_examples}
# Copy skills from this machine into the guest. Skills are copied, never
# linked; the guest target points at the per-instance copy. Missing source
# directories are skipped.
{skill_blocks}
# Guest home for {label}: {home}
"#
    )
}

fn harness_home(harness: Harness) -> &'static str {
    match harness {
        Harness::Opencode | Harness::GeminiCli => "~/.config",
        Harness::ClaudeCode => "~/.claude",
        Harness::Codex => "~/.codex",
        Harness::Pi => "~/.pi/agent",
        Harness::PrimeAgent => "~/.prime/agent",
        Harness::Aider => "~/.aider.conf.yml",
        Harness::Shell => "~",
    }
}

fn file_examples(harness: Harness) -> String {
    let examples: &[(&str, &str)] = match harness {
        Harness::Opencode => &[
            (
                "~/.config/opencode/opencode.json",
                "/root/.config/opencode/opencode.json",
            ),
            (
                "~/.config/opencode/auth.json",
                "/root/.config/opencode/auth.json",
            ),
        ],
        Harness::ClaudeCode => &[
            ("~/.claude/settings.json", "/root/.claude/settings.json"),
            ("~/.claude.json", "/root/.claude.json"),
        ],
        Harness::Codex => &[
            ("~/.codex/config.toml", "/root/.codex/config.toml"),
            ("~/.codex/auth.json", "/root/.codex/auth.json"),
        ],
        Harness::Pi => &[
            ("~/.pi/agent/models.json", "/root/.pi/agent/models.json"),
            ("~/.pi/agent/auth.json", "/root/.pi/agent/auth.json"),
        ],
        Harness::PrimeAgent => &[
            (
                "~/.prime/agent/models.json",
                "/root/.prime/agent/models.json",
            ),
            (
                "~/.prime/agent/settings.json",
                "/root/.prime/agent/settings.json",
            ),
        ],
        Harness::Aider => &[("~/.aider.conf.yml", "/root/.aider.conf.yml")],
        Harness::GeminiCli => &[
            ("~/.gemini/settings.json", "/root/.gemini/settings.json"),
            (
                "~/.gemini/oauth_creds.json",
                "/root/.gemini/oauth_creds.json",
            ),
        ],
        Harness::Shell => &[],
    };
    let mut out = String::new();
    for (source, target) in examples {
        out.push_str(&format!(
            "# [[files]]\n# source = {source:?}\n# target = {target:?}\n# mode = \"0600\"\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::profile::model::Profile;

    #[test]
    fn templates_are_valid_and_contain_no_missing_files() {
        for harness in Harness::ALL {
            if harness == Harness::Shell {
                continue;
            }
            let raw = template(harness);
            let profile: Profile = toml::from_str(&raw).unwrap();
            profile.validate().unwrap();
            assert!(profile.files.is_empty(), "{raw}");
            assert!(!raw.contains("[[files]]\nsource"), "{raw}");
        }
    }

    #[test]
    fn apply_script_is_valid_bash() {
        let script = apply_script();
        let output = std::process::Command::new("bash")
            .arg("-n")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("bash is required for the profile apply script");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
