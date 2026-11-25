//! Which segments the bar shows: one global settings row (doc 13
//! §Settings rows), pushed in at startup and on every change. A hidden
//! host figure is not read at all (doc 03 §Status bar).

use std::cell::RefCell;
use std::sync::atomic::{AtomicU16, Ordering};

use super::Item;
use crate::transport::host_stats::Wanted;

/// Settings row: the shown segments, as a comma-separated list of
/// [`Item::id`]s. Missing means all of them.
pub const STATUS_BAR_ITEMS_SETTING: &str = "status_bar_items";

impl Item {
    /// Every item, left side first, in bar order.
    pub const ALL: [Item; 9] = [
        Item::State,
        Item::Endpoint,
        Item::Cpu,
        Item::Memory,
        Item::Disk,
        Item::Bandwidth,
        Item::Uptime,
        Item::Elapsed,
        Item::Savings,
    ];

    /// The item's name in [`STATUS_BAR_ITEMS_SETTING`].
    pub fn id(self) -> &'static str {
        match self {
            Item::State => "state",
            Item::Endpoint => "endpoint",
            Item::Cpu => "cpu",
            Item::Memory => "memory",
            Item::Disk => "disk",
            Item::Bandwidth => "bandwidth",
            Item::Uptime => "uptime",
            Item::Elapsed => "elapsed",
            Item::Savings => "savings",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Item::State => "Connection state",
            Item::Endpoint => "Endpoint",
            Item::Cpu => "CPU",
            Item::Memory => "RAM",
            Item::Disk => "Disk",
            Item::Bandwidth => "Bandwidth",
            Item::Uptime => "Uptime",
            Item::Elapsed => "Time connected",
            Item::Savings => "Compression saving",
        }
    }

    fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// Which segments the bar shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Items(u16);

impl Items {
    pub const ALL: Self = Self((1 << Item::ALL.len()) - 1);
    pub const NONE: Self = Self(0);

    /// The stored row; a missing one shows every item, and names it does
    /// not know are skipped.
    pub fn from_setting(value: Option<&str>) -> Self {
        let Some(value) = value else {
            return Self::ALL;
        };
        let mut items = Self::NONE;
        for id in value.split(',').map(str::trim) {
            if let Some(item) = Item::ALL.into_iter().find(|item| item.id() == id) {
                items = items.with(item, true);
            }
        }
        items
    }

    pub fn to_setting(self) -> String {
        let shown: Vec<&str> = Item::ALL
            .into_iter()
            .filter(|item| self.shows(*item))
            .map(Item::id)
            .collect();
        shown.join(",")
    }

    pub fn shows(self, item: Item) -> bool {
        self.0 & item.bit() != 0
    }

    pub fn with(self, item: Item, shown: bool) -> Self {
        if shown {
            Self(self.0 | item.bit())
        } else {
            Self(self.0 & !item.bit())
        }
    }

    /// The host figures the shown items need, so a hidden one is not read.
    pub fn wanted(self) -> Wanted {
        Wanted {
            uptime: self.shows(Item::Uptime),
            cpu: self.shows(Item::Cpu),
            memory: self.shows(Item::Memory),
            bandwidth: self.shows(Item::Bandwidth),
            disks: self.shows(Item::Disk),
        }
    }
}

static ITEMS: AtomicU16 = AtomicU16::new(Items::ALL.0);

/// Told of a change; returns false once it is gone, to be dropped.
type Listener = Box<dyn Fn() -> bool>;

thread_local! {
    static LISTENERS: RefCell<Vec<Listener>> = const { RefCell::new(Vec::new()) };
}

/// `status_bar_items`, pushed in at startup and on every change; a change
/// reaches every bar through its window's listener.
pub fn install_items(items: Items) {
    if ITEMS.swap(items.0, Ordering::SeqCst) == items.0 {
        return;
    }
    let listeners = LISTENERS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
    let alive: Vec<Listener> = listeners.into_iter().filter(|told| told()).collect();
    LISTENERS.with(|slot| {
        let mut slot = slot.borrow_mut();
        let added = std::mem::replace(&mut *slot, alive);
        slot.extend(added);
    });
}

pub fn shown_items() -> Items {
    Items(ITEMS.load(Ordering::SeqCst))
}

/// Run `changed` after every change of the shown items, until it returns
/// false.
pub fn on_items_changed(changed: impl Fn() -> bool + 'static) {
    LISTENERS.with(|slot| slot.borrow_mut().push(Box::new(changed)));
}

#[cfg(test)]
mod tests;
