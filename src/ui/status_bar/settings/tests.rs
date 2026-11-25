use super::*;

#[test]
fn a_missing_row_shows_every_item() {
    assert_eq!(Items::from_setting(None), Items::ALL);
    assert!(Item::ALL.into_iter().all(|item| Items::ALL.shows(item)));
    assert_eq!(Items::ALL.wanted(), Wanted::ALL);
}

#[test]
fn the_row_round_trips_and_skips_unknown_names() {
    let items = Items::ALL
        .with(Item::Disk, false)
        .with(Item::Endpoint, false);
    let row = items.to_setting();
    assert_eq!(row, "state,cpu,memory,bandwidth,uptime,elapsed,savings");
    assert_eq!(Items::from_setting(Some(&row)), items);
    assert_eq!(
        Items::from_setting(Some(" cpu , nonsense,uptime")),
        Items::NONE.with(Item::Cpu, true).with(Item::Uptime, true)
    );
    assert_eq!(Items::from_setting(Some("")), Items::NONE);
    assert_eq!(Items::NONE.to_setting(), "");
}

#[test]
fn only_shown_host_items_are_read() {
    let wanted = Items::NONE
        .with(Item::State, true)
        .with(Item::Memory, true)
        .with(Item::Elapsed, true)
        .wanted();
    assert_eq!(
        wanted,
        Wanted {
            uptime: false,
            cpu: false,
            memory: true,
            bandwidth: false,
            disks: false,
        }
    );
    let local_only = Items::NONE
        .with(Item::State, true)
        .with(Item::Endpoint, true)
        .with(Item::Elapsed, true)
        .with(Item::Savings, true);
    assert!(!local_only.wanted().any(), "no script at all");
}

#[test]
fn the_ids_are_distinct() {
    let ids: std::collections::HashSet<&str> = Item::ALL.into_iter().map(Item::id).collect();
    assert_eq!(ids.len(), Item::ALL.len());
    let sides: Vec<Item> = Item::LEFT.into_iter().chain(Item::RIGHT).collect();
    assert_eq!(sides, Item::ALL, "every item is on a side, in bar order");
}
