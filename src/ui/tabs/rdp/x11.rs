//! X11 embedding helpers for the RDP tab: find the area's XID, track the
//! xfreerdp child window, and keep it sized, mapped and focused. GTK
//! thread only; the x11rb connection never leaves it.

use gtk4::prelude::*;
use x11rb::rust_connection::RustConnection;

/// What the area's surface says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum XidState {
    /// Not mapped yet (or a zero XID): keep polling.
    Unmapped,
    /// Mapped but not X11 (Wayland-native GDK): show the choice view.
    Wayland,
    /// The host window id to spawn xfreerdp into.
    Ready(u32),
}

/// Read the host XID off a (possibly unmapped) area.
pub(super) fn surface_xid(area: &gtk4::DrawingArea) -> XidState {
    let Some(native) = area.native() else {
        return XidState::Unmapped;
    };
    let Some(surface) = native.surface() else {
        return XidState::Unmapped;
    };
    match surface.downcast::<gdk4_x11::X11Surface>() {
        Ok(x11) => match u32::try_from(x11.xid()) {
            Ok(xid) if xid != 0 => XidState::Ready(xid),
            _ => XidState::Unmapped,
        },
        Err(_) => XidState::Wayland,
    }
}

/// All current children of the host window; empty when the query fails.
pub(super) fn child_windows(conn: &RustConnection, host: u32) -> Vec<u32> {
    use x11rb::protocol::xproto::ConnectionExt;
    conn.query_tree(host)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|tree| tree.children)
        .unwrap_or_default()
}

/// Our freshly spawned child: the youngest window under `host` that was not
/// in `baseline` (the tree snapshot taken before OUR spawn). A pre-existing
/// child — GTK itself keeps at least one under the toplevel, and an orphan
/// from a killed previous run may linger — is never ours. `None` when
/// nothing new appeared yet.
pub(super) fn track_newcomer(baseline: &[u32], children: &[u32]) -> Option<u32> {
    children
        .iter()
        .rev()
        .find(|child| !baseline.contains(child))
        .copied()
}

/// Resize our tracked child to the area allocation at the area's
/// surface-relative origin; a zero child or size is a no-op. Errors are
/// ignored: the 250 ms poll retries.
pub(super) fn configure_child(
    conn: &RustConnection,
    child: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt};
    if child == 0 || width <= 0 || height <= 0 {
        return;
    }
    let aux = ConfigureWindowAux::new()
        .x(x)
        .y(y)
        .width(width as u32)
        .height(height as u32);
    let _ = conn
        .configure_window(child, &aux)
        .map(|cookie| cookie.check());
    let _ = conn.flush();
}

/// Map or unmap our tracked child. GTK knows nothing of the foreign
/// window, so when the area is hidden (another tab selected, the window
/// sent to the tray, the tab closed) the remote desktop would otherwise
/// stay drawn over whatever the toplevel shows next.
pub(super) fn set_child_mapped(conn: &RustConnection, child: u32, mapped: bool) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    let _ = if mapped {
        conn.map_window(child)
    } else {
        conn.unmap_window(child)
    };
    let _ = conn.flush();
}

/// The area's origin in toplevel-surface coordinates. The host XID comes
/// from `native().surface()` — the toplevel window, which the area shares —
/// so the child must sit at the translated origin, not at (0, 0).
pub(super) fn area_origin(area: &gtk4::DrawingArea) -> Option<(i32, i32)> {
    let root = area.root()?;
    let point = area.compute_point(&root, &gtk4::graphene::Point::new(0.0, 0.0))?;
    Some((point.x() as i32, point.y() as i32))
}

/// Hand X input focus to our child when focus sits on the parent window
/// (gtk owns it). Without this there is no keyboard under a WM. Never
/// steals focus from anywhere else; on teardown focus is left alone.
pub(super) fn hand_focus_to_child(conn: &RustConnection, host: u32, child: u32) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ConnectionExt, InputFocus};
    if child == 0 {
        return;
    }
    let Ok(cookie) = conn.get_input_focus() else {
        return;
    };
    let Ok(focus) = cookie.reply() else {
        return;
    };
    if focus.focus == host {
        let _ = conn.set_input_focus(InputFocus::PARENT, child, x11rb::CURRENT_TIME);
        let _ = conn.flush();
    }
}

/// The embed container: one X11-backed child area. No controllers — the
/// remote desktop consumes input, and Tier-2 shortcuts must not intercept
/// keys (doc 08 §Tabs).
pub(super) fn build_area() -> gtk4::DrawingArea {
    let area = gtk4::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_focusable(true);
    area.set_focus_on_click(true);
    area
}
