use crate::protocol::api_url;
use crate::tls::Identity;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use ureq::Agent;

#[derive(Debug)]
pub enum ApiError {
    Status(u16, String),
    Transport(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Status(code, body) => write!(f, "HTTP {code}{}", status_hint(*code, body)),
            ApiError::Transport(error) => write!(f, "{error}"),
        }
    }
}

fn status_hint(code: u16, body: &str) -> String {
    let hint = match code {
        401 => " — a PIN is required or was incorrect",
        403 => " — the transfer was declined",
        409 => " — another transfer is already in progress",
        422 => " — the file checksum did not match",
        429 => " — too many requests, try again shortly",
        _ => "",
    };
    let body = body.trim();
    if hint.is_empty() {
        if body.is_empty() {
            String::new()
        } else {
            format!(" ({body})")
        }
    } else {
        hint.to_string()
    }
}

pub fn client(
    protocol: &str,
    fingerprint: &str,
    identity: Option<&Identity>,
    timeout: Option<Duration>,
) -> Result<Agent, String> {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        // A receiver that stops reading (canceled transfer) closes the
        // connection; without a write timeout the sender would sit blocked
        // instead of noticing and stopping.
        .timeout_write(Duration::from_secs(15))
        .redirects(0);
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if protocol == "https" {
        let identity = identity.ok_or_else(|| "No local certificate is available".to_string())?;
        let config = crate::tls::client_config(identity, fingerprint)?;
        builder = builder.tls_config(Arc::new(config));
    }
    Ok(builder.build())
}

pub fn post_json<T: Serialize>(
    agent: &Agent,
    ip: &str,
    port: u16,
    protocol: &str,
    route: &str,
    value: &T,
    timeout: Option<Duration>,
) -> Result<String, ApiError> {
    let url = api_url(ip, port, protocol, route);
    let body = serde_json::to_string(value).map_err(|e| ApiError::Transport(e.to_string()))?;
    let mut request = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json");
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    match request.send_string(&body) {
        Ok(response) => response
            .into_string()
            .map_err(|e| ApiError::Transport(e.to_string())),
        Err(ureq::Error::Status(code, response)) => Err(ApiError::Status(
            code,
            response.into_string().unwrap_or_default(),
        )),
        Err(ureq::Error::Transport(error)) => Err(ApiError::Transport(error.to_string())),
    }
}

pub fn post_json_parse<T: DeserializeOwned>(
    agent: &Agent,
    ip: &str,
    port: u16,
    protocol: &str,
    route: &str,
    value: &impl Serialize,
    timeout: Option<Duration>,
) -> Result<T, ApiError> {
    let text = post_json(agent, ip, port, protocol, route, value, timeout)?;
    serde_json::from_str(&text).map_err(|e| ApiError::Transport(format!("Invalid response: {e}")))
}
