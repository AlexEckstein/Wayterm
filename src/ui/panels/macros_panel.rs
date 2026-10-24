//! The side dock's Macros panel (doc 12 §Panel): the saved macros with
//! Play, Edit and Delete, and Record. Recording and playback act on the
//! notebook's current tab, which needs a connected terminal; a recording
//! stays on the tab it started in.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;

use crate::macros::{InputTap, Macro, MacroStep, Playback, PlaybackEnd};
use crate::store::SessionStore;
use crate::ui::panels::macro_editor::{default_name, MacroEditor};
use crate::ui::panels::side_dock::empty_state;
use crate::ui::tabs::SessionTab;
use crate::ui::widgets::{confirm, set_data, set_margins, window_of, wrapped_label};

/// Root widget data key: the panel keeps itself alive with its widgets.
const PANEL_KEY: &str = "wayterm-macros-panel";
const EMPTY: &str = "empty";
const LIST: &str = "list";
const NEEDS_TERMINAL: &str = "Open a connected terminal tab (SSH, local shell or serial) first.";

/// The panel. Clones share it.
#[derive(Clone)]
pub struct MacrosPanel {
    inner: Rc<PanelInner>,
}

struct PanelInner {
    root: gtk4::Box,
    store: Rc<SessionStore>,
    notebook: gtk4::Notebook,
    stack: gtk4::Stack,
    list: gtk4::ListBox,
    record: IconButton,
    play: IconButton,
    edit: gtk4::Button,
    delete: gtk4::Button,
    status: gtk4::Label,
    /// The listed macros, in row order.
    macros: RefCell<Vec<Macro>>,
    /// The tap of the tab being recorded.
    recording: RefCell<Option<InputTap>>,
    playing: RefCell<Option<Playback>>,
}

impl MacrosPanel {
    /// The panel over `store`'s macros, acting on `notebook`'s current tab.
    pub fn new(store: Rc<SessionStore>, notebook: &gtk4::Notebook) -> Self {
        let record = IconButton::new("media-record-symbolic", "Record");
        record
            .button
            .set_tooltip_text(Some("Record what you type in the current terminal tab"));

        let status = wrapped_label("");
        status.add_css_class("dim-label");
        status.set_visible(false);

        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Single);
        // A click selects; only a double-click (or Enter) plays.
        list.set_activate_on_single_click(false);
        list.add_css_class("navigation-sidebar");
        let scrolled = gtk4::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .build();
        let stack = gtk4::Stack::new();
        stack.set_vexpand(true);
        stack.add_named(
            &empty_state(
                "wayterm-macro-symbolic",
                "No macros yet\nPress Record, type in a terminal tab, then press Stop.",
            ),
            Some(EMPTY),
        );
        stack.add_named(&scrolled, Some(LIST));

        let play = IconButton::new("media-playback-start-symbolic", "Play");
        play.button
            .set_tooltip_text(Some("Play the macro into the current terminal tab"));
        let edit = gtk4::Button::with_label("Edit");
        let delete = gtk4::Button::with_label("Delete");
        // Clicking leaves keyboard focus in the terminal being recorded
        // or played into.
        for button in [&record.button, &play.button, &edit, &delete] {
            button.set_focus_on_click(false);
        }
        let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        // Equal widths: Play turning into Stop moves nothing.
        toolbar.set_homogeneous(true);
        toolbar.append(&play.button);
        toolbar.append(&edit);
        toolbar.append(&delete);

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        set_margins(&root, 6);
        root.append(&record.button);
        root.append(&status);
        root.append(&stack);
        root.append(&toolbar);

        let inner = Rc::new(PanelInner {
            root,
            store,
            notebook: notebook.clone(),
            stack,
            list,
            record,
            play,
            edit,
            delete,
            status,
            macros: RefCell::default(),
            recording: RefCell::default(),
            playing: RefCell::default(),
        });
        inner.connect_signals();
        let panel = Self { inner };
        panel.refresh(None);
        set_data(&panel.inner.root, PANEL_KEY, panel.clone());
        panel
    }

    pub fn widget(&self) -> &gtk4::Widget {
        self.inner.root.upcast_ref()
    }

    /// Reload the list from the store, selecting `select` if listed.
    pub fn refresh(&self, select: Option<i64>) {
        self.inner.refresh(select);
    }

    /// Start recording the current tab; `false` (with a note) without a
    /// connected terminal.
    pub fn start_recording(&self) -> bool {
        self.inner.start_recording()
    }

    /// Stop recording and return the steps; `None` when not recording.
    pub fn stop_recording(&self) -> Option<Vec<MacroStep>> {
        self.inner.stop_recording()
    }

    /// Play the selected macro into the current tab, or stop the one
    /// playing.
    pub fn toggle_play(&self) {
        self.inner.toggle_play();
    }
}

impl PanelInner {
    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.record.button.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.toggle_record();
            }
        });
        let weak = Rc::downgrade(self);
        self.play.button.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.toggle_play();
            }
        });
        let weak = Rc::downgrade(self);
        self.edit.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.edit_selected();
            }
        });
        let weak = Rc::downgrade(self);
        self.delete.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.delete_selected();
            }
        });
        let weak = Rc::downgrade(self);
        self.list.connect_selected_rows_changed(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.update_buttons();
            }
        });
        // Double-click or Enter on a macro plays it.
        let weak = Rc::downgrade(self);
        self.list.connect_row_activated(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                if inner.playing.borrow().is_none() {
                    inner.toggle_play();
                }
            }
        });
    }

    fn refresh(&self, select: Option<i64>) {
        let macros = self.store.list_macros().unwrap_or_else(|err| {
            eprintln!("Wayterm: failed to load macros: {err}");
            Vec::new()
        });
        self.list.remove_all();
        for m in &macros {
            self.list.append(&macro_row(m));
        }
        let index = select.and_then(|id| macros.iter().position(|m| m.id == id));
        self.stack
            .set_visible_child_name(if macros.is_empty() { EMPTY } else { LIST });
        *self.macros.borrow_mut() = macros;
        if let Some(row) = index.and_then(|i| self.list.row_at_index(i as i32)) {
            self.list.select_row(Some(&row));
        }
        self.update_buttons();
    }

    fn selected(&self) -> Option<Macro> {
        let index = self.list.selected_row()?.index();
        self.macros
            .borrow()
            .get(usize::try_from(index).ok()?)
            .cloned()
    }

    fn update_buttons(&self) {
        let recording = self.recording.borrow().is_some();
        let playing = self.playing.borrow().is_some();
        let selected = self.selected().is_some();
        if recording {
            self.record.set("media-playback-stop-symbolic", "Stop");
            self.record.button.add_css_class("destructive-action");
        } else {
            self.record.set("media-record-symbolic", "Record");
            self.record.button.remove_css_class("destructive-action");
        }
        if playing {
            self.play.set("media-playback-stop-symbolic", "Stop");
        } else {
            self.play.set("media-playback-start-symbolic", "Play");
        }
        self.play.button.set_sensitive(playing || selected);
        self.edit.set_sensitive(selected);
        self.delete.set_sensitive(selected);
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
    }

    fn toggle_record(self: &Rc<Self>) {
        if self.recording.borrow().is_some() {
            self.finish_recording();
        } else {
            self.start_recording();
        }
    }

    fn start_recording(&self) -> bool {
        let Some(tap) = SessionTab::active_input_tap(&self.notebook) else {
            self.set_status(NEEDS_TERMINAL);
            return false;
        };
        tap.start_recording();
        *self.recording.borrow_mut() = Some(tap);
        self.set_status("Recording. Type in the terminal, then press Stop.");
        self.update_buttons();
        if let Some(terminal) = SessionTab::active_terminal(&self.notebook) {
            terminal.widget().grab_focus();
        }
        true
    }

    fn stop_recording(&self) -> Option<Vec<MacroStep>> {
        let tap = self.recording.borrow_mut().take()?;
        self.set_status("");
        self.update_buttons();
        tap.stop_recording()
    }

    /// Stop, and open what was recorded in the editor; Save stores it.
    fn finish_recording(&self) {
        let steps = self.stop_recording().unwrap_or_default();
        if steps.is_empty() {
            self.set_status("Nothing was recorded.");
            return;
        }
        let recorded = Macro {
            id: 0,
            name: default_name(&self.macros.borrow()),
            steps,
        };
        let parent = window_of(&self.root);
        let Some(m) = MacroEditor::new(parent.as_ref(), "New Macro", &recorded).run() else {
            return;
        };
        match self.store.insert_macro(&m) {
            Ok(id) => self.refresh(Some(id)),
            Err(err) => self.set_status(&format!("Could not save the macro: {err}")),
        }
    }

    fn toggle_play(self: &Rc<Self>) {
        // Taken first: `stop` reports back through `on_done`, which
        // clears the slot too.
        let playing = self.playing.borrow_mut().take();
        if let Some(playback) = playing {
            playback.stop();
            return;
        }
        let Some(m) = self.selected() else {
            return;
        };
        let Some(terminal) = SessionTab::active_terminal(&self.notebook) else {
            self.set_status(NEEDS_TERMINAL);
            return;
        };
        let weak = Rc::downgrade(self);
        let playback = Playback::start(
            &glib::MainContext::default(),
            m.steps,
            &terminal,
            move |end| {
                if let Some(inner) = weak.upgrade() {
                    inner.playing.borrow_mut().take();
                    inner.set_status(match end {
                        PlaybackEnd::TerminalGone => "Playback ended: the terminal closed.",
                        PlaybackEnd::Finished | PlaybackEnd::Stopped => "",
                    });
                    inner.update_buttons();
                }
            },
        );
        *self.playing.borrow_mut() = Some(playback);
        self.set_status(&format!("Playing “{}”…", m.name));
        self.update_buttons();
    }

    fn edit_selected(&self) {
        let Some(current) = self.selected() else {
            return;
        };
        let parent = window_of(&self.root);
        let Some(edited) = MacroEditor::new(parent.as_ref(), "Edit Macro", &current).run() else {
            return;
        };
        match self.store.update_macro(&edited) {
            Ok(()) => self.refresh(Some(edited.id)),
            Err(err) => self.set_status(&format!("Could not save the macro: {err}")),
        }
    }

    fn delete_selected(self: &Rc<Self>) {
        let Some(m) = self.selected() else {
            return;
        };
        let weak = Rc::downgrade(self);
        let parent = window_of(&self.root);
        confirm(
            parent.as_ref(),
            &format!("Delete “{}”?", m.name),
            "The macro is deleted for good.",
            "Delete",
            move || {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                match inner.store.delete_macro(m.id) {
                    Ok(()) => inner.refresh(None),
                    Err(err) => inner.set_status(&format!("Could not delete the macro: {err}")),
                }
            },
        );
    }
}

/// A macro's row: its name over its summary.
fn macro_row(m: &Macro) -> gtk4::Box {
    let name = gtk4::Label::new(Some(&m.name));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let detail = gtk4::Label::new(Some(&m.summary()));
    detail.set_xalign(0.0);
    detail.add_css_class("dim-label");
    detail.add_css_class("caption");
    let row = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    set_margins(&row, 6);
    row.append(&name);
    row.append(&detail);
    row
}

/// A button showing an icon and a label, both changeable.
struct IconButton {
    button: gtk4::Button,
    image: gtk4::Image,
    label: gtk4::Label,
}

impl IconButton {
    fn new(icon: &str, text: &str) -> Self {
        let image = gtk4::Image::from_icon_name(icon);
        let label = gtk4::Label::new(Some(text));
        let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        content.set_halign(gtk4::Align::Center);
        content.append(&image);
        content.append(&label);
        let button = gtk4::Button::new();
        button.set_child(Some(&content));
        Self {
            button,
            image,
            label,
        }
    }

    fn set(&self, icon: &str, text: &str) {
        self.image.set_icon_name(Some(icon));
        self.label.set_text(text);
    }
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(in crate::ui) mod gtk_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::terminal::{create_terminal, TerminalBackend, TerminalEvent, TerminalHandle};

    fn spin_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out");
            glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn drain(handle: &mut TerminalHandle) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(event) = handle.events.try_recv() {
            if let TerminalEvent::DataOut(bytes) = event {
                out.extend(bytes);
            }
        }
        out
    }

    fn step(delay_ms: u32, keys: &[u8]) -> MacroStep {
        MacroStep {
            delay_ms,
            keys: keys.to_vec(),
        }
    }

    /// The list comes from the store, by name; the toolbar follows the
    /// selection; without a connected terminal, Record and Play explain.
    pub(in crate::ui) fn panel_lists_macros_and_needs_a_terminal() {
        let store = Rc::new(SessionStore::in_memory().unwrap());
        let notebook = gtk4::Notebook::new();
        let panel = MacrosPanel::new(store.clone(), &notebook);
        let inner = &panel.inner;
        assert_eq!(inner.stack.visible_child_name().as_deref(), Some(EMPTY));

        for name in ["zeta", "Alpha"] {
            store
                .insert_macro(&Macro {
                    id: 0,
                    name: name.into(),
                    steps: vec![step(0, b"x")],
                })
                .unwrap();
        }
        panel.refresh(None);
        assert_eq!(inner.stack.visible_child_name().as_deref(), Some(LIST));
        let names: Vec<String> = inner
            .macros
            .borrow()
            .iter()
            .map(|m| m.name.clone())
            .collect();
        assert_eq!(names, ["Alpha", "zeta"]);
        assert!(!inner.play.button.is_sensitive() && !inner.edit.is_sensitive());
        assert!(
            !inner.list.activates_on_single_click(),
            "a click must not play"
        );

        let zeta = inner.macros.borrow()[1].id;
        panel.refresh(Some(zeta));
        assert_eq!(inner.selected().map(|m| m.name).as_deref(), Some("zeta"));
        assert!(inner.play.button.is_sensitive() && inner.delete.is_sensitive());

        assert!(!panel.start_recording());
        panel.toggle_play();
        assert_eq!(inner.status.text(), NEEDS_TERMINAL);
        assert!(inner.playing.borrow().is_none());
    }

    /// Record from a real VTE terminal (its input arrives through
    /// `send_input`, the keystroke path), then play a macro back into it:
    /// both legs reach the pump's end of the channel.
    pub(in crate::ui) fn panel_records_and_plays_through_the_terminal() {
        let store = Rc::new(SessionStore::in_memory().unwrap());
        let notebook = gtk4::Notebook::new();
        let page = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        notebook.append_page(&page, None::<&gtk4::Widget>);
        let terminal = create_terminal();
        let backend: Arc<dyn TerminalBackend> = terminal.backend.clone();
        let (mut pump_end, tap) = InputTap::wrap(terminal, &glib::MainContext::default());
        SessionTab::set_terminal_for_page(page.upcast_ref(), backend.clone(), tap);
        let panel = MacrosPanel::new(store.clone(), &notebook);

        assert!(panel.start_recording());
        assert_eq!(panel.inner.record.label.text(), "Stop");
        backend.send_input(b"ls");
        backend.send_input(b"\r");
        // Not UTF-8: skips VTE's `commit` but is still recorded, in order.
        backend.send_input(&[0xff]);
        let mut forwarded = Vec::new();
        spin_until(|| {
            forwarded.extend(drain(&mut pump_end));
            forwarded.len() == 4
        });
        assert_eq!(forwarded, b"ls\r\xff");
        let steps = panel.stop_recording().expect("was recording");
        let keys: Vec<u8> = steps.iter().flat_map(|s| s.keys.clone()).collect();
        assert_eq!(keys, b"ls\r\xff");
        assert_eq!(panel.inner.record.label.text(), "Record");

        let id = store
            .insert_macro(&Macro {
                id: 0,
                name: "uptime".into(),
                steps: vec![step(0, b"uptime"), step(20, b"\r")],
            })
            .unwrap();
        panel.refresh(Some(id));
        panel.toggle_play();
        assert_eq!(panel.inner.play.label.text(), "Stop");
        let mut played = Vec::new();
        // The last step goes out in the poll that ends playback; the
        // relay forwards it on a later iteration.
        spin_until(|| {
            played.extend(drain(&mut pump_end));
            panel.inner.playing.borrow().is_none() && played.len() >= 7
        });
        assert_eq!(played, b"uptime\r");
        assert_eq!(panel.inner.play.label.text(), "Play");
    }
}
