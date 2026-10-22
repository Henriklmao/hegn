//! Hardened `virtiofsd` process management.
//!
//! Every session gets a dedicated virtiofsd instance whose `--shared-dir` is
//! exactly one project directory. The daemon always runs with a sandbox
//! (`namespace` by default, `chroot` available) so a guest cannot traverse out
//! of the exported tree.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::VirtiofsSandbox;

/// Configuration for one virtiofsd instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtiofsConfig {
    pub binary: PathBuf,
    pub shared_dir: PathBuf,
    pub socket_path: PathBuf,
    pub sandbox: VirtiofsSandbox,
    pub read_only: bool,
    /// Optional cache mode (`auto`, `always`, `never`).
    pub cache: Option<String>,
}

impl VirtiofsConfig {
    pub fn new(
        binary: impl Into<PathBuf>,
        shared_dir: impl Into<PathBuf>,
        socket_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            binary: binary.into(),
            shared_dir: shared_dir.into(),
            socket_path: socket_path.into(),
            sandbox: VirtiofsSandbox::Namespace,
            read_only: false,
            cache: Some("auto".to_string()),
        }
    }

    /// Build argv. The shared directory is canonicalized so symlinks cannot
    /// widen the export; the daemon is always sandboxed.
    pub fn build_args(&self) -> Result<Vec<OsString>> {
        let shared = self.shared_dir.canonicalize().with_context(|| {
            format!(
                "shared directory {} does not exist",
                self.shared_dir.display()
            )
        })?;
        if !shared.is_dir() {
            bail!("shared path {} is not a directory", shared.display());
        }
        if self.socket_path.as_os_str().is_empty() {
            bail!("virtiofsd socket path is empty");
        }
        if let Some(parent) = self.socket_path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            bail!("socket directory {} does not exist", parent.display());
        }

        let mut args: Vec<OsString> = vec![
            "--shared-dir".into(),
            shared.into_os_string(),
            "--socket-path".into(),
            self.socket_path.clone().into_os_string(),
            "--sandbox".into(),
            self.sandbox.as_flag().into(),
        ];
        if self.read_only {
            args.push("--readonly".into());
        }
        if let Some(cache) = &self.cache {
            args.push("--cache".into());
            args.push(cache.into());
        }
        Ok(args)
    }

    /// Spawn the daemon with stdout/stderr detached from the TUI.
    ///
    /// Both streams go to the null device: inheriting them would corrupt the
    /// alternate screen, and a piped stream that nobody drains fills up and
    /// blocks virtiofsd mid-session.
    pub fn spawn(&self) -> Result<VirtiofsProcess> {
        let args = self.build_args()?;
        // The socket may be left over from a crashed session.
        if self.socket_path.exists() {
            let _ = fs::remove_file(&self.socket_path);
        }
        let child = Command::new(&self.binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn {} (is virtiofsd installed?)",
                    self.binary.display()
                )
            })?;
        Ok(VirtiofsProcess {
            child,
            socket_path: self.socket_path.clone(),
        })
    }
}

/// A running virtiofsd instance. Killed on drop.
#[derive(Debug)]
pub struct VirtiofsProcess {
    child: Child,
    socket_path: PathBuf,
}

impl VirtiofsProcess {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Wait until virtiofsd has created its socket.
    pub fn wait_for_socket(&mut self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.socket_path.exists() {
                return Ok(());
            }
            if !self.is_running() {
                let status = self.child.wait().context("failed to wait for virtiofsd")?;
                bail!("virtiofsd exited before creating its socket ({status})");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bail!(
            "timed out waiting for virtiofsd socket {}",
            self.socket_path.display()
        );
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl Drop for VirtiofsProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    #[test]
    fn args_are_sandboxed_and_scoped_to_one_directory() {
        let (project, run) = setup();
        let cfg = VirtiofsConfig::new(
            "/usr/bin/virtiofsd",
            project.path(),
            run.path().join("fs.sock"),
        );
        let args = cfg.build_args().unwrap();
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(rendered.windows(2).any(|w| w
            == [
                "--shared-dir",
                project.path().canonicalize().unwrap().to_str().unwrap()
            ]));
        assert!(
            rendered
                .windows(2)
                .any(|w| w[0] == "--sandbox" && w[1] == "namespace")
        );
        assert!(rendered.windows(2).any(|w| w[0] == "--socket-path"));
        assert!(!rendered.iter().any(|a| a == "none"));
        assert!(
            rendered
                .windows(2)
                .any(|w| w[0] == "--cache" && w[1] == "auto")
        );
    }

    #[test]
    fn chroot_and_readonly_are_supported() {
        let (project, run) = setup();
        let mut cfg = VirtiofsConfig::new("virtiofsd", project.path(), run.path().join("s"));
        cfg.sandbox = VirtiofsSandbox::Chroot;
        cfg.read_only = true;
        cfg.cache = None;
        let rendered: Vec<String> = cfg
            .build_args()
            .unwrap()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            rendered
                .windows(2)
                .any(|w| w[0] == "--sandbox" && w[1] == "chroot")
        );
        assert!(rendered.contains(&"--readonly".to_string()));
        assert!(!rendered.contains(&"--cache".to_string()));
    }

    #[test]
    fn rejects_missing_shared_directory() {
        let (_project, run) = setup();
        let cfg = VirtiofsConfig::new("virtiofsd", "/missing/project", run.path().join("s"));
        assert!(cfg.build_args().is_err());
    }

    #[test]
    fn rejects_missing_socket_directory() {
        let (project, _run) = setup();
        let cfg = VirtiofsConfig::new(
            "virtiofsd",
            project.path(),
            "/definitely/missing/dir/fs.sock",
        );
        assert!(cfg.build_args().is_err());
    }

    #[test]
    fn spawn_does_not_block_on_noisy_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let (project, run) = setup();
        let socket = run.path().join("fs.sock");
        let binary = run.path().join("fake-virtiofsd");
        let script = format!(
            "#!/bin/sh\n# More than a pipe buffer: must be discarded, not left unread.\nhead -c 262144 /dev/zero >&2\ntouch '{}'\n",
            socket.display()
        );
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cfg = VirtiofsConfig::new(&binary, project.path(), &socket);
        let mut process = cfg.spawn().unwrap();
        process
            .wait_for_socket(Duration::from_secs(5))
            .expect("noisy virtiofsd should still create its socket");
        process.kill();
    }

    #[test]
    fn spawn_reports_missing_binary() {
        let (project, run) = setup();
        let cfg = VirtiofsConfig::new(
            "/definitely/not/virtiofsd",
            project.path(),
            run.path().join("fs.sock"),
        );
        let err = cfg.spawn().unwrap_err().to_string();
        assert!(err.contains("failed to spawn"), "{err}");
    }
}
