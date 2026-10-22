//! The display-free half of the session dialog: [`DialogFields`] and the
//! mapping to and from [`SessionConfig`], plus the jump-host picker rules.

use crate::store::model::{
    strip_tmux_prefix, vocab_index, vocab_value, AuthMethod, Folder, SessionConfig, SessionType,
    DEFAULT_SERIAL_BAUD, RDP_COLOR_DEPTHS, RDP_COLOR_DEPTH_DEFAULT, SERIAL_DATA_BITS,
    SERIAL_DATA_BITS_DEFAULT, SERIAL_FLOW_CONTROLS, SERIAL_FLOW_CONTROL_DEFAULT, SERIAL_PARITIES,
    SERIAL_PARITY_DEFAULT, SERIAL_STOP_BITS, SERIAL_STOP_BITS_DEFAULT, TMUX_NAME_PREFIX,
};
use crate::store::SessionStore;
use crate::ui::tunnels_dialog::model::{rules_from_drafts, ForwardRuleDraft};

// Type DropDown indices, in doc 09 order.
pub const TYPE_INDEX_SSH: u32 = 0;
pub const TYPE_INDEX_RDP: u32 = 1;
pub const TYPE_INDEX_VNC: u32 = 2;
pub const TYPE_INDEX_SERIAL: u32 = 3;
pub const TYPE_INDEX_LOCAL: u32 = 4;

pub(super) const TYPE_LABELS: [&str; 5] = ["ssh", "rdp", "vnc", "serial", "local"];
pub(super) const AUTH_LABELS: [&str; 5] =
    ["password", "key", "agent", "keyboard-interactive", "none"];

/// Shown in the Port spin for types without a port (it cannot show
/// "unset"); nothing is stored for them.
pub(super) const PORT_SPIN_PLACEHOLDER: u16 = 22;

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
    /// The edited row (the default for a new session): its id and
    /// metadata, and the columns the dialog has no widget for, carry
    /// through [`config_from_fields`].
    pub base: SessionConfig,
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
            base: SessionConfig::default(),
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

pub(super) fn auth_index_for(method: Option<&AuthMethod>) -> u32 {
    match method {
        Some(AuthMethod::Password) => 0,
        Some(AuthMethod::Key) => 1,
        Some(AuthMethod::Agent) => 2,
        Some(AuthMethod::KeyboardInteractive) => 3,
        Some(AuthMethod::None) | None => 4,
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
        .filter(|s| s.session_type.can_be_jump_host() && s.id != self_id)
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
    if !peer.session_type.can_be_jump_host() {
        return Some(format!(
            "“{}” is not an SSH session — only SSH sessions can be jump hosts.",
            peer.name
        ));
    }
    if peer.jump_session_id.is_some() {
        return Some(format!(
            "“{}” itself jumps through another session — depth > 1 is not supported (doc 04).",
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

/// Every folder in the store, nested ones included, in tree order (each
/// folder directly followed by its subfolders): the folder picker's source.
pub fn collect_folders(store: &SessionStore) -> rusqlite::Result<Vec<Folder>> {
    fn walk(
        store: &SessionStore,
        parent: Option<i64>,
        out: &mut Vec<Folder>,
    ) -> rusqlite::Result<()> {
        for folder in store.list_folders(parent)? {
            let id = folder.id;
            out.push(folder);
            walk(store, Some(id), out)?;
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(store, None, &mut out)?;
    Ok(out)
}

/// The folder picker's labels for `folders` (in tree order): a root folder
/// is its bare name, a nested one hangs off its parent with a `└──`
/// branch indented one step per extra level.
pub fn folder_labels(folders: &[Folder]) -> Vec<String> {
    let depth = |folder: &Folder| {
        let mut depth = 0;
        let mut parent = folder.parent_id;
        // Bounded by the list length so a corrupt cycle cannot hang.
        while let Some(id) = parent.filter(|_| depth < folders.len()) {
            depth += 1;
            parent = folders
                .iter()
                .find(|f| f.id == id)
                .and_then(|f| f.parent_id);
        }
        depth
    };
    folders
        .iter()
        .map(|folder| match depth(folder) {
            0 => folder.name.clone(),
            d => format!("{}└── {}", "    ".repeat(d - 1), folder.name),
        })
        .collect()
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
///
/// Columns without a widget — the appearance overrides, and per type the
/// key path, host-key policy and SSH, RDP or VNC options — keep the edited
/// row's values, the per-type ones only while its type is unchanged: a
/// save must never wipe what the dialog cannot show.
pub fn config_from_fields(
    fields: &DialogFields,
    folders: &[Folder],
    peers: &[SessionConfig],
) -> SessionConfig {
    let base = &fields.base;
    let session_type = session_type_from_index(fields.type_index);
    let kept = (base.session_type == session_type).then_some(base);
    let folder_id = (fields.folder_index as usize)
        .checked_sub(1)
        .and_then(|i| folders.get(i))
        .map(|f| f.id);
    let port = session_type.default_port().map(|default| {
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
        id: base.id,
        name: fields.name.trim().to_string(),
        session_type: session_type.clone(),
        folder_id,
        host: trimmed_opt(&fields.host),
        port,
        username: trimmed_opt(&fields.username),
        auth_method: Some(auth_method_from_index(fields.auth_index)),
        // Doc 09 defaults `rdp_compression` on; there is no widget for it.
        rdp_compression: kept.map_or(session_type == SessionType::RDP, |b| b.rdp_compression),
        tmux_enabled: fields.tmux_enabled,
        tmux_session_name,
        icon: trimmed_opt(&fields.icon),
        preferred_backend: base.preferred_backend.clone(),
        color_scheme: base.color_scheme.clone(),
        font_family: base.font_family.clone(),
        font_size: base.font_size,
        opacity: base.opacity,
        last_connected: base.last_connected.clone(),
        connect_count: base.connect_count,
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
            config.rdp_options = kept.and_then(|b| b.rdp_options.clone());
        }
        SessionType::LocalShell => {
            config.local_shell = trimmed_opt(&fields.local_shell);
            config.local_working_dir = trimmed_opt(&fields.local_working_dir);
        }
        // VNC's only secret is keyring-only (doc 05).
        SessionType::VNC => {
            if let Some(kept) = kept {
                config.vnc_color_depth = kept.vnc_color_depth;
                config.vnc_options = kept.vnc_options.clone();
            }
        }
        SessionType::SSH => {
            if let Some(kept) = kept {
                config.key_path = kept.key_path.clone();
                config.host_key_policy = kept.host_key_policy.clone();
                config.ssh_options = kept.ssh_options.clone();
            }
            config.jump_session_id = resolve_jump(peers, base.id, fields.jump_session_id);
            config.forward_rules = match rules_from_drafts(&fields.forward_drafts) {
                Ok(rules) if !rules.is_empty() => Some(rules),
                _ => None,
            };
        }
    }
    config
}
