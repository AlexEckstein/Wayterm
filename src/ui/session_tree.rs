//! The session tree sidebar: folders and saved sessions with a right-click
//! CRUD menu (docs 01, 06). Rows draw their icon by name from the icon
//! theme (`ui::icons`). [`collect_rows`] is the display-free part.

// GtkTreeView/TreeStore are deprecated since GTK 4.10; migrating to
// GtkListView is UI polish, not a contract change.
#![allow(deprecated)]

use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::store::SessionStore;
use crate::ui::folder_dialog::FolderDialog;
use crate::ui::icons::{resolve_icon, session_type_icon, FOLDER_ICON};
use crate::ui::session_dialog::{collect_peer_sessions, SessionDialog};
use crate::ui::widgets::{confirm, prompt_text, window_of};

/// One row of the tree, in display order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRow {
    pub id: i64,
    pub is_folder: bool,
    pub name: String,
    /// The stored icon name; `None` = `default_icon`.
    pub icon: Option<String>,
    pub default_icon: &'static str,
    pub parent_id: Option<i64>,
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
                icon: folder.icon,
                default_icon: FOLDER_ICON,
                parent_id: parent,
            });
            walk(store, Some(id), rows)?;
        }
        for session in store.list_sessions(parent)? {
            rows.push(SessionRow {
                id: session.id,
                is_folder: false,
                default_icon: session_type_icon(&session.session_type),
                name: session.name,
                icon: session.icon,
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
    scroll: gtk4::ScrolledWindow,
    /// The context menu, reused for every right-click. It hangs off
    /// `scroll`: a TreeView keeps its column buttons under an internal CSS
    /// node, so a popover parented to it trips a GTK assertion.
    popover: gtk4::PopoverMenu,
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
        let view_column = gtk4::TreeViewColumn::new();
        let icon = gtk4::CellRendererPixbuf::new();
        view_column.pack_start(&icon, false);
        view_column.add_attribute(&icon, "icon-name", COL_ICON as i32);
        let name = gtk4::CellRendererText::new();
        view_column.pack_start(&name, true);
        view_column.add_attribute(&name, "text", COL_NAME as i32);
        tree_view.append_column(&view_column);
        tree_view.selection().set_mode(gtk4::SelectionMode::Single);
        let scroll = gtk4::ScrolledWindow::builder()
            .child(&tree_view)
            .vexpand(true)
            .hexpand(true)
            .build();
        let popover = gtk4::PopoverMenu::from_model(None::<&gio::MenuModel>);
        popover.set_parent(&scroll);
        // The scrolled window only unparents the children it knows about.
        scroll.connect_destroy(glib::clone!(
            #[strong]
            popover,
            move |_| popover.unparent()
        ));

        let tree = Self {
            store,
            tree_view,
            model,
            scroll,
            popover,
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
        let theme = gtk4::IconTheme::for_display(&self.tree_view.display());
        let mut folder_iters = HashMap::new();
        for row in rows {
            let parent = row.parent_id.and_then(|id| folder_iters.get(&id));
            let iter = self.model.insert(parent, -1);
            let icon = resolve_icon(row.icon.as_deref(), row.default_icon, |name| {
                theme.has_icon(name)
            });
            self.model.set(
                &iter,
                &[
                    (COL_ICON, &icon),
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

    /// The tree inside its scrolled window.
    pub fn widget(&self) -> gtk4::Widget {
        self.scroll.clone().upcast()
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
            item(
                "Edit Folder",
                "edit-folder",
                Box::new(move |tree| tree.edit_folder(id)),
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

        // Window actions (main_window), shown whatever is selected — an
        // empty tree included.
        let transfer = gio::Menu::new();
        transfer.append(Some("Import Sessions…"), Some("win.import-sessions"));
        transfer.append(Some("Export Sessions…"), Some("win.export-sessions"));
        menu.append_section(None, &transfer);

        self.popover.set_menu_model(Some(&menu));
        self.popover.insert_action_group("tree", Some(&group));
        // Anchor at the click (tree coordinates, moved into the popover
        // parent's), not at the sidebar's edge.
        let click = gtk4::graphene::Point::new(x as f32, y as f32);
        let at = self
            .tree_view
            .compute_point(&self.scroll, &click)
            .unwrap_or(click);
        self.popover.set_pointing_to(Some(&gdk::Rectangle::new(
            at.x() as i32,
            at.y() as i32,
            0,
            0,
        )));
        self.popover.popup();
    }

    /// Edit a folder's name and icon.
    fn edit_folder(&self, id: i64) {
        let folder = match self.store.get_folder(id) {
            Ok(folder) => folder,
            Err(err) => return eprintln!("Wayterm: failed to load folder {id}: {err}"),
        };
        if let Some(edit) = FolderDialog::new(self.parent().as_ref(), &folder).run() {
            self.finish(
                "save the folder",
                self.store
                    .update_folder(id, &edit.name, edit.icon.as_deref()),
            );
        }
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
        let peers = collect_peer_sessions(&self.store);
        let dialog = match id {
            None => SessionDialog::new_session(&parent, &folders, &peers),
            Some(id) => match self.store.get_session(id) {
                Ok(config) => SessionDialog::edit(&parent, &config, &folders, &peers),
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

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Repeated right-clicks reuse one menu, and parenting it raises no GTK
    /// criticals (a popover on the TreeView itself did, once per click).
    pub(in crate::ui) fn context_menu_is_reused_and_parented_cleanly() {
        let criticals = Arc::new(AtomicUsize::new(0));
        let count = criticals.clone();
        let handler = glib::log_set_handler(
            Some("Gtk"),
            glib::LogLevels::LEVEL_CRITICAL,
            false,
            false,
            move |_, _, message| {
                eprintln!("Gtk-CRITICAL: {message}");
                count.fetch_add(1, Ordering::SeqCst);
            },
        );

        let store = Rc::new(SessionStore::in_memory().unwrap());
        store.insert_folder("A", None).unwrap();
        let tree = SessionTree::new(store, |_| {});
        tree.refresh();
        let window = gtk4::Window::new();
        window.set_child(Some(&tree.widget()));
        window.present();
        for _ in 0..3 {
            tree.show_context_menu(5.0, 5.0);
            glib::MainContext::default().iteration(false);
        }
        glib::log_remove_handler(Some("Gtk"), handler);

        assert_eq!(criticals.load(Ordering::SeqCst), 0);
        let mut child = tree.tree_view.first_child();
        while let Some(widget) = child {
            assert!(!widget.is::<gtk4::Popover>(), "popover on the TreeView");
            child = widget.next_sibling();
        }
        window.destroy();
    }
}
