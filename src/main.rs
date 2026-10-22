//! Binary entry point: hands off to [`wayterm::app::run`].

fn main() -> gtk4::glib::ExitCode {
    wayterm::app::run()
}
