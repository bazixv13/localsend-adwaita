use crate::core::{Core, Event};
use crate::net;
use crate::protocol::{
    api_url, percent_encode, FileMetadata, PrepareUploadRequest, PrepareUploadResponse,
    DEFAULT_PORT,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

struct OutgoingFile {
    path: PathBuf,
    name: String,
    size: u64,
    mime: String,
}

/// Starts a transfer on a worker thread. `cancel` is shared with the UI.
pub fn spawn(
    core: Arc<Core>,
    device_key: String,
    paths: Vec<PathBuf>,
    pin: Option<String>,
    cancel: Arc<AtomicBool>,
) {
    let _ = thread::Builder::new()
        .name("localsend-sender".to_string())
        .spawn(move || send(core, device_key, paths, pin, cancel));
}

/// Sends a chat message using LocalSend's text pseudo-file convention.
pub fn spawn_text(
    core: Arc<Core>,
    device_key: String,
    text: String,
    pin: Option<String>,
    cancel: Arc<AtomicBool>,
) {
    let _ = thread::Builder::new()
        .name("localsend-text-sender".to_string())
        .spawn(move || send_text(core, device_key, text, pin, cancel));
}

fn send_text(
    core: Arc<Core>,
    device_key: String,
    text: String,
    pin: Option<String>,
    cancel: Arc<AtomicBool>,
) {
    if text.is_empty() {
        core.emit(Event::SendFailed {
            device: device_key,
            message: "The message is empty".to_string(),
        });
        return;
    }
    if cancel.load(Ordering::SeqCst) {
        core.emit(Event::SendCanceled);
        return;
    }
    let Some((info, ip)) = core.device(&device_key) else {
        core.emit(Event::SendFailed {
            device: device_key,
            message: "The device is no longer available".to_string(),
        });
        return;
    };
    let agent = match net::client(
        info.protocol(),
        &info.fingerprint,
        core.identity().as_deref(),
        None,
    ) {
        Ok(agent) => agent,
        Err(error) => {
            core.emit(Event::SendFailed {
                device: device_key,
                message: error,
            });
            return;
        }
    };
    let port = info.port.unwrap_or(DEFAULT_PORT);
    let protocol = info.protocol();
    // LocalSend peers recognise a chat message by this exact pseudo-file: the
    // `text` file type plus the message in `preview`. Naming it `message.txt`
    // matches what the reference implementation sends, so receivers show a
    // message instead of offering to save an attachment.
    let id = "text-0".to_string();
    let mut files = HashMap::new();
    files.insert(
        id.clone(),
        FileMetadata {
            id: id.clone(),
            file_name: "message.txt".to_string(),
            size: text.len() as u64,
            file_type: "text".to_string(),
            sha256: None,
            preview: Some(text.clone()),
            metadata: None,
        },
    );
    let request = PrepareUploadRequest {
        info: core.info(None),
        files,
    };
    let mut prepare_url = api_url(&ip, port, protocol, "/prepare-upload");
    if let Some(pin) = pin.as_deref().filter(|pin| !pin.is_empty()) {
        prepare_url.push_str(&format!("?pin={}", percent_encode(pin)));
    }
    let body = match serde_json::to_string(&request) {
        Ok(body) => body,
        Err(error) => {
            core.emit(Event::SendFailed {
                device: device_key,
                message: error.to_string(),
            });
            return;
        }
    };
    let response = agent
        .post(&prepare_url)
        .timeout(Duration::from_secs(120))
        .set("Content-Type", "application/json")
        .send_string(&body);
    let response = match response {
        Ok(response) => {
            if response.status() == 204 {
                core.emit(Event::SendTextFinished { device: device_key });
                return;
            }
            response
        }
        Err(ureq::Error::Status(401, _)) => {
            core.emit(Event::SendTextNeedsPin {
                device: device_key,
                text,
            });
            return;
        }
        Err(ureq::Error::Status(403, _)) => {
            core.emit(Event::SendDeclined { device: device_key });
            return;
        }
        Err(ureq::Error::Status(code, response)) => {
            let message = net::ApiError::Status(code, response.into_string().unwrap_or_default());
            core.emit(Event::SendFailed {
                device: device_key,
                message: message.to_string(),
            });
            return;
        }
        Err(ureq::Error::Transport(error)) => {
            emit_transport_failure(&core, device_key, error.to_string());
            return;
        }
    };

    let response_text = response.into_string().unwrap_or_default();
    let prepare: PrepareUploadResponse = match serde_json::from_str(&response_text) {
        Ok(prepare) => prepare,
        Err(error) => {
            core.emit(Event::SendFailed {
                device: device_key,
                message: format!("The device sent an invalid response: {error}"),
            });
            return;
        }
    };
    let Some(token) = prepare.files.get(&id).cloned() else {
        // A 200 response without a token means the receiver accepted the
        // message without needing an upload.
        core.emit(Event::SendTextFinished { device: device_key });
        return;
    };
    if cancel.load(Ordering::SeqCst) {
        cancel_session(&agent, &ip, port, protocol, &prepare.session_id);
        core.emit(Event::SendCanceled);
        return;
    }
    let upload_url = format!(
        "{}?sessionId={}&fileId={}&token={}",
        api_url(&ip, port, protocol, "/upload"),
        percent_encode(&prepare.session_id),
        percent_encode(&id),
        percent_encode(&token),
    );
    let _outgoing = OutgoingGuard::new(core.clone(), &prepare.session_id, cancel.clone());
    let result = agent
        .post(&upload_url)
        .set("Content-Type", "text/plain; charset=utf-8")
        .set("Content-Length", &text.len().to_string())
        .send_bytes(text.as_bytes());
    match result {
        Ok(_) => {
            core.emit(Event::SendTextFinished { device: device_key });
        }
        Err(error) => {
            if cancel.load(Ordering::SeqCst) {
                cancel_session(&agent, &ip, port, protocol, &prepare.session_id);
                core.emit(Event::SendCanceled);
                return;
            }
            match error {
                ureq::Error::Status(403, _) => {
                    cancel_session(&agent, &ip, port, protocol, &prepare.session_id);
                    core.emit(Event::SendDeclined { device: device_key });
                }
                ureq::Error::Status(code, response) => {
                    cancel_session(&agent, &ip, port, protocol, &prepare.session_id);
                    let message =
                        net::ApiError::Status(code, response.into_string().unwrap_or_default())
                            .to_string();
                    core.emit(Event::SendFailed {
                        device: device_key,
                        message,
                    });
                }
                ureq::Error::Transport(error) => {
                    cancel_session(&agent, &ip, port, protocol, &prepare.session_id);
                    emit_transport_failure(&core, device_key, error.to_string());
                }
            }
        }
    }
}

fn send(
    core: Arc<Core>,
    device_key: String,
    paths: Vec<PathBuf>,
    pin: Option<String>,
    cancel: Arc<AtomicBool>,
) {
    let Some((info, ip)) = core.device(&device_key) else {
        core.emit(Event::SendFailed {
            device: device_key,
            message: "The device is no longer available".to_string(),
        });
        return;
    };

    let files = match collect_files(&paths) {
        Ok(files) if !files.is_empty() => files,
        Ok(_) => {
            core.emit(Event::SendFailed {
                device: device_key,
                message: "No files were selected".to_string(),
            });
            return;
        }
        Err(error) => {
            core.emit(Event::SendFailed {
                device: device_key,
                message: error,
            });
            return;
        }
    };

    let total_bytes: u64 = files.iter().map(|file| file.size).sum();
    let file_count = files.len();

    let agent = match net::client(
        info.protocol(),
        &info.fingerprint,
        core.identity().as_deref(),
        None,
    ) {
        Ok(agent) => agent,
        Err(error) => {
            core.emit(Event::SendFailed {
                device: device_key,
                message: error,
            });
            return;
        }
    };
    let port = info.port.unwrap_or(DEFAULT_PORT);
    let protocol = info.protocol();

    let mut metadata_map = HashMap::new();
    let mut id_for_file = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let id = format!("file-{index}");
        metadata_map.insert(
            id.clone(),
            FileMetadata {
                id: id.clone(),
                file_name: file.name.clone(),
                size: file.size,
                file_type: file.mime.clone(),
                sha256: None,
                preview: None,
                metadata: None,
            },
        );
        id_for_file.push(id);
    }

    let request = PrepareUploadRequest {
        info: core.info(None),
        files: metadata_map,
    };

    let mut prepare_url = api_url(&ip, port, protocol, "/prepare-upload");
    if let Some(pin) = pin.as_deref().filter(|pin| !pin.is_empty()) {
        prepare_url.push_str(&format!("?pin={}", percent_encode(pin)));
    }

    let body = match serde_json::to_string(&request) {
        Ok(body) => body,
        Err(error) => {
            core.emit(Event::SendFailed {
                device: device_key,
                message: error.to_string(),
            });
            return;
        }
    };

    let response = agent
        .post(&prepare_url)
        .timeout(Duration::from_secs(120))
        .set("Content-Type", "application/json")
        .send_string(&body);

    let response = match response {
        Ok(response) => {
            if response.status() == 204 {
                core.emit(Event::SendFinished {
                    device: device_key,
                    files: 0,
                    total_bytes: 0,
                });
                return;
            }
            response
        }
        Err(ureq::Error::Status(401, _)) => {
            core.emit(Event::SendNeedsPin {
                device: device_key,
                paths,
            });
            return;
        }
        Err(ureq::Error::Status(403, _)) => {
            core.emit(Event::SendDeclined { device: device_key });
            return;
        }
        Err(ureq::Error::Status(code, response)) => {
            let message = net::ApiError::Status(code, response.into_string().unwrap_or_default());
            core.emit(Event::SendFailed {
                device: device_key,
                message: message.to_string(),
            });
            return;
        }
        Err(ureq::Error::Transport(error)) => {
            emit_transport_failure(&core, device_key, error.to_string());
            return;
        }
    };

    let text = response.into_string().unwrap_or_default();
    let prepare: PrepareUploadResponse = match serde_json::from_str(&text) {
        Ok(prepare) => prepare,
        Err(error) => {
            core.emit(Event::SendFailed {
                device: device_key,
                message: format!("The device sent an invalid response: {error}"),
            });
            return;
        }
    };

    let session_id = prepare.session_id.clone();
    let _outgoing = OutgoingGuard::new(core.clone(), &session_id, cancel.clone());
    let mut done_before = 0u64;
    for (index, file) in files.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            cancel_session(&agent, &ip, port, protocol, &session_id);
            core.emit(Event::SendCanceled);
            return;
        }
        let id = &id_for_file[index];
        let Some(token) = prepare.files.get(id) else {
            core.emit(Event::SendFailed {
                device: device_key,
                message: "The device rejected some of the files".to_string(),
            });
            cancel_session(&agent, &ip, port, protocol, &session_id);
            return;
        };
        let upload_url = format!(
            "{}?sessionId={}&fileId={}&token={}",
            api_url(&ip, port, protocol, "/upload"),
            percent_encode(&session_id),
            percent_encode(id),
            percent_encode(token),
        );

        let file_handle = match File::open(&file.path) {
            Ok(file) => file,
            Err(error) => {
                core.emit(Event::SendFailed {
                    device: device_key,
                    message: format!("Could not open {}: {error}", file.name),
                });
                cancel_session(&agent, &ip, port, protocol, &session_id);
                return;
            }
        };

        let reader = ProgressReader {
            inner: file_handle,
            core: core.clone(),
            file_name: file.name.clone(),
            file_bytes_read: 0,
            done_before,
            total: total_bytes,
            files_done: index,
            files_total: file_count,
            cancel: cancel.clone(),
            last_emit: Instant::now() - Duration::from_secs(1),
        };

        let result = agent
            .post(&upload_url)
            .set("Content-Type", "application/octet-stream")
            .set("Content-Length", &file.size.to_string())
            .send(reader);

        match result {
            Ok(_) => {
                done_before += file.size;
                core.emit(Event::SendProgress {
                    file_name: file.name.clone(),
                    done: done_before,
                    total: total_bytes,
                    files_done: index + 1,
                    files_total: file_count,
                });
            }
            Err(error) => {
                if cancel.load(Ordering::SeqCst) {
                    cancel_session(&agent, &ip, port, protocol, &session_id);
                    core.emit(Event::SendCanceled);
                    return;
                }
                match error {
                    ureq::Error::Status(403, _) => {
                        cancel_session(&agent, &ip, port, protocol, &session_id);
                        core.emit(Event::SendDeclined { device: device_key });
                    }
                    ureq::Error::Status(code, response) => {
                        cancel_session(&agent, &ip, port, protocol, &session_id);
                        let message =
                            net::ApiError::Status(code, response.into_string().unwrap_or_default())
                                .to_string();
                        core.emit(Event::SendFailed {
                            device: device_key,
                            message,
                        });
                    }
                    ureq::Error::Transport(error) => {
                        cancel_session(&agent, &ip, port, protocol, &session_id);
                        emit_transport_failure(&core, device_key, error.to_string());
                    }
                }
                return;
            }
        }
    }

    core.emit(Event::SendFinished {
        device: device_key,
        files: file_count,
        total_bytes,
    });
}

fn cancel_session(agent: &ureq::Agent, ip: &str, port: u16, protocol: &str, session_id: &str) {
    let url = format!(
        "{}?sessionId={}",
        api_url(ip, port, protocol, "/cancel"),
        percent_encode(session_id)
    );
    let _ = agent.post(&url).timeout(Duration::from_secs(3)).call();
}

/// Reports a transport failure. A refused connection means the peer is no
/// longer serving transfers, so hide it from discovery until it answers a
/// probe or registration again.
fn emit_transport_failure(core: &Core, device: String, message: String) {
    if message.to_ascii_lowercase().contains("connection refused") {
        core.mark_device_unreachable(&device);
        core.emit(Event::SendUnavailable {
            message: "The device is not accepting transfers".to_string(),
        });
    } else {
        core.emit(Event::SendFailed {
            device,
            message: friendly_transport(&message),
        });
    }
}

fn friendly_transport(message: &str) -> String {
    if message.contains("timed out") || message.contains("timeout") {
        "The connection timed out".to_string()
    } else if message.contains("Connection refused") {
        "The device is not accepting transfers".to_string()
    } else if message.contains("certificate") || message.contains("fingerprint") {
        format!("Secure connection failed: {message}")
    } else {
        message.to_string()
    }
}

/// Publishes the transfer's cancel flag while it runs, so a receiver that
/// gives up can stop the upload through `POST /api/localsend/v2/cancel`
/// instead of the sender streaming the rest of the file into the void.
struct OutgoingGuard {
    core: Arc<Core>,
    session_id: String,
}

impl OutgoingGuard {
    fn new(core: Arc<Core>, session_id: &str, cancel: Arc<AtomicBool>) -> Self {
        core.register_outgoing(session_id, cancel);
        Self {
            core,
            session_id: session_id.to_string(),
        }
    }
}

impl Drop for OutgoingGuard {
    fn drop(&mut self) {
        self.core.unregister_outgoing(&self.session_id);
    }
}

struct ProgressReader {
    inner: File,
    core: Arc<Core>,
    file_name: String,
    file_bytes_read: u64,
    done_before: u64,
    total: u64,
    files_done: usize,
    files_total: usize,
    cancel: Arc<AtomicBool>,
    last_emit: Instant,
}

impl Read for ProgressReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "transfer canceled",
            ));
        }
        let read = self.inner.read(buffer)?;
        self.file_bytes_read += read as u64;
        if self.last_emit.elapsed() >= Duration::from_millis(120) {
            self.last_emit = Instant::now();
            self.core.emit(Event::SendProgress {
                file_name: self.file_name.clone(),
                done: self.done_before + self.file_bytes_read,
                total: self.total,
                files_done: self.files_done,
                files_total: self.files_total,
            });
        }
        Ok(read)
    }
}

fn collect_files(paths: &[PathBuf]) -> Result<Vec<OutgoingFile>, String> {
    let mut out = Vec::new();
    for path in paths {
        if path.is_dir() {
            let base = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "folder".to_string());
            walk_directory(path, &base, &mut out)?;
        } else if path.is_file() {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            push_file(path.clone(), name, &mut out);
        }
    }
    Ok(out)
}

fn walk_directory(
    directory: &Path,
    prefix: &str,
    out: &mut Vec<OutgoingFile>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("Could not read {}: {error}", directory.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        let relative = format!("{prefix}/{name}");
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            walk_directory(&path, &relative, out)?;
        } else if file_type.is_file() {
            push_file(path, relative, out);
        }
    }
    Ok(())
}

fn push_file(path: PathBuf, name: String, out: &mut Vec<OutgoingFile>) {
    let Ok(metadata) = std::fs::metadata(&path) else {
        return;
    };
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    out.push(OutgoingFile {
        size: metadata.len(),
        path,
        name,
        mime,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::protocol::DeviceInfo;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    fn read_http(stream: &mut TcpStream) -> (String, Vec<u8>, bool, bool) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut content_length = None;
        let mut chunked = false;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim();
                if name == "content-length" {
                    content_length = value.parse::<usize>().ok();
                }
                if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
                    chunked = true;
                }
            }
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                let mut size_line = String::new();
                reader.read_line(&mut size_line).unwrap();
                let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                if size == 0 {
                    let mut crlf = [0u8; 2];
                    reader.read_exact(&mut crlf).unwrap();
                    break;
                }
                let mut chunk = vec![0u8; size];
                reader.read_exact(&mut chunk).unwrap();
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf).unwrap();
                body.extend_from_slice(&chunk);
            }
        } else if let Some(length) = content_length {
            body.resize(length, 0);
            reader.read_exact(&mut body).unwrap();
        }
        (request_line, body, content_length.is_some(), chunked)
    }

    fn mock_receiver() -> (u16, mpsc::Receiver<Vec<u8>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut uploads = 0;
            while uploads < 1 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let (request_line, body, has_length, chunked) = read_http(&mut stream);
                        if request_line.contains("/prepare-upload") {
                            let response =
                                r#"{"sessionId":"test-session","files":{"file-0":"test-token"}}"#;
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                response.len(),
                                response
                            )
                            .unwrap();
                        } else if request_line.contains("/upload") {
                            assert!(has_length, "upload was not sent with Content-Length");
                            assert!(
                                !chunked,
                                "upload was sent chunked instead of with Content-Length"
                            );
                            assert!(request_line.contains("sessionId=test-session"));
                            assert!(request_line.contains("fileId=file-0"));
                            assert!(request_line.contains("token=test-token"));
                            sender.send(body).unwrap();
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                            .unwrap();
                            uploads += 1;
                        } else {
                            write!(
                                stream,
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                            .unwrap();
                        }
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20))
                    }
                    Err(_) => break,
                }
            }
        });
        (port, receiver, handle)
    }

    #[test]
    fn sends_a_file_over_https_to_this_receiver() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.key_pair.serialize_der();
        let fingerprint = crate::tls::sha256_hex(&cert_der);
        let receiver_identity = Arc::new(crate::tls::Identity {
            cert_der,
            key_der,
            fingerprint,
        });

        let sender_certified =
            rcgen::generate_simple_self_signed(vec!["sender.local".to_string()]).unwrap();
        let sender_cert_der = sender_certified.cert.der().to_vec();
        let sender_key_der = sender_certified.key_pair.serialize_der();
        let sender_fingerprint = crate::tls::sha256_hex(&sender_cert_der);
        let sender_identity = Arc::new(crate::tls::Identity {
            cert_der: sender_cert_der,
            key_der: sender_key_der,
            fingerprint: sender_fingerprint,
        });

        let receive_dir =
            std::env::temp_dir().join(format!("localsend-tls-receive-{}", uuid::Uuid::new_v4()));
        let send_dir =
            std::env::temp_dir().join(format!("localsend-tls-send-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&receive_dir).unwrap();
        std::fs::create_dir_all(&send_dir).unwrap();

        let mut receiver_config = Config::default();
        receiver_config.alias = "TLS Receiver".to_string();
        receiver_config.fingerprint = "receiver-http-fingerprint".to_string();
        receiver_config.encryption = true;
        receiver_config.pin.clear();
        receiver_config.port = 0;
        receiver_config.download_dir = receive_dir.clone();
        let receiver_core = Arc::new(Core::for_test_with_identity(
            receiver_config,
            receiver_identity.clone(),
        ));
        crate::server::start(receiver_core.clone()).expect("the TLS receiver should start");

        let deadline = Instant::now() + Duration::from_secs(10);
        while receiver_core.port() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        let port = receiver_core.port();
        assert_ne!(port, 0, "the receiver did not bind a port");
        assert_eq!(receiver_core.protocol(), "https");

        let receiver_events = receiver_core.subscribe();
        let (received_tx, received_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(event) = receiver_events.recv_blocking() {
                match event {
                    Event::IncomingRequest(_) => {}
                    Event::ReceiveFinished { .. } => {
                        let _ = received_tx.send(());
                    }
                    _ => {}
                }
            }
        });

        let mut sender_config = Config::default();
        sender_config.alias = "TLS Sender".to_string();
        sender_config.fingerprint = "sender-http-fingerprint".to_string();
        sender_config.encryption = false;
        let sender_core = Arc::new(Core::for_test_with_identity(
            sender_config,
            sender_identity.clone(),
        ));

        let (sent_tx, sent_rx) = mpsc::channel();
        let (failed_tx, failed_rx) = mpsc::channel();
        let sender_events = sender_core.subscribe();
        thread::spawn(move || {
            while let Ok(event) = sender_events.recv_blocking() {
                match event {
                    Event::SendFinished { .. } => {
                        let _ = sent_tx.send(());
                    }
                    Event::SendFailed { message, .. } => {
                        let _ = failed_tx.send(message);
                    }
                    _ => {}
                }
            }
        });

        let mut info = DeviceInfo::default();
        info.alias = "TLS Receiver".to_string();
        info.fingerprint = receiver_identity.fingerprint.clone();
        info.port = Some(port);
        info.protocol = Some("https".to_string());
        info.download = Some(false);
        sender_core.upsert_device(info, "127.0.0.1".to_string());
        let key = sender_core
            .devices_sorted()
            .into_iter()
            .find(|device| device.alias == "TLS Receiver")
            .expect("device was not registered")
            .key;

        let path = send_dir.join("hello.txt");
        let contents = b"secure hello".to_vec();
        std::fs::write(&path, &contents).unwrap();
        send(
            sender_core.clone(),
            key,
            vec![path],
            None,
            Arc::new(AtomicBool::new(false)),
        );

        if sent_rx.recv_timeout(Duration::from_secs(20)).is_err() {
            let message = failed_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or_else(|_| "the sender produced no result".to_string());
            panic!("the HTTPS send failed: {message}");
        }
        received_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the HTTPS receive did not finish");
        let saved = std::fs::read(receive_dir.join("hello.txt")).unwrap();
        assert_eq!(saved, contents);

        let _ = std::fs::remove_dir_all(&receive_dir);
        let _ = std::fs::remove_dir_all(&send_dir);
    }

    #[test]
    #[ignore = "requires a live LocalSend peer; set LOCALSEND_LIVE_IP, _PORT, _FINGERPRINT and _FILE"]
    fn sends_to_a_live_peer() {
        let ip = std::env::var("LOCALSEND_LIVE_IP").expect("LOCALSEND_LIVE_IP");
        let port: u16 = std::env::var("LOCALSEND_LIVE_PORT")
            .expect("LOCALSEND_LIVE_PORT")
            .parse()
            .expect("LOCALSEND_LIVE_PORT must be a number");
        let fingerprint =
            std::env::var("LOCALSEND_LIVE_FINGERPRINT").expect("LOCALSEND_LIVE_FINGERPRINT");
        let file =
            PathBuf::from(std::env::var("LOCALSEND_LIVE_FILE").expect("LOCALSEND_LIVE_FILE"));
        let _ = rustls::crypto::ring::default_provider().install_default();

        let certified =
            rcgen::generate_simple_self_signed(vec!["interop.local".to_string()]).unwrap();
        let cert_der = certified.cert.der().to_vec();
        let key_der = certified.key_pair.serialize_der();
        let identity = Arc::new(crate::tls::Identity {
            fingerprint: crate::tls::sha256_hex(&cert_der),
            cert_der,
            key_der,
        });

        let mut config = Config::default();
        config.alias = "LocalSend Adwaita".to_string();
        config.fingerprint = "interop-http-fingerprint".to_string();
        config.encryption = false;
        let core = Arc::new(Core::for_test_with_identity(config, identity));

        let mut info = DeviceInfo::default();
        info.alias = "Live LocalSend Peer".to_string();
        info.fingerprint = fingerprint;
        info.port = Some(port);
        info.protocol = Some("https".to_string());
        info.download = Some(false);
        core.upsert_device(info, ip);
        let key = core
            .devices_sorted()
            .into_iter()
            .next()
            .expect("live peer was not registered")
            .key;

        let events = core.subscribe();
        send(
            core.clone(),
            key,
            vec![file],
            None,
            Arc::new(AtomicBool::new(false)),
        );

        let mut finished = false;
        while let Ok(event) = events.try_recv() {
            match event {
                Event::SendFinished {
                    files, total_bytes, ..
                } => {
                    println!("live transfer finished: {files} file(s), {total_bytes} bytes");
                    finished = true;
                }
                Event::SendFailed { message, .. } => {
                    panic!("live send failed: {message}");
                }
                Event::SendNeedsPin { .. } => panic!("the live peer requires a PIN"),
                _ => {}
            }
        }
        assert!(finished, "the live send did not complete");
    }

    fn text_sender_core(port: u16) -> (Arc<Core>, String) {
        let mut config = Config::default();
        config.alias = "Text Sender".to_string();
        config.fingerprint = "text-sender-self".to_string();
        config.encryption = false;
        let core = Arc::new(Core::for_test(config));
        let mut info = DeviceInfo::default();
        info.alias = "Text Receiver".to_string();
        info.fingerprint = "text-receiver-peer".to_string();
        info.port = Some(port);
        info.protocol = Some("http".to_string());
        info.download = Some(false);
        core.upsert_device(info, "127.0.0.1".to_string());
        let key = core.devices_sorted()[0].key.clone();
        (core, key)
    }

    #[test]
    fn collects_folders_recursively() {
        let root =
            std::env::temp_dir().join(format!("localsend-folder-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("nested/b.txt"), b"b").unwrap();

        let files = collect_files(&[root.clone()]).unwrap();
        let mut names: Vec<String> = files.into_iter().map(|file| file.name).collect();
        names.sort();
        let base = root.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            names,
            vec![format!("{base}/a.txt"), format!("{base}/nested/b.txt")]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sends_a_text_message_with_204() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (line, body, _, _) = read_http(&mut stream);
            request_tx.send((line, body)).unwrap();
            write!(
                stream,
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let (core, key) = text_sender_core(port);
        let events = core.subscribe();
        send_text(
            core.clone(),
            key,
            "hello from adwaita".to_string(),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let (line, body) = request_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(line.contains("/prepare-upload"));
        let request: PrepareUploadRequest = serde_json::from_slice(&body).unwrap();
        let metadata = request.files.get("text-0").expect("missing text metadata");
        assert_eq!(metadata.file_type, "text");
        assert_eq!(metadata.preview.as_deref(), Some("hello from adwaita"));

        let mut finished = false;
        while let Ok(event) = events.try_recv() {
            match event {
                Event::SendTextFinished { .. } => finished = true,
                Event::SendFailed { message, .. } => panic!("text send failed: {message}"),
                _ => {}
            }
        }
        assert!(finished, "the text send did not report completion");
        server.join().unwrap();
    }

    #[test]
    fn reports_a_declined_text_message() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (line, _, _, _) = read_http(&mut stream);
            assert!(line.contains("/prepare-upload"));
            write!(
                stream,
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let (core, key) = text_sender_core(port);
        let events = core.subscribe();
        send_text(
            core.clone(),
            key,
            "declined message".to_string(),
            None,
            Arc::new(AtomicBool::new(false)),
        );

        let mut declined = false;
        while let Ok(event) = events.try_recv() {
            match event {
                Event::SendDeclined { .. } => declined = true,
                Event::SendFailed { message, .. } => panic!("text send failed: {message}"),
                _ => {}
            }
        }
        assert!(declined, "the text send did not report a decline");
        server.join().unwrap();
    }

    #[test]
    fn sends_a_text_message_with_upload_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (upload_tx, upload_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (line, _, _, _) = read_http(&mut stream);
            assert!(line.contains("/prepare-upload"));
            let response = r#"{"sessionId":"text-session","files":{"text-0":"text-token"}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
            drop(stream);

            let (mut stream, _) = listener.accept().unwrap();
            let (line, body, _, _) = read_http(&mut stream);
            assert!(line.contains("/upload"));
            upload_tx.send(body).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let (core, key) = text_sender_core(port);
        let events = core.subscribe();
        send_text(
            core.clone(),
            key,
            "fallback body".to_string(),
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let uploaded = upload_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(uploaded, b"fallback body");

        let mut finished = false;
        while let Ok(event) = events.try_recv() {
            match event {
                Event::SendTextFinished { .. } => finished = true,
                Event::SendFailed { message, .. } => panic!("text send failed: {message}"),
                _ => {}
            }
        }
        assert!(finished, "the fallback text send did not report completion");
        server.join().unwrap();
    }

    #[test]
    fn sends_a_file_over_http() {
        let (port, uploads, server) = mock_receiver();

        let mut config = Config::default();
        config.alias = "Test Sender".to_string();
        config.fingerprint = "self-fingerprint".to_string();
        config.encryption = false;
        let core = Arc::new(Core::for_test(config));

        let mut info = DeviceInfo::default();
        info.alias = "Mock Receiver".to_string();
        info.fingerprint = "peer-fingerprint".to_string();
        info.port = Some(port);
        info.protocol = Some("http".to_string());
        info.download = Some(false);
        core.upsert_device(info, "127.0.0.1".to_string());
        let key = core.devices_sorted()[0].key.clone();

        let events = core.subscribe();
        let directory =
            std::env::temp_dir().join(format!("localsend-sender-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("hello.txt");
        let contents = b"hello from localsend".to_vec();
        std::fs::write(&path, &contents).unwrap();

        send(
            core.clone(),
            key,
            vec![path],
            None,
            Arc::new(AtomicBool::new(false)),
        );

        let uploaded = uploads
            .recv_timeout(Duration::from_secs(10))
            .expect("the mock receiver never got the upload");
        assert_eq!(uploaded, contents);

        let mut finished = false;
        while let Ok(event) = events.try_recv() {
            if let Event::SendFinished { files, .. } = event {
                assert_eq!(files, 1);
                finished = true;
            }
        }
        assert!(finished, "the transfer did not report completion");

        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&directory);
    }
}
