//! Shared modal behaviour: destructive confirmations and the scrollbar offsets
//! recorded by the renderer.

use crossterm::event::{KeyCode, KeyEvent};

use crate::session::snapshot::SnapshotStore;

use super::*;

impl App {
    pub(super) fn handle_confirm_key(
        &mut self,
        key: KeyEvent,
        prompt: ConfirmPrompt,
    ) -> Vec<Effect> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.modal = Some(Modal::Confirm(prompt));
                self.scroll_modal(1);
                return Vec::new();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.modal = Some(Modal::Confirm(prompt));
                self.scroll_modal(-1);
                return Vec::new();
            }
            KeyCode::PageDown => {
                self.modal = Some(Modal::Confirm(prompt));
                self.scroll_modal_page(1);
                return Vec::new();
            }
            KeyCode::PageUp => {
                self.modal = Some(Modal::Confirm(prompt));
                self.scroll_modal_page(-1);
                return Vec::new();
            }
            _ => {}
        }
        // Destructive confirmation must be explicit: Enter never confirms.
        let confirmed = matches!(key.code, KeyCode::Char('y'));
        let cancelled = matches!(key.code, KeyCode::Char('n') | KeyCode::Esc);
        if cancelled {
            if let ConfirmAction::ApplyVm(vm) = prompt.action {
                self.pending_apply.insert(vm.clone());
                self.toast(
                    format!("VM {vm} needs apply; run hegn vm apply {vm}"),
                    ToastLevel::Warn,
                );
            }
            return Vec::new();
        }
        if !confirmed {
            self.modal = Some(Modal::Confirm(prompt));
            return Vec::new();
        }
        match prompt.action {
            ConfirmAction::DeleteSnapshot(name) => {
                match SnapshotStore::new(&self.config.snapshots_dir).delete(&name) {
                    Ok(()) => {
                        self.toast(format!("deleted snapshot {name}"), ToastLevel::Info);
                        self.refresh_snapshots();
                    }
                    Err(err) => self.toast(format!("delete failed: {err}"), ToastLevel::Error),
                }
                Vec::new()
            }
            ConfirmAction::RestoreSnapshot(snapshot) => vec![Effect::RestoreSnapshot(*snapshot)],
            ConfirmAction::SetupEnvironment => vec![Effect::SetupEnvironment],
            ConfirmAction::ApplyVm(vm) => {
                self.pending_apply.insert(vm.clone());
                vec![Effect::ApplyVm { vm }]
            }
        }
    }

    /// Scroll offset of the open modal, if its body scrolls independently.
    pub(super) fn modal_scroll_offset(&self) -> Option<usize> {
        match &self.modal {
            Some(Modal::Help(help)) => Some(help.scroll),
            Some(Modal::Doctor(modal)) => Some(modal.scroll),
            Some(Modal::Security(prompt)) => Some(prompt.scroll),
            Some(Modal::Confirm(prompt)) => Some(prompt.scroll),
            Some(Modal::DeleteVm(modal)) => Some(modal.scroll),
            Some(Modal::DeleteProject(modal)) => Some(modal.scroll),
            Some(Modal::DetachProject(modal)) => Some(modal.scroll),
            Some(Modal::Error { scroll, .. }) => Some(*scroll),
            _ => None,
        }
    }

    pub(super) fn set_modal_scroll(&mut self, offset: usize) {
        match self.modal.as_mut() {
            Some(Modal::Help(help)) => help.scroll = offset,
            Some(Modal::Doctor(modal)) => modal.scroll = offset,
            Some(Modal::Security(prompt)) => prompt.scroll = offset,
            Some(Modal::Confirm(prompt)) => prompt.scroll = offset,
            Some(Modal::DeleteVm(modal)) => modal.scroll = offset,
            Some(Modal::DeleteProject(modal)) => modal.scroll = offset,
            Some(Modal::DetachProject(modal)) => modal.scroll = offset,
            Some(Modal::Error { scroll, .. }) => *scroll = offset,
            _ => {}
        }
    }

    /// Move the open modal's body by `delta` lines, clamped to its track.
    pub(super) fn scroll_modal(&mut self, delta: isize) {
        let Some(hit) = self.modal_scroll else { return };
        let Some(current) = self.modal_scroll_offset() else {
            return;
        };
        let current = current.min(hit.max);
        self.set_modal_scroll(current.saturating_add_signed(delta).min(hit.max));
    }

    pub(super) fn scroll_modal_page(&mut self, pages: isize) {
        let Some(hit) = self.modal_scroll else { return };
        let step = hit.page.max(1) as isize;
        self.scroll_modal(step * pages);
    }

    /// Map a click or drag row on the scrollbar to an offset. The settings form
    /// and the create form follow their selection, so the bar moves the
    /// selection instead of a free offset.
    pub(super) fn jump_modal_scroll(&mut self, row: u16) {
        let Some(hit) = self.modal_scroll else { return };
        let track = hit.rect.height.max(1) as usize;
        let relative = row.saturating_sub(hit.rect.y) as usize;
        let span = track.saturating_sub(1).max(1);
        match self.modal.as_mut() {
            Some(Modal::Config(form)) => {
                let rows = form.rows().len();
                if rows > 1 {
                    form.select_row(relative * (rows - 1) / span);
                }
            }
            Some(Modal::CreateProject(form)) => {
                let rows = form.packages.len();
                if rows > 1 {
                    form.package_cursor = relative * (rows - 1) / span;
                    form.focus = CreateFocus::Packages;
                }
            }
            Some(Modal::CreateVm(form)) => {
                let rows = form.packages.len();
                if rows > 1 {
                    form.package_cursor = relative * (rows - 1) / span;
                    form.focus = CreateVmFocus::Packages;
                }
            }
            _ => {
                let offset = relative * hit.max / span;
                self.set_modal_scroll(offset.min(hit.max));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    fn help_scroll(app: &mut App) -> usize {
        match &app.modal {
            Some(Modal::Help(help)) => help.scroll,
            other => panic!("expected the help modal, got {other:?}"),
        }
    }

    #[test]
    fn apply_prompt_confirms_with_y_and_marks_the_vm_pending() {
        let (_dir, mut app) = test_app();
        app.prompt_apply("rust");
        assert!(matches!(app.modal, Some(Modal::Confirm(_))));
        assert!(!app.apply_pending("rust"));

        let effects = app.handle_key(key(KeyCode::Char('y')));
        assert!(
            matches!(effects.as_slice(), [Effect::ApplyVm { vm }] if vm == "rust"),
            "{effects:?}"
        );
        assert!(app.modal.is_none());
        assert!(app.apply_pending("rust"));
    }

    #[test]
    fn cancelling_the_apply_prompt_keeps_the_vm_pending() {
        let (_dir, mut app) = test_app();
        app.prompt_apply("rust");
        let effects = app.handle_key(key(KeyCode::Char('n')));
        assert!(effects.is_empty());
        assert!(app.modal.is_none());
        assert!(app.apply_pending("rust"));
        let toast = app.toast.as_ref().expect("expected a toast");
        assert_eq!(toast.level, ToastLevel::Warn);
        assert!(toast.text.contains("hegn vm apply rust"), "{}", toast.text);

        app.toast = None;
        app.prompt_apply("rust");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.apply_pending("rust"));
        assert!(app.toast.is_some(), "Esc cancels like n");
    }

    #[test]
    fn apply_prompt_yields_to_an_open_modal() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Help(HelpState::default()));
        app.prompt_apply("rust");
        assert!(matches!(app.modal, Some(Modal::Help(_))));
        assert!(app.apply_pending("rust"));
        let toast = app.toast.as_ref().expect("expected a toast");
        assert!(toast.text.contains("hegn vm apply rust"), "{}", toast.text);
    }

    #[test]
    fn help_scrolls_with_keys_and_clamps_to_the_track() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Help(HelpState::default()));
        app.modal_scroll = Some(ScrollHit {
            rect: Rect::new(10, 2, 1, 10),
            max: 6,
            page: 10,
            offset: 0,
        });

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(help_scroll(&mut app), 1);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(help_scroll(&mut app), 6, "clamped to the track maximum");
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(help_scroll(&mut app), 0);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(help_scroll(&mut app), 0, "cannot scroll above the top");
    }

    #[test]
    fn modal_scrollbar_hovers_and_drags() {
        let (_dir, mut app) = test_app();
        app.modal = Some(Modal::Help(HelpState::default()));
        app.modal_scroll = Some(ScrollHit {
            rect: Rect::new(40, 2, 1, 11),
            max: 10,
            page: 5,
            offset: 0,
        });
        let event = |kind, row| MouseEvent {
            kind,
            column: 40,
            row,
            modifiers: KeyModifiers::NONE,
        };

        app.handle_mouse(event(MouseEventKind::Moved, 4));
        assert!(app.scroll_hover, "the track highlights under the pointer");

        app.handle_mouse(event(MouseEventKind::Down(MouseButton::Left), 7));
        assert!(app.scroll_dragging);
        assert_eq!(help_scroll(&mut app), 5, "mid-track maps to mid-offset");

        app.handle_mouse(event(MouseEventKind::Drag(MouseButton::Left), 2));
        assert_eq!(help_scroll(&mut app), 0);

        app.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 2));
        assert!(!app.scroll_dragging);
    }
}
