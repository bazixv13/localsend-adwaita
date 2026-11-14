//! LocalSend HTTP/HTTPS receiver.
//!
//! Implements the Upload API endpoints from protocol v2.2:
//! `POST /register`, `GET /info`, `POST /prepare-upload`, `POST /upload`
//! and `POST /cancel`. Files are streamed to disk and path traversal is
//! rejected in `protocol::sanitize_relative_path`.

use crate::core::{Core, Event, IncomingFileInfo, IncomingRequest, ReceiveFile, ReceiveSession};
use crate::protocol::{DeviceInfo, PrepareUploadRequest, PrepareUploadResponse, DEFAULT_PORT};
use crate::history::{self, HistoryEntry, HistoryItem};
use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;
/// A transfer with no bytes moving for this long is treated as abandoned.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

pub fn start(core: Arc<Core>) -> Result<(), String> {
    let config = core.config();
    let listener = bind(config.port)?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Could not inspect the listening socket: {e}"))?
        .port();

    let mut tls_config = None;
    if config.encryption {
        match core.identity() {
            Some(identity) => match crate::tls::server_config(&identity) {
                Ok(server_config) => tls_config = Some(Arc::new(server_config)),
                Err(error) => {
                    eprintln!("{error}");
                    core.emit(Event::Toast(
                        "Encryption is unavailable; falling back to unencrypted transfers".into(),
                    ));
                }
            },
            None => {
                core.emit(Event::Toast(
                    "Encryption is unavailable; falling back to unencrypted transfers".into(),
                ));
            }
        }
    }

    let protocol = if tls_config.is_some() {
        "https"
    } else {
        "http"
    };
    core.set_server(port, protocol);

    std::thread::Builder::new()
        .name("localsend-http".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
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
            if let Err(error) = runtime.block_on(serve(core.clone(), listener, tls_config, port)) {
                core.emit(Event::ServerError(format!(
                    "Receiving is unavailable: {error}"
                )));
            }
        })
        .map_err(|e| format!("Could not start the receiver thread: {e}"))?;

    Ok(())
}

fn bind(preferred_port: u16) -> Result<std::net::TcpListener, String> {
    let mut last_error = String::new();
    for port in preferred_port..=preferred_port.saturating_add(10) {
        match std::net::TcpListener::bind(("0.0.0.0", port)) {
            Ok(listener) => {
                listener
                    .set_nonblocking(true)
                    .map_err(|e| format!("Could not configure the listening socket: {e}"))?;
                return Ok(listener);
            }
            Err(error) => last_error = format!("Port {port}: {error}"),
        }
    }
    Err(format!("Could not open a receiving port ({last_error})"))
}

async fn serve(
    core: Arc<Core>,
    listener: std::net::TcpListener,
    tls_config: Option<Arc<rustls::ServerConfig>>,
    port: u16,
) -> Result<(), String> {
    let listener = TcpListener::from_std(listener)
        .map_err(|e| format!("Could not use the listening socket: {e}"))?;
    let app = router(core.clone());
    let _ = port;

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
                        message: "the sender stopped responding".to_string(),
                    });
                }
            }
        });
    }

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("Accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
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
        && request
            .files
            .values()
            .all(|file| file.preview.as_deref().is_some_and(|preview| !preview.is_empty()));

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
            message: "the sender stopped responding".to_string(),
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
        let accepted = matches!(
            tokio::time::timeout(Duration::from_secs(60), decision_rx).await,
            Ok(Ok(true))
        );
        if !accepted {
            // Either the user declined or the sender gave up waiting; the
            // window must stop showing the request as pending.
            core.emit(Event::IncomingWithdrawn {
                session_id: session_id.clone(),
            });
            core.end_prepare();
            return StatusCode::FORBIDDEN.into_response();
        }
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
                core.emit(Event::Toast(format!(
                    "Could not save received files: {error}"
                )));
                core.end_prepare();
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };

    let mut session_files = Vec::with_capacity(files.len());
    let mut tokens = HashMap::new();
    for file in &files {
        let token = uuid::Uuid::new_v4().to_string();
        tokens.insert(file.id.clone(), token.clone());
        let sha256 = request
            .files
            .get(&file.id)
            .or_else(|| request.files.values().find(|f| f.id == file.id))
            .and_then(|f| f.sha256.clone());
        session_files.push(Arc::new(ReceiveFile {
            id: file.id.clone(),
            name: file.name.clone(),
            file_type: file.file_type.clone(),
            size: file.size,
            token,
            sha256,
            received: AtomicU64::new(0),
            done: AtomicBool::new(false),
            text: Mutex::new(None),
            path: Mutex::new(None),
        }));
    }

    let session = Arc::new(ReceiveSession {
        id: session_id.clone(),
        peer_alias: peer_alias.clone(),
        files: session_files,
        total_bytes,
        done_bytes: AtomicU64::new(0),
        files_done: AtomicUsize::new(0),
        canceled: AtomicBool::new(false),
        directory,
        last_activity: Mutex::new(Instant::now()),
        history_started: AtomicBool::new(false),
    });
    core.insert_session(session);
    core.end_prepare();

    json_response(
        StatusCode::OK,
        &PrepareUploadResponse {
            session_id,
            files: tokens,
        },
    )
}

struct TargetFile {
    path: PathBuf,
    file: std::fs::File,
}

async fn upload(
    State(core): State<Arc<Core>>,
    ConnectInfo(_peer): ConnectInfo<SocketAddr>,
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
    if session.canceled.load(Ordering::SeqCst) {
        return StatusCode::CONFLICT.into_response();
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

    let target = match create_target(&session.directory, &relative) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("Could not create target file: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if let Ok(mut slot) = file.path.lock() {
        *slot = Some(target.path.clone());
    }

    let mut output = tokio::fs::File::from_std(target.file);
    let mut stream = body.into_data_stream();
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let done_before = session.done_bytes.load(Ordering::SeqCst);
    let total = session.total_bytes.max(1);

    while let Some(chunk) = stream.next().await {
        if session.canceled.load(Ordering::SeqCst) {
            drop(output);
            let _ = tokio::fs::remove_file(&target.path).await;
            return StatusCode::CONFLICT.into_response();
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                drop(output);
                let _ = tokio::fs::remove_file(&target.path).await;
                fail_session(&core, &session, "The upload was interrupted");
                return StatusCode::BAD_REQUEST.into_response();
            }
        };
        if let Err(error) = output.write_all(&chunk).await {
            eprintln!("Write failed for {}: {error}", target.path.display());
            drop(output);
            let _ = tokio::fs::remove_file(&target.path).await;
            fail_session(&core, &session, "The file could not be written to disk");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        hasher.update(&chunk);
        received += chunk.len() as u64;
        session.touch();
        if last_emit.elapsed() >= Duration::from_millis(120) {
            last_emit = Instant::now();
            core.emit(Event::ReceiveProgress {
                session_id: session.id.clone(),
                file_name: file.name.clone(),
                done: done_before + received,
                total,
                files_done: session.files_done.load(Ordering::SeqCst),
                files_total: session.files.len(),
            });
        }
    }

    if let Err(error) = output.flush().await {
        eprintln!("Flush failed for {}: {error}", target.path.display());
        drop(output);
        let _ = tokio::fs::remove_file(&target.path).await;
        fail_session(&core, &session, "The file could not be written to disk");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    drop(output);

    if received < file.size {
        // A truncated upload is not a completed file.
        let _ = tokio::fs::remove_file(&target.path).await;
        fail_session(&core, &session, "The upload was incomplete");
        return StatusCode::BAD_REQUEST.into_response();
    }

    if let Some(expected) = file.sha256.as_deref() {
        let actual = hasher.finalize();
        let actual = actual
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !expected.eq_ignore_ascii_case(&actual) {
            let _ = tokio::fs::remove_file(&target.path).await;
            fail_session(
                &core,
                &session,
                "A received file did not match its checksum",
            );
            return StatusCode::UNPROCESSABLE_ENTITY.into_response();
        }
    }

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
        let text = file.text.lock().ok().and_then(|t| t.clone()).unwrap_or_default();
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
            let cache = if core.config().history_cache {
                let cache_root = core.history_cache_root();
                let index = session.files_done.load(Ordering::SeqCst) as usize;
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
    core.emit(Event::ReceiveProgress {
        session_id: session.id.clone(),
        file_name: file.name.clone(),
        done: session.done_bytes.load(Ordering::SeqCst),
        total: session.total_bytes.max(1),
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
    let done_before = session.done_bytes.load(Ordering::SeqCst);
    let total = session.total_bytes.max(1);

    while let Some(chunk) = stream.next().await {
        if session.canceled.load(Ordering::SeqCst) {
            return StatusCode::CONFLICT.into_response();
        }
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                fail_session(&core, &session, "The message was interrupted");
                return StatusCode::BAD_REQUEST.into_response();
            }
        };
        if data.len().saturating_add(chunk.len()) as u64 > MAX_TEXT_BYTES {
            fail_session(&core, &session, "The message is too large");
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        data.extend_from_slice(&chunk);
        hasher.update(&chunk);
        received += chunk.len() as u64;
        session.touch();
        if last_emit.elapsed() >= Duration::from_millis(120) {
            last_emit = Instant::now();
            core.emit(Event::ReceiveProgress {
                session_id: session.id.clone(),
                file_name: file.name.clone(),
                done: done_before + received,
                total,
                files_done: session.files_done.load(Ordering::SeqCst),
                files_total: session.files.len(),
            });
        }
    }

    if received < file.size {
        fail_session(&core, &session, "The message was incomplete");
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(expected) = file.sha256.as_deref() {
        let actual = hasher.finalize();
        let actual: String = actual.iter().map(|byte| format!("{byte:02x}")).collect();
        if !expected.eq_ignore_ascii_case(&actual) {
            fail_session(&core, &session, "The message did not match its checksum");
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
    if let Some(session) = core.take_session(&session_id) {
        session.canceled.store(true, Ordering::SeqCst);
        core.emit(Event::ReceiveCanceled {
            session_id: session.id.clone(),
            alias: session.peer_alias.clone(),
        });
    }
    // The peer can also cancel while it is *receiving* from us.
    core.cancel_outgoing(&session_id);
    StatusCode::OK.into_response()
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
    let body = serde_json::to_vec(value).unwrap_or_default();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
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
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                return Ok(TargetFile {
                    path: candidate,
                    file,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not find a free file name",
    ))
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

    #[tokio::test]
    async fn receives_an_uploaded_file() {
        let directory =
            std::env::temp_dir().join(format!("localsend-receive-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();

        let mut config = Config::default();
        config.alias = "Test Receiver".to_string();
        config.fingerprint = "self-test-fingerprint".to_string();
        config.encryption = false;
        config.pin.clear();
        config.download_dir = directory.clone();
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
        let digest = Sha256::digest(&contents);
        let sha256 = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
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

        tokio::time::timeout(Duration::from_secs(5), finished_rx.recv())
            .await
            .expect("no ReceiveFinished event")
            .expect("event channel closed");

        let _ = std::fs::remove_dir_all(&directory);
    }

    fn text_test_core(directory: &Path) -> Arc<Core> {
        let mut config = Config::default();
        config.alias = "Test Receiver".to_string();
        config.fingerprint = "self-test-fingerprint".to_string();
        config.encryption = false;
        config.pin.clear();
        config.download_dir = directory.to_path_buf();
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
        let digest = Sha256::digest(&contents);
        let sha256 = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

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
