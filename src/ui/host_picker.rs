//! The New Tab host picker (doc 03 §New Tab): "Local shell" first and
//! selected, then every saved session under its folders, all expanded, in
//! the session tree's order. Up/Down moves between hosts — folders are
//! headings, never selected — Return or a double-click opens the host in a
//! new tab, Escape cancels. [`picker_entries`] is the display-free part.

use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, glib};

use crate::store::model::{SessionConfig, SessionType};
use crate::store::SessionStore;
use crate::ui::icons::{resolve_icon, session_type_icon, FOLDER_ICON};
use crate::ui::session_tree::{collect_rows, SessionRow};
use crate::ui::tab::SessionTab;
use crate::ui::widgets::{set_margins, window_of};

/// The session id a local-shell tab records (tab persistence stores it):
/// no saved session has it, as SQLite assigns ids from 1.
pub const LOCAL_SHELL_ID: i64 = 0;

/// Indentation per folder level, in pixels.
const INDENT: i32 = 18;

/// The unsaved session behind "Local shell": the user's login shell
/// (`$SHELL`) in their home directory.
pub fn local_shell_session() -> SessionConfig {
    SessionConfig {
        id: LOCAL_SHELL_ID,
        name: "Local shell".into(),
        session_type: SessionType::LocalShell,
        local_working_dir: Some(glib::home_dir().to_string_lossy().into_owned()),
        ..Default::default()
    }
}

/// A folder heading drawn above an entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerHeading {
    pub name: String,
    /// The stored icon name; `None` = the folder icon.
    pub icon: Option<String>,
    /// Nesting depth: 0 at the root.
    pub depth: usize,
}

/// One host the picker offers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerEntry {
    /// `None` for the local shell.
    pub session_id: Option<i64>,
    pub name: String,
    /// The stored icon name; `None` = `default_icon`.
    pub icon: Option<String>,
    pub default_icon: &'static str,
    pub depth: usize,
    /// The folders that open right above this entry, outermost first.
    pub headings: Vec<PickerHeading>,
}

/// The local shell, then every session of `rows` (the session tree's
/// depth-first order) with the folder headings it opens. Folders with no
/// session below them are left out.
pub fn picker_entries(rows: &[SessionRow]) -> Vec<PickerEntry> {
    let mut entries = vec![PickerEntry {
        session_id: None,
        name: "Local shell".into(),
        icon: None,
        default_icon: session_type_icon(&SessionType::LocalShell),
        depth: 0,
        headings: Vec::new(),
    }];
    let mut depth_of: HashMap<i64, usize> = HashMap::new();
    let mut parent_of: HashMap<i64, Option<i64>> = HashMap::new();
    // Folders met since the last session; each heads the next session
    // below it, or is dropped.
    let mut pending: Vec<&SessionRow> = Vec::new();
    for row in rows {
        let depth = row
            .parent_id
            .and_then(|parent| depth_of.get(&parent))
            .map_or(0, |depth| depth + 1);
        if row.is_folder {
            depth_of.insert(row.id, depth);
            parent_of.insert(row.id, row.parent_id);
            pending.push(row);
            continue;
        }
        let mut ancestors = Vec::new();
        let mut folder = row.parent_id;
        while let Some(id) = folder {
            ancestors.push(id);
            folder = parent_of.get(&id).copied().flatten();
        }
        let headings = pending
            .drain(..)
            .filter(|folder| ancestors.contains(&folder.id))
            .map(|folder| PickerHeading {
                name: folder.name.clone(),
                icon: folder.icon.clone(),
                depth: depth_of[&folder.id],
            })
            .collect();
        entries.push(PickerEntry {
            session_id: Some(row.id),
            name: row.name.clone(),
            icon: row.icon.clone(),
            default_icon: row.default_icon,
            depth,
            headings,
        });
    }
    entries
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// Open the picker over `notebook`'s window; the chosen host opens in a
/// new tab of `notebook`.
pub fn show(notebook: &gtk4::Notebook, store: &Rc<SessionStore>) {
    let rows = collect_rows(store).unwrap_or_else(|err| {
        eprintln!("Wayterm: failed to list sessions for the host picker: {err}");
        Vec::new()
    });
    let entries = Rc::new(picker_entries(&rows));

    let window = gtk4::Window::builder()
        .title("New Tab")
        .modal(true)
        .default_width(360)
        .default_height(420)
        .build();
    if let Some(parent) = window_of(notebook) {
        window.set_transient_for(Some(&parent));
        window.set_destroy_with_parent(true);
    }

    let theme = gtk4::IconTheme::for_display(&notebook.display());
    let icon = move |custom: Option<&str>, default: &str| {
        resolve_icon(custom, default, |name| theme.has_icon(name)).to_string()
    };
    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Single);
    list.set_activate_on_single_click(false);
    for entry in entries.iter() {
        let row = gtk4::ListBoxRow::new();
        row.set_child(Some(&line(
            &icon(entry.icon.as_deref(), entry.default_icon),
            &entry.name,
            entry.depth,
        )));
        list.append(&row);
    }
    let headings: Vec<Option<gtk4::Widget>> = entries
        .iter()
        .map(|entry| {
            (!entry.headings.is_empty()).then(|| {
                let column = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                for heading in &entry.headings {
                    let line = line(
                        &icon(heading.icon.as_deref(), FOLDER_ICON),
                        &heading.name,
                        heading.depth,
                    );
                    line.add_css_class("dim-label");
                    column.append(&line);
                }
                column.upcast()
            })
        })
        .collect();
    list.set_header_func(move |row, _| {
        let heading = usize::try_from(row.index())
            .ok()
            .and_then(|index| headings.get(index).cloned().flatten());
        row.set_header(heading.as_ref());
    });

    let (for_activate, notebook, store) = (window.clone(), notebook.clone(), store.clone());
    list.connect_row_activated(move |_, row| {
        let picked = usize::try_from(row.index())
            .ok()
            .and_then(|index| entries.get(index));
        let Some(picked) = picked else {
            return;
        };
        for_activate.close();
        open(&notebook, &store, picked.session_id);
    });

    let keys = gtk4::EventControllerKey::new();
    let for_keys = window.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            for_keys.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();
    window.set_child(Some(&scrolled));
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
        first.grab_focus();
    }
    window.present();
}

/// An icon and a name, indented `depth` levels: one host or heading.
fn line(icon: &str, name: &str, depth: usize) -> gtk4::Box {
    let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    set_margins(&line, 4);
    let depth = i32::try_from(depth).unwrap_or(i32::MAX / INDENT);
    line.set_margin_start(8 + depth * INDENT);
    line.append(&gtk4::Image::from_icon_name(icon));
    line.append(&gtk4::Label::builder().label(name).xalign(0.0).build());
    line
}

/// Open the picked host: the local shell (`None`) or a saved session.
fn open(notebook: &gtk4::Notebook, store: &Rc<SessionStore>, session_id: Option<i64>) {
    let config = match session_id {
        None => local_shell_session(),
        Some(id) => match store.get_session(id) {
            Ok(config) => config,
            Err(err) => return eprintln!("Wayterm: failed to load session {id}: {err}"),
        },
    };
    SessionTab::open(notebook, &config, store);
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod gtk_tests {
    use super::*;

    /// The picker lists the local shell first and selected, headings sit
    /// on rows without being rows, and Return opens the selection.
    pub(in crate::ui) fn picker_opens_the_selected_host() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let _entered = runtime.enter();
        let store = Rc::new(SessionStore::in_memory().expect("in-memory store"));
        let folder = store.insert_folder("Lab", None).expect("folder");
        store
            .insert_session(&SessionConfig {
                name: "box".into(),
                session_type: SessionType::LocalShell,
                local_shell: Some("/bin/sh".into()),
                folder_id: Some(folder),
                ..Default::default()
            })
            .expect("session");
        let notebook = gtk4::Notebook::new();
        let parent = gtk4::Window::new();
        parent.set_child(Some(&notebook));
        parent.present();

        show(&notebook, &store);
        let picker = gtk4::Window::list_toplevels()
            .into_iter()
            .filter_map(|w| w.downcast::<gtk4::Window>().ok())
            .find(|w| w.title().as_deref() == Some("New Tab"))
            .expect("the picker is open");
        let list = picker
            .child()
            .and_downcast::<gtk4::ScrolledWindow>()
            .and_then(|scrolled| scrolled.child())
            .and_downcast::<gtk4::Viewport>()
            .and_then(|viewport| viewport.child())
            .and_downcast::<gtk4::ListBox>()
            .expect("the host list");
        let first = list.row_at_index(0).expect("local shell row");
        assert_eq!(list.selected_row(), Some(first));
        let second = list.row_at_index(1).expect("the session row");
        assert!(second.header().is_some(), "the folder heads its session");
        assert!(list.row_at_index(2).is_none(), "folders are not rows");

        // Activating the session row opens it in a new tab and closes the
        // picker.
        list.select_row(Some(&second));
        second.emit_activate();
        assert!(!picker.is_visible());
        assert_eq!(notebook.n_pages(), 1);
        assert_eq!(
            crate::ui::tab::current_session_id(&notebook),
            store
                .list_sessions(Some(folder))
                .unwrap()
                .first()
                .map(|s| s.id)
        );
        // Dropping the page cancels its terminal tasks.
        notebook.remove_page(Some(0));
        for _ in 0..20 {
            glib::MainContext::default().iteration(false);
        }
        parent.destroy();
    }
}
