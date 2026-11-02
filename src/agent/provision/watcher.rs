//! Serial-progress parsing and the interpolated progress bar.
//!
//! The guest emits small `hegn-progress:` JSON lines on the serial console.
//! Parsing is intentionally tolerant: systemd prefixes every line with a
//! timestamp and the service name, and the serial console may interleave other
//! output, so only the JSON object after the prefix matters.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::Harness;

use super::presets::{ProvisionPlan, Stage, stage_estimate};

/// Prefix used by the guest for machine-readable progress lines.
pub const PROGRESS_PREFIX: &str = "hegn-progress:";

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
    use crate::config::Harness;

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
}
