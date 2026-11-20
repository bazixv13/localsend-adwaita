//! LocalSend HTTP/HTTPS receiver.
//!
//! Implements the Upload API endpoints from protocol v2.2:
//! `POST /register`, `GET /info`, `POST /prepare-upload`, `POST /upload`
//! and `POST /cancel`. Files are streamed to disk and path traversal is
//! rejected in `protocol::sanitize_relative_path`.

use crate::core::{Core, Event, IncomingFileInfo, IncomingRequest, ReceiveFile, ReceiveSession};
use crate::history::{self, HistoryEntry, HistoryItem};
use crate::protocol::{DeviceInfo, PrepareUploadRequest, PrepareUploadResponse, DEFAULT_PORT};
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read as _;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;
/// JSON bodies are buffered in memory, so they need a ceiling. Uploads stream
/// and are not buffered, so the limit never applies to file data.
const MAX_JSON_BYTES: usize = MAX_TEXT_BYTES as usize + 1024 * 1024;
/// Kept free on the download drive so a receive never fills it completely.
const DISK_RESERVE: u64 = 16 * 1024 * 1024;
/// How long an unfinished upload is kept, so a retry can pick it up again.
const PARTIAL_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Expired partials are swept at most this often, so the check stays cheap.
const PARTIAL_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
static LAST_PARTIAL_SWEEP: OnceLock<Mutex<Instant>> = OnceLock::new();
/// Free space is re-checked this often while a file streams in, so a disk that
/// fills up mid-transfer fails cleanly instead of filling the filesystem.
const SPACE_CHECK_BYTES: u64 = 8 * 1024 * 1024;
/// A transfer with no bytes moving for this long is treated as abandoned.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Signals the running receiver to re-read its settings.
static CONTROL: OnceLock<tokio::sync::mpsc::UnboundedSender<()>> = OnceLock::new();

pub fn start(core: Arc<Core>) -> Result<(), String> {
    let listener = bind(core.config().port)?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Could not inspect the listening socket: {e}"))?
        .port();
    let tls_config = tls_config_for(&core);
    core.set_server(port, protocol_name(&tls_config));

    // Settings can reopen the listening socket while the app runs. The
    // receiver owns the socket, so the request travels through this channel
    // instead of the UI trying to rebind behind its back.
    let (control, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = CONTROL.set(control);

    std::thread::Builder::new()
        .name("localsend-http".to_string())
        .spawn(move || {
            // Uploads arrive in parallel (the protocol allows it), and each
            // one is mostly a blocking write to disk. A current-thread runtime
            // serialises them onto one worker, so a single slow write stalls
            // every other upload and the accept loop with it.
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    core.emit(Event::ServerError(format!(
                        "Could not start the HTTP runtime: {error}"
                    )));
                    return;
                }
            };
            if let Err(error) =
                runtime.block_on(serve(core.clone(), listener, tls_config, control_rx))
            {
                core.emit(Event::ServerError(format!(
                    "Receiving is unavailable: {error}"
                )));
            }
        })
        .map_err(|e| format!("Could not start the receiver thread: {e}"))?;

    Ok(())
}

/// Re-reads the port and encryption settings and applies them to the running
/// receiver, so no settings change needs an app restart.
pub fn reconfigure() {
    if let Some(control) = CONTROL.get() {
        let _ = control.send(());
    }
}

fn protocol_name(tls: &Option<Arc<rustls::ServerConfig>>) -> &'static str {
    if tls.is_some() {
        "https"
    } else {
        "http"
    }
}

fn tls_config_for(core: &Core) -> Option<Arc<rustls::ServerConfig>> {
    if !core.config().encryption {
        return None;
    }
    let fallback = || {
        core.emit(Event::Toast(crate::i18n::t(
            "Encryption is unavailable; falling back to unencrypted transfers",
        )));
    };
    match core.identity() {
        Some(identity) => match crate::tls::server_config(&identity) {
            Ok(server_config) => Some(Arc::new(server_config)),
            Err(error) => {
                eprintln!("{error}");
                fallback();
                None
            }
        },
        None => {
            fallback();
            None
        }
    }
}

fn bind_port(port: u16) -> Result<std::net::TcpListener, String> {
    let listener =
        std::net::TcpListener::bind(("0.0.0.0", port)).map_err(|error| error.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("Could not configure the listening socket: {e}"))?;
    Ok(listener)
}

fn bind(preferred_port: u16) -> Result<std::net::TcpListener, String> {
    let mut last_error = String::new();
    for port in preferred_port..=preferred_port.saturating_add(10) {
        match bind_port(port) {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = format!("Port {port}: {error}"),
        }
    }
    Err(format!("Could not open a receiving port ({last_error})"))
}

async fn serve(
    core: Arc<Core>,
    listener: std::net::TcpListener,
    initial_tls: Option<Arc<rustls::ServerConfig>>,
    mut control: tokio::sync::mpsc::UnboundedReceiver<()>,
) -> Result<(), String> {
    let mut listener = TcpListener::from_std(listener)
        .map_err(|e| format!("Could not use the listening socket: {e}"))?;
    let mut port = listener
        .local_addr()
        .map_err(|e| format!("Could not inspect the listening socket: {e}"))?
        .port();
    let mut tls_config = initial_tls;
    let app = router(core.clone());

    // A sender that vanishes mid-transfer leaves a session with no bytes
    // moving. Drop it and tell the window, so no dead progress page stays.
    {
        let core = core.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                for (session_id, alias) in core.prune_stale_sessions(IDLE_TIMEOUT) {
                    core.emit(Event::ReceiveFailed {
                        session_id,
                        alias,
                        message: crate::i18n::t("the sender stopped responding"),
                    });
                }
            }
        });
    }

    loop {
        let connection = tokio::select! {
            signal = control.recv() => {
                if signal.is_none() {
                    return Ok(());
                }
                let requested = core.config().port;
                if requested != port {
                    // A port chosen in Settings is used as given: silently
                    // moving to the next free one would make the number in the
                    // settings dialog a lie.
                    match bind_port(requested).and_then(|bound| {
                        TcpListener::from_std(bound)
                            .map_err(|error| format!("Could not use the new port: {error}"))
                    }) {
                        Ok(bound) => {
                            listener = bound;
                            port = listener
                                .local_addr()
                                .map(|address| address.port())
                                .unwrap_or(requested);
                        }
                        Err(error) => core.emit(Event::Toast(crate::i18n::fmt(
                            "Could not use port {}: {} Still receiving on {}.",
                            &[&requested, &error, &port],
                        ))),
                    }
                }
                tls_config = tls_config_for(&core);
                core.set_server(port, protocol_name(&tls_config));
                // Peers remember the old port and protocol, so tell them about
                // the new details instead of waiting for the next interval.
                core.request_announce();
                continue;
            }
            accepted = listener.accept() => match accepted {
                Ok(connection) => connection,
                Err(error) => {
                    eprintln!("Accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            },
        };
        let (stream, peer) = connection;
        let app = app.clone();
        let tls = tls_config.clone();
        tokio::spawn(async move {
            match tls {
                Some(config) => {
                    let acceptor = tokio_rustls::TlsAcceptor::from(config);
                    match tokio::time::timeout(Duration::from_secs(15), acceptor.accept(stream))
                        .await
                    {
                        Ok(Ok(tls_stream)) => {
                            serve_connection(tls_stream, app, peer).await;
                        }
                        Ok(Err(error)) => {
                            eprintln!("TLS handshake failed with {peer}: {error}");
                        }
                        Err(_) => eprintln!("TLS handshake timed out with {peer}"),
                    }
                }
                None => serve_connection(stream, app, peer).await,
            }
        });
    }
}

async fn serve_connection<S>(stream: S, app: Router, peer: SocketAddr)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service =
        hyper::service::service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
            request.extensions_mut().insert(ConnectInfo(peer));
            let app = app.clone();
            async move {
                let response = match app.oneshot(request.map(Body::new)).await {
                    Ok(response) => response,
                    Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                };
                Ok::<_, std::convert::Infallible>(response)
            }
        });

    if let Err(error) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        eprintln!("Connection error with {peer}: {error}");
    }
}

use tower::ServiceExt;

fn router(core: Arc<Core>) -> Router {
    Router::new()
        .route("/api/localsend/v2/register", post(register))
        .route("/api/localsend/v2/info", get(info))
        .route("/api/localsend/v2/prepare-upload", post(prepare_upload))
        .route("/api/localsend/v2/upload", post(upload))
        .route("/api/localsend/v2/cancel", post(cancel))
        // Buffered bodies are parsed into memory before the handlers see them.
        // The upload route streams its body, so file data is unaffected.
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES))
        .with_state(core)
}

async fn register(
    State(core): State<Arc<Core>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    let Ok(mut incoming) = serde_json::from_slice::<DeviceInfo>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !incoming.fingerprint.is_empty() && !core.is_own_fingerprint(&incoming.fingerprint) {
        if incoming.port.is_none() {
            incoming.port = Some(DEFAULT_PORT);
        }
        if incoming.protocol.is_none() {
            incoming.protocol = Some("http".to_string());
        }
        core.upsert_device_verified(incoming, peer.ip().to_string());
    }
    json_response(StatusCode::OK, &core.info(None))
}

async fn info(State(core): State<Arc<Core>>) -> Response {
    json_response(StatusCode::OK, &core.info(None))
}

async fn prepare_upload(
    State(core): State<Arc<Core>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    let Ok(request) = serde_json::from_slice::<PrepareUploadRequest>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let config = core.config();
    if config.pin_required() {
        let provided = query.get("pin").cloned().unwrap_or_default();
        if provided != config.pin {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }

    if request.files.is_empty() {
        return StatusCode::NO_CONTENT.into_response();
    }

    let all_messages = request.files.values().all(crate::protocol::is_message);
    let text_messages: Vec<String> = request
        .files
        .values()
        .filter_map(crate::protocol::message_body)
        .collect();
    // Peers embed a chat message in the preview of a text pseudo-file. It
    // expects 204 (no upload needed), not a file upload.
    let message_only = all_messages
        && !request.files.is_empty()
        && request.files.values().all(|file| {
            file.preview
                .as_deref()
                .is_some_and(|preview| !preview.is_empty())
        });

    for file in request.files.values() {
        if crate::protocol::is_message(file) {
            if file
                .preview
                .as_ref()
                .is_some_and(|preview| preview.len() as u64 > MAX_TEXT_BYTES)
            {
                return StatusCode::PAYLOAD_TOO_LARGE.into_response();
            }
            continue;
        }
        if crate::protocol::sanitize_relative_path(&file.file_name).is_none() {
            return StatusCode::BAD_REQUEST.into_response();
        }
    }

    for (session_id, alias) in core.prune_stale_sessions(IDLE_TIMEOUT) {
        core.emit(Event::ReceiveFailed {
            session_id,
            alias,
            message: crate::i18n::t("the sender stopped responding"),
        });
    }
    if !core.begin_prepare() {
        return StatusCode::CONFLICT.into_response();
    }
    if core.has_active_session() {
        core.end_prepare();
        return StatusCode::CONFLICT.into_response();
    }

    if request.info.fingerprint.is_empty() || !core.is_own_fingerprint(&request.info.fingerprint) {
        let mut info = request.info.clone();
        if info.port.is_none() {
            info.port = Some(DEFAULT_PORT);
        }
        if info.protocol.is_none() {
            info.protocol = Some("http".to_string());
        }
        core.upsert_device_verified(info, peer.ip().to_string());
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let mut files: Vec<IncomingFileInfo> = Vec::with_capacity(request.files.len());
    let mut total_bytes = 0u64;
    for (key, file) in &request.files {
        let id = if file.id.is_empty() {
            key.clone()
        } else {
            file.id.clone()
        };
        total_bytes = total_bytes.saturating_add(file.size);
        files.push(IncomingFileInfo {
            id,
            name: file.file_name.clone(),
            size: file.size,
            file_type: file.file_type.clone(),
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let peer_alias = if request.info.alias.trim().is_empty() {
        peer.ip().to_string()
    } else {
        request.info.alias.clone()
    };
    let (decision_tx, decision_rx) = tokio::sync::oneshot::channel();
    core.emit(Event::IncomingRequest(IncomingRequest {
        session_id: session_id.clone(),
        peer_alias: peer_alias.clone(),
        files: files.clone(),
        total_bytes,
        messages: if message_only {
            text_messages.clone()
        } else {
            Vec::new()
        },
        decision: decision_tx,
    }));

    // Quick Save accepts immediately. Without it the sender waits while the
    // user decides from the notification.
    if !message_only && !config.quick_save {
        struct PendingDecisionGuard {
            core: Arc<Core>,
            session_id: String,
            active: bool,
        }
        impl Drop for PendingDecisionGuard {
            fn drop(&mut self) {
                if self.active {
                    self.core.emit(Event::IncomingWithdrawn {
                        session_id: self.session_id.clone(),
                    });
                    self.core.end_prepare();
                }
            }
        }

        let mut guard = PendingDecisionGuard {
            core: core.clone(),
            session_id: session_id.clone(),
            active: true,
        };
        let accepted = matches!(
            tokio::time::timeout(Duration::from_secs(60), decision_rx).await,
            Ok(Ok(true))
        );
        if !accepted {
            // Either the user declined or the sender gave up waiting; the
            // window must stop showing the request as pending.
            guard.active = false;
            core.emit(Event::IncomingWithdrawn {
                session_id: session_id.clone(),
            });
            core.end_prepare();
            return StatusCode::FORBIDDEN.into_response();
        }
        guard.active = false;
    }

    if message_only {
        core.end_prepare();
        let entry = HistoryEntry {
            id: session_id.clone(),
            alias: peer_alias.clone(),
            timestamp: history::now_unix(),
            items: text_messages
                .iter()
                .map(|text| HistoryItem::Message {
                    id: history::make_item_id(),
                    bytes: history::message_bytes(text),
                    text: Some(text.clone()),
                })
                .collect(),
        };
        core.history_add_entry(entry);
        core.emit(Event::ReceiveText {
            alias: peer_alias,
            messages: text_messages,
        });
        return StatusCode::NO_CONTENT.into_response();
    }

    let directory = if all_messages {
        PathBuf::new()
    } else {
        match config.ensure_download_dir() {
            Ok(directory) => directory,
            Err(error) => {
                eprintln!("{error}");
                core.emit(Event::Toast(crate::i18n::fmt(
                    "Could not save received files: {}",
                    &[&error],
                )));
                core.end_prepare();
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };

    // Clear out partials from transfers that were abandoned more than a day
    // ago. Recent ones stay, so a retry can reuse them.
    if !all_messages {
        let root = directory.clone();
        let _ = tokio::task::spawn_blocking(move || sweep_partials(&root)).await;
    }

    // Files whose announced checksum matches a file already in the download
    // folder are accepted without a token: the sender uploads only the rest.
    let mut skipped: HashMap<String, PathBuf> = HashMap::new();
    {
        let candidates: Vec<(String, PathBuf, u64, String)> = files
            .iter()
            .filter(|file| !crate::protocol::is_text_file(&file.file_type))
            .filter_map(|file| {
                let sha256 = request
                    .files
                    .get(&file.id)
                    .or_else(|| request.files.values().find(|f| f.id == file.id))
                    .and_then(|f| f.sha256.clone())?;
                let relative = crate::protocol::sanitize_relative_path(&file.name)?;
                Some((file.id.clone(), directory.join(relative), file.size, sha256))
            })
            .collect();
        if !candidates.is_empty() {
            let found = tokio::task::spawn_blocking(move || {
                candidates
                    .into_iter()
                    .filter(|(_, path, size, sha256)| existing_file_matches(path, *size, sha256))
                    .map(|(id, path, _, _)| (id, path))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            skipped.extend(found);
        }
    }

    // Refuse a transfer that cannot fit instead of filling the drive and
    // failing half-way, which looks like a corrupt transfer to the sender.
    let needed: u64 = files
        .iter()
        .filter(|file| !crate::protocol::is_text_file(&file.file_type))
        .filter(|file| !skipped.contains_key(&file.id))
        .map(|file| file.size)
        .sum();
    if needed > 0 {
        if let Some(free) = available_space(&directory) {
            if needed.saturating_add(DISK_RESERVE) > free {
                core.emit(Event::Toast(crate::i18n::fmt(
                    "Not enough free space: {} needed, {} available",
                    &[
                        &crate::util::human_bytes(needed),
                        &crate::util::human_bytes(free),
                    ],
                )));
                core.end_prepare();
                return StatusCode::INSUFFICIENT_STORAGE.into_response();
            }
        }
    }

    let mut session_files = Vec::with_capacity(files.len());
    let mut tokens = HashMap::new();
    for file in &files {
        let announced = request
            .files
            .get(&file.id)
            .or_else(|| request.files.values().find(|f| f.id == file.id));
        let sha256 = announced.and_then(|f| f.sha256.clone());
        let metadata = announced.and_then(|f| f.metadata.clone());
        let existing = skipped.get(&file.id).cloned();
        let token = if existing.is_some() {
            String::new()
        } else {
            let token = uuid::Uuid::new_v4().to_string();
            tokens.insert(file.id.clone(), token.clone());
            token
        };
        session_files.push(Arc::new(ReceiveFile {
            id: file.id.clone(),
            name: file.name.clone(),
            file_type: file.file_type.clone(),
            size: file.size,
            token,
            sha256,
            received: AtomicU64::new(0),
            done: AtomicBool::new(false),
            existing: existing.is_some(),
            metadata,
            text: Mutex::new(None),
            path: Mutex::new(existing),
        }));
    }

    // Tell the sender how much of each file is already on disk, so it can
    // continue from there instead of starting over. Peers that ignore the
    // field simply upload the whole file again.
    let mut resume: HashMap<String, u64> = HashMap::new();
    for file in &session_files {
        if file.existing || crate::protocol::is_text_file(&file.file_type) {
            continue;
        }
        let Some(relative) = crate::protocol::sanitize_relative_path(&file.name) else {
            continue;
        };
        let partial = partial_path(&directory.join(relative));
        if let Ok(metadata) = std::fs::metadata(&partial) {
            if metadata.is_file() && metadata.len() > 0 && metadata.len() < file.size {
                resume.insert(file.id.clone(), metadata.len());
            }
        }
    }

    let peer_port = request.info.port_or_default();
    let peer_protocol = request.info.protocol().to_string();
    let peer_fingerprint = request.info.fingerprint.clone();
    let session = Arc::new(ReceiveSession {
        id: session_id.clone(),
        peer_alias: peer_alias.clone(),
        peer_ip: peer.ip().to_string(),
        peer_port,
        peer_protocol,
        peer_fingerprint,
        files: session_files,
        total_bytes,
        done_bytes: AtomicU64::new(0),
        received_bytes: AtomicU64::new(0),
        files_done: AtomicUsize::new(0),
        canceled: AtomicBool::new(false),
        directory,
        last_activity: Mutex::new(Instant::now()),
    });
    core.insert_session(session.clone());
    core.end_prepare();

    // Files the receiver already had are complete before any upload starts.
    for file in &session.files {
        if file.path.lock().map(|path| path.is_some()).unwrap_or(false) {
            let size = file.size;
            session.received_bytes.fetch_add(size, Ordering::SeqCst);
            complete_file(&core, &session, file, size);
        }
    }

    json_response(
        StatusCode::OK,
        &PrepareUploadResponse {
            session_id,
            files: tokens,
            resume,
        },
    )
}

/// Applies the sender's `modified`/`accessed` timestamps to the saved file,
/// so a transfer keeps the original dates like other LocalSend clients do.
fn apply_file_times(file: &ReceiveFile, path: Option<PathBuf>) {
    let Some(metadata) = file.metadata.as_ref() else {
        return;
    };
    let Some(path) = path else {
        return;
    };
    let mut times = std::fs::FileTimes::new();
    let mut any = false;
    if let Some(modified) = metadata.modified.as_deref().and_then(parse_timestamp) {
        times = times.set_modified(modified);
        any = true;
    }
    if let Some(accessed) = metadata.accessed.as_deref().and_then(parse_timestamp) {
        times = times.set_accessed(accessed);
        any = true;
    }
    if !any {
        return;
    }
    if let Ok(handle) = std::fs::OpenOptions::new().write(true).open(&path) {
        let _ = handle.set_times(times);
    }
}

fn parse_timestamp(value: &str) -> Option<std::time::SystemTime> {
    let parsed = gtk::glib::DateTime::from_iso8601(value, None).ok()?;
    let seconds = parsed.to_unix();
    if seconds < 0 {
        return None;
    }
    let nanos = u32::try_from(parsed.microsecond()).unwrap_or(0) * 1000;
    Some(std::time::UNIX_EPOCH + Duration::new(seconds as u64, nanos))
}

/// Whether an existing file is byte-identical to what the sender announced.
/// The size check avoids hashing files that obviously differ.
fn existing_file_matches(path: &Path, size: u64, sha256: &str) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() != size {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(_) => return false,
        }
    }
    crate::util::to_hex(&hasher.finalize()).eq_ignore_ascii_case(sha256)
}

struct TargetFile {
    /// Where the file appears once it is complete.
    path: PathBuf,
    /// Where the bytes are written meanwhile, so an interrupted transfer never
    /// leaves a truncated file with the real name.
    temp: PathBuf,
    file: std::fs::File,
}

async fn upload(
    State(core): State<Arc<Core>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
    body: Body,
) -> Response {
    let (Some(session_id), Some(file_id), Some(token)) = (
        query.get("sessionId").cloned(),
        query.get("fileId").cloned(),
        query.get("token").cloned(),
    ) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    let Some((session, file)) = core.find_session_file(&session_id, &file_id, &token) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    // Protocol 2.2: a token is only valid for the peer that prepared the
    // session, so an upload from another address is rejected.
    if !session.peer_ip.is_empty() && session.peer_ip != peer.ip().to_string() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if session.canceled.load(Ordering::SeqCst) {
        return canceled_response();
    }
    if file.done.load(Ordering::SeqCst) {
        return StatusCode::CONFLICT.into_response();
    }

    if crate::protocol::is_text_file(&file.file_type) {
        return receive_text(core, session, file, body).await;
    }

    let Some(relative) = crate::protocol::sanitize_relative_path(&file.name) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // A sender that knows how much is already on disk continues from there
    // instead of starting the file over. Peers that do not use the extension
    // send no offset, which rewrites the partial from the beginning.
    let offset: u64 = query
        .get("offset")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if offset > file.size {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let target = if offset == 0 {
        match create_target(&session.directory, &relative) {
            Ok(target) => target,
            Err(error) => {
                eprintln!("Could not create target file: {error}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    } else {
        match resume_target(&session.directory, &relative, offset) {
            Ok(target) => target,
            Err(ResumeError::Mismatch) => return StatusCode::CONFLICT.into_response(),
            Err(ResumeError::Io(error)) => {
                eprintln!("Could not resume a transfer: {error}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };
    if let Ok(mut slot) = file.path.lock() {
        *slot = Some(target.path.clone());
    }

    let mut output = tokio::fs::File::from_std(target.file);
    let mut stream = body.into_data_stream();
    let mut hasher = Sha256::new();
    let mut received = offset;
    if offset > 0 {
        // The checksum covers the whole file, so the prefix already on disk is
        // hashed before the remaining bytes stream in.
        if let Err(error) = hash_prefix(&target.temp, offset, &mut hasher) {
            eprintln!("Could not verify the resumed bytes: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        session.received_bytes.fetch_add(offset, Ordering::SeqCst);
    }
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let mut next_space_check = received.saturating_add(SPACE_CHECK_BYTES);
    let total = session.total_bytes.max(1);

    while let Some(chunk) = stream.next().await {
        if session.canceled.load(Ordering::SeqCst) {
            // The partial stays for the grace period: the sender can retry the
            // same name without the folder filling up with copies.
            drop(output);
            return canceled_response();
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                drop(output);
                // A sender that cancels drops the connection before its
                // /cancel call arrives, so give that call a moment.
                if peer_canceled(&session).await {
                    return canceled_response();
                }
                fail_session(
                    &core,
                    &session,
                    &crate::i18n::t("The upload was interrupted"),
                );
                return StatusCode::BAD_REQUEST.into_response();
            }
        };
        if let Err(error) = output.write_all(&chunk).await {
            eprintln!("Write failed for {}: {error}", target.path.display());
            drop(output);
            fail_session(
                &core,
                &session,
                &crate::i18n::t("The file could not be written to disk"),
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        hasher.update(&chunk);
        received += chunk.len() as u64;
        if received >= next_space_check {
            next_space_check = received.saturating_add(SPACE_CHECK_BYTES);
            if let Some(free) = available_space(&session.directory) {
                if free < DISK_RESERVE {
                    // Another program filled the disk while this file grew.
                    drop(output);
                    fail_session(&core, &session, &crate::i18n::t("The disk is full"));
                    return StatusCode::INSUFFICIENT_STORAGE.into_response();
                }
            }
        }
        let chunk_len = chunk.len() as u64;
        let done = session
            .received_bytes
            .fetch_add(chunk_len, Ordering::SeqCst)
            + chunk_len;
        session.touch();
        if last_emit.elapsed() >= Duration::from_millis(120) {
            last_emit = Instant::now();
            core.emit(Event::ReceiveProgress {
                session_id: session.id.clone(),
                file_name: file.name.clone(),
                done: done.min(total),
                total,
                files_done: session.files_done.load(Ordering::SeqCst),
                files_total: session.files.len(),
            });
        }
    }

    if let Err(error) = output.flush().await {
        eprintln!("Flush failed for {}: {error}", target.path.display());
        drop(output);
        fail_session(
            &core,
            &session,
            &crate::i18n::t("The file could not be written to disk"),
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    drop(output);

    if received < file.size {
        // The sender can stop mid-file when it cancels; its /cancel call may
        // still be on the way, so this is not a failure until it is ruled out.
        if peer_canceled(&session).await {
            return canceled_response();
        }
        // A truncated upload is not a completed file, but its bytes are kept
        // for the grace period so a retry is not starting from nothing.
        fail_session(
            &core,
            &session,
            &crate::i18n::t("The upload was incomplete"),
        );
        return StatusCode::BAD_REQUEST.into_response();
    }

    if let Some(expected) = file.sha256.as_deref() {
        let actual = crate::util::to_hex(&hasher.finalize());
        if !expected.eq_ignore_ascii_case(&actual) {
            let _ = tokio::fs::remove_file(&target.temp).await;
            fail_session(
                &core,
                &session,
                &crate::i18n::t("A received file did not match its checksum"),
            );
            return StatusCode::UNPROCESSABLE_ENTITY.into_response();
        }
    }

    // The upload is complete and verified, so publish it under its real name.
    if let Err(error) = tokio::fs::rename(&target.temp, &target.path).await {
        eprintln!("Could not finish {}: {error}", target.path.display());
        let _ = tokio::fs::remove_file(&target.temp).await;
        fail_session(
            &core,
            &session,
            &crate::i18n::t("The file could not be saved"),
        );
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    apply_file_times(&file, Some(target.path.clone()));
    complete_file(&core, &session, &file, received);
    StatusCode::OK.into_response()
}

/// Records a completed file, emits progress, and finishes the session once all
/// files are done. Text is reported separately so the UI never presents it as
/// a saved file. Each landed item is recorded in history immediately so a
/// cancelled transfer still keeps the files that were fully received.
fn complete_file(
    core: &Core,
    session: &Arc<ReceiveSession>,
    file: &Arc<ReceiveFile>,
    received: u64,
) {
    file.received.store(received, Ordering::SeqCst);
    file.done.store(true, Ordering::SeqCst);
    session.done_bytes.fetch_add(received, Ordering::SeqCst);

    // Record history for this completed item.
    if crate::protocol::is_text_file(&file.file_type) {
        let text = file
            .text
            .lock()
            .ok()
            .and_then(|t| t.clone())
            .unwrap_or_default();
        core.history_record_item(
            &session.id,
            &session.peer_alias,
            HistoryItem::Message {
                id: history::make_item_id(),
                bytes: history::message_bytes(&text),
                text: Some(text),
            },
        );
    } else {
        let path = file.path.lock().ok().and_then(|p| p.clone());
        if let Some(path) = path {
            let cache = if core.config().history_cache && !file.existing {
                let cache_root = core.history_cache_root();
                let index = session.files_done.load(Ordering::SeqCst);
                history::cache_file_copy(&path, &session.id, index, &file.name, &cache_root)
            } else {
                None
            };
            let mime_hint = mime_guess::from_path(&path)
                .first_raw()
                .unwrap_or("application/octet-stream")
                .to_string();
            core.history_record_item(
                &session.id,
                &session.peer_alias,
                HistoryItem::File {
                    id: history::make_item_id(),
                    name: file.name.clone(),
                    size: received,
                    mime_hint,
                    path,
                    cache,
                },
            );
        }
    }

    let files_done = session.files_done.fetch_add(1, Ordering::SeqCst) + 1;
    // Report the same monotonic counter the per-chunk updates use. Mixing in
    // `done_bytes` (completed files only) made the bar jump backwards when a
    // file finished while another was still uploading.
    let total = session.total_bytes.max(1);
    let done = session.received_bytes.load(Ordering::SeqCst).min(total);
    core.emit(Event::ReceiveProgress {
        session_id: session.id.clone(),
        file_name: file.name.clone(),
        done,
        total,
        files_done,
        files_total: session.files.len(),
    });

    if files_done < session.files.len() {
        return;
    }

    core.take_session(&session.id);
    let messages: Vec<String> = session
        .files
        .iter()
        .filter(|file| crate::protocol::is_text_file(&file.file_type))
        .filter_map(|file| file.text.lock().ok().and_then(|text| text.clone()))
        .collect();
    let files: Vec<String> = session
        .files
        .iter()
        .filter(|file| !crate::protocol::is_text_file(&file.file_type))
        .map(|file| file.name.clone())
        .collect();

    if files.is_empty() && !messages.is_empty() {
        core.emit(Event::ReceiveText {
            alias: session.peer_alias.clone(),
            messages,
        });
    } else {
        core.emit(Event::ReceiveFinished {
            session_id: session.id.clone(),
            alias: session.peer_alias.clone(),
            directory: session.directory.clone(),
            files,
            messages,
            total_bytes: session.done_bytes.load(Ordering::SeqCst),
        });
    }
}

/// Fallback for peers that send a text pseudo-file through the upload API
/// instead of putting the message in `preview`. Nothing is written to disk.
async fn receive_text(
    core: Arc<Core>,
    session: Arc<ReceiveSession>,
    file: Arc<ReceiveFile>,
    body: Body,
) -> Response {
    let mut stream = body.into_data_stream();
    let mut data = Vec::new();
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let total = session.total_bytes.max(1);

    while let Some(chunk) = stream.next().await {
        if session.canceled.load(Ordering::SeqCst) {
            return StatusCode::CONFLICT.into_response();
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                fail_session(
                    &core,
                    &session,
                    &crate::i18n::t("The message was interrupted"),
                );
                return StatusCode::BAD_REQUEST.into_response();
            }
        };
        if data.len().saturating_add(chunk.len()) as u64 > MAX_TEXT_BYTES {
            fail_session(&core, &session, &crate::i18n::t("The message is too large"));
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        data.extend_from_slice(&chunk);
        hasher.update(&chunk);
        received += chunk.len() as u64;
        let chunk_len = chunk.len() as u64;
        let done = session
            .received_bytes
            .fetch_add(chunk_len, Ordering::SeqCst)
            + chunk_len;
        session.touch();
        if last_emit.elapsed() >= Duration::from_millis(120) {
            last_emit = Instant::now();
            core.emit(Event::ReceiveProgress {
                session_id: session.id.clone(),
                file_name: file.name.clone(),
                done: done.min(total),
                total,
                files_done: session.files_done.load(Ordering::SeqCst),
                files_total: session.files.len(),
            });
        }
    }

    if received < file.size {
        fail_session(
            &core,
            &session,
            &crate::i18n::t("The message was incomplete"),
        );
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(expected) = file.sha256.as_deref() {
        let actual = crate::util::to_hex(&hasher.finalize());
        if !expected.eq_ignore_ascii_case(&actual) {
            fail_session(
                &core,
                &session,
                &crate::i18n::t("The message did not match its checksum"),
            );
            return StatusCode::UNPROCESSABLE_ENTITY.into_response();
        }
    }
    let text = String::from_utf8_lossy(&data).into_owned();
    let text = if text.is_empty() {
        file.name.clone()
    } else {
        text
    };
    if let Ok(mut slot) = file.text.lock() {
        *slot = Some(text);
    }
    complete_file(&core, &session, &file, received);
    StatusCode::OK.into_response()
}

async fn cancel(
    State(core): State<Arc<Core>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(session_id) = query.get("sessionId").cloned() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if let Some(session) = core.cancel_session(&session_id) {
        core.emit(Event::ReceiveCanceled {
            session_id: session.id.clone(),
            alias: session.peer_alias.clone(),
        });
    }
    // The peer can also cancel while it is *receiving* from us.
    core.cancel_outgoing(&session_id);
    StatusCode::OK.into_response()
}

/// A canceled transfer can look like a broken connection: the sender stops
/// mid-body and posts `/cancel` on a second connection a moment later. Wait
/// briefly so the transfer is reported as canceled instead of as a failure.
async fn peer_canceled(session: &ReceiveSession) -> bool {
    for _ in 0..20 {
        if session.canceled.load(Ordering::SeqCst) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    session.canceled.load(Ordering::SeqCst)
}

/// The receiver canceled this upload. The body lets our sender tell the case
/// apart from a busy receiver (both are 409); other clients only see the code.
fn canceled_response() -> Response {
    (StatusCode::CONFLICT, "canceled").into_response()
}

fn fail_session(core: &Core, session: &Arc<ReceiveSession>, message: &str) {
    if core.take_session(&session.id).is_some() {
        session.canceled.store(true, Ordering::SeqCst);
        core.emit(Event::ReceiveFailed {
            session_id: session.id.clone(),
            alias: session.peer_alias.clone(),
            message: message.to_string(),
        });
    }
}

fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not encode the response: {error}"),
        )
            .into_response(),
    }
}

fn create_target(directory: &Path, relative: &Path) -> std::io::Result<TargetFile> {
    if let Some(parent) = relative.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(directory.join(parent))?;
        }
    }
    for attempt in 0..10_000u32 {
        let candidate = if attempt == 0 {
            directory.join(relative)
        } else {
            directory.join(suffixed(relative, attempt))
        };
        if candidate.exists() {
            continue;
        }
        let temp = partial_path(&candidate);
        // A partial from a recent transfer is reused, so an interrupted send
        // does not leave more and more numbered files behind. Only a partial
        // that is past its grace period is thrown away.
        if temp.exists() && partial_expired(&temp) {
            let _ = std::fs::remove_file(&temp);
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
        {
            Ok(file) => {
                return Ok(TargetFile {
                    path: candidate,
                    temp,
                    file,
                })
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not find a free file name",
    ))
}

/// Whether an unfinished upload is past its grace period.
fn partial_expired(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_none_or(|age| age >= PARTIAL_TTL)
}

/// Removes partials older than the grace period, at most once an hour.
fn sweep_partials(directory: &Path) {
    let now = Instant::now();
    let last = LAST_PARTIAL_SWEEP.get_or_init(|| Mutex::new(now - PARTIAL_SWEEP_INTERVAL));
    {
        let Ok(mut last) = last.lock() else {
            return;
        };
        if now.duration_since(*last) < PARTIAL_SWEEP_INTERVAL {
            return;
        }
        *last = now;
    }
    remove_expired_partials(directory, 0);
}

fn remove_expired_partials(directory: &Path, depth: usize) {
    // Download folders can contain relative paths a few levels deep; the cap
    // keeps a pathological tree from making the sweep unbounded.
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            remove_expired_partials(&path, depth + 1);
        } else if kind.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "part")
            && partial_expired(&path)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

enum ResumeError {
    /// The partial does not hold exactly the announced prefix, so the sender
    /// has to start the file again.
    Mismatch,
    Io(std::io::Error),
}

/// Opens a partial for appending at `offset`. Its length must match exactly,
/// so a sender can neither skip bytes nor overwrite what is already there.
fn resume_target(
    directory: &Path,
    relative: &Path,
    offset: u64,
) -> Result<TargetFile, ResumeError> {
    let path = directory.join(relative);
    if path.exists() {
        return Err(ResumeError::Mismatch);
    }
    let temp = partial_path(&path);
    let metadata = std::fs::metadata(&temp).map_err(ResumeError::Io)?;
    if !metadata.is_file() || metadata.len() != offset {
        return Err(ResumeError::Mismatch);
    }
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(&temp)
        .map_err(ResumeError::Io)?;
    Ok(TargetFile { path, temp, file })
}

/// Feeds the bytes already on disk into the checksum, so the final digest still
/// covers the whole file when only the tail was uploaded.
fn hash_prefix(path: &Path, offset: u64, hasher: &mut Sha256) -> std::io::Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut hashed = 0u64;
    while hashed < offset {
        let want = usize::try_from((offset - hashed).min(buffer.len() as u64)).unwrap_or(0);
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the partial is shorter than the resume offset",
            ));
        }
        hasher.update(&buffer[..read]);
        hashed += read as u64;
    }
    Ok(())
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// Free bytes on the filesystem holding `path`, so a transfer that cannot fit
/// is refused before any data is written.
fn available_space(path: &Path) -> Option<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs only writes into the struct that is handed to it, and
    // `path` is a valid NUL-terminated string for the duration of the call.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    let block_size = if stats.f_frsize != 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    };
    Some(stats.f_bavail.saturating_mul(block_size))
}

fn suffixed(relative: &Path, attempt: u32) -> PathBuf {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let name = relative
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            (stem.to_string(), extension.to_string())
        }
        _ => (name, String::new()),
    };
    let file_name = if extension.is_empty() {
        format!("{stem} ({attempt})")
    } else {
        format!("{stem} ({attempt}).{extension}")
    };
    parent.join(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn partial_files_are_written_next_to_the_real_name() {
        assert_eq!(
            partial_path(Path::new("/tmp/report.pdf")),
            PathBuf::from("/tmp/report.pdf.part")
        );
    }

    #[test]
    fn a_retry_reuses_the_partial_of_an_interrupted_transfer() {
        let directory =
            std::env::temp_dir().join(format!("localsend-retry-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let relative = Path::new("clip.mp4");

        let first = create_target(&directory, relative).unwrap();
        drop(first.file);
        assert!(first.temp.exists());

        let second = create_target(&directory, relative).unwrap();
        assert_eq!(second.path, first.path);
        assert_eq!(second.temp, first.temp);

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_sweep_only_removes_partials_past_their_grace_period() {
        let directory =
            std::env::temp_dir().join(format!("localsend-sweep-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let recent = directory.join("recent.bin.part");
        let expired = directory.join("expired.bin.part");
        let complete = directory.join("complete.bin");
        std::fs::write(&recent, b"partial").unwrap();
        std::fs::write(&expired, b"partial").unwrap();
        std::fs::write(&complete, b"done").unwrap();

        let old = std::time::SystemTime::now() - PARTIAL_TTL - Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&expired)
            .unwrap()
            .set_modified(old)
            .unwrap();

        remove_expired_partials(&directory, 0);

        assert!(recent.exists(), "a fresh partial must survive the sweep");
        assert!(!expired.exists(), "a day-old partial must be removed");
        assert!(complete.exists(), "completed files are never swept");

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn reads_the_free_space_of_a_real_directory() {
        let free = available_space(&std::env::temp_dir()).expect("statvfs failed");
        assert!(free > 0, "a temp directory should have some space");
        assert!(available_space(Path::new("/this/path/does/not/exist")).is_none());
    }

    #[test]
    fn matches_existing_files_by_size_and_checksum() {
        let directory =
            std::env::temp_dir().join(format!("localsend-skip-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("song.m4a");
        std::fs::write(&path, b"hello").unwrap();
        let digest = crate::util::to_hex(&Sha256::digest(b"hello"));

        assert!(existing_file_matches(&path, 5, &digest));
        assert!(existing_file_matches(&path, 5, &digest.to_uppercase()));
        assert!(!existing_file_matches(&path, 4, &digest));
        assert!(!existing_file_matches(&path, 5, &"0".repeat(64)));
        assert!(!existing_file_matches(&directory, 5, &digest));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn accepts_but_does_not_request_files_it_already_has() {
        let directory =
            std::env::temp_dir().join(format!("localsend-skip-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let existing = b"already here".to_vec();
        std::fs::write(directory.join("song.m4a"), &existing).unwrap();

        let config = Config {
            alias: "Test Receiver".to_string(),
            fingerprint: "self-test-fingerprint".to_string(),
            encryption: false,
            quick_save: true,
            download_dir: directory.clone(),
            ..Config::default()
        };
        let core = Arc::new(Core::for_test(config));
        core.set_server(53317, "http");

        let app = router(core.clone());
        let body = json!({
            "info": {
                "alias": "Mock Sender",
                "version": "2.2",
                "deviceType": "desktop",
                "fingerprint": "peer-test-fingerprint",
                "port": 4444,
                "protocol": "http",
                "download": false
            },
            "files": {
                "f1": {
                    "id": "f1",
                    "fileName": "song.m4a",
                    "size": existing.len(),
                    "fileType": "audio/mp4",
                    "sha256": crate::util::to_hex(&Sha256::digest(&existing))
                },
                "f2": {
                    "id": "f2",
                    "fileName": "other.bin",
                    "size": 3,
                    "fileType": "application/octet-stream",
                    "sha256": crate::util::to_hex(&Sha256::digest(b"new"))
                }
            }
        })
        .to_string();

        let prepare = Request::builder()
            .method("POST")
            .uri("/api/localsend/v2/prepare-upload")
            .header("content-type", "application/json")
            .extension(ConnectInfo::<SocketAddr>(SocketAddr::from((
                [127, 0, 0, 1],
                4444,
            ))))
            .body(Body::from(body))
            .unwrap();
        let response = app.oneshot(prepare).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let prepared: PrepareUploadResponse = serde_json::from_slice(&bytes).unwrap();

        assert!(
            !prepared.files.contains_key("f1"),
            "the file already on disk must not be requested again"
        );
        assert!(
            prepared.files.contains_key("f2"),
            "new files still need an upload token"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn resumes_an_interrupted_upload_from_the_partial() {
        let directory =
            std::env::temp_dir().join(format!("localsend-resume-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let config = Config {
            alias: "Test Receiver".to_string(),
            fingerprint: "self-test-fingerprint".to_string(),
            encryption: false,
            download_dir: directory.clone(),
            ..Config::default()
        };
        let core = Arc::new(Core::for_test(config));
        core.set_server(53317, "http");
        let app = router(core.clone());
        let peer = SocketAddr::from(([127, 0, 0, 1], 4444));
        let contents = b"0123456789".to_vec();
        let sha256 = crate::util::to_hex(&Sha256::digest(&contents));

        let prepare_body = json!({
            "info": {
                "alias": "Mock Sender",
                "version": "2.2",
                "deviceType": "desktop",
                "fingerprint": "peer-resume-fingerprint",
                "port": 4444,
                "protocol": "http",
                "download": false
            },
            "files": {
                "f1": {
                    "id": "f1",
                    "fileName": "resume.bin",
                    "size": contents.len(),
                    "fileType": "application/octet-stream",
                    "sha256": sha256
                }
            }
        })
        .to_string();

        let prepare = Request::builder()
            .method("POST")
            .uri("/api/localsend/v2/prepare-upload")
            .header("content-type", "application/json")
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(prepare_body.clone()))
            .unwrap();
        let response = app.clone().oneshot(prepare).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let prepared: PrepareUploadResponse = serde_json::from_slice(&bytes).unwrap();
        let token = prepared.files.get("f1").unwrap().clone();

        // Half a file arrives and the connection drops.
        let first = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/localsend/v2/upload?sessionId={}&fileId=f1&token={}",
                prepared.session_id, token
            ))
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(contents[..4].to_vec()))
            .unwrap();
        let response = app.clone().oneshot(first).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            directory.join("resume.bin.part").exists(),
            "the bytes that did arrive must be kept"
        );

        // The sender retries: the receiver reports the prefix it already has.
        let prepare = Request::builder()
            .method("POST")
            .uri("/api/localsend/v2/prepare-upload")
            .header("content-type", "application/json")
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(prepare_body))
            .unwrap();
        let response = app.clone().oneshot(prepare).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let prepared: PrepareUploadResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(prepared.resume.get("f1"), Some(&4));
        let token = prepared.files.get("f1").unwrap().clone();

        let rest = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/localsend/v2/upload?sessionId={}&fileId=f1&token={}&offset=4",
                prepared.session_id, token
            ))
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(contents[4..].to_vec()))
            .unwrap();
        let response = app.oneshot(rest).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read(directory.join("resume.bin")).unwrap(),
            contents
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn a_canceled_upload_is_answered_with_the_cancel_token() {
        let directory =
            std::env::temp_dir().join(format!("localsend-cancel-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let config = Config {
            alias: "Test Receiver".to_string(),
            fingerprint: "self-test-fingerprint".to_string(),
            encryption: false,
            quick_save: true,
            download_dir: directory.clone(),
            ..Config::default()
        };
        let core = Arc::new(Core::for_test(config));
        core.set_server(53317, "http");
        let events = core.subscribe();
        let app = router(core.clone());
        let peer = SocketAddr::from(([127, 0, 0, 1], 4444));
        let contents = b"0123456789".to_vec();

        let prepare_body = json!({
            "info": {
                "alias": "Mock Sender",
                "version": "2.2",
                "deviceType": "desktop",
                "fingerprint": "peer-cancel-fingerprint",
                "port": 4444,
                "protocol": "http",
                "download": false
            },
            "files": {
                "f1": {
                    "id": "f1",
                    "fileName": "cancel.bin",
                    "size": contents.len(),
                    "fileType": "application/octet-stream"
                }
            }
        })
        .to_string();

        let prepare = Request::builder()
            .method("POST")
            .uri("/api/localsend/v2/prepare-upload")
            .header("content-type", "application/json")
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(prepare_body))
            .unwrap();
        let response = app.clone().oneshot(prepare).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let prepared: PrepareUploadResponse = serde_json::from_slice(&bytes).unwrap();
        let token = prepared.files.get("f1").unwrap().clone();
        while events.try_recv().is_ok() {}

        // The sender gives up before the upload starts.
        let cancel = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/localsend/v2/cancel?sessionId={}",
                prepared.session_id
            ))
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(cancel).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let event = events.try_recv().expect("the cancel must reach the UI");
        assert!(matches!(event, Event::ReceiveCanceled { .. }));

        // A late upload is refused with the token our sender understands.
        let upload = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/localsend/v2/upload?sessionId={}&fileId=f1&token={}",
                prepared.session_id, token
            ))
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(contents))
            .unwrap();
        let response = app.oneshot(upload).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"canceled");
        assert!(
            !directory.join("cancel.bin").exists(),
            "a canceled transfer must not publish a file"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn receives_an_uploaded_file() {
        let directory =
            std::env::temp_dir().join(format!("localsend-receive-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let config = Config {
            alias: "Test Receiver".to_string(),
            fingerprint: "self-test-fingerprint".to_string(),
            encryption: false,
            download_dir: directory.clone(),
            ..Config::default()
        };
        let core = Arc::new(Core::for_test(config));
        core.set_server(53317, "http");

        let events = core.subscribe();
        let (finished_tx, mut finished_rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                match event {
                    Event::IncomingRequest(_) => {}
                    Event::ReceiveFinished { .. } => {
                        let _ = finished_tx.send(()).await;
                        break;
                    }
                    _ => {}
                }
            }
        });

        let app = router(core.clone());
        let contents = b"hello from the protocol".to_vec();
        let sha256 = crate::util::to_hex(&Sha256::digest(&contents));
        let peer = SocketAddr::from(([127, 0, 0, 1], 4444));

        let body = json!({
            "info": {
                "alias": "Mock Sender",
                "version": "2.2",
                "deviceType": "desktop",
                "fingerprint": "peer-test-fingerprint",
                "port": 4444,
                "protocol": "http",
                "download": false
            },
            "files": {
                "f1": {
                    "id": "f1",
                    "fileName": "received.txt",
                    "size": contents.len(),
                    "fileType": "text/plain",
                    "sha256": sha256
                }
            }
        })
        .to_string();

        let prepare = Request::builder()
            .method("POST")
            .uri("/api/localsend/v2/prepare-upload")
            .header("content-type", "application/json")
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(body))
            .unwrap();
        let response = app.clone().oneshot(prepare).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let prepared: PrepareUploadResponse = serde_json::from_slice(&bytes).unwrap();
        let token = prepared
            .files
            .get("f1")
            .expect("missing upload token")
            .clone();

        let upload = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/localsend/v2/upload?sessionId={}&fileId=f1&token={}",
                prepared.session_id, token
            ))
            .header("content-type", "application/octet-stream")
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(contents.clone()))
            .unwrap();
        let response = app.oneshot(upload).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let saved = std::fs::read(directory.join("received.txt")).unwrap();
        assert_eq!(saved, contents);
        assert!(
            !directory.join("received.txt.part").exists(),
            "a completed upload must not leave its partial behind"
        );

        tokio::time::timeout(Duration::from_secs(5), finished_rx.recv())
            .await
            .expect("no ReceiveFinished event")
            .expect("event channel closed");

        let _ = std::fs::remove_dir_all(&directory);
    }

    fn text_test_core(directory: &Path) -> Arc<Core> {
        let config = Config {
            alias: "Test Receiver".to_string(),
            fingerprint: "self-test-fingerprint".to_string(),
            encryption: false,
            download_dir: directory.to_path_buf(),
            ..Config::default()
        };
        let core = Arc::new(Core::for_test(config));
        core.set_server(53317, "http");
        let _ = std::fs::remove_dir_all(directory);
        core
    }

    fn text_event_loop(core: &Arc<Core>) -> tokio::sync::mpsc::Receiver<Vec<String>> {
        let events = core.subscribe();
        let (text_tx, text_rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                match event {
                    Event::IncomingRequest(_) => {}
                    Event::ReceiveText { messages, .. } => {
                        let _ = text_tx.send(messages).await;
                    }
                    Event::ReceiveFinished { .. } => {
                        panic!("a text message must not complete as a file transfer")
                    }
                    _ => {}
                }
            }
        });
        text_rx
    }

    #[tokio::test]
    async fn receives_a_preview_text_message_without_saving() {
        let directory = std::env::temp_dir().join(format!(
            "localsend-preview-text-test-{}",
            uuid::Uuid::new_v4()
        ));
        let core = text_test_core(&directory);
        let mut text_rx = text_event_loop(&core);
        let peer = SocketAddr::from(([127, 0, 0, 1], 4444));

        let body = json!({
            "info": {
                "alias": "Mock Sender",
                "version": "2.2",
                "deviceType": "desktop",
                "fingerprint": "peer-text-fingerprint",
                "port": 4444,
                "protocol": "http",
                "download": false
            },
            "files": {
                "m1": {
                    "id": "m1",
                    "fileName": "message",
                    "size": 5,
                    "fileType": "text",
                    "preview": "hello"
                }
            }
        })
        .to_string();

        let prepare = Request::builder()
            .method("POST")
            .uri("/api/localsend/v2/prepare-upload")
            .header("content-type", "application/json")
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(body))
            .unwrap();
        let response = router(core.clone()).oneshot(prepare).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let messages = tokio::time::timeout(Duration::from_secs(5), text_rx.recv())
            .await
            .expect("no ReceiveText event")
            .expect("text channel closed");
        assert_eq!(messages, vec!["hello".to_string()]);
        assert!(
            !directory.exists(),
            "text must not create the download directory"
        );
        assert!(!core.has_active_session());
    }

    #[tokio::test]
    async fn receives_uploaded_text_without_saving() {
        let directory = std::env::temp_dir().join(format!(
            "localsend-uploaded-text-test-{}",
            uuid::Uuid::new_v4()
        ));
        let core = text_test_core(&directory);
        let mut text_rx = text_event_loop(&core);
        let peer = SocketAddr::from(([127, 0, 0, 1], 4444));
        let contents = b"a message body".to_vec();
        let sha256 = crate::util::to_hex(&Sha256::digest(&contents));

        let body = json!({
            "info": {
                "alias": "Mock Sender",
                "version": "2.2",
                "deviceType": "desktop",
                "fingerprint": "peer-upload-text-fingerprint",
                "port": 4444,
                "protocol": "http",
                "download": false
            },
            "files": {
                "m1": {
                    "id": "m1",
                    "fileName": "https://example.com/not-a-path",
                    "size": contents.len(),
                    "fileType": "text",
                    "sha256": sha256
                }
            }
        })
        .to_string();

        let app = router(core.clone());
        let prepare = Request::builder()
            .method("POST")
            .uri("/api/localsend/v2/prepare-upload")
            .header("content-type", "application/json")
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(body))
            .unwrap();
        let response = app.clone().oneshot(prepare).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let prepared: PrepareUploadResponse = serde_json::from_slice(&bytes).unwrap();
        let token = prepared
            .files
            .get("m1")
            .expect("missing upload token")
            .clone();

        let upload = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/localsend/v2/upload?sessionId={}&fileId=m1&token={}",
                prepared.session_id, token
            ))
            .header("content-type", "application/octet-stream")
            .extension(ConnectInfo::<SocketAddr>(peer))
            .body(Body::from(contents.clone()))
            .unwrap();
        let response = app.oneshot(upload).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let messages = tokio::time::timeout(Duration::from_secs(5), text_rx.recv())
            .await
            .expect("no ReceiveText event")
            .expect("text channel closed");
        assert_eq!(messages, vec![String::from_utf8(contents).unwrap()]);
        assert!(
            !directory.exists(),
            "text must not create the download directory"
        );
        assert!(!core.has_active_session());
    }
}
