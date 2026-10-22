//! The session dialog: create or edit a [`SessionConfig`] (doc 09).
//!
//! [`config_from_fields`] (in `model`) holds all of the mapping and is
//! display-free; the GTK part only gathers [`DialogFields`]. Choosing a
//! type reveals that type's group (SSH: auth, jump host, forwards, tmux;
//! serial: line settings; RDP: domain, resolution, depth; local: shell and
//! directory; VNC: a note that its password lives in the keyring). Only the selected group's values are stored.
//!
//! For the password method the SSH group also takes a password, kept only
//! long enough to hand to the keyring when "Save password to keyring" is
//! checked — the session row never holds it (doc 05).
//!
//! The tmux entry shows the full remote name (`wayterm-<suffix>`), which
//! is what `tmux list-sessions` prints; the row stores the bare suffix.
//! The entry follows the Name field until the user edits it.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::prelude::*;
use zeroize::Zeroizing;

use crate::store::model::{
    vocab_index, AuthMethod, Folder, SessionConfig, SessionType, DEFAULT_SERIAL_BAUD,
    RDP_COLOR_DEPTHS, RDP_COLOR_DEPTH_DEFAULT, SERIAL_DATA_BITS, SERIAL_DATA_BITS_DEFAULT,
    SERIAL_FLOW_CONTROLS, SERIAL_FLOW_CONTROL_DEFAULT, SERIAL_PARITIES, SERIAL_PARITY_DEFAULT,
    SERIAL_STOP_BITS, SERIAL_STOP_BITS_DEFAULT,
};
use crate::transport::credentials::CredentialError;
use crate::ui::icons::{session_type_icon, IconPicker};
use crate::ui::tunnels_dialog::editor::RuleEditor;
use crate::ui::tunnels_dialog::model::{drafts_from_rules, ForwardRuleDraft, V2_NOTE};
use crate::ui::widgets::{button_row, dim_label, error_label, set_margins, Modal};

mod model;
pub use model::*;

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// The VNC group's note: its password lives in the keyring.
const VNC_KEYRING_NOTE: &str = "The VNC password is looked up in your OS keyring \
                                (saved sessions only) — never stored in Wayterm's database. \
                                Save it with your keyring tools (or via KeePassXC's Secret \
                                Service); the connect prompt stores nothing.";
/// The tmux check box's label.
const TMUX_LABEL: &str = "Persist via tmux — survive reconnects; sessions on the remote host \
                          are named wayterm-<session name>";

/// The save check box's label once a password is typed.
const SAVE_PASSWORD_LABEL: &str = "Save password to keyring";
/// The save check box's label while the password is empty.
const SAVE_PASSWORD_LABEL_EMPTY: &str = "Save password to keyring (enter password to enable)";

// ---------------------------------------------------------------------------
// Store-on-save decisions (display-free)
// ---------------------------------------------------------------------------

/// The password entry and its checkbox show for the password method only.
fn password_rows_visible(auth_index: u32) -> bool {
    auth_index == auth_index_for(Some(&AuthMethod::Password))
}

/// The checkbox's sensitivity and label: it needs a typed password.
fn save_password_state(password_typed: bool) -> (bool, &'static str) {
    if password_typed {
        (true, SAVE_PASSWORD_LABEL)
    } else {
        (false, SAVE_PASSWORD_LABEL_EMPTY)
    }
}

/// Store on save only for an SSH session on the password method, with the
/// box checked and a password typed.
fn should_store_password(type_index: u32, auth_index: u32, checked: bool, password: &str) -> bool {
    session_type_from_index(type_index).stores_password_on_save()
        && password_rows_visible(auth_index)
        && checked
        && !password.is_empty()
}

/// The inline line shown when the keyring refuses the password.
fn store_error_message(err: &CredentialError) -> String {
    format!("Could not store password: {err}")
}

/// Persists the saved session: its id, or `None` when saving failed (the
/// callback reports why).
pub type SaveSessionFn = Box<dyn Fn(&SessionConfig) -> Option<i64>>;
/// Stores a typed password for `(session_id, session_name)`.
pub type StorePasswordFn =
    Box<dyn Fn(i64, &str, &Zeroizing<Vec<u8>>) -> Result<(), CredentialError>>;

/// Labelled rows in a two-column grid.
struct FormGrid {
    /// The grid.
    grid: gtk4::Grid,
    /// The next free row.
    row: i32,
}

impl FormGrid {
    /// An empty grid with `margin` around it.
    fn new(margin: i32) -> Self {
        let grid = gtk4::Grid::builder()
            .row_spacing(6)
            .column_spacing(12)
            .build();
        set_margins(&grid, margin);
        Self { grid, row: 0 }
    }

    /// Add `widget` under `label`.
    fn add(&mut self, label: &str, widget: &impl IsA<gtk4::Widget>) {
        let label = gtk4::Label::builder()
            .label(label)
            .halign(gtk4::Align::End)
            .build();
        self.grid.attach(&label, 0, self.row, 1, 1);
        self.grid.attach(widget, 1, self.row, 1, 1);
        self.row += 1;
    }

    /// Add a text entry.
    fn entry(&mut self, label: &str, placeholder: &str) -> gtk4::Entry {
        let entry = gtk4::Entry::builder()
            .hexpand(true)
            .placeholder_text(placeholder)
            .activates_default(true)
            .build();
        self.add(label, &entry);
        entry
    }

    /// Add a number field from `min` to `max`.
    fn spin(&mut self, label: &str, min: f64, max: f64) -> gtk4::SpinButton {
        let spin = gtk4::SpinButton::with_range(min, max, 1.0);
        self.add(label, &spin);
        spin
    }

    /// Add a dropdown of `values`.
    fn dropdown<T: ToString>(&mut self, label: &str, values: &[T]) -> gtk4::DropDown {
        let labels: Vec<String> = values.iter().map(ToString::to_string).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let dropdown = gtk4::DropDown::builder()
            .model(&gtk4::StringList::new(&labels))
            .hexpand(true)
            .build();
        self.add(label, &dropdown);
        dropdown
    }

    /// A widget spanning both columns.
    fn span(&mut self, widget: &impl IsA<gtk4::Widget>) {
        self.grid.attach(widget, 0, self.row, 2, 1);
        self.row += 1;
    }

    /// The rows under a title, in a revealer that starts hidden.
    fn into_group(self, title: &str) -> gtk4::Revealer {
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let title = gtk4::Label::builder()
            .label(title)
            .halign(gtk4::Align::Start)
            .margin_start(6)
            .margin_top(8)
            .build();
        content.append(&title);
        content.append(&self.grid);
        gtk4::Revealer::builder()
            .child(&content)
            .transition_type(gtk4::RevealerTransitionType::SlideDown)
            .build()
    }
}

/// The per-type groups, by type index.
#[derive(Clone)]
struct TypeGroups([gtk4::Revealer; 5]);

impl TypeGroups {
    /// Show only the group for `type_index`.
    fn apply(&self, type_index: u32) {
        for (index, group) in self.0.iter().enumerate() {
            group.set_reveal_child(index as u32 == type_index);
        }
    }
}

/// The tmux entry follows the Name field until the user edits it.
#[derive(Clone, Default)]
struct TmuxNameFollow {
    /// The user edited the tmux name; stop following Name.
    overridden: Rc<Cell<bool>>,
    /// Set while we write the entry ourselves, so `changed` is not taken
    /// for a user edit.
    programmatic: Rc<Cell<bool>>,
}

impl TmuxNameFollow {
    /// Write `text` without taking it for a user edit.
    fn fill(&self, entry: &gtk4::Entry, text: &str) {
        self.programmatic.set(true);
        entry.set_text(text);
        self.programmatic.set(false);
    }
}

/// Every input widget of the dialog.
#[derive(Clone)]
struct Widgets {
    /// Session name.
    name: gtk4::Entry,
    /// Type: ssh, rdp, vnc, serial, local.
    session_type: gtk4::DropDown,
    /// Folder.
    folder: gtk4::DropDown,
    /// Host.
    host: gtk4::Entry,
    /// Port; 0 means the type's default.
    port: gtk4::SpinButton,
    /// Username.
    username: gtk4::Entry,
    /// SSH auth method.
    auth: gtk4::DropDown,
    /// The password and save rows (password method only).
    password_rows: gtk4::Box,
    /// Password to save; never stored in the row.
    password: gtk4::PasswordEntry,
    /// "Save password to keyring".
    save_password: gtk4::CheckButton,
    /// Icon picker.
    icon: IconPicker,
    /// Persist via tmux.
    tmux: gtk4::CheckButton,
    /// tmux session name, shown with its prefix.
    tmux_name: gtk4::Entry,
    /// Jump host.
    jump: gtk4::DropDown,
    /// The session id behind each jump entry; `None` = direct.
    jump_ids: Vec<Option<i64>>,
    /// Forward rules.
    forward_rules: RuleEditor,
    /// Forward-rule validation error.
    forward_error: gtk4::Label,
    /// Serial device.
    serial_device: gtk4::Entry,
    /// Baud rate.
    serial_baud: gtk4::SpinButton,
    /// Data bits.
    serial_data_bits: gtk4::DropDown,
    /// Parity.
    serial_parity: gtk4::DropDown,
    /// Stop bits.
    serial_stop_bits: gtk4::DropDown,
    /// Flow control.
    serial_flow_control: gtk4::DropDown,
    /// RDP domain.
    rdp_domain: gtk4::Entry,
    /// RDP resolution, `WIDTHxHEIGHT`.
    rdp_resolution: gtk4::Entry,
    /// RDP color depth.
    rdp_color_depth: gtk4::DropDown,
    /// Local shell.
    local_shell: gtk4::Entry,
    /// Local working directory.
    local_working_dir: gtk4::Entry,
}

impl Widgets {
    /// The current values over `base`, the edited row.
    fn fields(&self, base: &SessionConfig) -> DialogFields {
        DialogFields {
            name: self.name.text().into(),
            type_index: self.session_type.selected(),
            folder_index: self.folder.selected(),
            host: self.host.text().into(),
            port: self.port.value() as u32,
            username: self.username.text().into(),
            auth_index: self.auth.selected(),
            icon: self.icon.icon().unwrap_or_default(),
            tmux_enabled: self.tmux.is_active(),
            tmux_session_name: self.tmux_name.text().into(),
            jump_session_id: self
                .jump_ids
                .get(self.jump.selected() as usize)
                .copied()
                .flatten(),
            forward_drafts: self.forward_rules.drafts(),
            serial_device: self.serial_device.text().into(),
            serial_baud: self.serial_baud.value() as u32,
            serial_data_bits_index: self.serial_data_bits.selected(),
            serial_parity_index: self.serial_parity.selected(),
            serial_stop_bits_index: self.serial_stop_bits.selected(),
            serial_flow_control_index: self.serial_flow_control.selected(),
            rdp_domain: self.rdp_domain.text().into(),
            rdp_resolution: self.rdp_resolution.text().into(),
            rdp_color_depth_index: self.rdp_color_depth.selected(),
            local_shell: self.local_shell.text().into(),
            local_working_dir: self.local_working_dir.text().into(),
            base: base.clone(),
        }
    }

    /// Show the password rows for the password method.
    fn apply_auth(&self) {
        self.password_rows
            .set_visible(password_rows_visible(self.auth.selected()));
    }

    /// The checkbox follows whether a password is typed; clearing the
    /// password unchecks it.
    fn apply_password_typed(&self) {
        let typed = !self.password.text().is_empty();
        let (sensitive, label) = save_password_state(typed);
        self.save_password.set_sensitive(sensitive);
        self.save_password.set_label(Some(label));
        if !typed {
            self.save_password.set_active(false);
        }
    }

    /// The typed password when it should go to the keyring on save.
    fn password_to_store(&self) -> Option<Zeroizing<Vec<u8>>> {
        let password = Zeroizing::new(self.password.text().to_string());
        should_store_password(
            self.session_type.selected(),
            self.auth.selected(),
            self.save_password.is_active(),
            &password,
        )
        .then(|| Zeroizing::new(password.as_bytes().to_vec()))
    }

    /// Fill every widget from `config`.
    fn fill(&self, config: &SessionConfig, folders: &[Folder]) {
        let text = |value: &Option<String>| value.clone().unwrap_or_default();
        self.name.set_text(&config.name);
        self.session_type
            .set_selected(type_index_for(&config.session_type));
        let folder_index = config
            .folder_id
            .and_then(|id| folders.iter().position(|f| f.id == id))
            .map_or(0, |i| i as u32 + 1);
        self.folder.set_selected(folder_index);
        self.host.set_text(&text(&config.host));
        let port = config
            .port
            .or_else(|| config.session_type.default_port())
            .unwrap_or(PORT_SPIN_PLACEHOLDER);
        self.port.set_value(port.into());
        self.username.set_text(&text(&config.username));
        self.auth
            .set_selected(auth_index_for(config.auth_method.as_ref()));
        self.icon
            .set_default(session_type_icon(&config.session_type));
        self.icon.set_icon(config.icon.as_deref());
        self.tmux.set_active(config.tmux_enabled);
        self.serial_device.set_text(&text(&config.serial_device));
        let baud = config
            .serial_baud_rate
            .filter(|b| *b > 0)
            .unwrap_or(DEFAULT_SERIAL_BAUD);
        self.serial_baud.set_value(baud.into());
        self.serial_data_bits.set_selected(vocab_index(
            &SERIAL_DATA_BITS,
            config.serial_data_bits,
            SERIAL_DATA_BITS_DEFAULT,
        ));
        self.serial_parity.set_selected(vocab_index(
            &SERIAL_PARITIES,
            config.serial_parity.as_deref(),
            SERIAL_PARITY_DEFAULT,
        ));
        self.serial_stop_bits.set_selected(vocab_index(
            &SERIAL_STOP_BITS,
            config.serial_stop_bits,
            SERIAL_STOP_BITS_DEFAULT,
        ));
        self.serial_flow_control.set_selected(vocab_index(
            &SERIAL_FLOW_CONTROLS,
            config.serial_flow_control.as_deref(),
            SERIAL_FLOW_CONTROL_DEFAULT,
        ));
        self.rdp_domain.set_text(&text(&config.rdp_domain));
        self.rdp_resolution
            .set_text(&format_resolution(config.rdp_width, config.rdp_height));
        self.rdp_color_depth.set_selected(vocab_index(
            &RDP_COLOR_DEPTHS,
            config.rdp_color_depth,
            RDP_COLOR_DEPTH_DEFAULT,
        ));
        self.local_shell.set_text(&text(&config.local_shell));
        self.local_working_dir
            .set_text(&text(&config.local_working_dir));
    }
}

/// The dialog. Build with [`Self::new_session`] or [`Self::edit`], then
/// [`Self::run`] it.
pub struct SessionDialog {
    /// The modal; its result is the saved row.
    modal: Modal<SessionConfig>,
    /// The input widgets.
    widgets: Widgets,
    /// The per-type groups.
    groups: TypeGroups,
    /// tmux-name following.
    tmux_follow: TmuxNameFollow,
    /// The edited row; a new session's is the default.
    base: SessionConfig,
    /// Folders the folder dropdown offers.
    folders: Vec<Folder>,
    /// Other sessions, for the jump-host picker.
    peers: Vec<SessionConfig>,
    /// The dialog's layout.
    vbox: gtk4::Box,
}

impl SessionDialog {
    /// A dialog for a new session.
    pub fn new_session(parent: &gtk4::Window, folders: &[Folder], peers: &[SessionConfig]) -> Self {
        Self::build(parent, folders, peers, 0, None, &[])
    }

    /// A dialog editing `config`.
    pub fn edit(
        parent: &gtk4::Window,
        config: &SessionConfig,
        folders: &[Folder],
        peers: &[SessionConfig],
    ) -> Self {
        let mut dialog = Self::build(
            parent,
            folders,
            peers,
            config.id,
            config.jump_session_id,
            &drafts_from_rules(config.forward_rules.as_ref()),
        );
        // Fills the type before the port: a type change resets the port.
        dialog.widgets.fill(config, folders);
        // A pre-fill is not a user edit, but a stored name is the user's
        // own choice and keeps winning over later renames.
        dialog
            .tmux_follow
            .fill(&dialog.widgets.tmux_name, &config.tmux_name_for_dialog());
        if config.tmux_session_name.is_some() {
            dialog.tmux_follow.overridden.set(true);
        }
        dialog.base = config.clone();
        // `set_selected` only notifies on change, so apply SSH explicitly.
        dialog.groups.apply(type_index_for(&config.session_type));
        dialog.widgets.apply_auth();
        dialog
    }

    /// Run modally; `None` on cancel. OK only closes on success: an
    /// invalid forward draft keeps the dialog open with the error inline,
    /// and the jump choice resolves through [`resolve_jump`] on save.
    ///
    /// OK saves through `save`; then, when "Save password to keyring" is
    /// checked, the typed password goes to `store_password` with the saved
    /// id. A refused store leaves the session saved and the dialog open on
    /// the reason, with OK turned into Close.
    pub fn run(
        self,
        save: SaveSessionFn,
        store_password: StorePasswordFn,
    ) -> Option<SessionConfig> {
        let ok = gtk4::Button::with_label("OK");
        let cancel = self.modal.button("Cancel", || None);
        let password_error = error_label();
        password_error.set_visible(false);
        set_margins(&password_error, 12);
        self.vbox.append(&password_error);
        let saved = Rc::new(Cell::new(false));
        {
            let (window, result, widgets, base, folders, peers) = (
                self.modal.window.clone(),
                self.modal.result.clone(),
                self.widgets.clone(),
                self.base.clone(),
                self.folders.clone(),
                self.peers.clone(),
            );
            let password_error = password_error.clone();
            ok.connect_clicked(move |ok| {
                if saved.get() {
                    window.close();
                    return;
                }
                if let Err(message) = widgets.forward_rules.rules() {
                    widgets.forward_error.set_text(&message);
                    return;
                }
                let fields = widgets.fields(&base);
                let mut config = config_from_fields(&fields, &folders, &peers);
                let secret = widgets.password_to_store();
                let Some(id) = save(&config) else {
                    window.close();
                    return;
                };
                config.id = id;
                let stored = secret.map(|secret| store_password(id, &config.name, &secret));
                *result.borrow_mut() = Some(config);
                match stored {
                    Some(Err(err)) => {
                        saved.set(true);
                        password_error.set_text(&store_error_message(&err));
                        password_error.set_visible(true);
                        ok.set_label("Close");
                    }
                    _ => window.close(),
                }
            });
            self.modal.window.set_default_widget(Some(&ok));
        }
        let buttons = button_row(&[&cancel, &ok]);
        set_margins(&buttons, 12);
        self.vbox.append(&buttons);
        self.modal.run()
    }

    /// Build the dialog around `base`.
    fn build(
        parent: &gtk4::Window,
        folders: &[Folder],
        peers: &[SessionConfig],
        self_id: i64,
        current_jump: Option<i64>,
        initial_drafts: &[ForwardRuleDraft],
    ) -> Self {
        let modal = Modal::new("Session", Some(parent));
        modal.window.set_default_size(560, 680);

        let mut form = FormGrid::new(12);
        let name = form.entry("Name:", "");
        let session_type = form.dropdown("Type:", &TYPE_LABELS);
        let folder_names: Vec<String> = std::iter::once("None (root)".to_string())
            .chain(folder_labels(folders))
            .collect();
        let folder = form.dropdown("Folder:", &folder_names);
        let host = form.entry("Host:", "");
        let port = form.spin("Port:", 1.0, 65535.0);
        let username = form.entry("Username:", "");
        let auth = form.dropdown("Auth Method:", &AUTH_LABELS);
        let icon = IconPicker::new(session_type_icon(&SessionType::SSH));
        form.add("Icon:", icon.widget());

        let mut ssh = FormGrid::new(6);
        // Password (password method only): never stored on the row, only
        // in the keyring when the box is checked.
        let password = gtk4::PasswordEntry::builder()
            .show_peek_icon(true)
            .hexpand(true)
            .activates_default(true)
            .placeholder_text("only saved to the keyring")
            .build();
        let password_label = gtk4::Label::builder()
            .label("Password:")
            .halign(gtk4::Align::End)
            .build();
        let password_line = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        password_line.append(&password_label);
        password_line.append(&password);
        let save_password = gtk4::CheckButton::with_label(SAVE_PASSWORD_LABEL_EMPTY);
        save_password.set_sensitive(false);
        let password_rows = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        password_rows.append(&password_line);
        password_rows.append(&save_password);
        ssh.span(&password_rows);

        let tmux = gtk4::CheckButton::with_label(TMUX_LABEL);
        ssh.span(&tmux);
        let tmux_name = ssh.entry("tmux session:", "wayterm-<session name>");

        // Jump host (doc 04): one ProxyJump hop through another SSH
        // session. Depth>1 choices stay listed but are refused with an
        // inline message; a stale stored id shows the missing entry.
        let (jump_labels, jump_ids, jump_selected) =
            jump_picker_model(peers, self_id, current_jump);
        let jump = ssh.dropdown("Jump host:", &jump_labels);
        jump.set_selected(jump_selected);
        jump.set_tooltip_text(Some(
            "The SSH session to connect through (one ProxyJump hop, doc 04). Depth > 1 is refused.",
        ));
        let jump_message = error_label();
        if let Some(message) = jump_error(peers, self_id, current_jump) {
            jump_message.set_text(&message);
        } else {
            jump_message.set_visible(false);
        }
        ssh.span(&jump_message);
        let last_good = Rc::new(Cell::new(jump_selected));
        let choosing = Rc::new(Cell::new(false));
        {
            let (ids, message, last_good, choosing) = (
                jump_ids.clone(),
                jump_message.clone(),
                last_good.clone(),
                choosing.clone(),
            );
            let peers = peers.to_vec();
            jump.connect_notify_local(Some("selected"), move |dropdown, _| {
                if choosing.get() {
                    return;
                }
                let desired = ids.get(dropdown.selected() as usize).copied().flatten();
                match jump_error(&peers, self_id, desired) {
                    None => {
                        message.set_visible(false);
                        message.set_text("");
                        last_good.set(dropdown.selected());
                    }
                    Some(reason) => {
                        message.set_text(&reason);
                        message.set_visible(true);
                        choosing.set(true);
                        dropdown.set_selected(last_good.get());
                        choosing.set(false);
                    }
                }
            });
        }

        // Forward rules (doc 04 §Port forwarding): the same drafts the
        // Tunnels dialog edits for a connected session.
        ssh.span(&dim_label(
            "Port forwards (Local starts at connect; Remote/Dynamic are v2):",
        ));
        ssh.span(&dim_label(V2_NOTE));
        let forward_rules = RuleEditor::new(None);
        ssh.span(forward_rules.widget());
        for draft in initial_drafts {
            forward_rules.add(draft, None);
        }
        let forward_add = gtk4::Button::with_label("Add forward");
        let for_add = forward_rules.clone();
        forward_add.connect_clicked(move |_| for_add.add(&ForwardRuleDraft::default(), None));
        ssh.span(&forward_add);
        let forward_error = error_label();
        ssh.span(&forward_error);

        let mut serial = FormGrid::new(6);
        let serial_device = serial.entry("Device:", "/dev/ttyUSB0");
        let serial_baud = serial.spin("Baud:", 50.0, 4_000_000.0);
        let serial_data_bits = serial.dropdown("Data Bits:", &SERIAL_DATA_BITS);
        let serial_parity = serial.dropdown("Parity:", &SERIAL_PARITIES);
        let serial_stop_bits = serial.dropdown("Stop Bits:", &SERIAL_STOP_BITS);
        let serial_flow_control = serial.dropdown("Flow Control:", &SERIAL_FLOW_CONTROLS);

        let mut rdp = FormGrid::new(6);
        let rdp_domain = rdp.entry("Domain:", "optional");
        let rdp_resolution = rdp.entry("Resolution:", "1024x768 (doc 08 default)");
        let rdp_color_depth = rdp.dropdown("Color Depth:", &RDP_COLOR_DEPTHS);

        let mut vnc = FormGrid::new(6);
        vnc.span(
            &gtk4::Label::builder()
                .label(VNC_KEYRING_NOTE)
                .wrap(true)
                .xalign(0.0)
                .build(),
        );

        let mut local = FormGrid::new(6);
        let local_shell = local.entry("Shell:", "$SHELL (default)");
        let local_working_dir = local.entry("Working Directory:", "~ (default)");

        // In TYPE_INDEX_* order.
        let groups = TypeGroups([
            ssh.into_group("SSH"),
            rdp.into_group("RDP"),
            vnc.into_group("VNC"),
            serial.into_group("Serial"),
            local.into_group("Local Shell"),
        ]);

        let widgets = Widgets {
            name,
            session_type,
            folder,
            host,
            port,
            username,
            auth,
            password_rows,
            password,
            save_password,
            icon,
            tmux,
            tmux_name,
            jump,
            jump_ids,
            forward_rules,
            forward_error,
            serial_device,
            serial_baud,
            serial_data_bits,
            serial_parity,
            serial_stop_bits,
            serial_flow_control,
            rdp_domain,
            rdp_resolution,
            rdp_color_depth,
            local_shell,
            local_working_dir,
        };
        let defaults = DialogFields::default();
        widgets.serial_baud.set_value(defaults.serial_baud.into());
        for (dropdown, index) in [
            (&widgets.serial_data_bits, defaults.serial_data_bits_index),
            (&widgets.serial_parity, defaults.serial_parity_index),
            (&widgets.serial_stop_bits, defaults.serial_stop_bits_index),
            (
                &widgets.serial_flow_control,
                defaults.serial_flow_control_index,
            ),
            (&widgets.rdp_color_depth, defaults.rdp_color_depth_index),
        ] {
            dropdown.set_selected(index);
        }

        // A type change reveals its group, resets the port to its default
        // (so a stale 22 is never stored for RDP), and swaps the default
        // icon.
        let (for_type, port, icon) = (groups.clone(), widgets.port.clone(), widgets.icon.clone());
        widgets
            .session_type
            .connect_notify_local(Some("selected"), move |dropdown, _| {
                for_type.apply(dropdown.selected());
                let session_type = session_type_from_index(dropdown.selected());
                let default = session_type.default_port();
                port.set_value(default.unwrap_or(PORT_SPIN_PLACEHOLDER).into());
                icon.set_default(session_type_icon(&session_type));
            });
        groups.apply(widgets.session_type.selected());
        // The handler above only runs on a change, so a new session would
        // otherwise start at the spin's minimum (1); `fill` overrides this
        // when editing.
        let initial_type = session_type_from_index(widgets.session_type.selected());
        widgets.port.set_value(
            initial_type
                .default_port()
                .unwrap_or(PORT_SPIN_PLACEHOLDER)
                .into(),
        );

        let for_auth = widgets.clone();
        widgets
            .auth
            .connect_notify_local(Some("selected"), move |_, _| for_auth.apply_auth());
        widgets.apply_auth();
        let for_password = widgets.clone();
        widgets
            .password
            .connect_changed(move |_| for_password.apply_password_typed());
        widgets.apply_password_typed();

        let tmux_follow = TmuxNameFollow::default();
        let (follow, entry) = (tmux_follow.clone(), widgets.tmux_name.clone());
        widgets.name.connect_changed(move |name| {
            if !follow.overridden.get() {
                follow.fill(&entry, &default_tmux_session_name(&name.text()));
            }
        });
        let follow = tmux_follow.clone();
        widgets.tmux_name.connect_changed(move |_| {
            if !follow.programmatic.get() {
                follow.overridden.set(true);
            }
        });
        tmux_follow.fill(&widgets.tmux_name, &default_tmux_session_name(""));

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        vbox.append(&form.grid);
        for group in &groups.0 {
            vbox.append(group);
        }
        // Takes the spare height, keeping the buttons at the bottom.
        let spacer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        vbox.append(&spacer);
        modal.window.set_child(Some(&vbox));

        Self {
            modal,
            widgets,
            groups,
            tmux_follow,
            base: SessionConfig::default(),
            folders: folders.to_vec(),
            peers: peers.to_vec(),
            vbox,
        }
    }
}

#[cfg(test)]
mod tests;

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    pub(in crate::ui) fn tmux_group_entry_follows_name_until_user_edits() {
        let revealed = |g: &gtk4::Revealer| g.reveals_child();

        let parent = gtk4::Window::new();
        let dialog = SessionDialog::new_session(&parent, &[], &[]);
        let w = &dialog.widgets;
        assert_eq!(w.tmux_name.text(), "wayterm-");
        assert!(!w.tmux.is_active());
        w.name.set_text("prod");
        assert_eq!(w.tmux_name.text(), "wayterm-prod");
        w.tmux_name.set_text("wayterm-custom");
        w.name.set_text("other");
        assert_eq!(w.tmux_name.text(), "wayterm-custom", "a user edit wins");
        dialog.groups.apply(TYPE_INDEX_SSH);
        assert!(revealed(&dialog.groups.0[TYPE_INDEX_SSH as usize]));
        dialog.groups.apply(TYPE_INDEX_SERIAL);
        assert!(!revealed(&dialog.groups.0[TYPE_INDEX_SSH as usize]));

        // Edit mode: a stored name is shown in full and survives renames.
        let stored = SessionConfig {
            id: 7,
            name: "prod".into(),
            port: Some(2222),
            tmux_enabled: true,
            tmux_session_name: Some("shared".into()),
            ..Default::default()
        };
        let dialog = SessionDialog::edit(&parent, &stored, &[], &[]);
        assert_eq!(dialog.widgets.icon.icon(), None);
        assert!(dialog.widgets.tmux.is_active());
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-shared");
        assert_eq!(dialog.widgets.port.value(), 2222.0);
        dialog.widgets.name.set_text("renamed");
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-shared");
        dialog.widgets.session_type.set_selected(TYPE_INDEX_LOCAL);
        assert_eq!(
            dialog.widgets.icon.shown_icon().as_deref(),
            Some("wayterm-terminal-symbolic"),
            "the default icon follows the type"
        );

        // Without a stored name, the derived one keeps following.
        let derived = SessionConfig {
            id: 8,
            name: "prod".into(),
            tmux_enabled: true,
            icon: Some("wayterm-homelab".into()),
            ..Default::default()
        };
        let dialog = SessionDialog::edit(&parent, &derived, &[], &[]);
        assert_eq!(
            dialog.widgets.icon.icon().as_deref(),
            Some("wayterm-homelab")
        );
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-prod");
        dialog.widgets.name.set_text("renamed");
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-renamed");
    }

    /// The jump picker lists SSH peers only, refuses depth>1 with a message
    /// and a revert, shows the missing entry for a stale id, and the
    /// forward editor adds rows that round-trip into drafts.
    pub(in crate::ui) fn jump_picker_and_forward_editor() {
        use crate::store::model::{ForwardKind, ForwardRule};

        let ssh = |id: i64, name: &str, jump: Option<i64>| SessionConfig {
            id,
            name: name.to_string(),
            session_type: crate::store::model::SessionType::SSH,
            jump_session_id: jump,
            ..Default::default()
        };
        let peers = vec![
            ssh(1, "bastion", None),
            ssh(2, "via-bastion", Some(1)),
            SessionConfig {
                id: 3,
                name: "win".to_string(),
                session_type: crate::store::model::SessionType::RDP,
                ..Default::default()
            },
        ];
        let parent = gtk4::Window::new();

        // New session: direct + SSH peers only (no self yet, no RDP).
        let dialog = SessionDialog::new_session(&parent, &[], &peers);
        assert_eq!(dialog.widgets.jump_ids, vec![None, Some(1), Some(2)]);
        assert_eq!(dialog.widgets.jump.selected(), 0);

        // Picking the depth>1 peer is refused: the selection reverts to
        // direct (the message text itself is covered display-free).
        dialog.widgets.jump.set_selected(2);
        assert_eq!(dialog.widgets.jump.selected(), 0, "depth>1 reverts");
        let fields = dialog.widgets.fields(&dialog.base);
        assert_eq!(fields.jump_session_id, None);

        // Picking the clean bastion sticks.
        dialog.widgets.jump.set_selected(1);
        assert_eq!(dialog.widgets.jump.selected(), 1);
        let fields = dialog.widgets.fields(&dialog.base);
        assert_eq!(fields.jump_session_id, Some(1));
        dialog.modal.window.close();

        // Edit with a stale id: the missing entry renders and is selected.
        let stale = SessionConfig {
            id: 9,
            name: "web".to_string(),
            session_type: crate::store::model::SessionType::SSH,
            jump_session_id: Some(77),
            forward_rules: Some(vec![ForwardRule {
                kind: ForwardKind::Local,
                bind_addr: "127.0.0.1".to_string(),
                bind_port: 8080,
                remote_host: "web1.lan".to_string(),
                remote_port: 80,
                enabled: true,
            }]),
            ..Default::default()
        };
        let dialog = SessionDialog::edit(&parent, &stale, &[], &peers);
        let selected = dialog.widgets.jump.selected() as usize;
        assert_eq!(dialog.widgets.jump_ids[selected], Some(77));
        let fields = dialog.widgets.fields(&dialog.base);
        assert_eq!(
            fields.jump_session_id,
            Some(77),
            "the raw picker value survives"
        );
        assert_eq!(
            config_from_fields(&fields, &[], &peers).jump_session_id,
            None,
            "saving a stale id clears it instead of violating the FK"
        );
        assert_eq!(dialog.widgets.forward_rules.rows().len(), 1);
        let fields = dialog.widgets.fields(&dialog.base);
        assert_eq!(
            fields.forward_drafts.len(),
            1,
            "stored rules load into the editor"
        );
        assert_eq!(fields.forward_drafts[0].bind_port, "8080");
        dialog.modal.window.close();
    }
}
