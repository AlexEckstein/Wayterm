//! The session tree sidebar: folders and saved sessions with a right-click
//! CRUD menu; sessions drag onto folders to move (docs 01, 06). Rows draw their icon by name from the icon
//! theme (`ui::icons`). [`collect_rows`] is the display-free part.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::store::SessionStore;
use crate::ui::folder_dialog::FolderDialog;
use crate::ui::icons::{resolve_icon, session_type_icon, FOLDER_ICON};
use crate::ui::session_dialog::SessionDialog;
use crate::ui::widgets::{confirm, data, prompt_text, set_data, window_of};

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

/// One item of the list model, boxed in a `glib::BoxedAnyObject`.
struct Node {
    id: i64,
    is_folder: bool,
    name: String,
    parent_id: Option<i64>,
    /// The resolved icon name.
    icon: String,
    /// The folder's contents; `None` for sessions and empty folders, which
    /// then draw no expander.
    children: Option<gio::ListStore>,
}

/// The node behind a model item (a `TreeListRow` or the boxed node itself).
fn node_of(item: &glib::Object) -> Option<glib::BoxedAnyObject> {
    match item.downcast_ref::<gtk4::TreeListRow>() {
        Some(row) => row.item()?.downcast().ok(),
        None => item.clone().downcast().ok(),
    }
}

/// The selected row: `(id, is_folder, name)`.
type Selected = (i64, bool, String);

/// Key under which each row widget keeps a weak ref to its `ListItem`, so
/// a click can be traced back to a model position.
const LIST_ITEM_KEY: &str = "wayterm-list-item";

#[derive(Clone)]
pub struct SessionTree {
    store: Rc<SessionStore>,
    list_view: gtk4::ListView,
    /// Top-level folders and sessions; the tree model wraps it.
    root: gio::ListStore,
    tree_model: gtk4::TreeListModel,
    selection: gtk4::SingleSelection,
    scroll: gtk4::ScrolledWindow,
    /// The context menu, reused for every right-click. It hangs off
    /// `scroll`, outside the list view's own widget tree.
    popover: gtk4::PopoverMenu,
}

impl SessionTree {
    /// `on_open_session` runs with the session id when a session row is
    /// activated (double-click / Enter).
    pub fn new(store: Rc<SessionStore>, on_open_session: impl Fn(i64) + 'static) -> Self {
        let root = gio::ListStore::new::<glib::BoxedAnyObject>();
        let tree_model = gtk4::TreeListModel::new(root.clone(), false, false, |item| {
            let node = node_of(item)?;
            let children = node.borrow::<Node>().children.clone();
            children.map(|store| store.upcast())
        });
        let selection = gtk4::SingleSelection::builder()
            .model(&tree_model)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let list_view = gtk4::ListView::builder()
            .model(&selection)
            .vexpand(true)
            .hexpand(true)
            .build();
        let scroll = gtk4::ScrolledWindow::builder()
            .child(&list_view)
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
            list_view,
            root,
            tree_model,
            selection,
            scroll,
            popover,
        };

        tree.list_view.set_factory(Some(&row_factory(&tree)));

        // A session dropped below the rows moves to the root.
        let drop = gtk4::DropTarget::new(glib::Type::I64, gdk::DragAction::MOVE);
        let for_drop = tree.clone();
        drop.connect_drop(move |_, value, _, _| {
            value
                .get::<i64>()
                .is_ok_and(|id| for_drop.move_session(id, None))
        });
        tree.list_view.add_controller(drop);

        // Activating a session opens it; activating a folder toggles it.
        let for_activate = tree.clone();
        tree.list_view.connect_activate(move |_, position| {
            let Some(row) = for_activate.tree_model.row(position) else {
                return;
            };
            let Some(node) = node_of(row.upcast_ref()) else {
                return;
            };
            let (id, is_folder) = {
                let node = node.borrow::<Node>();
                (node.id, node.is_folder)
            };
            if is_folder {
                row.set_expanded(!row.is_expanded());
            } else {
                on_open_session(id);
            }
        });

        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        let for_menu = tree.clone();
        gesture.connect_pressed(move |_, _, x, y| {
            let position = for_menu.position_at(x, y);
            for_menu
                .selection
                .set_selected(position.unwrap_or(gtk4::INVALID_LIST_POSITION));
            for_menu.show_context_menu(x, y);
        });
        tree.list_view.add_controller(gesture);
        tree
    }

    /// Reload the tree from the store; open folders stay open.
    pub fn refresh(&self) {
        self.rebuild(self.expanded_folders());
    }

    /// Ids of the folders currently expanded.
    fn expanded_folders(&self) -> HashSet<i64> {
        (0..self.tree_model.n_items())
            .filter_map(|position| self.tree_model.row(position))
            .filter(|row| row.is_expanded())
            .filter_map(|row| node_of(row.upcast_ref()))
            .map(|node| node.borrow::<Node>().id)
            .collect()
    }

    /// Reload the tree from the store and expand the folders in `expanded`
    /// (those still present and non-empty).
    fn rebuild(&self, expanded: HashSet<i64>) {
        let rows = match collect_rows(&self.store) {
            Ok(rows) => rows,
            Err(err) => {
                eprintln!("Wayterm: failed to load the session tree: {err}");
                self.root.remove_all();
                return;
            }
        };
        let theme = gtk4::IconTheme::for_display(&self.list_view.display());
        // Folders with contents get a child store; rows arrive parents
        // first, so each folder's store exists before its children.
        let non_empty: HashSet<Option<i64>> = rows.iter().map(|row| row.parent_id).collect();
        let mut stores = HashMap::new();
        let mut top = Vec::new();
        for row in rows {
            let children = (row.is_folder && non_empty.contains(&Some(row.id))).then(|| {
                let store = gio::ListStore::new::<glib::BoxedAnyObject>();
                stores.insert(row.id, store.clone());
                store
            });
            let icon = resolve_icon(row.icon.as_deref(), row.default_icon, |name| {
                theme.has_icon(name)
            });
            let node = glib::BoxedAnyObject::new(Node {
                id: row.id,
                is_folder: row.is_folder,
                name: row.name,
                parent_id: row.parent_id,
                icon: icon.to_string(),
                children,
            });
            match row.parent_id.and_then(|id| stores.get(&id)) {
                Some(store) => store.append(&node),
                None => top.push(node),
            }
        }
        self.root.splice(0, self.root.n_items(), &top);
        // Expanding a row inserts its children right after it, so walking
        // forward also reaches nested folders.
        let mut position = 0;
        while let Some(row) = self.tree_model.row(position) {
            let open = node_of(row.upcast_ref()).is_some_and(|node| {
                let node = node.borrow::<Node>();
                node.is_folder && expanded.contains(&node.id)
            });
            if open {
                row.set_expanded(true);
            }
            position += 1;
        }
    }

    /// Move session `id` into `folder` (`None` = root), keep that folder
    /// open, and refresh once idle — callers are drop handlers on the very
    /// rows the refresh replaces. False if nothing moved.
    fn move_session(&self, id: i64, folder: Option<i64>) -> bool {
        let mut config = match self.store.get_session(id) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("Wayterm: failed to load session {id}: {err}");
                return false;
            }
        };
        if config.folder_id == folder {
            return false;
        }
        config.folder_id = folder;
        if let Err(err) = self.store.update_session(&config) {
            eprintln!("Wayterm: failed to move the session: {err}");
            return false;
        }
        let mut expanded = self.expanded_folders();
        expanded.extend(folder);
        let tree = self.clone();
        glib::idle_add_local_once(move || tree.rebuild(expanded));
        true
    }

    /// The tree inside its scrolled window.
    pub fn widget(&self) -> gtk4::Widget {
        self.scroll.clone().upcast()
    }

    fn selected(&self) -> Option<Selected> {
        let node = node_of(&self.selection.selected_item()?)?;
        let node = node.borrow::<Node>();
        Some((node.id, node.is_folder, node.name.clone()))
    }

    /// The model position of the row at `(x, y)` in list view coordinates.
    fn position_at(&self, x: f64, y: f64) -> Option<u32> {
        let mut widget = self.list_view.pick(x, y, gtk4::PickFlags::DEFAULT);
        while let Some(current) = widget {
            if let Some(item) = data::<glib::WeakRef<gtk4::ListItem>>(&current, LIST_ITEM_KEY) {
                return item
                    .upgrade()
                    .map(|item| item.position())
                    .filter(|&position| position != gtk4::INVALID_LIST_POSITION);
            }
            if current == *self.list_view.upcast_ref::<gtk4::Widget>() {
                break;
            }
            widget = current.parent();
        }
        None
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
            .list_view
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
        window_of(&self.list_view)
    }
}

/// The node shown by a row widget's list item.
fn node_at(item: &glib::WeakRef<gtk4::ListItem>) -> Option<glib::BoxedAnyObject> {
    node_of(&item.upgrade()?.item()?)
}

/// Row widgets: an expander (indent and disclosure arrow) around the
/// icon and name. Session rows drag their id; every row is a drop target
/// that moves the session into the row's folder (a folder row's own, a
/// session row's parent).
fn row_factory(tree: &SessionTree) -> gtk4::SignalListItemFactory {
    let factory = gtk4::SignalListItemFactory::new();
    let tree = tree.clone();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        content.append(&gtk4::Image::new());
        content.append(&gtk4::Label::builder().xalign(0.0).build());
        let expander = gtk4::TreeExpander::builder().child(&content).build();
        set_data(&expander, LIST_ITEM_KEY, item.downgrade());

        let drag = gtk4::DragSource::new();
        drag.set_actions(gdk::DragAction::MOVE);
        let weak = item.downgrade();
        drag.connect_prepare(move |_, _, _| {
            let node = node_at(&weak)?;
            let node = node.borrow::<Node>();
            (!node.is_folder).then(|| gdk::ContentProvider::for_value(&node.id.to_value()))
        });
        drag.connect_drag_begin(glib::clone!(
            #[weak]
            content,
            move |source, _| {
                let icon = gtk4::WidgetPaintable::new(Some(&content));
                source.set_icon(Some(&icon), 0, 0);
            }
        ));
        expander.add_controller(drag);

        let drop = gtk4::DropTarget::new(glib::Type::I64, gdk::DragAction::MOVE);
        let weak = item.downgrade();
        let tree = tree.clone();
        drop.connect_drop(move |_, value, _, _| {
            let (Ok(id), Some(node)) = (value.get::<i64>(), node_at(&weak)) else {
                return false;
            };
            let folder = {
                let node = node.borrow::<Node>();
                if node.is_folder {
                    Some(node.id)
                } else {
                    node.parent_id
                }
            };
            tree.move_session(id, folder)
        });
        expander.add_controller(drop);

        item.set_child(Some(&expander));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let Some(expander) = item.child().and_downcast::<gtk4::TreeExpander>() else {
            return;
        };
        let row = item.item().and_downcast::<gtk4::TreeListRow>();
        expander.set_list_row(row.as_ref());
        let Some(node) = row.and_then(|row| node_of(row.upcast_ref())) else {
            return;
        };
        let node = node.borrow::<Node>();
        let Some(content) = expander.child() else {
            return;
        };
        if let Some(image) = content.first_child().and_downcast::<gtk4::Image>() {
            image.set_icon_name(Some(&node.icon));
        }
        if let Some(label) = content.last_child().and_downcast::<gtk4::Label>() {
            label.set_text(&node.name);
        }
    });
    factory.connect_unbind(|_, item| {
        if let Some(expander) = item
            .downcast_ref::<gtk4::ListItem>()
            .and_then(|item| item.child())
            .and_downcast::<gtk4::TreeExpander>()
        {
            expander.set_list_row(None);
        }
    });
    factory
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
    /// criticals.
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
        let mut child = tree.list_view.first_child();
        while let Some(widget) = child {
            assert!(!widget.is::<gtk4::Popover>(), "popover on the list view");
            child = widget.next_sibling();
        }
        window.destroy();
    }

    /// Folders nest their contents, expand on demand, and selection maps
    /// back to the stored row; a refresh rebuilds the model.
    pub(in crate::ui) fn rows_nest_expand_and_select() {
        let store = Rc::new(SessionStore::in_memory().unwrap());
        let folder = store.insert_folder("A", None).unwrap();
        let empty = store.insert_folder("Empty", None).unwrap();
        let inner = store
            .insert_session(&crate::store::model::SessionConfig {
                name: "inner".to_string(),
                folder_id: Some(folder),
                ..Default::default()
            })
            .unwrap();
        store
            .insert_session(&crate::store::model::SessionConfig {
                name: "outer".to_string(),
                ..Default::default()
            })
            .unwrap();
        let tree = SessionTree::new(store.clone(), |_| {});
        tree.refresh();

        // A, Empty, outer — collapsed; only the non-empty folder expands.
        assert_eq!(tree.tree_model.n_items(), 3);
        let row = tree.tree_model.row(0).unwrap();
        assert!(row.is_expandable());
        assert!(!tree.tree_model.row(1).unwrap().is_expandable());
        row.set_expanded(true);
        assert_eq!(tree.tree_model.n_items(), 4);

        tree.selection.set_selected(1);
        assert_eq!(tree.selected(), Some((inner, false, "inner".to_string())));
        tree.selection.set_selected(2);
        assert_eq!(tree.selected(), Some((empty, true, "Empty".to_string())));

        // A stays open across the refresh: A, inner, outer.
        store.delete_folder(empty).unwrap();
        tree.refresh();
        assert_eq!(tree.tree_model.n_items(), 3);
    }

    /// Moving a session opens its new folder and keeps open folders open;
    /// a move to where it already is changes nothing.
    pub(in crate::ui) fn moving_a_session_keeps_folders_open() {
        let store = Rc::new(SessionStore::in_memory().unwrap());
        let a = store.insert_folder("A", None).unwrap();
        let b = store.insert_folder("B", None).unwrap();
        let session = |name: &str, folder_id| crate::store::model::SessionConfig {
            name: name.to_string(),
            folder_id,
            ..Default::default()
        };
        store.insert_session(&session("in-a", Some(a))).unwrap();
        let moved = store.insert_session(&session("moved", None)).unwrap();
        let tree = SessionTree::new(store.clone(), |_| {});
        tree.refresh();
        let settle = || while glib::MainContext::default().iteration(false) {};
        let names = || {
            (0..tree.tree_model.n_items())
                .filter_map(|position| node_of(tree.tree_model.row(position)?.upcast_ref()))
                .map(|node| node.borrow::<Node>().name.clone())
                .collect::<Vec<_>>()
        };

        tree.tree_model.row(0).unwrap().set_expanded(true);
        assert_eq!(names(), ["A", "in-a", "B", "moved"]);

        // Into the empty folder B: B opens, A stays open.
        assert!(tree.move_session(moved, Some(b)));
        settle();
        assert_eq!(store.get_session(moved).unwrap().folder_id, Some(b));
        assert_eq!(names(), ["A", "in-a", "B", "moved"]);
        assert!(tree.tree_model.row(2).unwrap().is_expanded());

        assert!(!tree.move_session(moved, Some(b)));

        // Back to the root; B is empty again and loses its expander.
        assert!(tree.move_session(moved, None));
        settle();
        assert_eq!(store.get_session(moved).unwrap().folder_id, None);
        assert_eq!(names(), ["A", "in-a", "B", "moved"]);
        assert!(!tree.tree_model.row(2).unwrap().is_expandable());

        // A plain refresh keeps A open too.
        tree.refresh();
        assert!(tree.tree_model.row(0).unwrap().is_expanded());
    }
}
