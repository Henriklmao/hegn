//! Measurement modes: running VMs, one project, or a throwaway project.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::{Config, Harness};
use crate::project;
use crate::vm::registry::{VmRegistry, same_project};

use super::cleanup::remove_temp_project;
use super::report::{
    BenchmarkReport, Cleanup, DiskCost, LegacyInstanceDisk, ProcessCost, SessionCost, VmDisk,
    human_bytes, print_report,
};
use super::usage::{
    Usage, cmdline_contains, matches_binary, process_alive, process_tree_usage, usage_excluding,
};
use super::{MIB, TEMP_PREFIX};

/// `session.json` written by `hegn instance start`.
///
/// The struct is deliberately permissive: instance-scoped sessions gained
/// fields over time, and `benchmark` must also read records written by older
/// versions.
#[derive(Debug, Deserialize)]
struct SessionFile {
    #[serde(default)]
    project: Option<PathBuf>,
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default, alias = "instance", alias = "sandbox")]
    vm: Option<String>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    vm_pid: u32,
    #[serde(default)]
    relay_pid: u32,
    #[serde(default)]
    virtiofs_pids: Vec<u32>,
    /// Pre-v2 single-share session.
    #[serde(default)]
    virtiofs_pid: u32,
}

/// Measure the current host cost and print or emit the report.
///
/// - no project and no `--clean`: measure every running instance; if nothing
///   runs and the terminal is interactive, offer to boot a throwaway one;
/// - `project`: boot that project's instance (or reuse it) and measure it;
/// - `clean`: create a temporary project, boot it, measure it, stop and remove
///   it again.
pub fn run(config: &Config, project: Option<&str>, clean: bool, json: bool) -> Result<i32> {
    if let Some(project) = project {
        return measure_project(config, project, json);
    }
    if clean {
        return measure_temporary(config, json);
    }
    let mut report = collect(config);
    if !report.sessions.is_empty() {
        return emit(report, json);
    }
    if !json && io::stdin().is_terminal() {
        let question = "No VM is running. Create a temporary benchmark project, boot a \
                        VM and remove both afterwards?";
        if confirm(question)? {
            return measure_temporary(config, json);
        }
    }

    report.notes.push(
        "no VM running; pass a project name to boot one, or --clean for a throwaway \
         project"
            .to_string(),
    );
    emit(report, json)
}

fn emit(report: BenchmarkReport, json: bool) -> Result<i32> {
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    Ok(0)
}

fn confirm(question: &str) -> Result<bool> {
    eprint!("{question} [y/N] ");
    io::stderr().flush().context("failed to write the prompt")?;
    let mut answer = String::new();
    if io::stdin()
        .read_line(&mut answer)
        .context("failed to read the answer")?
        == 0
    {
        return Ok(false);
    }
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Boot (or reuse) the instance of an existing project and measure it.
fn measure_project(config: &Config, project: &str, json: bool) -> Result<i32> {
    let path = crate::cli::resolve_project(config, project)?;

    // Prefer an instance that is already running for this project, base or
    // clone; otherwise create/reuse the base instance and start it.
    if let Some((session, _)) = running_sessions(config).into_iter().find(|(session, _)| {
        session
            .project
            .as_ref()
            .is_some_and(|running| same_project(running, &path))
    }) {
        // Prefer the recorded instance name; legacy records predate it.
        let mut report = match session.vm.as_deref().filter(|name| !name.is_empty()) {
            Some(name) => collect_for_vm(config, name),
            None => collect_for_project(config, &path),
        };
        report
            .notes
            .push("the VM was already running; nothing was started".to_string());
        return emit(report, json);
    }

    let harness = project::read_meta(&path).harness.unwrap_or(config.harness);
    let registry = VmRegistry::new(config.clone());
    let vm = registry.ensure_for_project(&path, harness)?;
    registry.manager(vm.clone())?.start()?;

    let mut report = collect_for_vm(config, &vm.name);
    report.started = true;
    report.notes.push(format!(
        "VM '{}' started; measured right after the VMM socket came up, so guest RSS \
         grows while the guest boots (attach with `hegn attach {}`, stop it with \
         `hegn instance stop {}`)",
        vm.name, vm.name, vm.name
    ));
    note_other_instances(config, &vm.name, &mut report.notes);
    emit(report, json)
}

/// Create a throwaway project, boot it, measure it and clean up again.
fn measure_temporary(config: &Config, json: bool) -> Result<i32> {
    let name = format!("{TEMP_PREFIX}{}", Utc::now().timestamp_millis());
    let template = config.templates_dir.join("AGENTS.md");
    let template = template.is_file().then_some(template);
    let path = project::create_project(
        &config.projects_root,
        &name,
        template.as_deref(),
        config.harness,
    )?;
    let meta = project::ProjectMeta {
        harness: Some(config.harness),
        packages: packages_for(config.harness),
        template: template.map(|path| path.display().to_string()),
        ..project::ProjectMeta::default()
    };
    // `hegn.toml` is optional; never leave the throwaway project behind
    // because its metadata could not be written.
    if let Err(err) = project::write_meta(&path, &meta) {
        eprintln!(
            "warning: could not write {}: {err}",
            path.join("hegn.toml").display()
        );
    }

    let registry = VmRegistry::new(config.clone());
    let mut vm = None;
    let started = (|| -> Result<BenchmarkReport> {
        let created = registry.ensure_for_project(&path, config.harness)?;
        // Record the VM before booting: if the start fails, the cleanup below
        // still has to unregister it.
        vm = Some(created.clone());
        registry.manager(created)?.start()?;
        let mut report = collect_for_vm(config, &vm.as_ref().unwrap().name);
        report.started = true;
        report.notes.push(
            "measured right after the VMM socket came up, so guest RSS grows while the guest \
             boots; the instance was stopped and removed after this snapshot"
                .to_string(),
        );
        note_other_instances(config, &vm.as_ref().unwrap().name, &mut report.notes);
        Ok(report)
    })();

    let stopped = vm
        .as_ref()
        .map(|vm| {
            registry
                .manager(vm.clone())
                .is_ok_and(|manager| manager.stop().is_ok())
        })
        .unwrap_or(false);
    let instance_removed = vm
        .as_ref()
        .map(|vm| registry.delete(&vm.name, true).is_ok())
        .unwrap_or(false);
    let removed = match remove_temp_project(config, &path) {
        Ok(()) => true,
        Err(err) => {
            eprintln!("warning: could not remove {}: {err}", path.display());
            false
        }
    };

    match started {
        Ok(mut report) => {
            report.cleanup = Some(Cleanup {
                project: path.clone(),
                instance: vm.as_ref().map(|vm| vm.name.clone()).unwrap_or_default(),
                instance_stopped: stopped,
                removed,
            });
            if !stopped {
                report
                    .notes
                    .push("the temporary VM could not be stopped".to_string());
            }
            if !instance_removed {
                report
                    .notes
                    .push("the temporary VM could not be removed".to_string());
            }
            if !removed {
                report.notes.push(format!(
                    "could not remove the temporary project {}; remove it manually",
                    path.display()
                ));
            }
            emit(report, json)
        }
        Err(err) => {
            if removed {
                Err(err)
            } else {
                Err(err.context(format!(
                    "the temporary project {} could not be removed; remove it manually",
                    path.display()
                )))
            }
        }
    }
}

fn packages_for(harness: Harness) -> Vec<String> {
    harness.packages().iter().map(|p| p.to_string()).collect()
}

/// Build the report for every running VM without printing anything.
pub fn collect(config: &Config) -> BenchmarkReport {
    let data_root = default_data_root();
    collect_with(config, data_root.as_deref(), |_, _| true)
}

fn default_data_root() -> Option<PathBuf> {
    dirs::data_dir().map(|root| root.join("hegn"))
}

fn collect_for_vm(config: &Config, name: &str) -> BenchmarkReport {
    let data_root = default_data_root();
    collect_with(config, data_root.as_deref(), |session, _| {
        session.vm.as_deref() == Some(name)
    })
}

fn collect_for_project(config: &Config, project: &Path) -> BenchmarkReport {
    let data_root = default_data_root();
    collect_with(config, data_root.as_deref(), |session, _| {
        session
            .project
            .as_ref()
            .is_some_and(|running| same_project(running, project))
    })
}

fn collect_with(
    config: &Config,
    data_root: Option<&Path>,
    keep: impl Fn(&SessionFile, &Path) -> bool,
) -> BenchmarkReport {
    let mut notes = Vec::new();
    let sessions = session_costs(config, &mut notes, keep);
    let (vcpus, guest_memory_bytes, process_rss_bytes) = totals(config, &sessions);
    // Snapshots are per VM; take the largest measured guest as the estimate.
    let memory_mib = sessions
        .iter()
        .map(|session| session.memory_mib)
        .max()
        .unwrap_or(config.memory_mib);
    let (snapshots, snapshot_estimate_bytes) = snapshot_estimate(config, memory_mib);

    let disk = disk_costs(config);
    let disk_allocated_bytes = disk.iter().map(|cost| cost.allocated_bytes).sum();
    let vm_disk = vm_disk_costs(config);
    let vm_disk_allocated_bytes = vm_disk.iter().map(|vm| vm.allocated_bytes).sum();
    let legacy_instances = legacy_instance_disk(config, data_root);

    if sessions.is_empty() {
        notes.push(
            "no running VM found; disk usage and the configured guest cost are reported \
             instead"
                .to_string(),
        );
    } else {
        notes.push(format!(
            "guest RAM is populated on demand: {} in total across {} VM(s)",
            human_bytes(guest_memory_bytes),
            sessions.len()
        ));
    }
    if legacy_instances.is_some() {
        notes.push(
            "legacy v1 instance directories are not active VMs and are not included in the \
             disk total; verify they are unused, then remove them manually (hegn never \
             deletes them)"
                .to_string(),
        );
    }

    BenchmarkReport {
        generated_at: Utc::now(),
        sessions,
        vcpus,
        guest_memory_bytes,
        process_rss_bytes,
        started: false,
        cleanup: None,
        snapshots,
        snapshot_estimate_bytes,
        disk,
        disk_allocated_bytes,
        vm_disk,
        vm_disk_allocated_bytes,
        legacy_instances,
        notes,
    }
}

fn totals(config: &Config, sessions: &[SessionCost]) -> (u32, u64, u64) {
    if sessions.is_empty() {
        return (
            u32::from(config.vcpus),
            u64::from(config.memory_mib) * MIB,
            0,
        );
    }
    let vcpus = sessions
        .iter()
        .map(|session| u32::from(session.vcpus))
        .sum();
    let memory = sessions
        .iter()
        .map(|session| session.guest_memory_bytes)
        .sum();
    let rss = sessions
        .iter()
        .map(|session| session.processes.iter().map(|p| p.rss_bytes).sum::<u64>())
        .sum();
    (vcpus, memory, rss)
}

/// The instances that are alive right now.
fn running_sessions(config: &Config) -> Vec<(SessionFile, PathBuf)> {
    let mut running = Vec::new();
    for path in session_candidates(config) {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<SessionFile>(&raw) else {
            continue;
        };
        if process_alive(session.vm_pid)
            && matches_binary(session.vm_pid, &config.vm_binary(), "cloud-hypervisor")
        {
            running.push((session, path));
        }
    }
    running
}

fn note_other_instances(config: &Config, measured: &str, notes: &mut Vec<String>) {
    let others = running_sessions(config)
        .into_iter()
        .filter(|(session, _)| session.vm.as_deref() != Some(measured))
        .count();
    if others > 0 {
        notes.push(format!(
            "{others} other VM(s) are running; their RAM is not part of this report"
        ));
    }
}

/// Read the costs of every session that passes `keep`.
fn session_costs(
    config: &Config,
    notes: &mut Vec<String>,
    keep: impl Fn(&SessionFile, &Path) -> bool,
) -> Vec<SessionCost> {
    let mut costs = Vec::new();
    for path in session_candidates(config) {
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let session: SessionFile = match serde_json::from_str(&raw) {
            Ok(session) => session,
            Err(err) => {
                notes.push(format!(
                    "ignoring unreadable session metadata {}: {err}",
                    path.display()
                ));
                continue;
            }
        };
        let alive = process_alive(session.vm_pid)
            && matches_binary(session.vm_pid, &config.vm_binary(), "cloud-hypervisor");
        if !alive {
            if keep(&session, &path) {
                notes.push(format!(
                    "session metadata {} records VM pid {} which is no longer running",
                    path.display(),
                    session.vm_pid
                ));
            }
            continue;
        }
        if !keep(&session, &path) {
            continue;
        }
        let (vcpus, memory_mib) = instance_limits(&path, config);
        costs.push(build_session(&session, config, vcpus, memory_mib, notes));
    }
    costs
}

/// Turn one live `session.json` into its measured host cost.
fn build_session(
    session: &SessionFile,
    config: &Config,
    vcpus: u8,
    memory_mib: u32,
    notes: &mut Vec<String>,
) -> SessionCost {
    let vm = config.vm_binary();
    let virtiofsd = &config.virtiofsd_bin;
    let mut processes = Vec::new();
    for (role, pid, binary, fallback) in [
        (
            "cloud-hypervisor",
            session.vm_pid,
            Some(&vm),
            "cloud-hypervisor",
        ),
        ("console-relay", session.relay_pid, None, "__console-relay"),
    ] {
        if pid == 0 {
            continue;
        }
        let matches = match binary {
            Some(binary) => matches_binary(pid, binary, fallback),
            None => cmdline_contains(pid, fallback),
        };
        if !matches {
            notes.push(format!(
                "pid {pid} recorded as {role} no longer belongs to that process"
            ));
            continue;
        }
        match process_tree_usage(pid) {
            Some((rss_bytes, virtual_bytes)) => processes.push(ProcessCost {
                role: role.to_string(),
                pid,
                rss_bytes,
                virtual_bytes,
            }),
            None => notes.push(format!("{role} pid {pid} is not running")),
        }
    }

    let mut member_pids = session.virtiofs_pids.clone();
    if session.virtiofs_pid != 0 {
        member_pids.push(session.virtiofs_pid);
    }
    for (index, pid) in member_pids.into_iter().enumerate() {
        if pid == 0 {
            continue;
        }
        let role = format!("virtiofsd-{index}");
        if !matches_binary(pid, virtiofsd, "virtiofsd") {
            notes.push(format!(
                "pid {pid} recorded as {role} no longer belongs to that process"
            ));
            continue;
        }
        match process_tree_usage(pid) {
            Some((rss_bytes, virtual_bytes)) => processes.push(ProcessCost {
                role,
                pid,
                rss_bytes,
                virtual_bytes,
            }),
            None => notes.push(format!("{role} pid {pid} is not running")),
        }
    }

    let uptime_secs = session
        .started_at
        .map(|started| (Utc::now() - started).num_seconds().max(0) as u64);

    SessionCost {
        instance: session
            .vm
            .clone()
            .or_else(|| {
                session
                    .project
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "default".to_string()),
        project: session.project.clone(),
        workspace: session.workspace.clone(),
        harness: session.harness.clone(),
        vcpus,
        memory_mib,
        guest_memory_bytes: u64::from(memory_mib) * MIB,
        started_at: session.started_at,
        uptime_secs,
        processes,
    }
}

/// Candidate `session.json` paths: the instance-scoped layout and the legacy
/// `state_dir/run` layout.
fn session_candidates(config: &Config) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let legacy = config.state_dir.join("run").join("session.json");
    if legacy.is_file() {
        paths.push(legacy);
    }
    if let Ok(entries) = fs::read_dir(&config.vms_dir) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("run").join("session.json");
            if candidate.is_file() {
                paths.push(candidate);
            }
        }
    }
    paths.sort();
    paths
}

/// Read per-instance limits from the sibling `hegn.toml`, if there is one.
fn instance_limits(session: &Path, config: &Config) -> (u8, u32) {
    #[derive(Deserialize, Default)]
    struct Limits {
        vcpus: Option<u8>,
        memory_mib: Option<u32>,
    }

    let config_file = session
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join(crate::vm::config::VM_CONFIG_FILE));
    let limits = config_file
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|raw| toml::from_str::<Limits>(&raw).ok())
        .unwrap_or_default();
    (
        limits.vcpus.unwrap_or(config.vcpus),
        limits.memory_mib.unwrap_or(config.memory_mib),
    )
}

/// Number of snapshots and the upper bound they would need on disk.
fn snapshot_estimate(config: &Config, memory_mib: u32) -> (u64, u64) {
    let count = fs::read_dir(&config.snapshots_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| {
                    entry.file_name() != ".staging"
                        && entry.file_type().is_ok_and(|kind| kind.is_dir())
                })
                .count() as u64
        })
        .unwrap_or(0);
    (count, count * u64::from(memory_mib) * MIB)
}

fn disk_costs(config: &Config) -> Vec<DiskCost> {
    let logs = config.logs_dir();
    let mut dirs: Vec<(String, PathBuf, Option<PathBuf>)> = Vec::new();
    if let Some(dir) = config.kernel_image.parent() {
        dirs.push(("images".to_string(), dir.to_path_buf(), None));
    }
    dirs.push(("snapshots".to_string(), config.snapshots_dir.clone(), None));
    dirs.push(("VMs".to_string(), config.vms_dir.clone(), None));
    dirs.push(("logs".to_string(), logs.clone(), None));
    // Session logs live inside the state directory; do not count them twice.
    dirs.push(("state".to_string(), config.state_dir.clone(), Some(logs)));

    dirs.into_iter()
        .map(|(name, path, exclude)| {
            let exists = path.is_dir();
            let usage = if exists {
                usage_excluding(&path, exclude.as_deref())
            } else {
                Usage::default()
            };
            DiskCost {
                name,
                path,
                exists,
                allocated_bytes: usage.allocated,
                apparent_bytes: usage.apparent,
                files: usage.files,
            }
        })
        .collect()
}

/// Disk usage of every active VM directory below `vms_dir`.
///
/// Only direct child directories are VMs. v1 instance directories elsewhere
/// are reported separately and never counted here.
fn vm_disk_costs(config: &Config) -> Vec<VmDisk> {
    let Ok(entries) = fs::read_dir(&config.vms_dir) else {
        return Vec::new();
    };
    let mut vms = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_dir() {
            continue;
        }
        let usage = usage_excluding(&path, None);
        vms.push(VmDisk {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
            allocated_bytes: usage.allocated,
            apparent_bytes: usage.apparent,
            files: usage.files,
        });
    }
    vms.sort_by(|left, right| left.name.cmp(&right.name));
    vms
}

/// Existing v1 instance directories that are not active VMs in v2.
///
/// v1 stored instances next to the snapshot root as `instances` and later
/// `sandboxes`. The default data directory is checked too, so a custom
/// `vms_dir` cannot hide the old default directories.
fn legacy_instance_dirs(config: &Config, data_root: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(parent) = config.vms_dir.parent() {
        candidates.push(parent.join("instances"));
        candidates.push(parent.join("sandboxes"));
    }
    if let Some(root) = data_root {
        candidates.push(root.join("instances"));
        candidates.push(root.join("sandboxes"));
    }
    candidates.sort();
    candidates.dedup();
    candidates.retain(|path| path.is_dir() && !path.starts_with(&config.vms_dir));
    candidates
}

/// Combined usage of the v1 instance directories, or `None` when there are
/// none. This is read-only accounting; benchmark never removes them.
fn legacy_instance_disk(config: &Config, data_root: Option<&Path>) -> Option<LegacyInstanceDisk> {
    let paths = legacy_instance_dirs(config, data_root);
    if paths.is_empty() {
        return None;
    }
    let mut total = Usage::default();
    for path in &paths {
        let usage = usage_excluding(path, None);
        total.allocated += usage.allocated;
        total.apparent += usage.apparent;
        total.files += usage.files;
    }
    Some(LegacyInstanceDisk {
        paths,
        allocated_bytes: total.allocated,
        apparent_bytes: total.apparent,
        files: total.files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_candidates_cover_the_instance_layout() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            vms_dir: dir.path().join("instances"),
            ..Config::default()
        };

        fs::create_dir_all(config.vms_dir.join("a/run")).unwrap();
        fs::write(config.vms_dir.join("a/run/session.json"), b"{}").unwrap();

        let candidates = session_candidates(&config);
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].ends_with("instances/a/run/session.json"));
    }

    #[test]
    fn collect_without_session_still_reports_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            snapshots_dir: dir.path().join("data/snapshots"),
            vms_dir: dir.path().join("data/instances"),
            kernel_image: dir.path().join("data/images/kernel"),
            vcpus: 4,
            memory_mib: 1024,
            ..Config::default()
        };

        let report = collect_with(&config, None, |_, _| true);
        assert!(report.sessions.is_empty());
        assert_eq!(report.vcpus, 4);
        assert_eq!(report.guest_memory_bytes, 1024 * MIB);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("no running VM"))
        );
        assert!(report.disk.iter().any(|cost| cost.name == "images"));
        assert!(report.disk.iter().any(|cost| cost.name == "VMs"));
        assert!(report.vm_disk.is_empty());
        assert!(report.legacy_instances.is_none());
    }

    #[test]
    fn totals_sum_all_sessions() {
        let config = Config {
            vcpus: 1,
            memory_mib: 111,
            ..Config::default()
        };
        let session = |vcpus: u8, memory_mib: u32, rss: u64| SessionCost {
            instance: "s".into(),
            project: None,
            workspace: None,
            harness: None,
            vcpus,
            memory_mib,
            guest_memory_bytes: u64::from(memory_mib) * MIB,
            started_at: None,
            uptime_secs: None,
            processes: vec![ProcessCost {
                role: "virtiofsd".into(),
                pid: 1,
                rss_bytes: rss,
                virtual_bytes: rss,
            }],
        };
        let sessions = vec![session(2, 1024, 10), session(4, 2048, 20)];
        assert_eq!(totals(&config, &sessions), (6, 3072 * MIB, 30));
        assert_eq!(totals(&config, &[]), (1, 111 * MIB, 0));
    }

    #[test]
    fn snapshot_estimate_uses_the_guest_size() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            snapshots_dir: dir.path().join("snapshots"),
            ..Config::default()
        };
        fs::create_dir_all(config.snapshots_dir.join("one")).unwrap();
        fs::create_dir_all(config.snapshots_dir.join("two")).unwrap();
        fs::create_dir_all(config.snapshots_dir.join(".staging/three")).unwrap();

        let (count, estimate) = snapshot_estimate(&config, 2048);
        assert_eq!(count, 2);
        assert_eq!(estimate, 2 * 2048 * MIB);
    }

    #[test]
    fn instance_limits_prefer_the_vm_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vcpus: 2,
            memory_mib: 2048,
            ..Config::default()
        };

        let run = dir.path().join("vm/run");
        fs::create_dir_all(&run).unwrap();
        fs::write(
            dir.path().join("vm/vm.toml"),
            b"vcpus = 8\nmemory_mib = 8192\n",
        )
        .unwrap();

        let (vcpus, memory) = instance_limits(&run.join("session.json"), &config);
        assert_eq!((vcpus, memory), (8, 8192));
    }

    #[test]
    fn instance_limits_default_without_or_with_a_broken_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vcpus: 2,
            memory_mib: 2048,
            ..Config::default()
        };
        let run = dir.path().join("vm/run");
        fs::create_dir_all(&run).unwrap();
        let session = run.join("session.json");

        assert_eq!(instance_limits(&session, &config), (2, 2048));

        fs::write(dir.path().join("vm/vm.toml"), b"not = [valid").unwrap();
        assert_eq!(instance_limits(&session, &config), (2, 2048));

        fs::write(dir.path().join("vm/vm.toml"), b"vcpus = 4\n").unwrap();
        assert_eq!(instance_limits(&session, &config), (4, 2048));
    }

    #[test]
    fn session_candidates_include_the_legacy_layout_in_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            vms_dir: dir.path().join("instances"),
            ..Config::default()
        };
        for instance in ["b", "a"] {
            fs::create_dir_all(config.vms_dir.join(instance).join("run")).unwrap();
            fs::write(
                config.vms_dir.join(instance).join("run/session.json"),
                b"{}",
            )
            .unwrap();
        }
        fs::create_dir_all(config.state_dir.join("run")).unwrap();
        fs::write(config.state_dir.join("run/session.json"), b"{}").unwrap();

        let candidates = session_candidates(&config);
        assert_eq!(candidates.len(), 3);
        let mut sorted = candidates.clone();
        sorted.sort();
        assert_eq!(candidates, sorted);
        assert!(
            candidates
                .iter()
                .any(|path| path.ends_with("instances/a/run/session.json"))
        );
    }

    #[test]
    fn session_costs_skips_corrupt_metadata_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            vms_dir: dir.path().join("instances"),
            ..Config::default()
        };
        fs::create_dir_all(config.vms_dir.join("a/run")).unwrap();
        fs::write(config.vms_dir.join("a/run/session.json"), b"{not json").unwrap();

        let mut notes = Vec::new();
        let costs = session_costs(&config, &mut notes, |_, _| true);
        assert!(costs.is_empty());
        assert!(
            notes
                .iter()
                .any(|note| note.contains("ignoring unreadable session metadata")),
            "{notes:?}"
        );
    }

    #[test]
    fn snapshot_estimate_ignores_files_and_staging() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            snapshots_dir: dir.path().join("snapshots"),
            ..Config::default()
        };
        fs::create_dir_all(config.snapshots_dir.join("one")).unwrap();
        fs::create_dir_all(config.snapshots_dir.join(".staging/two")).unwrap();
        fs::write(config.snapshots_dir.join("notes.txt"), b"x").unwrap();

        assert_eq!(snapshot_estimate(&config, 512), (1, 512 * MIB));
    }

    #[test]
    fn disk_costs_does_not_count_logs_twice() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            vms_dir: dir.path().join("instances"),
            snapshots_dir: dir.path().join("snapshots"),
            kernel_image: dir.path().join("images/kernel"),
            ..Config::default()
        };
        fs::create_dir_all(config.logs_dir()).unwrap();
        fs::write(config.logs_dir().join("session-1.log"), vec![0u8; 100]).unwrap();
        fs::write(config.state_dir.join("state.json"), vec![0u8; 10]).unwrap();

        let disk = disk_costs(&config);
        let logs = disk.iter().find(|cost| cost.name == "logs").unwrap();
        let state = disk.iter().find(|cost| cost.name == "state").unwrap();
        assert_eq!(logs.files, 1);
        assert_eq!(logs.apparent_bytes, 100);
        assert_eq!(state.files, 1, "the log inside state/ must be excluded");
        assert_eq!(state.apparent_bytes, 10);
    }

    #[test]
    fn build_session_falls_back_to_the_project_name_and_clamps_uptime() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vcpus: 2,
            memory_mib: 2048,
            ..Config::default()
        };
        let session = SessionFile {
            project: Some(dir.path().join("demo")),
            workspace: None,
            vm: None,
            harness: None,
            started_at: Some(Utc::now() + chrono::Duration::seconds(60)),
            vm_pid: 0,
            relay_pid: 0,
            virtiofs_pids: Vec::new(),
            virtiofs_pid: 0,
        };

        let mut notes = Vec::new();
        let cost = build_session(&session, &config, 4, 512, &mut notes);
        assert_eq!(cost.instance, "demo");
        assert_eq!(cost.uptime_secs, Some(0), "future timestamps clamp to zero");
        assert_eq!(cost.guest_memory_bytes, 512 * MIB);
        assert_eq!(cost.vcpus, 4);
        assert!(cost.processes.is_empty());
    }

    #[test]
    fn running_sessions_ignores_a_recycled_pid() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            vms_dir: dir.path().join("data/instances"),
            ..Config::default()
        };
        fs::create_dir_all(config.state_dir.join("run")).unwrap();
        let session = serde_json::json!({
            "project": dir.path().join("project"),
            "instance": "demo",
            "harness": "shell",
            "started_at": "2026-01-01T00:00:00Z",
            "vm_pid": std::process::id(),
            "virtiofs_pid": 0,
            "relay_pid": 0,
        });
        fs::write(
            config.state_dir.join("run/session.json"),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();

        assert!(
            running_sessions(&config).is_empty(),
            "the test process is not a Cloud Hypervisor"
        );
    }

    #[test]
    fn vm_disk_costs_reports_only_active_vm_directories() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vms_dir: dir.path().join("vms"),
            ..Config::default()
        };
        for (name, bytes) in [("beta", 200usize), ("alpha", 100)] {
            let vm = config.vms_dir.join(name);
            fs::create_dir_all(&vm).unwrap();
            fs::write(vm.join("rootfs.raw"), vec![0u8; bytes]).unwrap();
        }
        fs::write(config.vms_dir.join("vm-registry.toml"), b"not a VM").unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), vec![0u8; 4096]).unwrap();
        std::os::unix::fs::symlink(outside.path(), config.vms_dir.join("linked")).unwrap();

        let vms = vm_disk_costs(&config);
        let names = vms.iter().map(|vm| vm.name.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["alpha", "beta"], "sorted and directories only");
        assert!(vms.iter().all(|vm| vm.files == 1));
        assert!(
            vms.iter().all(|vm| vm.path.starts_with(&config.vms_dir)),
            "only active vms_dir children are VMs"
        );
    }

    #[test]
    fn legacy_instance_disk_is_separate_from_active_vms() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().join("data");
        let config = Config {
            state_dir: dir.path().join("state"),
            snapshots_dir: data_root.join("snapshots"),
            vms_dir: data_root.join("vms"),
            kernel_image: data_root.join("images/kernel"),
            ..Config::default()
        };
        let active = config.vms_dir.join("demo");
        fs::create_dir_all(&active).unwrap();
        fs::write(active.join("rootfs.raw"), vec![0u8; 1024]).unwrap();
        fs::create_dir_all(data_root.join("instances/old-a")).unwrap();
        fs::write(
            data_root.join("instances/old-a/rootfs.raw"),
            vec![0u8; 2048],
        )
        .unwrap();
        fs::create_dir_all(data_root.join("sandboxes/old-b")).unwrap();
        fs::write(
            data_root.join("sandboxes/old-b/rootfs.raw"),
            vec![0u8; 4096],
        )
        .unwrap();

        let vms = vm_disk_costs(&config);
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].name, "demo");
        assert!(
            vms.iter()
                .all(|vm| vm.name != "instances" && vm.name != "sandboxes"),
            "legacy directories are not active VMs"
        );

        let legacy = legacy_instance_disk(&config, Some(&data_root)).expect("legacy dirs");
        assert_eq!(
            legacy.paths,
            vec![data_root.join("instances"), data_root.join("sandboxes")]
        );
        assert_eq!(legacy.files, 2);
        assert!(legacy.allocated_bytes >= 2048 + 4096);
        assert!(legacy.apparent_bytes >= 2048 + 4096);

        let report = collect_with(&config, Some(&data_root), |_, _| true);
        assert_eq!(
            report.vm_disk_allocated_bytes,
            report
                .vm_disk
                .iter()
                .map(|vm| vm.allocated_bytes)
                .sum::<u64>()
        );
        assert_eq!(report.vm_disk.len(), 1);
        assert!(report.legacy_instances.is_some());
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("not active VMs")
                    && note.contains("not included in the disk total")),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn legacy_instance_disk_is_absent_without_legacy_directories() {
        let dir = tempfile::tempdir().unwrap();
        let data_root = dir.path().join("data");
        let config = Config {
            vms_dir: data_root.join("vms"),
            ..Config::default()
        };

        assert!(legacy_instance_disk(&config, Some(&data_root)).is_none());
        let report = collect_with(&config, Some(&data_root), |_, _| true);
        assert!(report.legacy_instances.is_none());
        assert!(
            !report.notes.iter().any(|note| note.contains("legacy v1")),
            "{:?}",
            report.notes
        );
    }
}
