//! The sidebar's widgets (doc 07 §Layout), built and laid out but not
//! wired: what they do is the sidebar's business.

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib, pango};

use super::{columns, Column, RowView, START_PATH};
use crate::ui::widgets::{data, set_data, set_margins};

/// Key under which each cell keeps a weak ref to its `ListItem`, so a
/// click can be traced back to a row.
const LIST_ITEM_KEY: &str = "wayterm-sftp-list-item";

/// The narrowest the Name column gets before the listing scrolls sideways.
const NAME_MIN_CHARS: i32 = 14;

/// The toolbar buttons plus Cancel.
pub(super) struct Buttons {
    pub(super) upload: gtk4::Button,
    pub(super) download: gtk4::Button,
    pub(super) delete: gtk4::Button,
    pub(super) rename: gtk4::Button,
    pub(super) refresh: gtk4::Button,
    pub(super) mkdir: gtk4::Button,
    pub(super) cancel: gtk4::Button,
}

pub(super) struct Widgets {
    pub(super) container: gtk4::Box,
    pub(super) path_entry: gtk4::Entry,
    pub(super) buttons: Buttons,
    pub(super) error_label: gtk4::Label,
    pub(super) column_view: gtk4::ColumnView,
    /// The columns a header's menu can hide, each with the menu's action.
    pub(super) columns: Vec<(Column, gtk4::ColumnViewColumn, gio::SimpleAction)>,
    /// The listing's rows, each a boxed [`RowView`].
    pub(super) rows: gio::ListStore,
    pub(super) selection: gtk4::SingleSelection,
    /// The row menu, reused for every right-click on a row.
    row_menu: gtk4::PopoverMenu,
    /// The bar and Cancel, shown only while a transfer runs.
    pub(super) progress_row: gtk4::Box,
    pub(super) progress_bar: gtk4::ProgressBar,
    pub(super) status_label: gtk4::Label,
    pub(super) follow_check: gtk4::CheckButton,
}

impl Widgets {
    /// Doc 07 §Layout, top to bottom; `local` for a local-shell tab's.
    pub(super) fn new(local: bool) -> Self {
        let path_entry = gtk4::Entry::new();
        path_entry.set_text(START_PATH);
        path_entry.set_placeholder_text(Some(if local { "path" } else { "remote path" }));
        path_entry.set_hexpand(true);

        // The icons are bundled with the app (`ui::icons`).
        let buttons = Buttons {
            upload: icon_button("wayterm-upload-symbolic", "Upload"),
            download: icon_button("wayterm-download-symbolic", "Download"),
            delete: icon_button("wayterm-trash-symbolic", "Delete"),
            rename: icon_button("wayterm-rename-symbolic", "Rename"),
            refresh: icon_button("wayterm-refresh-symbolic", "Refresh"),
            mkdir: icon_button("wayterm-folder-new-symbolic", "New folder"),
            cancel: gtk4::Button::with_label("Cancel"),
        };
        let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        for button in [
            &buttons.upload,
            &buttons.download,
            &buttons.delete,
            &buttons.rename,
            &buttons.refresh,
            &buttons.mkdir,
        ] {
            toolbar.append(button);
        }
        buttons
            .cancel
            .set_tooltip_text(Some("Cancel the running transfer"));
        buttons.cancel.set_sensitive(false);

        // Hidden while there is no error.
        let error_label = line_label();
        error_label.add_css_class("dim-label");
        error_label.set_visible(false);

        let rows = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk4::SingleSelection::builder()
            .model(&rows)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let column_view = gtk4::ColumnView::builder().model(&selection).build();
        let columns = add_columns(&column_view);
        let scrolled = gtk4::ScrolledWindow::builder()
            .child(&column_view)
            .vexpand(true)
            .hexpand(true)
            .build();
        let row_menu = gtk4::PopoverMenu::from_model(None::<&gio::MenuModel>);
        row_menu.set_parent(&scrolled);
        row_menu.set_has_arrow(false);
        row_menu.set_halign(gtk4::Align::Start);
        // The scrolled window only unparents the children it knows about.
        scrolled.connect_destroy(glib::clone!(
            #[strong]
            row_menu,
            move |_| row_menu.unparent()
        ));

        // The bar with Cancel, and the last transfer's result: each shows
        // only when it has something to say.
        let progress_bar = gtk4::ProgressBar::new();
        progress_bar.set_show_text(true);
        progress_bar.set_hexpand(true);
        progress_bar.set_ellipsize(pango::EllipsizeMode::Middle);
        let progress_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        progress_row.append(&progress_bar);
        progress_row.append(&buttons.cancel);
        let status_label = line_label();

        let follow_check = gtk4::CheckButton::with_label("Follow terminal directory");
        follow_check.set_active(true);
        follow_check.set_tooltip_text(Some(
            "Show the directory the terminal's shell is in, whenever it changes",
        ));
        follow_check.set_focus_on_click(false);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        set_margins(&container, 4);
        container.append(&path_entry);
        container.append(&toolbar);
        container.append(&error_label);
        container.append(&scrolled);
        container.append(&progress_row);
        container.append(&status_label);
        container.append(&follow_check);

        Self {
            container,
            path_entry,
            buttons,
            error_label,
            column_view,
            columns,
            rows,
            selection,
            row_menu,
            progress_row,
            progress_bar,
            status_label,
            follow_check,
        }
    }

    /// Show the chosen columns, and tick them in the header menu.
    pub(super) fn apply_columns(&self) {
        let chosen = columns();
        for (column, view_column, action) in &self.columns {
            let shown = chosen.shows(*column);
            view_column.set_visible(shown);
            action.set_state(&shown.to_variant());
        }
    }

    pub(super) fn row_at(&self, position: u32) -> Option<RowView> {
        self.rows.item(position).and_then(|item| row_of(&item))
    }

    pub(super) fn selected_row(&self) -> Option<RowView> {
        self.selection
            .selected_item()
            .and_then(|item| row_of(&item))
    }

    /// The position of the row at `(x, y)` in listing coordinates; `None`
    /// over the headers or below the rows.
    pub(super) fn position_at(&self, x: f64, y: f64) -> Option<u32> {
        let mut widget = self.column_view.pick(x, y, gtk4::PickFlags::DEFAULT);
        while let Some(current) = widget {
            if let Some(item) = data::<glib::WeakRef<gtk4::ListItem>>(&current, LIST_ITEM_KEY) {
                return item
                    .upgrade()
                    .map(|item| item.position())
                    .filter(|&position| position != gtk4::INVALID_LIST_POSITION);
            }
            if current == *self.column_view.upcast_ref::<gtk4::Widget>() {
                break;
            }
            widget = current.parent();
        }
        None
    }

    /// Pop the row menu up at `(x, y)` in listing coordinates, with
    /// `menu`'s items acting through the `row` action group.
    pub(super) fn popup_row_menu(
        &self,
        menu: &gio::Menu,
        actions: &gio::SimpleActionGroup,
        x: f64,
        y: f64,
    ) {
        self.row_menu.set_menu_model(Some(menu));
        self.row_menu.insert_action_group("row", Some(actions));
        let click = gtk4::graphene::Point::new(x as f32, y as f32);
        let at = self
            .row_menu
            .parent()
            .and_then(|parent| self.column_view.compute_point(&parent, &click))
            .unwrap_or(click);
        self.row_menu.set_pointing_to(Some(&gdk::Rectangle::new(
            at.x() as i32,
            at.y() as i32,
            0,
            0,
        )));
        self.row_menu.popup();
    }
}

/// A button carrying a symbolic icon and a tooltip naming the operation.
fn icon_button(icon_name: &str, tooltip: &str) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon_name);
    button.set_tooltip_text(Some(tooltip));
    button.set_focus_on_click(false);
    button
}

/// A one-line message, cut at its end in a narrow dock.
fn line_label() -> gtk4::Label {
    let label = gtk4::Label::new(None);
    label.set_xalign(0.0);
    label.set_ellipsize(pango::EllipsizeMode::End);
    label
}

/// The [`RowView`] behind a listing item.
fn row_of(item: &glib::Object) -> Option<RowView> {
    let boxed = item.downcast_ref::<glib::BoxedAnyObject>()?;
    let row = boxed.borrow::<RowView>().clone();
    Some(row)
}

/// Name, then the columns a right-click on any header can hide.
fn add_columns(
    column_view: &gtk4::ColumnView,
) -> Vec<(Column, gtk4::ColumnViewColumn, gio::SimpleAction)> {
    let header_menu = gio::Menu::new();
    let actions = gio::SimpleActionGroup::new();
    let name = name_column();
    name.set_header_menu(Some(&header_menu));
    column_view.append_column(&name);
    let mut columns = Vec::new();
    for column in Column::ALL {
        let action = gio::SimpleAction::new_stateful(column.id(), None, &true.to_variant());
        actions.add_action(&action);
        header_menu.append(
            Some(column.title()),
            Some(&format!("columns.{}", column.id())),
        );
        let view_column = detail_column(column);
        view_column.set_header_menu(Some(&header_menu));
        column_view.append_column(&view_column);
        columns.push((column, view_column, action));
    }
    column_view.insert_action_group("columns", Some(&actions));
    columns
}

/// A resizable column whose cells `setup` makes and `bind` fills from
/// their row. Cells are padded: not every theme pads them, and unpadded
/// columns run into each other.
fn cell_column<Cell: IsA<gtk4::Widget> + IsA<glib::Object>>(
    title: &str,
    setup: impl Fn() -> Cell + 'static,
    bind: impl Fn(&Cell, &RowView) + 'static,
) -> gtk4::ColumnViewColumn {
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_setup(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let cell = setup();
        cell.set_margin_start(6);
        cell.set_margin_end(6);
        set_data(&cell, LIST_ITEM_KEY, item.downgrade());
        item.set_child(Some(&cell));
    });
    factory.connect_bind(move |_, object| {
        let Some(item) = object.downcast_ref::<gtk4::ListItem>() else {
            return;
        };
        let cell = item.child().and_downcast::<Cell>();
        let row = item.item().and_then(|object| row_of(&object));
        if let (Some(cell), Some(row)) = (cell, row) {
            bind(&cell, &row);
        }
    });
    gtk4::ColumnViewColumn::builder()
        .title(title)
        .resizable(true)
        .factory(&factory)
        .build()
}

/// The Name column: each row's icon beside its name, which takes the
/// width the other columns leave and is cut in the middle when short of it.
fn name_column() -> gtk4::ColumnViewColumn {
    let setup = || {
        let cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        cell.append(&gtk4::Image::new());
        cell.append(
            &gtk4::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(pango::EllipsizeMode::Middle)
                .width_chars(NAME_MIN_CHARS)
                .build(),
        );
        cell
    };
    let name = cell_column("Name", setup, |cell: &gtk4::Box, row| {
        if let Some(image) = cell.first_child().and_downcast::<gtk4::Image>() {
            image.set_icon_name(Some(row.icon));
        }
        if let Some(label) = cell.last_child().and_downcast::<gtk4::Label>() {
            label.set_text(&row.name);
            label.set_tooltip_text(Some(&row.name));
        }
    });
    name.set_expand(true);
    name
}

/// A column of `column`'s text for each row, dimmed beside the names.
/// Digits are all one width, so sizes and dates line up down the column;
/// sizes sit against the column's right edge.
fn detail_column(column: Column) -> gtk4::ColumnViewColumn {
    let (xalign, text): (f32, fn(&RowView) -> &str) = match column {
        Column::Size => (1.0, |row| &row.size),
        Column::Modified => (0.0, |row| &row.modified),
        Column::Permissions => (0.0, |row| &row.permissions),
    };
    let setup = move || {
        let attributes = pango::AttrList::new();
        attributes.insert(pango::AttrFontFeatures::new("tnum=1"));
        if column == Column::Permissions {
            attributes.insert(pango::AttrString::new_family("monospace"));
        }
        let label = gtk4::Label::builder()
            .xalign(xalign)
            .attributes(&attributes)
            .build();
        label.add_css_class("dim-label");
        label
    };
    cell_column(column.title(), setup, move |label: &gtk4::Label, row| {
        label.set_text(text(row));
    })
}
