//! The Edit Folder dialog: a folder's name and icon (doc 09).

use gtk4::prelude::*;

use crate::store::model::Folder;
use crate::ui::icons::{IconPicker, FOLDER_ICON};
use crate::ui::widgets::{ok_cancel_row, set_margins, Modal};

/// What the dialog saves: the name, and the icon (`None` = the default).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderEdit {
    pub name: String,
    pub icon: Option<String>,
}

/// The edit to store. A blank name keeps `current`'s.
pub fn folder_edit(current: &Folder, name: &str, icon: Option<String>) -> FolderEdit {
    let name = name.trim();
    FolderEdit {
        name: if name.is_empty() {
            current.name.clone()
        } else {
            name.to_string()
        },
        icon,
    }
}

pub struct FolderDialog {
    modal: Modal<FolderEdit>,
    // Read back by the widget test only.
    #[cfg_attr(not(test), allow(dead_code))]
    name: gtk4::Entry,
    #[cfg_attr(not(test), allow(dead_code))]
    icon: IconPicker,
}

impl FolderDialog {
    pub fn new(parent: Option<&gtk4::Window>, folder: &Folder) -> Self {
        let modal = Modal::new("Edit Folder", parent);
        modal.window.set_default_width(420);
        modal.window.set_resizable(false);

        let name = gtk4::Entry::builder()
            .text(&folder.name)
            .hexpand(true)
            .activates_default(true)
            .build();
        let icon = IconPicker::new(FOLDER_ICON);
        icon.set_icon(folder.icon.as_deref());

        let grid = gtk4::Grid::builder()
            .row_spacing(6)
            .column_spacing(12)
            .build();
        for (row, (label, widget)) in [
            ("Name:", name.clone().upcast::<gtk4::Widget>()),
            ("Icon:", icon.widget().clone().upcast()),
        ]
        .into_iter()
        .enumerate()
        {
            let label = gtk4::Label::builder()
                .label(label)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Start)
                .margin_top(6)
                .build();
            grid.attach(&label, 0, row as i32, 1, 1);
            grid.attach(&widget, 1, row as i32, 1, 1);
        }

        let (for_ok, picker, current) = (name.clone(), icon.clone(), folder.clone());
        let buttons = ok_cancel_row(&modal, move || {
            Some(folder_edit(&current, &for_ok.text(), picker.icon()))
        });
        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        set_margins(&vbox, 12);
        vbox.append(&grid);
        vbox.append(&buttons);
        modal.window.set_child(Some(&vbox));
        Self { modal, name, icon }
    }

    /// Run modally; `None` on cancel.
    pub fn run(self) -> Option<FolderEdit> {
        self.modal.run()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(icon: Option<&str>) -> Folder {
        Folder {
            id: 3,
            name: "homelab".into(),
            parent_id: None,
            sort_order: 0,
            icon: icon.map(str::to_string),
        }
    }

    #[test]
    fn blank_name_keeps_the_current_one() {
        let edit = folder_edit(&folder(None), "   ", None);
        assert_eq!(edit.name, "homelab");
        assert_eq!(folder_edit(&folder(None), " lab ", None).name, "lab");
    }

    #[test]
    fn icon_is_passed_through() {
        let edit = folder_edit(&folder(Some("old")), "homelab", Some("new".into()));
        assert_eq!(edit.icon.as_deref(), Some("new"));
        let cleared = folder_edit(&folder(Some("old")), "homelab", None);
        assert_eq!(cleared.icon, None);
    }
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(in crate::ui) mod gtk_tests {
    use super::*;

    pub(in crate::ui) fn edit_folder_prefills_name_and_icon() {
        let folder = Folder {
            id: 1,
            name: "homelab".into(),
            parent_id: None,
            sort_order: 0,
            icon: Some("network-server-symbolic".into()),
        };
        let dialog = FolderDialog::new(None, &folder);
        assert_eq!(dialog.name.text(), "homelab");
        assert_eq!(
            dialog.icon.icon().as_deref(),
            Some("network-server-symbolic")
        );
        dialog.modal.window.destroy();
    }
}
