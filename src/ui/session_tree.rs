//! Session tree widget — read-only sidebar showing folders and saved
//! sessions (doc 01 / doc 10 Task 7).
//!
//! Split into a display-free data layer ([`SessionRow`], [`collect_rows`],
//! [`icon_for_session_type`]) and a thin GTK layer ([`SessionTree`]).
//! The data layer is unit-testable without a display.

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::model::SessionType;
use crate::store::SessionStore;

/// One flattened row in the displayed tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRow {
    pub id: i64,
    pub is_folder: bool,
    pub name: String,
    pub icon: &'static str,
    pub parent_id: Option<i64>,
}

/// Map a session type to its display icon.
///
/// Keep this in sync with the `SessionType` enum variants (doc 06).
pub fn icon_for_session_type(session_type: &SessionType) -> &'static str {
    match session_type {
        SessionType::SSH => "🖥",
        SessionType::RDP => "🖼",
        SessionType::VNC => "📺",
        SessionType::Serial => "🔌",
        SessionType::LocalShell => "🏠",
    }
}

/// Collect rows for the whole tree in depth-first display order.
///
/// Order within each parent: folders first (sorted by `sort_order` then
/// `name`), then sessions (sorted by `sort_order` then `name`). This is
/// the MobaXterm-style contract expected by the UI (doc 01).
pub fn collect_rows(store: &SessionStore) -> rusqlite::Result<Vec<SessionRow>> {
    let mut rows = Vec::new();
    collect_children(store, None, None, &mut rows)?;
    Ok(rows)
}

fn collect_children(
    store: &SessionStore,
    parent_id: Option<i64>,
    parent_row_id: Option<i64>,
    rows: &mut Vec<SessionRow>,
) -> rusqlite::Result<()> {
    let folders = store.list_folders(parent_id)?;
    let sessions = store.list_sessions(parent_id)?;

    for folder in folders {
        let folder_id = folder.id;
        rows.push(SessionRow {
            id: folder_id,
            is_folder: true,
            name: folder.name,
            icon: "📁",
            parent_id: parent_row_id,
        });
        collect_children(store, Some(folder_id), Some(folder_id), rows)?;
    }

    for session in sessions {
        rows.push(SessionRow {
            id: session.id,
            is_folder: false,
            name: session.name,
            icon: icon_for_session_type(&session.session_type),
            parent_id: parent_row_id,
        });
    }

    Ok(())
}

/// GTK tree view for the session sidebar.
///
/// Columns:
/// - 0: icon (`String`)
/// - 1: name (`String`)
/// - 2: id (`i64`, hidden)
/// - 3: is_folder (`bool`, hidden)
//
// GTK 4.10 deprecates TreeView/TreeStore in favor of GtkListView +
// GtkTreeListModel, but doc 01's session tree is written against
// TreeView and the plan (Task 7) specifies it. Revisit if/when the
// UI migrates to the list-based widgets (Phase 2+ UI polish).
#[allow(deprecated)]
pub struct SessionTree {
    store: SessionStore,
    tree_view: gtk4::TreeView,
    model: gtk4::TreeStore,
}

impl SessionTree {
    /// Build the tree view with an empty model.
    #[allow(deprecated)]
    pub fn new(store: SessionStore) -> Self {
        let model = gtk4::TreeStore::new(&[
            glib::Type::STRING,
            glib::Type::STRING,
            glib::Type::I64,
            glib::Type::BOOL,
        ]);

        let tree_view = gtk4::TreeView::builder()
            .model(&model)
            .headers_visible(false)
            .build();
        tree_view.set_vexpand(true);
        tree_view.set_hexpand(true);

        let icon_col = gtk4::TreeViewColumn::new();
        let icon_renderer = gtk4::CellRendererText::new();
        icon_col.pack_start(&icon_renderer, false);
        icon_col.add_attribute(&icon_renderer, "text", 0);
        tree_view.append_column(&icon_col);

        let name_col = gtk4::TreeViewColumn::new();
        let name_renderer = gtk4::CellRendererText::new();
        name_col.pack_start(&name_renderer, true);
        name_col.add_attribute(&name_renderer, "text", 1);
        tree_view.append_column(&name_col);

        let selection = tree_view.selection();
        selection.set_mode(gtk4::SelectionMode::Single);

        Self {
            store,
            tree_view,
            model,
        }
    }

    /// Reload the tree from the store.
    #[allow(deprecated)]
    pub fn refresh(&self) {
        self.model.clear();

        let rows = match collect_rows(&self.store) {
            Ok(r) => r,
            Err(_) => return, // Silent failure for now; Task 8 adds error UI.
        };

        let mut iter_for_id = std::collections::HashMap::<i64, gtk4::TreeIter>::new();

        for row in rows {
            let parent_iter = row.parent_id.and_then(|pid| iter_for_id.get(&pid).cloned());
            let iter = self.model.insert(parent_iter.as_ref(), -1);
            self.model.set(
                &iter,
                &[
                    (0, &row.icon),
                    (1, &row.name),
                    (2, &row.id),
                    (3, &row.is_folder),
                ],
            );
            if row.is_folder {
                iter_for_id.insert(row.id, iter);
            }
        }
    }

    /// Return the underlying tree view as a generic widget.
    pub fn widget(&self) -> gtk4::Widget {
        self.tree_view.clone().upcast()
    }
}

#[cfg(test)]
mod tests;
