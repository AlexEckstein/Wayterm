//! The forward-rule editor both dialogs embed — the Tunnels dialog and the
//! session dialog's SSH group — so they build rows, read drafts back and
//! report bad input the same way. Positions never shift: Remove hides a
//! row and marks it, so a status update's rule id always finds its row.

use std::cell::{Cell, Ref, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;

use super::model::{
    rules_from_drafts, status_text, ForwardRuleDraft, FORWARD_KIND_LABELS, KIND_INDEX_LOCAL,
    V2_NOTE,
};
use super::status_source::{ForwardState, ForwardStatusSource, RuleId};
use crate::store::model::ForwardRule;
use crate::ui::widgets::{dim_label, set_margins};

/// The Tunnels dialog's side of each row: a status cell, the unsupported-kind
/// note, and
/// an Enabled toggle that drives the engine through `source` — `None`
/// offline, which leaves the engine controls insensitive.
#[derive(Clone)]
pub(crate) struct Live {
    pub(crate) source: Option<Arc<dyn ForwardStatusSource>>,
}

/// One editable rule.
pub(crate) struct RuleRow {
    pub(crate) kind: gtk4::DropDown,
    bind: gtk4::Entry,
    bind_port: gtk4::Entry,
    remote: gtk4::Entry,
    remote_port: gtk4::Entry,
    pub(crate) enabled: gtk4::CheckButton,
    /// The live status cell (Tunnels dialog only).
    pub(crate) status: Option<gtk4::Label>,
    removed: Rc<Cell<bool>>,
}

impl RuleRow {
    /// The row as typed; `None` once removed.
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

    /// Show the engine's `state` for this rule, unless it was removed.
    pub(crate) fn show_status(&self, state: &ForwardState) {
        if let (Some(status), false) = (&self.status, self.removed.get()) {
            status.set_text(&status_text(Some(state), true));
        }
    }
}

/// The rows of one editor in a `ListBox`. Clones share them.
#[derive(Clone)]
pub(crate) struct RuleEditor {
    list: gtk4::ListBox,
    rows: Rc<RefCell<Vec<RuleRow>>>,
    live: Option<Live>,
}

impl RuleEditor {
    /// An empty editor; `live` adds the Tunnels dialog's cells to each row.
    pub(crate) fn new(live: Option<Live>) -> Self {
        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::None);
        Self {
            list,
            rows: Rc::default(),
            live,
        }
    }

    pub(crate) fn widget(&self) -> &gtk4::ListBox {
        &self.list
    }

    /// Every row ever added, in rule order, removed ones included.
    pub(crate) fn rows(&self) -> Ref<'_, Vec<RuleRow>> {
        self.rows.borrow()
    }

    /// The rows still listed, as typed.
    pub(crate) fn drafts(&self) -> Vec<ForwardRuleDraft> {
        self.rows
            .borrow()
            .iter()
            .filter_map(RuleRow::draft)
            .collect()
    }

    /// Every listed row as a rule, or the error line: each failing row's
    /// message, prefixed by its 1-based row number.
    pub(crate) fn rules(&self) -> Result<Vec<ForwardRule>, String> {
        rules_from_drafts(&self.drafts()).map_err(|errors| {
            errors
                .iter()
                .enumerate()
                .filter_map(|(index, error)| {
                    error.as_ref().map(|e| format!("Row {}: {e}", index + 1))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    /// Append a row for `draft`; `state` is what the engine last reported
    /// for it (live rows only). Only Local converts on save: Remote and
    /// Dynamic stay selectable so their refusal is visible.
    pub(crate) fn add(&self, draft: &ForwardRuleDraft, state: Option<&ForwardState>) {
        let rule_id = self.rows.borrow().len();
        let kind = gtk4::DropDown::builder()
            .model(&gtk4::StringList::new(&FORWARD_KIND_LABELS))
            .tooltip_text(V2_NOTE)
            .width_request(130)
            .build();
        kind.set_selected(draft.kind_index);
        let bind = entry(&draft.bind_addr, "bind addr", 120);
        let bind_port = entry(&draft.bind_port, "bind port", 70);
        let remote = entry(&draft.remote_host, "remote host", 130);
        let remote_port = entry(&draft.remote_port, "remote port", 70);
        let enabled =
            gtk4::CheckButton::with_label(if self.live.is_some() { "Enabled" } else { "On" });
        enabled.set_active(draft.enabled);

        let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        set_margins(&hbox, 4);
        hbox.append(&kind);
        hbox.append(&bind);
        hbox.append(&bind_port);
        hbox.append(&remote);
        hbox.append(&remote_port);
        hbox.append(&enabled);
        let status = match &self.live {
            Some(live) => Some(live_cells(&hbox, live, rule_id, &kind, &enabled, state)),
            None => {
                enabled.set_tooltip_text(Some("Enabled rules start at connect (Local only)"));
                None
            }
        };
        let remove = gtk4::Button::with_label("Remove");
        hbox.append(&remove);
        let list_row = gtk4::ListBoxRow::builder().child(&hbox).build();
        self.list.append(&list_row);

        let removed = Rc::new(Cell::new(false));
        {
            // Weak: the button lives inside the row.
            let (list_row, removed) = (list_row.downgrade(), removed.clone());
            remove.connect_clicked(move |_| {
                removed.set(true);
                if let Some(list_row) = list_row.upgrade() {
                    list_row.set_visible(false);
                }
            });
        }
        self.rows.borrow_mut().push(RuleRow {
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
}

/// Append the Tunnels dialog's cells to `hbox` and wire them: the status
/// cell, the unsupported-kind note, and the engine toggle, sensitive only for a Local
/// rule while online — re-armed whenever the kind changes (doc 04).
fn live_cells(
    hbox: &gtk4::Box,
    live: &Live,
    rule_id: RuleId,
    kind: &gtk4::DropDown,
    enabled: &gtk4::CheckButton,
    state: Option<&ForwardState>,
) -> gtk4::Label {
    let online = live.source.is_some();
    let status = gtk4::Label::new(Some(&status_text(state, online)));
    status.set_width_request(150);
    status.set_xalign(0.0);
    let v2_note = dim_label("v2 — never starts");
    v2_note.set_tooltip_text(Some(V2_NOTE));
    hbox.append(&status);
    hbox.append(&v2_note);

    let arm = {
        let enabled = enabled.clone();
        move |kind: &gtk4::DropDown| {
            let local = kind.selected() == KIND_INDEX_LOCAL;
            v2_note.set_visible(!local);
            enabled.set_sensitive(online && local);
        }
    };
    arm(kind);
    kind.set_sensitive(online);
    kind.connect_notify_local(Some("selected"), move |kind, _| arm(kind));
    // Live application: flipping Enabled starts/stops the rule's listener;
    // the resulting state paints through the dialog's subscription.
    if let Some(source) = live.source.clone() {
        enabled.connect_toggled(move |button| {
            source.set_rule_enabled(rule_id, button.is_active());
        });
    }
    status
}

fn entry(text: &str, placeholder: &str, width: i32) -> gtk4::Entry {
    gtk4::Entry::builder()
        .text(text)
        .placeholder_text(placeholder)
        .width_request(width)
        .build()
}
