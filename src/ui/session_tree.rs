//! The session tree sidebar: folders and saved sessions with a right-click
//! CRUD menu (docs 01, 06). [`collect_rows`] is the display-free part.

// GtkTreeView/TreeStore are deprecated since GTK 4.10; migrating to
// GtkListView is UI polish, not a contract change.
#![allow(deprecated)]

use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::store::model::SessionType;
use crate::store::SessionStore;
use crate::ui::session_dialog::SessionDialog;
use crate::ui::widgets::{confirm, prompt_text, window_of};

/// One row of the tree, in display order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRow {
    pub id: i64,
    pub is_folder: bool,
    pub name: String,
    pub icon: &'static str,
    pub parent_id: Option<i64>,
}

pub fn icon_for_session_type(session_type: &SessionType) -> &'static str {
    match session_type {
        SessionType::SSH => "🖥",
        SessionType::RDP => "🖼",
        SessionType::VNC => "📺",
        SessionType::Serial => "🔌",
        SessionType::LocalShell => "🏠",
    }
}

/// The whole tree depth-first: at each level folders first, then sessions,
/// each by `sort_order` then name.
pub fn collect_rows(store: &SessionStore) -> rusqlite::Result<Vec<SessionRow>> {
    fn walk(
        store: &SessionStore,
        parent: Option<i64>,
        rows: &mut Vec<SessionRow>,
    ) -> rusqlite::Result<()> {
        for folder in store.list_folders(parent)? {
            let id = folder.id;
            rows.push(SessionRow {
                id,
                is_folder: true,
                name: folder.name,
                icon: "📁",
                parent_id: parent,
            });
            walk(store, Some(id), rows)?;
        }
        for session in store.list_sessions(parent)? {
            rows.push(SessionRow {
                id: session.id,
                is_folder: false,
                name: session.name,
                icon: icon_for_session_type(&session.session_type),
                parent_id: parent,
            });
        }
        Ok(())
    }
    let mut rows = Vec::new();
    walk(store, None, &mut rows)?;
    Ok(rows)
}

// Tree store columns.
const COL_ICON: u32 = 0;
const COL_NAME: u32 = 1;
const COL_ID: u32 = 2;
const COL_IS_FOLDER: u32 = 3;

/// The selected row: `(id, is_folder, name)`.
type Selected = (i64, bool, String);

#[derive(Clone)]
pub struct SessionTree {
    store: Rc<SessionStore>,
    tree_view: gtk4::TreeView,
    model: gtk4::TreeStore,
}

impl SessionTree {
    /// `on_open_session` runs with the session id when a session row is
    /// activated (double-click / Enter).
    pub fn new(store: Rc<SessionStore>, on_open_session: impl Fn(i64) + 'static) -> Self {
        let model = gtk4::TreeStore::new(&[
            glib::Type::STRING,
            glib::Type::STRING,
            glib::Type::I64,
            glib::Type::BOOL,
        ]);
        let tree_view = gtk4::TreeView::builder()
            .model(&model)
            .headers_visible(false)
            .vexpand(true)
            .hexpand(true)
            .build();
        for (column, expand) in [(COL_ICON, false), (COL_NAME, true)] {
            let view_column = gtk4::TreeViewColumn::new();
            let renderer = gtk4::CellRendererText::new();
            view_column.pack_start(&renderer, expand);
            view_column.add_attribute(&renderer, "text", column as i32);
            tree_view.append_column(&view_column);
        }
        tree_view.selection().set_mode(gtk4::SelectionMode::Single);

        let tree = Self {
            store,
            tree_view,
            model,
        };

        let for_activate = tree.clone();
        tree.tree_view.connect_row_activated(move |_, _, _| {
            if let Some((id, false, _)) = for_activate.selected() {
                on_open_session(id);
            }
        });

        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        let for_menu = tree.clone();
        gesture.connect_pressed(move |_, _, x, y| {
            let selection = for_menu.tree_view.selection();
            match for_menu.tree_view.path_at_pos(x as i32, y as i32) {
                Some((Some(path), _, _, _)) => selection.select_path(&path),
                _ => selection.unselect_all(),
            }
            for_menu.show_context_menu(x, y);
        });
        tree.tree_view.add_controller(gesture);
        tree
    }

    /// Reload the tree from the store.
    pub fn refresh(&self) {
        self.model.clear();
        let rows = match collect_rows(&self.store) {
            Ok(rows) => rows,
            Err(err) => {
                eprintln!("Wayterm: failed to load the session tree: {err}");
                return;
            }
        };
        let mut folder_iters = HashMap::new();
        for row in rows {
            let parent = row.parent_id.and_then(|id| folder_iters.get(&id));
            let iter = self.model.insert(parent, -1);
            self.model.set(
                &iter,
                &[
                    (COL_ICON, &row.icon),
                    (COL_NAME, &row.name),
                    (COL_ID, &row.id),
                    (COL_IS_FOLDER, &row.is_folder),
                ],
            );
            if row.is_folder {
                folder_iters.insert(row.id, iter);
            }
        }
    }

    pub fn widget(&self) -> gtk4::Widget {
        self.tree_view.clone().upcast()
    }

    fn selected(&self) -> Option<Selected> {
        let (model, iter) = self.tree_view.selection().selected()?;
        Some((
            model.get::<i64>(&iter, COL_ID as i32),
            model.get::<bool>(&iter, COL_IS_FOLDER as i32),
            model.get::<String>(&iter, COL_NAME as i32),
        ))
    }

    /// Log a failed mutation, or refresh after a successful one.
    fn finish<T>(&self, what: &str, result: rusqlite::Result<T>) {
        match result {
            Ok(_) => self.refresh(),
            Err(err) => eprintln!("Wayterm: failed to {what}: {err}"),
        }
    }

    /// The menu at the pointer: items depend on what is selected.
    fn show_context_menu(&self, x: f64, y: f64) {
        let selected = self.selected();
        let folder = selected.clone().filter(|(_, is_folder, _)| *is_folder);
        let session = selected.filter(|(_, is_folder, _)| !is_folder);

        let menu = gio::Menu::new();
        let group = gio::SimpleActionGroup::new();
        let item = |label: &str, name: &str, run: Box<dyn Fn(&SessionTree)>| {
            menu.append(Some(label), Some(&format!("tree.{name}")));
            let action = gio::SimpleAction::new(name, None);
            let tree = self.clone();
            action.connect_activate(move |_, _| run(&tree));
            group.add_action(&action);
        };

        let parent = folder.as_ref().map(|(id, _, _)| *id);
        item(
            "New Folder",
            "new-folder",
            Box::new(move |tree| {
                if let Some(name) = prompt_text(tree.parent().as_ref(), "New Folder", "New Folder")
                {
                    tree.finish("create a folder", tree.store.insert_folder(&name, parent));
                }
            }),
        );
        if let Some((id, _, name)) = folder {
            let current = name.clone();
            item(
                "Rename Folder",
                "rename-folder",
                Box::new(move |tree| {
                    if let Some(new_name) =
                        prompt_text(tree.parent().as_ref(), "Rename Folder", &current)
                    {
                        tree.finish("rename the folder", tree.store.rename_folder(id, &new_name));
                    }
                }),
            );
            item(
                "Delete Folder",
                "delete-folder",
                Box::new(move |tree| {
                    let tree = tree.clone();
                    confirm(
                        tree.parent().as_ref(),
                        &format!("Delete folder '{name}'?"),
                        "Sub-folders will be deleted. Sessions inside will be moved to the root.",
                        "Delete",
                        move || tree.finish("delete the folder", tree.store.delete_folder(id)),
                    );
                }),
            );
        }
        item(
            "New Session",
            "new-session",
            Box::new(|tree| tree.edit_session(None)),
        );
        if let Some((id, _, name)) = session {
            item(
                "Edit Session",
                "edit-session",
                Box::new(move |tree| tree.edit_session(Some(id))),
            );
            item(
                "Delete Session",
                "delete-session",
                Box::new(move |tree| {
                    let tree = tree.clone();
                    confirm(
                        tree.parent().as_ref(),
                        &format!("Delete session '{name}'?"),
                        "This cannot be undone.",
                        "Delete",
                        move || tree.finish("delete the session", tree.store.delete_session(id)),
                    );
                }),
            );
        }

        let popover = gtk4::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.tree_view);
        // Anchor at the click, not at the sidebar's edge.
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 0, 0)));
        popover.insert_action_group("tree", Some(&group));
        popover.popup();
    }

    /// Create (`None`) or edit a session through the session dialog.
    fn edit_session(&self, id: Option<i64>) {
        let Some(parent) = self.parent() else {
            return;
        };
        let folders = match self.store.list_folders(None) {
            Ok(folders) => folders,
            Err(err) => return eprintln!("Wayterm: failed to list folders: {err}"),
        };
        let dialog = match id {
            None => SessionDialog::new_session(&parent, &folders),
            Some(id) => match self.store.get_session(id) {
                Ok(config) => SessionDialog::edit(&parent, &config, &folders),
                Err(err) => return eprintln!("Wayterm: failed to load session {id}: {err}"),
            },
        };
        if let Some(config) = dialog.run() {
            match id {
                None => self.finish("save the session", self.store.insert_session(&config)),
                Some(_) => self.finish("save the session", self.store.update_session(&config)),
            }
        }
    }

    fn parent(&self) -> Option<gtk4::Window> {
        window_of(&self.tree_view)
    }
}

#[cfg(test)]
mod tests;
