//! Ratatui renderer. Pure functions over [`App`] make the UI testable with a
//! `TestBackend`.
//!
//! The renderer is split at screen and modal boundaries: [`header`] owns the
//! top bar, [`screens`] the dashboard and project layouts, [`modals`] the
//! overlay dialogs, and [`widgets`]/[`text`]/[`theme`] the shared building
//! blocks. This module only wires them together in [`draw`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::Block;

use crate::tui::app::{
    App, ConfigStepHit, ConfigTabHit, FooterHit, RowHit, Screen, ScrollHit, SectionHit,
};

mod header;
mod modals;
mod screens;
#[cfg(test)]
mod test_support;
mod text;
mod theme;
mod toast;
mod widgets;

#[derive(Debug, Default)]
pub struct UiHits {
    pub footer: Vec<FooterHit>,
    pub projects: Vec<RowHit>,
    pub instances: Vec<RowHit>,
    pub snapshots: Vec<RowHit>,
    /// Clickable project section headers/panels.
    pub sections: Vec<SectionHit>,
    /// Clickable terminal pane and collapsed terminal strip.
    pub terminal: Option<Rect>,
    pub terminal_strip: Option<Rect>,
    /// Settings rows and `‹ ›` steppers inside the configuration modal.
    pub config: Vec<RowHit>,
    pub config_steppers: Vec<ConfigStepHit>,
    pub config_tabs: Vec<ConfigTabHit>,
    /// Scrollbar track of the open modal, when its body overflows.
    pub modal_scroll: Option<ScrollHit>,
}

#[derive(Default)]
pub(crate) struct ModalHits {
    pub(crate) footer: Vec<FooterHit>,
    pub(crate) config: Vec<RowHit>,
    pub(crate) config_steppers: Vec<ConfigStepHit>,
    pub(crate) config_tabs: Vec<ConfigTabHit>,
    pub(crate) scroll: Option<ScrollHit>,
}

impl ModalHits {
    pub(crate) fn buttons(footer: Vec<FooterHit>) -> Self {
        Self {
            footer,
            ..Self::default()
        }
    }
}

pub fn draw(frame: &mut Frame, app: &App) -> UiHits {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default()), area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(area);

    header::render_header(frame, app, chunks[0]);
    let hits = match app.screen {
        Screen::Dashboard => {
            let rows = screens::dashboard::render_dashboard(frame, app, chunks[1]);
            RenderHits {
                projects: rows.0,
                instances: rows.1,
                snapshots: rows.2,
                ..RenderHits::default()
            }
        }
        Screen::Project => screens::project::render_project(frame, app, chunks[1]),
    };
    let mut footer = screens::footer::render_footer(frame, app, chunks[2]);
    let mut config = Vec::new();
    let mut config_steppers = Vec::new();
    let mut config_tabs = Vec::new();
    let mut modal_scroll = None;
    if let Some(modal) = &app.modal {
        let modal_hits = modals::render_modal(frame, app, modal, area);
        footer.extend(modal_hits.footer);
        config = modal_hits.config;
        config_steppers = modal_hits.config_steppers;
        config_tabs = modal_hits.config_tabs;
        modal_scroll = modal_hits.scroll;
    }
    toast::render_toast(frame, app, chunks[1]);

    UiHits {
        footer,
        projects: hits.projects,
        instances: hits.instances,
        snapshots: hits.snapshots,
        sections: hits.sections,
        terminal: hits.terminal,
        terminal_strip: hits.terminal_strip,
        config,
        config_steppers,
        config_tabs,
        modal_scroll,
    }
}

#[derive(Default)]
pub(crate) struct RenderHits {
    pub(crate) projects: Vec<RowHit>,
    pub(crate) instances: Vec<RowHit>,
    pub(crate) snapshots: Vec<RowHit>,
    pub(crate) sections: Vec<SectionHit>,
    pub(crate) terminal: Option<Rect>,
    pub(crate) terminal_strip: Option<Rect>,
}

#[cfg(test)]
mod tests {
    use crate::config::Harness;
    use crate::config::form::ConfigForm;
    use crate::tui::app::{CreateProjectForm, HelpState, Modal};
    use crate::tui::ui::test_support::*;

    #[test]
    fn footer_hint_row_is_visible() {
        let (_dir, app) = app();
        let (text, _) = render(&app);
        assert!(text.contains("/ filter"), "{text}");
    }

    #[test]
    fn tiny_terminals_render_modals_without_panicking() {
        let (_dir, mut app) = app();
        let path = app.config.state_dir.join("config.toml");
        for (width, height) in [(40, 10), (24, 8), (16, 6)] {
            app.modal = Some(Modal::Help(HelpState::default()));
            let _ = render_sized(&app, width, height);
            app.modal = Some(Modal::CreateProject(CreateProjectForm::new(Harness::Shell)));
            let _ = render_sized(&app, width, height);
            app.modal = Some(Modal::Config(ConfigForm::global(
                app.config.clone(),
                path.clone(),
            )));
            let _ = render_sized(&app, width, height);
        }
    }
}
