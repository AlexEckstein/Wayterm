//! The Tunnels dialog (Phase 11): one row per forward rule of an SSH
//! session — kind, bind, remote, ENABLED toggle, and a LIVE STATUS cell
//! driven by a [`ForwardStatusSource`].
//!
//! Working fallback + seam: the dialog takes
//! `Option<Arc<dyn ForwardStatusSource>>`. With `None` (not connected, or
//! a non-SSH session) it still opens — editing rules offline is useful —
//! but every status cell reads "no active connection" and the controls
//! that need an engine stay insensitive. With `Some`, the enable toggle
//! live-applies through [`ForwardStatusSource::set_rule_enabled`] and
//! status updates arrive on a tokio unbounded channel drained by
//! `MainContext::spawn_local` (the `tab.rs` pattern): no blocking recv on
//! the UI path. Persistence happens on OK: [`TunnelsDialog::run`] returns the
//! validated rules and the caller writes them back through the store, so
//! the store roundtrip stays intact.
//!
//! Remote/Dynamic rows render with a v2 note and never arm an engine
//! control (doc 11 §Limitations); only Local rows start listeners.

pub mod editor;
pub mod model;
pub mod status_source;

#[cfg(test)]
mod tests;

pub use status_source::{EngineStatusBridge, FakeStatusSource, ForwardStatusSource};

use std::sync::Arc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::model::{ForwardRule, SessionConfig};
use crate::ui::widgets::{button_row, dim_label, error_label, set_margins, Modal};

use editor::{Live, RuleEditor};
use model::{drafts_from_rules, lookup_status, ForwardRuleDraft, HELP_ENTRIES, V2_NOTE};

/// The window action id, registered in `main_window.rs` like the other
/// `win.*` actions.
pub const TUNNELS_ACTION: &str = "tunnels";

/// The dialog. Build with [`Self::new`], then [`Self::run`] it.
pub struct TunnelsDialog {
    modal: Modal<Vec<ForwardRule>>,
    rules: RuleEditor,
    vbox: gtk4::Box,
    error: gtk4::Label,
}

impl TunnelsDialog {
    /// `source == None` is the offline fallback: rules editable, live
    /// cells read "no active connection", engine controls insensitive.
    pub fn new(
        parent: &gtk4::Window,
        session: &SessionConfig,
        source: Option<Arc<dyn ForwardStatusSource>>,
    ) -> Self {
        let online = source.is_some();
        let snapshot = source.as_ref().map(|s| s.snapshot()).unwrap_or_default();
        let modal = Modal::new(&format!("Tunnels — {}", session.name), Some(parent));
        modal.window.set_default_size(640, 420);

        let title = gtk4::Label::builder()
            .label(format!("Port forwards for “{}”", session.name))
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .build();
        let help = help_button();
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        header.append(&title);
        header.append(&help);
        let offline = gtk4::Label::builder()
            .label("No active connection — editing rules only; live status unavailable.")
            .halign(gtk4::Align::Start)
            .wrap(true)
            .visible(!online)
            .build();
        let v2 = dim_label(V2_NOTE);

        let rules = RuleEditor::new(Some(Live {
            source: source.clone(),
        }));
        let scroll = gtk4::ScrolledWindow::builder()
            .child(rules.widget())
            .vexpand(true)
            .hexpand(true)
            .has_frame(true)
            .min_content_height(220)
            .build();

        for (rule_id, draft) in drafts_from_rules(session.forward_rules.as_ref())
            .iter()
            .enumerate()
        {
            rules.add(draft, lookup_status(&snapshot, rule_id));
        }
        let error = error_label();

        // Live updates (the tab.rs pattern): the source broadcasts into
        // this drain, which paints the row's status cell on the GTK thread.
        // `recv().await` never blocks the UI; the drain ends with the dialog.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if let Some(source) = &source {
            source.subscribe(tx);
        }
        let for_status = rules.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Some(update) = rx.recv().await {
                if let Some(row) = for_status.rows().get(update.rule_id) {
                    row.show_status(&update.state);
                }
            }
        });

        let add = gtk4::Button::with_label("Add rule");
        let for_add = rules.clone();
        add.connect_clicked(move |_| for_add.add(&ForwardRuleDraft::default(), None));

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        set_margins(&vbox, 12);
        vbox.append(&header);
        vbox.append(&offline);
        vbox.append(&v2);
        vbox.append(&scroll);
        vbox.append(&add);
        vbox.append(&error);
        modal.window.set_child(Some(&vbox));

        Self {
            modal,
            rules,
            vbox,
            error,
        }
    }

    /// Run modally; `None` on cancel. `Some` is the validated rules to
    /// persist through the store. OK only closes on success, so inline
    /// errors keep the dialog open.
    pub fn run(self) -> Option<Vec<ForwardRule>> {
        let ok = gtk4::Button::with_label("OK");
        let cancel = self.modal.button("Cancel", || None);
        {
            let (window, result, rules, error) = (
                self.modal.window.clone(),
                self.modal.result.clone(),
                self.rules.clone(),
                self.error.clone(),
            );
            ok.connect_clicked(move |_| match rules.rules() {
                Ok(rules) => {
                    *result.borrow_mut() = Some(rules);
                    window.close();
                }
                Err(message) => error.set_text(&message),
            });
            self.modal.window.set_default_widget(Some(&ok));
        }
        self.vbox.append(&button_row(&[&cancel, &ok]));
        self.modal.run()
    }
}

/// The top-right help icon: a popover explaining each row control
/// ([`HELP_ENTRIES`]).
fn help_button() -> gtk4::MenuButton {
    let grid = gtk4::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    set_margins(&grid, 6);
    for (row, (term, explanation)) in (0..).zip(HELP_ENTRIES) {
        let term = gtk4::Label::builder()
            .label(term)
            .halign(gtk4::Align::Start)
            .valign(gtk4::Align::Start)
            .build();
        term.add_css_class("heading");
        let explanation = gtk4::Label::builder()
            .label(explanation)
            .halign(gtk4::Align::Start)
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(60)
            .build();
        grid.attach(&term, 0, row, 1, 1);
        grid.attach(&explanation, 1, row, 1, 1);
    }
    let popover = gtk4::Popover::builder().child(&grid).build();
    let button = gtk4::MenuButton::builder()
        .icon_name("dialog-question-symbolic")
        .tooltip_text("What do these options mean?")
        .popover(&popover)
        .valign(gtk4::Align::Center)
        .build();
    button.add_css_class("flat");
    button
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;
    use crate::store::model::ForwardKind;
    use model::KIND_INDEX_REMOTE;
    use status_source::{ForwardEvent, ForwardState};

    fn session_with_rules() -> SessionConfig {
        SessionConfig {
            id: 1,
            name: "web".to_string(),
            forward_rules: Some(vec![
                ForwardRule {
                    kind: ForwardKind::Local,
                    bind_addr: "127.0.0.1".to_string(),
                    bind_port: 8080,
                    remote_host: "web1.lan".to_string(),
                    remote_port: 80,
                    enabled: true,
                },
                ForwardRule {
                    kind: ForwardKind::Remote,
                    bind_addr: "0.0.0.0".to_string(),
                    bind_port: 2222,
                    remote_host: "db.lan".to_string(),
                    remote_port: 22,
                    enabled: false,
                },
            ]),
            ..Default::default()
        }
    }

    pub(in crate::ui) fn rows_status_and_toggle() {
        let parent = gtk4::Window::new();
        let session = session_with_rules();

        // Offline: every status cell is the fallback and engine controls
        // are insensitive, but the rules still render.
        let dialog = TunnelsDialog::new(&parent, &session, None);
        assert_eq!(dialog.rules.rows().len(), 2);
        // The help icon carries a popover explaining every row control.
        let help = dialog
            .vbox
            .first_child()
            .and_then(|header| header.last_child())
            .and_downcast::<gtk4::MenuButton>()
            .expect("help button ends the header row")
            .popover()
            .expect("help popover");
        let grid = help.child().and_downcast::<gtk4::Grid>().expect("grid");
        let first = grid.child_at(0, 0).and_downcast::<gtk4::Label>();
        assert_eq!(first.map(|l| l.text()).as_deref(), Some("Kind"));
        for row in dialog.rules.rows().iter() {
            assert_eq!(row.status.as_ref().unwrap().text(), model::OFFLINE_STATUS);
            assert!(!row.enabled.is_sensitive(), "offline: no engine control");
        }
        assert!(!dialog.rules.rows()[0].kind.is_sensitive());
        dialog.modal.window.close();

        // Online through the fake: the snapshot paints, the toggle reaches
        // the source, and a broadcast repaints the cell.
        let source = Arc::new(FakeStatusSource::new(vec![
            ForwardState::Listening,
            ForwardState::Stopped,
        ]));
        let dialog = TunnelsDialog::new(&parent, &session, Some(source.clone()));
        assert_eq!(
            dialog.rules.rows()[0].status.as_ref().unwrap().text(),
            "Listening"
        );
        assert_eq!(
            dialog.rules.rows()[1].status.as_ref().unwrap().text(),
            "Stopped"
        );
        // The Remote row never arms an engine control, even online; its
        // kind still reads Remote. The Local row does arm one.
        assert!(!dialog.rules.rows()[1].enabled.is_sensitive());
        assert_eq!(dialog.rules.rows()[1].kind.selected(), KIND_INDEX_REMOTE);
        assert!(dialog.rules.rows()[0].enabled.is_sensitive());

        dialog.rules.rows()[0].enabled.set_active(false);
        assert_eq!(source.toggles(), vec![(0, false)]);
        // The toggle's echo arrives on the drain; spin the main loop once.
        while glib::MainContext::default().iteration(false) {}
        assert_eq!(
            dialog.rules.rows()[0].status.as_ref().unwrap().text(),
            "Stopped"
        );

        source.emit(ForwardEvent {
            rule_id: 0,
            state: ForwardState::Active { conns: 2 },
        });
        while glib::MainContext::default().iteration(false) {}
        assert_eq!(
            dialog.rules.rows()[0].status.as_ref().unwrap().text(),
            "2 connections"
        );
        dialog.modal.window.close();
    }
}
