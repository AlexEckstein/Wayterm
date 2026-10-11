//! The session dialog: create or edit a [`SessionConfig`] (doc 06).
//!
//! [`config_from_fields`] holds all of the mapping and is display-free;
//! the GTK part only gathers [`DialogFields`]. Choosing a type reveals that
//! type's group (SSH: tmux; serial: line settings; RDP: domain, resolution,
//! depth; local: shell and directory; VNC: a note that its password lives
//! in the keyring). Only the selected group's values are stored.
//!
//! The tmux entry shows the full remote name (`wayterm-<suffix>`), which
//! is what `tmux list-sessions` prints; the row stores the bare suffix.
//! The entry follows the Name field until the user edits it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

use crate::store::model::{
    strip_tmux_prefix, vocab_index, vocab_value, AuthMethod, Folder, SessionConfig, SessionType,
    DEFAULT_SERIAL_BAUD, RDP_COLOR_DEPTHS, RDP_COLOR_DEPTH_DEFAULT, SERIAL_DATA_BITS,
    SERIAL_DATA_BITS_DEFAULT, SERIAL_FLOW_CONTROLS, SERIAL_FLOW_CONTROL_DEFAULT, SERIAL_PARITIES,
    SERIAL_PARITY_DEFAULT, SERIAL_STOP_BITS, SERIAL_STOP_BITS_DEFAULT, TMUX_NAME_PREFIX,
};
use crate::ui::icons::{session_type_icon, IconPicker};
use crate::ui::widgets::{ok_cancel_row, set_margins, Modal};

// Type DropDown indices, in doc 06 order.
pub const TYPE_INDEX_SSH: u32 = 0;
pub const TYPE_INDEX_RDP: u32 = 1;
pub const TYPE_INDEX_VNC: u32 = 2;
pub const TYPE_INDEX_SERIAL: u32 = 3;
pub const TYPE_INDEX_LOCAL: u32 = 4;

const TYPE_LABELS: [&str; 5] = ["ssh", "rdp", "vnc", "serial", "local"];
const AUTH_LABELS: [&str; 5] = ["password", "key", "agent", "keyboard-interactive", "none"];

/// Shown in the Port spin for types without a port (it cannot show
/// "unset"); nothing is stored for them.
const PORT_SPIN_PLACEHOLDER: u16 = 22;

/// The dialog's widget values, kept plain so the mapping is testable
/// without GTK. `*_index` fields are DropDown positions.
#[derive(Clone, Debug)]
pub struct DialogFields {
    pub name: String,
    pub type_index: u32,
    /// 0 = root, otherwise 1 + the index into the folder list.
    pub folder_index: u32,
    pub host: String,
    pub port: u32,
    pub username: String,
    pub auth_index: u32,
    /// Icon name; empty = the type's default.
    pub icon: String,
    pub notes: String,
    pub tmux_enabled: bool,
    /// As shown: the full `wayterm-<suffix>` name.
    pub tmux_session_name: String,
    pub serial_device: String,
    pub serial_baud: u32,
    pub serial_data_bits_index: u32,
    pub serial_parity_index: u32,
    pub serial_stop_bits_index: u32,
    pub serial_flow_control_index: u32,
    pub rdp_domain: String,
    /// `WIDTHxHEIGHT`; empty means doc 08's 1024×768 at connect time.
    pub rdp_resolution: String,
    pub rdp_color_depth_index: u32,
    pub local_shell: String,
    pub local_working_dir: String,
    // Carried through from the edited row.
    pub id: i64,
    pub last_connected: Option<String>,
    pub connect_count: u32,
}

impl Default for DialogFields {
    fn default() -> Self {
        Self {
            name: String::new(),
            type_index: TYPE_INDEX_SSH,
            folder_index: 0,
            host: String::new(),
            port: 22,
            username: String::new(),
            auth_index: 0,
            icon: String::new(),
            notes: String::new(),
            tmux_enabled: false,
            tmux_session_name: String::new(),
            serial_device: String::new(),
            serial_baud: DEFAULT_SERIAL_BAUD,
            serial_data_bits_index: default_index(&SERIAL_DATA_BITS, SERIAL_DATA_BITS_DEFAULT),
            serial_parity_index: default_index(&SERIAL_PARITIES, SERIAL_PARITY_DEFAULT),
            serial_stop_bits_index: default_index(&SERIAL_STOP_BITS, SERIAL_STOP_BITS_DEFAULT),
            serial_flow_control_index: default_index(
                &SERIAL_FLOW_CONTROLS,
                SERIAL_FLOW_CONTROL_DEFAULT,
            ),
            rdp_domain: String::new(),
            rdp_resolution: String::new(),
            rdp_color_depth_index: default_index(&RDP_COLOR_DEPTHS, RDP_COLOR_DEPTH_DEFAULT),
            local_shell: String::new(),
            local_working_dir: String::new(),
            id: 0,
            last_connected: None,
            connect_count: 0,
        }
    }
}

fn default_index<T: Copy + PartialEq>(values: &[T], default: T) -> u32 {
    vocab_index(values, Some(default), default)
}

/// DropDown index → type; anything unknown is SSH.
pub fn session_type_from_index(index: u32) -> SessionType {
    match index {
        TYPE_INDEX_RDP => SessionType::RDP,
        TYPE_INDEX_VNC => SessionType::VNC,
        TYPE_INDEX_SERIAL => SessionType::Serial,
        TYPE_INDEX_LOCAL => SessionType::LocalShell,
        _ => SessionType::SSH,
    }
}

pub fn type_index_for(session_type: &SessionType) -> u32 {
    match session_type {
        SessionType::SSH => TYPE_INDEX_SSH,
        SessionType::RDP => TYPE_INDEX_RDP,
        SessionType::VNC => TYPE_INDEX_VNC,
        SessionType::Serial => TYPE_INDEX_SERIAL,
        SessionType::LocalShell => TYPE_INDEX_LOCAL,
    }
}

fn auth_method_from_index(index: u32) -> AuthMethod {
    match index {
        1 => AuthMethod::Key,
        2 => AuthMethod::Agent,
        3 => AuthMethod::KeyboardInteractive,
        4 => AuthMethod::None,
        _ => AuthMethod::Password,
    }
}

fn auth_index_for(method: Option<&AuthMethod>) -> u32 {
    match method {
        Some(AuthMethod::Password) => 0,
        Some(AuthMethod::Key) => 1,
        Some(AuthMethod::Agent) => 2,
        Some(AuthMethod::KeyboardInteractive) => 3,
        Some(AuthMethod::None) | None => 4,
    }
}

/// The protocol's registered port; serial and local sessions have none.
pub fn default_port_for(session_type: &SessionType) -> Option<u16> {
    match session_type {
        SessionType::SSH => Some(22),
        SessionType::RDP => Some(3389),
        SessionType::VNC => Some(5900),
        SessionType::Serial | SessionType::LocalShell => None,
    }
}

/// `WIDTHxHEIGHT` → `(width, height)`; `None` (stored as NULL) when empty
/// or malformed.
pub fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.trim().split_once(['x', 'X'])?;
    let width: u32 = width.trim().parse().ok()?;
    let height: u32 = height.trim().parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

pub fn format_resolution(width: Option<u32>, height: Option<u32>) -> String {
    match (width, height) {
        (Some(width), Some(height)) => format!("{width}x{height}"),
        _ => String::new(),
    }
}

/// The tmux name offered for a session called `session_name`.
pub fn default_tmux_session_name(session_name: &str) -> String {
    format!("{TMUX_NAME_PREFIX}{}", session_name.trim())
}

fn trimmed_opt(value: &str) -> Option<String> {
    Some(value.trim().to_string()).filter(|v| !v.is_empty())
}

/// The session the dialog's values describe. An out-of-range port falls
/// back to the type's default. Per-type columns are written only for the
/// selected type, so values left in a hidden group never leak; the tmux
/// columns are common and stored as typed (only SSH reads them).
pub fn config_from_fields(fields: &DialogFields, folders: &[Folder]) -> SessionConfig {
    let session_type = session_type_from_index(fields.type_index);
    let folder_id = (fields.folder_index as usize)
        .checked_sub(1)
        .and_then(|i| folders.get(i))
        .map(|f| f.id);
    let port = default_port_for(&session_type).map(|default| {
        u16::try_from(fields.port)
            .ok()
            .filter(|p| *p > 0)
            .unwrap_or(default)
    });
    // Strip one `wayterm-`, and store an empty entry as NULL ("derive from
    // the session name at connect").
    let tmux_session_name = trimmed_opt(&fields.tmux_session_name)
        .map(|name| strip_tmux_prefix(&name).to_string())
        .filter(|name| !name.is_empty());

    let mut config = SessionConfig {
        id: fields.id,
        name: fields.name.trim().to_string(),
        session_type: session_type.clone(),
        folder_id,
        host: trimmed_opt(&fields.host),
        port,
        username: trimmed_opt(&fields.username),
        auth_method: Some(auth_method_from_index(fields.auth_index)),
        // Doc 06 defaults `rdp_compression` on; there is no widget for it.
        rdp_compression: session_type == SessionType::RDP,
        tmux_enabled: fields.tmux_enabled,
        tmux_session_name,
        icon: trimmed_opt(&fields.icon),
        notes: trimmed_opt(&fields.notes),
        last_connected: fields.last_connected.clone(),
        connect_count: fields.connect_count,
        ..Default::default()
    };

    match session_type {
        SessionType::Serial => {
            config.serial_device = trimmed_opt(&fields.serial_device);
            config.serial_baud_rate = Some(match fields.serial_baud {
                0 => DEFAULT_SERIAL_BAUD,
                baud => baud,
            });
            config.serial_data_bits = Some(vocab_value(
                &SERIAL_DATA_BITS,
                fields.serial_data_bits_index,
                SERIAL_DATA_BITS_DEFAULT,
            ));
            config.serial_parity = Some(
                vocab_value(
                    &SERIAL_PARITIES,
                    fields.serial_parity_index,
                    SERIAL_PARITY_DEFAULT,
                )
                .to_string(),
            );
            config.serial_stop_bits = Some(vocab_value(
                &SERIAL_STOP_BITS,
                fields.serial_stop_bits_index,
                SERIAL_STOP_BITS_DEFAULT,
            ));
            config.serial_flow_control = Some(
                vocab_value(
                    &SERIAL_FLOW_CONTROLS,
                    fields.serial_flow_control_index,
                    SERIAL_FLOW_CONTROL_DEFAULT,
                )
                .to_string(),
            );
        }
        SessionType::RDP => {
            config.rdp_domain = trimmed_opt(&fields.rdp_domain);
            let resolution = parse_resolution(&fields.rdp_resolution);
            config.rdp_width = resolution.map(|(w, _)| w);
            config.rdp_height = resolution.map(|(_, h)| h);
            config.rdp_color_depth = Some(vocab_value(
                &RDP_COLOR_DEPTHS,
                fields.rdp_color_depth_index,
                RDP_COLOR_DEPTH_DEFAULT,
            ));
        }
        SessionType::LocalShell => {
            config.local_shell = trimmed_opt(&fields.local_shell);
            config.local_working_dir = trimmed_opt(&fields.local_working_dir);
        }
        // VNC's only secret is keyring-only (doc 06).
        SessionType::SSH | SessionType::VNC => {}
    }
    config
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

const VNC_KEYRING_NOTE: &str = "The VNC password is stored in your OS keyring on first \
                                connect — never in Wayterm's database.";
const TMUX_LABEL: &str = "Persist via tmux — survive reconnects; sessions on the remote host \
                          are named wayterm-<session name>";

/// Labelled rows in a two-column grid.
struct FormGrid {
    grid: gtk4::Grid,
    row: i32,
}

impl FormGrid {
    fn new(margin: i32) -> Self {
        let grid = gtk4::Grid::builder()
            .row_spacing(6)
            .column_spacing(12)
            .build();
        set_margins(&grid, margin);
        Self { grid, row: 0 }
    }

    fn add(&mut self, label: &str, widget: &impl IsA<gtk4::Widget>) {
        let label = gtk4::Label::builder()
            .label(label)
            .halign(gtk4::Align::End)
            .build();
        self.grid.attach(&label, 0, self.row, 1, 1);
        self.grid.attach(widget, 1, self.row, 1, 1);
        self.row += 1;
    }

    fn entry(&mut self, label: &str, placeholder: &str) -> gtk4::Entry {
        let entry = gtk4::Entry::builder()
            .hexpand(true)
            .placeholder_text(placeholder)
            .activates_default(true)
            .build();
        self.add(label, &entry);
        entry
    }

    fn spin(&mut self, label: &str, min: f64, max: f64) -> gtk4::SpinButton {
        let spin = gtk4::SpinButton::with_range(min, max, 1.0);
        self.add(label, &spin);
        spin
    }

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
    fn apply(&self, type_index: u32) {
        for (index, group) in self.0.iter().enumerate() {
            group.set_reveal_child(index as u32 == type_index);
        }
    }
}

/// The tmux entry follows the Name field until the user edits it.
#[derive(Clone, Default)]
struct TmuxNameFollow {
    overridden: Rc<Cell<bool>>,
    /// Set while we write the entry ourselves, so `changed` is not taken
    /// for a user edit.
    programmatic: Rc<Cell<bool>>,
}

impl TmuxNameFollow {
    fn fill(&self, entry: &gtk4::Entry, text: &str) {
        self.programmatic.set(true);
        entry.set_text(text);
        self.programmatic.set(false);
    }
}

#[derive(Clone)]
struct Widgets {
    name: gtk4::Entry,
    session_type: gtk4::DropDown,
    folder: gtk4::DropDown,
    host: gtk4::Entry,
    port: gtk4::SpinButton,
    username: gtk4::Entry,
    auth: gtk4::DropDown,
    icon: IconPicker,
    notes: gtk4::TextView,
    tmux: gtk4::CheckButton,
    tmux_name: gtk4::Entry,
    serial_device: gtk4::Entry,
    serial_baud: gtk4::SpinButton,
    serial_data_bits: gtk4::DropDown,
    serial_parity: gtk4::DropDown,
    serial_stop_bits: gtk4::DropDown,
    serial_flow_control: gtk4::DropDown,
    rdp_domain: gtk4::Entry,
    rdp_resolution: gtk4::Entry,
    rdp_color_depth: gtk4::DropDown,
    local_shell: gtk4::Entry,
    local_working_dir: gtk4::Entry,
}

impl Widgets {
    /// The current values; `base` carries the edited row's identity.
    fn fields(&self, base: &DialogFields) -> DialogFields {
        let buffer = self.notes.buffer();
        DialogFields {
            name: self.name.text().into(),
            type_index: self.session_type.selected(),
            folder_index: self.folder.selected(),
            host: self.host.text().into(),
            port: self.port.value() as u32,
            username: self.username.text().into(),
            auth_index: self.auth.selected(),
            icon: self.icon.icon().unwrap_or_default(),
            notes: buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .into(),
            tmux_enabled: self.tmux.is_active(),
            tmux_session_name: self.tmux_name.text().into(),
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
            ..base.clone()
        }
    }

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
            .or_else(|| default_port_for(&config.session_type))
            .unwrap_or(PORT_SPIN_PLACEHOLDER);
        self.port.set_value(port.into());
        self.username.set_text(&text(&config.username));
        self.auth
            .set_selected(auth_index_for(config.auth_method.as_ref()));
        self.icon
            .set_default(session_type_icon(&config.session_type));
        self.icon.set_icon(config.icon.as_deref());
        self.notes.buffer().set_text(&text(&config.notes));
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
    modal: Modal<SessionConfig>,
    widgets: Widgets,
    groups: TypeGroups,
    tmux_follow: TmuxNameFollow,
    base: Rc<RefCell<DialogFields>>,
}

impl SessionDialog {
    pub fn new_session(parent: &gtk4::Window, folders: &[Folder]) -> Self {
        Self::build(parent, folders)
    }

    pub fn edit(parent: &gtk4::Window, config: &SessionConfig, folders: &[Folder]) -> Self {
        let dialog = Self::build(parent, folders);
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
        *dialog.base.borrow_mut() = DialogFields {
            id: config.id,
            last_connected: config.last_connected.clone(),
            connect_count: config.connect_count,
            ..DialogFields::default()
        };
        // `set_selected` only notifies on change, so apply SSH explicitly.
        dialog.groups.apply(type_index_for(&config.session_type));
        dialog
    }

    /// Run modally; `None` on cancel.
    pub fn run(self) -> Option<SessionConfig> {
        self.modal.run()
    }

    fn build(parent: &gtk4::Window, folders: &[Folder]) -> Self {
        let modal = Modal::new("Session", Some(parent));
        modal.window.set_default_size(420, 560);

        let mut form = FormGrid::new(12);
        let name = form.entry("Name:", "");
        let session_type = form.dropdown("Type:", &TYPE_LABELS);
        let folder_names: Vec<&str> = std::iter::once("None (root)")
            .chain(folders.iter().map(|f| f.name.as_str()))
            .collect();
        let folder = form.dropdown("Folder:", &folder_names);
        let host = form.entry("Host:", "");
        let port = form.spin("Port:", 1.0, 65535.0);
        let username = form.entry("Username:", "");
        let auth = form.dropdown("Auth Method:", &AUTH_LABELS);
        let icon = IconPicker::new(session_type_icon(&SessionType::SSH));
        form.add("Icon:", icon.widget());
        let notes = gtk4::TextView::builder()
            .wrap_mode(gtk4::WrapMode::WordChar)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(6)
            .right_margin(6)
            .build();
        let notes_scroll = gtk4::ScrolledWindow::builder()
            .child(&notes)
            .vexpand(true)
            .hexpand(true)
            .has_frame(true)
            .height_request(120)
            .build();
        form.add("Notes:", &notes_scroll);

        let mut ssh = FormGrid::new(6);
        let tmux = gtk4::CheckButton::with_label(TMUX_LABEL);
        ssh.span(&tmux);
        let tmux_name = ssh.entry("tmux session:", "wayterm-<session name>");

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
            icon,
            notes,
            tmux,
            tmux_name,
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
                let default = default_port_for(&session_type);
                port.set_value(default.unwrap_or(PORT_SPIN_PLACEHOLDER).into());
                icon.set_default(session_type_icon(&session_type));
            });
        groups.apply(widgets.session_type.selected());

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

        let base = Rc::new(RefCell::new(DialogFields::default()));
        let (for_ok, base_for_ok, folders) = (widgets.clone(), base.clone(), folders.to_vec());
        let buttons = ok_cancel_row(&modal, move || {
            let fields = for_ok.fields(&base_for_ok.borrow());
            Some(config_from_fields(&fields, &folders))
        });
        buttons.set_margin_top(12);

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        vbox.append(&form.grid);
        for group in &groups.0 {
            vbox.append(group);
        }
        vbox.append(&buttons);
        set_margins(&buttons, 12);
        modal.window.set_child(Some(&vbox));

        Self {
            modal,
            widgets,
            groups,
            tmux_follow,
            base,
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
        let dialog = SessionDialog::new_session(&parent, &[]);
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
        let dialog = SessionDialog::edit(&parent, &stored, &[]);
        assert_eq!(dialog.widgets.icon.icon(), None);
        assert!(dialog.widgets.tmux.is_active());
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-shared");
        assert_eq!(dialog.widgets.port.value(), 2222.0);
        dialog.widgets.name.set_text("renamed");
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-shared");
        dialog.widgets.session_type.set_selected(TYPE_INDEX_LOCAL);
        assert_eq!(
            dialog.widgets.icon.shown_icon().as_deref(),
            Some("utilities-terminal-symbolic"),
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
        let dialog = SessionDialog::edit(&parent, &derived, &[]);
        assert_eq!(
            dialog.widgets.icon.icon().as_deref(),
            Some("wayterm-homelab")
        );
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-prod");
        dialog.widgets.name.set_text("renamed");
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-renamed");
    }
}
