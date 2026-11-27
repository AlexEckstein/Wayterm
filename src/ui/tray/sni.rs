//! StatusNotifierItem and its dbusmenu over gio's D-Bus (doc 03 §Tray).
//!
//! The protocol is two objects on the session bus: the item
//! (`org.kde.StatusNotifierItem` at [`ITEM_PATH`]) and its menu
//! (`com.canonical.dbusmenu` at [`MENU_PATH`]), registered with the
//! desktop's `org.kde.StatusNotifierWatcher`. Both are static: the icon
//! and the menu never change, so no change signal is ever sent.
//!
//! The calls are answered by the pure functions below ([`item_call`],
//! [`menu_call`] and the property getters); [`Service`] wires them to a
//! [`gio::DBusConnection`]. gio dispatches each call on the main context
//! that registered the objects, so the actions run on the GTK main thread
//! without a queue or a runtime of their own.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gio, glib};

use super::{MenuEntry, TrayAction, TrayIcon};

pub const ITEM_PATH: &str = "/StatusNotifierItem";
pub const MENU_PATH: &str = "/MenuBar";
pub const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
pub const MENU_INTERFACE: &str = "com.canonical.dbusmenu";
pub const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
pub const WATCHER_PATH: &str = "/StatusNotifierWatcher";
pub const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
/// The longest a call to the bus or the watcher may take.
const CALL_TIMEOUT_MS: i32 = 5_000;

const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";

/// Both interfaces, as the watcher and hosts introspect them. gio checks
/// every reply and property against these signatures.
const INTROSPECTION: &str = r#"<node>
  <interface name="org.kde.StatusNotifierItem">
    <method name="ContextMenu">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <method name="Activate">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <method name="SecondaryActivate">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <method name="Scroll">
      <arg name="delta" type="i" direction="in"/>
      <arg name="orientation" type="s" direction="in"/>
    </method>
    <method name="ProvideXdgActivationToken">
      <arg name="token" type="s" direction="in"/>
    </method>
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="WindowId" type="i" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconPixmap" type="a(iiay)" access="read"/>
    <property name="OverlayIconName" type="s" access="read"/>
    <property name="OverlayIconPixmap" type="a(iiay)" access="read"/>
    <property name="AttentionIconName" type="s" access="read"/>
    <property name="AttentionIconPixmap" type="a(iiay)" access="read"/>
    <property name="AttentionMovieName" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <signal name="NewTitle"/>
    <signal name="NewIcon"/>
    <signal name="NewAttentionIcon"/>
    <signal name="NewOverlayIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewStatus">
      <arg name="status" type="s"/>
    </signal>
  </interface>
  <interface name="com.canonical.dbusmenu">
    <method name="GetLayout">
      <arg name="parentId" type="i" direction="in"/>
      <arg name="recursionDepth" type="i" direction="in"/>
      <arg name="propertyNames" type="as" direction="in"/>
      <arg name="revision" type="u" direction="out"/>
      <arg name="layout" type="(ia{sv}av)" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg name="ids" type="ai" direction="in"/>
      <arg name="propertyNames" type="as" direction="in"/>
      <arg name="properties" type="a(ia{sv})" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg name="id" type="i" direction="in"/>
      <arg name="name" type="s" direction="in"/>
      <arg name="value" type="v" direction="out"/>
    </method>
    <method name="Event">
      <arg name="id" type="i" direction="in"/>
      <arg name="eventId" type="s" direction="in"/>
      <arg name="data" type="v" direction="in"/>
      <arg name="timestamp" type="u" direction="in"/>
    </method>
    <method name="EventGroup">
      <arg name="events" type="a(isvu)" direction="in"/>
      <arg name="idErrors" type="ai" direction="out"/>
    </method>
    <method name="AboutToShow">
      <arg name="id" type="i" direction="in"/>
      <arg name="needUpdate" type="b" direction="out"/>
    </method>
    <method name="AboutToShowGroup">
      <arg name="ids" type="ai" direction="in"/>
      <arg name="updatesNeeded" type="ai" direction="out"/>
      <arg name="idErrors" type="ai" direction="out"/>
    </method>
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <signal name="ItemsPropertiesUpdated">
      <arg name="updatedProps" type="a(ia{sv})"/>
      <arg name="removedProps" type="a(ias)"/>
    </signal>
    <signal name="LayoutUpdated">
      <arg name="revision" type="u"/>
      <arg name="parent" type="i"/>
    </signal>
    <signal name="ItemActivationRequested">
      <arg name="id" type="i"/>
      <arg name="timestamp" type="u"/>
    </signal>
  </interface>
</node>"#;

/// What the item reports about itself.
#[derive(Clone, Debug)]
pub struct ItemProps {
    pub id: String,
    pub title: String,
    pub icon_name: String,
    /// Empty when the embedded icon failed to decode.
    pub icon_pixmap: Vec<TrayIcon>,
}

/// The answer to one D-Bus call, and the actions it fires once sent.
#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub reply: Result<Option<glib::Variant>, (&'static str, String)>,
    pub actions: Vec<TrayAction>,
}

impl Outcome {
    fn ok(value: Option<glib::Variant>) -> Self {
        Self {
            reply: Ok(value),
            actions: Vec::new(),
        }
    }

    fn error(name: &'static str, message: impl Into<String>) -> Self {
        Self {
            reply: Err((name, message.into())),
            actions: Vec::new(),
        }
    }
}

/// The `a(iiay)` pixmaps, unwrapped: a `Variant` nested in a tuple
/// would be boxed as `v`.
fn pixmap_list(icons: &[TrayIcon]) -> Vec<(i32, i32, Vec<u8>)> {
    icons
        .iter()
        .map(|icon| (icon.width, icon.height, icon.data.clone()))
        .collect()
}

fn pixmaps(icons: &[TrayIcon]) -> glib::Variant {
    pixmap_list(icons).to_variant()
}

/// An `org.kde.StatusNotifierItem` property; `None` for one it lacks.
pub fn item_property(item: &ItemProps, name: &str) -> Option<glib::Variant> {
    Some(match name {
        "Category" => "ApplicationStatus".to_variant(),
        "Id" => item.id.to_variant(),
        "Title" => item.title.to_variant(),
        "Status" => "Active".to_variant(),
        "WindowId" => 0i32.to_variant(),
        "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH.to_string())
            .expect("MENU_PATH is a valid object path")
            .to_variant(),
        // Left-click activates; the menu is the right-click.
        "ItemIsMenu" => false.to_variant(),
        "IconName" => item.icon_name.to_variant(),
        "IconPixmap" => pixmaps(&item.icon_pixmap),
        "IconThemePath" | "OverlayIconName" | "AttentionIconName" | "AttentionMovieName" => {
            "".to_variant()
        }
        "OverlayIconPixmap" | "AttentionIconPixmap" => pixmaps(&[]),
        "ToolTip" => ("", pixmap_list(&[]), "", "").to_variant(),
        _ => return None,
    })
}

/// A call on the item. A left-click restores the window; the item draws
/// no menu of its own (`ContextMenu`), so the host shows [`MENU_PATH`].
pub fn item_call(method: &str, params: &glib::Variant) -> Outcome {
    match method {
        "Activate" if params.get::<(i32, i32)>().is_some() => Outcome {
            reply: Ok(None),
            actions: vec![TrayAction::ShowWindow],
        },
        "ContextMenu" => Outcome::error(UNKNOWN_METHOD, "the host shows the Menu property"),
        "SecondaryActivate" | "Scroll" | "ProvideXdgActivationToken" => Outcome::ok(None),
        _ => Outcome::error(UNKNOWN_METHOD, format!("no method {method}")),
    }
}

/// A `com.canonical.dbusmenu` property; `None` for one it lacks.
pub fn menu_property(name: &str) -> Option<glib::Variant> {
    Some(match name {
        "Version" => 3u32.to_variant(),
        "TextDirection" => "ltr".to_variant(),
        "Status" => "normal".to_variant(),
        "IconThemePath" => Vec::<String>::new().to_variant(),
        _ => return None,
    })
}

/// The tray menu as dbusmenu sees it: the root is id 0 and entry `i` of
/// the layout is id `i + 1`. It never changes, so its revision is 0.
#[derive(Clone, Debug)]
pub struct Menu {
    entries: Vec<MenuEntry>,
}

impl Menu {
    pub fn new(entries: Vec<MenuEntry>) -> Self {
        Self { entries }
    }

    fn entry(&self, id: i32) -> Option<Option<MenuEntry>> {
        match id {
            0 => Some(None),
            _ => usize::try_from(id - 1)
                .ok()
                .and_then(|index| self.entries.get(index))
                .map(|entry| Some(*entry)),
        }
    }

    fn ids(&self) -> impl Iterator<Item = i32> + '_ {
        (0..=self.entries.len()).map(|id| id as i32)
    }

    /// The item's properties, only those `filter` names unless it is
    /// empty. Defaults (a standard, enabled, visible item) are left out,
    /// as the spec allows.
    pub fn properties(&self, id: i32, filter: &[String]) -> Option<HashMap<String, glib::Variant>> {
        let all: Vec<(&str, glib::Variant)> = match self.entry(id)? {
            None => vec![("children-display", "submenu".to_variant())],
            Some(MenuEntry::Item(action)) => vec![("label", action.label().to_variant())],
            Some(MenuEntry::Separator) => vec![("type", "separator".to_variant())],
        };
        Some(
            all.into_iter()
                .filter(|(name, _)| filter.is_empty() || filter.iter().any(|f| f == name))
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        )
    }

    /// The `(ia{sv}av)` node for `id`, its children down to `depth`
    /// levels (`None`: all of them).
    pub fn layout(&self, id: i32, depth: Option<u32>, filter: &[String]) -> Option<glib::Variant> {
        let properties = self.properties(id, filter)?;
        let children: Vec<glib::Variant> = if id == 0 && depth != Some(0) {
            self.ids()
                .skip(1)
                .filter_map(|child| self.layout(child, depth.map(|d| d - 1), filter))
                .collect()
        } else {
            Vec::new()
        };
        Some((id, properties, children).to_variant())
    }

    /// `None` for an id that is not in the menu; otherwise what a click
    /// on it does, if anything.
    fn event(&self, id: i32, event_id: &str) -> Option<Option<TrayAction>> {
        let entry = self.entry(id)?;
        Some(match (entry, event_id) {
            (Some(MenuEntry::Item(action)), "clicked") => Some(action),
            _ => None,
        })
    }
}

/// A call on the menu.
pub fn menu_call(menu: &Menu, method: &str, params: &glib::Variant) -> Outcome {
    let bad_args = || Outcome::error(INVALID_ARGS, format!("bad arguments to {method}"));
    match method {
        "GetLayout" => {
            let Some((parent, depth, filter)) = params.get::<(i32, i32, Vec<String>)>() else {
                return bad_args();
            };
            match menu.layout(parent, u32::try_from(depth).ok(), &filter) {
                // Built field by field: `(0u32, layout)` would box the
                // layout as `v`.
                Some(layout) => Outcome::ok(Some(glib::Variant::tuple_from_iter([
                    0u32.to_variant(),
                    layout,
                ]))),
                None => Outcome::error(INVALID_ARGS, "parentId not found"),
            }
        }
        "GetGroupProperties" => {
            let Some((ids, filter)) = params.get::<(Vec<i32>, Vec<String>)>() else {
                return bad_args();
            };
            let ids = if ids.is_empty() {
                menu.ids().collect()
            } else {
                ids
            };
            let found: Vec<(i32, HashMap<String, glib::Variant>)> = ids
                .into_iter()
                .filter_map(|id| menu.properties(id, &filter).map(|props| (id, props)))
                .collect();
            Outcome::ok(Some((found,).to_variant()))
        }
        "GetProperty" => {
            let Some((id, name)) = params.get::<(i32, String)>() else {
                return bad_args();
            };
            let Some(props) = menu.properties(id, std::slice::from_ref(&name)) else {
                return Outcome::error(INVALID_ARGS, "id not found");
            };
            match props.get(&name) {
                Some(value) => Outcome::ok(Some((value.clone(),).to_variant())),
                None => Outcome::error(INVALID_ARGS, "property not found"),
            }
        }
        "Event" => {
            let Some((id, event_id, _data, _time)) =
                params.get::<(i32, String, glib::Variant, u32)>()
            else {
                return bad_args();
            };
            match menu.event(id, &event_id) {
                Some(action) => Outcome {
                    reply: Ok(None),
                    actions: action.into_iter().collect(),
                },
                None => Outcome::error(INVALID_ARGS, "id not found"),
            }
        }
        "EventGroup" => {
            let Some((events,)) = params.get::<(Vec<(i32, String, glib::Variant, u32)>,)>() else {
                return bad_args();
            };
            if events.is_empty() {
                return Outcome::error(INVALID_ARGS, "no events");
            }
            let mut actions = Vec::new();
            let mut not_found = Vec::new();
            for (id, event_id, _data, _time) in &events {
                match menu.event(*id, event_id) {
                    Some(action) => actions.extend(action),
                    None => not_found.push(*id),
                }
            }
            if not_found.len() == events.len() {
                return Outcome::error(INVALID_ARGS, "none of the ids are in the menu");
            }
            Outcome {
                reply: Ok(Some((not_found,).to_variant())),
                actions,
            }
        }
        // The menu never changes, so no update is ever needed.
        "AboutToShow" => {
            let Some((id,)) = params.get::<(i32,)>() else {
                return bad_args();
            };
            match menu.entry(id) {
                Some(_) => Outcome::ok(Some((false,).to_variant())),
                None => Outcome::error(INVALID_ARGS, "id not found"),
            }
        }
        "AboutToShowGroup" => {
            let Some((ids,)) = params.get::<(Vec<i32>,)>() else {
                return bad_args();
            };
            let not_found: Vec<i32> = ids
                .iter()
                .copied()
                .filter(|id| menu.entry(*id).is_none())
                .collect();
            if !ids.is_empty() && not_found.len() == ids.len() {
                return Outcome::error(INVALID_ARGS, "none of the ids are in the menu");
            }
            Outcome::ok(Some((Vec::<i32>::new(), not_found).to_variant()))
        }
        _ => Outcome::error(UNKNOWN_METHOD, format!("no method {method}")),
    }
}

/// The bus name the item owns: unique per process, in the form hosts
/// expect.
pub fn bus_name() -> String {
    format!("org.kde.StatusNotifierItem-{}-1", std::process::id())
}

/// Whether a tray host will show the item, shared with the close
/// handler: false until the watcher has taken the item, and again
/// whenever it goes away.
pub type Availability = Rc<Cell<bool>>;

/// The two objects and the bus name, registered on one connection. Drop
/// it to take the icon away.
pub struct Service {
    connection: gio::DBusConnection,
    registrations: Vec<gio::RegistrationId>,
    name: String,
    available: Availability,
    _watcher_owner: Option<gio::SignalSubscription>,
}

impl Service {
    /// Export the item and its menu on `connection` and own the item's
    /// bus name, then register with the watcher. The registration
    /// finishes on the main context: [`Service::availability`] turns true
    /// once a host will show the item. It is redone whenever a watcher
    /// appears on the bus, as when the desktop's panel restarts.
    pub fn start(
        connection: &gio::DBusConnection,
        item: ItemProps,
        menu: Menu,
        on_action: Rc<dyn Fn(TrayAction)>,
    ) -> Result<Self, glib::Error> {
        let node = gio::DBusNodeInfo::for_xml(INTROSPECTION)?;
        let interface = |name| {
            node.lookup_interface(name)
                .expect("INTROSPECTION declares both interfaces")
        };
        let mut service = Self {
            connection: connection.clone(),
            registrations: Vec::new(),
            name: bus_name(),
            available: Rc::new(Cell::new(false)),
            _watcher_owner: None,
        };

        let item = Rc::new(item);
        let item_props = item.clone();
        let item_action = on_action.clone();
        service.registrations.push(
            connection
                .register_object(ITEM_PATH, &interface(ITEM_INTERFACE))
                .method_call(move |_, _, _, _, method, params, invocation| {
                    answer(invocation, item_call(method, &params), &item_action);
                })
                .property(move |_, _, _, _, name| {
                    item_property(&item_props, name).unwrap_or_else(|| "".to_variant())
                })
                .build()?,
        );
        let menu = Rc::new(menu);
        service.registrations.push(
            connection
                .register_object(MENU_PATH, &interface(MENU_INTERFACE))
                .method_call(move |_, _, _, _, method, params, invocation| {
                    answer(invocation, menu_call(&menu, method, &params), &on_action);
                })
                .property(|_, _, _, _, name| menu_property(name).unwrap_or_else(|| "".to_variant()))
                .build()?,
        );

        // DBUS_NAME_FLAG_DO_NOT_QUEUE; 1 is DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER.
        let reply = connection.call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            Some(&(service.name.as_str(), 4u32).to_variant()),
            Some(glib::VariantTy::new("(u)").expect("a valid type")),
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
        )?;
        if reply.get::<(u32,)>() != Some((1,)) {
            return Err(glib::Error::new(
                gio::DBusError::Failed,
                &format!("{} is owned by another process", service.name),
            ));
        }

        // Subscribed before the first registration, so a watcher that
        // starts in between is not missed.
        let weak = connection.downgrade();
        let (name, available) = (service.name.clone(), service.available.clone());
        service._watcher_owner = Some(connection.subscribe_to_signal(
            Some("org.freedesktop.DBus"),
            Some("org.freedesktop.DBus"),
            Some("NameOwnerChanged"),
            Some("/org/freedesktop/DBus"),
            Some(WATCHER_NAME),
            gio::DBusSignalFlags::NONE,
            move |signal| {
                let Some(connection) = weak.upgrade() else {
                    return;
                };
                match signal.parameters.get::<(String, String, String)>() {
                    Some((_, _, new_owner)) if !new_owner.is_empty() => {
                        register(&connection, &name, &available);
                    }
                    _ => {
                        tracing::info!("tray watcher went away; close will quit");
                        available.set(false);
                    }
                }
            },
        ));
        register(connection, &service.name, &service.available);
        Ok(service)
    }

    pub fn availability(&self) -> Availability {
        self.available.clone()
    }

    pub fn bus_name(&self) -> &str {
        &self.name
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.available.set(false);
        for id in self.registrations.drain(..) {
            let _ = self.connection.unregister_object(id);
        }
        self.connection.call(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ReleaseName",
            Some(&(self.name.as_str(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}

/// Send the reply, then run what the call asked for.
fn answer(
    invocation: gio::DBusMethodInvocation,
    outcome: Outcome,
    on_action: &Rc<dyn Fn(TrayAction)>,
) {
    match outcome.reply {
        Ok(value) => invocation.return_value(value.as_ref()),
        Err((name, message)) => invocation.return_dbus_error(name, &message),
    }
    for action in outcome.actions {
        on_action(action);
    }
}

/// Hand the item to the watcher, then ask it whether any host shows
/// items. Never auto-starts a watcher: without one there is no tray.
fn register(connection: &gio::DBusConnection, name: &str, available: &Availability) {
    let weak = connection.downgrade();
    let available = available.clone();
    connection.call(
        Some(WATCHER_NAME),
        WATCHER_PATH,
        WATCHER_INTERFACE,
        "RegisterStatusNotifierItem",
        Some(&(name,).to_variant()),
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        CALL_TIMEOUT_MS,
        gio::Cancellable::NONE,
        move |result| {
            if let Err(err) = result {
                tracing::info!(error = %err, "no tray watcher; close will quit");
                available.set(false);
                return;
            }
            let Some(connection) = weak.upgrade() else {
                return;
            };
            connection.call(
                Some(WATCHER_NAME),
                WATCHER_PATH,
                "org.freedesktop.DBus.Properties",
                "Get",
                Some(&(WATCHER_INTERFACE, "IsStatusNotifierHostRegistered").to_variant()),
                Some(glib::VariantTy::new("(v)").expect("a valid type")),
                gio::DBusCallFlags::NO_AUTO_START,
                CALL_TIMEOUT_MS,
                gio::Cancellable::NONE,
                move |result| {
                    let host = result
                        .ok()
                        .and_then(|reply| reply.get::<(glib::Variant,)>())
                        .and_then(|(value,)| value.get::<bool>())
                        .unwrap_or(false);
                    if !host {
                        tracing::info!("no tray host shows the icon; close will quit");
                    }
                    available.set(host);
                },
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tray::tray_menu_layout;

    /// A dbusmenu layout node, `(ia{sv}av)`.
    type Node = (i32, HashMap<String, glib::Variant>, Vec<glib::Variant>);

    fn menu() -> Menu {
        Menu::new(tray_menu_layout())
    }

    fn none() -> Vec<String> {
        Vec::new()
    }

    fn ok_value(outcome: &Outcome) -> &glib::Variant {
        outcome
            .reply
            .as_ref()
            .expect("a reply")
            .as_ref()
            .expect("a value")
    }

    #[test]
    fn the_introspection_parses_with_both_interfaces() {
        let node = gio::DBusNodeInfo::for_xml(INTROSPECTION).unwrap();
        assert!(node.lookup_interface(ITEM_INTERFACE).is_some());
        assert!(node.lookup_interface(MENU_INTERFACE).is_some());
    }

    /// gio rejects a property whose value does not match its declared
    /// type, so every one must.
    #[test]
    fn every_declared_property_has_a_value_of_its_type() {
        let item = ItemProps {
            id: "id".into(),
            title: "t".into(),
            icon_name: "i".into(),
            icon_pixmap: vec![TrayIcon {
                width: 1,
                height: 1,
                data: vec![255, 1, 2, 3],
            }],
        };
        // The interface's own block of INTROSPECTION.
        let xml = |iface: &str| {
            let start = INTROSPECTION
                .find(&format!("<interface name=\"{iface}\""))
                .unwrap();
            let len = INTROSPECTION[start..].find("</interface>").unwrap();
            INTROSPECTION[start..start + len].to_string()
        };
        let props = |xml: &str| -> Vec<(String, String)> {
            xml.lines()
                .filter(|line| line.contains("<property"))
                .map(|line| {
                    let attr = |key: &str| {
                        let start = line.find(&format!("{key}=\"")).unwrap() + key.len() + 2;
                        line[start..][..line[start..].find('"').unwrap()].to_string()
                    };
                    (attr("name"), attr("type"))
                })
                .collect()
        };
        let item_props = props(&xml(ITEM_INTERFACE));
        assert_eq!(item_props.len(), 16);
        for (name, ty) in item_props {
            let value = item_property(&item, &name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(value.type_().as_str(), ty, "{name}");
        }
        let menu_props = props(&xml(MENU_INTERFACE));
        assert_eq!(menu_props.len(), 4);
        for (name, ty) in menu_props {
            let value = menu_property(&name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(value.type_().as_str(), ty, "{name}");
        }
    }

    #[test]
    fn the_item_names_its_menu_and_is_not_one_itself() {
        let item = ItemProps {
            id: "id".into(),
            title: "t".into(),
            icon_name: "i".into(),
            icon_pixmap: Vec::new(),
        };
        let menu_path = item_property(&item, "Menu").unwrap();
        assert_eq!(menu_path.str(), Some(MENU_PATH));
        assert_eq!(item_property(&item, "ItemIsMenu"), Some(false.to_variant()));
        assert_eq!(item_property(&item, "Nope"), None);
    }

    #[test]
    fn activate_shows_the_window_and_context_menu_is_refused() {
        let activate = item_call("Activate", &(3i32, 4i32).to_variant());
        assert_eq!(activate.reply, Ok(None));
        assert_eq!(activate.actions, vec![TrayAction::ShowWindow]);
        let context = item_call("ContextMenu", &(3i32, 4i32).to_variant());
        assert_eq!(context.reply.unwrap_err().0, UNKNOWN_METHOD);
        assert!(context.actions.is_empty());
        let scroll = item_call("Scroll", &(1i32, "vertical").to_variant());
        assert_eq!(scroll, Outcome::ok(None));
    }

    #[test]
    fn the_layout_is_the_root_with_the_four_entries() {
        let outcome = menu_call(&menu(), "GetLayout", &(0i32, -1i32, none()).to_variant());
        let (revision, (root, root_props, children)) = ok_value(&outcome)
            .get::<(u32, Node)>()
            .expect("(u(ia{sv}av))");
        assert_eq!((revision, root), (0, 0));
        assert_eq!(
            root_props.get("children-display"),
            Some(&"submenu".to_variant())
        );
        let children: Vec<(i32, HashMap<String, glib::Variant>)> = children
            .iter()
            .map(|child| {
                let (id, props, grandchildren) = child.get::<Node>().expect("(ia{sv}av)");
                assert!(grandchildren.is_empty());
                (id, props)
            })
            .collect();
        let ids: Vec<i32> = children.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_eq!(children[0].1["label"].str(), Some("Open Wayterm"));
        assert_eq!(children[1].1["label"].str(), Some("New Session"));
        assert_eq!(children[2].1["type"].str(), Some("separator"));
        assert_eq!(children[3].1["label"].str(), Some("Quit"));
    }

    #[test]
    fn layout_depth_and_filter_are_honoured() {
        let shallow = menu_call(&menu(), "GetLayout", &(0i32, 0i32, none()).to_variant());
        let (_, (_, _, children)) = ok_value(&shallow).get::<(u32, Node)>().unwrap();
        assert!(children.is_empty());

        let filter = vec!["type".to_string()];
        let filtered = menu_call(&menu(), "GetLayout", &(1i32, -1i32, filter).to_variant());
        let (_, (id, props, _)) = ok_value(&filtered).get::<(u32, Node)>().unwrap();
        assert_eq!(id, 1);
        assert!(props.is_empty(), "a standard item has no type property");

        let missing = menu_call(&menu(), "GetLayout", &(9i32, -1i32, none()).to_variant());
        assert_eq!(missing.reply.unwrap_err().0, INVALID_ARGS);
    }

    #[test]
    fn a_click_fires_the_entrys_action_and_nothing_else_does() {
        let event = |id: i32, event_id: &str| {
            menu_call(
                &menu(),
                "Event",
                &(id, event_id, 0i32.to_variant(), 0u32).to_variant(),
            )
        };
        assert_eq!(event(1, "clicked").actions, vec![TrayAction::ShowWindow]);
        assert_eq!(event(2, "clicked").actions, vec![TrayAction::NewSession]);
        assert_eq!(event(4, "clicked").actions, vec![TrayAction::Quit]);
        assert_eq!(event(4, "hovered"), Outcome::ok(None));
        assert_eq!(event(3, "clicked"), Outcome::ok(None));
        assert_eq!(event(0, "opened"), Outcome::ok(None));
        assert_eq!(event(5, "clicked").reply.unwrap_err().0, INVALID_ARGS);
    }

    #[test]
    fn an_event_group_reports_the_ids_it_lacks() {
        let events = vec![
            (9i32, "clicked".to_string(), 0i32.to_variant(), 0u32),
            (4i32, "clicked".to_string(), 0i32.to_variant(), 0u32),
        ];
        let outcome = menu_call(&menu(), "EventGroup", &(events,).to_variant());
        assert_eq!(outcome.actions, vec![TrayAction::Quit]);
        assert_eq!(ok_value(&outcome).get::<(Vec<i32>,)>(), Some((vec![9],)));

        let unknown = vec![(9i32, "clicked".to_string(), 0i32.to_variant(), 0u32)];
        let outcome = menu_call(&menu(), "EventGroup", &(unknown,).to_variant());
        assert_eq!(outcome.reply.unwrap_err().0, INVALID_ARGS);
        assert!(outcome.actions.is_empty());
    }

    #[test]
    fn properties_and_about_to_show() {
        let all = menu_call(
            &menu(),
            "GetGroupProperties",
            &(Vec::<i32>::new(), none()).to_variant(),
        );
        let (found,) = ok_value(&all)
            .get::<(Vec<(i32, HashMap<String, glib::Variant>)>,)>()
            .unwrap();
        assert_eq!(found.len(), 5);

        let label = menu_call(&menu(), "GetProperty", &(2i32, "label").to_variant());
        let (value,) = ok_value(&label).get::<(glib::Variant,)>().unwrap();
        assert_eq!(value.str(), Some("New Session"));
        let absent = menu_call(&menu(), "GetProperty", &(3i32, "label").to_variant());
        assert_eq!(absent.reply.unwrap_err().0, INVALID_ARGS);

        let show = menu_call(&menu(), "AboutToShow", &(0i32,).to_variant());
        assert_eq!(ok_value(&show).get::<(bool,)>(), Some((false,)));
        let group = menu_call(&menu(), "AboutToShowGroup", &(vec![0i32, 7],).to_variant());
        assert_eq!(
            ok_value(&group).get::<(Vec<i32>, Vec<i32>)>(),
            Some((vec![], vec![7]))
        );
    }

    #[test]
    fn bad_arguments_and_unknown_methods_are_errors() {
        let bad = menu_call(&menu(), "GetLayout", &(1i32,).to_variant());
        assert_eq!(bad.reply.unwrap_err().0, INVALID_ARGS);
        let unknown = menu_call(&menu(), "Nope", &().to_variant());
        assert_eq!(unknown.reply.unwrap_err().0, UNKNOWN_METHOD);
    }
}
