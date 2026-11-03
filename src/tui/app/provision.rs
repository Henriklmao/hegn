//! First-boot harness provisioning state and the pending-launch handshake.

use super::*;

impl App {
    /// Record the latest first-boot provisioning stage.
    pub fn set_provision_progress(&mut self, progress: crate::agent::provision::ProvisionProgress) {
        self.provision = Some(progress);
    }

    /// The guest finished installing the harness preset.
    pub fn provision_ready(&mut self) {
        self.provision = None;
        self.provision_running = false;
        self.provision_ready_seen = true;
        if let Some(pending) = self.pending_launch.as_mut() {
            pending.ready = true;
        }
        self.toast("harness preset ready", ToastLevel::Info);
    }

    /// Provisioning stopped before the marker was written.
    pub fn provision_failed(&mut self, message: String) {
        self.provision = None;
        self.provision_running = false;
        self.provision_ready_seen = false;
        self.pending_launch = None;
        self.toast(
            format!("harness setup failed: {message}"),
            ToastLevel::Error,
        );
    }

    /// Note that first-boot provisioning is running.
    pub fn provision_started(&mut self) {
        self.provision_running = true;
        if let Some(pending) = self.pending_launch.as_mut() {
            pending.provision_seen = true;
        }
    }
}
