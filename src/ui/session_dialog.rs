//! Session config dialog — create/edit a [`SessionConfig`] (doc 06 / doc
//! 10 Task 9).
//!
//! The GTK side is display-heavy and cannot be unit-tested without a
//! display.  All mapping logic (DropDown index → enum, port defaulting,
//! identity-field preservation) is isolated in
//! [`config_from_fields`](config_from_fields) and exercised by the
//! tests in [`tests`].
//!
//! **Scope note for Task 9:** the plan Task 9 description mentions
//! type-specific conditional show/hide fields (e.g. serial device/baud).
//! That complexity is intentionally deferred to Task 10 when this dialog
//! is wired into the session tree, so Task 9 stays small and the dialog is
//! fully testable with a pure mapping function.  The dialog only exposes
//! the common dialog-editable fields from doc 06.

use crate::store::model::{AuthMethod, Folder, SessionConfig, SessionType};

/// Values extracted from the GTK widgets in the dialog.
///
/// Kept deliberately plain so [`config_from_fields`] can be tested without
/// initializing GTK.
#[derive(Clone, Debug)]
pub struct DialogFields {
    pub name: String,
    pub type_index: u32,
    pub folder_index: u32,
    pub host: String,
    pub port: u32,
    pub username: String,
    pub auth_index: u32,
    pub notes: String,

    // Identity fields from edit mode (not dialog-editable but carried
    // through to the returned config).
    pub id: i64,
    #[allow(dead_code)]
    pub folder_id: Option<i64>, // carried through in edit mode; config_from_fields ignores it in favour of folder_index
    pub last_connected: Option<String>,
    pub connect_count: u32,
}

impl Default for DialogFields {
    fn default() -> Self {
        Self {
            name: String::new(),
            type_index: 0,
            folder_index: 0,
            host: String::new(),
            port: 22,
            username: String::new(),
            auth_index: 0,
            notes: String::new(),
            id: 0,
            folder_id: None,
            last_connected: None,
            connect_count: 0,
        }
    }
}

/// Convert the plain widget-collected values into a fully populated
/// [`SessionConfig`].
///
/// - `type_index` maps to the [`SessionType`] variants in doc-06 order.
/// - `auth_index` maps to the [`AuthMethod`] variants in doc-06 order.
/// - `folder_index` is resolved against the folder list passed to the
///   dialog constructor: `0` means root, otherwise it indexes into
///   `folders` and stores that folder's id.
/// - Invalid `port` values (zero or > 65535) fall back to 22.
/// - Edit-mode identity fields are preserved unchanged.
pub fn config_from_fields(fields: &DialogFields, folders: &[Folder]) -> SessionConfig {
    let session_type = match fields.type_index {
        1 => SessionType::RDP,
        2 => SessionType::VNC,
        3 => SessionType::Serial,
        4 => SessionType::LocalShell,
        _ => SessionType::SSH,
    };

    let auth_method = match fields.auth_index {
        1 => Some(AuthMethod::Key),
        2 => Some(AuthMethod::Agent),
        3 => Some(AuthMethod::KeyboardInteractive),
        4 => Some(AuthMethod::None),
        _ => Some(AuthMethod::Password),
    };

    let folder_id = if fields.folder_index == 0 {
        None
    } else {
        folders
            .get((fields.folder_index as usize).saturating_sub(1))
            .map(|f| f.id)
    };

    let port = if (1..=65535).contains(&fields.port) {
        Some(fields.port as u16)
    } else {
        Some(22)
    };

    let host = if fields.host.trim().is_empty() {
        None
    } else {
        Some(fields.host.trim().to_string())
    };

    let username = if fields.username.trim().is_empty() {
        None
    } else {
        Some(fields.username.trim().to_string())
    };

    let notes = if fields.notes.trim().is_empty() {
        None
    } else {
        Some(fields.notes.trim().to_string())
    };

    SessionConfig {
        id: fields.id,
        name: fields.name.trim().to_string(),
        session_type,
        folder_id,
        host,
        port,
        username,
        auth_method,
        notes,
        last_connected: fields.last_connected.clone(),
        connect_count: fields.connect_count,
        ..Default::default()
    }
}

// The GTK4 constructors and helpers are only used by the UI layer; unit
// tests target the pure `config_from_fields` function.  Silence dead-code
// warnings in non-test builds until Task 10 wires the dialog into the tree.
#[allow(dead_code)]
mod gtk_dialog {
    use gtk4::prelude::*;

    use super::{config_from_fields, DialogFields, Folder, SessionConfig};

    /// Static labels shown in the Type DropDown.
    const TYPE_LABELS: &[&str] = &["ssh", "rdp", "vnc", "serial", "local"];

    /// Static labels shown in the Auth Method DropDown.
    const AUTH_LABELS: &[&str] = &["password", "key", "agent", "keyboard-interactive", "none"];

    /// Edit-mode metadata carried in a shared cell so the OK handler can read
    /// the current values without capturing `Self`.
    #[derive(Clone, Debug)]
    struct DialogState {
        edit_id: i64,
        edit_last_connected: Option<String>,
        edit_connect_count: u32,
    }

    /// Per-widget handles captured after construction so `edit()` can pre-fill.
    #[derive(Clone)]
    struct Widgets {
        name_entry: gtk4::Entry,
        type_dropdown: gtk4::DropDown,
        folder_dropdown: gtk4::DropDown,
        host_entry: gtk4::Entry,
        port_spin: gtk4::SpinButton,
        username_entry: gtk4::Entry,
        auth_dropdown: gtk4::DropDown,
        notes_view: gtk4::TextView,
    }

    /// GTK4 dialog for creating or editing a saved session.
    ///
    /// Only the common dialog-editable fields from doc 06 are exposed in this
    /// task (type-specific conditional fields are deferred to Task 10).  The
    /// dialog returns a fully populated [`SessionConfig`] on OK and `None` on
    /// cancel.
    pub struct SessionDialog {
        window: gtk4::ApplicationWindow,
        state: std::rc::Rc<std::cell::RefCell<DialogState>>,
        result: std::rc::Rc<std::cell::RefCell<Option<SessionConfig>>>,
        done: std::rc::Rc<std::cell::RefCell<bool>>,
    }

    impl SessionDialog {
        /// Create a dialog for a new session, with empty defaults and type SSH.
        pub fn new_session(parent: &gtk4::Window, folders: &[Folder]) -> Self {
            Self::build(parent, folders)
        }

        /// Create a dialog pre-filled from `config`.
        pub fn edit(parent: &gtk4::Window, config: &SessionConfig, folders: &[Folder]) -> Self {
            let this = Self::build(parent, folders);

            let widgets = this.widgets();
            widgets.name_entry.set_text(&config.name);

            let type_index = match config.session_type {
                super::SessionType::SSH => 0,
                super::SessionType::RDP => 1,
                super::SessionType::VNC => 2,
                super::SessionType::Serial => 3,
                super::SessionType::LocalShell => 4,
            };
            widgets.type_dropdown.set_selected(type_index);

            let folder_index = if let Some(id) = config.folder_id {
                folders
                    .iter()
                    .position(|f| f.id == id)
                    .map(|idx| (idx + 1) as u32)
                    .unwrap_or(0)
            } else {
                0
            };
            widgets.folder_dropdown.set_selected(folder_index);

            widgets
                .host_entry
                .set_text(config.host.as_deref().unwrap_or(""));
            widgets
                .port_spin
                .set_value(f64::from(config.port.unwrap_or(22)));
            widgets
                .username_entry
                .set_text(config.username.as_deref().unwrap_or(""));

            let auth_index = match config.auth_method {
                None => 4,
                Some(super::AuthMethod::Password) => 0,
                Some(super::AuthMethod::Key) => 1,
                Some(super::AuthMethod::Agent) => 2,
                Some(super::AuthMethod::KeyboardInteractive) => 3,
                Some(super::AuthMethod::None) => 4,
            };
            widgets.auth_dropdown.set_selected(auth_index);

            widgets
                .notes_view
                .buffer()
                .set_text(config.notes.as_deref().unwrap_or(""));

            *this.state.borrow_mut() = DialogState {
                edit_id: config.id,
                edit_last_connected: config.last_connected.clone(),
                edit_connect_count: config.connect_count,
            };

            this
        }

        /// Run the dialog modally and return the resulting [`SessionConfig`].
        ///
        /// Uses nested main-loop iteration, matching the proven pattern in
        /// [`session_tree::prompt_text`](crate::ui::session_tree::prompt_text).
        pub fn run(&self) -> Option<SessionConfig> {
            self.window.present();

            while !*self.done.borrow() {
                gtk4::glib::MainContext::default().iteration(true);
            }

            self.result.take()
        }

        /// Build the shared UI and wire OK / Cancel / close.
        fn build(parent: &gtk4::Window, folders: &[Folder]) -> Self {
            let window = gtk4::ApplicationWindow::builder()
                .title("Session")
                .modal(true)
                .default_width(420)
                .default_height(560)
                .resizable(true)
                .transient_for(parent)
                .destroy_with_parent(true)
                .build();

            let grid = gtk4::Grid::builder()
                .row_spacing(6)
                .column_spacing(12)
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build();

            let mut row = 0;

            let name_entry = Self::add_labeled_row(&grid, row, "Name:");
            row += 1;

            let type_dropdown = Self::add_dropdown_row(&grid, row, "Type:", TYPE_LABELS);
            row += 1;

            let folder_names: Vec<String> = std::iter::once("None (root)".to_string())
                .chain(folders.iter().map(|f| f.name.clone()))
                .collect();
            let folder_dropdown = Self::add_dropdown_row(
                &grid,
                row,
                "Folder:",
                &folder_names.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            row += 1;

            let host_entry = Self::add_labeled_row(&grid, row, "Host:");
            row += 1;

            let port_spin = gtk4::SpinButton::with_range(1.0, 65535.0, 1.0);
            Self::add_widget_row(&grid, row, "Port:", port_spin.upcast_ref::<gtk4::Widget>());
            row += 1;

            let username_entry = Self::add_labeled_row(&grid, row, "Username:");
            row += 1;

            let auth_dropdown = Self::add_dropdown_row(&grid, row, "Auth Method:", AUTH_LABELS);
            row += 1;

            let notes_view = gtk4::TextView::builder()
                .wrap_mode(gtk4::WrapMode::WordChar)
                .vexpand(true)
                .hexpand(true)
                .top_margin(6)
                .bottom_margin(6)
                .left_margin(6)
                .right_margin(6)
                .build();
            let notes_scroll = gtk4::ScrolledWindow::builder()
                .child(&notes_view)
                .vexpand(true)
                .hexpand(true)
                .has_frame(true)
                .height_request(120)
                .build();
            Self::add_widget_row(
                &grid,
                row,
                "Notes:",
                notes_scroll.upcast_ref::<gtk4::Widget>(),
            );
            // row is intentionally unused after the last row; keep the
            // increment for symmetry and future edits.

            let ok_button = gtk4::Button::with_label("OK");
            let cancel_button = gtk4::Button::with_label("Cancel");
            cancel_button.set_margin_end(12);

            let button_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            button_box.set_halign(gtk4::Align::End);
            button_box.set_margin_top(12);
            button_box.append(&cancel_button);
            button_box.append(&ok_button);

            let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            vbox.append(&grid);
            vbox.append(&button_box);
            window.set_child(Some(&vbox));

            let state = std::rc::Rc::new(std::cell::RefCell::new(DialogState {
                edit_id: 0,
                edit_last_connected: None,
                edit_connect_count: 0,
            }));
            let result = std::rc::Rc::new(std::cell::RefCell::new(None::<SessionConfig>));
            let done = std::rc::Rc::new(std::cell::RefCell::new(false));

            let result_ok = result.clone();
            let done_ok = done.clone();
            let state_ok = state.clone();
            let folders_for_ok = folders.to_vec();

            ok_button.connect_clicked({
                let window = window.clone();
                let name_entry = name_entry.clone();
                let type_dropdown = type_dropdown.clone();
                let folder_dropdown = folder_dropdown.clone();
                let host_entry = host_entry.clone();
                let port_spin = port_spin.clone();
                let username_entry = username_entry.clone();
                let auth_dropdown = auth_dropdown.clone();
                let notes_view = notes_view.clone();
                move |_| {
                    let notes = notes_view.buffer().text(
                        &notes_view.buffer().start_iter(),
                        &notes_view.buffer().end_iter(),
                        true,
                    );
                    let state = state_ok.borrow();
                    let fields = DialogFields {
                        name: name_entry.text().to_string(),
                        type_index: type_dropdown.selected(),
                        folder_index: folder_dropdown.selected(),
                        host: host_entry.text().to_string(),
                        port: port_spin.value() as u32,
                        username: username_entry.text().to_string(),
                        auth_index: auth_dropdown.selected(),
                        notes: notes.to_string(),
                        id: state.edit_id,
                        folder_id: None,
                        last_connected: state.edit_last_connected.clone(),
                        connect_count: state.edit_connect_count,
                    };
                    *result_ok.borrow_mut() = Some(config_from_fields(&fields, &folders_for_ok));
                    *done_ok.borrow_mut() = true;
                    window.close();
                }
            });

            let done_cancel = done.clone();
            cancel_button.connect_clicked({
                let window = window.clone();
                move |_| {
                    *done_cancel.borrow_mut() = true;
                    window.close();
                }
            });

            let done_close = done.clone();
            window.connect_close_request(move |window| {
                *done_close.borrow_mut() = true;
                window.set_visible(false);
                gtk4::glib::Propagation::Proceed
            });

            window.set_default_widget(Some(&ok_button));

            let widgets = Widgets {
                name_entry,
                type_dropdown,
                folder_dropdown,
                host_entry,
                port_spin,
                username_entry,
                auth_dropdown,
                notes_view,
            };

            // Keep the widget handles alive on the window so `edit()` can use
            // them after construction.
            unsafe {
                window.set_data("wayterm-session-dialog-widgets", widgets);
            }

            Self {
                window,
                state,
                result,
                done,
            }
        }

        /// Retrieve the widget handles stored on the window.
        fn widgets(&self) -> Widgets {
            unsafe {
                self.window
                    .data::<Widgets>("wayterm-session-dialog-widgets")
                    .map(|d| (*d.as_ptr()).clone())
                    .expect("dialog widgets missing")
            }
        }

        /// Add a label + [`gtk4::Entry`] row to `grid` and return the entry.
        fn add_labeled_row(grid: &gtk4::Grid, row: i32, label_text: &str) -> gtk4::Entry {
            let entry = gtk4::Entry::builder()
                .hexpand(true)
                .activates_default(true)
                .build();
            Self::add_widget_row(grid, row, label_text, entry.upcast_ref::<gtk4::Widget>());
            entry
        }

        /// Add a label + [`gtk4::DropDown`] row to `grid` and return the dropdown.
        fn add_dropdown_row(
            grid: &gtk4::Grid,
            row: i32,
            label_text: &str,
            labels: &[&str],
        ) -> gtk4::DropDown {
            let model = gtk4::StringList::new(labels);
            let dropdown = gtk4::DropDown::builder()
                .model(&model)
                .hexpand(true)
                .build();
            Self::add_widget_row(grid, row, label_text, dropdown.upcast_ref::<gtk4::Widget>());
            dropdown
        }

        /// Add a label + arbitrary widget row to `grid`.
        fn add_widget_row(grid: &gtk4::Grid, row: i32, label_text: &str, widget: &gtk4::Widget) {
            let label = gtk4::Label::builder()
                .label(label_text)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Center)
                .build();
            grid.attach(&label, 0, row, 1, 1);
            grid.attach(widget, 1, row, 1, 1);
        }
    }
}

pub use gtk_dialog::SessionDialog;

#[cfg(test)]
mod tests;
