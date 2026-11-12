//! Following the shell's working directory (doc 07 §Following the shell):
//! the directory comes from the shell's own reports, or from asking the
//! remote, and [`SftpView`](super::SftpView) says whether the listing
//! moves.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::glib;

use super::SidebarInner;
use crate::sftp::ShellCwdProbe;

/// Asks the remote where the tab's shell is, one probe at a time.
#[derive(Default)]
pub(super) struct ShellProbe {
    pub(super) probe: RefCell<Option<ShellCwdProbe>>,
    running: Cell<bool>,
    /// A request came while one ran; it is served when that ends.
    again: Cell<bool>,
}

impl SidebarInner {
    /// The shell is in `path` (`reported` over OSC 7, or probed): go there
    /// if following says so. Before a client is attached, the first
    /// listing starts there instead.
    pub(super) fn shell_seen(self: &Rc<Self>, path: &str, reported: bool) {
        let target = self.state.borrow_mut().shell_moved(path, reported);
        if let Some(target) = target {
            self.go_to_shell_dir(target);
        }
    }

    fn go_to_shell_dir(self: &Rc<Self>, path: String) {
        if self.client().is_some() {
            self.cd(path);
        } else {
            self.state.borrow_mut().start_at(&path);
            self.sync_path_entry();
        }
    }

    /// The checkbox changed (render setting it to the state is no change).
    /// Ticking it catches up with the shell and asks where it is now.
    pub(super) fn set_following(self: &Rc<Self>, on: bool) {
        if self.state.borrow().following == on {
            return;
        }
        let target = self.state.borrow_mut().set_following(on);
        if let Some(target) = target {
            self.go_to_shell_dir(target);
        }
        self.probe_shell();
    }

    /// Ask the remote where the shell is, unless following is off or the
    /// shell reports that itself.
    pub(super) fn probe_shell(self: &Rc<Self>) {
        if !self.state.borrow().wants_probe() {
            return;
        }
        let shell = &self.shell_probe;
        let Some(probe) = shell.probe.borrow().clone() else {
            return;
        };
        if shell.running.replace(true) {
            return shell.again.set(true);
        }
        let reply = probe.spawn();
        let sidebar = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let dir = reply.await.ok().flatten();
            sidebar.shell_probe.running.set(false);
            if let Some(dir) = dir {
                sidebar.shell_seen(&dir, false);
            }
            if sidebar.shell_probe.again.replace(false) {
                sidebar.probe_shell();
            }
        });
    }
}
