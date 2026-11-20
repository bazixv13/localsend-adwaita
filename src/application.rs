use crate::core::{Core, Event};
use crate::window;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use libadwaita as adw;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const APP_ID: &str = "io.github.localsend_adwaita.LocalSend";
pub const RESOURCE_PATH: &str = "/io/github/localsend_adwaita/LocalSend";

/// Set when the file manager handed files to the app, so "Start in Background"
/// does not hide the window that was just opened to send them.
static OPENED_FILES: AtomicBool = AtomicBool::new(false);

pub fn run() -> glib::ExitCode {
    // Set up gettext before GTK reads the locale, so translated strings are
    // available as soon as the first label is built.
    crate::i18n::init();

    // Both the TLS server and client use the ring provider explicitly.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    app.connect_startup(|_| {
        register_resources();
        load_css();
    });

    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }

        let core = Arc::new(Core::new());
        let window = window::build(app, core.clone());

        if let Err(error) = crate::tray::spawn(core.clone()) {
            eprintln!("Tray icon unavailable: {error}");
        }

        if let Err(error) = crate::server::start(core.clone()) {
            core.emit(Event::ServerError(format!(
                "Receiving is unavailable: {error}"
            )));
        }
        crate::discovery::spawn(core.clone());

        // Multicast can be blocked on some networks. If nothing has appeared
        // shortly after launch, fall back to the HTTP sweep once.
        let scan_core = core.clone();
        glib::timeout_add_seconds_local_once(8, move || {
            if scan_core.devices_sorted().is_empty() {
                crate::discovery::scan(scan_core.clone());
            }
        });

        // With "Start in Background" the window only opens once the tray is
        // confirmed, so the app never hides itself with no way back.
        if core.config().start_in_background {
            let check_core = core.clone();
            let check_window = window.clone();
            glib::timeout_add_seconds_local_once(2, move || {
                if check_core.tray_active() && !OPENED_FILES.load(Ordering::SeqCst) {
                    check_window.set_visible(false);
                    crate::tray::set_window_visible(false);
                } else {
                    check_window.present();
                }
            });
        } else {
            window.present();
        }
    });

    app.connect_open(|app, files, _hint| {
        OPENED_FILES.store(true, Ordering::SeqCst);
        let paths: Vec<PathBuf> = files.iter().filter_map(|file| file.path()).collect();
        // A first launch can deliver "open" before "activate", so make sure
        // the window exists before handing it the files. This must happen
        // inside this call: with no window the application has nothing holding
        // it and quits as soon as the handler returns.
        if app.active_window().is_none() {
            app.activate();
        }
        window::queue_opened_files(paths);
    });

    let quit = gio::SimpleAction::new("quit", None);
    {
        let app = app.clone();
        quit.connect_activate(move |_, _| app.quit());
    }
    app.add_action(&quit);

    let show_pending = gio::SimpleAction::new("show-pending-text", None);
    {
        let app = app.clone();
        show_pending.connect_activate(move |_, _| {
            if let Some(window) = app.active_window() {
                window.present();
            }
            window::flush_pending_text();
        });
    }
    app.add_action(&show_pending);

    let accept = gio::SimpleAction::new("accept-transfer", Some(glib::VariantTy::STRING));
    accept.connect_activate(|_, parameter| {
        if let Some(session_id) = parameter.and_then(|value| value.str()) {
            window::resolve_incoming_transfer(true, session_id);
        }
    });
    app.add_action(&accept);

    let decline = gio::SimpleAction::new("decline-transfer", Some(glib::VariantTy::STRING));
    decline.connect_activate(|_, parameter| {
        if let Some(session_id) = parameter.and_then(|value| value.str()) {
            window::resolve_incoming_transfer(false, session_id);
        }
    });
    app.add_action(&decline);
    app.set_accels_for_action("app.quit", &["<Control>q"]);

    // Settings saves are coalesced, so make sure the last edit is written
    // before the process goes away.
    app.connect_shutdown(|_| crate::core::flush_config_save());

    app.run()
}

fn register_resources() {
    let bytes = glib::Bytes::from_static(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/resources.gresource"
    )));
    match gio::Resource::from_data(&bytes) {
        Ok(resource) => gio::resources_register(&resource),
        Err(error) => eprintln!("Could not load application resources: {error}"),
    }

    if let Some(display) = gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_resource_path(&format!("{RESOURCE_PATH}/icons"));
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(include_str!("../data/style.css"));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
