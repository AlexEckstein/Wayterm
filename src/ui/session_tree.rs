//! Session tree widget — sidebar showing folders and saved sessions
//! (doc 01 / doc 10 Tasks 7, 8 and 10).
//!
//! Split into a display-free data layer ([`SessionRow`], [`collect_rows`],
//! [`icon_for_session_type`]) and a thin GTK layer ([`SessionTree`]).
//! The data layer is unit-testable without a display. Folder and session
//! CRUD are exposed through a right-click context menu; all mutations go
//! through [`SessionStore`] and are followed by [`refresh()`](SessionTree::refresh).

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::model::SessionType;
use crate::store::SessionStore;
use crate::ui::session_dialog::SessionDialog;

// gtk4 re-exports these in its prelude, but the prelude import is not
// always enough for the compiler to resolve the free-function names in
// modern gtk4-rs; importing explicitly keeps the code unambiguous.
use gdk4 as gdk;
use gio;

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
    ///
    /// `on_open_session` is called when a session row is double-clicked
    /// (row-activated).  It receives the session id and the row's display
    /// name.  The caller (MainWindow) provides this callback so the tree
    /// stays decoupled from the notebook: the tree does not need to own or
    /// reference the tab container directly.
    #[allow(deprecated)]
    pub fn new<F>(store: SessionStore, on_open_session: F) -> Self
    where
        F: Fn(i64) + 'static,
    {
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

        // Task 11: double-click/row-activated on a session opens its tab.
        tree_view.connect_row_activated({
            let tree_view = tree_view.clone();
            move |_tree_view, _path, _column| {
                let (id, is_folder) = selected_row_info(&tree_view);
                if let Some(id) = id {
                    if !is_folder {
                        on_open_session(id);
                    }
                }
            }
        });

        let this = Self {
            store,
            tree_view: tree_view.clone(),
            model,
        };
        unsafe {
            tree_view.set_data("wayterm-session-tree", this.clone());
        }

        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        if let Ok(store) = this.store.duplicate() {
            gesture.connect_pressed({
                let tree_view = tree_view.clone();
                move |gesture, _n_press, x, y| {
                    let selection = tree_view.selection();
                    if let Some((Some(path), _, _, _)) = tree_view.path_at_pos(x as i32, y as i32) {
                        selection.select_path(&path);
                    } else {
                        selection.unselect_all();
                    }
                    if let Some(widget) = gesture.widget() {
                        show_context_menu(Some(&widget), &tree_view, &store, x, y);
                    }
                }
            });
        } else {
            eprintln!("Wayterm: failed to duplicate session store for context menu");
        }
        tree_view.add_controller(gesture);

        this
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

impl Clone for SessionTree {
    fn clone(&self) -> Self {
        let store = self.store.duplicate().unwrap_or_else(|e| {
            eprintln!("Wayterm: failed to duplicate session tree store: {e}");
            // Return an empty in-memory store as a safe fallback.  The tree
            // will render empty and refresh() will silently fail until the
            // underlying problem is resolved, rather than crashing the app.
            SessionStore::in_memory().expect("failed to create fallback store")
        });
        Self {
            store,
            tree_view: self.tree_view.clone(),
            model: self.model.clone(),
        }
    }
}

/// Display the context menu at the given pointer position.
///
/// `parent_widget` is the widget the popover will be attached to and the
/// coordinate space for `x`/`y` (the widget-relative click position from
/// the gesture). `tree_view` and `store` are the ones that receive the
/// menu actions.
///
/// The menu is anchored with `Popover::set_pointing_to()` at a zero-size
/// rectangle at the click point. Without it, GTK anchors a popover to the
/// parent widget's edge by default — for a full-height sidebar that puts
/// the menu above/below the window instead of at the cursor.
#[allow(deprecated)]
fn show_context_menu(
    parent_widget: Option<&gtk4::Widget>,
    tree_view: &gtk4::TreeView,
    store: &SessionStore,
    x: f64,
    y: f64,
) {
    let (selected_id, selected_is_folder) = selected_row_info(tree_view);
    let selected_is_session = selected_id.is_some_and(|_| !selected_is_folder);

    let menu = gio::Menu::new();
    menu.append(Some("New Folder"), Some("tree.new-folder"));

    if selected_is_folder {
        menu.append(Some("Rename Folder"), Some("tree.rename-folder"));
        menu.append(Some("Delete Folder"), Some("tree.delete-folder"));
    }

    menu.append(Some("New Session"), Some("tree.new-session"));

    if selected_is_session {
        menu.append(Some("Edit Session"), Some("tree.edit-session"));
        menu.append(Some("Delete Session"), Some("tree.delete-session"));
    }

    let popover = gtk4::PopoverMenu::from_model(Some(&menu));
    if let Some(parent) = parent_widget {
        popover.set_parent(parent);
        // Anchor the menu to the click position (widget-relative
        // coordinates, zero-size rectangle). Falls back to GTK's default
        // widget-edge anchoring when the parent is unknown.
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 0, 0)));
    }

    let action_group = gio::SimpleActionGroup::new();
    let new_folder = gio::SimpleAction::new("new-folder", None);
    let rename_folder = gio::SimpleAction::new("rename-folder", None);
    let delete_folder = gio::SimpleAction::new("delete-folder", None);
    let new_session = gio::SimpleAction::new("new-session", None);
    let edit_session = gio::SimpleAction::new("edit-session", None);
    let delete_session = gio::SimpleAction::new("delete-session", None);

    let store_new = store.duplicate().unwrap_or_else(|e| {
        eprintln!("Wayterm: failed to duplicate store for new-folder handler: {e}");
        SessionStore::in_memory().expect("failed to create fallback store")
    });
    let tree_view_new = tree_view.clone();
    new_folder.connect_activate(move |_, _| {
        let parent = gtk_window_for(&tree_view_new);
        let (folder_id, is_folder) = selected_row_info(&tree_view_new);
        let parent_folder_id = folder_id.filter(|_| is_folder);
        if let Some(name) = prompt_text(parent.as_ref(), "New Folder", "New Folder") {
            let _ = store_new.insert_folder(&name, parent_folder_id);
            if let Some(tree) = session_tree_from_tree_view(&tree_view_new) {
                tree.refresh();
            }
        }
    });

    let store_rename = store.duplicate().unwrap_or_else(|e| {
        eprintln!("Wayterm: failed to duplicate store for rename-folder handler: {e}");
        SessionStore::in_memory().expect("failed to create fallback store")
    });
    let tree_view_rename = tree_view.clone();
    rename_folder.connect_activate(move |_, _| {
        let parent = gtk_window_for(&tree_view_rename);
        if let Some((id, true)) = selected_folder(&tree_view_rename) {
            let current_name = selected_row_name(&tree_view_rename).unwrap_or_default();
            if let Some(new_name) = prompt_text(parent.as_ref(), "Rename Folder", &current_name) {
                let _ = store_rename.rename_folder(id, &new_name);
                if let Some(tree) = session_tree_from_tree_view(&tree_view_rename) {
                    tree.refresh();
                }
            }
        }
    });

    let store_delete = store.duplicate().unwrap_or_else(|e| {
        eprintln!("Wayterm: failed to duplicate store for delete-folder handler: {e}");
        SessionStore::in_memory().expect("failed to create fallback store")
    });
    let tree_view_delete = tree_view.clone();
    delete_folder.connect_activate(move |_, _| {
        if let Some((id, name)) = selected_folder_with_name(&tree_view_delete) {
            let parent = gtk_window_for(&tree_view_delete);
            confirm_delete_folder(parent.as_ref(), &name, {
                let store = store_delete.duplicate().unwrap_or_else(|e| {
                    eprintln!("Wayterm: failed to duplicate store for delete: {e}");
                    SessionStore::in_memory().expect("failed to create fallback store")
                });
                let tree_view = tree_view_delete.clone();
                move || {
                    let _ = store.delete_folder(id);
                    if let Some(tree) = session_tree_from_tree_view(&tree_view) {
                        tree.refresh();
                    }
                }
            });
        }
    });

    let store_new_session = store.duplicate().unwrap_or_else(|e| {
        eprintln!("Wayterm: failed to duplicate store for new-session handler: {e}");
        SessionStore::in_memory().expect("failed to create fallback store")
    });
    let tree_view_new_session = tree_view.clone();
    new_session.connect_activate(move |_, _| {
        let parent = gtk_window_for(&tree_view_new_session);
        let folders = match store_new_session.list_folders(None) {
            Ok(f) => f,
            Err(err) => {
                eprintln!("Wayterm: failed to list folders for new session dialog: {err}");
                return;
            }
        };
        if let Some(parent) = parent {
            let dialog = SessionDialog::new_session(&parent, &folders);
            if let Some(config) = dialog.run() {
                let _ = store_new_session.insert_session(&config);
                if let Some(tree) = session_tree_from_tree_view(&tree_view_new_session) {
                    tree.refresh();
                }
            }
        }
    });

    let store_edit_session = store.duplicate().unwrap_or_else(|e| {
        eprintln!("Wayterm: failed to duplicate store for edit-session handler: {e}");
        SessionStore::in_memory().expect("failed to create fallback store")
    });
    let tree_view_edit_session = tree_view.clone();
    edit_session.connect_activate(move |_, _| {
        let parent = gtk_window_for(&tree_view_edit_session);
        let Some((id, false)) = selected_folder(&tree_view_edit_session) else {
            return;
        };
        let Ok(config) = store_edit_session.get_session(id) else {
            eprintln!("Wayterm: failed to load session {id} for edit");
            return;
        };
        let folders = match store_edit_session.list_folders(None) {
            Ok(f) => f,
            Err(err) => {
                eprintln!("Wayterm: failed to list folders for edit session dialog: {err}");
                return;
            }
        };
        if let Some(parent) = parent {
            let dialog = SessionDialog::edit(&parent, &config, &folders);
            if let Some(config) = dialog.run() {
                let _ = store_edit_session.update_session(&config);
                if let Some(tree) = session_tree_from_tree_view(&tree_view_edit_session) {
                    tree.refresh();
                }
            }
        }
    });

    let store_delete_session = store.duplicate().unwrap_or_else(|e| {
        eprintln!("Wayterm: failed to duplicate store for delete-session handler: {e}");
        SessionStore::in_memory().expect("failed to create fallback store")
    });
    let tree_view_delete_session = tree_view.clone();
    delete_session.connect_activate(move |_, _| {
        if let Some((id, name)) = selected_session_with_name(&tree_view_delete_session) {
            let parent = gtk_window_for(&tree_view_delete_session);
            confirm_delete_session(parent.as_ref(), &name, {
                let store = store_delete_session.duplicate().unwrap_or_else(|e| {
                    eprintln!("Wayterm: failed to duplicate store for delete session: {e}");
                    SessionStore::in_memory().expect("failed to create fallback store")
                });
                let tree_view = tree_view_delete_session.clone();
                move || {
                    let _ = store.delete_session(id);
                    if let Some(tree) = session_tree_from_tree_view(&tree_view) {
                        tree.refresh();
                    }
                }
            });
        }
    });

    action_group.add_action(&new_folder);
    action_group.add_action(&rename_folder);
    action_group.add_action(&delete_folder);
    action_group.add_action(&new_session);
    action_group.add_action(&edit_session);
    action_group.add_action(&delete_session);

    popover.insert_action_group("tree", Some(&action_group));
    popover.popup();
}

/// Ask the user for a short text string in a small modal window.
///
/// Returns `Some(value)` if the user confirmed, `None` if cancelled.
/// The `parent` window is used only as the transient parent when one is
/// available; the dialog stays usable even without it.
///
/// Shared with the SFTP sidebar (doc 09), which has no entry-capable
/// `AlertDialog` and needs the same "type one short string" interaction for
/// Rename and Mkdir.
pub(crate) fn prompt_text(
    parent: Option<&gtk4::Window>,
    title: &str,
    initial: &str,
) -> Option<String> {
    let result = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let done = std::rc::Rc::new(std::cell::RefCell::new(false));

    let dialog = gtk4::ApplicationWindow::builder()
        .title(title)
        .modal(true)
        .default_width(320)
        .default_height(120)
        .resizable(false)
        .build();
    if let Some(p) = parent {
        dialog.set_transient_for(Some(p));
        dialog.set_destroy_with_parent(true);
    }

    let entry = gtk4::Entry::new();
    entry.set_text(initial);
    entry.set_activates_default(true);
    entry.set_margin_top(12);
    entry.set_margin_bottom(12);
    entry.set_margin_start(12);
    entry.set_margin_end(12);
    entry.set_hexpand(true);

    let ok_button = gtk4::Button::with_label("OK");
    let cancel_button = gtk4::Button::with_label("Cancel");
    cancel_button.set_margin_end(12);

    let button_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    button_box.set_halign(gtk4::Align::End);
    button_box.set_margin_start(12);
    button_box.set_margin_end(12);
    button_box.set_margin_bottom(12);
    button_box.append(&cancel_button);
    button_box.append(&ok_button);

    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    vbox.append(&entry);
    vbox.append(&button_box);
    dialog.set_child(Some(&vbox));

    let result_ok = result.clone();
    let done_ok = done.clone();
    ok_button.connect_clicked({
        let dialog = dialog.clone();
        let entry = entry.clone();
        move |_| {
            *result_ok.borrow_mut() = Some(entry.text().to_string());
            *done_ok.borrow_mut() = true;
            dialog.close();
        }
    });

    let done_cancel = done.clone();
    cancel_button.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            *done_cancel.borrow_mut() = true;
            dialog.close();
        }
    });

    let done_close = done.clone();
    dialog.connect_close_request(move |window| {
        *done_close.borrow_mut() = true;
        window.set_visible(false);
        gtk4::glib::Propagation::Proceed
    });

    dialog.set_default_widget(Some(&ok_button));
    dialog.present();

    while !*done.borrow() {
        gtk4::glib::MainContext::default().iteration(true);
    }

    result.take()
}

/// Confirm deletion of a folder, then run the provided callback on confirm.
fn confirm_delete_folder(
    parent: Option<&gtk4::Window>,
    folder_name: &str,
    on_confirm: impl FnOnce() + 'static,
) {
    let dialog = gtk4::AlertDialog::builder()
        .message(format!("Delete folder '{}'?", folder_name))
        .detail("Sub-folders will be deleted. Sessions inside will be moved to the root.")
        .modal(true)
        .build();
    dialog.set_buttons(&["Cancel", "Delete"]);
    dialog.set_cancel_button(0);
    dialog.set_default_button(1);

    dialog.choose(parent, gio::Cancellable::NONE, move |res| {
        if let Ok(1) = res {
            on_confirm();
        }
    });
}

/// Return the id and `is_folder` flag of the currently selected row.
#[allow(deprecated)]
fn selected_row_info(tree_view: &gtk4::TreeView) -> (Option<i64>, bool) {
    let selection = tree_view.selection();
    let Some((model, iter)) = selection.selected() else {
        return (None, false);
    };
    let id = model.get::<i64>(&iter, 2);
    let is_folder = model.get::<bool>(&iter, 3);
    (Some(id), is_folder)
}

/// Return the name of the currently selected row.
#[allow(deprecated)]
fn selected_row_name(tree_view: &gtk4::TreeView) -> Option<String> {
    let selection = tree_view.selection();
    let (model, iter) = selection.selected()?;
    Some(model.get::<glib::GString>(&iter, 1).to_string())
}

/// Return the selected folder's id if a folder is selected.
#[allow(deprecated)]
fn selected_folder(tree_view: &gtk4::TreeView) -> Option<(i64, bool)> {
    let (id, is_folder) = selected_row_info(tree_view);
    id.zip(Some(is_folder)).filter(|(_, f)| *f)
}

/// Return the selected folder's id and name.
#[allow(deprecated)]
fn selected_folder_with_name(tree_view: &gtk4::TreeView) -> Option<(i64, String)> {
    let (id, is_folder) = selected_row_info(tree_view);
    if !is_folder {
        return None;
    }
    let name = selected_row_name(tree_view).unwrap_or_default();
    id.map(|id| (id, name))
}

/// Return the selected session's id and name.
#[allow(deprecated)]
fn selected_session_with_name(tree_view: &gtk4::TreeView) -> Option<(i64, String)> {
    let (id, is_folder) = selected_row_info(tree_view);
    if is_folder {
        return None;
    }
    let name = selected_row_name(tree_view).unwrap_or_default();
    id.map(|id| (id, name))
}

/// Confirm deletion of a session, then run the provided callback on confirm.
fn confirm_delete_session(
    parent: Option<&gtk4::Window>,
    session_name: &str,
    on_confirm: impl FnOnce() + 'static,
) {
    let dialog = gtk4::AlertDialog::builder()
        .message(format!("Delete session '{}'?", session_name))
        .detail("This cannot be undone.")
        .modal(true)
        .build();
    dialog.set_buttons(&["Cancel", "Delete"]);
    dialog.set_cancel_button(0);
    dialog.set_default_button(1);

    dialog.choose(parent, gio::Cancellable::NONE, move |res| {
        if let Ok(1) = res {
            on_confirm();
        }
    });
}

/// Walk from a tree view up through widget parents to find a [`gtk4::Window`].
#[allow(deprecated)]
fn gtk_window_for(tree_view: &gtk4::TreeView) -> Option<gtk4::Window> {
    let mut widget = tree_view.clone().upcast::<gtk4::Widget>();
    loop {
        if let Ok(window) = widget.clone().downcast::<gtk4::Window>() {
            return Some(window);
        }
        widget = widget.parent()?;
    }
}

/// Recover the [`SessionTree`] that owns this `TreeView` via widget data.
#[allow(deprecated)]
fn session_tree_from_tree_view(tree_view: &gtk4::TreeView) -> Option<SessionTree> {
    unsafe {
        tree_view
            .data::<SessionTree>("wayterm-session-tree")
            .map(|d| d.as_ref().clone())
    }
}

#[cfg(test)]
mod tests;
