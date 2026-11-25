//! Rendering helpers shared by the CLI handlers: VM, report and snapshot output
//! plus the small JSON/text utilities they need.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

pub(crate) use crate::shared::text::human_bytes as human_size;

use crate::config::Config;
use crate::project::{Project, ProjectRegistry, filter_projects};
use crate::session::snapshot::SnapshotMeta;
use crate::session::state::StateStore;
use crate::vm::config::VmStore;
use crate::vm::{VmReport, VmSummary, process_alive};

/// One project-list row with its VM badge.
///
/// The original [`Project`] fields are flattened, so the JSON shape only gains
/// `vm` and `primary` and existing consumers keep working.
#[derive(Debug, Serialize)]
pub(crate) struct ProjectRow {
    #[serde(flatten)]
    pub project: Project,
    pub vm: Option<String>,
    pub primary: bool,
}

/// Union discovered projects with registry-only External paths, then attach VM
/// membership and the primary marker.
///
/// The registry is authoritative for assigned projects that live outside
/// `projects_root`, so discovery alone is not enough. Rows are deduplicated by
/// canonical path, hidden entries are skipped, and the existing fuzzy
/// filter/rank is applied to the union.
pub(crate) fn project_rows(
    config: &Config,
    projects: &[Project],
    query: &str,
) -> Result<Vec<ProjectRow>> {
    let registry = ProjectRegistry::load(&config.state_dir)?;
    let state = StateStore::load(config.state_dir.join("state.json"));
    let store = VmStore::new(config);
    let mut primaries: HashMap<String, Option<String>> = HashMap::new();
    for vm in store.list()? {
        let primary = vm.primary.clone().or_else(|| {
            registry
                .exported_members(&vm.name)
                .first()
                .map(|entry| entry.name.clone())
        });
        primaries.insert(vm.name, primary);
    }

    let mut union = projects.to_vec();
    let mut seen: HashSet<PathBuf> = projects
        .iter()
        .map(|project| {
            project
                .path
                .canonicalize()
                .unwrap_or_else(|_| project.path.clone())
        })
        .collect();
    for entry in registry.entries() {
        let picker = config.projects_root.join(&entry.name);
        if entry.hidden || state.is_hidden(&entry.path) || state.is_hidden(&picker) {
            continue;
        }
        let canonical = entry
            .path
            .canonicalize()
            .unwrap_or_else(|_| entry.path.clone());
        if !seen.insert(canonical) {
            continue;
        }
        // `Project::load` keeps a missing path: its git summary carries an
        // error and the row still tells the user the project is registered.
        let mut project = Project::load(entry.path.clone());
        project.name = entry.name.clone();
        union.push(project);
    }

    let indices = filter_projects(&union, query);
    Ok(indices
        .into_iter()
        .map(|index| {
            let project = &union[index];
            let entry = project
                .path
                .canonicalize()
                .ok()
                .and_then(|canonical| registry.find_by_path(&canonical));
            let vm = entry.and_then(|entry| entry.vm.clone());
            let primary = match (&vm, entry) {
                (Some(vm), Some(entry)) => primaries
                    .get(vm)
                    .is_some_and(|primary| primary.as_deref() == Some(entry.name.as_str())),
                _ => false,
            };
            ProjectRow {
                project: project.clone(),
                vm,
                primary,
            }
        })
        .collect())
}

pub(crate) fn print_projects(rows: &[ProjectRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
    } else {
        println!(
            "{:<28} {:<16} {:<7} {:<20} PATH",
            "NAME", "VM", "PRIMARY", "GIT"
        );
        for row in rows {
            println!(
                "{:<28} {:<16} {:<7} {:<20} {}",
                row.project.name,
                row.vm.as_deref().unwrap_or("-"),
                if row.primary { "*" } else { "" },
                row.project.git.badge(),
                row.project.display_path()
            );
        }
    }
    Ok(())
}

pub(crate) fn print_vms(vms: &[VmSummary], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(vms)?);
        return Ok(());
    }
    if vms.is_empty() {
        println!("No VMs. Create one with: hegn vm create <name>");
        return Ok(());
    }
    println!(
        "{:<24} {:<12} {:<8} {:<12} PRIMARY",
        "VM", "STATE", "MEMBERS", "HARNESS"
    );
    for vm in vms {
        println!(
            "{:<24} {:<12} {:<8} {:<12} {}",
            vm.name,
            vm.state,
            vm.members,
            vm.harness.label(),
            vm.project.display()
        );
    }
    Ok(())
}

pub(crate) fn print_report(report: &VmReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!("vm           {}", report.vm);
    println!("state        {}", report.state);
    println!("backend      {}", report.backend);
    println!("members      {}", report.members);
    if let Some(primary) = &report.primary {
        println!("primary      {primary}");
    }
    if let Some(project) = &report.project {
        println!("project      {}", project.display());
    }
    println!(
        "resources    {} vCPU · {} MiB",
        report.vcpus, report.memory_mib
    );
    println!("api socket   {}", report.api_socket.display());
    println!("serial       {}", report.serial_socket.display());
    println!("console      {}", report.console_socket.display());
    println!(
        "virtiofs     {} ({} member daemon(s))",
        if report.virtiofs_running {
            "running"
        } else {
            "stopped"
        },
        report.virtiofs_pids.len()
    );
    if let Some(pid) = report.vm_pid {
        println!(
            "vm           pid {pid} ({})",
            if process_alive(pid) { "alive" } else { "dead" }
        );
    }
    if let Some(pid) = report.relay_pid {
        println!(
            "relay        pid {pid} ({})",
            if process_alive(pid) { "alive" } else { "dead" }
        );
    }
    for pid in &report.virtiofs_pids {
        println!(
            "virtiofsd    pid {pid} ({})",
            if process_alive(*pid) { "alive" } else { "dead" }
        );
    }
    if let Some(log) = &report.serial_log {
        println!("serial log   {}", log.display());
    }
    for endpoint in &report.local_endpoints {
        println!("endpoint     {endpoint}");
    }
    if let Some(note) = &report.note {
        println!("note         {note}");
    }
    Ok(())
}

/// Render snapshot metadata as the plain-text block printed by `show` and
/// `create`. Kept separate so the VM label can be asserted without capturing
/// stdout.
pub(crate) fn snapshot_text(meta: &SnapshotMeta) -> String {
    use std::fmt::Write as _;

    let mut text = String::new();
    let _ = writeln!(text, "{}", meta.name);
    let _ = writeln!(
        text,
        "  created {}",
        meta.created_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    let _ = writeln!(text, "  state   {}", meta.vm_state);
    let _ = writeln!(text, "  size    {}", meta.size_label());
    let _ = writeln!(text, "  dir     {}", meta.dir.display());
    let _ = writeln!(text, "  source  {}", meta.source_url());
    if let Some(vm) = meta.vm() {
        let _ = writeln!(text, "  VM      {vm}");
    }
    if let Some(project) = &meta.project {
        let _ = writeln!(text, "  project  {}", project.display());
    }
    text
}

pub(crate) fn print_snapshot(meta: &SnapshotMeta, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(meta)?);
    } else {
        print!("{}", snapshot_text(meta));
    }
    Ok(())
}

pub(crate) fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_size_keeps_bytes_below_the_first_boundary() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(1023), "1023 B");
    }

    #[test]
    fn human_size_switches_units_at_each_binary_boundary() {
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn human_size_uses_tebibytes_for_larger_values() {
        assert_eq!(human_size(1024_u64.pow(4)), "1.0 TiB");
        assert_eq!(human_size(u64::MAX), "16777216.0 TiB");
    }

    #[test]
    fn snapshot_text_labels_the_vm_and_drops_the_legacy_instance_label() {
        let meta = SnapshotMeta {
            name: "demo".into(),
            vm: Some("rust".into()),
            ..SnapshotMeta::default()
        };
        let text = snapshot_text(&meta);
        assert!(text.contains("  VM      rust"), "{text}");
        assert!(
            !text.to_lowercase().contains("instance"),
            "the transitional label must not leak: {text}"
        );

        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["vm"], "rust");
        assert!(
            json.get("instance").is_none(),
            "JSON already emits vm, not instance"
        );
    }

    #[test]
    fn project_rows_add_vm_and_primary_without_dropping_project_fields() {
        use crate::cli::test_support::config;
        use crate::config::Harness;
        use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
        use crate::vm::config::VmConfig;
        use chrono::Utc;

        let (_dir, config) = config();
        let mut vm = VmConfig::from_config(&config, "rust", Some(Harness::Shell));
        vm.primary = Some("parser".into());
        VmStore::new(&config).save(&vm).unwrap();

        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        let mut projects = Vec::new();
        for (name, assigned) in [("parser", true), ("cli", true), ("free", false)] {
            let path = config.projects_root.join(name);
            std::fs::create_dir_all(&path).unwrap();
            let canonical = path.canonicalize().unwrap();
            registry
                .upsert(ProjectEntry {
                    name: name.into(),
                    path: canonical.clone(),
                    vm: None,
                    read_only: false,
                    kind: ProjectKind::External,
                    parent: None,
                    managed: false,
                    hidden: false,
                    created_at: Utc::now(),
                })
                .unwrap();
            if assigned {
                registry.assign(name, "rust", false).unwrap();
            }
            projects.push(Project::load(canonical));
        }

        let rows = project_rows(&config, &projects, "").unwrap();
        assert_eq!(rows.len(), 3);
        let parser = rows
            .iter()
            .find(|row| row.project.name == "parser")
            .unwrap();
        assert_eq!(parser.vm.as_deref(), Some("rust"));
        assert!(parser.primary);
        let cli = rows.iter().find(|row| row.project.name == "cli").unwrap();
        assert_eq!(cli.vm.as_deref(), Some("rust"));
        assert!(!cli.primary);
        let free = rows.iter().find(|row| row.project.name == "free").unwrap();
        assert!(free.vm.is_none());
        assert!(!free.primary);

        let value = serde_json::to_value(parser).unwrap();
        assert_eq!(value["name"], "parser");
        assert_eq!(value["vm"], "rust");
        assert_eq!(value["primary"], true);
        assert!(
            value.get("git").is_some(),
            "flattened project fields survive"
        );
    }

    #[test]
    fn project_rows_include_registry_only_external_projects_and_skip_hidden() {
        use crate::cli::test_support::config;
        use crate::config::Harness;
        use crate::project::{ProjectEntry, ProjectKind, ProjectRegistry};
        use crate::vm::config::VmConfig;
        use chrono::Utc;

        let (dir, config) = config();
        let mut vm = VmConfig::from_config(&config, "rust", Some(Harness::Shell));
        vm.primary = Some("outside".into());
        VmStore::new(&config).save(&vm).unwrap();

        let outside = dir.path().join("code/outside");
        std::fs::create_dir_all(&outside).unwrap();
        let hidden = dir.path().join("code/hidden");
        std::fs::create_dir_all(&hidden).unwrap();
        let missing = dir.path().join("code/missing");
        std::fs::create_dir_all(&missing).unwrap();
        let mut registry = ProjectRegistry::load(&config.state_dir).unwrap();
        for (name, path, hidden, assigned) in [
            ("outside", &outside, false, true),
            ("hidden", &hidden, true, false),
            ("missing", &missing, false, false),
        ] {
            registry
                .upsert(ProjectEntry {
                    name: name.into(),
                    path: path.to_path_buf(),
                    vm: None,
                    read_only: false,
                    kind: ProjectKind::External,
                    parent: None,
                    managed: false,
                    hidden,
                    created_at: Utc::now(),
                })
                .unwrap();
            if assigned {
                registry.assign(name, "rust", true).unwrap();
            }
        }
        std::fs::remove_dir_all(&missing).unwrap();

        let rows = project_rows(&config, &[], "outside").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project.name, "outside");
        assert_eq!(rows[0].vm.as_deref(), Some("rust"));
        assert!(rows[0].primary);

        let all = project_rows(&config, &[], "").unwrap();
        let names: Vec<&str> = all.iter().map(|row| row.project.name.as_str()).collect();
        assert!(!names.contains(&"hidden"), "{names:?}");
        let missing = all
            .iter()
            .find(|row| row.project.name == "missing")
            .expect("a missing registered path still gets a row");
        assert_eq!(
            missing.project.git.error.as_deref(),
            Some("not a directory")
        );
        assert!(missing.vm.is_none());
    }
}
