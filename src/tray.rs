//! StatusNotifierItem tray icon.
//!
//! The icon is only published when a StatusNotifierWatcher is running on the
//! session bus (on GNOME that means the AppIndicator extension). Actions are
//! forwarded to the UI through the core event channel, so the tray can run on
//! its own thread.

use crate::core::{Core, Event};
use crate::i18n::t;
use ksni::menu::StandardItem;
use ksni::{Handle, MenuItem, Tray, TrayMethods};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const APP_ICON: &str = "send-to";

static WINDOW_VISIBLE: AtomicBool = AtomicBool::new(true);
/// Set while an item is being published. Publishing is asynchronous, so two
/// quick switch flips would otherwise register two items.
static STARTING: AtomicBool = AtomicBool::new(false);
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
                    t("Hide LocalSend")
                } else {
                    t("Show LocalSend")
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
                label: t("Quit"),
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

/// Shows or hides the tray icon while the app runs, so the preference needs
/// no restart.
pub fn set_enabled(core: &Arc<Core>, enabled: bool) {
    if enabled {
        if !core.tray_active() {
            if let Err(error) = spawn(core.clone()) {
                eprintln!("Tray icon unavailable: {error}");
            }
        }
    } else {
        hide(core);
    }
}

fn hide(core: &Core) {
    let handle = handle_slot().lock().ok().and_then(|mut slot| slot.take());
    // The runtime goes with the item. Keeping the handle around would make
    // `set_window_visible` schedule updates for a service that no longer
    // exists, and the next start installs a fresh one anyway.
    let runtime = runtime_slot().lock().ok().and_then(|mut slot| slot.take());
    if let (Some(handle), Some(runtime)) = (handle, runtime) {
        runtime.spawn(async move {
            let _ = handle.shutdown().await;
        });
    }
    core.set_tray_active(false);
}

/// Starts the tray icon on its own runtime thread. `tray_active` is only set
/// once the item is published, so closing the window never strands the app.
pub fn spawn(core: Arc<Core>) -> Result<(), String> {
    if !core.config().tray_icon {
        return Ok(());
    }
    // A second start would register a second item while the first is still
    // being published, which leaves two icons behind one switch.
    if STARTING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let publish_core = core.clone();
    let spawned = std::thread::Builder::new()
        .name("localsend-tray".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("Tray runtime unavailable: {error}");
                    STARTING.store(false, Ordering::SeqCst);
                    return;
                }
            };
            if let Ok(mut slot) = runtime_slot().lock() {
                *slot = Some(runtime.handle().clone());
            }
            runtime.block_on(async move {
                let tray = LocalSendTray {
                    core: publish_core.clone(),
                }
                .disable_dbus_name(true);
                let handle = match tray.spawn().await {
                    Ok(handle) => handle,
                    Err(error) => {
                        eprintln!("Tray icon unavailable: {error}");
                        STARTING.store(false, Ordering::SeqCst);
                        return;
                    }
                };
                // The switch can be turned back off while the item is being
                // published. Taking effect immediately means the icon must not
                // outlive that, which is what happened before: the icon stayed
                // until the app was restarted while the preference said off.
                if !publish_core.config().tray_icon {
                    let _ = handle.shutdown().await;
                    publish_core.set_tray_active(false);
                    STARTING.store(false, Ordering::SeqCst);
                    return;
                }
                if let Ok(mut slot) = handle_slot().lock() {
                    *slot = Some(Arc::new(handle));
                }
                publish_core.set_tray_active(true);
                STARTING.store(false, Ordering::SeqCst);
                // Keep the service running for the lifetime of the process.
                std::future::pending::<()>().await;
            });
        });
    if spawned.is_err() {
        STARTING.store(false, Ordering::SeqCst);
    }
    spawned.map(|_| ()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_core(tray_icon: bool) -> Arc<Core> {
        let config = Config {
            tray_icon,
            ..Config::default()
        };
        Arc::new(Core::for_test(config))
    }

    #[test]
    fn does_not_publish_when_the_preference_is_off() {
        let core = test_core(false);
        set_enabled(&core, true);
        assert!(!core.tray_active());
        // The publish guard must be released again, or the next switch-on
        // would be silently ignored.
        assert!(!STARTING.load(Ordering::SeqCst));
    }

    #[test]
    fn ignores_a_second_publish_while_one_is_in_flight() {
        let core = test_core(true);
        STARTING.store(true, Ordering::SeqCst);
        assert!(spawn(core.clone()).is_ok());
        assert!(!core.tray_active());
        STARTING.store(false, Ordering::SeqCst);
    }
}
