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

pub mod model;
pub mod status_source;

#[cfg(test)]
mod tests;

pub use status_source::{
    EngineStatusBridge, FakeStatusSource, ForwardState, ForwardStatus, ForwardStatusSource,
};

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::model::{ForwardRule, SessionConfig};
use crate::ui::widgets::{set_margins, Modal};

use model::{
    drafts_from_rules, lookup_status, rules_from_drafts, status_text, ForwardRuleDraft,
    FORWARD_KIND_LABELS, HELP_ENTRIES, KIND_INDEX_LOCAL, V2_NOTE,
};

/// The window action id, registered in `main_window.rs` like the other
/// `win.*` actions.
pub const TUNNELS_ACTION: &str = "tunnels";

/// One dialog row. Positions never shift: removing hides the `ListBoxRow`
/// and marks it, so a status update's `rule_index` always finds its row.
/// (`list_row` itself needs no handle past construction: the `ListBox` owns
/// it and the remove button holds its own clone.)
struct RuleRow {
    kind: gtk4::DropDown,
    bind: gtk4::Entry,
    bind_port: gtk4::Entry,
    remote: gtk4::Entry,
    remote_port: gtk4::Entry,
    enabled: gtk4::CheckButton,
    status: gtk4::Label,
    removed: Rc<Cell<bool>>,
}

impl RuleRow {
    fn draft(&self) -> Option<ForwardRuleDraft> {
        if self.removed.get() {
            return None;
        }
        Some(ForwardRuleDraft {
            kind_index: self.kind.selected(),
            bind_addr: self.bind.text().into(),
            bind_port: self.bind_port.text().into(),
            remote_host: self.remote.text().into(),
            remote_port: self.remote_port.text().into(),
            enabled: self.enabled.is_active(),
        })
    }
}

/// Add-row context: everything [`add_row`] needs beyond the draft.
struct RowCtx {
    online: bool,
    source: Option<Arc<dyn ForwardStatusSource>>,
    snapshot: Vec<ForwardStatus>,
}

/// The dialog. Build with [`Self::new`], then [`Self::run`] it.
pub struct TunnelsDialog {
    modal: Modal<Vec<ForwardRule>>,
    rows: Rc<RefCell<Vec<RuleRow>>>,
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
        let ctx = RowCtx {
            online: source.is_some(),
            snapshot: source.as_ref().map(|s| s.snapshot()).unwrap_or_default(),
            source,
        };
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
            .visible(!ctx.online)
            .build();
        let v2 = dim_label(V2_NOTE);

        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::None);
        let scroll = gtk4::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hexpand(true)
            .has_frame(true)
            .min_content_height(220)
            .build();

        let rows: Rc<RefCell<Vec<RuleRow>>> = Rc::default();
        for draft in drafts_from_rules(session.forward_rules.as_ref()) {
            add_row(&list, &rows, &draft, &ctx);
        }

        let error = gtk4::Label::builder()
            .halign(gtk4::Align::Start)
            .wrap(true)
            .build();
        error.add_css_class("error");

        // Live updates (the tab.rs pattern): the source broadcasts into
        // this drain, which paints the row's status cell on the GTK thread.
        // `recv().await` never blocks the UI; the drain ends with the dialog.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if let Some(source) = &ctx.source {
            source.subscribe(tx);
        }
        let for_status = rows.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Some(update) = rx.recv().await {
                if let Some(row) = for_status.borrow().get(update.rule_index) {
                    if !row.removed.get() {
                        row.status.set_text(&status_text(Some(&update.state), true));
                    }
                }
            }
        });

        let add = gtk4::Button::with_label("Add rule");
        {
            let (list, rows, online, source) =
                (list.clone(), rows.clone(), ctx.online, ctx.source.clone());
            add.connect_clicked(move |_| {
                let ctx = RowCtx {
                    online,
                    source: source.clone(),
                    snapshot: Vec::new(),
                };
                add_row(&list, &rows, &ForwardRuleDraft::default(), &ctx);
            });
        }

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
            rows,
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
            let (window, result, rows, error) = (
                self.modal.window.clone(),
                self.modal.result.clone(),
                self.rows.clone(),
                self.error.clone(),
            );
            ok.connect_clicked(move |_| {
                let drafts: Vec<ForwardRuleDraft> =
                    rows.borrow().iter().filter_map(RuleRow::draft).collect();
                match rules_from_drafts(&drafts) {
                    Ok(rules) => {
                        *result.borrow_mut() = Some(rules);
                        window.close();
                    }
                    Err(errors) => {
                        error.set_text(&format_errors(&errors));
                    }
                }
            });
            self.modal.window.set_default_widget(Some(&ok));
        }
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        buttons.set_halign(gtk4::Align::End);
        buttons.append(&cancel);
        buttons.append(&ok);
        self.vbox.append(&buttons);
        self.modal.run()
    }
}

/// The dialog-level error line: each failing row's message, prefixed by its
/// 1-based row number.
fn format_errors(errors: &[Option<String>]) -> String {
    errors
        .iter()
        .enumerate()
        .filter_map(|(index, error)| error.as_ref().map(|e| format!("Row {}: {e}", index + 1)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn add_row(
    list: &gtk4::ListBox,
    rows: &Rc<RefCell<Vec<RuleRow>>>,
    draft: &ForwardRuleDraft,
    ctx: &RowCtx,
) {
    let rule_index = rows.borrow().len();
    let online = ctx.online;
    let kind = gtk4::DropDown::builder()
        .model(&gtk4::StringList::new(&FORWARD_KIND_LABELS))
        .tooltip_text(V2_NOTE)
        .width_request(130)
        .build();
    kind.set_selected(draft.kind_index);
    let bind = row_entry(&draft.bind_addr, "bind addr", 130);
    let bind_port = row_entry(&draft.bind_port, "bind port", 80);
    let remote = row_entry(&draft.remote_host, "remote host", 150);
    let remote_port = row_entry(&draft.remote_port, "remote port", 80);
    let enabled = gtk4::CheckButton::with_label("Enabled");
    enabled.set_active(draft.enabled);
    let status = gtk4::Label::new(Some(&status_text(
        lookup_status(&ctx.snapshot, rule_index),
        online,
    )));
    status.set_width_request(150);
    status.set_xalign(0.0);
    let v2_note = dim_label("v2 — never starts");
    v2_note.set_tooltip_text(Some(V2_NOTE));

    let is_local = kind.selected() == KIND_INDEX_LOCAL;
    v2_note.set_visible(!is_local);
    enabled.set_sensitive(online && is_local);
    if !online {
        kind.set_sensitive(false);
    }

    // A kind change re-arms the v2 note and the engine control: only Local
    // rows can drive a listener (doc 11).
    let (v2_for_kind, enabled_for_kind) = (v2_note.clone(), enabled.clone());
    kind.connect_notify_local(Some("selected"), move |kind, _| {
        let is_local = kind.selected() == KIND_INDEX_LOCAL;
        v2_for_kind.set_visible(!is_local);
        enabled_for_kind.set_sensitive(online && is_local);
    });

    // Live application: flipping Enabled starts/stops that rule's listener
    // through the source; the resulting state paints via the subscription.
    if let Some(source) = ctx.source.clone() {
        enabled.connect_toggled(move |button| {
            source.set_rule_enabled(rule_index, button.is_active());
        });
    }

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    set_margins(&hbox, 6);
    hbox.append(&kind);
    hbox.append(&bind);
    hbox.append(&bind_port);
    hbox.append(&remote);
    hbox.append(&remote_port);
    hbox.append(&enabled);
    hbox.append(&status);
    hbox.append(&v2_note);
    let remove = gtk4::Button::with_label("Remove");
    hbox.append(&remove);
    let list_row = gtk4::ListBoxRow::builder().child(&hbox).build();
    list.append(&list_row);

    let removed = Rc::new(Cell::new(false));
    {
        let (list_row, removed) = (list_row.clone(), removed.clone());
        remove.connect_clicked(move |_| {
            removed.set(true);
            list_row.set_visible(false);
        });
    }

    rows.borrow_mut().push(RuleRow {
        kind,
        bind,
        bind_port,
        remote,
        remote_port,
        enabled,
        status,
        removed,
    });
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

fn row_entry(text: &str, placeholder: &str, width: i32) -> gtk4::Entry {
    gtk4::Entry::builder()
        .text(text)
        .placeholder_text(placeholder)
        .width_request(width)
        .build()
}

fn dim_label(text: &str) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .label(text)
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    label.add_css_class("dim-label");
    label
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;
    use crate::store::model::ForwardKind;
    use model::KIND_INDEX_REMOTE;

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
        assert_eq!(dialog.rows.borrow().len(), 2);
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
        for row in dialog.rows.borrow().iter() {
            assert_eq!(row.status.text(), model::OFFLINE_STATUS);
            assert!(!row.enabled.is_sensitive(), "offline: no engine control");
        }
        assert!(!dialog.rows.borrow()[0].kind.is_sensitive());
        dialog.modal.window.close();

        // Online through the fake: the snapshot paints, the toggle reaches
        // the source, and a broadcast repaints the cell.
        let source = Arc::new(FakeStatusSource::new(vec![
            ForwardState::Listening,
            ForwardState::Stopped,
        ]));
        let dialog = TunnelsDialog::new(&parent, &session, Some(source.clone()));
        assert_eq!(dialog.rows.borrow()[0].status.text(), "Listening");
        assert_eq!(dialog.rows.borrow()[1].status.text(), "Stopped");
        // The Remote row never arms an engine control, even online; its
        // kind still reads Remote. The Local row does arm one.
        assert!(!dialog.rows.borrow()[1].enabled.is_sensitive());
        assert_eq!(dialog.rows.borrow()[1].kind.selected(), KIND_INDEX_REMOTE);
        assert!(dialog.rows.borrow()[0].enabled.is_sensitive());

        dialog.rows.borrow()[0].enabled.set_active(false);
        assert_eq!(source.toggles(), vec![(0, false)]);
        // The toggle's echo arrives on the drain; spin the main loop once.
        while glib::MainContext::default().iteration(false) {}
        assert_eq!(dialog.rows.borrow()[0].status.text(), "Stopped");

        source.emit(ForwardStatus {
            rule_index: 0,
            state: ForwardState::Active { conns: 2 },
        });
        while glib::MainContext::default().iteration(false) {}
        assert_eq!(dialog.rows.borrow()[0].status.text(), "2 connections");
        dialog.modal.window.close();
    }
}
