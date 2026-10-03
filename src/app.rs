use gtk4::glib;
use gtk4::prelude::*;

use crate::ui::MainWindow;

pub const APP_ID: &str = "com.alex-eckstein.wayterm";

pub fn run() -> glib::ExitCode {
    let app = gtk4::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &gtk4::Application) {
    MainWindow::new(app);
}
