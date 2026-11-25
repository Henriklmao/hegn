//! Guest workspace roots for multi-project VMs.
//!
//! In a multi-project VM the guest sees one mount per member below
//! `guest_projects_base` (default `/projects/<slug>`) plus `/workspace`, which
//! is a symlink to the VM's primary member. Every one of those paths is
//! *workspace activity*: deleting something below a member is destroying
//! uncommitted work exactly like deleting below `/workspace`, and reading a
//! sibling's secrets stays inside the VM trust boundary rather than escaping
//! it.
//!
//! [`GuestWorkspacePolicy`] carries the roots for one VM so the destructive
//! and exfiltration guardrails can classify guest paths without hardcoding
//! `/workspace`. Host-side protections are deliberately untouched: this type
//! never widens what a command may do on the host.

use std::path::Path;

use thiserror::Error;

/// Default guest directory that holds one mount per exported project.
pub const DEFAULT_GUEST_PROJECTS_BASE: &str = "/projects";

/// Guest symlink to the VM's primary project. Always a workspace root.
pub const WORKSPACE_GUEST_PATH: &str = "/workspace";

/// Errors raised while assembling a [`GuestWorkspacePolicy`].
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GuestWorkspacePolicyError {
    #[error("unsafe guest projects base {0:?}: expected an absolute, simple path")]
    UnsafeBase(String),
    #[error("unsafe guest project slug {0:?}: expected a single simple path component")]
    UnsafeSlug(String),
    #[error("duplicate guest project slug {0:?}")]
    DuplicateSlug(String),
    #[error("primary project slug {0:?} is not an exported member")]
    UnknownPrimary(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GuestWorkspaceRoot {
    path: String,
    read_only: bool,
}

/// The set of guest paths that count as workspace for one VM.
///
/// The policy always contains [`WORKSPACE_GUEST_PATH`] and may contain any
/// number of member mounts below the configured base. A VM with no members is
/// still a valid policy (useful before membership is resolved).
///
/// Path matching is component-aware and never follows `..`:
///
/// ```
/// # use hegn::security::GuestWorkspacePolicy;
/// let policy = GuestWorkspacePolicy::with_members("/projects", [("parser", false)]).unwrap();
/// assert!(policy.contains("/projects/parser/src"));
/// assert!(!policy.contains("/projects/parser-other"));
/// assert!(!policy.contains("/projects"));
/// assert!(policy.contains("/workspace"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestWorkspacePolicy {
    base: String,
    /// Sorted by path so [`Self::roots`] is deterministic.
    roots: Vec<GuestWorkspaceRoot>,
    primary: Option<String>,
}

impl Default for GuestWorkspacePolicy {
    fn default() -> Self {
        Self {
            base: DEFAULT_GUEST_PROJECTS_BASE.to_string(),
            roots: vec![GuestWorkspaceRoot {
                path: WORKSPACE_GUEST_PATH.to_string(),
                read_only: false,
            }],
            primary: None,
        }
    }
}

impl GuestWorkspacePolicy {
    /// Create an empty policy rooted at `base`.
    ///
    /// `base` must be an absolute path below `/` made of simple components.
    /// A trailing slash or repeated separators are normalized.
    pub fn new(base: impl Into<String>) -> Result<Self, GuestWorkspacePolicyError> {
        let base = normalize_base(&base.into())?;
        Ok(Self {
            base,
            roots: vec![GuestWorkspaceRoot {
                path: WORKSPACE_GUEST_PATH.to_string(),
                read_only: false,
            }],
            primary: None,
        })
    }

    /// Create a policy with one member per `(slug, read_only)` pair.
    pub fn with_members<I, S>(
        base: impl Into<String>,
        members: I,
    ) -> Result<Self, GuestWorkspacePolicyError>
    where
        I: IntoIterator<Item = (S, bool)>,
        S: Into<String>,
    {
        let mut policy = Self::new(base)?;
        for (slug, read_only) in members {
            policy.add_member(slug, read_only)?;
        }
        Ok(policy)
    }

    /// Export one member at `<base>/<slug>`.
    ///
    /// Slugs are a single path component ("parser", "cli-2"); separators,
    /// `.`/`..`, whitespace and control characters are rejected so a slug can
    /// never move the guest path outside the configured base.
    pub fn add_member(
        &mut self,
        slug: impl Into<String>,
        read_only: bool,
    ) -> Result<(), GuestWorkspacePolicyError> {
        let slug = slug.into();
        validate_slug(&slug)?;
        let path = format!("{}/{}", self.base, slug);
        if self.roots.iter().any(|root| root.path == path) {
            return Err(GuestWorkspacePolicyError::DuplicateSlug(slug));
        }
        self.roots.push(GuestWorkspaceRoot { path, read_only });
        self.roots.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(())
    }

    /// Mark `slug` as the VM's primary project.
    ///
    /// `/workspace` is a symlink to this member, so its read-only flag follows
    /// the member's flag. This matters only for write policy; read access to a
    /// read-only member is still normal workspace activity.
    pub fn set_primary(&mut self, slug: &str) -> Result<(), GuestWorkspacePolicyError> {
        let path = format!("{}/{}", self.base, slug);
        let read_only = self
            .roots
            .iter()
            .find(|root| root.path == path)
            .map(|root| root.read_only)
            .ok_or_else(|| GuestWorkspacePolicyError::UnknownPrimary(slug.to_string()))?;
        if let Some(workspace) = self
            .roots
            .iter_mut()
            .find(|root| root.path == WORKSPACE_GUEST_PATH)
        {
            workspace.read_only = read_only;
        }
        self.primary = Some(slug.to_string());
        Ok(())
    }

    /// Builder form of [`Self::set_primary`].
    pub fn with_primary(mut self, slug: &str) -> Result<Self, GuestWorkspacePolicyError> {
        self.set_primary(slug)?;
        Ok(self)
    }

    /// Configured guest directory that contains the member mounts.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Primary project slug, when one has been declared.
    pub fn primary(&self) -> Option<&str> {
        self.primary.as_deref()
    }

    /// Every workspace root, sorted, always including `/workspace`.
    pub fn roots(&self) -> Vec<&str> {
        self.roots.iter().map(|root| root.path.as_str()).collect()
    }

    /// True when `path` is a workspace root or a descendant of one.
    ///
    /// Matching is case-sensitive and component-aware: `/projects/parser`
    /// contains `/projects/parser/src` but not `/projects/parser-other`.
    /// Empty, relative, `..`-bearing, NUL-bearing and control-character paths
    /// are rejected rather than normalized.
    pub fn contains(&self, path: &str) -> bool {
        normalize_guest_path(path)
            .and_then(|normalized| self.matching_root(&normalized))
            .is_some()
    }

    /// [`Self::contains`] for an OS path; non-UTF-8 paths are rejected.
    pub fn contains_path(&self, path: &Path) -> bool {
        path.to_str().is_some_and(|path| self.contains(path))
    }

    /// Conservative containment for untrusted shell text.
    ///
    /// Unlike [`Self::contains`], `..` segments are resolved lexically first,
    /// so `rm -rf /projects/parser/../cli` is still recognised as workspace
    /// activity. Resolution never climbs above `/`; paths that would do so are
    /// rejected.
    pub fn contains_resolved(&self, path: &str) -> bool {
        resolve_guest_path(path)
            .and_then(|resolved| self.matching_root(&resolved))
            .is_some()
    }

    /// True when `path` is inside a member mounted read-only.
    ///
    /// Read-only protects writes, not reads: the path is still contained by
    /// [`Self::contains`]. `/workspace` reflects its primary member when
    /// [`Self::set_primary`] was called.
    pub fn is_read_only_root(&self, path: &str) -> bool {
        normalize_guest_path(path)
            .and_then(|normalized| self.matching_root(&normalized))
            .is_some_and(|root| root.read_only)
    }

    /// True when `path` targets the configured base or every member at once.
    ///
    /// This recognises destructive aggregate targets:
    ///
    /// - the base itself (`/projects`, `/projects/`)
    /// - top-level globs (`/projects/*`, `/projects/**`, with optional
    ///   trailing slash)
    ///
    /// It is intentionally *not* ordinary containment: the bare base is not a
    /// workspace root, `/projects/parser-other` does not match the `parser`
    /// member, and lookalikes such as `/projectspace` or `/projects-other`
    /// never match. A policy without members has nothing to cover and always
    /// returns `false`, preserving single-project `/workspace` behaviour.
    pub fn covers_all_members(&self, path: &str) -> bool {
        if !self.has_members() {
            return false;
        }
        let Some(normalized) = normalize_guest_path(path) else {
            return false;
        };
        if normalized == self.base {
            return true;
        }
        normalized
            .strip_prefix(&self.base)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| rest == "*" || rest == "**")
    }

    fn has_members(&self) -> bool {
        self.roots
            .iter()
            .any(|root| root.path != WORKSPACE_GUEST_PATH)
    }

    fn matching_root(&self, normalized: &str) -> Option<&GuestWorkspaceRoot> {
        self.roots
            .iter()
            .filter(|root| {
                normalized == root.path
                    || (normalized.len() > root.path.len()
                        && normalized.starts_with(&root.path)
                        && normalized.as_bytes()[root.path.len()] == b'/')
            })
            // Prefer the most specific root when bases overlap.
            .max_by_key(|root| root.path.len())
    }
}

/// Normalize a guest path without allowing `..`.
///
/// Drops empty and `.` components, collapses repeated separators and trailing
/// slashes, and returns `None` for relative, empty, `/`, NUL-bearing,
/// control-character or parent-traversing paths.
fn normalize_guest_path(path: &str) -> Option<String> {
    if !is_plain_absolute_path(path) {
        return None;
    }
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => return None,
            other => components.push(other),
        }
    }
    (!components.is_empty()).then(|| format!("/{}", components.join("/")))
}

/// Resolve `.`/`..` lexically, refusing to climb above `/`.
fn resolve_guest_path(path: &str) -> Option<String> {
    if !is_plain_absolute_path(path) {
        return None;
    }
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            other => components.push(other),
        }
    }
    (!components.is_empty()).then(|| format!("/{}", components.join("/")))
}

/// Absolute, non-empty, no NUL and no control characters.
fn is_plain_absolute_path(path: &str) -> bool {
    path.starts_with('/') && !path.is_empty() && !path.chars().any(char::is_control)
}

fn normalize_base(base: &str) -> Result<String, GuestWorkspacePolicyError> {
    let normalized = normalize_guest_path(base)
        .ok_or_else(|| GuestWorkspacePolicyError::UnsafeBase(base.into()))?;
    if normalized == "/"
        || !normalized
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && is_simple_component(component))
    {
        return Err(GuestWorkspacePolicyError::UnsafeBase(base.into()));
    }
    Ok(normalized)
}

fn validate_slug(slug: &str) -> Result<(), GuestWorkspacePolicyError> {
    if slug.is_empty()
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || !slug.chars().all(is_simple_component_char)
    {
        return Err(GuestWorkspacePolicyError::UnsafeSlug(slug.into()));
    }
    Ok(())
}

fn is_simple_component(component: &str) -> bool {
    component.chars().all(is_simple_component_char)
}

fn is_simple_component_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::*;

    fn guest_policy(base: &str, members: &[(&str, bool)]) -> GuestWorkspacePolicy {
        GuestWorkspacePolicy::with_members(base, members.iter().copied()).unwrap()
    }

    #[test]
    fn empty_member_set_still_allows_workspace() {
        let policy = GuestWorkspacePolicy::default();
        assert_eq!(policy.roots(), vec![WORKSPACE_GUEST_PATH]);
        assert!(policy.contains("/workspace"));
        assert!(policy.contains("/workspace/src"));
        assert!(!policy.contains("/projects/parser"));
        assert!(!policy.is_read_only_root("/workspace"));
    }

    #[test]
    fn member_children_are_workspace() {
        let policy = guest_policy("/projects", &[("parser", false)]);
        assert!(policy.contains("/projects/parser"));
        assert!(policy.contains("/projects/parser/src/main.rs"));
        assert!(policy.contains("/workspace"));
        assert_eq!(
            policy.roots(),
            vec!["/projects/parser", "/workspace"],
            "roots are deterministic"
        );
    }

    #[test]
    fn sibling_prefix_collisions_are_rejected() {
        let policy = guest_policy("/projects", &[("parser", false), ("cli", false)]);
        assert!(policy.contains("/projects/parser"));
        assert!(!policy.contains("/projects/parser-other"));
        assert!(!policy.contains("/projects/parser-other/src"));
        assert!(!policy.contains("/projects/cli2"));
        assert!(!policy.contains("/projectspace"));
        assert!(!policy.contains("/projects/par"));
    }

    #[test]
    fn base_and_parent_traversal_are_rejected() {
        let policy = guest_policy("/projects", &[("parser", false)]);
        for path in [
            "/projects",
            "/projects/",
            "/projects/..",
            "/projects/../etc",
            "/projects/./..",
            "/projects/parser/..",
            "/projects/parser/../cli",
            "..",
            "../projects/parser",
            "projects/parser",
            "",
            "/",
            ".",
            "/.",
            "/workspace/..",
        ] {
            assert!(!policy.contains(path), "{path:?} must not be contained");
        }
    }

    #[test]
    fn normalizes_trailing_slashes_repeated_separators_and_dot_segments() {
        let policy = guest_policy("/projects", &[("parser", false)]);
        for path in [
            "/projects/parser/",
            "/projects/parser//",
            "//projects//parser//src",
            "/projects/parser/./src",
            "/projects/./parser/./src/",
            "/workspace/",
            "/workspace//src",
        ] {
            assert!(policy.contains(path), "{path:?} must be contained");
        }
    }

    #[test]
    fn workspace_children_are_roots_but_lookalikes_are_not() {
        let policy = guest_policy("/projects", &[("parser", false)]);
        assert!(policy.contains("/workspace"));
        assert!(policy.contains("/workspace/src"));
        assert!(!policy.contains("/workspace2"));
        assert!(!policy.contains("/workspace-backup"));
        assert!(!policy.contains("/Workspace"));
        assert!(!policy.contains("/etc"));
        assert!(!policy.contains("/home/user"));
    }

    #[test]
    fn multiple_members_are_independently_contained() {
        let policy = guest_policy(
            "/projects",
            &[("parser", false), ("cli", false), ("docs", false)],
        );
        assert!(policy.contains("/projects/parser/src"));
        assert!(policy.contains("/projects/cli/.env"));
        assert!(policy.contains("/projects/docs/book.md"));
        assert_eq!(policy.roots().len(), 4);
        assert!(policy.roots().contains(&"/workspace"));
    }

    #[test]
    fn read_only_members_are_still_readable_workspace() {
        let policy = guest_policy("/projects", &[("parser", false), ("cli", true)]);
        assert!(policy.contains("/projects/cli"));
        assert!(policy.contains("/projects/cli/.env"));
        assert!(policy.is_read_only_root("/projects/cli"));
        assert!(policy.is_read_only_root("/projects/cli/src"));
        assert!(!policy.is_read_only_root("/projects/parser"));
        assert!(!policy.is_read_only_root("/workspace"));
        assert!(!policy.is_read_only_root("/etc/shadow"));
    }

    #[test]
    fn base_and_top_level_globs_cover_all_members() {
        let policy = guest_policy("/projects", &[("parser", false), ("cli", true)]);
        for path in [
            "/projects",
            "/projects/",
            "//projects",
            "/projects/*",
            "/projects/**",
            "/projects/*/",
            "/projects/**/",
            "/projects/./*",
        ] {
            assert!(
                policy.covers_all_members(path),
                "{path:?} covers every member"
            );
        }
    }

    #[test]
    fn covers_all_members_rejects_lookalikes_and_member_paths() {
        let policy = guest_policy("/projects", &[("parser", false), ("cli", true)]);
        for path in [
            "/projectspace",
            "/projects-other",
            "/projects2",
            "/workspace-backup",
            "/workspace",
            "/projects/parser",
            "/projects/parser/",
            "/projects/parser-other",
            "/projects/parser-other/*",
            "/projects/*/src",
            "/projects/*/*",
            "/projects/..",
            "/projects/../etc",
            "projects/*",
            "",
        ] {
            assert!(
                !policy.covers_all_members(path),
                "{path:?} must not cover every member"
            );
        }
    }

    #[test]
    fn covers_all_members_uses_the_configured_base_and_ignores_empty_policies() {
        let custom = guest_policy("/legacy-workspace", &[("parser", false)]);
        assert!(custom.covers_all_members("/legacy-workspace"));
        assert!(custom.covers_all_members("/legacy-workspace/*"));
        assert!(!custom.covers_all_members("/projects"));
        assert!(!custom.covers_all_members("/projects/*"));

        let empty = GuestWorkspacePolicy::default();
        assert!(!empty.covers_all_members("/projects"));
        assert!(!empty.covers_all_members("/projects/*"));
        assert!(!empty.covers_all_members("/workspace"));
    }

    #[test]
    fn primary_makes_workspace_follow_the_member_read_only_flag() {
        let mut policy = guest_policy("/projects", &[("parser", false), ("cli", true)]);
        assert_eq!(policy.primary(), None);
        policy.set_primary("cli").unwrap();
        assert_eq!(policy.primary(), Some("cli"));
        assert!(policy.contains("/workspace/src"));
        assert!(policy.is_read_only_root("/workspace/src"));
        assert!(policy.is_read_only_root("/workspace"));

        let policy = guest_policy("/projects", &[("parser", false), ("cli", true)]);
        let policy = policy.with_primary("parser").unwrap();
        assert!(!policy.is_read_only_root("/workspace"));
        assert_eq!(
            policy.primary(),
            Some("parser"),
            "with_primary keeps the declaration"
        );
        assert_eq!(
            GuestWorkspacePolicy::with_members::<_, &str>("/projects", [])
                .unwrap()
                .with_primary("parser")
                .unwrap_err(),
            GuestWorkspacePolicyError::UnknownPrimary("parser".into())
        );
    }

    #[test]
    fn custom_base_is_honoured_and_workspace_stays_a_root() {
        let policy = guest_policy("/legacy-workspace", &[("parser", false)]);
        assert!(policy.contains("/legacy-workspace/parser/src"));
        assert!(policy.contains("/workspace"));
        assert!(!policy.contains("/projects/parser"));
        assert_eq!(policy.base(), "/legacy-workspace");
    }

    #[test]
    fn base_validation_rejects_odd_shapes() {
        for bad in [
            "",
            "/",
            "/.",
            "/..",
            "/projects/..",
            "/projects/../etc",
            "projects",
            "~/projects",
            "/work space",
            "/work\nspace",
            "/projects/$name",
        ] {
            assert!(
                GuestWorkspacePolicy::new(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
        for good in ["/projects/", "//projects", "/opt/hegn-projects_1.2"] {
            let policy = GuestWorkspacePolicy::new(good).unwrap();
            assert!(policy.contains("/workspace"));
        }
        assert_eq!(
            GuestWorkspacePolicy::new("relative").unwrap_err(),
            GuestWorkspacePolicyError::UnsafeBase("relative".into())
        );
    }

    #[test]
    fn slug_validation_rejects_odd_shapes() {
        for bad in ["", ".", "..", "a/b", "../cli", "par ser", "par\nser", "π"] {
            assert!(
                GuestWorkspacePolicy::with_members("/projects", [(bad, false)]).is_err(),
                "{bad:?} must be rejected"
            );
        }
        let mut policy = guest_policy("/projects", &[("parser", false)]);
        assert_eq!(
            policy.add_member("parser", true).unwrap_err(),
            GuestWorkspacePolicyError::DuplicateSlug("parser".into())
        );
        policy.add_member("parser-2", false).unwrap();
        assert!(policy.contains("/projects/parser-2"));
        assert!(!policy.contains("/projects/parser-20"));
    }

    #[test]
    fn non_utf8_nul_and_control_paths_are_rejected() {
        let policy = guest_policy("/projects", &[("parser", false)]);
        let non_utf8 = OsStr::from_bytes(b"/projects/parser/\xff");
        assert!(!policy.contains_path(Path::new(non_utf8)));
        assert!(policy.contains_path(Path::new("/projects/parser/src")));
        assert!(!policy.contains("/projects/parser\0/src"));
        assert!(!policy.contains("/projects/parser\n/src"));
        assert!(!policy.contains("/projects/parser\u{7f}"));
    }

    #[test]
    fn resolved_containment_catches_parent_traversal_inside_workspace() {
        let policy = guest_policy("/projects", &[("parser", false), ("cli", false)]);
        assert!(policy.contains_resolved("/projects/parser/src/../target"));
        assert!(policy.contains_resolved("/projects/parser/../cli/.env"));
        assert!(
            policy.contains_resolved("/projects/parser-other/../parser"),
            "lexical resolution still lands inside the parser root"
        );
        assert!(policy.contains_resolved("/projects/./parser/./src"));
        assert!(policy.contains_resolved("/workspace/../workspace/src"));
        assert!(!policy.contains_resolved("/projects/parser/../../etc/passwd"));
        assert!(!policy.contains_resolved("/../../etc"));
        assert!(!policy.contains_resolved(".."));
        assert!(!policy.contains_resolved("/projects"));
        assert!(!policy.contains_resolved("/"));

        // `contains` itself stays strict about `..`.
        assert!(!policy.contains("/projects/parser/../cli"));
    }

    #[test]
    fn overlapping_roots_prefer_the_most_specific_match() {
        let policy = guest_policy("/workspace", &[("parser", true)]);
        assert!(policy.contains("/workspace"));
        assert!(policy.contains("/workspace/parser"));
        assert!(policy.is_read_only_root("/workspace/parser/src"));
        assert!(!policy.is_read_only_root("/workspace/other"));
    }
}
