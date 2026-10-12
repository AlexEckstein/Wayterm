//! The Credentials settings page: the Bitwarden source's switch and CLI
//! path, and whether the keyring and the CLI answer (doc 11 §Credential
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
use crate::transport::credentials::{bitwarden_status, BitwardenStatus};
use crate::ui::widgets::set_margins;

// ---------------------------------------------------------------------------
// Display-free data layer
// ---------------------------------------------------------------------------

/// `1` turns the Bitwarden source on; anything else (or no row) is off.
pub const BITWARDEN_ENABLED_KEY: &str = "bitwarden.enabled";
/// The `bw` binary to run; blank or missing means [`DEFAULT_BITWARDEN_BIN`].
pub const BITWARDEN_BIN_KEY: &str = "bitwarden.bin";
pub const DEFAULT_BITWARDEN_BIN: &str = "bw";

/// The credential settings the chain is built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialSettings {
    pub bitwarden_enabled: bool,
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
        let read = |key: &str| {
            store.get_setting(key).unwrap_or_else(|err| {
                eprintln!("Wayterm: failed to read setting '{key}': {err}");
                None
            })
        };
        Self {
            bitwarden_enabled: bitwarden_enabled(read(BITWARDEN_ENABLED_KEY).as_deref()),
            bitwarden_bin: bitwarden_bin(read(BITWARDEN_BIN_KEY).as_deref()),
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

pub fn save_enabled(store: &SessionStore, on: bool) -> rusqlite::Result<()> {
    store.set_setting(BITWARDEN_ENABLED_KEY, if on { "1" } else { "0" })
}

pub fn save_bin(store: &SessionStore, bin: &str) -> rusqlite::Result<()> {
    store.set_setting(BITWARDEN_BIN_KEY, bin.trim())
}

/// Whether the OS keyring answers.
// BRIDGE(K1): connect KeyringSource::available() here. The stub keyring on
// main stores nothing, so it reports unavailable.
pub fn probe_keyring() -> bool {
    false
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

    let heading = gtk4::Label::new(Some("Credential sources"));
    heading.add_css_class("heading");
    heading.set_xalign(0.0);
    let intro = gtk4::Label::new(Some(
        "Secrets come from the OS keyring first, then Bitwarden (when on), then a prompt. \
         Bitwarden items are named wayterm/<session name>; unlock the vault and export \
         BW_SESSION before starting Wayterm.",
    ));
    intro.set_xalign(0.0);
    intro.set_wrap(true);
    intro.add_css_class("dim-label");

    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");

    let switch = gtk4::Switch::builder()
        .active(settings.bitwarden_enabled)
        .valign(gtk4::Align::Center)
        .build();
    list.append(&row("Use Bitwarden", &switch));

    let bin = gtk4::Entry::builder()
        .placeholder_text(DEFAULT_BITWARDEN_BIN)
        .hexpand(true)
        .build();
    if settings.bitwarden_bin != DEFAULT_BITWARDEN_BIN {
        bin.set_text(&settings.bitwarden_bin);
    }
    list.append(&row("Bitwarden CLI path", &bin));

    let keyring_status = status_label(keyring_status_text(false));
    list.append(&row_of(&keyring_status));
    let bitwarden_row = row_of_label();
    bitwarden_row.0.set_visible(settings.bitwarden_enabled);
    list.append(&bitwarden_row.0);

    probe_keyring_into(&keyring_status);
    if settings.bitwarden_enabled {
        probe_bitwarden_into(&bitwarden_row.1, settings.bitwarden_bin.clone());
    }

    {
        let (store, row, label, bin) = (
            store.clone(),
            bitwarden_row.0.clone(),
            bitwarden_row.1.clone(),
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
        let (switch, label) = (switch.clone(), bitwarden_row.1.clone());
        bin.connect_activate(move |entry| {
            if switch.is_active() {
                probe_bitwarden_into(&label, bitwarden_bin(Some(&entry.text())));
            }
        });
    }

    let column = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    set_margins(&column, 18);
    column.append(&heading);
    column.append(&intro);
    column.append(&list);

    let scrolled = gtk4::ScrolledWindow::builder()
        .child(&column)
        .vexpand(true)
        .hexpand(true)
        .build();
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scrolled.upcast()
}

/// title | control.
fn row(title: &str, control: &impl IsA<gtk4::Widget>) -> gtk4::ListBoxRow {
    let label = gtk4::Label::new(Some(title));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    control.upcast_ref::<gtk4::Widget>().update_relation(&[
        gtk4::accessible::Relation::LabelledBy(&[label.upcast_ref()]),
    ]);
    let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    set_margins(&hbox, 12);
    hbox.append(&label);
    hbox.append(control);
    gtk4::ListBoxRow::builder()
        .child(&hbox)
        .activatable(false)
        .build()
}

fn status_label(text: &str) -> gtk4::Label {
    let label = gtk4::Label::new(Some(text));
    label.set_xalign(0.0);
    label
}

fn row_of(label: &gtk4::Label) -> gtk4::ListBoxRow {
    set_margins(label, 12);
    gtk4::ListBoxRow::builder()
        .child(label)
        .activatable(false)
        .build()
}

fn row_of_label() -> (gtk4::ListBoxRow, gtk4::Label) {
    let label = status_label("Bitwarden: checking…");
    (row_of(&label), label)
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
