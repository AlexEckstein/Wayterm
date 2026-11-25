//! The Customize page's status bar section (doc 13 §Customize): a switch
//! per segment, for every tab. A hidden host figure is not read.

use std::rc::Rc;

use gtk4::prelude::*;

use super::rows::save_and_apply;
use super::{save_status_bar_items, status_bar_items};
use crate::store::SessionStore;
use crate::ui::settings::{boxed_list, described, row};
use crate::ui::status_bar::{Item, STATUS_BAR_ITEMS_SETTING};

/// What showing `item` means; a host figure says it is read only while
/// shown.
pub fn subtitle(item: Item) -> &'static str {
    match item {
        Item::State => "The colored dot and Connected, Reconnecting or the error.",
        Item::Endpoint => "The host and port, or the shell or serial line.",
        Item::Cpu => "How busy the host's CPUs are. Off: the CPU is not read.",
        Item::Memory => "The share of the host's memory in use. Off: memory is not read.",
        Item::Disk => "Used and total space of the host's filesystems. Off: df never runs.",
        Item::Bandwidth => "Traffic on the host's default-route interface. Off: not read.",
        Item::Uptime => "How long the host has been up. Off: not read.",
        Item::Elapsed => "How long the tab has been connected.",
        Item::Savings => "What SSH compression saves on the connection.",
    }
}

pub(super) fn section(store: &Rc<SessionStore>) -> gtk4::ListBox {
    let shown = status_bar_items(store);
    let list = boxed_list();
    for item in Item::ALL {
        list.append(&item_row(store, item, shown.shows(item)));
    }
    list
}

/// title over subtitle | switch.
fn item_row(store: &Rc<SessionStore>, item: Item, on: bool) -> gtk4::ListBoxRow {
    let switch = gtk4::Switch::builder()
        .active(on)
        .valign(gtk4::Align::Center)
        .build();
    let text = described(item.title(), Some(subtitle(item)), &switch);
    let store = Rc::clone(store);
    switch.connect_active_notify(move |switch| {
        let items = status_bar_items(&store).with(item, switch.is_active());
        save_and_apply(&store, STATUS_BAR_ITEMS_SETTING, |store| {
            save_status_bar_items(store, items)
        });
    });
    row(&[text.upcast_ref(), switch.upcast_ref()])
}
