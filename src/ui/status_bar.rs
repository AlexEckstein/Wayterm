//! The status bar: segments for the connection state, the endpoint, the
//! host's CPU, memory, disks, bandwidth and uptime, the time connected and
//! what compression saves, each with a tooltip that adds detail (doc 03
//! §Status bar). [`segments`] decides what each shows and [`settings`]
//! which are shown; [`StatusBar`] only renders it. Which tab drives it is
//! the window's business.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::pango::EllipsizeMode;
use gtk4::prelude::*;

use crate::transport::types::TransportState;

pub mod segments;
pub mod settings;
pub use segments::*;
pub use settings::*;

/// Reads the current tab's [`LiveStatus`].
pub type LiveSource = Rc<dyn Fn() -> LiveStatus>;

/// A segment's widgets: the separator before it and a label per field,
/// with an optional icon.
#[derive(Clone)]
struct Part {
    separator: gtk4::Separator,
    area: gtk4::Box,
    labels: Vec<gtk4::Label>,
    /// Per label the texts it is held as wide as ([`Item::widest`]); a held
    /// label has tabular digits.
    widest: Vec<Vec<String>>,
    /// The font the widths were measured in.
    measured_in: Rc<RefCell<Option<String>>>,
}

impl Part {
    fn new(
        icon: Option<&str>,
        ellipsize: EllipsizeMode,
        widest: Vec<Vec<String>>,
        wrap_tooltip: bool,
    ) -> Self {
        let area = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        if let Some(icon) = icon {
            area.append(
                &gtk4::Image::builder()
                    .icon_name(icon)
                    .accessible_role(gtk4::AccessibleRole::Presentation)
                    .build(),
            );
        }
        let labels: Vec<gtk4::Label> = (0..widest.len().max(1))
            .map(|_| {
                let label = gtk4::Label::new(None);
                label.set_ellipsize(ellipsize);
                label.set_xalign(0.0);
                area.append(&label);
                label
            })
            .collect();
        // GTK wraps a tooltip at 70 average characters, which breaks the
        // disk tooltip's monospace table.
        if !wrap_tooltip {
            let tip = gtk4::Label::new(None);
            area.connect_query_tooltip(move |area, _, _, _, tooltip| {
                let Some(markup) = area.tooltip_markup() else {
                    return false;
                };
                tip.set_markup(&markup);
                tooltip.set_custom(Some(&tip));
                true
            });
        }
        Self {
            separator: gtk4::Separator::new(gtk4::Orientation::Vertical),
            area,
            labels,
            widest,
            measured_in: Rc::default(),
        }
    }

    fn append_to(&self, container: &gtk4::Box) {
        container.append(&self.separator);
        container.append(&self.area);
    }

    fn markup(&self, text: &str, color: Option<&str>) -> Option<String> {
        let mut attributes = String::new();
        if !self.widest.is_empty() {
            attributes.push_str(" font_features=\"tnum\"");
        }
        if let Some(color) = color {
            attributes.push_str(&format!(" foreground=\"{color}\""));
        }
        (!attributes.is_empty()).then(|| {
            format!(
                "<span{attributes}>{}</span>",
                glib::markup_escape_text(text)
            )
        })
    }

    fn show(&self, segment: Option<&Segment>) {
        if let Some(segment) = segment {
            let whole = [segment.text.clone()];
            let fields = if segment.fields.is_empty() {
                &whole[..]
            } else {
                &segment.fields[..]
            };
            for (label, text) in self.labels.iter().zip(fields) {
                match self.markup(text, segment.color) {
                    Some(markup) => label.set_markup(&markup),
                    None => label.set_text(text),
                }
            }
            match &segment.tooltip_markup {
                Some(markup) => self.area.set_tooltip_markup(Some(markup)),
                None => self.area.set_tooltip_text(Some(&segment.tooltip)),
            }
            self.hold_widths();
        }
        self.area.set_visible(segment.is_some());
    }

    /// Gives each held label the width of its widest text, so a changing
    /// figure moves nothing; measured again when the font changes.
    fn hold_widths(&self) {
        let Some(first) = self.labels.first().filter(|_| !self.widest.is_empty()) else {
            return;
        };
        let font = first
            .pango_context()
            .font_description()
            .map(|font| font.to_string());
        if *self.measured_in.borrow() == font {
            return;
        }
        for (label, texts) in self.labels.iter().zip(&self.widest) {
            let layout = label.create_pango_layout(None);
            let width = texts
                .iter()
                .map(|text| {
                    match self.markup(text, None) {
                        Some(markup) => layout.set_markup(&markup),
                        None => layout.set_text(text),
                    }
                    layout.pixel_size().0
                })
                .max()
                .unwrap_or(-1);
            label.set_size_request(width, -1);
        }
        self.measured_in.replace(font);
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

/// An item's widgets. Only the state's tooltip wraps, as it may be a long
/// error message.
fn part(item: Item) -> Part {
    let (icon, ellipsize) = match item {
        Item::State => (None, EllipsizeMode::End),
        Item::Endpoint => (None, EllipsizeMode::Middle),
        Item::Elapsed => (Some("document-open-recent-symbolic"), EllipsizeMode::None),
        _ => (None, EllipsizeMode::None),
    };
    Part::new(icon, ellipsize, item.widest(), item == Item::State)
}

/// The shown segments of [`Item::LEFT`] on the left and [`Item::RIGHT`]
/// on the right, the state led by a coloured dot, ticking every second
/// while connected.
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
            let items = shown_items();
            for side in [&left, &right] {
                for (item, part) in side {
                    let segment = items.shows(*item).then(|| item.segment(&tab, &live));
                    part.show(segment.flatten().as_ref());
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
