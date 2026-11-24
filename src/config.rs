use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::protocol::DEFAULT_PORT;

/// Settings are stored as JSON so the application works straight from a
/// build directory and inside a Flatpak sandbox without a GSettings schema
/// installation step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub alias: String,
    pub device_type: String,
    pub port: u16,
    pub pin: String,
    pub download_dir: PathBuf,
    pub encryption: bool,
    /// Random identifier used when transfers are unencrypted.
    pub fingerprint: String,
    /// Keep the app alive after the window is closed.
    pub run_in_background: bool,
    /// Show a system tray icon. The icon is only shown when a
    /// StatusNotifierWatcher is available on the session bus.
    pub tray_icon: bool,
    /// Accept incoming transfers immediately without opening the window. When
    /// off, each transfer waits for the user to accept or decline.
    pub quick_save: bool,
    /// Keep cached copies of received files in history for quick access.
    pub history_cache: bool,
    /// Start without showing the window when a tray icon is available.
    pub start_in_background: bool,
    /// Show "Send with LocalSend" in the GNOME Files context menu. Read by the
    /// optional Nautilus extension as well as by the app.
    pub files_integration: bool,
}

impl Default for Config {
    fn default() -> Self {
        let alias = default_alias();
        Self {
            alias,
            device_type: "desktop".to_string(),
            port: DEFAULT_PORT,
            pin: String::new(),
            download_dir: default_download_dir(),
            encryption: true,
            fingerprint: String::new(),
            run_in_background: true,
            tray_icon: true,
            quick_save: false,
            history_cache: true,
            start_in_background: false,
            files_integration: true,
        }
    }
}

fn default_alias() -> String {
    let hostname = gtk::glib::host_name();
    let hostname = hostname.trim();
    if hostname.is_empty() {
        "LocalSend".to_string()
    } else {
        hostname.to_string()
    }
}

fn default_download_dir() -> PathBuf {
    gtk::glib::user_special_dir(gtk::glib::UserDirectory::Downloads)
        .unwrap_or_else(|| gtk::glib::home_dir().join("Downloads"))
        .join("LocalSend")
}

pub fn config_dir() -> PathBuf {
    gtk::glib::user_config_dir().join("localsend-adwaita")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<Config>(&contents) {
                Ok(config) => config.normalized(),
                Err(error) => {
                    eprintln!("Could not parse {}: {error}", path.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    fn normalized(mut self) -> Self {
        if self.alias.trim().is_empty() {
            self.alias = default_alias();
        }
        if self.port < 1024 {
            self.port = DEFAULT_PORT;
        }
        if self.download_dir.as_os_str().is_empty() {
            self.download_dir = default_download_dir();
        }
        if !matches!(
            self.device_type.as_str(),
            "desktop" | "mobile" | "web" | "headless" | "server"
        ) {
            self.device_type = "desktop".to_string();
        }
        self.pin = self
            .pin
            .chars()
            .filter(|c| c.is_ascii_digit())
            .take(12)
            .collect();
        self
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        let path = config_path();
        let tmp = dir.join("config.json.tmp");
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&tmp, data).map_err(|e| format!("Could not write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .map_err(|e| format!("Could not replace {}: {e}", path.display()))?;
        Ok(())
    }

    pub fn ensure_download_dir(&self) -> Result<PathBuf, String> {
        fs::create_dir_all(&self.download_dir)
            .map_err(|e| format!("Could not create {}: {e}", self.download_dir.display()))?;
        Ok(self.download_dir.clone())
    }

    pub fn pin_required(&self) -> bool {
        !self.pin.is_empty()
    }

    pub fn download_dir_display(&self) -> String {
        display_path(&self.download_dir)
    }
}

pub fn display_path(path: &Path) -> String {
    if let Some(home) = gtk::glib::home_dir().to_str() {
        if let Ok(stripped) = path.strip_prefix(home) {
            let suffix = stripped.to_string_lossy();
            if suffix.is_empty() {
                return "Home".to_string();
            }
            return format!("~/{suffix}");
        }
    }
    path.to_string_lossy().into_owned()
}
