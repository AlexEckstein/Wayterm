//! The sidebar's two settings rows (doc 13 §Settings rows), pushed in at
//! startup and read by every sidebar: whether a connect opens SFTP, and
//! the listing's optional columns.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Settings row (doc 13 §Settings rows): open the sidebar's SFTP channel when an SSH tab
/// connects, and bring the side dock's SFTP panel forward. Off, the
/// sidebar starts hidden and attaches on its first show, so no channel
/// opens for a sidebar nobody looks at.
pub const SFTP_ON_CONNECT_SETTING: &str = "sftp_on_connect";

static OPEN_ON_CONNECT: AtomicBool = AtomicBool::new(true);

/// `sftp_on_connect`, pushed in at startup and on every change; tabs read
/// it when they are built and when they connect.
pub fn install_settings(open_on_connect: bool) {
    OPEN_ON_CONNECT.store(open_on_connect, Ordering::SeqCst);
}

pub fn open_on_connect() -> bool {
    OPEN_ON_CONNECT.load(Ordering::SeqCst)
}

/// Whether a freshly connected sidebar attaches at once or waits for its
/// first show. A sidebar the user already opened attaches regardless.
pub fn attach_on_connect(open_on_connect: bool, shown: bool) -> bool {
    open_on_connect || shown
}

/// Settings row (doc 13 §Settings rows): the listing's optional columns
/// that are shown, as a comma-separated list of [`Column::id`]s. Missing
/// means all of them.
pub const SFTP_COLUMNS_SETTING: &str = "sftp_columns";

/// A listing column the user can hide; Name always shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Column {
    Size,
    Modified,
    Permissions,
}

impl Column {
    /// Listing order, after Name.
    pub const ALL: [Column; 3] = [Column::Size, Column::Modified, Column::Permissions];

    /// The column's name in [`SFTP_COLUMNS_SETTING`] and in the header
    /// menu's actions.
    pub fn id(self) -> &'static str {
        match self {
            Column::Size => "size",
            Column::Modified => "modified",
            Column::Permissions => "permissions",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Column::Size => "Size",
            Column::Modified => "Modified",
            Column::Permissions => "Permissions",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Which optional columns the listing shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Columns(u8);

impl Columns {
    pub const ALL: Self = Self(0b111);
    pub const NONE: Self = Self(0);

    /// The stored row; a missing one shows every column, and names it does
    /// not know are skipped.
    pub fn from_setting(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::ALL;
        };
        let mut columns = Self::NONE;
        for id in value.split(',').map(str::trim) {
            if let Some(column) = Column::ALL.into_iter().find(|column| column.id() == id) {
                columns = columns.with(column, true);
            }
        }
        columns
    }

    pub fn to_setting(self) -> String {
        let shown: Vec<&str> = Column::ALL
            .into_iter()
            .filter(|column| self.shows(*column))
            .map(Column::id)
            .collect();
        shown.join(",")
    }

    pub fn shows(self, column: Column) -> bool {
        self.0 & column.bit() != 0
    }

    pub fn with(self, column: Column, shown: bool) -> Self {
        if shown {
            Self(self.0 | column.bit())
        } else {
            Self(self.0 & !column.bit())
        }
    }
}

static COLUMNS: AtomicU8 = AtomicU8::new(Columns::ALL.0);

type SaveColumns = Box<dyn Fn(Columns)>;

thread_local! {
    static SAVE_COLUMNS: RefCell<Option<SaveColumns>> = const { RefCell::new(None) };
}

/// `sftp_columns`, pushed in at startup; every sidebar shows these.
pub fn install_columns(columns: Columns) {
    COLUMNS.store(columns.0, Ordering::SeqCst);
}

pub fn columns() -> Columns {
    Columns(COLUMNS.load(Ordering::SeqCst))
}

/// How a choice made in a column header's menu is stored.
pub fn save_columns_with(save: impl Fn(Columns) + 'static) {
    SAVE_COLUMNS.with(|slot| *slot.borrow_mut() = Some(Box::new(save)));
}

/// The user picked `columns` in a header menu: every sidebar shows them
/// from now on, and they are stored.
pub fn choose_columns(columns: Columns) {
    install_columns(columns);
    SAVE_COLUMNS.with(|slot| {
        if let Some(save) = slot.borrow().as_ref() {
            save(columns);
        }
    });
}
