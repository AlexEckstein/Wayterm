//! The Credentials settings page: the Bitwarden source's switch and CLI
//! path, and whether the keyring and the CLI answer (doc 05 §Credential
//! sources). Every change is saved at once; the chain reads the rows at
//! each connect.
//!
//! The settings rows and the probe classification are display-free;
//! [`page`] only renders them. Unlike the General switches, a missing
//! `bitwarden.enabled` row reads as **off**: running an external CLI is
//! opt-in.

use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::store::SessionStore;
use crate::transport::credentials::{bitwarden_status, BitwardenStatus, KeyringSource};

// ---------------------------------------------------------------------------
// Display-free data layer
// ---------------------------------------------------------------------------

/// `1` turns the Bitwarden source on; anything else (or no row) is off.
pub const BITWARDEN_ENABLED_KEY: &str = "bitwarden.enabled";
/// The `bw` binary to run; blank or missing means [`DEFAULT_BITWARDEN_BIN`].
pub const BITWARDEN_BIN_KEY: &str = "bitwarden.bin";
/// The CLI run when `bitwarden.bin` is blank or missing.
pub const DEFAULT_BITWARDEN_BIN: &str = "bw";

/// The credential settings the chain is built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialSettings {
    /// `bitwarden.enabled`: off unless explicitly on.
    pub bitwarden_enabled: bool,
    /// `bitwarden.bin`, trimmed.
    pub bitwarden_bin: String,
}

impl Default for CredentialSettings {
    fn default() -> Self {
        Self {
            bitwarden_enabled: false,
            bitwarden_bin: DEFAULT_BITWARDEN_BIN.to_string(),
        }
    }
}

impl CredentialSettings {
    /// The stored rows; an unreadable row counts as missing.
    pub fn load(store: &SessionStore) -> Self {
        Self {
            bitwarden_enabled: bitwarden_enabled(store.setting(BITWARDEN_ENABLED_KEY).as_deref()),
            bitwarden_bin: bitwarden_bin(store.setting(BITWARDEN_BIN_KEY).as_deref()),
        }
    }
}

/// Opt-in: only an explicit `1` (or `true`/`yes`/`on`) enables.
pub fn bitwarden_enabled(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// The trimmed path, or the default when blank or missing.
pub fn bitwarden_bin(raw: Option<&str>) -> String {
    match raw.map(str::trim) {
        Some(bin) if !bin.is_empty() => bin.to_string(),
        _ => DEFAULT_BITWARDEN_BIN.to_string(),
    }
}

/// Write `bitwarden.enabled`.
pub fn save_enabled(store: &SessionStore, on: bool) -> rusqlite::Result<()> {
    store.set_setting(BITWARDEN_ENABLED_KEY, if on { "1" } else { "0" })
}

/// Write `bitwarden.bin`, trimmed.
pub fn save_bin(store: &SessionStore, bin: &str) -> rusqlite::Result<()> {
    store.set_setting(BITWARDEN_BIN_KEY, bin.trim())
}

/// Whether the OS keyring answers.
pub fn probe_keyring() -> bool {
    KeyringSource::new("_live_connect", "").available()
}

/// The keyring status row.
pub fn keyring_status_text(available: bool) -> &'static str {
    if available {
        "Keyring: available"
    } else {
        "Keyring: unavailable"
    }
}

/// The Bitwarden status row: one line, never vault contents.
pub fn bitwarden_status_text(status: &BitwardenStatus) -> String {
    match status {
        BitwardenStatus::Detected { state: Some(state) } if state == "unauthenticated" => {
            "Bitwarden: detected (not logged in)".to_string()
        }
        BitwardenStatus::Detected { .. } => "Bitwarden: detected".to_string(),
        BitwardenStatus::NotDetected(_) => "Bitwarden: not detected".to_string(),
    }
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// The page widget: the Bitwarden switch and path over the two status rows.
pub fn page(store: Rc<SessionStore>) -> gtk4::Widget {
    let settings = CredentialSettings::load(&store);

    let intro = gtk4::Label::new(Some(
        "Secrets come from the OS keyring first, then Bitwarden (when on), then a prompt. \
         Bitwarden items are named wayterm/<session name>; unlock the vault and export \
         BW_SESSION before starting Wayterm.",
    ));
    intro.set_xalign(0.0);
    intro.set_wrap(true);
    intro.add_css_class("dim-label");

    let switch = gtk4::Switch::builder()
        .active(settings.bitwarden_enabled)
        .valign(gtk4::Align::Center)
        .build();
    let bin = gtk4::Entry::builder()
        .placeholder_text(DEFAULT_BITWARDEN_BIN)
        .hexpand(true)
        .build();
    if settings.bitwarden_bin != DEFAULT_BITWARDEN_BIN {
        bin.set_text(&settings.bitwarden_bin);
    }
    let keyring_status = status_label(keyring_status_text(false));
    let bitwarden_status = status_label("Bitwarden: checking…");
    let bitwarden_row = super::row(&[bitwarden_status.upcast_ref()]);
    bitwarden_row.set_visible(settings.bitwarden_enabled);

    let list = super::boxed_list();
    list.append(&labelled_row("Use Bitwarden", &switch));
    list.append(&labelled_row("Bitwarden CLI path", &bin));
    list.append(&super::row(&[keyring_status.upcast_ref()]));
    list.append(&bitwarden_row);

    probe_keyring_into(&keyring_status);
    if settings.bitwarden_enabled {
        probe_bitwarden_into(&bitwarden_status, settings.bitwarden_bin.clone());
    }

    {
        let (store, row, label, bin) = (
            store.clone(),
            bitwarden_row.clone(),
            bitwarden_status.clone(),
            bin.clone(),
        );
        switch.connect_active_notify(move |switch| {
            let on = switch.is_active();
            if let Err(err) = save_enabled(&store, on) {
                eprintln!("Wayterm: failed to save setting '{BITWARDEN_ENABLED_KEY}': {err}");
            }
            row.set_visible(on);
            if on {
                probe_bitwarden_into(&label, bitwarden_bin(Some(&bin.text())));
            }
        });
    }
    {
        let store = store.clone();
        bin.connect_changed(move |entry| {
            if let Err(err) = save_bin(&store, &entry.text()) {
                eprintln!("Wayterm: failed to save setting '{BITWARDEN_BIN_KEY}': {err}");
            }
        });
    }
    {
        let (switch, label) = (switch.clone(), bitwarden_status.clone());
        bin.connect_activate(move |entry| {
            if switch.is_active() {
                probe_bitwarden_into(&label, bitwarden_bin(Some(&entry.text())));
            }
        });
    }

    super::scrolling_page(&[
        super::heading("Credential sources").upcast_ref(),
        intro.upcast_ref(),
        list.upcast_ref(),
    ])
}

/// title | control.
fn labelled_row(title: &str, control: &impl IsA<gtk4::Widget>) -> gtk4::ListBoxRow {
    let text = super::described(title, None, control);
    super::row(&[text.upcast_ref(), control.upcast_ref()])
}

/// A left-aligned status line.
fn status_label(text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(Some(text));
    label.set_xalign(0.0);
    label
}

/// Probe off the UI thread; a failed probe reads as unavailable.
fn probe_keyring_into(label: &gtk4::Label) {
    let label = label.clone();
    glib::MainContext::default().spawn_local(async move {
        let available = gtk4::gio::spawn_blocking(probe_keyring)
            .await
            .unwrap_or(false);
        label.set_text(keyring_status_text(available));
    });
}

/// Run `<bin> status` off the UI thread and show the one-line result.
fn probe_bitwarden_into(label: &gtk4::Label, bin: String) {
    label.set_text("Bitwarden: checking…");
    let label = label.clone();
    glib::MainContext::default().spawn_local(async move {
        let status = gtk4::gio::spawn_blocking(move || bitwarden_status(&bin))
            .await
            .unwrap_or_else(|_| BitwardenStatus::NotDetected("probe failed".into()));
        label.set_text(&bitwarden_status_text(&status));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_bw() {
        let store = SessionStore::in_memory().unwrap();
        assert_eq!(
            CredentialSettings::load(&store),
            CredentialSettings::default()
        );
        assert!(!CredentialSettings::default().bitwarden_enabled);
        assert_eq!(CredentialSettings::default().bitwarden_bin, "bw");
    }

    #[test]
    fn saved_rows_load_back() {
        let store = SessionStore::in_memory().unwrap();
        save_enabled(&store, true).unwrap();
        save_bin(&store, "  /opt/bw/bin/bw ").unwrap();
        assert_eq!(
            store.get_setting(BITWARDEN_ENABLED_KEY).unwrap().as_deref(),
            Some("1")
        );
        assert_eq!(
            CredentialSettings::load(&store),
            CredentialSettings {
                bitwarden_enabled: true,
                bitwarden_bin: "/opt/bw/bin/bw".into(),
            }
        );
        save_enabled(&store, false).unwrap();
        save_bin(&store, "").unwrap();
        assert_eq!(
            CredentialSettings::load(&store),
            CredentialSettings::default()
        );
    }

    #[test]
    fn enabled_is_opt_in() {
        assert!(!bitwarden_enabled(None));
        assert!(!bitwarden_enabled(Some("")));
        assert!(!bitwarden_enabled(Some("0")));
        assert!(!bitwarden_enabled(Some("maybe")));
        assert!(bitwarden_enabled(Some("1")));
        assert!(bitwarden_enabled(Some(" On ")));
    }

    #[test]
    fn blank_bin_falls_back_to_bw() {
        assert_eq!(bitwarden_bin(None), "bw");
        assert_eq!(bitwarden_bin(Some("   ")), "bw");
        assert_eq!(bitwarden_bin(Some(" /usr/bin/bw ")), "/usr/bin/bw");
    }

    #[test]
    fn status_texts() {
        assert_eq!(keyring_status_text(true), "Keyring: available");
        assert_eq!(keyring_status_text(false), "Keyring: unavailable");
        assert_eq!(keyring_status_text(probe_keyring()), "Keyring: unavailable");
        let detected = |state: Option<&str>| BitwardenStatus::Detected {
            state: state.map(str::to_string),
        };
        assert_eq!(
            bitwarden_status_text(&detected(Some("locked"))),
            "Bitwarden: detected"
        );
        assert_eq!(
            bitwarden_status_text(&detected(Some("unauthenticated"))),
            "Bitwarden: detected (not logged in)"
        );
        assert_eq!(
            bitwarden_status_text(&detected(None)),
            "Bitwarden: detected"
        );
        assert_eq!(
            bitwarden_status_text(&BitwardenStatus::NotDetected("x".into())),
            "Bitwarden: not detected"
        );
    }
}
