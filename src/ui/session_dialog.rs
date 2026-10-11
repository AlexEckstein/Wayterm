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
use crate::store::SessionStore;
use crate::ui::tunnels_dialog::model::{
    drafts_from_rules, rules_from_drafts, ForwardRuleDraft, FORWARD_KIND_LABELS, V2_NOTE,
};
use crate::ui::widgets::{set_margins, Modal};

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
    /// The chosen jump host's session id (`None` = direct). Validated by
    /// [`resolve_jump`]; only stored for SSH.
    pub jump_session_id: Option<i64>,
    /// Editable forward rules (ports as text); only stored for SSH, and
    /// only when every draft validates.
    pub forward_drafts: Vec<ForwardRuleDraft>,
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
            jump_session_id: None,
            forward_drafts: Vec::new(),
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

/// Label of the "no jump host" picker entry.
pub const JUMP_NONE_LABEL: &str = "None (direct)";

/// The picker entry for a `jump_session_id` whose session is gone (deleted
/// since, or an import that could not map it): the dialog shows this
/// instead of failing, and saving resolves it to `None` (see
/// [`resolve_jump`]).
pub fn jump_missing_label(id: i64) -> String {
    format!("jump host missing (id {id})")
}

/// Jump candidates: SSH sessions other than the edited one. Depth>1 peers
/// (which themselves jump) stay listed so the picker can refuse them with
/// [`jump_error`]'s message instead of hiding them.
pub fn jump_candidates(peers: &[SessionConfig], self_id: i64) -> Vec<&SessionConfig> {
    peers
        .iter()
        .filter(|s| s.session_type == SessionType::SSH && s.id != self_id)
        .collect()
}

/// Why `desired` cannot be the jump host, if it cannot. `None` (direct) is
/// always fine; a stale id reports the peer as missing.
pub fn jump_error(peers: &[SessionConfig], self_id: i64, desired: Option<i64>) -> Option<String> {
    let id = desired?;
    if id == self_id {
        return Some("A session cannot jump through itself.".to_string());
    }
    let Some(peer) = peers.iter().find(|s| s.id == id) else {
        return Some(format!(
            "{} — it may have been deleted; saving clears it.",
            jump_missing_label(id)
        ));
    };
    if peer.session_type != SessionType::SSH {
        return Some(format!(
            "“{}” is not an SSH session — only SSH sessions can be jump hosts.",
            peer.name
        ));
    }
    if peer.jump_session_id.is_some() {
        return Some(format!(
            "“{}” itself jumps through another session — depth > 1 is not supported (doc 11).",
            peer.name
        ));
    }
    None
}

/// The stored `jump_session_id` for `desired`: `None` unless the choice is
/// valid. Stale ids self-heal to `None` on save instead of violating the
/// foreign key.
pub fn resolve_jump(peers: &[SessionConfig], self_id: i64, desired: Option<i64>) -> Option<i64> {
    desired.filter(|_| jump_error(peers, self_id, desired).is_none())
}

/// The picker model: entry labels, the id each entry stores (`None` for
/// direct), and the selected position for the edited row's `current` id. A
/// stale `current` appends the missing entry so the dialog shows it rather
/// than failing.
pub fn jump_picker_model(
    peers: &[SessionConfig],
    self_id: i64,
    current: Option<i64>,
) -> (Vec<String>, Vec<Option<i64>>, u32) {
    let mut labels = vec![JUMP_NONE_LABEL.to_string()];
    let mut ids: Vec<Option<i64>> = vec![None];
    for peer in jump_candidates(peers, self_id) {
        labels.push(peer.name.clone());
        ids.push(Some(peer.id));
    }
    let selected = match current {
        None => 0,
        Some(id) => ids.iter().position(|entry| *entry == Some(id)).map_or_else(
            || {
                labels.push(jump_missing_label(id));
                ids.push(Some(id));
                (labels.len() - 1) as u32
            },
            |pos| pos as u32,
        ),
    };
    (labels, ids, selected)
}

/// Every session in the store, across folders, in tree order: the jump
/// picker's source. (The store only lists one folder at a time, so this
/// walks the way the session tree does.)
pub fn collect_peer_sessions(store: &SessionStore) -> Vec<SessionConfig> {
    fn walk(store: &SessionStore, parent: Option<i64>, out: &mut Vec<SessionConfig>) {
        if let Ok(folders) = store.list_folders(parent) {
            for folder in folders {
                walk(store, Some(folder.id), out);
            }
        }
        if let Ok(sessions) = store.list_sessions(parent) {
            out.extend(sessions);
        }
    }
    let mut out = Vec::new();
    walk(store, None, &mut out);
    out
}

/// The session the dialog's values describe. An out-of-range port falls
/// back to the type's default. Per-type columns are written only for the
/// selected type, so values left in a hidden group never leak; the tmux
/// columns are common and stored as typed (only SSH reads them). Jump and
/// forward rules are SSH-owned the same way: they store only for SSH, and
/// invalid drafts (which the dialog's OK validation never lets through)
/// defensively store as no rules rather than partial ones.
pub fn config_from_fields(
    fields: &DialogFields,
    folders: &[Folder],
    peers: &[SessionConfig],
) -> SessionConfig {
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
    if session_type == SessionType::SSH {
        config.jump_session_id = resolve_jump(peers, fields.id, fields.jump_session_id);
        config.forward_rules = match rules_from_drafts(&fields.forward_drafts) {
            Ok(rules) if !rules.is_empty() => Some(rules),
            _ => None,
        };
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
    notes: gtk4::TextView,
    tmux: gtk4::CheckButton,
    tmux_name: gtk4::Entry,
    jump: gtk4::DropDown,
    jump_ids: Vec<Option<i64>>,
    forward_rows: Rc<RefCell<Vec<ForwardRow>>>,
    forward_error: gtk4::Label,
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
            notes: buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .into(),
            tmux_enabled: self.tmux.is_active(),
            tmux_session_name: self.tmux_name.text().into(),
            jump_session_id: self
                .jump_ids
                .get(self.jump.selected() as usize)
                .copied()
                .flatten(),
            forward_drafts: self
                .forward_rows
                .borrow()
                .iter()
                .filter_map(ForwardRow::draft)
                .collect(),
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

/// One forward-rule row in the session dialog's SSH editor. Positions
/// never shift (see the Tunnels dialog): removing hides the row and marks
/// it.
struct ForwardRow {
    kind: gtk4::DropDown,
    bind: gtk4::Entry,
    bind_port: gtk4::Entry,
    remote: gtk4::Entry,
    remote_port: gtk4::Entry,
    enabled: gtk4::CheckButton,
    removed: Rc<Cell<bool>>,
}

impl ForwardRow {
    fn draft(&self) -> Option<ForwardRuleDraft> {
        if self.removed.get() {
            return None;
        }
        Some(ForwardRuleDraft {
            kind_index: self.kind.selected(),
            bind_addr: self.bind.text().into(),
            bind_port: self.bind_port.text().into(),
            remote_host: self.remote.text().into(),
            remote_port: self.remote_port.text().into(),
            enabled: self.enabled.is_active(),
        })
    }
}

/// Append one editable row for `draft`. Only Local converts on save;
/// Remote/Dynamic stay selectable so the v2 refusal is visible, and are
/// refused with [`V2_NOTE`] by the shared validator.
fn add_forward_row(
    list: &gtk4::ListBox,
    rows: &Rc<RefCell<Vec<ForwardRow>>>,
    draft: &ForwardRuleDraft,
) {
    let kind = gtk4::DropDown::builder()
        .model(&gtk4::StringList::new(&FORWARD_KIND_LABELS))
        .tooltip_text(V2_NOTE)
        .width_request(130)
        .build();
    kind.set_selected(draft.kind_index);
    let bind = forward_entry(&draft.bind_addr, "bind addr", 120);
    let bind_port = forward_entry(&draft.bind_port, "bind port", 70);
    let remote = forward_entry(&draft.remote_host, "remote host", 130);
    let remote_port = forward_entry(&draft.remote_port, "remote port", 70);
    let enabled = gtk4::CheckButton::with_label("On");
    enabled.set_active(draft.enabled);
    enabled.set_tooltip_text(Some("Enabled rules start at connect (Local only)"));

    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    set_margins(&hbox, 4);
    hbox.append(&kind);
    hbox.append(&bind);
    hbox.append(&bind_port);
    hbox.append(&remote);
    hbox.append(&remote_port);
    hbox.append(&enabled);
    let remove = gtk4::Button::with_label("Remove");
    hbox.append(&remove);
    let list_row = gtk4::ListBoxRow::builder().child(&hbox).build();
    list.append(&list_row);

    let removed = Rc::new(Cell::new(false));
    {
        let (list_row, removed) = (list_row.clone(), removed.clone());
        remove.connect_clicked(move |_| {
            removed.set(true);
            list_row.set_visible(false);
        });
    }

    rows.borrow_mut().push(ForwardRow {
        kind,
        bind,
        bind_port,
        remote,
        remote_port,
        enabled,
        removed,
    });
}

fn forward_entry(text: &str, placeholder: &str, width: i32) -> gtk4::Entry {
    gtk4::Entry::builder()
        .text(text)
        .placeholder_text(placeholder)
        .width_request(width)
        .build()
}

fn note_label(text: &str) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .label(text)
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    label.add_css_class("dim-label");
    label
}

fn error_label() -> gtk4::Label {
    let label = gtk4::Label::builder()
        .halign(gtk4::Align::Start)
        .wrap(true)
        .build();
    label.add_css_class("error");
    label
}

/// The dialog. Build with [`Self::new_session`] or [`Self::edit`], then
/// [`Self::run`] it.
pub struct SessionDialog {
    modal: Modal<SessionConfig>,
    widgets: Widgets,
    groups: TypeGroups,
    tmux_follow: TmuxNameFollow,
    base: Rc<RefCell<DialogFields>>,
    folders: Vec<Folder>,
    peers: Vec<SessionConfig>,
    vbox: gtk4::Box,
}

impl SessionDialog {
    pub fn new_session(parent: &gtk4::Window, folders: &[Folder], peers: &[SessionConfig]) -> Self {
        Self::build(parent, folders, peers, 0, None, &[])
    }

    pub fn edit(
        parent: &gtk4::Window,
        config: &SessionConfig,
        folders: &[Folder],
        peers: &[SessionConfig],
    ) -> Self {
        let dialog = Self::build(
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

    /// Run modally; `None` on cancel. OK only closes on success: an
    /// invalid forward draft keeps the dialog open with the error inline,
    /// and the jump choice resolves through [`resolve_jump`] on save.
    pub fn run(self) -> Option<SessionConfig> {
        let ok = gtk4::Button::with_label("OK");
        let cancel = self.modal.button("Cancel", || None);
        {
            let (window, result, widgets, base, folders, peers, forward_error) = (
                self.modal.window.clone(),
                self.modal.result.clone(),
                self.widgets.clone(),
                self.base.clone(),
                self.folders.clone(),
                self.peers.clone(),
                self.widgets.forward_error.clone(),
            );
            ok.connect_clicked(move |_| {
                let fields = widgets.fields(&base.borrow());
                match rules_from_drafts(&fields.forward_drafts) {
                    Ok(_) => {
                        *result.borrow_mut() = Some(config_from_fields(&fields, &folders, &peers));
                        window.close();
                    }
                    Err(errors) => {
                        forward_error.set_text(&forward_batch_error(&errors));
                    }
                }
            });
            self.modal.window.set_default_widget(Some(&ok));
        }
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        buttons.set_halign(gtk4::Align::End);
        buttons.append(&cancel);
        buttons.append(&ok);
        buttons.set_margin_top(12);
        set_margins(&buttons, 12);
        self.vbox.append(&buttons);
        self.modal.run()
    }

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
        let folder_names: Vec<&str> = std::iter::once("None (root)")
            .chain(folders.iter().map(|f| f.name.as_str()))
            .collect();
        let folder = form.dropdown("Folder:", &folder_names);
        let host = form.entry("Host:", "");
        let port = form.spin("Port:", 1.0, 65535.0);
        let username = form.entry("Username:", "");
        let auth = form.dropdown("Auth Method:", &AUTH_LABELS);
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

        // Jump host (doc 11): one ProxyJump hop through another SSH
        // session. Depth>1 choices stay listed but are refused with an
        // inline message; a stale stored id shows the missing entry.
        let (jump_labels, jump_ids, jump_selected) =
            jump_picker_model(peers, self_id, current_jump);
        let jump = ssh.dropdown("Jump host:", &jump_labels);
        jump.set_selected(jump_selected);
        jump.set_tooltip_text(Some(
            "The SSH session to connect through (one ProxyJump hop, doc 11). Depth > 1 is refused.",
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

        // Forward rules (doc 11 §Port forwarding): the same drafts the
        // Tunnels dialog edits for a connected session.
        ssh.span(&note_label(
            "Port forwards (Local starts at connect; Remote/Dynamic are v2):",
        ));
        ssh.span(&note_label(V2_NOTE));
        let forward_list = gtk4::ListBox::new();
        forward_list.set_selection_mode(gtk4::SelectionMode::None);
        ssh.span(&forward_list);
        let forward_rows: Rc<RefCell<Vec<ForwardRow>>> = Rc::default();
        for draft in initial_drafts {
            add_forward_row(&forward_list, &forward_rows, draft);
        }
        let forward_add = gtk4::Button::with_label("Add forward");
        {
            let (list, rows) = (forward_list.clone(), forward_rows.clone());
            forward_add.connect_clicked(move |_| {
                add_forward_row(&list, &rows, &ForwardRuleDraft::default());
            });
        }
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
            notes,
            tmux,
            tmux_name,
            jump,
            jump_ids,
            forward_rows,
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

        // A type change reveals its group and resets the port to its
        // default, so a stale 22 is never stored for RDP.
        let (for_type, port) = (groups.clone(), widgets.port.clone());
        widgets
            .session_type
            .connect_notify_local(Some("selected"), move |dropdown, _| {
                for_type.apply(dropdown.selected());
                let default = default_port_for(&session_type_from_index(dropdown.selected()));
                port.set_value(default.unwrap_or(PORT_SPIN_PLACEHOLDER).into());
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

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        vbox.append(&form.grid);
        for group in &groups.0 {
            vbox.append(group);
        }
        modal.window.set_child(Some(&vbox));

        Self {
            modal,
            widgets,
            groups,
            tmux_follow,
            base,
            folders: folders.to_vec(),
            peers: peers.to_vec(),
            vbox,
        }
    }
}

/// The OK-time error line for invalid forward drafts: each failing row's
/// message with its 1-based row number.
fn forward_batch_error(errors: &[Option<String>]) -> String {
    errors
        .iter()
        .enumerate()
        .filter_map(|(index, error)| error.as_ref().map(|e| format!("Row {}: {e}", index + 1)))
        .collect::<Vec<_>>()
        .join("\n")
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
        assert!(dialog.widgets.tmux.is_active());
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-shared");
        assert_eq!(dialog.widgets.port.value(), 2222.0);
        dialog.widgets.name.set_text("renamed");
        assert_eq!(dialog.widgets.tmux_name.text(), "wayterm-shared");

        // Without a stored name, the derived one keeps following.
        let derived = SessionConfig {
            id: 8,
            name: "prod".into(),
            tmux_enabled: true,
            ..Default::default()
        };
        let dialog = SessionDialog::edit(&parent, &derived, &[], &[]);
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
        let fields = dialog.widgets.fields(&dialog.base.borrow());
        assert_eq!(fields.jump_session_id, None);

        // Picking the clean bastion sticks.
        dialog.widgets.jump.set_selected(1);
        assert_eq!(dialog.widgets.jump.selected(), 1);
        let fields = dialog.widgets.fields(&dialog.base.borrow());
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
        let fields = dialog.widgets.fields(&dialog.base.borrow());
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
        assert_eq!(dialog.widgets.forward_rows.borrow().len(), 1);
        let fields = dialog.widgets.fields(&dialog.base.borrow());
        assert_eq!(
            fields.forward_drafts.len(),
            1,
            "stored rules load into the editor"
        );
        assert_eq!(fields.forward_drafts[0].bind_port, "8080");
        dialog.modal.window.close();
    }
}
