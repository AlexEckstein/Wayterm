use gtk4::glib;

mod app;
mod store;

fn main() -> glib::ExitCode {
    app::run()
}
