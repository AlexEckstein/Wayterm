//! Terminal color schemes on the UI side (doc 02 §Color schemes): keeps the
//! installed `default_color_scheme`, watches the desktop theme, and pushes
//! the resolved scheme ([`color_scheme::resolve`]) to every live terminal.
//!
//! Tabs get their scheme as their terminal is created ([`apply_to`]). A
//! change of the global row ([`install_default_scheme`], from the settings
//! page) or of the desktop theme re-resolves every terminal in the tracked
//! notebooks ([`track`]). The theme comes from `gtk4::Settings` —
//! `gtk-application-prefer-dark-theme` and `gtk-theme-name` — which GTK
//! fills from xdg-desktop-portal on Linux, so a KDE or GNOME light/dark
//! switch reaches open tabs. Main thread only.

use std::cell::RefCell;

use gtk4::glib;
use gtk4::prelude::*;

use crate::terminal::color_scheme::{self, classify_system_theme, Brightness, UNKNOWN_THEME};
use crate::terminal::TerminalBackend;
use crate::ui::tab::SessionTab;

#[derive(Default)]
struct Appearance {
    /// The installed `default_color_scheme` row.
    default_scheme: Option<String>,
    /// The windows' tab notebooks.
    notebooks: Vec<glib::WeakRef<gtk4::Notebook>>,
    /// The theme watch is connected (once per process).
    watching: bool,
}

thread_local! {
    static APPEARANCE: RefCell<Appearance> = RefCell::default();
}

/// Record the stored `default_color_scheme` and re-apply it to every open
/// terminal.
pub fn install_default_scheme(raw: Option<String>) {
    APPEARANCE.with(|state| state.borrow_mut().default_scheme = raw);
    refresh();
}

/// The desktop theme's brightness; [`UNKNOWN_THEME`] before GTK is up.
pub fn system_brightness() -> Brightness {
    if !gtk4::is_initialized_main_thread() {
        return UNKNOWN_THEME;
    }
    match gtk4::Settings::default() {
        Some(settings) => classify_system_theme(
            settings.is_gtk_application_prefer_dark_theme(),
            settings.gtk_theme_name().as_deref(),
        ),
        None => UNKNOWN_THEME,
    }
}

/// Give `backend` the scheme its session resolves to.
pub fn apply_to(backend: &dyn TerminalBackend, session_override: Option<&str>) {
    apply_with(backend, session_override, system_brightness());
}

fn apply_with(backend: &dyn TerminalBackend, session_override: Option<&str>, system: Brightness) {
    let global = APPEARANCE.with(|state| state.borrow().default_scheme.clone());
    let scheme = color_scheme::resolve(session_override, global.as_deref(), system);
    backend.set_color_scheme(scheme.name);
}

/// Re-apply on theme changes to the terminals in `notebook`, starting the
/// theme watch on first use.
pub fn track(notebook: &gtk4::Notebook) {
    let start_watching = APPEARANCE.with(|state| {
        let mut state = state.borrow_mut();
        state.notebooks.retain(|weak| weak.upgrade().is_some());
        state.notebooks.push(notebook.downgrade());
        !std::mem::replace(&mut state.watching, true)
    });
    if start_watching {
        if let Some(settings) = gtk4::Settings::default() {
            settings.connect_gtk_application_prefer_dark_theme_notify(|_| refresh());
            settings.connect_gtk_theme_name_notify(|_| refresh());
        }
    }
}

/// Re-resolve and push the scheme to every terminal in the tracked
/// notebooks.
pub fn refresh() {
    let notebooks: Vec<gtk4::Notebook> = APPEARANCE.with(|state| {
        state
            .borrow()
            .notebooks
            .iter()
            .filter_map(glib::WeakRef::upgrade)
            .collect()
    });
    if notebooks.is_empty() {
        return;
    }
    let system = system_brightness();
    for notebook in notebooks {
        for page_num in 0..notebook.n_pages() {
            let Some(page) = notebook.nth_page(Some(page_num)) else {
                continue;
            };
            if let Some((terminal, scheme)) = SessionTab::terminal_and_scheme(&page) {
                apply_with(&*terminal, scheme.as_deref(), system);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::fake::Interaction;
    use crate::terminal::FakeBackend;

    fn pushed(backend: &FakeBackend) -> Vec<String> {
        backend
            .interactions()
            .into_iter()
            .filter_map(|call| match call {
                Interaction::SetColorScheme(name) => Some(name),
                _ => None,
            })
            .collect()
    }

    /// The installed global row, the session override and the system theme
    /// decide the palette name the backend gets.
    #[test]
    fn apply_pushes_the_resolved_palette_name() {
        let backend = FakeBackend::new();
        install_default_scheme(Some("System".into()));
        apply_with(&backend, None, Brightness::Light);
        apply_with(&backend, None, Brightness::Dark);
        apply_with(&backend, Some("Light"), Brightness::Dark);
        install_default_scheme(Some("Light".into()));
        apply_with(&backend, None, Brightness::Dark);
        install_default_scheme(None);
        apply_with(&backend, Some(""), Brightness::Light);
        assert_eq!(
            pushed(&backend),
            ["Light", "Dark", "Light", "Light", "Light"]
        );
    }

    /// Without GTK, the theme is the defined default (no panic).
    #[test]
    fn system_brightness_before_gtk_is_the_default() {
        if !gtk4::is_initialized() {
            assert_eq!(system_brightness(), UNKNOWN_THEME);
        }
    }
}
