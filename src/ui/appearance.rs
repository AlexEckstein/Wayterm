//! The app follows the desktop's light or dark theme (doc 02 §Color
//! schemes, doc 13 §Window theme): the window chrome and every live terminal.
//!
//! Terminals get their scheme ([`color_scheme::resolve`]) as they are
//! created ([`apply_to`]); a change of a global scheme row
//! ([`install_schemes`], from the settings page) or of the desktop theme
//! re-resolves every terminal in the tracked notebooks ([`track`]).
//!
//! Font, opacity and scrollback go the same way ([`apply_to`],
//! [`install_terminal_defaults`]), without following the desktop theme. So
//! do each backend's options, through its own install (doc 02 §View
//! options).
//!
//! The desktop theme is xdg-desktop-portal's `color-scheme`
//! ([`color_scheme::portal_color_scheme`]), read at start and followed
//! through `SettingChanged`. Without a portal answer it falls back to
//! GTK's own settings (`gtk_preference`), weighed with `gtk-theme-name`
//! ([`classify_system_theme`]). Those don't track a live switch on KDE:
//! the theme name stays `Breeze`, and the dark flag comes from
//! `settings.ini`, which GTK reads once.
//!
//! The chrome: the portal's preference picks GTK's theme variant
//! (`apply_chrome`), and `UserCss` reloads the user's
//! `gtk-4.0/gtk.css` when it changes — KDE recolors GTK apps by rewriting
//! the `colors.css` it imports, which GTK also reads only at startup. Main
//! thread only.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::store::model::SessionConfig;
use crate::terminal::color_scheme::{
    self, backend_row, classify_system_theme, Brightness, SchemeChoice,
    DEFAULT_COLOR_SCHEME_SETTING, GHOSTTY_COLOR_SCHEME_SETTING, UNKNOWN_THEME,
    VTE_COLOR_SCHEME_SETTING,
};
use crate::terminal::{TerminalBackend, TerminalDefaults};
use crate::ui::tabs::SessionTab;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_SETTINGS: &str = "org.freedesktop.portal.Settings";
const APPEARANCE_NAMESPACE: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";
const PORTAL_TIMEOUT_MS: i32 = 2_000;

/// GTK 4.20+'s setting, and its `GtkInterfaceColorScheme` values; read and
/// written by name, since the build targets older GTK too.
const INTERFACE_COLOR_SCHEME: &str = "gtk-interface-color-scheme";
const INTERFACE_DARK: i32 = 2;
const INTERFACE_LIGHT: i32 = 3;
const PREFER_DARK: &str = "gtk-application-prefer-dark-theme";

/// How long the user stylesheet's directory must be quiet before a
/// reload: a theme switch rewrites several files.
const USER_CSS_SETTLE: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Appearance {
    /// The installed `default_color_scheme` row.
    default_scheme: Option<String>,
    /// The installed rows of each backend's own scheme.
    vte_scheme: Option<String>,
    ghostty_scheme: Option<String>,
    /// The installed font, opacity and scrollback rows.
    terminal_defaults: TerminalDefaults,
    /// The windows' tab notebooks.
    notebooks: Vec<glib::WeakRef<gtk4::Notebook>>,
    /// The portal's light/dark preference, when it states one.
    desktop: Option<Brightness>,
    /// The theme watches are connected (once per process).
    watching: bool,
    /// The portal's `SettingChanged` subscription.
    portal_watch: Option<gio::SignalSubscription>,
    /// The user stylesheet, reloaded on change.
    user_css: Option<UserCss>,
}

thread_local! {
    static APPEARANCE: RefCell<Appearance> = RefCell::default();
}

impl Appearance {
    /// The global scheme row in force for the backend called `backend`.
    fn scheme_row(&self, backend: &str) -> Option<String> {
        let own = match backend {
            "vte" => self.vte_scheme.as_deref(),
            "ghostty" => self.ghostty_scheme.as_deref(),
            _ => None,
        };
        backend_row(own, self.default_scheme.as_deref()).map(str::to_string)
    }
}

/// Record the stored scheme rows and re-apply them to every open terminal.
pub fn install_schemes(default: Option<String>, vte: Option<String>, ghostty: Option<String>) {
    for (key, raw) in [
        (DEFAULT_COLOR_SCHEME_SETTING, &default),
        (VTE_COLOR_SCHEME_SETTING, &vte),
        (GHOSTTY_COLOR_SCHEME_SETTING, &ghostty),
    ] {
        if let Some(raw) = raw.as_deref().filter(|raw| unknown_scheme(raw)) {
            tracing::warn!(key = %key, value = %raw, "unknown color scheme setting; ignoring it");
        }
    }
    APPEARANCE.with(|state| {
        let mut state = state.borrow_mut();
        state.default_scheme = default;
        state.vte_scheme = vte;
        state.ghostty_scheme = ghostty;
    });
    refresh();
}

/// A stored scheme that is neither blank nor a known choice: resolution
/// passes over it.
fn unknown_scheme(raw: &str) -> bool {
    !raw.trim().is_empty() && SchemeChoice::from_setting(raw).is_none()
}

/// [`install_schemes`] with no backend's own row.
#[cfg(test)]
pub(crate) fn install_default_scheme(raw: Option<String>) {
    install_schemes(raw, None, None);
}

/// Record the stored font, opacity, scrollback and backend rows, and hand
/// what changed to the open terminals that take it.
pub fn install_terminal_defaults(defaults: TerminalDefaults) {
    let previous = APPEARANCE.with(|state| {
        std::mem::replace(&mut state.borrow_mut().terminal_defaults, defaults.clone())
    });
    if previous.vte != defaults.vte {
        crate::terminal::vte::install_options(&defaults.vte);
    }
    #[cfg(feature = "backend-ghostty")]
    if previous.ghostty != defaults.ghostty {
        crate::terminal::ghostty::install_options(&defaults.ghostty);
    }
    if !previous.same_shared_rows(&defaults) {
        for_each_terminal(apply_settings);
    }
    // After the options: a scheme takes its colors from them.
    if previous.vte.colors != defaults.vte.colors
        || previous.ghostty.colors != defaults.ghostty.colors
    {
        refresh();
    }
}

/// The desktop theme's brightness ([`classify_system_theme`]): the
/// portal's preference, else GTK's (`gtk_preference`), with the GTK
/// theme name. Before GTK is up, the portal's preference or
/// [`UNKNOWN_THEME`].
pub fn system_brightness() -> Brightness {
    let desktop = APPEARANCE.with(|state| state.borrow().desktop);
    let settings = gtk4::is_initialized_main_thread()
        .then(gtk4::Settings::default)
        .flatten();
    let Some(settings) = settings else {
        return desktop.unwrap_or(UNKNOWN_THEME);
    };
    classify_system_theme(
        desktop.or_else(|| gtk_preference(&settings)),
        settings.gtk_theme_name().as_deref(),
    )
}

/// The preference GTK's own settings state: `gtk-interface-color-scheme`
/// (GTK 4.20+, kept current from the desktop) when it says dark or light,
/// else a set `gtk-application-prefer-dark-theme`.
fn gtk_preference(settings: &gtk4::Settings) -> Option<Brightness> {
    match interface_color_scheme(settings) {
        Some(INTERFACE_DARK) => Some(Brightness::Dark),
        Some(INTERFACE_LIGHT) => Some(Brightness::Light),
        _ => settings
            .is_gtk_application_prefer_dark_theme()
            .then_some(Brightness::Dark),
    }
}

/// `gtk-interface-color-scheme` as its number; `None` before GTK 4.20.
fn interface_color_scheme(settings: &gtk4::Settings) -> Option<i32> {
    settings.find_property(INTERFACE_COLOR_SCHEME)?;
    settings
        .property_value(INTERFACE_COLOR_SCHEME)
        .transform::<i32>()
        .ok()?
        .get()
        .ok()
}

/// The light/dark preference the chrome is drawn with, as GTK's settings
/// hold it.
#[cfg(test)]
pub(in crate::ui) fn chrome_preference() -> Option<Brightness> {
    gtk_preference(&gtk4::Settings::default()?)
}

/// Give `backend` the scheme, font, opacity and scrollback `session`
/// resolves to.
pub fn apply_to(backend: &dyn TerminalBackend, session: &SessionConfig) {
    if let Some(raw) = session
        .color_scheme
        .as_deref()
        .filter(|raw| unknown_scheme(raw))
    {
        tracing::warn!(
            session = session.id,
            value = %raw,
            "unknown color scheme on the session; using the settings' scheme"
        );
    }
    apply_with(backend, session, system_brightness());
    apply_settings(backend, session);
}

fn apply_with(backend: &dyn TerminalBackend, session: &SessionConfig, system: Brightness) {
    let global = APPEARANCE.with(|state| state.borrow().scheme_row(backend.name()));
    let session_override = session.color_scheme.as_deref();
    let scheme = color_scheme::resolve(session_override, global.as_deref(), system);
    tracing::debug!(
        backend = %backend.name(),
        session = session.id,
        scheme = %scheme.name,
        system = ?system,
        "color scheme applied"
    );
    backend.set_color_scheme(scheme.name);
}

fn apply_settings(backend: &dyn TerminalBackend, session: &SessionConfig) {
    let defaults = APPEARANCE.with(|state| state.borrow().terminal_defaults.clone());
    backend.set_font(&defaults.font(session.font_family.as_deref(), session.font_size));
    backend.set_opacity(defaults.opacity(session.opacity));
    backend.set_scrollback_limit(defaults.scrollback_lines);
}

/// Re-apply on theme changes to the terminals in `notebook`, starting the
/// theme watches on first use.
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
            if settings.find_property(INTERFACE_COLOR_SCHEME).is_some() {
                settings.connect_notify_local(Some(INTERFACE_COLOR_SCHEME), |_, _| refresh());
            }
        }
        // Unit tests drive the preference and the stylesheet directly: the
        // real desktop must not race their assertions.
        if !cfg!(test) {
            watch_portal();
            let user_css = UserCss::follow(&glib::user_config_dir().join("gtk-4.0"));
            APPEARANCE.with(|state| state.borrow_mut().user_css = user_css);
        }
    }
}

/// Record the desktop's preference (`None`: no portal, or no preference),
/// move the chrome's theme variant to it, and re-color open terminals.
pub(in crate::ui) fn set_desktop_preference(preference: Option<Brightness>) {
    let previous =
        APPEARANCE.with(|state| std::mem::replace(&mut state.borrow_mut().desktop, preference));
    if preference != previous {
        let stated = match preference {
            Some(Brightness::Dark) => "dark",
            Some(Brightness::Light) => "light",
            None => "none",
        };
        tracing::info!(preference = %stated, "desktop light/dark preference changed");
        if gtk4::is_initialized_main_thread() {
            if let Some(settings) = gtk4::Settings::default() {
                apply_chrome(&settings, preference);
            }
        }
    }
    refresh();
}

/// Point the chrome's theme variant at `preference`; `None` hands it back
/// to GTK's own settings. GTK 4.20+ picks the variant from
/// `gtk-interface-color-scheme` but reloads the theme only when the theme
/// name or `gtk-application-prefer-dark-theme` notifies (and warns when the
/// latter is set), so resetting the latter reloads it. Older GTK picks the
/// variant from `gtk-application-prefer-dark-theme` alone.
fn apply_chrome(settings: &gtk4::Settings, preference: Option<Brightness>) {
    let scheme = settings.find_property(INTERFACE_COLOR_SCHEME);
    match (preference, scheme) {
        (Some(preference), Some(scheme)) => {
            let number = match preference {
                Brightness::Dark => INTERFACE_DARK,
                Brightness::Light => INTERFACE_LIGHT,
            };
            let value = glib::EnumClass::with_type(scheme.value_type())
                .and_then(|class| class.to_value(number));
            match value {
                Some(value) => settings.set_property_from_value(INTERFACE_COLOR_SCHEME, &value),
                None => tracing::warn!(
                    "cannot set {INTERFACE_COLOR_SCHEME}; the window keeps its theme variant"
                ),
            }
            settings.reset_property(PREFER_DARK);
        }
        (Some(preference), None) => {
            settings.set_gtk_application_prefer_dark_theme(preference == Brightness::Dark);
        }
        (None, scheme) => {
            if scheme.is_some() {
                settings.reset_property(INTERFACE_COLOR_SCHEME);
            }
            settings.reset_property(PREFER_DARK);
        }
    }
}

/// Read the portal's `color-scheme`, then follow its `SettingChanged`.
fn watch_portal() {
    glib::spawn_future_local(async {
        let bus = match gio::bus_get_future(gio::BusType::Session).await {
            Ok(bus) => bus,
            Err(err) => {
                tracing::info!(error = %err, "no session bus; following the GTK theme settings");
                return;
            }
        };
        let subscription = bus.subscribe_to_signal(
            Some(PORTAL),
            Some(PORTAL_SETTINGS),
            Some("SettingChanged"),
            Some(PORTAL_PATH),
            Some(APPEARANCE_NAMESPACE),
            gio::DBusSignalFlags::NONE,
            |signal| {
                if let Some(preference) = changed_preference(signal.parameters) {
                    set_desktop_preference(preference);
                }
            },
        );
        APPEARANCE.with(|state| state.borrow_mut().portal_watch = Some(subscription));

        let read = |method| {
            bus.call_future(
                Some(PORTAL),
                PORTAL_PATH,
                PORTAL_SETTINGS,
                method,
                Some(&(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                PORTAL_TIMEOUT_MS,
            )
        };
        // `ReadOne` is portal version 2; older portals only have `Read`.
        let reply = match read("ReadOne").await {
            Ok(reply) => Ok(reply),
            Err(_) => read("Read").await,
        };
        match reply {
            Ok(reply) => set_desktop_preference(portal_preference(&reply.child_value(0))),
            Err(err) => tracing::info!(
                error = %err,
                "desktop portal unavailable; following the GTK theme settings"
            ),
        }
    });
}

/// A `SettingChanged(namespace, key, value)` for `color-scheme`: the new
/// preference. Other keys are `None`.
fn changed_preference(parameters: &glib::Variant) -> Option<Option<Brightness>> {
    let (namespace, key, value) = parameters.get::<(String, String, glib::Variant)>()?;
    (namespace == APPEARANCE_NAMESPACE && key == COLOR_SCHEME_KEY)
        .then(|| portal_preference(&value))
}

/// The portal's `color-scheme` out of however many variants carry it
/// (`ReadOne` answers `v`, the older `Read` a `v` inside a `v`).
fn portal_preference(value: &glib::Variant) -> Option<Brightness> {
    let mut value = value.clone();
    while value.type_() == glib::VariantTy::VARIANT {
        value = value.as_variant()?;
    }
    color_scheme::portal_color_scheme(value.get()?)
}

/// Re-resolve and push the scheme to every terminal in the tracked
/// notebooks.
pub fn refresh() {
    let system = system_brightness();
    tracing::debug!(system = ?system, "re-applying color schemes to open terminals");
    for_each_terminal(|terminal, session| apply_with(terminal, session, system));
}

/// Every terminal in the tracked notebooks, with its session.
fn for_each_terminal(apply: impl Fn(&dyn TerminalBackend, &SessionConfig)) {
    let notebooks: Vec<gtk4::Notebook> = APPEARANCE.with(|state| {
        state
            .borrow()
            .notebooks
            .iter()
            .filter_map(glib::WeakRef::upgrade)
            .collect()
    });
    for notebook in notebooks {
        for page_num in 0..notebook.n_pages() {
            let Some(page) = notebook.nth_page(Some(page_num)) else {
                continue;
            };
            if let Some((terminal, session)) = SessionTab::terminal_and_session(&page) {
                apply(&*terminal, &session);
            }
        }
    }
}

/// A user `gtk.css` and what it imports from its directory, reloaded when
/// a `.css` file there changes. GTK loads that file once at startup; this
/// provider, added after GTK's at the same user priority, overrides the
/// stale copy. It is added on the first change only, so an unchanged file
/// is not applied twice, and removed on drop.
pub(in crate::ui) struct UserCss {
    /// Owned here; the monitor holds it weakly.
    _stylesheet: Rc<Stylesheet>,
    _monitor: gio::FileMonitor,
}

struct Stylesheet {
    path: PathBuf,
    provider: gtk4::CssProvider,
    display: gdk::Display,
    installed: Cell<bool>,
    pending: Cell<bool>,
}

impl UserCss {
    /// Follow `dir/gtk.css`; `None` without a display or a directory watch.
    pub(in crate::ui) fn follow(dir: &Path) -> Option<Self> {
        let display = gdk::Display::default()?;
        let monitor = gio::File::for_path(dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
            .map_err(|err| {
                tracing::warn!(
                    path = %dir.display(),
                    error = %err,
                    "not watching the directory for theme changes"
                );
            })
            .ok()?;
        let stylesheet = Rc::new(Stylesheet {
            path: dir.join("gtk.css"),
            provider: gtk4::CssProvider::new(),
            display,
            installed: Cell::new(false),
            pending: Cell::new(false),
        });
        let weak = Rc::downgrade(&stylesheet);
        monitor.connect_changed(move |_, file, other, _| {
            let is_css = |file: &gio::File| {
                file.path()
                    .is_some_and(|path| path.extension().is_some_and(|ext| ext == "css"))
            };
            if !is_css(file) && !other.is_some_and(is_css) {
                return;
            }
            if let Some(stylesheet) = weak.upgrade() {
                stylesheet.schedule();
            }
        });
        Some(Self {
            _stylesheet: stylesheet,
            _monitor: monitor,
        })
    }
}

impl Stylesheet {
    fn schedule(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(USER_CSS_SETTLE, move || {
            if let Some(stylesheet) = weak.upgrade() {
                stylesheet.pending.set(false);
                stylesheet.load();
            }
        });
    }

    fn load(&self) {
        tracing::debug!(path = %self.path.display(), "reloading the user stylesheet");
        if self.path.exists() {
            self.provider.load_from_path(&self.path);
        } else {
            self.provider.load_from_string("");
        }
        if !self.installed.replace(true) {
            gtk4::style_context_add_provider_for_display(
                &self.display,
                &self.provider,
                gtk4::STYLE_PROVIDER_PRIORITY_USER,
            );
        }
    }
}

impl Drop for Stylesheet {
    fn drop(&mut self) {
        if self.installed.get() {
            gtk4::style_context_remove_provider_for_display(&self.display, &self.provider);
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

    /// Global rows reach a new terminal, under a session's overrides.
    #[test]
    fn apply_pushes_the_resolved_font_opacity_and_scrollback() {
        install_terminal_defaults(TerminalDefaults {
            font_family: "Hack".into(),
            font_size: 10.0,
            opacity: 0.8,
            scrollback_lines: 500,
            ..Default::default()
        });
        let settings = |session: &SessionConfig| {
            let backend = FakeBackend::new();
            apply_to(&backend, session);
            backend
                .interactions()
                .into_iter()
                .filter(|call| !matches!(call, Interaction::SetColorScheme(_)))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            settings(&SessionConfig::default()),
            [
                Interaction::SetFont("Hack 10".into()),
                Interaction::SetOpacity(0.8),
                Interaction::SetScrollbackLimit(500),
            ]
        );
        let session = SessionConfig {
            font_family: Some("Fira Code".into()),
            font_size: Some(14.0),
            opacity: Some(1.0),
            ..Default::default()
        };
        assert_eq!(
            settings(&session),
            [
                Interaction::SetFont("Fira Code 14".into()),
                Interaction::SetOpacity(1.0),
                Interaction::SetScrollbackLimit(500),
            ]
        );
        install_terminal_defaults(TerminalDefaults::default());
    }

    /// The installed global row, the session override and the system theme
    /// decide the palette name the backend gets.
    #[test]
    fn apply_pushes_the_resolved_palette_name() {
        let backend = FakeBackend::new();
        let session = |scheme: Option<&str>| SessionConfig {
            color_scheme: scheme.map(str::to_string),
            ..Default::default()
        };
        install_default_scheme(Some("System".into()));
        apply_with(&backend, &session(None), Brightness::Light);
        apply_with(&backend, &session(None), Brightness::Dark);
        apply_with(&backend, &session(Some("Light")), Brightness::Dark);
        install_default_scheme(Some("Light".into()));
        apply_with(&backend, &session(None), Brightness::Dark);
        install_default_scheme(None);
        apply_with(&backend, &session(Some("")), Brightness::Light);
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

    /// The desktop's preference decides the system theme until it is
    /// withdrawn.
    #[test]
    fn the_desktop_preference_wins_until_withdrawn() {
        if gtk4::is_initialized() {
            return;
        }
        set_desktop_preference(Some(Brightness::Light));
        assert_eq!(system_brightness(), Brightness::Light);
        set_desktop_preference(Some(Brightness::Dark));
        assert_eq!(system_brightness(), Brightness::Dark);
        set_desktop_preference(None);
        assert_eq!(system_brightness(), UNKNOWN_THEME);
    }

    fn lines_with<'a>(output: &'a str, message: &str) -> Vec<&'a str> {
        output
            .lines()
            .filter(|line| line.contains(message))
            .collect()
    }

    /// A scheme row naming no scheme is a warning with its key; a blank or
    /// known one is not.
    #[test]
    fn an_unknown_scheme_row_is_logged_with_its_key() {
        let (buffer, guard) = crate::logging::tests::capture(None);
        install_schemes(
            Some("System".into()),
            Some("Solarized".into()),
            Some(" ".into()),
        );
        drop(guard);
        install_schemes(None, None, None);
        let output = buffer.contents();

        let lines = lines_with(&output, "unknown color scheme setting");
        assert_eq!(lines.len(), 1, "{output}");
        assert!(lines[0].contains("WARN"), "{}", lines[0]);
        assert!(lines[0].contains("key=vte.color_scheme"), "{}", lines[0]);
        assert!(lines[0].contains("value=Solarized"), "{}", lines[0]);
    }

    /// A session override naming no scheme is a warning with the session's
    /// id when its terminal is set up, and the debug line names the scheme
    /// it got instead.
    #[test]
    fn an_unknown_session_scheme_is_logged_with_the_session() {
        let (buffer, guard) = crate::logging::tests::capture(Some("debug"));
        install_default_scheme(Some("Light".into()));
        let session = SessionConfig {
            id: 42,
            color_scheme: Some("Nord".into()),
            ..Default::default()
        };
        apply_to(&FakeBackend::new(), &session);
        apply_to(&FakeBackend::new(), &SessionConfig::default());
        drop(guard);
        install_default_scheme(None);
        let output = buffer.contents();

        let warned = lines_with(&output, "unknown color scheme on the session");
        assert_eq!(warned.len(), 1, "{output}");
        assert!(warned[0].contains("WARN"), "{}", warned[0]);
        assert!(warned[0].contains("session=42"), "{}", warned[0]);
        assert!(warned[0].contains("value=Nord"), "{}", warned[0]);
        let applied = lines_with(&output, "color scheme applied");
        assert_eq!(applied.len(), 2, "{output}");
        assert!(applied[0].contains("DEBUG"), "{}", applied[0]);
        assert!(applied[0].contains("session=42"), "{}", applied[0]);
        assert!(applied[0].contains("scheme=Light"), "{}", applied[0]);
    }

    /// A change of the desktop's preference is one info line; restating
    /// it is none.
    #[test]
    fn a_desktop_preference_change_is_logged_once() {
        if gtk4::is_initialized() {
            return;
        }
        let (buffer, guard) = crate::logging::tests::capture(None);
        set_desktop_preference(Some(Brightness::Dark));
        set_desktop_preference(Some(Brightness::Dark));
        set_desktop_preference(None);
        drop(guard);
        let output = buffer.contents();

        let lines = lines_with(&output, "desktop light/dark preference changed");
        assert_eq!(lines.len(), 2, "{output}");
        assert!(lines[0].contains("INFO"), "{}", lines[0]);
        assert!(lines[0].contains("preference=dark"), "{}", lines[0]);
        assert!(lines[1].contains("preference=none"), "{}", lines[1]);
    }

    /// `ReadOne`'s `v`, `Read`'s `v` in `v`, and a bare value all carry
    /// the preference; other types carry none.
    #[test]
    fn portal_replies_unwrap_to_a_preference() {
        let wrap = |value: glib::Variant| glib::Variant::from_variant(&value);
        assert_eq!(
            portal_preference(&1u32.to_variant()),
            Some(Brightness::Dark)
        );
        assert_eq!(
            portal_preference(&wrap(2u32.to_variant())),
            Some(Brightness::Light)
        );
        assert_eq!(
            portal_preference(&wrap(wrap(1u32.to_variant()))),
            Some(Brightness::Dark)
        );
        assert_eq!(portal_preference(&wrap(0u32.to_variant())), None);
        assert_eq!(portal_preference(&"dark".to_variant()), None);
    }

    /// Each backend resolves through its own row, then the shared one; a
    /// backend with no row of its own, as in tests, takes the shared one.
    #[test]
    fn each_backend_has_its_own_scheme_row() {
        let state = Appearance {
            default_scheme: Some("Dark".into()),
            vte_scheme: Some("Light".into()),
            ghostty_scheme: Some("Solarized".into()),
            ..Default::default()
        };
        assert_eq!(state.scheme_row("vte").as_deref(), Some("Light"));
        assert_eq!(state.scheme_row("ghostty").as_deref(), Some("Dark"));
        assert_eq!(state.scheme_row("fake").as_deref(), Some("Dark"));
    }

    /// Only the appearance namespace's `color-scheme` key changes the
    /// preference.
    #[test]
    fn setting_changed_reads_only_the_color_scheme() {
        let signal = |namespace: &str, key: &str, value: u32| {
            (namespace, key, value.to_variant()).to_variant()
        };
        assert_eq!(
            changed_preference(&signal(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY, 1)),
            Some(Some(Brightness::Dark))
        );
        assert_eq!(
            changed_preference(&signal(APPEARANCE_NAMESPACE, COLOR_SCHEME_KEY, 0)),
            Some(None)
        );
        assert_eq!(
            changed_preference(&signal(APPEARANCE_NAMESPACE, "accent-color", 1)),
            None
        );
        assert_eq!(
            changed_preference(&signal("org.gnome.desktop.interface", COLOR_SCHEME_KEY, 1)),
            None
        );
        assert_eq!(changed_preference(&"x".to_variant()), None);
    }
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The desktop's preference re-draws the window chrome in the theme's
    /// dark or light variant, and withdrawing it hands back to GTK.
    pub(in crate::ui) fn the_chrome_follows_the_desktop_preference() {
        let settings = gtk4::Settings::default().expect("GTK settings");
        let theme_before = settings.gtk_theme_name();
        // GTK's built-in theme has both variants on every GTK 4.
        settings.set_gtk_theme_name(Some("Default"));
        let window = gtk4::Window::new();
        window.present();
        // A fresh label per check: its style is computed from the current
        // theme once the window lays it out.
        let text_is_light = || {
            let label = gtk4::Label::new(Some("chrome"));
            window.set_child(Some(&label));
            let context = glib::MainContext::default();
            for _ in 0..20 {
                context.iteration(false);
                std::thread::sleep(Duration::from_millis(5));
            }
            label.color().red() > 0.5
        };

        set_desktop_preference(Some(Brightness::Dark));
        assert!(text_is_light(), "dark variant: light text");
        set_desktop_preference(Some(Brightness::Light));
        assert!(!text_is_light(), "light variant: dark text");
        set_desktop_preference(Some(Brightness::Dark));
        assert!(text_is_light(), "and back");

        set_desktop_preference(None);
        assert_eq!(system_brightness(), {
            let preference = gtk_preference(&settings);
            classify_system_theme(preference, Some("Default"))
        });
        settings.set_gtk_theme_name(theme_before.as_deref());
        window.destroy();
    }

    /// Rewriting the imported `colors.css`, as KDE does on a light/dark
    /// switch, re-styles open widgets; dropping the watch takes the
    /// stylesheet away again.
    pub(in crate::ui) fn user_css_reloads_when_its_files_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let colors = dir.path().join("colors.css");
        std::fs::write(dir.path().join("gtk.css"), "@import 'colors.css';").unwrap();
        std::fs::write(&colors, ".wayterm-probe { color: #102030; }").unwrap();

        let window = gtk4::Window::new();
        let probe = gtk4::Label::new(Some("probe"));
        probe.add_css_class("wayterm-probe");
        window.set_child(Some(&probe));
        // Styles only re-validate on a mapped window's frame clock.
        window.present();
        let color = || {
            let color = probe.color();
            let channel = |c: f32| (c * 255.0).round() as u8;
            (
                channel(color.red()),
                channel(color.green()),
                channel(color.blue()),
            )
        };
        let initial = color();

        let user_css = UserCss::follow(dir.path()).expect("directory watch");
        assert_eq!(color(), initial, "not applied before a change");
        std::fs::write(&colors, ".wayterm-probe { color: #102030; }\n").unwrap();
        wait_for("first reload", || color() == (0x10, 0x20, 0x30));

        // An atomic replace (write, then rename over) is a change too.
        let staged = dir.path().join("colors.css.new");
        std::fs::write(&staged, ".wayterm-probe { color: #c0d0e0; }").unwrap();
        std::fs::rename(&staged, &colors).unwrap();
        wait_for("rename reload", || color() == (0xc0, 0xd0, 0xe0));

        drop(user_css);
        wait_for("provider removed", || color() == initial);
        window.destroy();
    }
}
