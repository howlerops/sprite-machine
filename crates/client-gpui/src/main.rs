//! The desktop app: opens the main window and runs until it closes.

fn main() {
    client_gpui::shell::run(client_gpui::shell::default_config());
}
