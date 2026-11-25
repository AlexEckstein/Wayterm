//! The status bar: segments for the connection state, the endpoint, the
//! host's CPU, memory, disks, bandwidth and uptime, the time connected and
//! what compression saves, each with a tooltip that adds detail (doc 03
//! §Status bar). [`segments`] decides what each shows; [`StatusBar`] only
//! renders it. Which tab drives it is the window's business.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;

use crate::transport::types::TransportState;

pub mod segments;
pub use segments::*;

/// Reads the current tab's [`LiveStatus`].
pub type LiveSource = Rc<dyn Fn() -> LiveStatus>;

/// A segment's widgets: the separator before it and its label, with an
/// optional icon.
#[derive(Clone)]
struct Part {
    separator: gtk4::Separator,
    area: gtk4::Box,
    label: gtk4::Label,
    /// Tabular digits, so a ticking figure keeps its width.
    numeric: bool,
}

impl Part {
    fn new(icon: Option<&str>, ellipsize: EllipsizeMode, numeric: bool) -> Self {
        let label = gtk4::Label::new(None);
        label.set_ellipsize(ellipsize);
        label.set_xalign(0.0);
        let area = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        if let Some(icon) = icon {
            area.append(
                &gtk4::Image::builder()
                    .icon_name(icon)
                    .accessible_role(gtk4::AccessibleRole::Presentation)
                    .build(),
            );
        }
        area.append(&label);
        Self {
            separator: gtk4::Separator::new(gtk4::Orientation::Vertical),
            area,
            label,
            numeric,
        }
    }

    fn append_to(&self, container: &gtk4::Box) {
        container.append(&self.separator);
        container.append(&self.area);
    }

    fn show(&self, segment: Option<&Segment>) {
        if let Some(segment) = segment {
            let mut attributes = String::new();
            if self.numeric {
                attributes.push_str(" font_features=\"tnum\"");
            }
            if let Some(color) = segment.color {
                attributes.push_str(&format!(" foreground=\"{color}\""));
            }
            if attributes.is_empty() {
                self.label.set_text(&segment.text);
            } else {
                self.label.set_markup(&format!(
                    "<span{attributes}>{}</span>",
                    glib::markup_escape_text(&segment.text)
                ));
            }
            match &segment.tooltip_markup {
                Some(markup) => self.area.set_tooltip_markup(Some(markup)),
                None => self.area.set_tooltip_text(Some(&segment.tooltip)),
            }
        }
        self.area.set_visible(segment.is_some());
    }
}

/// A separator shows only between two shown segments of one side.
fn separate(side: &[(Item, Part)]) {
    let mut before = false;
    for (_, part) in side {
        let shown = part.area.is_visible();
        part.separator.set_visible(shown && before);
        before |= shown;
    }
}

/// An item's widgets. The time and the host figures have tabular digits.
fn part(item: Item) -> Part {
    match item {
        Item::State => Part::new(None, EllipsizeMode::End, false),
        Item::Endpoint => Part::new(None, EllipsizeMode::Middle, false),
        Item::Cpu | Item::Memory | Item::Disk | Item::Bandwidth => {
            Part::new(None, EllipsizeMode::None, true)
        }
        Item::Uptime | Item::Savings => Part::new(None, EllipsizeMode::None, false),
        Item::Elapsed => Part::new(
            Some("document-open-recent-symbolic"),
            EllipsizeMode::None,
            true,
        ),
    }
}

/// The segments of [`Item::LEFT`] on the left and [`Item::RIGHT`] on the
/// right, the state led by a coloured dot, ticking every second while
/// connected.
pub struct StatusBar {
    container: gtk4::Box,
    dot: gtk4::Label,
    left: Vec<(Item, Part)>,
    right: Vec<(Item, Part)>,
    timer: RefCell<Option<glib::SourceId>>,
    live_source: RefCell<Option<LiveSource>>,
}

impl StatusBar {
    /// Starts out Disconnected.
    pub fn new() -> Self {
        let dot = gtk4::Label::builder()
            .use_markup(true)
            .valign(gtk4::Align::Center)
            .accessible_role(gtk4::AccessibleRole::Presentation)
            .build();
        let parts = |items: &[Item]| -> Vec<(Item, Part)> {
            items.iter().map(|&item| (item, part(item))).collect()
        };
        let (left, right) = (parts(&Item::LEFT), parts(&Item::RIGHT));

        let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        container.add_css_class("wayterm-status-bar");
        for (item, part) in &left {
            if *item == Item::State {
                part.area.prepend(&dot);
            }
            part.append_to(&container);
        }
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        container.append(&spacer);
        for (_, part) in &right {
            part.append_to(&container);
        }
        container.set_margin_top(2);
        container.set_margin_bottom(2);
        container.set_margin_start(8);
        container.set_margin_end(8);

        let this = Self {
            container,
            dot,
            left,
            right,
            timer: RefCell::default(),
            live_source: RefCell::default(),
        };
        this.set_state(&TransportState::Disconnected, "", 0, None, None);
        this
    }

    pub fn widget(&self) -> &gtk4::Widget {
        self.container.upcast_ref()
    }

    /// Where the next [`StatusBar::set_state`] reads the live figures from.
    pub fn set_live_source(&self, source: Option<LiveSource>) {
        self.live_source.replace(source);
    }

    /// Show `state`. Connected shows the time since `since` and ticks it;
    /// anything else stops the tick.
    pub fn set_state(
        &self,
        state: &TransportState,
        host: &str,
        port: u16,
        since: Option<&ConnectedSince>,
        session: Option<&SessionInfo>,
    ) {
        self.stop_timer();
        let connected = *state == TransportState::Connected;
        let tab = TabStatus {
            state: state.clone(),
            host: host.to_string(),
            port,
            session: session.cloned(),
            since: connected.then(|| since.cloned().unwrap_or_else(ConnectedSince::now)),
        };
        let source = self.live_source.borrow().clone().filter(|_| connected);
        self.dot.set_markup(&format!(
            "<span foreground=\"{}\">●</span>",
            state_color(state).hex()
        ));
        let (left, right) = (self.left.clone(), self.right.clone());
        let refresh = move || {
            let live = source.as_ref().map(|source| source()).unwrap_or_default();
            for side in [&left, &right] {
                for (item, part) in side {
                    part.show(item.segment(&tab, &live).as_ref());
                }
                separate(side);
            }
        };
        refresh();
        if connected {
            let tick = glib::timeout_add_local(Duration::from_secs(1), move || {
                refresh();
                glib::ControlFlow::Continue
            });
            self.timer.replace(Some(tick));
        }
    }

    fn stop_timer(&self) {
        if let Some(tick) = self.timer.take() {
            tick.remove();
        }
    }
}

impl Drop for StatusBar {
    fn drop(&mut self) {
        self.stop_timer();
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
pub(in crate::ui) mod gtk_tests;
