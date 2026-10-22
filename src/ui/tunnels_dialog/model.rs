//! The Tunnels dialog's display-free model: editable rule drafts with
//! validation, and the live-status summaries the rows render.
//!
//! [`ForwardRuleDraft`] keeps ports as text so the dialog can show
//! "not a number" inline instead of refusing the keystroke; only valid
//! drafts convert into the stored [`ForwardRule`] (doc 09 wire keys
//! `kind/bind_addr/bind_port/remote_host/remote_port/enabled` are never
//! renamed here). The session dialog reuses the drafts and the validators
//! for its own forward-rules editor, so both editors accept exactly the
//! same input.

use super::status_source::{ForwardEvent, ForwardState, RuleId};
use crate::store::model::{ForwardKind, ForwardRule};

/// `DropDown` positions for the kind selector, in [`ForwardKind`] order.
pub const KIND_INDEX_LOCAL: u32 = 0;
pub const KIND_INDEX_REMOTE: u32 = 1;
pub const KIND_INDEX_DYNAMIC: u32 = 2;

/// The kind labels. `gtk4::DropDown` has no per-item sensitivity, so the
/// unsupported kinds stay choosable but are marked in their label,
/// refused by validation with [`V2_NOTE`], and explained in a tooltip.
pub const FORWARD_KIND_LABELS: [&str; 3] = ["Local", "Remote (v2)", "Dynamic (v2)"];

/// Why a non-Local kind is refused (doc 04 §Limitations).
pub const V2_NOTE: &str =
    "Remote and dynamic forwarding are v2 (doc 04) — only Local rules start in this version.";

/// The help popover's contents: one (term, explanation) pair per row
/// control, in row order, plus how saving works.
pub const HELP_ENTRIES: [(&str, &str); 9] = [
    (
        "Kind",
        "Local forwards a port on this computer to a host the SSH server can \
         reach (like ssh -L). Remote (ssh -R) and Dynamic (SOCKS, ssh -D) are \
         planned for v2 and never start in this version.",
    ),
    (
        "Bind address",
        "The local address the listener binds to. Keep 127.0.0.1 so only this \
         computer can connect; 0.0.0.0 opens the port to your whole network.",
    ),
    (
        "Bind port",
        "The local port to listen on (1-65535). Point your client at \
         bind address:bind port. Ports below 1024 usually need root.",
    ),
    (
        "Remote host",
        "The host the SSH server connects to on your behalf. It is resolved on \
         the server, so localhost means the server itself.",
    ),
    (
        "Remote port",
        "The port on the remote host that traffic is delivered to (1-65535).",
    ),
    (
        "Enabled",
        "Whether the rule runs with the connection. While connected, toggling \
         it starts or stops the listener immediately.",
    ),
    (
        "Status",
        "Live state of the listener: Stopped, Listening, the number of open \
         connections, or an error. Reads \"no active connection\" when the \
         session is not connected.",
    ),
    ("Remove", "Deletes the rule."),
    (
        "Saving",
        "OK saves the rules; Cancel discards edits. New or changed rules take \
         effect the next time the session is opened in a tab. Tunnels close \
         with their tab.",
    ),
];

/// The status cell when the dialog has no engine to ask (offline or a
/// non-SSH session): editing still works, live state does not.
pub const OFFLINE_STATUS: &str = "no active connection";

/// One editable forward-rule row. Ports are text: [`validate_draft`]
/// reports non-numeric input as an inline error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardRuleDraft {
    pub kind_index: u32,
    pub bind_addr: String,
    pub bind_port: String,
    pub remote_host: String,
    pub remote_port: String,
    pub enabled: bool,
}

impl Default for ForwardRuleDraft {
    fn default() -> Self {
        Self {
            kind_index: KIND_INDEX_LOCAL,
            bind_addr: "127.0.0.1".to_string(),
            bind_port: String::new(),
            remote_host: String::new(),
            remote_port: String::new(),
            enabled: true,
        }
    }
}

/// `DropDown` position → kind; anything unknown is Local.
pub fn kind_from_index(index: u32) -> ForwardKind {
    match index {
        KIND_INDEX_REMOTE => ForwardKind::Remote,
        KIND_INDEX_DYNAMIC => ForwardKind::Dynamic,
        _ => ForwardKind::Local,
    }
}

pub fn kind_index_for(kind: &ForwardKind) -> u32 {
    match kind {
        ForwardKind::Local => KIND_INDEX_LOCAL,
        ForwardKind::Remote => KIND_INDEX_REMOTE,
        ForwardKind::Dynamic => KIND_INDEX_DYNAMIC,
    }
}

/// The stored rule as an editable draft (ports rendered as text).
pub fn draft_from_rule(rule: &ForwardRule) -> ForwardRuleDraft {
    ForwardRuleDraft {
        kind_index: kind_index_for(&rule.kind),
        bind_addr: rule.bind_addr.clone(),
        bind_port: rule.bind_port.to_string(),
        remote_host: rule.remote_host.clone(),
        remote_port: rule.remote_port.to_string(),
        enabled: rule.enabled,
    }
}

/// The session's stored rules as drafts; `None` (no rules yet) is no rows.
pub fn drafts_from_rules(rules: Option<&Vec<ForwardRule>>) -> Vec<ForwardRuleDraft> {
    rules.map_or_else(Vec::new, |rules| {
        rules.iter().map(draft_from_rule).collect()
    })
}

fn parse_port(text: &str, what: &str) -> Result<u16, String> {
    let err = || format!("{what} must be a port 1-65535");
    let port: u16 = text.trim().parse().map_err(|_| err())?;
    if port == 0 {
        Err(err())
    } else {
        Ok(port)
    }
}

/// The stored rule a draft describes. Only Local converts: Remote/Dynamic
/// rows are viewable but refused with [`V2_NOTE`].
pub fn validate_draft(draft: &ForwardRuleDraft) -> Result<ForwardRule, String> {
    let kind = kind_from_index(draft.kind_index);
    if kind != ForwardKind::Local {
        return Err(V2_NOTE.to_string());
    }
    if draft.bind_addr.trim().is_empty() {
        return Err("bind address must not be empty for a Local rule".to_string());
    }
    let bind_port = parse_port(&draft.bind_port, "bind port")?;
    if draft.remote_host.trim().is_empty() {
        return Err("remote host must not be empty".to_string());
    }
    let remote_port = parse_port(&draft.remote_port, "remote port")?;
    Ok(ForwardRule {
        kind,
        bind_addr: draft.bind_addr.trim().to_string(),
        bind_port,
        remote_host: draft.remote_host.trim().to_string(),
        remote_port,
        enabled: draft.enabled,
    })
}

/// Every draft converted, or the per-row errors in row order (`None` for
/// rows that converted). The dialogs show the `Some` entries inline.
pub fn rules_from_drafts(
    drafts: &[ForwardRuleDraft],
) -> Result<Vec<ForwardRule>, Vec<Option<String>>> {
    let mut rules = Vec::with_capacity(drafts.len());
    let mut errors = vec![None; drafts.len()];
    let mut failed = false;
    for (index, draft) in drafts.iter().enumerate() {
        match validate_draft(draft) {
            Ok(rule) => rules.push(rule),
            Err(err) => {
                errors[index] = Some(err);
                failed = true;
            }
        }
    }
    if failed {
        Err(errors)
    } else {
        Ok(rules)
    }
}

/// The state of one rule by id; missing means the engine never reported
/// it, which renders as [`ForwardState::Stopped`].
pub fn lookup_status(statuses: &[ForwardEvent], rule_id: RuleId) -> Option<&ForwardState> {
    statuses
        .iter()
        .find(|status| status.rule_id == rule_id)
        .map(|status| &status.state)
}

/// The live-status cell. `online == false` is the offline fallback: the
/// dialog edits rules but has no engine, so every cell reads
/// [`OFFLINE_STATUS`] and engine controls stay insensitive.
pub fn status_text(state: Option<&ForwardState>, online: bool) -> String {
    if !online {
        return OFFLINE_STATUS.to_string();
    }
    match state {
        None | Some(ForwardState::Stopped) => "Stopped".to_string(),
        Some(ForwardState::Listening) => "Listening".to_string(),
        Some(ForwardState::Active { conns }) => {
            if *conns == 1 {
                "1 connection".to_string()
            } else {
                format!("{conns} connections")
            }
        }
        Some(ForwardState::LastFailed { error }) => format!("Error: {error}"),
    }
}
