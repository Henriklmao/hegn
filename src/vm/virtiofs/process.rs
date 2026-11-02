//! Handle for a running `virtiofsd` daemon.
//!
//! The process is killed on drop, including removal of its vhost-user socket,
//! so a crashed or cancelled session does not leave a stale daemon behind.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// A running virtiofsd instance. Killed on drop.
#[derive(Debug)]
pub struct VirtiofsProcess {
    pub(super) child: Child,
    pub(super) socket_path: PathBuf,
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
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("socket wait timeout is too large")?;
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
