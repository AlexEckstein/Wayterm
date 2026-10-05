use gtk4::glib;

mod app;
mod store;
mod terminal;
mod transport;
mod ui;

fn main() -> glib::ExitCode {
    app::run()
}
