//! Installs the GNOME Files (Nautilus) menu item.
//!
//! The extension is a Python file that Nautilus loads from
//! `~/.local/share/nautilus-python/extensions`. A Flatpak cannot write there
//! unless the sandbox is granted that one directory, which the manifest does,
//! so the script is embedded in the binary and copied out on demand instead of
//! being symlinked (symlinks pointing into the sandbox do not work).

use std::fs;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "localsend_adwaita.py";
const EXTENSION: &str = include_str!("../data/nautilus/localsend_adwaita.py");

/// Where Nautilus picks extensions up for the current user.
///
/// The literal `~/.local/share` path comes first: inside the sandbox
/// `XDG_DATA_HOME` points at the app's own `~/.var/app/...` directory, which
/// Nautilus never reads, while the manifest grants access to the host path.
pub fn extensions_dir() -> PathBuf {
    binding_candidates()
        .into_iter()
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| home_dir().join(Path::new(HOST_DIR)))
}

/// The directory Nautilus reads on the host, relative to the home directory.
const HOST_DIR: &str = ".local/share/nautilus-python/extensions";

fn binding_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![home_dir().join(HOST_DIR)];
    candidates.push(
        gtk::glib::user_data_dir()
            .join("nautilus-python")
            .join("extensions"),
    );
    candidates.extend(
        gtk::glib::system_data_dirs()
            .iter()
            .map(|dir| dir.join("nautilus-python").join("extensions")),
    );
    candidates
}

/// The user's real home directory. GLib reports the per-app home
/// (`~/.var/app/<id>`) inside a Flatpak, while `$HOME` stays the host's.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(gtk::glib::home_dir)
}

pub fn extension_path() -> PathBuf {
    extensions_dir().join(FILE_NAME)
}

fn in_sandbox() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Copies the current script into place, updating it after an app update and
/// restoring it if it was deleted. Unchanged files are left alone.
pub fn install() -> Result<(), String> {
    let dir = extensions_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    let path = extension_path();
    if fs::read_to_string(&path).ok().as_deref() != Some(EXTENSION) {
        fs::write(&path, EXTENSION)
            .map_err(|e| format!("Could not write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Library directories that can hold the Nautilus Python shim. The host's are
/// included because a Flatpak has to report on the Files that runs outside it.
fn binding_roots() -> Vec<PathBuf> {
    const PREFIXES: [&str; 5] = [
        "/usr",
        "/usr/local",
        "/run/host/usr",
        "/run/host/usr/local",
        "/app",
    ];
    const LIBDIRS: [&str; 6] = [
        "lib",
        "lib64",
        "lib32",
        "lib/x86_64-linux-gnu",
        "lib/aarch64-linux-gnu",
        "lib/riscv64-linux-gnu",
    ];
    let mut roots = Vec::new();
    for prefix in PREFIXES {
        for libdir in LIBDIRS {
            roots.push(Path::new(prefix).join(libdir).join("nautilus"));
        }
    }
    roots
}
fn has_binding_shim(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("extensions-")
            && entry.path().join("libnautilus-python.so").is_file()
    })
}

/// Whether Nautilus has the Python bindings that load the menu item.
///
/// `None` means the answer cannot be known: a Flatpak cannot read the host's
/// libraries, and the bindings are installed on the host, not in the sandbox.
pub fn python_bindings_installed() -> Option<bool> {
    let roots = binding_roots();
    if in_sandbox() {
        let host_roots: Vec<&PathBuf> = roots
            .iter()
            .filter(|root| root.starts_with("/run/host"))
            .collect();
        if !host_roots.iter().any(|root| root.is_dir()) {
            return None;
        }
        return Some(host_roots.iter().any(|root| has_binding_shim(root)));
    }
    Some(roots.iter().any(|root| has_binding_shim(root)))
}

/// The command that installs the bindings on this distribution, if known.
pub fn install_command() -> Option<String> {
    // Inside a Flatpak, /etc belongs to the runtime; the host's copy lives
    // under /run/host.
    let contents = ["/run/host/etc/os-release", "/etc/os-release"]
        .iter()
        .find_map(|path| fs::read_to_string(path).ok())?;
    let id = os_release_value(&contents, "ID")?;
    let like = os_release_value(&contents, "ID_LIKE").unwrap_or_default();
    package_command(&id, &like)
}

fn os_release_value(contents: &str, key: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() != key {
            return None;
        }
        Some(value.trim().trim_matches(['"', '\'']).to_ascii_lowercase())
    })
}

/// Maps a distribution to its package for the Nautilus Python bindings.
fn package_command(id: &str, id_like: &str) -> Option<String> {
    let ids = std::iter::once(id).chain(id_like.split_whitespace());
    for candidate in ids {
        let command = match candidate {
            "fedora" | "rhel" | "centos" | "rocky" | "almalinux" => {
                "sudo dnf install nautilus-python"
            }
            "debian" | "ubuntu" | "linuxmint" | "pop" | "elementary" | "zorin" | "neon"
            | "kali" => "sudo apt install python3-nautilus",
            "arch" | "manjaro" | "endeavouros" | "garuda" => "sudo pacman -S python-nautilus",
            "opensuse" | "opensuse-tumbleweed" | "opensuse-leap" | "sles" => {
                "sudo zypper install nautilus-python"
            }
            "alpine" => "sudo apk add nautilus-python",
            "void" => "sudo xbps-install nautilus-python",
            "gentoo" => "sudo emerge gnome-extra/nautilus-python",
            _ => continue,
        };
        return Some(command.to_string());
    }
    None
}

pub fn uninstall() -> Result<(), String> {
    let path = extension_path();
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not remove {}: {error}", path.display())),
    }
}

/// Applies the saved preference at startup so the menu item matches the switch.
pub fn sync(enabled: bool) {
    let result = if enabled { install() } else { uninstall() };
    if let Err(error) = result {
        eprintln!("GNOME Files integration: {error}");
    }
}

/// Nautilus only loads extensions at startup. Only possible outside the
/// sandbox; a Flatpak tells the user to restart Files instead.
pub fn restart_files() -> bool {
    if in_sandbox() {
        return false;
    }
    std::process::Command::new("nautilus")
        .arg("-q")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_source_defines_the_menu_item() {
        assert!(EXTENSION.contains("Send with LocalSend"));
        assert!(EXTENSION.contains("files_integration"));
    }

    #[test]
    fn package_commands_follow_the_distribution() {
        assert_eq!(
            package_command("fedora", "").as_deref(),
            Some("sudo dnf install nautilus-python")
        );
        assert_eq!(
            package_command("ubuntu", "debian").as_deref(),
            Some("sudo apt install python3-nautilus")
        );
        // ID_LIKE is only consulted when the ID itself is unknown.
        assert_eq!(
            package_command("nobara", "fedora").as_deref(),
            Some("sudo dnf install nautilus-python")
        );
        assert_eq!(package_command("andes", ""), None);
        let release = "NAME=\"Fedora Linux\"\nID=fedora\nID_LIKE=\"rhel centos\"\n";
        assert_eq!(os_release_value(release, "ID").as_deref(), Some("fedora"));
        assert_eq!(
            os_release_value(release, "ID_LIKE").as_deref(),
            Some("rhel centos")
        );
    }
}
