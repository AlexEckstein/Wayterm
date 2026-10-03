use gtk4::glib;

mod app;
mod store;
mod ui;

fn main() -> glib::ExitCode {
    app::run()
}
