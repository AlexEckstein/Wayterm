//! The About page (doc 13 §About): the mark and name, then the versions of
//! Wayterm and the libraries it runs on, then where the source lives.
//!
//! [`versions`] is display-free; [`page`] only renders it.

use gtk4::prelude::*;

use crate::terminal::{ghostty_version, vte_version};

/// The mark in `app-icon.gresource.xml`; `-symbolic` makes GTK paint it in
/// the theme's text color.
pub const MARK_ICON: &str = "wayterm-mark-symbolic";

/// The libghostty-vt value in a build without the `backend-ghostty` feature.
pub const GHOSTTY_NOT_INCLUDED: &str = "Not included in this build";

/// (title, value) per row of the Versions list, in order. The commit row
/// is left out of a build made outside a git checkout.
pub fn versions() -> Vec<(&'static str, String)> {
    let mut rows = vec![("Wayterm", env!("CARGO_PKG_VERSION").to_string())];
    if let Some(commit) = option_env!("WAYTERM_GIT_COMMIT") {
        rows.push(("Commit", commit.to_string()));
    }
    rows.push((
        "libghostty-vt",
        ghostty_version().unwrap_or_else(|| GHOSTTY_NOT_INCLUDED.to_string()),
    ));
    rows.push(("VTE", vte_version()));
    rows.push((
        "GTK",
        format!(
            "{}.{}.{}",
            gtk4::major_version(),
            gtk4::minor_version(),
            gtk4::micro_version()
        ),
    ));
    rows
}

pub fn page() -> gtk4::Widget {
    // The name below says it for assistive technology.
    let mark = gtk4::Image::builder()
        .icon_name(MARK_ICON)
        .pixel_size(128)
        .halign(gtk4::Align::Center)
        .accessible_role(gtk4::AccessibleRole::Presentation)
        .build();
    let name = gtk4::Label::new(Some("Wayterm"));
    name.add_css_class("title-1");
    name.set_margin_bottom(12);

    let list = super::boxed_list();
    for (title, value) in versions() {
        list.append(&value_row(title, &value));
    }

    let project_heading = super::heading("Project");
    project_heading.set_margin_top(12);
    let project = super::boxed_list();
    let repository = gtk4::glib::markup_escape_text(env!("CARGO_PKG_REPOSITORY"));
    let link = gtk4::Label::builder()
        .label(format!(
            "<a href=\"{repository}\">{}</a>",
            repository.trim_start_matches("https://")
        ))
        .use_markup(true)
        .xalign(1.0)
        .valign(gtk4::Align::Center)
        .build();
    let text = super::described("Source code", None, &link);
    project.append(&super::row(&[text.upcast_ref(), link.upcast_ref()]));
    project.append(&value_row("License", env!("CARGO_PKG_LICENSE")));

    super::scrolling_page(&[
        mark.upcast_ref(),
        name.upcast_ref(),
        super::heading("Versions").upcast_ref(),
        list.upcast_ref(),
        project_heading.upcast_ref(),
        project.upcast_ref(),
    ])
}

/// title | value, the value selectable so it can be copied into a report.
fn value_row(title: &str, value: &str) -> gtk4::ListBoxRow {
    let value = gtk4::Label::builder()
        .label(value)
        .selectable(true)
        .xalign(1.0)
        .valign(gtk4::Align::Center)
        .css_classes(["dim-label"])
        .build();
    let text = super::described(title, None, &value);
    super::row(&[text.upcast_ref(), value.upcast_ref()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_list_wayterm_first_and_ghostty_only_when_built() {
        let rows = versions();
        let titles: Vec<&str> = rows.iter().map(|(title, _)| *title).collect();
        let mut expected = vec!["Wayterm"];
        if option_env!("WAYTERM_GIT_COMMIT").is_some() {
            expected.push("Commit");
        }
        expected.extend(["libghostty-vt", "VTE", "GTK"]);
        assert_eq!(titles, expected);
        assert_eq!(rows[0].1, env!("CARGO_PKG_VERSION"));

        let ghostty = &rows.iter().find(|(t, _)| *t == "libghostty-vt").unwrap().1;
        match ghostty_version() {
            Some(version) => assert_eq!(ghostty, &version),
            None => assert_eq!(ghostty, GHOSTTY_NOT_INCLUDED),
        }
        for (title, value) in &rows {
            assert!(!value.is_empty(), "{title} has no value");
        }
    }
}
