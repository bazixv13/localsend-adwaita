use rand::RngCore;

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn initials(alias: &str) -> String {
    let mut parts = alias.split_whitespace();
    let first = parts.next().and_then(|part| part.chars().next());
    let second = parts.next().and_then(|part| part.chars().next());
    match (first, second) {
        (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "?".to_string(),
    }
}

/// A stable, human-scannable name for the connected interface.
pub fn device_type_label(device_type: &str) -> String {
    crate::i18n::t(match device_type {
        "mobile" => "Mobile",
        "web" => "Web browser",
        "headless" => "Headless device",
        "server" => "Server",
        _ => "Desktop",
    })
}

pub fn device_type_icon(device_type: &str) -> &'static str {
    match device_type {
        "mobile" => "phone-symbolic",
        "web" => "web-browser-symbolic",
        "headless" => "utilities-terminal-symbolic",
        "server" => "network-server-symbolic",
        _ => "computer-symbolic",
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    to_hex(&buffer)
}
