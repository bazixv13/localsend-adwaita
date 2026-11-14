//! LocalSend REST protocol v2.2 data types.
//!
//! All structures are deliberately tolerant when deserializing: peers may be
//! older (or newer) than us, and unknown or absent optional fields must not
//! break discovery or transfers.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub const PROTOCOL_VERSION: &str = "2.2";
pub const DEFAULT_PORT: u16 = 53317;
pub const MULTICAST_IP: [u8; 4] = [224, 0, 0, 167];
pub const MULTICAST_PORT: u16 = 53317;
pub const API_PREFIX: &str = "/api/localsend/v2";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceInfo {
    pub alias: String,
    pub version: String,
    pub device_model: Option<String>,
    pub device_type: Option<String>,
    pub fingerprint: String,
    pub port: Option<u16>,
    pub protocol: Option<String>,
    pub download: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce: Option<bool>,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            alias: String::new(),
            version: PROTOCOL_VERSION.to_string(),
            device_model: None,
            device_type: Some("desktop".to_string()),
            fingerprint: String::new(),
            port: None,
            protocol: None,
            download: Some(false),
            announce: None,
        }
    }
}

impl DeviceInfo {
    pub fn protocol(&self) -> &str {
        match self.protocol.as_deref() {
            Some("https") => "https",
            _ => "http",
        }
    }

    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    pub fn device_type(&self) -> &str {
        normalized_device_type(self.device_type.as_deref())
    }
}

pub fn normalized_device_type(value: Option<&str>) -> &'static str {
    match value.unwrap_or("desktop") {
        "mobile" => "mobile",
        "web" => "web",
        "headless" => "headless",
        "server" => "server",
        _ => "desktop",
    }
}

/// LocalSend sends chat messages as pseudo-files whose `fileType` is `text`.
/// The message itself is carried in `preview` (or, for older peers, the upload
/// body), so it must never be treated as a path on disk.
pub fn is_text_file(file_type: &str) -> bool {
    // `text` is LocalSend's message pseudo-file. A real `.txt` file uses the
    // MIME type `text/plain` and must still be saved normally.
    file_type.trim().eq_ignore_ascii_case("text")
}

/// Whether an announced file is a chat message rather than something to save.
///
/// Peers either mark the pseudo-file as `text` or label it `text/plain` while
/// putting the message in `preview`. A real text file has no preview, so only
/// text MIME types carrying a preview count as messages here.
pub fn is_message(file: &FileMetadata) -> bool {
    if is_text_file(&file.file_type) {
        return true;
    }
    file.file_type
        .trim()
        .to_ascii_lowercase()
        .starts_with("text/")
        && file
            .preview
            .as_deref()
            .is_some_and(|preview| !preview.is_empty())
}

/// The body of a chat message, either from `preview` or, for peers that only
/// set the pseudo-file name, from the file name itself.
pub fn message_body(file: &FileMetadata) -> Option<String> {
    match file.preview.as_deref() {
        Some(preview) if !preview.is_empty() => Some(preview.to_string()),
        _ if is_text_file(&file.file_type) && !file.file_name.is_empty() => {
            Some(file.file_name.clone())
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FileMetadata {
    pub id: String,
    pub file_name: String,
    pub size: u64,
    pub file_type: String,
    pub sha256: Option<String>,
    pub preview: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PrepareUploadRequest {
    pub info: DeviceInfo,
    pub files: HashMap<String, FileMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PrepareUploadResponse {
    pub session_id: String,
    pub files: HashMap<String, String>,
}

/// Builds the API URL for a peer or for our own routes.
pub fn api_url(ip: &str, port: u16, protocol: &str, route: &str) -> String {
    let host = if ip.contains(':') && !ip.starts_with('[') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    };
    let protocol = if protocol == "https" { "https" } else { "http" };
    format!("{protocol}://{host}:{port}{API_PREFIX}{route}")
}

/// Percent-encodes a query value. Only unreserved characters are kept.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Turns a sender-provided file name into a safe path relative to the
/// download directory. Folders are transmitted as file names containing `/`.
pub fn sanitize_relative_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    // Reject absolute paths and Windows drive prefixes outright.
    if name.starts_with('/') || name.starts_with('\\') {
        return None;
    }
    let mut path = PathBuf::new();
    for component in name.split(['/', '\\']) {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return None;
        }
        if component.ends_with(':') || component.chars().any(|c| c.is_control() || c == '\0') {
            return None;
        }
        path.push(component);
    }
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_file_names() {
        assert_eq!(
            sanitize_relative_path("holiday/photo.jpg"),
            Some(PathBuf::from("holiday/photo.jpg"))
        );
        assert_eq!(sanitize_relative_path("../escape"), None);
        assert_eq!(sanitize_relative_path("/etc/passwd"), None);
        assert_eq!(sanitize_relative_path("a/../../b"), None);
        assert_eq!(sanitize_relative_path("C:\\Windows\\evil"), None);
        assert_eq!(
            sanitize_relative_path("nested\\file.txt"),
            Some(PathBuf::from("nested/file.txt"))
        );
    }

    #[test]
    fn distinguishes_messages_from_text_files() {
        assert!(is_text_file("text"));
        assert!(is_text_file("TEXT"));
        assert!(!is_text_file("text/plain"));
        assert!(!is_text_file("image/png"));
    }

    #[test]
    fn parses_minimal_device_info() {
        let info: DeviceInfo =
            serde_json::from_str(r#"{"alias":"Phone","fingerprint":"abc"}"#).unwrap();
        assert_eq!(info.alias, "Phone");
        assert_eq!(info.protocol(), "http");
        assert_eq!(info.port_or_default(), DEFAULT_PORT);
    }
}
