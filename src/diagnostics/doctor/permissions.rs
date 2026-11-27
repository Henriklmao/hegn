//! Permission audit and repair for hegn's private state.
//!
//! A `sudo hegn` run (network setup, some doctor fixes) writes into the same
//! XDG trees as the unprivileged user. The next user run owns the parent
//! directories but cannot read or replace root-owned entries, which used to
//! surface as a bare `Permission denied (os error 13)`. The audit below finds
//! those entries and the repair hands them back, either directly when running
//! as root through `sudo` or through one `chown -R` escalation.

use std::collections::HashSet;
use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::shared::permissions::{Owner, adopt_owner, invoking_owner};
use crate::shared::text::shell_quote_always;

/// Stop after this many entries so a pathological tree cannot stall the TUI.
const MAX_ENTRIES: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProblemKind {
    /// The entry belongs to another user; only `chown` can fix it.
    ForeignOwner { uid: u32 },
    /// The current owner lacks read/write (or execute on a directory) access.
    Inaccessible,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Problem {
    pub path: PathBuf,
    pub kind: ProblemKind,
}

/// Result of a read-only ownership and access walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Audit {
    pub problems: Vec<Problem>,
    /// The walk hit [`MAX_ENTRIES`]; later entries were not inspected.
    pub truncated: bool,
}

impl Audit {
    pub(super) fn ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// One-line detail for the doctor check.
    pub(super) fn detail(&self) -> String {
        if self.problems.is_empty() {
            return if self.truncated {
                format!("no problems in the first {MAX_ENTRIES} entries")
            } else {
                "all private state is accessible".to_string()
            };
        }
        let mut detail = format!(
            "{} unreadable or root-owned entr{}: ",
            self.problems.len(),
            if self.problems.len() == 1 { "y" } else { "ies" }
        );
        for (index, problem) in self.problems.iter().take(3).enumerate() {
            if index > 0 {
                detail.push_str(", ");
            }
            detail.push_str(&problem.path.display().to_string());
        }
        if self.problems.len() > 3 {
            detail.push_str(", …");
        }
        detail.push_str("; run `hegn doctor --fix`");
        detail
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairReport {
    /// Entries whose owner access bits were widened.
    pub chmodded: usize,
    /// Entries (or whole trees) handed to the invoking user.
    pub chowned: usize,
    /// Paths that still need attention after a failed repair.
    pub failed: Vec<String>,
    /// The audit hit its entry cap and did not inspect the whole tree.
    pub truncated: bool,
}

impl RepairReport {
    pub fn any(&self) -> bool {
        self.chmodded + self.chowned > 0
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.chowned > 0 {
            parts.push(format!("handed {} entr(ies) back", self.chowned));
        }
        if self.chmodded > 0 {
            parts.push(format!("widened {} mode(s)", self.chmodded));
        }
        if !self.failed.is_empty() {
            parts.push(format!(
                "{} still need attention: {}",
                self.failed.len(),
                self.failed
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if self.truncated {
            parts.push(format!(
                "audit stopped after {MAX_ENTRIES} entries; run the fix again"
            ));
        }
        if parts.is_empty() {
            "no permission problems found".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// Audit every configured private tree without following symlinks.
pub(super) fn audit(config: &Config) -> Audit {
    let owner = invoking_owner();
    // A plain root run owns everything it writes; only `sudo` (SUDO_UID set) or
    // an unprivileged run has a meaningful "foreign" owner to compare against.
    let check_owner = owner.uid != 0;

    let mut audit = Audit::default();
    let mut seen = HashSet::new();
    let mut visited = 0usize;
    let mut audited_roots: Vec<PathBuf> = Vec::new();
    let mut roots = roots(config);
    // Audit the shallowest roots first so a nested root is not walked twice.
    roots.sort_by_key(|path| path.components().count());

    'roots: for root in roots {
        if audited_roots.iter().any(|parent| root.starts_with(parent)) {
            continue;
        }
        let Ok(root_metadata) = fs::symlink_metadata(&root) else {
            continue;
        };
        if root_metadata.file_type().is_symlink() {
            continue;
        }
        audited_roots.push(root.clone());
        let read_only = is_read_only_asset(config, &root);
        let mut pending = vec![root];
        while let Some(path) = pending.pop() {
            if visited >= MAX_ENTRIES {
                audit.truncated = true;
                break 'roots;
            }
            visited += 1;
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(err) if is_permission_denied(&err) => {
                    push_problem(&mut audit, &mut seen, path, ProblemKind::Inaccessible);
                    continue;
                }
                Err(_) => continue,
            };
            // Never follow a planted symlink, and never chown through one.
            if metadata.file_type().is_symlink() {
                continue;
            }
            // A foreign entry is a problem even when root can read it: the next
            // unprivileged run cannot. For a plain root run `check_owner` is
            // false, so root-owned state is never chowned away from root.
            // Read-only assets (kernel/rootfs) may legitimately be root-owned and
            // world-readable; only flag them when they are actually unusable.
            // Compare the uid only: group-only mismatches do not block hegn and
            // must not chown a shared tree.
            let foreign = check_owner && metadata.uid() != owner.uid;
            let usable = has_required_access(&path, &metadata, read_only);
            if !usable || (foreign && !read_only) {
                let kind = if foreign {
                    ProblemKind::ForeignOwner {
                        uid: metadata.uid(),
                    }
                } else {
                    ProblemKind::Inaccessible
                };
                push_problem(&mut audit, &mut seen, path.clone(), kind);
            }
            if metadata.is_dir() {
                match fs::read_dir(&path) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            pending.push(entry.path());
                        }
                    }
                    Err(err) if is_permission_denied(&err) => {
                        push_problem(&mut audit, &mut seen, path, ProblemKind::Inaccessible);
                    }
                    Err(_) => {}
                }
            }
        }
    }
    audit
}

fn roots(config: &Config) -> Vec<PathBuf> {
    let mut roots = vec![
        config.state_dir.clone(),
        config.snapshots_dir.clone(),
        config.vms_dir.clone(),
        config.templates_dir.clone(),
        config.profiles_dir.clone(),
        config.kernel_image.clone(),
        config.rootfs_image.clone(),
    ];
    // `profiles_dir` lives below `~/.config/hegn`; auditing its parent also
    // covers `config.toml` without consulting the process-wide config path.
    if let Some(parent) = config.profiles_dir.parent() {
        roots.push(parent.to_path_buf());
    }
    // Audit the shared `.../hegn` data root as well: if that directory itself
    // is root-owned, every configured child is unreachable and its own lstat
    // fails with EACCES.
    for dir in [&config.vms_dir, &config.snapshots_dir] {
        if let Some(parent) = dir.parent()
            && parent.file_name().is_some_and(|name| name == "hegn")
        {
            roots.push(parent.to_path_buf());
        }
    }
    roots
}

fn is_read_only_asset(config: &Config, path: &Path) -> bool {
    path == config.kernel_image || path == config.rootfs_image
}

fn has_required_access(path: &Path, metadata: &fs::Metadata, read_only: bool) -> bool {
    // Root can always pass; the ownership check above still flags leftovers.
    if unsafe { libc::geteuid() } == 0 {
        return true;
    }
    let mut mode = libc::R_OK;
    if metadata.is_dir() {
        mode |= libc::W_OK | libc::X_OK;
    } else if !(read_only && metadata.is_file()) {
        mode |= libc::W_OK;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // Safety: `path` is a valid NUL-terminated path and `mode` is a valid
    // access mask; access(2) only reads the process credentials.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

fn push_problem(audit: &mut Audit, seen: &mut HashSet<PathBuf>, path: PathBuf, kind: ProblemKind) {
    if seen.insert(path.clone()) {
        audit.problems.push(Problem { path, kind });
    }
}

/// Repair what [`audit`] found.
///
/// Same-owner entries are widened in place. Foreign trees are handed to the
/// invoking user: directly when this process is root, otherwise through the
/// same fixed-path `sudo`/`pkexec` escalation used for network setup.
pub fn repair(config: &Config) -> Result<RepairReport> {
    let audit = audit(config);
    if audit.problems.is_empty() {
        return Ok(RepairReport::default());
    }
    let owner = invoking_owner();
    let mut report = RepairReport {
        truncated: audit.truncated,
        ..RepairReport::default()
    };
    let mut foreign = Vec::new();

    for problem in &audit.problems {
        if !repairable(&problem.path, owner) {
            report.failed.push(format!(
                "{}: outside hegn's user-controlled state; fix it manually",
                problem.path.display()
            ));
            continue;
        }
        if is_read_only_asset(config, &problem.path)
            && matches!(problem.kind, ProblemKind::ForeignOwner { .. })
        {
            report.failed.push(format!(
                "{}: root-owned VM image; make it readable or point kernel_image/rootfs_image at \
                 a user-readable copy",
                problem.path.display()
            ));
            continue;
        }
        match problem.kind {
            ProblemKind::Inaccessible => match widen_owner_access(&problem.path) {
                Ok(()) => report.chmodded += 1,
                Err(err) => report
                    .failed
                    .push(format!("{}: {err}", problem.path.display())),
            },
            // Only a root leftover is safe to hand over automatically. Another
            // user's entries in a shared tree must never be taken over.
            ProblemKind::ForeignOwner { uid: 0 } => foreign.push(problem.path.clone()),
            ProblemKind::ForeignOwner { uid } => report.failed.push(format!(
                "{}: owned by uid {uid}; fix it manually",
                problem.path.display()
            )),
        }
    }

    let roots = topmost(
        foreign
            .into_iter()
            .map(|path| canonical_entry(&path))
            .collect::<Result<Vec<_>>>()?,
    );
    if !roots.is_empty() {
        // Re-check the resolved ancestry immediately before the privileged call
        // so a directory swapped after the audit cannot redirect the chown.
        for root in &roots {
            if !repairable(root, owner) {
                bail!(
                    "refusing to change ownership of {}: it is no longer inside the \
                     user-controlled hegn state",
                    root.display()
                );
            }
        }
        if unsafe { libc::geteuid() } == 0 {
            for root in &roots {
                adopt_owner(root, owner).with_context(|| {
                    format!("failed to hand {} to user {}", root.display(), owner.uid)
                })?;
            }
        } else {
            escalate_chown(&roots, owner)?;
        }
        report.chowned = roots.len();
    }
    Ok(report)
}

/// Remove paths already covered by a shallower entry.
fn topmost(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort_by_key(|path| path.components().count());
    let mut kept: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !kept.iter().any(|parent| path.starts_with(parent)) {
            kept.push(path);
        }
    }
    kept
}

/// Directories that must never be written by a hegn repair, even when a
/// configured path or a planted symlink points into them.
const SYSTEM_ROOTS: &[&str] = &[
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/boot",
    "/dev",
    "/proc",
    "/sys",
    "/root",
    "/var/lib",
    "/var/cache",
];

/// Whether a repair may touch `path`.
///
/// The configured roots are user-controlled, so a hostile `config.toml` could
/// point `vms_dir` at `/etc` or at another user's tree. Canonicalize the parent
/// (the entry itself may be unreadable), refuse system trees, and require every
/// ancestor to belong to the invoking user or root. A world-writable,
/// non-sticky ancestor would let another user swap the tree between the audit
/// and the repair, so it is refused as well.
fn repairable(path: &Path, owner: Owner) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let Ok(parent) = fs::canonicalize(parent) else {
        return false;
    };
    let canonical = parent.join(name);
    if SYSTEM_ROOTS.iter().any(|root| {
        let root = Path::new(root);
        canonical == root || canonical.starts_with(root)
    }) {
        return false;
    }
    let mut current = Some(parent.as_path());
    while let Some(dir) = current {
        let Ok(metadata) = fs::symlink_metadata(dir) else {
            return false;
        };
        if metadata.uid() != owner.uid && metadata.uid() != 0 {
            return false;
        }
        let mode = metadata.permissions().mode();
        if mode & 0o002 != 0 && mode & 0o1000 == 0 {
            return false;
        }
        current = dir.parent();
    }
    true
}

/// Resolve a path's parent without touching the final entry (which may be the
/// unreadable leftover itself).
fn canonical_entry(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent", path.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?;
    Ok(fs::canonicalize(parent)
        .with_context(|| format!("failed to resolve {}", parent.display()))?
        .join(name))
}

fn widen_owner_access(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mode = metadata.permissions().mode();
    let wanted = if metadata.is_dir() {
        mode | 0o700
    } else {
        mode | 0o600
    };
    fs::set_permissions(path, fs::Permissions::from_mode(wanted))
}

fn escalate_chown(paths: &[PathBuf], owner: Owner) -> Result<()> {
    let helpers = crate::network::privilege_helpers();
    if helpers.is_empty() {
        bail!(
            "cannot repair ownership automatically: neither pkexec nor sudo is installed. Run {}",
            manual_command(paths, owner)
        );
    }
    // Resolve `chown` through the fixed path too: pkexec does not inherit the
    // caller's PATH, so an absolute program removes any ambiguity.
    let chown = crate::network::safe_command_path("chown")
        .map_or_else(|| OsString::from("chown"), PathBuf::into_os_string);
    let spec = owner_spec(owner);
    let mut failures = Vec::new();
    for (helper, prefix) in helpers {
        let helper_path = crate::network::safe_command_path(helper)
            .with_context(|| format!("failed to locate {helper}"))?;
        let mut args: Vec<OsString> = Vec::new();
        if prefix {
            args.push("--".into());
        }
        args.push(chown.clone());
        args.push("-R".into());
        // Never let a planted symlink redirect a root chown outside the tree,
        // and never operate on `/` even if a configured root collapses to it.
        args.push("-h".into());
        args.push("-P".into());
        args.push("--preserve-root".into());
        args.push(spec.clone().into());
        args.push("--".into());
        for path in paths {
            args.push(path.clone().into_os_string());
        }
        // Describe the change and echo the exact command before the password
        // prompt so the user can inspect what will run as root.
        eprintln!(
            "hegn: handing {} root-owned hegn path(s) back to {} with a recursive chown that does \
             not follow symlinks; requesting root privileges; about to run:\n  {}",
            paths.len(),
            spec,
            render_command(helper, &args)
        );
        match Command::new(&helper_path)
            .args(&args)
            .env("PATH", crate::network::SAFE_PATH)
            .status()
        {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => failures.push(format!("{helper} exited with {status}")),
            Err(err) => failures.push(format!("failed to run {helper}: {err}")),
        }
    }
    bail!(
        "{}; run {} manually",
        failures.join("; "),
        manual_command(paths, owner)
    )
}

fn owner_spec(owner: Owner) -> String {
    match owner.gid {
        Some(gid) => format!("{}:{gid}", owner.uid),
        None => owner.uid.to_string(),
    }
}

fn manual_command(paths: &[PathBuf], owner: Owner) -> String {
    let mut parts = vec![
        "sudo".to_string(),
        "chown".to_string(),
        "-R".to_string(),
        "-h".to_string(),
        "-P".to_string(),
        "--preserve-root".to_string(),
        owner_spec(owner),
        "--".to_string(),
    ];
    parts.extend(
        paths
            .iter()
            .map(|path| shell_quote_always(&path.to_string_lossy())),
    );
    parts.join(" ")
}

fn render_command(helper: &str, args: &[OsString]) -> String {
    let mut parts = vec![shell_quote_always(helper)];
    parts.extend(
        args.iter()
            .map(|arg| shell_quote_always(&arg.to_string_lossy())),
    );
    parts.join(" ")
}

fn is_permission_denied(error: &io::Error) -> bool {
    crate::shared::fs::is_permission_denied(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> Config {
        Config {
            state_dir: dir.join("state"),
            snapshots_dir: dir.join("snapshots"),
            vms_dir: dir.join("vms"),
            templates_dir: dir.join("templates"),
            profiles_dir: dir.join("config/hegn/profiles"),
            kernel_image: dir.join("images/kernel"),
            rootfs_image: dir.join("images/rootfs"),
            ..Config::default()
        }
    }

    fn unprivileged() -> bool {
        (unsafe { libc::geteuid() }) != 0
    }

    #[test]
    fn a_missing_tree_needs_no_repair() {
        let dir = tempfile::tempdir().unwrap();
        let audit = audit(&config(dir.path()));
        assert!(audit.ok(), "{audit:?}");
        assert!(audit.detail().contains("accessible"));
    }

    #[test]
    fn an_unreadable_file_is_repaired_by_widening_its_mode() {
        if !unprivileged() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let vm_dir = config.vms_dir.join("demo");
        fs::create_dir_all(&vm_dir).unwrap();
        let path = vm_dir.join("seed-generation");
        fs::write(&path, "7\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let found = audit(&config);
        assert_eq!(found.problems.len(), 1, "{found:?}");
        assert_eq!(found.problems[0].kind, ProblemKind::Inaccessible);
        assert!(
            found.detail().contains("seed-generation"),
            "{:?}",
            found.detail()
        );

        let report = repair(&config).unwrap();
        assert!(report.any(), "{report:?}");
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(audit(&config).ok());
    }

    #[test]
    fn an_unreadable_directory_is_repaired_recursively_through_its_owner_bits() {
        if !unprivileged() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let path = config.state_dir.join("logs");
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let found = audit(&config);
        assert_eq!(found.problems[0].path, path);
        assert_eq!(found.problems[0].kind, ProblemKind::Inaccessible);

        repair(&config).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(audit(&config).ok());
    }

    #[test]
    fn a_read_only_root_owned_image_is_not_a_problem() {
        if !unprivileged() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        fs::create_dir_all(config.kernel_image.parent().unwrap()).unwrap();
        fs::write(&config.kernel_image, b"kernel").unwrap();
        // Simulate a system-wide image: readable but not writable. The owner is
        // still this user, so only the access mask is under test.
        fs::set_permissions(&config.kernel_image, fs::Permissions::from_mode(0o400)).unwrap();

        assert!(audit(&config).ok(), "a readable image needs no repair");
    }

    #[test]
    fn repair_refuses_system_paths() {
        let owner = Owner {
            uid: unsafe { libc::geteuid() },
            gid: None,
        };
        assert!(!repairable(Path::new("/etc/shadow"), owner));
        assert!(!repairable(Path::new("/usr/lib/thing"), owner));
        assert!(!repairable(Path::new("/"), owner));

        let dir = tempfile::tempdir().unwrap();
        assert!(repairable(&dir.path().join("state"), owner));
    }

    #[test]
    fn topmost_keeps_only_the_shallowest_paths() {
        let paths = vec![
            PathBuf::from("/state/logs/a"),
            PathBuf::from("/state"),
            PathBuf::from("/state/network/b"),
            PathBuf::from("/other/c"),
        ];
        assert_eq!(
            topmost(paths),
            vec![PathBuf::from("/state"), PathBuf::from("/other/c")]
        );
    }

    #[test]
    fn the_manual_command_separates_options_from_paths() {
        let command = manual_command(
            &[PathBuf::from("/state/with space")],
            Owner {
                uid: 1000,
                gid: Some(1001),
            },
        );
        assert_eq!(
            command,
            "sudo chown -R -h -P --preserve-root 1000:1001 -- '/state/with space'"
        );
    }

    #[test]
    fn the_detail_names_at_most_three_paths() {
        let audit = Audit {
            problems: (0..5)
                .map(|index| Problem {
                    path: PathBuf::from(format!("/state/{index}")),
                    kind: ProblemKind::Inaccessible,
                })
                .collect(),
            truncated: false,
        };
        let detail = audit.detail();
        assert!(detail.contains("/state/0"), "{detail}");
        assert!(detail.contains("/state/2"), "{detail}");
        assert!(!detail.contains("/state/3"), "{detail}");
        assert!(detail.contains('…'), "{detail}");
    }
}
