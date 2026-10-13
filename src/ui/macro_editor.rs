//! The macro editor (doc 12 §Editor): a macro's name and its steps — the
//! pause before each one and its keys in the key notation — with buttons
//! to insert, move and delete steps. It opens after every recording and
//! from the Macros panel's Edit. [`Draft`] is the display-free model.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use gtk4::prelude::*;

use crate::macros::model::{self, MacroStep};
use crate::macros::notation::{self, NotationError};
use crate::macros::Macro;
use crate::ui::widgets::{button_row, set_margins, wrapped_label, Modal};

/// One step being edited: the pause, and the keys as notation text.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DraftStep {
    pub delay_ms: u32,
    pub keys: String,
}

/// A macro being edited.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Draft {
    pub id: i64,
    pub name: String,
    pub steps: Vec<DraftStep>,
}

/// Why a draft cannot be saved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DraftError {
    NoName,
    /// Step `index` (0-based) has keys that do not parse.
    BadKeys {
        index: usize,
        error: NotationError,
    },
}

impl fmt::Display for DraftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoName => write!(f, "Give the macro a name."),
            Self::BadKeys { index, error } => write!(f, "Step {}: {error}", index + 1),
        }
    }
}

/// A structural edit, from one of a row's buttons.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepOp {
    /// A blank step at this index (the end at most).
    Insert(usize),
    Remove(usize),
    /// Swap this step with the one before it.
    MoveUp(usize),
    /// Swap this step with the one after it.
    MoveDown(usize),
}

impl Draft {
    pub fn from_macro(m: &Macro) -> Self {
        Self {
            id: m.id,
            name: m.name.clone(),
            steps: m
                .steps
                .iter()
                .map(|step| DraftStep {
                    delay_ms: step.delay_ms,
                    keys: notation::encode(&step.keys),
                })
                .collect(),
        }
    }

    /// Apply `op`; out-of-range ops change nothing.
    pub fn apply(&mut self, op: StepOp) {
        let len = self.steps.len();
        match op {
            StepOp::Insert(index) => self.steps.insert(index.min(len), DraftStep::default()),
            StepOp::Remove(index) if index < len => {
                self.steps.remove(index);
            }
            StepOp::MoveUp(index) if (1..len).contains(&index) => self.steps.swap(index - 1, index),
            StepOp::MoveDown(index) if index + 1 < len => self.steps.swap(index, index + 1),
            _ => {}
        }
    }

    /// The steps, if every one's keys parse.
    pub fn steps(&self) -> Result<Vec<MacroStep>, DraftError> {
        self.steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                notation::decode(&step.keys)
                    .map(|keys| MacroStep {
                        delay_ms: step.delay_ms,
                        keys,
                    })
                    .map_err(|error| DraftError::BadKeys { index, error })
            })
            .collect()
    }

    /// The macro to save: a trimmed, non-blank name and parsing keys.
    pub fn to_macro(&self) -> Result<Macro, DraftError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(DraftError::NoName);
        }
        Ok(Macro {
            id: self.id,
            name: name.to_string(),
            steps: self.steps()?,
        })
    }
}

/// `Macro N` for the lowest N no macro is named (case-insensitive).
pub fn default_name(existing: &[Macro]) -> String {
    (1..)
        .map(|n| format!("Macro {n}"))
        .find(|name| !existing.iter().any(|m| m.name.eq_ignore_ascii_case(name)))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GTK layer
// ---------------------------------------------------------------------------

const KEYS_HELP: &str = "Each step waits its pause, then sends its keys. Text is typed as \
    written; <Enter>, <Tab>, <Esc>, <Backspace>, <Up>/<Down>/<Left>/<Right>, <Home>, <End>, \
    <PageUp>, <PageDown>, <Insert>, <Delete>, <F1>–<F12>, <Ctrl-C> and <0x1B> name other \
    keys and bytes. Write <lt> for a literal \"<\".";

/// The editor window. Clones share it.
#[derive(Clone)]
pub struct MacroEditor {
    inner: Rc<EditorInner>,
}

struct EditorInner {
    modal: Modal<Macro>,
    name: gtk4::Entry,
    list: gtk4::ListBox,
    summary: gtk4::Label,
    error: gtk4::Label,
    save: gtk4::Button,
    draft: RefCell<Draft>,
    /// The rows' widgets, in step order (rebuilt with the rows).
    rows: RefCell<Vec<RowWidgets>>,
}

struct RowWidgets {
    pause: gtk4::SpinButton,
    keys: gtk4::Entry,
}

impl MacroEditor {
    /// An editor for `m`; `title` names what is being edited.
    pub fn new(parent: Option<&gtk4::Window>, title: &str, m: &Macro) -> Self {
        let modal = Modal::new(title, parent);
        modal.window.set_default_size(640, 520);

        let name = gtk4::Entry::builder()
            .text(&m.name)
            .hexpand(true)
            .activates_default(true)
            .build();
        let name_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        name_row.append(&gtk4::Label::new(Some("Name:")));
        name_row.append(&name);

        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.add_css_class("boxed-list");
        let scrolled = gtk4::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .min_content_height(240)
            .build();

        let add = gtk4::Button::with_label("Add Step");
        let summary = gtk4::Label::new(None);
        summary.add_css_class("dim-label");
        summary.set_hexpand(true);
        summary.set_xalign(1.0);
        let below = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        below.append(&add);
        below.append(&summary);

        let help = wrapped_label(KEYS_HELP);
        help.add_css_class("dim-label");
        let error = wrapped_label("");
        error.add_css_class("error");
        error.set_visible(false);

        let inner = Rc::new_cyclic(|weak: &std::rc::Weak<EditorInner>| {
            let weak = weak.clone();
            let save = modal.button("Save", move || {
                let inner = weak.upgrade()?;
                inner.commit_pauses();
                let saved = inner.draft.borrow().to_macro().ok();
                saved
            });
            EditorInner {
                modal,
                name,
                list,
                summary,
                error,
                save,
                draft: RefCell::new(Draft::from_macro(m)),
                rows: RefCell::default(),
            }
        });
        let cancel = inner.modal.button("Cancel", || None);
        inner.modal.window.set_default_widget(Some(&inner.save));

        let weak = Rc::downgrade(&inner);
        inner.name.connect_changed(move |entry| {
            if let Some(inner) = weak.upgrade() {
                inner.draft.borrow_mut().name = entry.text().to_string();
                inner.refresh();
            }
        });
        let weak = Rc::downgrade(&inner);
        add.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                let end = inner.draft.borrow().steps.len();
                inner.apply(StepOp::Insert(end));
            }
        });

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        set_margins(&vbox, 12);
        vbox.append(&name_row);
        vbox.append(&scrolled);
        vbox.append(&below);
        vbox.append(&help);
        vbox.append(&inner.error);
        vbox.append(&button_row(&[&cancel, &inner.save]));
        inner.modal.window.set_child(Some(&vbox));

        inner.render();
        Self { inner }
    }

    /// Run modally: the edited macro on Save, `None` on Cancel or close.
    /// The editor's state lives until the window closes: Save reads it.
    pub fn run(self) -> Option<Macro> {
        let modal = Modal {
            window: self.inner.modal.window.clone(),
            result: self.inner.modal.result.clone(),
        };
        modal.run()
    }
}

impl EditorInner {
    /// Apply a structural edit and rebuild the rows.
    fn apply(self: &Rc<Self>, op: StepOp) {
        self.commit_pauses();
        self.draft.borrow_mut().apply(op);
        self.render();
        if let StepOp::Insert(index) = op {
            if let Some(row) = self.rows.borrow().get(index) {
                row.keys.grab_focus();
            }
        }
    }

    /// A spin button takes typed digits only on Enter or focus-out; take
    /// them now.
    fn commit_pauses(&self) {
        for row in self.rows.borrow().iter() {
            row.pause.update();
        }
    }

    /// Rebuild every row from the draft.
    fn render(self: &Rc<Self>) {
        self.list.remove_all();
        self.rows.borrow_mut().clear();
        let steps = self.draft.borrow().steps.clone();
        for (index, step) in steps.iter().enumerate() {
            let (row, widgets) = self.build_row(index, steps.len(), step);
            self.list.append(&row);
            self.rows.borrow_mut().push(widgets);
        }
        self.refresh();
    }

    fn build_row(
        self: &Rc<Self>,
        index: usize,
        count: usize,
        step: &DraftStep,
    ) -> (gtk4::Box, RowWidgets) {
        let number = gtk4::Label::new(Some(&(index + 1).to_string()));
        number.add_css_class("dim-label");
        number.set_width_chars(3);
        number.set_xalign(1.0);

        let pause = gtk4::SpinButton::with_range(0.0, f64::from(u32::MAX), 10.0);
        pause.set_digits(0);
        pause.set_width_chars(7);
        pause.set_value(f64::from(step.delay_ms));
        pause.set_tooltip_text(Some("Pause before this step, in milliseconds"));
        let ms = gtk4::Label::new(Some("ms"));
        ms.add_css_class("dim-label");

        let keys = gtk4::Entry::builder()
            .text(&step.keys)
            .hexpand(true)
            .placeholder_text("Keys, e.g. ls<Enter>")
            .build();
        keys.add_css_class("monospace");
        keys.update_property(&[gtk4::accessible::Property::Label("Keys")]);
        mark_keys(&keys);

        let weak = Rc::downgrade(self);
        pause.connect_value_changed(move |spin| {
            if let Some(inner) = weak.upgrade() {
                if let Some(step) = inner.draft.borrow_mut().steps.get_mut(index) {
                    step.delay_ms = spin.value() as u32;
                }
                inner.refresh();
            }
        });
        let weak = Rc::downgrade(self);
        keys.connect_changed(move |entry| {
            mark_keys(entry);
            if let Some(inner) = weak.upgrade() {
                if let Some(step) = inner.draft.borrow_mut().steps.get_mut(index) {
                    step.keys = entry.text().to_string();
                }
                inner.refresh();
            }
        });

        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        set_margins(&row, 4);
        row.append(&number);
        row.append(&pause);
        row.append(&ms);
        row.append(&keys);
        for (icon, tooltip, op, enabled) in [
            (
                "list-add-symbolic",
                "Insert a step below",
                StepOp::Insert(index + 1),
                true,
            ),
            (
                "go-up-symbolic",
                "Move up",
                StepOp::MoveUp(index),
                index > 0,
            ),
            (
                "go-down-symbolic",
                "Move down",
                StepOp::MoveDown(index),
                index + 1 < count,
            ),
            (
                "user-trash-symbolic",
                "Delete step",
                StepOp::Remove(index),
                true,
            ),
        ] {
            let button = gtk4::Button::from_icon_name(icon);
            button.add_css_class("flat");
            button.set_tooltip_text(Some(tooltip));
            button.update_property(&[gtk4::accessible::Property::Label(tooltip)]);
            button.set_sensitive(enabled);
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(inner) = weak.upgrade() {
                    inner.apply(op);
                }
            });
            row.append(&button);
        }
        (row, RowWidgets { pause, keys })
    }

    /// Update the summary, the error line, and Save from the draft.
    fn refresh(&self) {
        let draft = self.draft.borrow();
        let checked = draft.to_macro();
        match &checked {
            Ok(m) => self.summary.set_text(&model::summary(&m.steps)),
            Err(_) => self.summary.set_text(""),
        }
        let error = checked.as_ref().err().map(ToString::to_string);
        self.error.set_text(error.as_deref().unwrap_or_default());
        self.error.set_visible(error.is_some());
        self.save.set_sensitive(checked.is_ok());
    }
}

/// Flag a keys entry whose text does not parse.
fn mark_keys(entry: &gtk4::Entry) {
    match notation::decode(&entry.text()) {
        Ok(_) => {
            entry.remove_css_class("error");
            entry.set_tooltip_text(None);
        }
        Err(error) => {
            entry.add_css_class("error");
            entry.set_tooltip_text(Some(&error.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn macro_with(keys: &[&[u8]]) -> Macro {
        Macro {
            id: 7,
            name: "deploy".into(),
            steps: keys
                .iter()
                .enumerate()
                .map(|(i, keys)| MacroStep {
                    delay_ms: i as u32 * 100,
                    keys: keys.to_vec(),
                })
                .collect(),
        }
    }

    fn keys(draft: &Draft) -> Vec<&str> {
        draft.steps.iter().map(|s| s.keys.as_str()).collect()
    }

    #[test]
    fn a_draft_roundtrips_its_macro() {
        let m = macro_with(&[b"l", b"\x1b[A", b"\r", &[0xff]]);
        let draft = Draft::from_macro(&m);
        assert_eq!(keys(&draft), ["l", "<Up>", "<Enter>", "<0xFF>"]);
        assert_eq!(draft.to_macro(), Ok(m));
    }

    #[test]
    fn steps_insert_move_and_remove() {
        let mut draft = Draft::from_macro(&macro_with(&[b"a", b"b", b"c"]));
        draft.apply(StepOp::MoveUp(2));
        assert_eq!(keys(&draft), ["a", "c", "b"]);
        draft.apply(StepOp::MoveDown(0));
        assert_eq!(keys(&draft), ["c", "a", "b"]);
        draft.apply(StepOp::Insert(1));
        assert_eq!(keys(&draft), ["c", "", "a", "b"]);
        assert_eq!(draft.steps[1], DraftStep::default());
        draft.apply(StepOp::Remove(0));
        assert_eq!(keys(&draft), ["", "a", "b"]);
        draft.apply(StepOp::Insert(99));
        assert_eq!(keys(&draft), ["", "a", "b", ""], "past the end appends");
    }

    #[test]
    fn out_of_range_moves_change_nothing() {
        let mut draft = Draft::from_macro(&macro_with(&[b"a", b"b"]));
        let before = draft.clone();
        for op in [
            StepOp::MoveUp(0),
            StepOp::MoveDown(1),
            StepOp::MoveUp(5),
            StepOp::Remove(2),
        ] {
            draft.apply(op);
        }
        assert_eq!(draft, before);
    }

    #[test]
    fn saving_needs_a_name_and_keys_that_parse() {
        let mut draft = Draft::from_macro(&macro_with(&[b"a", b"b"]));
        draft.name = "  ".into();
        assert_eq!(draft.to_macro(), Err(DraftError::NoName));
        draft.name = " deploy ".into();
        assert_eq!(draft.to_macro().unwrap().name, "deploy");
        draft.steps[1].keys = "<Entr>".into();
        let err = draft.to_macro().unwrap_err();
        assert_eq!(
            err,
            DraftError::BadKeys {
                index: 1,
                error: NotationError::UnknownKey("Entr".into())
            }
        );
        assert!(err.to_string().starts_with("Step 2: unknown key <Entr>"));
    }

    #[test]
    fn an_empty_keys_step_is_a_pure_pause() {
        let mut draft = Draft::from_macro(&macro_with(&[b"a"]));
        draft.apply(StepOp::Insert(1));
        draft.steps[1].delay_ms = 500;
        let steps = draft.to_macro().unwrap().steps;
        assert_eq!(
            steps[1],
            MacroStep {
                delay_ms: 500,
                keys: Vec::new()
            }
        );
    }

    #[test]
    fn default_names_take_the_lowest_free_number() {
        let named = |names: &[&str]| -> Vec<Macro> {
            names
                .iter()
                .map(|name| Macro {
                    name: (*name).into(),
                    ..Macro::default()
                })
                .collect()
        };
        assert_eq!(default_name(&[]), "Macro 1");
        assert_eq!(default_name(&named(&["macro 1", "Macro 3"])), "Macro 2");
    }
}

/// Widget checks; they need a display and run from `ui::gtk_tests`.
#[cfg(test)]
pub(super) mod gtk_tests {
    use super::*;

    fn entry_texts(editor: &MacroEditor) -> Vec<String> {
        editor
            .inner
            .rows
            .borrow()
            .iter()
            .map(|row| row.keys.text().to_string())
            .collect()
    }

    /// Rows mirror the steps; a bad key disables Save until fixed; the
    /// row buttons' edits rebuild the rows and reach the saved macro.
    pub(in crate::ui) fn editor_rows_follow_the_draft() {
        let m = Macro {
            id: 4,
            name: "deploy".into(),
            steps: vec![
                MacroStep {
                    delay_ms: 0,
                    keys: b"ls".to_vec(),
                },
                MacroStep {
                    delay_ms: 250,
                    keys: b"\r".to_vec(),
                },
            ],
        };
        let editor = MacroEditor::new(None, "Edit Macro", &m);
        let inner = &editor.inner;
        assert_eq!(entry_texts(&editor), ["ls", "<Enter>"]);
        assert_eq!(inner.rows.borrow()[1].pause.value(), 250.0);
        assert!(inner.save.is_sensitive());
        assert_eq!(inner.summary.text(), "2 steps · 0.2 s");

        let bad = inner.rows.borrow()[0].keys.clone();
        bad.set_text("ls <x");
        assert!(!inner.save.is_sensitive());
        assert!(inner.error.get_visible());
        assert!(bad.has_css_class("error"));
        bad.set_text("ls -la");
        assert!(inner.save.is_sensitive());
        assert!(!bad.has_css_class("error"));

        inner.apply(StepOp::MoveDown(0));
        assert_eq!(entry_texts(&editor), ["<Enter>", "ls -la"]);
        inner.rows.borrow()[0].pause.set_value(1500.0);
        inner.apply(StepOp::Insert(2));
        assert_eq!(entry_texts(&editor), ["<Enter>", "ls -la", ""]);
        inner.name.set_text("");
        assert!(!inner.save.is_sensitive(), "a name is required");
        inner.name.set_text("deploy 2");

        let saved = inner.draft.borrow().to_macro().unwrap();
        assert_eq!(saved.id, 4);
        assert_eq!(saved.name, "deploy 2");
        assert_eq!(
            saved.steps,
            [
                MacroStep {
                    delay_ms: 1500,
                    keys: b"\r".to_vec()
                },
                MacroStep {
                    delay_ms: 0,
                    keys: b"ls -la".to_vec()
                },
                MacroStep::default(),
            ]
        );
        inner.modal.window.destroy();
    }
}
