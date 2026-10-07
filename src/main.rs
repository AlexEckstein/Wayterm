use gtk4::glib;

mod app;
mod sftp;
mod store;
mod terminal;
mod transport;
mod ui;

fn main() -> glib::ExitCode {
    // Phase 2 seam: keep the transport public API compiled and lint-clean
    // until the GTK connection dialog wires it in Phase 3.
    transport::touch_public_api();
    ui::touch_dialog_api();
    sftp::touch_public_api();
    app::run()
}
