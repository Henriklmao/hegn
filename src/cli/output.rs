//! Rendering helpers shared by the CLI handlers: VM, report and snapshot output
//! plus the small JSON/text utilities they need.

use anyhow::Result;

pub(crate) use crate::shared::text::human_bytes as human_size;

use crate::session::snapshot::SnapshotMeta;
use crate::vm::{VmReport, VmSummary, process_alive};

pub(crate) fn print_instances(instances: &[VmSummary], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(instances)?);
        return Ok(());
    }
    if instances.is_empty() {
        println!("No VMs. Create one with: hegn vm create <name>");
        return Ok(());
    }
    println!(
        "{:<24} {:<12} {:<8} {:<12} PRIMARY",
        "VM", "STATE", "MEMBERS", "HARNESS"
    );
    for vm in instances {
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

pub(crate) fn print_snapshot(meta: &SnapshotMeta, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(meta)?);
    } else {
        println!("{}", meta.name);
        println!(
            "  created {}",
            meta.created_at.format("%Y-%m-%d %H:%M:%S UTC")
        );
        println!("  state   {}", meta.vm_state);
        println!("  size    {}", meta.size_label());
        println!("  dir     {}", meta.dir.display());
        println!("  source  {}", meta.source_url());
        if let Some(instance) = &meta.instance {
            println!("  instance {instance}");
        }
        if let Some(project) = &meta.project {
            println!("  project  {}", project.display());
        }
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
}
