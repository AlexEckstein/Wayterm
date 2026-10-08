//! Session config dialog — create/edit a [`SessionConfig`] (doc 06 / doc
//! 10 Phase 1).
//!
//! The GTK side is display-heavy and cannot be unit-tested without a
//! display.  All mapping logic (DropDown index → enum, port defaulting,
//! per-type field mapping, identity-field preservation) is isolated in
//! [`config_from_fields`] and exercised by the tests in [`tests`].
//!
//! Per-type field groups (doc 06 §"Session Config Struct"): the Type
//! DropDown drives [`TypeGroups::apply`], which reveals exactly the group
//! matching the selected type and hides the rest.
//!
//! * **serial** — device, baud, data bits, parity, stop bits, flow control
//!   (`serial_*` columns, doc 06 defaults 8 / `none` / 1 / `none`).
//! * **rdp** — domain, resolution, color depth (`rdp_domain`,
//!   `rdp_width`/`rdp_height`, `rdp_color_depth`; doc 08 §RDP falls back to
//!   1024×768 / 32 bpp).
//! * **ssh** — the tmux persistence toggle (doc 04 §Tmux Session
//!   Persistence), revealed only for SSH: the checkbox plus the remote
//!   session name. Key path, jump hosts and port forwards are edited
//!   elsewhere (doc 11).
//! * **vnc** — no secret field. The VNC password goes to the OS keyring on
//!   first connect (docs 06 §Credential Storage / 11), so the group only
//!   says so.
//! * **local** — shell path and working directory (`local_shell`,
//!   `local_working_dir`).
//!
//! ## tmux persistence (doc 04 §Tmux Session Persistence)
//!
//! The SSH group holds the "Persist via tmux" checkbox and the remote
//! session-name Entry. The Entry shows the **full** remote name
//! (`wayterm-<suffix>`) so what the user reads matches
//! `tmux list-sessions`; the stored `tmux_session_name` is the bare
//! suffix, because `SSHTransport` prepends `wayterm-` itself.
//! [`config_from_fields`] strips one leading
//! [`TMUX_NAME_PREFIX`](crate::store::model::TMUX_NAME_PREFIX) on the way
//! in, so a bare name typed by hand cannot become `wayterm-wayterm-x`.
//! An Entry left empty is stored as NULL and resolved from the session
//! name at connect ([`SessionConfig::tmux_session_suffix`]).
//!
//! The Entry follows the Name field until the user types in it, after
//! which their name wins — see `TmuxNameFollow` in `gtk_dialog`. Doc 04
//! scopes the feature to SSH, so the flag is stored for every type (the
//! columns are common in doc 06) and only the SSH transport reads it.

use crate::store::model::{
    strip_tmux_prefix, vocab_index, vocab_value, AuthMethod, Folder, SessionConfig, SessionType,
    RDP_COLOR_DEPTHS, RDP_COLOR_DEPTH_DEFAULT, SERIAL_DATA_BITS, SERIAL_DATA_BITS_DEFAULT,
    SERIAL_FLOW_CONTROLS, SERIAL_FLOW_CONTROL_DEFAULT, SERIAL_PARITIES, SERIAL_PARITY_DEFAULT,
    SERIAL_STOP_BITS, SERIAL_STOP_BITS_DEFAULT, TMUX_NAME_PREFIX,
};

/// Type DropDown index for SSH (doc-06 order).
pub const TYPE_INDEX_SSH: u32 = 0;
/// Type DropDown index for RDP (doc-06 order).
pub const TYPE_INDEX_RDP: u32 = 1;
/// Type DropDown index for VNC (doc-06 order).
pub const TYPE_INDEX_VNC: u32 = 2;
/// Type DropDown index for serial (doc-06 order).
pub const TYPE_INDEX_SERIAL: u32 = 3;
/// Type DropDown index for local shell (doc-06 order).
pub const TYPE_INDEX_LOCAL: u32 = 4;

/// Baud rate the serial group starts on.
///
/// Doc 06 leaves `serial_baud_rate` nullable with no documented default, so
/// this is the standard USB/serial rate; a stored value overrides it.
pub const DEFAULT_SERIAL_BAUD: u32 = 115_200;

/// Value shown in the Port spin for types that have no network port.
///
/// [`gtk4::SpinButton`] starts at 1, so "unset" cannot be displayed; the
/// widget is unused for serial and local sessions because
/// [`default_port_for`] stores no port for them.
const PORT_SPIN_PLACEHOLDER: u16 = 22;

/// Values extracted from the GTK widgets in the dialog.
///
/// Kept deliberately plain so [`config_from_fields`] can be tested without
/// initializing GTK. `*_index` fields are DropDown selections into the
/// per-type vocabularies in [`crate::store::model`].
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

    // tmux group (doc 04 §Tmux Session Persistence; stored in doc 06's
    // `tmux_enabled` / `tmux_session_name` columns).
    pub tmux_enabled: bool,
    /// The remote tmux session name as typed in the dialog, i.e. the full
    /// `wayterm-<suffix>` spelling the box documents (doc 04's naming
    /// convention). `config_from_fields` strips one leading
    /// [`TMUX_NAME_PREFIX`](crate::store::model::TMUX_NAME_PREFIX) before
    /// storing, because `SSHTransport` prepends it.
    pub tmux_session_name: String,

    // Serial group (doc 06 `serial_*` columns).
    pub serial_device: String,
    pub serial_baud: u32,
    pub serial_data_bits_index: u32,
    pub serial_parity_index: u32,
    pub serial_stop_bits_index: u32,
    pub serial_flow_control_index: u32,

    // RDP group (doc 06 `rdp_domain` / `rdp_width` / `rdp_height` /
    // `rdp_color_depth`).
    pub rdp_domain: String,
    /// `WIDTHxHEIGHT` as typed; empty means "use doc 08's 1024×768".
    pub rdp_resolution: String,
    pub rdp_color_depth_index: u32,

    // VNC group: no secret field — the password is keyring-only
    // (docs 06 §Credential Storage / 11).

    // Local shell group (doc 06 `local_*` columns).
    pub local_shell: String,
    pub local_working_dir: String,

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
            type_index: TYPE_INDEX_SSH,
            folder_index: 0,
            host: String::new(),
            port: 22,
            username: String::new(),
            auth_index: 0,
            notes: String::new(),
            tmux_enabled: false,
            tmux_session_name: String::new(),
            serial_device: String::new(),
            serial_baud: DEFAULT_SERIAL_BAUD,
            serial_data_bits_index: vocab_index(
                &SERIAL_DATA_BITS,
                Some(SERIAL_DATA_BITS_DEFAULT),
                SERIAL_DATA_BITS_DEFAULT,
            ),
            serial_parity_index: vocab_index(
                &SERIAL_PARITIES,
                Some(SERIAL_PARITY_DEFAULT),
                SERIAL_PARITY_DEFAULT,
            ),
            serial_stop_bits_index: vocab_index(
                &SERIAL_STOP_BITS,
                Some(SERIAL_STOP_BITS_DEFAULT),
                SERIAL_STOP_BITS_DEFAULT,
            ),
            serial_flow_control_index: vocab_index(
                &SERIAL_FLOW_CONTROLS,
                Some(SERIAL_FLOW_CONTROL_DEFAULT),
                SERIAL_FLOW_CONTROL_DEFAULT,
            ),
            rdp_domain: String::new(),
            rdp_resolution: String::new(),
            rdp_color_depth_index: vocab_index(
                &RDP_COLOR_DEPTHS,
                Some(RDP_COLOR_DEPTH_DEFAULT),
                RDP_COLOR_DEPTH_DEFAULT,
            ),
            local_shell: String::new(),
            local_working_dir: String::new(),
            id: 0,
            folder_id: None,
            last_connected: None,
            connect_count: 0,
        }
    }
}

/// DropDown index → [`SessionType`], in the dialog's doc-06 order.
///
/// Any index outside the five known entries means SSH.
pub fn session_type_from_index(index: u32) -> SessionType {
    match index {
        TYPE_INDEX_RDP => SessionType::RDP,
        TYPE_INDEX_VNC => SessionType::VNC,
        TYPE_INDEX_SERIAL => SessionType::Serial,
        TYPE_INDEX_LOCAL => SessionType::LocalShell,
        _ => SessionType::SSH,
    }
}

/// [`SessionType`] → DropDown index, for pre-filling the Type widget.
pub fn type_index_for(session_type: &SessionType) -> u32 {
    match session_type {
        SessionType::SSH => TYPE_INDEX_SSH,
        SessionType::RDP => TYPE_INDEX_RDP,
        SessionType::VNC => TYPE_INDEX_VNC,
        SessionType::Serial => TYPE_INDEX_SERIAL,
        SessionType::LocalShell => TYPE_INDEX_LOCAL,
    }
}

/// Default TCP port per session type.
///
/// Doc 06 stores `port` as a nullable common column and names no per-type
/// default, so these are the registered ports for each protocol. Serial and
/// local sessions have no network port at all — a serial session names a
/// device and a local session names a shell — so both return `None`.
pub fn default_port_for(session_type: &SessionType) -> Option<u16> {
    match session_type {
        SessionType::SSH => Some(22),
        SessionType::RDP => Some(3389),
        SessionType::VNC => Some(5900),
        SessionType::Serial | SessionType::LocalShell => None,
    }
}

/// Parse the RDP "Resolution" field (`WIDTHxHEIGHT`) into doc 06's
/// `rdp_width` / `rdp_height`.
///
/// Empty or malformed input yields `None`, leaving both columns NULL so
/// doc 08 §RDP applies its 1024×768 fallback at connect time.
pub fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.trim().split_once(['x', 'X'])?;
    let width: u32 = width.trim().parse().ok()?;
    let height: u32 = height.trim().parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// The remote tmux session name the dialog offers for a session called
/// `session_name`: doc 04's `wayterm-<sessionName>` convention.
///
/// Display-free on purpose — the GTK Entry and the tests both derive the
/// name through this function, so the auto-update behaviour cannot drift
/// between them.
pub fn default_tmux_session_name(session_name: &str) -> String {
    format!("{}{}", TMUX_NAME_PREFIX, session_name.trim())
}

/// Render doc 06's `rdp_width` / `rdp_height` back into the dialog's
/// "Resolution" field. `None` on either side renders empty.
pub fn format_resolution(width: Option<u32>, height: Option<u32>) -> String {
    match (width, height) {
        (Some(width), Some(height)) => format!("{width}x{height}"),
        _ => String::new(),
    }
}

fn trimmed_opt(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.trim().to_string())
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
/// - `port` is kept as typed when in range, and otherwise falls back to
///   [`default_port_for`] the selected type (SSH 22, RDP 3389, VNC 5900).
///   Serial and local sessions store no port.
/// - Per-type payloads are written **only** for the selected type, so
///   values typed into a group that is now hidden never leak into the row.
///   The VNC group has no payload: its password is keyring-only.
/// - The tmux flag is the exception: doc 04 scopes tmux persistence to
///   SSH, but `tmux_enabled` / `tmux_session_name` are common columns
///   (doc 06), so they are stored as typed for every type and only the SSH
///   transport reads them. The stored name is the bare suffix —
///   `SSHTransport` prepends `wayterm-`.
/// - Edit-mode identity fields are preserved unchanged.
pub fn config_from_fields(fields: &DialogFields, folders: &[Folder]) -> SessionConfig {
    let session_type = session_type_from_index(fields.type_index);

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

    let port = default_port_for(&session_type).map(|default| {
        if (1..=65535).contains(&fields.port) {
            fields.port as u16
        } else {
            default
        }
    });

    let host = trimmed_opt(&fields.host);
    let username = trimmed_opt(&fields.username);
    let notes = trimmed_opt(&fields.notes);

    // Strip one leading `wayterm-` so the remote never ends up with
    // `wayterm-wayterm-…`, and store an empty box as NULL: that means
    // "derive the name from the session name at connect", not a session
    // literally called `wayterm-`.
    let tmux_session_name = trimmed_opt(&fields.tmux_session_name)
        .map(|name| strip_tmux_prefix(&name).to_string())
        .filter(|name| !name.is_empty());

    // Doc 06's schema defaults `rdp_compression` to 1 (on) and the dialog
    // has no compression widget, so RDP sessions are stored with it on;
    // the other types keep the struct default.
    let mut config = SessionConfig {
        id: fields.id,
        name: fields.name.trim().to_string(),
        session_type: session_type.clone(),
        folder_id,
        host,
        port,
        username,
        auth_method,
        rdp_compression: session_type == SessionType::RDP,
        tmux_enabled: fields.tmux_enabled,
        tmux_session_name,
        notes,
        last_connected: fields.last_connected.clone(),
        connect_count: fields.connect_count,
        ..Default::default()
    };

    match session_type {
        SessionType::Serial => {
            config.serial_device = trimmed_opt(&fields.serial_device);
            config.serial_baud_rate = Some(if fields.serial_baud > 0 {
                fields.serial_baud
            } else {
                DEFAULT_SERIAL_BAUD
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
            let (width, height) = parse_resolution(&fields.rdp_resolution)
                .map_or((None, None), |(w, h)| (Some(w), Some(h)));
            config.rdp_width = width;
            config.rdp_height = height;
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
        // SSH has no per-type payload in this dialog, and VNC has none at
        // all: `vnc_password` is a keyring secret (docs 06/11).
        SessionType::SSH | SessionType::VNC => {}
    }

    config
}

// The GTK4 constructors and helpers are only used by the UI layer; unit
// tests target the pure `config_from_fields` function.  Silence dead-code
// warnings in non-test builds until the dialog is exercised end-to-end.
#[allow(dead_code)]
mod gtk_dialog {
    use gtk4::prelude::*;

    use crate::store::model::{
        vocab_index, AuthMethod, RDP_COLOR_DEPTHS, RDP_COLOR_DEPTH_DEFAULT, SERIAL_DATA_BITS,
        SERIAL_DATA_BITS_DEFAULT, SERIAL_FLOW_CONTROLS, SERIAL_FLOW_CONTROL_DEFAULT,
        SERIAL_PARITIES, SERIAL_PARITY_DEFAULT, SERIAL_STOP_BITS, SERIAL_STOP_BITS_DEFAULT,
    };

    use super::{
        config_from_fields, default_port_for, default_tmux_session_name, format_resolution,
        session_type_from_index, type_index_for, DialogFields, Folder, SessionConfig,
        DEFAULT_SERIAL_BAUD, PORT_SPIN_PLACEHOLDER, TYPE_INDEX_LOCAL, TYPE_INDEX_RDP,
        TYPE_INDEX_SERIAL, TYPE_INDEX_SSH, TYPE_INDEX_VNC,
    };

    /// Static labels shown in the Type DropDown — index `n` is
    /// `TYPE_INDEX_*` constant `n`.
    const TYPE_LABELS: &[&str] = &["ssh", "rdp", "vnc", "serial", "local"];

    /// Static labels shown in the Auth Method DropDown.
    const AUTH_LABELS: &[&str] = &["password", "key", "agent", "keyboard-interactive", "none"];

    /// Shown in the VNC group instead of a password field: doc 06
    /// §Credential Storage keeps `vnc_password` in the OS keyring, never in
    /// the session database, so there is nothing to type here.
    const VNC_KEYRING_NOTE: &str =
        "The VNC password is stored in your OS keyring on first connect — never in \
         Wayterm's database.";

    /// Checkbox label for tmux persistence, revealed in the SSH group only
    /// (doc 04 §Tmux Session Persistence).
    const TMUX_LABEL: &str = "Persist via tmux — survive reconnects; sessions on the remote \
                              host are named wayterm-<session name>";

    /// Placeholder for the tmux session-name Entry. The field is pre-filled
    /// with the derived name, so the placeholder only shows when the user
    /// clears it — which means "derive at connect".
    const TMUX_NAME_PLACEHOLDER: &str = "wayterm-<session name>";

    /// The dialog's tmux Entry auto-update rule (doc 04 §Tmux Session
    /// Persistence), as state both signal handlers can see.
    ///
    /// The Entry is pre-filled with `wayterm-<session name>` and keeps
    /// following the Name field until the user types in it themselves; after
    /// that their name wins and the Name field stops touching it. An Entry
    /// cleared by hand stays empty, which `config_from_fields` stores as NULL
    /// and [`SessionConfig::tmux_session_suffix`] resolves from the session
    /// name at connect time.
    #[derive(Clone, Default)]
    struct TmuxNameFollow {
        /// Set once the user edits the Entry. Never cleared: the only way
        /// back to "follows the name" is a fresh dialog.
        overridden: std::rc::Rc<std::cell::Cell<bool>>,
        /// `gtk4::Entry::set_text` emits `changed` synchronously, so the
        /// Entry's own handler must be muted while we write it or our own
        /// fill would count as a user edit.
        programmatic: std::rc::Rc<std::cell::Cell<bool>>,
    }

    impl TmuxNameFollow {
        /// Write `text` into `entry` verbatim, without recording an edit.
        ///
        /// Takes the finished name rather than a session name: the
        /// edit-mode pre-fill already carries the `wayterm-` prefix
        /// ([`SessionConfig::tmux_name_for_dialog`]) and must not gain a
        /// second one.
        fn fill(&self, entry: &gtk4::Entry, text: &str) {
            self.programmatic.set(true);
            entry.set_text(text);
            self.programmatic.set(false);
        }

        /// Name field changed: re-derive unless the user took the Entry over.
        fn sync_to(&self, entry: &gtk4::Entry, name_entry: &gtk4::Entry) {
            if !self.overridden.get() {
                self.fill(entry, &default_tmux_session_name(&name_entry.text()));
            }
        }

        /// Entry changed: a user edit, unless we caused it.
        fn note_edit(&self) {
            if !self.programmatic.get() {
                self.overridden.set(true);
            }
        }

        /// Stop following the Name field for good — used by edit mode when
        /// the row carries an explicit session name, so renaming the
        /// session cannot silently discard a name the user chose earlier.
        fn take_over(&self) {
            self.overridden.set(true);
        }
    }

    /// Edit-mode metadata carried in a shared cell so the OK handler can read
    /// the current values without capturing `Self`.
    #[derive(Clone, Debug)]
    struct DialogState {
        edit_id: i64,
        edit_last_connected: Option<String>,
        edit_connect_count: u32,
    }

    /// The four per-type field groups, one per non-SSH type.
    ///
    /// [`apply`](Self::apply) is the show/hide seam: the Type DropDown's
    /// `notify::selected` handler calls it, so exactly the group matching
    /// the selected type is revealed.
    #[derive(Clone)]
    struct TypeGroups {
        ssh: gtk4::Revealer,
        serial: gtk4::Revealer,
        rdp: gtk4::Revealer,
        vnc: gtk4::Revealer,
        local: gtk4::Revealer,
    }

    impl TypeGroups {
        /// Reveal the group for `type_index`; hide every other group.
        fn apply(&self, type_index: u32) {
            self.ssh.set_reveal_child(type_index == TYPE_INDEX_SSH);
            self.serial
                .set_reveal_child(type_index == TYPE_INDEX_SERIAL);
            self.rdp.set_reveal_child(type_index == TYPE_INDEX_RDP);
            self.vnc.set_reveal_child(type_index == TYPE_INDEX_VNC);
            self.local.set_reveal_child(type_index == TYPE_INDEX_LOCAL);
        }
    }

    /// Accumulates the labelled rows of one per-type group and wraps them in
    /// a titled, initially hidden [`gtk4::Revealer`].
    struct GroupRows {
        title: String,
        grid: gtk4::Grid,
        row: i32,
    }

    impl GroupRows {
        fn new(title: &str) -> Self {
            Self {
                title: title.to_string(),
                grid: gtk4::Grid::builder()
                    .row_spacing(6)
                    .column_spacing(12)
                    .margin_start(6)
                    .margin_end(6)
                    .margin_top(6)
                    .margin_bottom(6)
                    .build(),
                row: 0,
            }
        }

        /// Add a row around an already-built widget.
        fn add(&mut self, label_text: &str, widget: &gtk4::Widget) {
            let label = gtk4::Label::builder()
                .label(label_text)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Center)
                .build();
            self.grid.attach(&label, 0, self.row, 1, 1);
            self.grid.attach(widget, 1, self.row, 1, 1);
            self.row += 1;
        }

        /// Add an [`gtk4::Entry`] row and return it for later pre-fill.
        fn entry(&mut self, label_text: &str, placeholder: &str) -> gtk4::Entry {
            let entry = gtk4::Entry::builder()
                .hexpand(true)
                .placeholder_text(placeholder)
                .activates_default(true)
                .build();
            self.add(label_text, entry.upcast_ref::<gtk4::Widget>());
            entry
        }

        /// Add an integer [`gtk4::SpinButton`] row and return it.
        fn spin(&mut self, label_text: &str, range: (f64, f64)) -> gtk4::SpinButton {
            let spin = gtk4::SpinButton::with_range(range.0, range.1, 1.0);
            spin.set_digits(0);
            self.add(label_text, spin.upcast_ref::<gtk4::Widget>());
            spin
        }

        /// Add a [`gtk4::DropDown`] row and return it.
        fn dropdown(&mut self, label_text: &str, labels: &[&str]) -> gtk4::DropDown {
            let model = gtk4::StringList::new(labels);
            let dropdown = gtk4::DropDown::builder()
                .model(&model)
                .hexpand(true)
                .build();
            self.add(label_text, dropdown.upcast_ref::<gtk4::Widget>());
            dropdown
        }

        /// Add a [`gtk4::CheckButton`] row spanning both columns, and
        /// return it.
        fn check(&mut self, label_text: &str) -> gtk4::CheckButton {
            let check = gtk4::CheckButton::with_label(label_text);
            check.set_halign(gtk4::Align::Start);
            self.grid.attach(&check, 0, self.row, 2, 1);
            self.row += 1;
            check
        }

        /// Add a read-only note row (used by the VNC group).
        fn note(&mut self, text: &str) {
            let label = gtk4::Label::builder()
                .label(text)
                .wrap(true)
                .xalign(0.0)
                .build();
            let row = self.row;
            self.grid.attach(&label, 1, row, 1, 1);
            self.row += 1;
        }

        /// Wrap the accumulated rows in a titled [`gtk4::Revealer`] that
        /// starts hidden — `TypeGroups::apply` reveals it when its type is
        /// selected.
        fn finish(self) -> gtk4::Revealer {
            let title = gtk4::Label::builder()
                .label(self.title)
                .halign(gtk4::Align::Start)
                .margin_start(6)
                .margin_top(8)
                .build();

            let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            content.append(&title);
            content.append(&self.grid);

            gtk4::Revealer::builder()
                .child(&content)
                .transition_type(gtk4::RevealerTransitionType::SlideDown)
                .reveal_child(false)
                .build()
        }
    }

    /// Render a numeric vocabulary as DropDown labels.
    fn number_labels<T: std::fmt::Display>(values: &[T]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
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
        tmux_check: gtk4::CheckButton,
        tmux_session_name_entry: gtk4::Entry,
        notes_view: gtk4::TextView,
        serial_device_entry: gtk4::Entry,
        serial_baud_spin: gtk4::SpinButton,
        serial_data_bits_dropdown: gtk4::DropDown,
        serial_parity_dropdown: gtk4::DropDown,
        serial_stop_bits_dropdown: gtk4::DropDown,
        serial_flow_control_dropdown: gtk4::DropDown,
        rdp_domain_entry: gtk4::Entry,
        rdp_resolution_entry: gtk4::Entry,
        rdp_color_depth_dropdown: gtk4::DropDown,
        local_shell_entry: gtk4::Entry,
        local_working_dir_entry: gtk4::Entry,
    }

    /// GTK4 dialog for creating or editing a saved session.
    ///
    /// Exposes doc 06's common dialog-editable fields plus one per-type
    /// group per session type; only the group matching the selected type is
    /// revealed. The dialog returns a fully populated [`SessionConfig`] on OK
    /// and `None` on cancel.
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

            let type_index = type_index_for(&config.session_type);
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
            // Serial and local sessions have no port; show the placeholder in
            // the (unused) spin rather than an out-of-range zero.
            let shown_port = config
                .port
                .or_else(|| default_port_for(&config.session_type))
                .unwrap_or(PORT_SPIN_PLACEHOLDER);
            widgets.port_spin.set_value(f64::from(shown_port));
            widgets
                .username_entry
                .set_text(config.username.as_deref().unwrap_or(""));

            let auth_index = match config.auth_method {
                None => 4,
                Some(AuthMethod::Password) => 0,
                Some(AuthMethod::Key) => 1,
                Some(AuthMethod::Agent) => 2,
                Some(AuthMethod::KeyboardInteractive) => 3,
                Some(AuthMethod::None) => 4,
            };
            widgets.auth_dropdown.set_selected(auth_index);

            // tmux (doc 04 §Tmux Session Persistence). The stored column is
            // the bare suffix and the Entry shows the full remote name. Fill
            // programmatically: an edit-mode pre-fill is not a user edit, so
            // the Entry keeps following the Name field until they type in it.
            widgets.tmux_check.set_active(config.tmux_enabled);
            this.tmux_follow().fill(
                &widgets.tmux_session_name_entry,
                &config.tmux_name_for_dialog(),
            );
            // An explicit stored name is the user's own choice, so it keeps
            // winning over the Name field — exactly as if they had typed it.
            if config.tmux_session_name.is_some() {
                this.tmux_follow().take_over();
            }

            // Per-type groups (doc 06 columns; doc 08 fallbacks).
            widgets
                .serial_device_entry
                .set_text(config.serial_device.as_deref().unwrap_or(""));
            widgets.serial_baud_spin.set_value(f64::from(
                config
                    .serial_baud_rate
                    .filter(|baud| *baud > 0)
                    .unwrap_or(DEFAULT_SERIAL_BAUD),
            ));
            widgets.serial_data_bits_dropdown.set_selected(vocab_index(
                &SERIAL_DATA_BITS,
                config.serial_data_bits,
                SERIAL_DATA_BITS_DEFAULT,
            ));
            widgets.serial_parity_dropdown.set_selected(vocab_index(
                &SERIAL_PARITIES,
                config.serial_parity.as_deref(),
                SERIAL_PARITY_DEFAULT,
            ));
            widgets.serial_stop_bits_dropdown.set_selected(vocab_index(
                &SERIAL_STOP_BITS,
                config.serial_stop_bits,
                SERIAL_STOP_BITS_DEFAULT,
            ));
            widgets
                .serial_flow_control_dropdown
                .set_selected(vocab_index(
                    &SERIAL_FLOW_CONTROLS,
                    config.serial_flow_control.as_deref(),
                    SERIAL_FLOW_CONTROL_DEFAULT,
                ));

            widgets
                .rdp_domain_entry
                .set_text(config.rdp_domain.as_deref().unwrap_or(""));
            widgets
                .rdp_resolution_entry
                .set_text(&format_resolution(config.rdp_width, config.rdp_height));
            widgets.rdp_color_depth_dropdown.set_selected(vocab_index(
                &RDP_COLOR_DEPTHS,
                config.rdp_color_depth,
                RDP_COLOR_DEPTH_DEFAULT,
            ));

            widgets
                .local_shell_entry
                .set_text(config.local_shell.as_deref().unwrap_or(""));
            widgets
                .local_working_dir_entry
                .set_text(config.local_working_dir.as_deref().unwrap_or(""));

            widgets
                .notes_view
                .buffer()
                .set_text(config.notes.as_deref().unwrap_or(""));

            *this.state.borrow_mut() = DialogState {
                edit_id: config.id,
                edit_last_connected: config.last_connected.clone(),
                edit_connect_count: config.connect_count,
            };

            // `set_selected` only emits `notify::selected` on a change, so
            // apply the group visibility explicitly for an SSH row.
            this.groups().apply(type_index);

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

            let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
            vbox.append(&grid);

            // --- Per-type groups (doc 06 §"Session Config Struct") ---------
            let mut ssh = GroupRows::new("SSH");
            let tmux_check = ssh.check(TMUX_LABEL);
            let tmux_session_name_entry = ssh.entry("tmux session:", TMUX_NAME_PLACEHOLDER);
            let ssh_group = ssh.finish();

            let mut serial = GroupRows::new("Serial");
            let serial_device_entry = serial.entry("Device:", "/dev/ttyUSB0");
            let serial_baud_spin = serial.spin("Baud:", (50.0, 4_000_000.0));
            let data_bits_labels = number_labels(&SERIAL_DATA_BITS);
            let serial_data_bits_dropdown = serial.dropdown(
                "Data Bits:",
                &data_bits_labels
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            );
            let serial_parity_dropdown = serial.dropdown("Parity:", &SERIAL_PARITIES);
            let stop_bits_labels = number_labels(&SERIAL_STOP_BITS);
            let serial_stop_bits_dropdown = serial.dropdown(
                "Stop Bits:",
                &stop_bits_labels
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            );
            let serial_flow_control_dropdown =
                serial.dropdown("Flow Control:", &SERIAL_FLOW_CONTROLS);
            let serial_group = serial.finish();

            let mut rdp = GroupRows::new("RDP");
            let rdp_domain_entry = rdp.entry("Domain:", "optional");
            let rdp_resolution_entry = rdp.entry("Resolution:", "1024x768 (doc 08 default)");
            let color_depth_labels = number_labels(&RDP_COLOR_DEPTHS);
            let rdp_color_depth_dropdown = rdp.dropdown(
                "Color Depth:",
                &color_depth_labels
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            );
            let rdp_group = rdp.finish();

            let mut vnc = GroupRows::new("VNC");
            vnc.note(VNC_KEYRING_NOTE);
            let vnc_group = vnc.finish();

            let mut local = GroupRows::new("Local Shell");
            let local_shell_entry = local.entry("Shell:", "$SHELL (default)");
            let local_working_dir_entry = local.entry("Working Directory:", "~ (default)");
            let local_group = local.finish();

            let groups = TypeGroups {
                ssh: ssh_group,
                serial: serial_group,
                rdp: rdp_group,
                vnc: vnc_group,
                local: local_group,
            };
            vbox.append(&groups.ssh);
            vbox.append(&groups.serial);
            vbox.append(&groups.rdp);
            vbox.append(&groups.vnc);
            vbox.append(&groups.local);

            // Type change → show exactly the matching group, hide the rest.
            // The Port spin follows too: each protocol has its own default
            // (SSH 22, RDP 3389, VNC 5900) and the spin cannot show "unset",
            // so a stale 22 would otherwise be stored for an RDP session.
            // `edit()` pre-fills the port from the config after this runs.
            let groups_for_notify = groups.clone();
            let port_for_notify = port_spin.clone();
            type_dropdown.connect_notify_local(Some("selected"), move |dropdown, _| {
                let session_type = session_type_from_index(dropdown.selected());
                groups_for_notify.apply(dropdown.selected());
                port_for_notify.set_value(f64::from(
                    default_port_for(&session_type).unwrap_or(PORT_SPIN_PLACEHOLDER),
                ));
            });
            groups.apply(type_dropdown.selected());

            // tmux micro-UX (doc 04 §Tmux Session Persistence): see
            // `TmuxNameFollow` for the whole rule.
            let tmux_follow = TmuxNameFollow::default();
            {
                let tmux_entry = tmux_session_name_entry.clone();
                let name_entry_for_sync = name_entry.clone();
                let follow = tmux_follow.clone();
                name_entry.connect_changed(move |_| {
                    follow.sync_to(&tmux_entry, &name_entry_for_sync);
                });
            }
            {
                let follow = tmux_follow.clone();
                tmux_session_name_entry.connect_changed(move |_| follow.note_edit());
            }
            tmux_follow.fill(
                &tmux_session_name_entry,
                &default_tmux_session_name(&name_entry.text()),
            );

            let ok_button = gtk4::Button::with_label("OK");
            let cancel_button = gtk4::Button::with_label("Cancel");
            cancel_button.set_margin_end(12);

            let button_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            button_box.set_halign(gtk4::Align::End);
            button_box.set_margin_top(12);
            button_box.append(&cancel_button);
            button_box.append(&ok_button);

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
                let tmux_check = tmux_check.clone();
                let tmux_session_name_entry = tmux_session_name_entry.clone();
                let notes_view = notes_view.clone();
                let serial_device_entry = serial_device_entry.clone();
                let serial_baud_spin = serial_baud_spin.clone();
                let serial_data_bits_dropdown = serial_data_bits_dropdown.clone();
                let serial_parity_dropdown = serial_parity_dropdown.clone();
                let serial_stop_bits_dropdown = serial_stop_bits_dropdown.clone();
                let serial_flow_control_dropdown = serial_flow_control_dropdown.clone();
                let rdp_domain_entry = rdp_domain_entry.clone();
                let rdp_resolution_entry = rdp_resolution_entry.clone();
                let rdp_color_depth_dropdown = rdp_color_depth_dropdown.clone();
                let local_shell_entry = local_shell_entry.clone();
                let local_working_dir_entry = local_working_dir_entry.clone();
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
                        tmux_enabled: tmux_check.is_active(),
                        tmux_session_name: tmux_session_name_entry.text().to_string(),
                        notes: notes.to_string(),
                        serial_device: serial_device_entry.text().to_string(),
                        serial_baud: serial_baud_spin.value() as u32,
                        serial_data_bits_index: serial_data_bits_dropdown.selected(),
                        serial_parity_index: serial_parity_dropdown.selected(),
                        serial_stop_bits_index: serial_stop_bits_dropdown.selected(),
                        serial_flow_control_index: serial_flow_control_dropdown.selected(),
                        rdp_domain: rdp_domain_entry.text().to_string(),
                        rdp_resolution: rdp_resolution_entry.text().to_string(),
                        rdp_color_depth_index: rdp_color_depth_dropdown.selected(),
                        local_shell: local_shell_entry.text().to_string(),
                        local_working_dir: local_working_dir_entry.text().to_string(),
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
                tmux_check,
                tmux_session_name_entry,
                notes_view,
                serial_device_entry,
                serial_baud_spin,
                serial_data_bits_dropdown,
                serial_parity_dropdown,
                serial_stop_bits_dropdown,
                serial_flow_control_dropdown,
                rdp_domain_entry,
                rdp_resolution_entry,
                rdp_color_depth_dropdown,
                local_shell_entry,
                local_working_dir_entry,
            };

            // Keep the widget handles alive on the window so `edit()` can use
            // them after construction.
            unsafe {
                window.set_data("wayterm-session-dialog-widgets", widgets);
                window.set_data("wayterm-session-dialog-groups", groups);
                window.set_data("wayterm-session-dialog-tmux-follow", tmux_follow);
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

        /// Retrieve the tmux Entry auto-update state stored on the window.
        fn tmux_follow(&self) -> TmuxNameFollow {
            unsafe {
                self.window
                    .data::<TmuxNameFollow>("wayterm-session-dialog-tmux-follow")
                    .map(|d| (*d.as_ptr()).clone())
                    .expect("dialog tmux follow state missing")
            }
        }

        /// Retrieve the per-type groups stored on the window.
        fn groups(&self) -> TypeGroups {
            unsafe {
                self.window
                    .data::<TypeGroups>("wayterm-session-dialog-groups")
                    .map(|d| (*d.as_ptr()).clone())
                    .expect("dialog groups missing")
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

        /// Add a label + [`gtk4::DropDown`] row to `grid` and return the
        /// dropdown.
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
    /// Widget-level tests for the tmux group.
    ///
    /// These need a display, so they are `#[ignore]`d by default and run
    /// under a virtual one:
    ///
    /// ```text
    /// xvfb-run -a cargo test -- --ignored gtk_dialog
    /// ```
    ///
    /// The pure mapping logic (what gets *stored*) is covered without a
    /// display in the sibling [`tests`](super::tests) module; what is only
    /// observable here is the Entry's auto-update behaviour and the SSH
    /// group's visibility, both of which the GTK API can express but plain
    /// data cannot.
    #[cfg(test)]
    mod gtk_tests {
        use crate::store::model::{SessionConfig, SessionType};

        use super::*;

        // One test, not several: gtk4::init() can only win the default main
        // context once per process, and cargo runs tests in parallel
        // threads, so a second GTK test would race this one for it.
        #[test]
        #[ignore = "needs a display; run under xvfb-run"]
        fn tmux_group_entry_follows_name_until_user_edits() {
            gtk4::init().expect("gtk init");

            // New session: Entry pre-filled from the (empty) name, box off.
            let parent = gtk4::Window::new();
            let dialog = SessionDialog::new_session(&parent, &[]);
            let widgets = dialog.widgets();
            assert_eq!(widgets.tmux_session_name_entry.text(), "wayterm-");
            assert!(!widgets.tmux_check.is_active());

            // Name edits drive the Entry.
            widgets.name_entry.set_text("prod");
            assert_eq!(widgets.tmux_session_name_entry.text(), "wayterm-prod");
            widgets.name_entry.set_text("prod2");
            assert_eq!(widgets.tmux_session_name_entry.text(), "wayterm-prod2");

            // Once the user edits the Entry, the Name field stops touching it.
            widgets.tmux_session_name_entry.set_text("wayterm-custom");
            widgets.name_entry.set_text("other");
            assert_eq!(widgets.tmux_session_name_entry.text(), "wayterm-custom");

            // SSH reveals the tmux group; other types hide it.
            let groups = dialog.groups();
            let revealed = |g: &gtk4::Revealer| g.property::<bool>("reveal-child");
            groups.apply(TYPE_INDEX_SSH);
            assert!(revealed(&groups.ssh), "SSH reveals its tmux group");
            groups.apply(TYPE_INDEX_SERIAL);
            assert!(!revealed(&groups.ssh), "serial hides the tmux group");
            drop(dialog);

            // Edit mode pre-fills box + full remote name, and the stored
            // name is not clobbered by later name edits.
            let config = SessionConfig {
                id: 7,
                name: "prod".to_string(),
                session_type: SessionType::SSH,
                tmux_enabled: true,
                tmux_session_name: Some("shared".to_string()),
                ..Default::default()
            };
            let dialog = SessionDialog::edit(&parent, &config, &[]);
            let widgets = dialog.widgets();
            assert!(widgets.tmux_check.is_active());
            assert_eq!(widgets.tmux_session_name_entry.text(), "wayterm-shared");
            widgets.name_entry.set_text("renamed");
            assert_eq!(widgets.tmux_session_name_entry.text(), "wayterm-shared");
            drop(dialog);

            // Edit mode on a row with no stored name shows the derived one,
            // and it still follows the Name field (pre-fill is not a user
            // edit).
            let config = SessionConfig {
                id: 8,
                name: "prod".to_string(),
                tmux_enabled: true,
                ..Default::default()
            };
            let dialog = SessionDialog::edit(&parent, &config, &[]);
            let widgets = dialog.widgets();
            assert_eq!(widgets.tmux_session_name_entry.text(), "wayterm-prod");
            widgets.name_entry.set_text("renamed");
            assert_eq!(widgets.tmux_session_name_entry.text(), "wayterm-renamed");
        }
    }
}

pub use gtk_dialog::SessionDialog;

#[cfg(test)]
mod tests;
