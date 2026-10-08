use gtk4::glib;

mod app;
mod hotkeys;
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
    ui::touch_sftp_api();
    sftp::touch_public_api();
    // Phase 4: the hotkey engine's data half (doc 07) is complete and
    // covered by its inline tests; `HotkeyEngine::install` and the
    // settings page are the UI task's, and the `#[allow(dead_code)]` on
    // hotkeys/mod.rs goes away with them. The module stays declared here
    // so `cargo test --bins` keeps running those tests.
    app::run()
}
