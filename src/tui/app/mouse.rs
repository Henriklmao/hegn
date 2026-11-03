//! Mouse handling: footer clicks, row selection, scrolling and scrollbar drags.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::config::form::FormOutcome;

use super::util::rect_contains;
use super::*;

/// Maximum delay between two clicks on the same row to count as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(450);

impl App {
    /// Mouse handling for footer clicks, row selection and scrolling.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Vec<Effect> {
        match mouse.kind {
            MouseEventKind::Moved => {
                let pos = (mouse.column, mouse.row);
                self.hovered = self
                    .footer_hits
                    .iter()
                    .find(|hit| rect_contains(hit.rect, pos))
                    .map(|hit| hit.action);
                self.config_hover = self
                    .config_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .map(|row| row.index);
                self.scroll_hover = self.scroll_dragging
                    || self
                        .modal_scroll
                        .is_some_and(|hit| rect_contains(hit.rect, pos));
                Vec::new()
            }
            MouseEventKind::ScrollUp => {
                if let Some(Modal::Config(form)) = self.modal.as_mut() {
                    form.select_delta(-1);
                } else if let Some(Modal::CreateProject(form)) = self.modal.as_mut() {
                    form.package_cursor = form.package_cursor.saturating_sub(1);
                } else if self.modal.is_some() {
                    self.scroll_modal(-3);
                } else if self.screen == Screen::Project
                    && self.project_view != ProjectView::Compact
                {
                    self.scroll = self.scroll.saturating_add(3).min(self.terminal.len());
                } else {
                    self.select_next(-1);
                }
                Vec::new()
            }
            MouseEventKind::ScrollDown => {
                if let Some(Modal::Config(form)) = self.modal.as_mut() {
                    form.select_delta(1);
                } else if let Some(Modal::CreateProject(form)) = self.modal.as_mut() {
                    form.package_cursor =
                        (form.package_cursor + 1).min(form.packages.len().saturating_sub(1));
                } else if self.modal.is_some() {
                    self.scroll_modal(3);
                } else if self.screen == Screen::Project
                    && self.project_view != ProjectView::Compact
                {
                    self.scroll = self.scroll.saturating_sub(3);
                } else {
                    self.select_next(1);
                }
                Vec::new()
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.scroll_dragging {
                    self.scroll_hover = true;
                    self.jump_modal_scroll(mouse.row);
                }
                Vec::new()
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.scroll_dragging = false;
                Vec::new()
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = (mouse.column, mouse.row);
                if let Some(hit) = self
                    .footer_hits
                    .iter()
                    .find(|hit| rect_contains(hit.rect, pos))
                    .copied()
                {
                    return self.handle_action(hit.action);
                }
                if let Some(hit) = self.modal_scroll
                    && rect_contains(hit.rect, pos)
                {
                    self.scroll_dragging = true;
                    self.scroll_hover = true;
                    self.jump_modal_scroll(pos.1);
                    return Vec::new();
                }
                if matches!(self.modal, Some(Modal::Config(_))) {
                    return self.click_config_row(pos);
                }
                if self
                    .terminal_strip_hit
                    .is_some_and(|rect| rect_contains(rect, pos))
                {
                    return self.open_terminal();
                }
                if self
                    .terminal_hit
                    .is_some_and(|rect| rect_contains(rect, pos))
                {
                    self.project_view = ProjectView::Split;
                    self.terminal_focus = true;
                    self.terminal_mode = TerminalMode::Normal;
                    return Vec::new();
                }
                if let Some(row) = self
                    .project_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .copied()
                {
                    self.focus = Focus::Projects;
                    self.selected = row.index;
                    self.refresh_instance_filter();
                    let path = self.selected_project().map(|p| p.path.clone());
                    if let Some(path) = path {
                        let now = Instant::now();
                        let double = self.last_click.as_ref().is_some_and(|(at, p)| {
                            p == &path && now.duration_since(*at) < DOUBLE_CLICK
                        });
                        self.last_click = Some((now, path));
                        if double {
                            return self.open_selected();
                        }
                    }
                }
                if let Some(row) = self
                    .instance_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .copied()
                {
                    self.focus = Focus::Instances;
                    self.instance_selected = row.index;
                    let path = self
                        .selected_instance()
                        .map(|instance| instance.config_path.clone());
                    if let Some(path) = path {
                        let now = Instant::now();
                        let double = self.last_click.as_ref().is_some_and(|(at, p)| {
                            p == &path && now.duration_since(*at) < DOUBLE_CLICK
                        });
                        self.last_click = Some((now, path));
                        if double {
                            return self.attach_selected_instance();
                        }
                    }
                }
                if let Some(row) = self
                    .snapshot_rows
                    .iter()
                    .find(|row| rect_contains(row.rect, pos))
                    .copied()
                {
                    self.focus = Focus::Snapshots;
                    if self.screen == Screen::Project {
                        self.project_snapshot_selected = row.index;
                    } else {
                        self.selected_snapshot = row.index;
                    }
                }
                if let Some(hit) = self
                    .section_hits
                    .iter()
                    .find(|hit| rect_contains(hit.rect, pos))
                    .copied()
                {
                    self.screen = Screen::Project;
                    self.focus = hit.section.focus();
                    if self.project_view != ProjectView::Compact {
                        self.project_view = ProjectView::Section;
                        self.terminal_focus = false;
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Click inside the settings modal: steppers change a value, a row click
    /// selects and activates it (toggle, cycle, step or inline edit).
    pub(super) fn click_config_row(&mut self, pos: (u16, u16)) -> Vec<Effect> {
        let tab = self
            .config_tabs
            .iter()
            .find(|hit| rect_contains(hit.rect, pos))
            .map(|hit| hit.page);
        if let Some(page) = tab
            && let Some(Modal::Config(form)) = self.modal.as_mut()
        {
            form.set_page(page);
            return Vec::new();
        }
        let stepper = self
            .config_steppers
            .iter()
            .find(|hit| rect_contains(hit.rect, pos))
            .copied();
        let row = self
            .config_rows
            .iter()
            .find(|row| rect_contains(row.rect, pos))
            .copied();
        let outcome = {
            let Some(Modal::Config(form)) = self.modal.as_mut() else {
                return Vec::new();
            };
            if let Some(stepper) = stepper {
                form.adjust_field(stepper.field, stepper.step);
                return Vec::new();
            }
            if let Some(row) = row
                && form.select_row(row.index)
            {
                form.click()
            } else {
                FormOutcome::Stay
            }
        };
        match outcome {
            FormOutcome::OpenProfile(harness) => {
                self.modal = None;
                vec![Effect::OpenProfileEditor(harness)]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::test_support::*;

    #[test]
    fn footer_buttons_track_mouse_hover() {
        let (_dir, mut app) = test_app();
        app.footer_hits = vec![FooterHit {
            rect: Rect::new(4, 10, 8, 1),
            action: ActionId::Open,
        }];
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 6,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.hovered, Some(ActionId::Open));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 40,
            row: 20,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.hovered, None);

        app.hovered = Some(ActionId::Open);
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.hovered, None, "keyboard input clears the hover state");
    }

    fn click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn double_clicking_a_project_row_opens_it() {
        let (_dir, mut app) = test_app();
        let path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        app.set_projects(vec![Project {
            name: "demo".into(),
            path: path.clone(),
            git: GitSummary::default(),
            last_opened: None,
        }]);
        app.project_rows = vec![RowHit {
            rect: Rect::new(0, 2, 20, 1),
            index: 0,
        }];

        app.handle_mouse(click(1, 2));
        assert_eq!(app.screen, Screen::Dashboard, "one click only selects");
        assert_eq!(app.selected_project().unwrap().name, "demo");

        app.handle_mouse(click(1, 2));
        assert_eq!(app.screen, Screen::Project);
        assert_eq!(app.open_project.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn double_clicking_an_instance_asks_to_attach() {
        let (_dir, mut app) = test_app();
        let inside = app.config.instances_dir.join("demo").join("workspace");
        let instance = fake_instance(&app, "demo", inside);
        install_instances(&mut app, vec![instance]);
        app.screen = Screen::Project;
        app.instance_rows = vec![RowHit {
            rect: Rect::new(0, 4, 20, 1),
            index: 0,
        }];

        assert!(app.handle_mouse(click(1, 4)).is_empty());
        let effects = app.handle_mouse(click(1, 4));

        assert!(matches!(
            effects.as_slice(),
            [Effect::AttachInstance { instance }] if instance == "demo"
        ));
        assert_eq!(app.instance.state, InstanceState::Starting);
    }

    #[test]
    fn section_clicks_focus_the_section_and_expand_only_when_split() {
        let (_dir, mut app) = test_app();
        app.screen = Screen::Project;
        app.section_hits = vec![SectionHit {
            rect: Rect::new(0, 1, 12, 1),
            section: ProjectSection::Snapshots,
        }];
        app.project_view = ProjectView::Split;

        app.handle_mouse(click(1, 1));

        assert_eq!(app.focus, Focus::Snapshots);
        assert_eq!(app.project_view, ProjectView::Section);
        assert!(!app.terminal_focus);

        app.project_view = ProjectView::Compact;
        app.focus = Focus::Instances;
        app.handle_mouse(click(1, 1));
        assert_eq!(app.focus, Focus::Snapshots);
        assert_eq!(app.project_view, ProjectView::Compact);
    }

    #[test]
    fn clicking_the_terminal_pane_returns_to_normal_mode() {
        let (_dir, mut app) = test_app();
        app.attach_console(Box::new(FakeConsole::new("/projects/demo")));
        app.project_view = ProjectView::Compact;
        app.terminal_focus = false;
        app.terminal_mode = TerminalMode::Insert;
        app.terminal_hit = Some(Rect::new(40, 0, 30, 20));

        app.handle_mouse(click(45, 5));

        assert_eq!(app.project_view, ProjectView::Split);
        assert!(app.terminal_focus);
        assert_eq!(
            app.terminal_mode,
            TerminalMode::Normal,
            "a mouse focus change must not leave the guest in insert passthrough"
        );
    }

    #[test]
    fn clicking_delete_project_opens_a_confirmation_not_a_deletion() {
        let (_dir, mut app) = test_app();
        let path = app.config.projects_root.join("demo");
        std::fs::create_dir_all(&path).unwrap();
        app.projects = vec![Project {
            name: "demo".into(),
            path,
            git: GitSummary::default(),
            last_opened: None,
        }];
        app.visible = vec![0];
        app.selected = 0;
        app.footer_hits = vec![FooterHit {
            rect: Rect::new(0, 30, 10, 1),
            action: ActionId::DeleteProject,
        }];

        let effects = app.handle_mouse(click(1, 30));

        assert!(effects.is_empty());
        assert!(matches!(app.modal, Some(Modal::DeleteProject(_))));
    }

    #[test]
    fn scrolling_the_split_view_moves_the_terminal_not_the_selection() {
        let (_dir, mut app) = test_app();
        app.set_projects(vec![fake_project("a"), fake_project("b")]);
        app.screen = Screen::Project;
        app.project_view = ProjectView::Split;
        app.terminal.push_bytes(b"one\ntwo\nthree\n");
        app.scroll = 0;
        app.selected = 0;

        let scroll = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(scroll(MouseEventKind::ScrollUp));
        assert!(app.scroll > 0);
        assert_eq!(
            app.selected, 0,
            "terminal scrolling must not move the picker"
        );
        app.handle_mouse(scroll(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll, 0);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn mouse_scroll_moves_selection() {
        let (_dir, mut app) = test_app();
        app.set_projects(vec![fake_project("a"), fake_project("b")]);
        let event = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(event);
        assert_eq!(app.selected, 1);
    }
}
