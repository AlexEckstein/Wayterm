//! The tray's D-Bus service on a private bus (doc 03 §Tray): the
//! handshake with a fake StatusNotifierWatcher, what a host reads, and the
//! clicks it sends. Each test runs its own `dbus-daemon` and skips itself
//! when there is none.
//!
//! The service answers on the test thread's main context, so the host's
//! calls come from a second thread while the test pumps that context.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{gio, glib};
use wayterm::ui::tray::{self, TrayAction, sni};

const WAIT: Duration = Duration::from_secs(5);

struct PrivateBus {
    daemon: Child,
    address: String,
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn private_bus() -> Option<PrivateBus> {
    let mut daemon = match Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(daemon) => daemon,
        Err(err) => {
            eprintln!("skipping: no dbus-daemon ({err})");
            return None;
        }
    };
    let mut address = String::new();
    BufReader::new(daemon.stdout.take()?)
        .read_line(&mut address)
        .ok()?;
    Some(PrivateBus {
        daemon,
        address: address.trim().to_string(),
    })
}

fn connect(address: &str) -> gio::DBusConnection {
    gio::DBusConnection::for_address_sync(
        address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
        gio::Cancellable::NONE,
    )
    .expect("connect to the private bus")
}

fn own_name(connection: &gio::DBusConnection, name: &str) {
    let reply = connection
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            Some(&(name, 4u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
        )
        .expect("RequestName");
    assert_eq!(reply.get::<(u32,)>(), Some((1,)));
}

const WATCHER_XML: &str = r#"<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem">
      <arg name="service" type="s" direction="in"/>
    </method>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
  </interface>
</node>"#;

/// A watcher on its own connection that records what registers with it.
struct FakeWatcher {
    connection: gio::DBusConnection,
    registered: Rc<RefCell<Vec<String>>>,
}

/// Leaving the bus drops the watcher's name, as a panel restart does.
impl Drop for FakeWatcher {
    fn drop(&mut self) {
        let _ = self.connection.close_sync(gio::Cancellable::NONE);
    }
}

fn fake_watcher(address: &str, host: bool) -> FakeWatcher {
    let connection = connect(address);
    let node = gio::DBusNodeInfo::for_xml(WATCHER_XML).unwrap();
    let interface = node.lookup_interface(sni::WATCHER_INTERFACE).unwrap();
    let registered: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = registered.clone();
    connection
        .register_object(sni::WATCHER_PATH, &interface)
        .method_call(move |_, _, _, _, _, params, invocation| {
            let (service,) = params.get::<(String,)>().expect("(s)");
            log.borrow_mut().push(service);
            invocation.return_value(None);
        })
        .property(move |_, _, _, _, _| host.to_variant())
        .build()
        .expect("export the watcher");
    own_name(&connection, sni::WATCHER_NAME);
    FakeWatcher {
        connection,
        registered,
    }
}

/// Pump `context` until `done` holds or [`WAIT`] passes.
fn pump_until(context: &glib::MainContext, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        if !context.iteration(false) {
            thread::sleep(Duration::from_millis(5));
        }
    }
    done()
}

/// Pump `context` for `time`, for an outcome that shows by not happening.
fn pump_for(context: &glib::MainContext, time: Duration) {
    let deadline = Instant::now() + time;
    pump_until(context, || Instant::now() >= deadline);
}

/// Run `host` on its own thread and connection, pumping the test's
/// context until it returns.
fn as_host<T: Send + 'static>(
    context: &glib::MainContext,
    address: &str,
    service: String,
    host: impl FnOnce(&HostCalls) -> T + Send + 'static,
) -> T {
    let address = address.to_string();
    let worker = thread::spawn(move || {
        host(&HostCalls {
            connection: connect(&address),
            service,
        })
    });
    assert!(
        pump_until(context, || worker.is_finished()),
        "host timed out"
    );
    worker.join().expect("the host thread")
}

struct HostCalls {
    connection: gio::DBusConnection,
    service: String,
}

impl HostCalls {
    fn call(
        &self,
        path: &str,
        interface: &str,
        method: &str,
        args: glib::Variant,
    ) -> Result<glib::Variant, glib::Error> {
        self.connection.call_sync(
            Some(&self.service),
            path,
            interface,
            method,
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            5_000,
            gio::Cancellable::NONE,
        )
    }

    fn item_property(&self, name: &str) -> glib::Variant {
        let reply = self
            .call(
                sni::ITEM_PATH,
                "org.freedesktop.DBus.Properties",
                "Get",
                (sni::ITEM_INTERFACE, name).to_variant(),
            )
            .unwrap_or_else(|err| panic!("Get {name}: {err}"));
        reply.get::<(glib::Variant,)>().unwrap().0
    }
}

/// A dbusmenu layout node, `(ia{sv}av)`.
type Node = (i32, HashMap<String, glib::Variant>, Vec<glib::Variant>);

type Clicks = Rc<RefCell<Vec<TrayAction>>>;

fn start(connection: &gio::DBusConnection) -> (tray::TrayHandle, Clicks) {
    let clicks: Clicks = Rc::default();
    let log = clicks.clone();
    let handle = tray::start_tray(connection, move |action| log.borrow_mut().push(action))
        .expect("the tray starts");
    (handle, clicks)
}

#[test]
fn a_host_sees_the_item_and_its_menu_and_clicks_reach_the_app() {
    let Some(bus) = private_bus() else { return };
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| {
            let watcher = fake_watcher(&bus.address, true);
            let (handle, clicks) = start(&connect(&bus.address));
            let available = handle.availability();
            assert!(pump_until(&context, || available.get()), "never available");
            assert_eq!(*watcher.registered.borrow(), vec![handle.bus_name()]);

            let service = handle.bus_name().to_string();
            let (properties, children, context_menu) =
                as_host(&context, &bus.address, service, |host| {
                    let properties: HashMap<&str, glib::Variant> = [
                        "Id",
                        "Title",
                        "IconName",
                        "Menu",
                        "ItemIsMenu",
                        "Status",
                        "IconPixmap",
                        "ToolTip",
                    ]
                    .into_iter()
                    .map(|name| (name, host.item_property(name)))
                    .collect();
                    let layout = host
                        .call(
                            sni::MENU_PATH,
                            sni::MENU_INTERFACE,
                            "GetLayout",
                            (0i32, -1i32, Vec::<String>::new()).to_variant(),
                        )
                        .expect("GetLayout");
                    let (_, (_, _, children)) = layout.get::<(u32, Node)>().expect("(u(ia{sv}av))");
                    let labels: Vec<Option<String>> = children
                        .iter()
                        .map(|child| {
                            let (_, props, _) = child.get::<Node>().unwrap();
                            props.get("label").and_then(|label| label.get::<String>())
                        })
                        .collect();
                    // Quit, then a left-click on the icon.
                    host.call(
                        sni::MENU_PATH,
                        sni::MENU_INTERFACE,
                        "Event",
                        (4i32, "clicked", 0i32.to_variant(), 0u32).to_variant(),
                    )
                    .expect("Event");
                    host.call(
                        sni::ITEM_PATH,
                        sni::ITEM_INTERFACE,
                        "Activate",
                        (0i32, 0i32).to_variant(),
                    )
                    .expect("Activate");
                    let context_menu = host.call(
                        sni::ITEM_PATH,
                        sni::ITEM_INTERFACE,
                        "ContextMenu",
                        (0i32, 0i32).to_variant(),
                    );
                    (
                        properties
                            .into_iter()
                            .map(|(k, v)| {
                                (k.to_string(), format!("{} {}", v.type_(), v.print(false)))
                            })
                            .collect::<HashMap<_, _>>(),
                        labels,
                        context_menu.is_err(),
                    )
                });

            // Each as `<D-Bus type> <value>`.
            let id = format!("s '{}'", wayterm::app::APP_ID);
            assert_eq!(properties["Id"], id);
            assert_eq!(properties["IconName"], id);
            assert_eq!(properties["Title"], "s 'Wayterm'");
            assert_eq!(properties["Menu"], format!("o '{}'", sni::MENU_PATH));
            assert_eq!(properties["ItemIsMenu"], "b false");
            assert_eq!(properties["Status"], "s 'Active'");
            assert!(
                properties["IconPixmap"].starts_with("a(iiay) [(64, 64, "),
                "{}",
                &properties["IconPixmap"][..60]
            );
            assert_eq!(properties["ToolTip"], "(sa(iiay)ss) ('', [], '', '')");
            assert_eq!(
                children,
                vec![
                    Some("Open Wayterm".into()),
                    Some("New Session".into()),
                    None,
                    Some("Quit".into()),
                ]
            );
            assert!(context_menu, "ContextMenu is refused");
            assert_eq!(
                *clicks.borrow(),
                vec![TrayAction::Quit, TrayAction::ShowWindow]
            );

            drop(handle);
            assert!(!available.get(), "dropping the handle takes the tray away");
        })
        .unwrap();
}

#[test]
fn without_a_watcher_the_tray_waits_and_registers_when_one_appears() {
    let Some(bus) = private_bus() else { return };
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| {
            let (handle, _clicks) = start(&connect(&bus.address));
            let available = handle.availability();
            // The registration fails fast: nothing auto-starts a watcher.
            pump_for(&context, Duration::from_millis(300));
            assert!(!available.get());

            let watcher = fake_watcher(&bus.address, true);
            assert!(
                pump_until(&context, || available.get()),
                "registers with the new watcher"
            );
            assert_eq!(*watcher.registered.borrow(), vec![handle.bus_name()]);

            drop(watcher);
            assert!(
                pump_until(&context, || !available.get()),
                "a watcher leaving takes the tray away"
            );
        })
        .unwrap();
}

#[test]
fn a_watcher_with_no_host_leaves_the_tray_unavailable() {
    let Some(bus) = private_bus() else { return };
    let context = glib::MainContext::new();
    context
        .with_thread_default(|| {
            let watcher = fake_watcher(&bus.address, false);
            let (handle, _clicks) = start(&connect(&bus.address));
            assert!(pump_until(&context, || !watcher
                .registered
                .borrow()
                .is_empty()));
            // Let the host query finish.
            let available = handle.availability();
            pump_for(&context, Duration::from_millis(300));
            assert!(!available.get());
        })
        .unwrap();
}
