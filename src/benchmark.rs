//! `hegn benchmark` — measured host cost of a sandbox session.
//!
//! The report answers the questions behind `docs/resources.md`: how much RAM
//! the VMM, `virtiofsd` and the console relay actually use, how much disk the
//! images, snapshots and logs occupy, and what the configured guest would cost
//! at most.
//!
//! Three modes:
//!
//! - no argument: measure the running sandbox (read-only), or ask whether to
//!   boot a throwaway one when nothing is running;
//! - `benchmark <project>`: boot that project's sandbox and measure it;
//! - `benchmark --clean`: create a temporary project, boot it, measure it, stop
//!   it and delete the project again.
//!
//! Process numbers come from `/proc/<pid>/status`, disk numbers from
//! `st_blocks` (allocated) and `st_size` (apparent), so sparse files such as
//! the cloud-init seed and the qcow2 image are reported honestly.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Harness};
use crate::project;
use crate::sandbox::SandboxManager;

const MIB: u64 = 1024 * 1024;
/// Prefix for throwaway projects created by `benchmark --clean`.
const TEMP_PREFIX: &str = "hegn-benchmark-";

/// One host process that belongs to a running session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessCost {
    /// `cloud-hypervisor`, `virtiofsd` or `console-relay`.
    pub role: String,
    pub pid: u32,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
}

/// Disk usage of one hegn directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskCost {
    pub name: String,
    pub path: PathBuf,
    pub exists: bool,
    /// Blocks actually allocated on disk (`st_blocks`, sparse-aware).
    pub allocated_bytes: u64,
    /// Sum of the file sizes, which can be larger for sparse files.
    pub apparent_bytes: u64,
    pub files: u64,
}

/// The running session, resolved from a `session.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCost {
    pub sandbox: String,
    pub project: Option<PathBuf>,
    pub harness: Option<String>,
    pub vcpus: u8,
    pub memory_mib: u32,
    pub started_at: Option<DateTime<Utc>>,
    pub uptime_secs: Option<u64>,
    pub processes: Vec<ProcessCost>,
}

/// Machine-readable result of `hegn benchmark`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BenchmarkReport {
    pub generated_at: DateTime<Utc>,
    pub session: Option<SessionCost>,
    pub vcpus: u8,
    /// Guest RAM configured for the session (or for the next one).
    pub guest_memory_bytes: u64,
    /// Resident memory of the session processes observed right now.
    pub process_rss_bytes: u64,
    /// Whether this invocation booted a sandbox.
    pub started: bool,
    /// Present for `--clean`: what happened to the throwaway project.
    pub cleanup: Option<Cleanup>,
    pub snapshots: u64,
    /// `snapshots * memory_mib`; disk contents are not part of a snapshot.
    pub snapshot_estimate_bytes: u64,
    pub disk: Vec<DiskCost>,
    pub disk_allocated_bytes: u64,
    pub notes: Vec<String>,
}

/// Outcome of removing a `--clean` project after measuring it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cleanup {
    pub project: PathBuf,
    pub sandbox_stopped: bool,
    pub removed: bool,
}

/// `session.json` written by `hegn sandbox start`.
///
/// The struct is deliberately permissive: sandbox-scoped sessions gained
/// fields over time, and `benchmark` must also read records written by older
/// versions.
#[derive(Debug, Deserialize)]
struct SessionFile {
    #[serde(default)]
    project: Option<PathBuf>,
    #[serde(default)]
    sandbox: Option<String>,
    #[serde(default)]
    harness: Option<String>,
    #[serde(default)]
    started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    vm_pid: u32,
    #[serde(default)]
    relay_pid: u32,
    #[serde(default)]
    virtiofs_pid: u32,
}

/// Measure the current host cost and print or emit the report.
///
/// - no project and no `--clean`: measure the running sandbox; if nothing runs
///   and the terminal is interactive, offer to boot a throwaway one;
/// - `project`: boot that project (or measure it when it is already running);
/// - `clean`: create a temporary project, boot it, measure it, stop and remove
///   it again.
pub fn run(config: &Config, project: Option<&str>, clean: bool, json: bool) -> Result<i32> {
    if let Some(project) = project {
        return measure_project(config, project, json);
    }
    if clean {
        return measure_temporary(config, json);
    }
    if running_session(config).is_some() {
        return emit(collect(config), json);
    }
    if !json && io::stdin().is_terminal() {
        let question = "No sandbox is running. Create a temporary benchmark project, boot a \
                        VM and remove both afterwards?";
        if confirm(question)? {
            return measure_temporary(config, json);
        }
    }

    let mut report = collect(config);
    report.notes.push(
        "no sandbox running; pass a project name to boot one, or --clean for a throwaway \
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
    print!("{question} [y/N] ");
    io::stdout().flush().context("failed to write the prompt")?;
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

/// Boot (or reuse) the sandbox of an existing project and measure it.
fn measure_project(config: &Config, project: &str, json: bool) -> Result<i32> {
    let path = crate::cli::resolve_project(config, project)?;
    if let Some((session, session_path)) = running_session(config) {
        let matches_project = session
            .project
            .as_ref()
            .is_some_and(|running| same_path(running, &path));
        if !matches_project {
            bail!(
                "a sandbox is already running for {}; stop it with `hegn sandbox stop` first",
                session
                    .project
                    .as_ref()
                    .map(|running| running.display().to_string())
                    .unwrap_or_else(|| session_path.display().to_string())
            );
        }
        let mut report = collect(config);
        report
            .notes
            .push("the sandbox was already running; nothing was started".to_string());
        return emit(report, json);
    }

    let harness = project::read_meta(&path).harness.unwrap_or(config.harness);
    SandboxManager::new(config.clone()).start(&path, harness)?;

    let mut report = collect(config);
    report.started = true;
    report.notes.push(format!(
        "sandbox started for {}; attach with `hegn attach`, stop it with `hegn sandbox stop`",
        path.display()
    ));
    emit(report, json)
}

/// Create a throwaway project, boot it, measure it and clean up again.
fn measure_temporary(config: &Config, json: bool) -> Result<i32> {
    if let Some((session, _)) = running_session(config) {
        bail!(
            "a sandbox is already running for {}; stop it with `hegn sandbox stop` first",
            session
                .project
                .as_ref()
                .map(|running| running.display().to_string())
                .unwrap_or_else(|| "an unknown project".to_string())
        );
    }

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
    };
    project::write_meta(&path, &meta)?;

    let started = (|| -> Result<BenchmarkReport> {
        SandboxManager::new(config.clone()).start(&path, config.harness)?;
        let mut report = collect(config);
        report.started = true;
        Ok(report)
    })();

    let stopped = SandboxManager::new(config.clone()).stop().is_ok();
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
                sandbox_stopped: stopped,
                removed,
            });
            if !stopped {
                report
                    .notes
                    .push("the temporary sandbox could not be stopped".to_string());
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

/// Remove a throwaway project, refusing anything that is not one of ours.
fn remove_temp_project(config: &Config, path: &Path) -> Result<()> {
    let in_projects_root = path.parent() == Some(config.projects_root.as_path());
    let prefixed = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(TEMP_PREFIX));
    if !in_projects_root || !prefixed {
        bail!(
            "refusing to remove {}: not a benchmark project under {}",
            path.display(),
            config.projects_root.display()
        );
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.file_type().is_dir() {
        bail!("refusing to remove {}: not a directory", path.display());
    }
    fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Build the report without printing anything.
pub fn collect(config: &Config) -> BenchmarkReport {
    let mut notes = Vec::new();
    let session = session_cost(config, &mut notes);
    let vcpus = session.as_ref().map_or(config.vcpus, |cost| cost.vcpus);
    let memory_mib = session
        .as_ref()
        .map_or(config.memory_mib, |cost| cost.memory_mib);
    let guest_memory_bytes = u64::from(memory_mib) * MIB;
    let (snapshots, snapshot_estimate_bytes) = snapshot_estimate(config, memory_mib);

    let disk = disk_costs(config);
    let disk_allocated_bytes = disk.iter().map(|cost| cost.allocated_bytes).sum();
    let process_rss_bytes = session
        .as_ref()
        .map(|cost| cost.processes.iter().map(|p| p.rss_bytes).sum())
        .unwrap_or(0);

    if session.is_none() {
        notes.push(
            "no running sandbox session found; disk usage and the configured guest cost are \
             reported instead"
                .to_string(),
        );
    } else {
        notes.push(format!(
            "guest RAM is populated on demand: {vcpus} vCPU, {} configured",
            human_bytes(guest_memory_bytes)
        ));
    }

    BenchmarkReport {
        generated_at: Utc::now(),
        session,
        vcpus,
        guest_memory_bytes,
        process_rss_bytes,
        started: false,
        cleanup: None,
        snapshots,
        snapshot_estimate_bytes,
        disk,
        disk_allocated_bytes,
        notes,
    }
}

/// The session that is alive right now, if any.
fn running_session(config: &Config) -> Option<(SessionFile, PathBuf)> {
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
            return Some((session, path));
        }
    }
    None
}

/// Locate the session that is actually running and read its process costs.
fn session_cost(config: &Config, notes: &mut Vec<String>) -> Option<SessionCost> {
    let mut stale: Option<(SessionFile, PathBuf)> = None;
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
        if process_alive(session.vm_pid)
            && matches_binary(session.vm_pid, &config.vm_binary(), "cloud-hypervisor")
        {
            let (vcpus, memory_mib) = sandbox_limits(&path, config);
            return Some(build_session(&session, config, vcpus, memory_mib, notes));
        }
        stale.get_or_insert((session, path));
    }

    let (session, path) = stale?;
    notes.push(format!(
        "session metadata {} records VM pid {} which is no longer running",
        path.display(),
        session.vm_pid
    ));
    let (vcpus, memory_mib) = sandbox_limits(&path, config);
    Some(build_session(&session, config, vcpus, memory_mib, notes))
}

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
        (
            "virtiofsd",
            session.virtiofs_pid,
            Some(virtiofsd),
            "virtiofsd",
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

    let uptime_secs = session
        .started_at
        .map(|started| (Utc::now() - started).num_seconds().max(0) as u64);

    SessionCost {
        sandbox: session
            .sandbox
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
        harness: session.harness.clone(),
        vcpus,
        memory_mib,
        started_at: session.started_at,
        uptime_secs,
        processes,
    }
}

/// Candidate `session.json` paths: the sandbox-scoped layout and the legacy
/// `state_dir/run` layout.
fn session_candidates(config: &Config) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let legacy = config.state_dir.join("run").join("session.json");
    if legacy.is_file() {
        paths.push(legacy);
    }
    if let Some(root) = data_root(config)
        && let Ok(entries) = fs::read_dir(root.join("sandboxes"))
    {
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

/// `~/.local/share/hegn`, derived from `snapshots_dir` so the command also
/// works with a config written before sandboxes existed.
fn data_root(config: &Config) -> Option<PathBuf> {
    config.snapshots_dir.parent().map(Path::to_path_buf)
}

/// Read per-sandbox limits from the sibling `hegn.toml`, if there is one.
fn sandbox_limits(session: &Path, config: &Config) -> (u8, u32) {
    #[derive(Deserialize, Default)]
    struct Limits {
        vcpus: Option<u8>,
        memory_mib: Option<u32>,
    }

    let config_file = session
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join("hegn.toml"));
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
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    if let Some(dir) = config.kernel_image.parent() {
        dirs.push(("images".to_string(), dir.to_path_buf()));
    }
    dirs.push(("snapshots".to_string(), config.snapshots_dir.clone()));
    dirs.push(("logs".to_string(), config.logs_dir()));
    dirs.push(("state".to_string(), config.state_dir.clone()));
    if let Some(root) = data_root(config) {
        dirs.push(("sandboxes".to_string(), root.join("sandboxes")));
    }

    dirs.into_iter()
        .map(|(name, path)| {
            let exists = path.is_dir();
            let usage = if exists {
                usage(&path)
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Usage {
    allocated: u64,
    apparent: u64,
    files: u64,
}

/// Recursively sum allocated and apparent bytes without following symlinks.
fn usage(root: &Path) -> Usage {
    let mut total = Usage::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let kind = metadata.file_type();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if let Ok(entries) = fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if kind.is_file() {
            total.files += 1;
            total.apparent += metadata.len();
            total.allocated += allocated_bytes(&metadata);
        }
    }
    total
}

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks() * 512
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

/// `(VmRSS, VmSize)` of a process in bytes, or `None` if it is not running.
fn proc_usage(pid: u32) -> Option<(u64, u64)> {
    let raw = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut rss = None;
    let mut virt = None;
    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            rss = parse_kib(value);
        } else if let Some(value) = line.strip_prefix("VmSize:") {
            virt = parse_kib(value);
        }
    }
    Some((rss?, virt?))
}

/// RSS and virtual size of a process plus all of its descendants.
///
/// `virtiofsd` re-executes itself into a sandboxed worker, so the pid recorded
/// in `session.json` is only the parent half of the file server.
fn process_tree_usage(root: u32) -> Option<(u64, u64)> {
    let mut parents = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            if let Some(parent) = read_ppid(pid) {
                parents.push((pid, parent));
            }
        }
    }

    let mut total = (0u64, 0u64);
    let mut found = false;
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if let Some((rss, virt)) = proc_usage(pid) {
            total.0 += rss;
            total.1 += virt;
            found = true;
        }
        stack.extend(
            parents
                .iter()
                .filter(|(_, parent)| *parent == pid)
                .map(|(child, _)| *child),
        );
    }
    found.then_some(total)
}

/// Parent pid from `/proc/<pid>/stat`; `comm` may contain spaces and `)`.
fn read_ppid(pid: u32) -> Option<u32> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &raw[raw.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn parse_kib(value: &str) -> Option<u64> {
    let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
    Some(kib * 1024)
}

fn process_alive(pid: u32) -> bool {
    pid != 0 && Path::new("/proc").join(pid.to_string()).exists()
}

/// Whether the process command line contains `needle`.
///
/// Session metadata can outlive a reboot, and a recycled pid must not be
/// reported as the VMM, the file server or the relay.
fn cmdline_contains(pid: u32, needle: &str) -> bool {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            String::from_utf8_lossy(&raw)
                .replace('\0', " ")
                .contains(needle)
        })
        .unwrap_or(false)
}

/// Whether the process runs `binary`, or at least a binary with its file name.
///
/// The configured path may be renamed (`cloud_hypervisor_bin = "/opt/ch"`), so
/// both the configured name and the conventional one are accepted.
fn matches_binary(pid: u32, binary: &Path, fallback: &str) -> bool {
    let configured = binary
        .file_name()
        .map(|name| name.to_string_lossy())
        .filter(|name| !name.is_empty())
        .is_some_and(|name| cmdline_contains(pid, &name));
    configured || cmdline_contains(pid, fallback)
}

/// Render bytes with binary units and one decimal place.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn human_uptime(secs: u64) -> String {
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m {}s", secs % 60)
    }
}

fn print_report(report: &BenchmarkReport) {
    println!("hegn benchmark\n");

    match &report.session {
        Some(session) => {
            let harness = session.harness.as_deref().unwrap_or("unknown harness");
            println!("Session   {} ({harness})", session.sandbox);
            if let Some(project) = &session.project {
                println!("Project   {}", project.display());
            }
            if let Some(uptime) = session.uptime_secs {
                println!("Uptime    {}", human_uptime(uptime));
            }
            println!(
                "Guest     {} vCPU, {} configured (populated on demand)",
                session.vcpus,
                human_bytes(report.guest_memory_bytes)
            );
            println!();
            println!(
                "{:<18} {:>7} {:>10} {:>10}",
                "PROCESS", "PID", "RSS", "VIRTUAL"
            );
            for process in &session.processes {
                println!(
                    "{:<18} {:>7} {:>10} {:>10}",
                    process.role,
                    process.pid,
                    human_bytes(process.rss_bytes),
                    human_bytes(process.virtual_bytes)
                );
            }
            println!(
                "{:<18} {:>7} {:>10} {:>10}",
                "host total",
                "",
                human_bytes(report.process_rss_bytes),
                ""
            );
        }
        None => {
            println!("Session   none running");
            println!(
                "Guest     {} at most ({} vCPU configured)",
                human_bytes(report.guest_memory_bytes),
                report.vcpus
            );
        }
    }

    if report.started {
        println!("\nStarted   a sandbox for this measurement");
    }
    if let Some(cleanup) = &report.cleanup {
        println!(
            "\nCleanup   {} (sandbox stopped: {}, project removed: {})",
            cleanup.project.display(),
            cleanup.sandbox_stopped,
            cleanup.removed
        );
    }

    println!(
        "\n{:<12} {:>11} {:>11} {:>7}  PATH",
        "DISK", "ALLOCATED", "APPARENT", "FILES"
    );
    for cost in &report.disk {
        if !cost.exists {
            println!(
                "{:<12} {:>11} {:>11} {:>7}  {}",
                cost.name,
                "-",
                "-",
                "-",
                cost.path.display()
            );
            continue;
        }
        println!(
            "{:<12} {:>11} {:>11} {:>7}  {}",
            cost.name,
            human_bytes(cost.allocated_bytes),
            human_bytes(cost.apparent_bytes),
            cost.files,
            cost.path.display()
        );
    }
    println!(
        "{:<12} {:>11}",
        "disk total",
        human_bytes(report.disk_allocated_bytes)
    );

    if report.snapshots > 0 {
        println!(
            "\nSnapshots  {} (up to {} if every guest page is touched)",
            report.snapshots,
            human_bytes(report.snapshot_estimate_bytes)
        );
    }

    if !report.notes.is_empty() {
        println!("\nNotes:");
        for note in &report.notes {
            println!("- {note}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(2048 + 512), "2.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * MIB), "3.0 GiB");
    }

    #[test]
    fn human_uptime_stays_readable() {
        assert_eq!(human_uptime(42), "0m 42s");
        assert_eq!(human_uptime(90), "1m 30s");
        assert_eq!(human_uptime(3 * 3600 + 5 * 60), "3h 5m");
    }

    #[test]
    fn proc_usage_reports_the_current_process() {
        let (rss, virt) = proc_usage(std::process::id()).expect("own /proc entry");
        assert!(rss > 0);
        assert!(virt >= rss);
        assert_eq!(proc_usage(u32::MAX - 1), None);

        let (tree_rss, tree_virt) =
            process_tree_usage(std::process::id()).expect("own process tree");
        // RSS can change between the two reads, so only assert that the tree
        // includes a live root; comparing sums across time would be flaky.
        assert!(tree_rss > 0);
        assert!(tree_virt > 0);
    }

    #[test]
    fn read_ppid_matches_the_os_parent() {
        let pid = std::process::id();
        assert_eq!(read_ppid(pid), Some(std::os::unix::process::parent_id()));
    }

    #[test]
    fn cmdline_contains_matches_the_test_binary() {
        let pid = std::process::id();
        let exe = std::env::current_exe().expect("current exe");
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(cmdline_contains(pid, &name));
        assert!(!cmdline_contains(pid, "definitely-not-on-this-cmdline"));
        assert!(!cmdline_contains(u32::MAX - 1, "hegn"));
    }

    #[test]
    fn matches_binary_accepts_the_configured_path() {
        let pid = std::process::id();
        let exe = std::env::current_exe().expect("current exe");
        assert!(matches_binary(pid, &exe, "no-such-fallback"));
        assert!(!matches_binary(
            pid,
            Path::new("/opt/not-running"),
            "no-such-fallback"
        ));
    }

    #[test]
    fn usage_counts_files_without_following_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), vec![0u8; 4096]).unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/other"), b"abc").unwrap();

        let first = usage(dir.path());
        assert_eq!(first.files, 2);
        assert_eq!(first.apparent, 4096 + 3);
        assert!(first.allocated > 0);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("file"), dir.path().join("link")).unwrap();
            let linked = usage(dir.path());
            assert_eq!(linked.files, 2, "symlinks are not followed or counted");
        }
    }

    #[test]
    fn session_candidates_cover_both_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            snapshots_dir: dir.path().join("data/snapshots"),
            ..Config::default()
        };

        fs::create_dir_all(config.state_dir.join("run")).unwrap();
        fs::write(config.state_dir.join("run/session.json"), b"{}").unwrap();
        fs::create_dir_all(
            config
                .snapshots_dir
                .parent()
                .unwrap()
                .join("sandboxes/a/run"),
        )
        .unwrap();
        fs::write(
            config
                .snapshots_dir
                .parent()
                .unwrap()
                .join("sandboxes/a/run/session.json"),
            b"{}",
        )
        .unwrap();

        let candidates = session_candidates(&config);
        assert_eq!(candidates.len(), 2);
        assert!(
            candidates
                .iter()
                .any(|path| path.ends_with("state/run/session.json"))
        );
        assert!(
            candidates
                .iter()
                .any(|path| path.ends_with("sandboxes/a/run/session.json"))
        );
    }

    #[test]
    fn collect_without_session_still_reports_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            snapshots_dir: dir.path().join("data/snapshots"),
            kernel_image: dir.path().join("data/images/kernel"),
            vcpus: 4,
            memory_mib: 1024,
            ..Config::default()
        };

        let report = collect(&config);
        assert!(report.session.is_none());
        assert_eq!(report.vcpus, 4);
        assert_eq!(report.guest_memory_bytes, 1024 * MIB);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("no running sandbox"))
        );
        assert!(report.disk.iter().any(|cost| cost.name == "images"));
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
    fn sandbox_limits_prefer_the_sandbox_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            vcpus: 2,
            memory_mib: 2048,
            ..Config::default()
        };

        let run = dir.path().join("sandbox/run");
        fs::create_dir_all(&run).unwrap();
        fs::write(
            dir.path().join("sandbox/hegn.toml"),
            b"vcpus = 8\nmemory_mib = 8192\n",
        )
        .unwrap();

        let (vcpus, memory) = sandbox_limits(&run.join("session.json"), &config);
        assert_eq!((vcpus, memory), (8, 8192));
    }

    #[test]
    fn report_serialises_to_json() {
        let report = BenchmarkReport {
            generated_at: Utc::now(),
            session: None,
            vcpus: 2,
            guest_memory_bytes: 2048 * MIB,
            process_rss_bytes: 0,
            started: false,
            cleanup: None,
            snapshots: 0,
            snapshot_estimate_bytes: 0,
            disk: Vec::new(),
            disk_allocated_bytes: 0,
            notes: Vec::new(),
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["guest_memory_bytes"], 2048 * MIB);
        assert_eq!(json["started"], false);
        assert!(json["cleanup"].is_null());
        assert!(json["session"].is_null());
    }

    #[test]
    fn remove_temp_project_only_deletes_its_own_projects() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            projects_root: dir.path().join("projects"),
            ..Config::default()
        };
        let temp = config.projects_root.join("hegn-benchmark-1");
        fs::create_dir_all(&temp).unwrap();
        fs::write(temp.join("README.md"), b"x").unwrap();

        remove_temp_project(&config, &temp).unwrap();
        assert!(!temp.exists());

        let foreign = config.projects_root.join("my-project");
        fs::create_dir_all(&foreign).unwrap();
        let err = remove_temp_project(&config, &foreign).unwrap_err();
        assert!(err.to_string().contains("refusing to remove"), "{err}");
        assert!(foreign.exists());

        let outside = dir.path().join("hegn-benchmark-outside");
        fs::create_dir_all(&outside).unwrap();
        let err = remove_temp_project(&config, &outside).unwrap_err();
        assert!(err.to_string().contains("refusing to remove"), "{err}");
        assert!(outside.exists());
    }

    #[test]
    fn running_session_ignores_a_recycled_pid() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            state_dir: dir.path().join("state"),
            snapshots_dir: dir.path().join("data/snapshots"),
            ..Config::default()
        };
        fs::create_dir_all(config.state_dir.join("run")).unwrap();
        let session = serde_json::json!({
            "project": dir.path().join("project"),
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
            running_session(&config).is_none(),
            "the test process is not a Cloud Hypervisor"
        );
    }
}
