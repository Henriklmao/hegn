//! Doctor report and first-time setup modals.

use crate::diagnostics::doctor::DoctorReport;

use super::*;

impl App {
    /// Open the explicit confirmation for the automatic first-time setup.
    ///
    /// The setup itself may ask for sudo/pkexec; this confirmation is the
    /// in-TUI "yes" before anything privileged is attempted.
    pub fn open_setup_confirmation(&mut self) -> Vec<Effect> {
        let actions = crate::diagnostics::doctor::setup_actions(&self.config);
        if !actions.any() {
            self.toast("nothing to set up", ToastLevel::Info);
            return Vec::new();
        }
        let mut lines = vec![
            "hegn can prepare this machine automatically:".to_string(),
            String::new(),
        ];
        if actions.create_dirs {
            lines.push("• create missing state/project/template/log directories".into());
        }
        if actions.init_config {
            lines.push("• write the default ~/.config/hegn/config.toml".into());
        }
        if actions.init_profiles {
            lines
                .push("• write per-harness profile templates under ~/.config/hegn/profiles".into());
        }
        if actions.download_images {
            lines.push("• download and SHA-256 verify the VM kernel and rootfs".into());
        }
        if actions.configure_network {
            lines.push(
                "• create the guarded TAP pool and host nftables rules (asks for sudo)".into(),
            );
        }
        lines.push(String::new());
        lines.push("Continue?".into());
        self.modal = Some(Modal::Confirm(ConfirmPrompt {
            title: "First-time setup".into(),
            message: lines.join("\n"),
            confirm_label: "Set up".into(),
            action: ConfirmAction::SetupEnvironment,
            scroll: 0,
        }));
        Vec::new()
    }

    /// Install a fresh doctor report, e.g. after a background image download.
    pub fn show_doctor(&mut self, report: DoctorReport, note: Option<String>) {
        if let Some(note) = note {
            let level = if note.contains("failed") {
                ToastLevel::Error
            } else {
                ToastLevel::Info
            };
            self.toast(note, level);
        }
        self.modal = Some(Modal::Doctor(DoctorModal::new(report)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn failures_open_the_doctor_modal_and_d_requests_a_download() {
        let (dir, mut app) = test_app();
        app.config.kernel_image = dir.path().join("missing-kernel");
        app.config.rootfs_image = dir.path().join("missing-rootfs");
        app.fail_start("missing VM assets");
        match &app.modal {
            Some(Modal::Doctor(modal)) => {
                assert!(!modal.report.ok);
                assert!(modal.report.can_download());
                assert!(
                    modal
                        .error
                        .as_deref()
                        .is_some_and(|e| e.contains("missing VM"))
                );
            }
            other => panic!("expected doctor modal, got {other:?}"),
        }
        let effects = app.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(effects.as_slice(), [Effect::DownloadImages]));
        assert!(matches!(
            app.modal,
            Some(Modal::Doctor(DoctorModal {
                downloading: true,
                ..
            }))
        ));

        app.handle_key(key(KeyCode::Esc));
        app.handle_action(ActionId::Doctor);
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));
    }

    #[test]
    fn doctor_setup_requires_explicit_confirmation() {
        let (_dir, mut app) = test_app();
        app.config.kernel_image = app.config.projects_root.join("missing-kernel");
        app.config.rootfs_image = app.config.projects_root.join("missing-rootfs");
        app.fail_start("missing VM assets");
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));

        // The setup key opens a confirmation; nothing runs yet.
        let effects = app.handle_key(key(KeyCode::Char('s')));
        assert!(effects.is_empty());
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));

        // Enter must not confirm; `y` is explicit.
        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));
        let effects = app.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(effects.as_slice(), [Effect::SetupEnvironment]));
    }
}
