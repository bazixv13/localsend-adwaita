mod application;
mod config;
mod core;
mod discovery;
mod history;
mod net;
mod protocol;
mod sender;
mod server;
mod tls;
mod tray;
mod util;
mod window;

fn main() -> gtk::glib::ExitCode {
    application::run()
}
