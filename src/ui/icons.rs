//! Session-tree icons (doc 06): every icon is an icon name, drawn through
//! the GTK icon theme. Wayterm ships its own symbolic set (servers, racks,
//! databases, UPS, network gear, …) compiled into the binary as a
//! GResource (`assets/tree-icons`, `build.rs`), so the defaults and the
//! picker look the same on every desktop whatever icon theme is installed.
//! Custom icons are SVGs copied into `$XDG_DATA_HOME/wayterm/icons/`,
//! which is on the theme's search path, so they are just more names
//! (`wayterm-<file stem>`). `-symbolic` SVGs, the built-in ones included,
//! are recolored to match the theme.
//!
//! Everything above the GTK layer is display-free.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};

use crate::store::model::SessionType;
use crate::ui::widgets::window_of;

/// The default folder icon.
pub const FOLDER_ICON: &str = "wayterm-folder-symbolic";

/// Prefix of every built-in and imported icon's name, so none shadows a
/// theme icon.
pub const CUSTOM_PREFIX: &str = "wayterm-";

/// Imported SVGs larger than this are refused (icons are a few KiB).
pub const MAX_SVG_BYTES: u64 = 1024 * 1024;

/// Where the built-in icons live in the GResource bundle; laid out as a
/// hicolor `scalable/actions` directory, as `IconTheme` requires.
const BUILTIN_RESOURCE_PATH: &str = "/com/alex-eckstein/wayterm/icons";

/// The built-in icons, in picker order: infrastructure, network, compute,
/// devices, organizing. One SVG each in `assets/tree-icons` (a test keeps
/// the two in step).
pub const BUILTIN: &[&str] = &[
    "wayterm-server-symbolic",
    "wayterm-server-rack-symbolic",
    "wayterm-database-symbolic",
    "wayterm-nas-symbolic",
    "wayterm-harddisk-symbolic",
    "wayterm-ups-symbolic",
    "wayterm-power-symbolic",
    "wayterm-cpu-symbolic",
    "wayterm-sbc-symbolic",
    "wayterm-switch-symbolic",
    "wayterm-router-symbolic",
    "wayterm-firewall-symbolic",
    "wayterm-wifi-symbolic",
    "wayterm-vpn-symbolic",
    "wayterm-web-symbolic",
    "wayterm-cloud-symbolic",
    "wayterm-backup-symbolic",
    "wayterm-vm-symbolic",
    "wayterm-container-symbolic",
    "wayterm-cluster-symbolic",
    "wayterm-terminal-symbolic",
    "wayterm-code-symbolic",
    "wayterm-gear-symbolic",
    "wayterm-monitoring-symbolic",
    "wayterm-key-symbolic",
    "wayterm-desktop-symbolic",
    "wayterm-remote-desktop-symbolic",
    "wayterm-laptop-symbolic",
    "wayterm-serial-symbolic",
    "wayterm-printer-symbolic",
    "wayterm-camera-symbolic",
    "wayterm-media-symbolic",
    "wayterm-mail-symbolic",
    "wayterm-home-symbolic",
    "wayterm-lightbulb-symbolic",
    "wayterm-sensor-symbolic",
    "wayterm-folder-symbolic",
    "wayterm-star-symbolic",
];

/// The icon a session of `session_type` has when it sets none.
pub fn session_type_icon(session_type: &SessionType) -> &'static str {
    match session_type {
        SessionType::SSH => "wayterm-server-symbolic",
        SessionType::RDP => "wayterm-desktop-symbolic",
        SessionType::VNC => "wayterm-remote-desktop-symbolic",
        SessionType::Serial => "wayterm-serial-symbolic",
        SessionType::LocalShell => "wayterm-terminal-symbolic",
    }
}

/// A readable label for an icon name: `wayterm-server-rack-symbolic` →
/// `server rack`.
pub fn icon_label(name: &str) -> String {
    let name = name.strip_prefix(CUSTOM_PREFIX).unwrap_or(name);
    let name = name.strip_suffix("-symbolic").unwrap_or(name);
    name.replace('-', " ")
}

/// The name to draw: `custom` when set and `available`, else `default` —
/// a deleted SVG or an icon missing from this theme never draws as a
/// broken image.
pub fn resolve_icon<'a>(
    custom: Option<&'a str>,
    default: &'a str,
    available: impl Fn(&str) -> bool,
) -> &'a str {
    custom
        .map(str::trim)
        .filter(|name| !name.is_empty() && available(name))
        .unwrap_or(default)
}

/// `$XDG_DATA_HOME/wayterm/icons`.
pub fn custom_icon_dir() -> PathBuf {
    crate::app::data_dir().join("icons")
}

/// The icon name an SVG at `path` is imported as: `wayterm-` plus the
/// file stem, lowercased, every run of other characters turned into one
/// `-`. A trailing `-symbolic` survives, so recoloring still applies.
pub fn icon_name_for_file(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut name = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_end_matches('-');
    let name = name.strip_prefix(CUSTOM_PREFIX).unwrap_or(name);
    if name.is_empty() || name == "symbolic" {
        format!("{CUSTOM_PREFIX}icon")
    } else {
        format!("{CUSTOM_PREFIX}{name}")
    }
}

/// `name` with a `-<n>` counter, kept before a `-symbolic` suffix.
fn numbered(name: &str, n: u32) -> String {
    match name.strip_suffix("-symbolic") {
        Some(base) => format!("{base}-{n}-symbolic"),
        None => format!("{name}-{n}"),
    }
}

/// Copy the SVG at `source` into `dir` and return its icon name. Only
/// `.svg` files that look like SVG and are at most [`MAX_SVG_BYTES`] are
/// taken. Importing the same file again reuses its name; a different file
/// with the same name, or one named like a built-in icon, gets a counter,
/// so existing icons never change.
pub fn import_svg(source: &Path, dir: &Path) -> io::Result<String> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidData, message);
    let is_svg = source
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"));
    if !is_svg {
        return Err(invalid("only SVG files (.svg) can be icons"));
    }
    if std::fs::metadata(source)?.len() > MAX_SVG_BYTES {
        return Err(invalid("the SVG is larger than 1 MiB"));
    }
    let bytes = std::fs::read(source)?;
    if !String::from_utf8_lossy(&bytes).contains("<svg") {
        return Err(invalid("the file is not an SVG image"));
    }
    std::fs::create_dir_all(dir)?;
    let base = icon_name_for_file(source);
    let mut name = base.clone();
    for n in 2.. {
        if BUILTIN.contains(&name.as_str()) {
            name = numbered(&base, n);
            continue;
        }
        let target = dir.join(format!("{name}.svg"));
        match std::fs::read(&target) {
            Ok(existing) if existing == bytes => return Ok(name),
            Ok(_) => name = numbered(&base, n),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                std::fs::write(&target, &bytes)?;
                return Ok(name);
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("the counter is unbounded")
}

/// Names of the SVGs in `dir`, sorted; empty when it does not exist.
pub fn custom_icons(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let is_svg = path.extension()?.eq_ignore_ascii_case("svg");
            is_svg.then(|| path.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

/// Put the built-in icons and the custom icon directory on `display`'s
/// icon theme. Called once at startup.
pub fn register_icons(display: &gdk::Display) {
    register_builtin_icons(display);
    let dir = custom_icon_dir();
    // Best effort: a missing directory only means no custom icons yet.
    let _ = std::fs::create_dir_all(&dir);
    gtk4::IconTheme::for_display(display).add_search_path(dir);
}

/// Register the embedded GResource bundle (once per process) and put its
/// icons on `display`'s theme.
fn register_builtin_icons(display: &gdk::Display) {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| {
        if let Err(err) = gio::resources_register_include!("tree-icons.gresource") {
            eprintln!("Wayterm: failed to load the built-in icons: {err}");
        }
    });
    gtk4::IconTheme::for_display(display).add_resource_path(BUILTIN_RESOURCE_PATH);
}

/// Make the theme re-read its search path, so an icon imported a moment
/// ago resolves at once.
fn rescan(display: &gdk::Display) {
    let theme = gtk4::IconTheme::for_display(display);
    let paths = theme.search_path();
    let paths: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    theme.set_search_path(&paths);
}

/// An icon chooser: a preview, the icon name (empty = the default), a grid
/// of built-in and imported icons, "Import SVG…", and a reset button.
#[derive(Clone)]
pub struct IconPicker {
    root: gtk4::Box,
    entry: gtk4::Entry,
    preview: gtk4::Image,
    error: gtk4::Label,
    default: Rc<Cell<&'static str>>,
}

impl IconPicker {
    pub fn new(default: &'static str) -> Self {
        let preview = gtk4::Image::builder().pixel_size(24).build();
        let entry = gtk4::Entry::builder()
            .hexpand(true)
            .activates_default(true)
            .build();
        let grid = gtk4::MenuButton::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text("Choose an icon")
            .build();
        let import = gtk4::Button::with_label("Import SVG…");
        let reset = gtk4::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text("Use the default icon")
            .build();
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        row.append(&preview);
        row.append(&entry);
        row.append(&grid);
        row.append(&import);
        row.append(&reset);
        let error = gtk4::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["error"])
            .build();
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        root.append(&row);
        root.append(&error);

        let picker = Self {
            root,
            entry,
            preview,
            error,
            default: Rc::new(Cell::new(default)),
        };
        picker.set_default(default);

        let for_entry = picker.clone();
        picker
            .entry
            .connect_changed(move |_| for_entry.update_preview());
        let for_reset = picker.clone();
        reset.connect_clicked(move |_| for_reset.set_icon(None));
        let for_import = picker.clone();
        import.connect_clicked(move |_| for_import.choose_svg());
        let for_grid = picker.clone();
        grid.set_create_popup_func(move |button| {
            button.set_popover(Some(&for_grid.grid_popover()));
        });
        picker
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.root
    }

    /// The chosen icon; `None` = the default.
    pub fn icon(&self) -> Option<String> {
        Some(self.entry.text().trim().to_string()).filter(|name| !name.is_empty())
    }

    pub fn set_icon(&self, icon: Option<&str>) {
        self.entry.set_text(icon.unwrap_or_default());
    }

    /// The icon shown while none is chosen (a session's follows its type).
    pub fn set_default(&self, default: &'static str) {
        self.default.set(default);
        self.entry
            .set_placeholder_text(Some(&format!("Default ({})", icon_label(default))));
        self.update_preview();
    }

    /// The icon name the preview draws.
    #[cfg(test)]
    pub(crate) fn shown_icon(&self) -> Option<String> {
        self.preview.icon_name().map(Into::into)
    }

    fn update_preview(&self) {
        let icon = self.icon();
        self.preview
            .set_icon_name(Some(icon.as_deref().unwrap_or(self.default.get())));
        self.error.set_visible(false);
    }

    fn show_error(&self, message: &str) {
        self.error.set_text(message);
        self.error.set_visible(true);
    }

    /// The built-in icons, then every imported SVG under its own heading —
    /// rebuilt on each open so a fresh import shows up.
    fn grid_popover(&self) -> gtk4::Popover {
        let popover = gtk4::Popover::new();
        let sections = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        let builtin: Vec<String> = BUILTIN.iter().map(|name| name.to_string()).collect();
        let imported = custom_icons(&custom_icon_dir());
        for (title, names) in [("Built-in", builtin), ("Imported", imported)] {
            if names.is_empty() {
                continue;
            }
            let heading = gtk4::Label::builder()
                .label(title)
                .xalign(0.0)
                .css_classes(["heading"])
                .build();
            sections.append(&heading);
            sections.append(&self.icon_grid(&names, &popover));
        }
        let scroll = gtk4::ScrolledWindow::builder()
            .child(&sections)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(360)
            .build();
        popover.set_child(Some(&scroll));
        popover
    }

    /// One button per icon; a click picks it and closes `popover`.
    fn icon_grid(&self, names: &[String], popover: &gtk4::Popover) -> gtk4::FlowBox {
        let flow = gtk4::FlowBox::builder()
            .selection_mode(gtk4::SelectionMode::None)
            .max_children_per_line(8)
            .min_children_per_line(8)
            .homogeneous(true)
            .build();
        for name in names {
            let image = gtk4::Image::builder()
                .icon_name(name)
                .pixel_size(24)
                .build();
            let button = gtk4::Button::builder()
                .child(&image)
                .tooltip_text(icon_label(name))
                .has_frame(false)
                .build();
            let (picker, popover, name) = (self.clone(), popover.clone(), name.clone());
            button.connect_clicked(move |_| {
                picker.set_icon(Some(&name));
                popover.popdown();
            });
            flow.insert(&button, -1);
        }
        flow
    }

    fn choose_svg(&self) {
        let filter = gtk4::FileFilter::new();
        filter.set_name(Some("SVG images"));
        filter.add_mime_type("image/svg+xml");
        filter.add_suffix("svg");
        let filters = gio::ListStore::new::<gtk4::FileFilter>();
        filters.append(&filter);
        let dialog = gtk4::FileDialog::builder()
            .title("Import SVG Icon")
            .modal(true)
            .filters(&filters)
            .default_filter(&filter)
            .build();
        let picker = self.clone();
        dialog.open(
            window_of(&self.root).as_ref(),
            gio::Cancellable::NONE,
            move |file: Result<gio::File, glib::Error>| {
                if let Some(path) = file.ok().and_then(|file| file.path()) {
                    picker.import(&path);
                }
            },
        );
    }

    fn import(&self, path: &Path) {
        let dir = custom_icon_dir();
        let name = match import_svg(path, &dir) {
            Ok(name) => name,
            Err(err) => return self.show_error(&format!("Could not import the icon: {err}")),
        };
        // `<svg` in the text is no proof GTK can draw it.
        let file = dir.join(format!("{name}.svg"));
        if let Err(err) = gtk4::gdk_pixbuf::Pixbuf::from_file_at_size(&file, 32, 32) {
            let _ = std::fs::remove_file(&file);
            return self.show_error(&format!("Could not import the icon: {err}"));
        }
        rescan(&self.root.display());
        self.set_icon(Some(&name));
    }
}

#[cfg(test)]
mod tests;

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    /// An SVG dropped into a search-path directory resolves by name once
    /// the theme rescans, and the picker maps empty to the default.
    pub(in crate::ui) fn custom_svg_resolves_and_picker_round_trips() {
        let display = gdk::Display::default().expect("display");
        register_builtin_icons(&display);
        let theme = gtk4::IconTheme::for_display(&display);
        for name in BUILTIN {
            assert!(theme.has_icon(name), "built-in {name} is in the theme");
        }

        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("Home Lab.svg");
        std::fs::write(
            &source,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16"/></svg>"#,
        )
        .unwrap();
        let dir = tmp.path().join("icons");
        let name = import_svg(&source, &dir).unwrap();
        assert_eq!(name, "wayterm-home-lab");

        theme.add_search_path(&dir);
        rescan(&display);
        assert!(theme.has_icon(&name), "imported icon is in the theme");
        assert_eq!(
            resolve_icon(Some(&name), FOLDER_ICON, |n| theme.has_icon(n)),
            name
        );

        let picker = IconPicker::new(FOLDER_ICON);
        assert_eq!(picker.icon(), None);
        assert_eq!(picker.preview.icon_name().as_deref(), Some(FOLDER_ICON));
        picker.set_icon(Some(&name));
        assert_eq!(picker.icon().as_deref(), Some(name.as_str()));
        assert_eq!(picker.preview.icon_name().as_deref(), Some(name.as_str()));
        picker.set_icon(Some("  "));
        assert_eq!(picker.icon(), None);
        picker.set_default("computer-symbolic");
        assert_eq!(
            picker.preview.icon_name().as_deref(),
            Some("computer-symbolic")
        );
    }
}
