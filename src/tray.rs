//! StatusNotifierItem tray icon.
//!
//! The icon is only published when a StatusNotifierWatcher is running on the
//! session bus (on GNOME that means the AppIndicator extension). Actions are
//! forwarded to the UI through the core event channel, so the tray can run on
//! its own thread.

use crate::core::{Core, Event};
use ksni::menu::StandardItem;
use ksni::{Handle, MenuItem, Tray, TrayMethods};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const APP_ICON: &str = "send-to";

static WINDOW_VISIBLE: AtomicBool = AtomicBool::new(true);
static HANDLE: OnceLock<Mutex<Option<Arc<Handle<LocalSendTray>>>>> = OnceLock::new();
static RUNTIME: OnceLock<Mutex<Option<tokio::runtime::Handle>>> = OnceLock::new();

fn handle_slot() -> &'static Mutex<Option<Arc<Handle<LocalSendTray>>>> {
    HANDLE.get_or_init(|| Mutex::new(None))
}

fn runtime_slot() -> &'static Mutex<Option<tokio::runtime::Handle>> {
    RUNTIME.get_or_init(|| Mutex::new(None))
}

struct LocalSendTray {
    core: Arc<Core>,
}

impl Tray for LocalSendTray {
    fn id(&self) -> String {
        "io.github.localsend_adwaita.LocalSend".to_string()
    }

    fn title(&self) -> String {
        "LocalSend".to_string()
    }

    fn icon_name(&self) -> String {
        APP_ICON.to_string()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        if WINDOW_VISIBLE.load(Ordering::SeqCst) {
            self.core.emit(Event::TrayHide);
        } else {
            self.core.emit(Event::TrayShow);
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let visible = WINDOW_VISIBLE.load(Ordering::SeqCst);
        vec![
            StandardItem {
                label: if visible {
                    "Hide LocalSend".to_string()
                } else {
                    "Show LocalSend".to_string()
                },
                activate: Box::new(|tray: &mut Self| {
                    if WINDOW_VISIBLE.load(Ordering::SeqCst) {
                        tray.core.emit(Event::TrayHide);
                    } else {
                        tray.core.emit(Event::TrayShow);
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    tray.core.emit(Event::TrayQuit);
                    // The receiver and discovery threads keep the process
                    // alive, which leaves a ghost entry in GNOME's background
                    // apps list, so quitting ends the process outright.
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(400));
                        std::process::exit(0);
                    });
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Keeps the tray menu label in sync with the window state.
pub fn set_window_visible(visible: bool) {
    WINDOW_VISIBLE.store(visible, Ordering::SeqCst);
    // `Handle::update` is async and needs the tray runtime, which runs on its
    // own thread, so the refresh is scheduled there.
    let handle = handle_slot().lock().ok().and_then(|slot| slot.clone());
    let runtime = runtime_slot().lock().ok().and_then(|slot| slot.clone());
    if let (Some(handle), Some(runtime)) = (handle, runtime) {
        runtime.spawn(async move {
            handle.update(|_| {}).await;
        });
    }
}

/// Starts the tray icon on its own runtime thread. `tray_active` is only set
/// once the item is published, so closing the window never strands the app.
pub fn spawn(core: Arc<Core>) -> Result<(), String> {
    if !core.config().tray_icon {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("localsend-tray".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("Tray runtime unavailable: {error}");
                    return;
                }
            };
            if let Ok(mut slot) = runtime_slot().lock() {
                *slot = Some(runtime.handle().clone());
            }
            runtime.block_on(async move {
                let tray = LocalSendTray { core: core.clone() }.disable_dbus_name(true);
                match tray.spawn().await {
                    Ok(handle) => {
                        if let Ok(mut slot) = handle_slot().lock() {
                            *slot = Some(Arc::new(handle));
                        }
                        core.set_tray_active(true);
                    }
                    Err(error) => {
                        eprintln!("Tray icon unavailable: {error}");
                        return;
                    }
                }
                // Keep the service running for the lifetime of the process.
                std::future::pending::<()>().await;
            });
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}
