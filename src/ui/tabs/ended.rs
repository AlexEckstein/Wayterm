//! The ended-session prompt (doc 03 §Lifecycle): when the shell exits on
//! its own (`exit`, logout) nothing reconnects. The terminal stays with a
//! notice, and `r` reconnects or `q` closes the tab.

use std::cell::Cell;

use gtk4::prelude::*;
use gtk4::{gdk, glib};

/// Written into the terminal: attributes reset, on a line of its own.
pub const NOTICE: &str =
    "\r\n\x1b[0m\x1b[1mSession disconnected.\x1b[0m Press r to reconnect or q to close this tab.\r\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndedChoice {
    Reconnect,
    CloseTab,
}

/// `r` or `q`, either case. A chord (Ctrl, Alt, Super) is not a choice, so
/// copy and the other shortcuts keep working.
pub fn choice_for_key(key: gdk::Key, state: gdk::ModifierType) -> Option<EndedChoice> {
    let chord = gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::META_MASK;
    if state.intersects(chord) {
        return None;
    }
    match key {
        gdk::Key::r | gdk::Key::R => Some(EndedChoice::Reconnect),
        gdk::Key::q | gdk::Key::Q => Some(EndedChoice::CloseTab),
        _ => None,
    }
}

/// Call `on_choice` for the first choice typed into `terminal`. Capture
/// phase, so the terminal never sees the key; any other key passes through.
pub fn install(terminal: &gtk4::Widget, on_choice: impl Fn(EndedChoice) + 'static) {
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    // One choice per notice: the page swaps the terminal out right after.
    let chosen = Cell::new(false);
    keys.connect_key_pressed(move |_, key, _, state| match choice_for_key(key, state) {
        Some(choice) => {
            if !chosen.replace(true) {
                on_choice(choice);
            }
            glib::Propagation::Stop
        }
        None => glib::Propagation::Proceed,
    });
    terminal.add_controller(keys);
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: gdk::ModifierType = gdk::ModifierType::empty();

    #[test]
    fn r_reconnects_and_q_closes_in_either_case() {
        let shift = gdk::ModifierType::SHIFT_MASK;
        assert_eq!(
            choice_for_key(gdk::Key::r, NONE),
            Some(EndedChoice::Reconnect)
        );
        assert_eq!(
            choice_for_key(gdk::Key::R, shift),
            Some(EndedChoice::Reconnect)
        );
        assert_eq!(
            choice_for_key(gdk::Key::q, NONE),
            Some(EndedChoice::CloseTab)
        );
        assert_eq!(
            choice_for_key(gdk::Key::Q, shift),
            Some(EndedChoice::CloseTab)
        );
    }

    #[test]
    fn other_keys_and_chords_are_not_choices() {
        for key in [gdk::Key::Return, gdk::Key::space, gdk::Key::x] {
            assert_eq!(choice_for_key(key, NONE), None);
        }
        for chord in [
            gdk::ModifierType::CONTROL_MASK,
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
            gdk::ModifierType::ALT_MASK,
            gdk::ModifierType::SUPER_MASK,
        ] {
            assert_eq!(choice_for_key(gdk::Key::r, chord), None);
            assert_eq!(choice_for_key(gdk::Key::q, chord), None);
        }
    }

    #[test]
    fn the_notice_names_both_keys_on_a_fresh_line() {
        assert!(NOTICE.starts_with("\r\n\x1b[0m"));
        assert!(NOTICE.ends_with("\r\n"));
        assert!(NOTICE.contains("Session disconnected."));
        assert!(NOTICE.contains("r to reconnect"));
        assert!(NOTICE.contains("q to close this tab"));
    }
}
