//! Keystroke macros (doc 12): record what is typed into a terminal tab,
//! pauses included, edit it, and play it back into a terminal tab.
//!
//! - [`model`]: a [`Macro`] is a name and [`MacroStep`]s — keys plus the
//!   pause before them.
//! - [`recorder`]: timestamped input chunks in, steps out.
//! - [`notation`]: the editor's text form of the keys (`<Enter>`, `<Up>`).
//! - [`tap`]: [`InputTap`], the relay between a terminal and its pump that
//!   feeds an armed recording.
//! - [`playback`]: [`Playback`], replaying steps through
//!   `TerminalBackend::send_input`.

pub mod model;
pub mod notation;
pub mod playback;
pub mod recorder;
pub mod tap;

pub use model::{Macro, MacroStep};
pub use playback::{Playback, PlaybackEnd};
pub use recorder::Recorder;
pub use tap::InputTap;
