use crate::config::Config;
use crate::history::{HistoryEntry, HistoryItem, HistoryStore, RemoveOutcome, Resolution};
use crate::protocol::{DeviceInfo, DEFAULT_PORT};
use crate::tls::Identity;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Device {
    pub info: DeviceInfo,
    pub ip: String,
    pub last_seen: Instant,
    pub failed_probes: u8,
}

#[derive(Debug, Clone)]
pub struct DeviceSnapshot {
    pub key: String,
    pub alias: String,
    pub device_type: String,
    pub ip: String,
}

#[derive(Debug, Clone)]
pub struct IncomingFileInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub file_type: String,
}

pub struct IncomingRequest {
    pub session_id: String,
    pub peer_alias: String,
    pub files: Vec<IncomingFileInfo>,
    pub total_bytes: u64,
    pub messages: Vec<String>,
    /// The UI answers this when Quick Save is off.
    pub decision: tokio::sync::oneshot::Sender<bool>,
}

pub struct ReceiveFile {
    pub id: String,
    pub name: String,
    pub file_type: String,
    pub size: u64,
    pub token: String,
    pub sha256: Option<String>,
    pub received: AtomicU64,
    pub done: AtomicBool,
    /// The file was already in the download folder, so it is not uploaded
    /// again and must not be copied into the history cache.
    pub existing: bool,
    /// Optional timestamps announced by the sender.
    pub metadata: Option<crate::protocol::FileMetadataExt>,
    pub text: Mutex<Option<String>>,
    pub path: Mutex<Option<PathBuf>>,
}

pub struct ReceiveSession {
    pub id: String,
    pub peer_alias: String,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_protocol: String,
    pub peer_fingerprint: String,
    pub files: Vec<Arc<ReceiveFile>>,
    pub total_bytes: u64,
    pub done_bytes: AtomicU64,
    /// Bytes written to disk so far. Peers upload several files at once, so
    /// progress must come from one shared counter instead of per-request
    /// snapshots, which would make the total jump up and down.
    pub received_bytes: AtomicU64,
    pub files_done: AtomicUsize,
    pub canceled: AtomicBool,
    pub directory: PathBuf,
    /// Last time bytes moved. A session that stops moving means the sender
    /// vanished, so the window must stop showing it as running.
    pub last_activity: Mutex<Instant>,
}

impl ReceiveSession {
    pub fn find_file(&self, id: &str) -> Option<Arc<ReceiveFile>> {
        self.files.iter().find(|file| file.id == id).cloned()
    }

    pub fn touch(&self) {
        if let Ok(mut last) = self.last_activity.lock() {
            *last = Instant::now();
        }
    }

    pub fn idle(&self) -> Duration {
        self.last_activity
            .lock()
            .map(|last| last.elapsed())
            .unwrap_or_default()
    }
}

pub enum Event {
    DevicesChanged,
    ServerError(String),
    IncomingRequest(IncomingRequest),
    ReceiveProgress {
        session_id: String,
        file_name: String,
        done: u64,
        total: u64,
        files_done: usize,
        files_total: usize,
    },
    ReceiveFinished {
        session_id: String,
        alias: String,
        directory: PathBuf,
        files: Vec<String>,
        messages: Vec<String>,
        total_bytes: u64,
    },
    ReceiveText {
        alias: String,
        messages: Vec<String>,
    },
    ReceiveFailed {
        session_id: String,
        alias: String,
        message: String,
    },
    ReceiveCanceled {
        session_id: String,
        alias: String,
    },
    /// A transfer request was never answered because the sender gave up.
    IncomingWithdrawn {
        session_id: String,
    },
    SendProgress {
        file_name: String,
        done: u64,
        total: u64,
        files_done: usize,
        files_total: usize,
    },
    SendFinished {
        device: String,
        files: usize,
        total_bytes: u64,
    },
    SendTextFinished {
        device: String,
    },
    SendTextNeedsPin {
        device: String,
        text: String,
    },
    SendFailed {
        device: String,
        message: String,
    },
    SendDeclined {
        device: String,
    },
    SendUnavailable {
        message: String,
    },
    SendCanceled,
    /// The tray icon asked for the window to be shown.
    TrayShow,
    /// The tray icon asked for the window to be hidden again.
    TrayHide,
    /// The tray icon asked for the application to quit.
    TrayQuit,
    SendNeedsPin {
        device: String,
        paths: Vec<PathBuf>,
    },
    Toast(String),
    HistoryChanged,
}

pub struct Core {
    events: async_channel::Sender<Event>,
    receiver: Mutex<Option<async_channel::Receiver<Event>>>,
    config: RwLock<Config>,
    pub devices: Mutex<HashMap<String, Device>>,
    unreachable: Mutex<HashSet<String>>,
    pub sessions: Mutex<HashMap<String, Arc<ReceiveSession>>>,
    pub history: Mutex<HistoryStore>,
    port: AtomicU16,
    protocol: RwLock<&'static str>,
    identity: RwLock<Option<Arc<Identity>>>,
    preparing: AtomicBool,
    pub scanning: AtomicBool,
    revision: AtomicU64,
    tray_active: AtomicBool,
    outgoing: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Core {
    pub fn new() -> Self {
        let mut config = Config::load();
        if config.fingerprint.is_empty() {
            config.fingerprint = crate::util::random_hex(32);
            let _ = config.save();
        }
        let identity = match crate::tls::load_or_create() {
            Ok(identity) => Some(Arc::new(identity)),
            Err(error) => {
                eprintln!("TLS identity unavailable: {error}");
                None
            }
        };
        Self::with_parts(config, identity, HistoryStore::load_default())
    }

    fn with_parts(config: Config, identity: Option<Arc<Identity>>, history: HistoryStore) -> Self {
        let (events, receiver) = async_channel::unbounded();
        Self {
            events,
            receiver: Mutex::new(Some(receiver)),
            config: RwLock::new(config),
            devices: Mutex::new(HashMap::new()),
            unreachable: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
            history: Mutex::new(history),
            port: AtomicU16::new(DEFAULT_PORT),
            protocol: RwLock::new("http"),
            identity: RwLock::new(identity),
            preparing: AtomicBool::new(false),
            scanning: AtomicBool::new(false),
            revision: AtomicU64::new(0),
            tray_active: AtomicBool::new(false),
            outgoing: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_identity(config: Config, identity: Arc<Identity>) -> Self {
        let core = Self::for_test(config);
        *core.identity.write().unwrap() = Some(identity);
        core
    }

    #[cfg(test)]
    pub(crate) fn for_test(config: Config) -> Self {
        let root =
            std::env::temp_dir().join(format!("localsend-core-test-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        let cache = root.join("cache/history");
        let _ = std::fs::create_dir_all(&data);
        let _ = std::fs::create_dir_all(&cache);
        Self::with_parts(
            config,
            None,
            HistoryStore::load(data.join("history.json"), cache),
        )
    }

    pub fn subscribe(&self) -> async_channel::Receiver<Event> {
        self.receiver
            .lock()
            .unwrap()
            .take()
            .expect("event receiver already taken")
    }

    pub fn emit(&self, event: Event) {
        let _ = self.events.try_send(event);
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn update_config(&self, update: impl FnOnce(&mut Config)) -> Config {
        let config = {
            let mut guard = self.config.write().unwrap();
            update(&mut guard);
            guard.clone()
        };
        if let Err(error) = config.save() {
            eprintln!("Could not save settings: {error}");
        }
        config
    }

    /// Whether a tray icon is currently shown. Closing the window may only
    /// hide it when the user still has a way to get it back.
    pub fn tray_active(&self) -> bool {
        self.tray_active.load(Ordering::SeqCst)
    }

    pub fn set_tray_active(&self, active: bool) {
        self.tray_active.store(active, Ordering::SeqCst);
    }

    pub fn set_server(&self, port: u16, protocol: &'static str) {
        self.port.store(port, Ordering::SeqCst);
        *self.protocol.write().unwrap() = protocol;
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    pub fn protocol(&self) -> &'static str {
        *self.protocol.read().unwrap()
    }

    pub fn identity(&self) -> Option<Arc<Identity>> {
        self.identity.read().unwrap().clone()
    }

    pub fn fingerprint(&self) -> String {
        if self.protocol() == "https" {
            self.identity
                .read()
                .unwrap()
                .as_ref()
                .map(|identity| identity.fingerprint.clone())
                .unwrap_or_default()
        } else {
            self.config.read().unwrap().fingerprint.clone()
        }
    }

    pub fn is_own_fingerprint(&self, fingerprint: &str) -> bool {
        if fingerprint.is_empty() {
            return false;
        }
        let config = self.config();
        fingerprint == self.fingerprint() || fingerprint == config.fingerprint
    }

    pub fn info(&self, announce: Option<bool>) -> DeviceInfo {
        let config = self.config();
        DeviceInfo {
            alias: config.alias.clone(),
            version: crate::protocol::PROTOCOL_VERSION.to_string(),
            device_model: Some("GNOME".to_string()),
            device_type: Some(config.device_type.clone()),
            fingerprint: self.fingerprint(),
            port: Some(self.port()),
            protocol: Some(self.protocol().to_string()),
            download: Some(false),
            announce,
        }
    }

    pub fn upsert_device(&self, info: DeviceInfo, ip: String) -> bool {
        self.upsert_device_inner(info, ip, false)
    }

    /// An upsert confirmed by the peer's HTTP server. Successful probes reset
    /// the failure counter used to remove peers whose discovery service still
    /// announces after their transfer server has gone away.
    pub fn upsert_device_verified(&self, info: DeviceInfo, ip: String) -> bool {
        self.upsert_device_inner(info, ip, true)
    }

    fn upsert_device_inner(&self, mut info: DeviceInfo, ip: String, verified: bool) -> bool {
        if self.is_own_fingerprint(&info.fingerprint) {
            return false;
        }
        if info.alias.trim().is_empty() {
            info.alias = if ip.is_empty() {
                "Unknown device".to_string()
            } else {
                ip.clone()
            };
        } else {
            info.alias = info.alias.trim().to_string();
        }
        info.port = Some(info.port_or_default());
        info.protocol = Some(info.protocol().to_string());
        info.download = Some(info.download.unwrap_or(false));
        info.announce = None;

        let key = Self::device_key(&info, &ip);
        if verified {
            self.unreachable.lock().unwrap().remove(&key);
        } else if self.unreachable.lock().unwrap().contains(&key) {
            // A broadcast from a peer whose transfer server refused a
            // connection is not proof that it is back. Wait for /register or
            // /info to succeed before showing it again.
            return false;
        }
        let mut devices = self.devices.lock().unwrap();
        let changed = match devices.get_mut(&key) {
            Some(existing) => {
                let changed = existing.ip != ip || existing.info != info;
                existing.info = info;
                existing.ip = ip;
                existing.last_seen = Instant::now();
                if verified {
                    existing.failed_probes = 0;
                }
                changed
            }
            None => {
                devices.insert(
                    key,
                    Device {
                        info,
                        ip,
                        last_seen: Instant::now(),
                        failed_probes: 0,
                    },
                );
                true
            }
        };
        drop(devices);
        if changed {
            self.revision.fetch_add(1, Ordering::SeqCst);
            self.emit(Event::DevicesChanged);
        }
        changed
    }

    /// Monotonic counter the UI can poll to catch any missed device event.
    pub fn device_revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    fn device_key(info: &DeviceInfo, ip: &str) -> String {
        if info.fingerprint.is_empty() {
            format!("ip:{ip}")
        } else {
            format!("fp:{}", info.fingerprint)
        }
    }

    pub fn remove_stale_devices(&self, ttl: Duration) -> bool {
        let now = Instant::now();
        let mut devices = self.devices.lock().unwrap();
        let before = devices.len();
        devices.retain(|_, device| now.duration_since(device.last_seen) < ttl);
        let removed = before.saturating_sub(devices.len());
        let changed = removed > 0;
        drop(devices);
        if changed {
            eprintln!("Removed {removed} stale device(s)");
            self.revision.fetch_add(1, Ordering::SeqCst);
            self.emit(Event::DevicesChanged);
        }
        changed
    }

    pub fn devices_for_probe(&self) -> Vec<(String, DeviceInfo, String)> {
        self.devices
            .lock()
            .unwrap()
            .iter()
            .map(|(key, device)| (key.clone(), device.info.clone(), device.ip.clone()))
            .collect()
    }

    pub fn mark_device_probe_failed(&self, key: &str) -> bool {
        let mut devices = self.devices.lock().unwrap();
        let mut remove = false;
        if let Some(device) = devices.get_mut(key) {
            device.failed_probes = device.failed_probes.saturating_add(1);
            remove = device.failed_probes >= 2;
        }
        if remove {
            devices.remove(key);
        }
        drop(devices);
        if remove {
            self.unreachable.lock().unwrap().insert(key.to_string());
            eprintln!("Removed an unreachable device");
            self.revision.fetch_add(1, Ordering::SeqCst);
            self.emit(Event::DevicesChanged);
        }
        remove
    }

    /// Hides a peer whose transfer server refused a connection. Discovery
    /// broadcasts alone do not bring it back; a successful /register or /info
    /// response does.
    pub fn mark_device_unreachable(&self, key: &str) {
        let removed = self.devices.lock().unwrap().remove(key).is_some();
        self.unreachable.lock().unwrap().insert(key.to_string());
        if removed {
            eprintln!("Removed a device that is not accepting transfers");
            self.revision.fetch_add(1, Ordering::SeqCst);
            self.emit(Event::DevicesChanged);
        }
    }

    pub fn reset_device_probe_failures(&self, key: &str) {
        if let Some(device) = self.devices.lock().unwrap().get_mut(key) {
            device.failed_probes = 0;
        }
    }

    pub fn devices_sorted(&self) -> Vec<DeviceSnapshot> {
        let devices = self.devices.lock().unwrap();
        let mut out: Vec<DeviceSnapshot> = devices
            .iter()
            .map(|(key, device)| DeviceSnapshot {
                key: key.clone(),
                alias: device.info.alias.clone(),
                device_type: device.info.device_type().to_string(),
                ip: device.ip.clone(),
            })
            .collect();
        drop(devices);
        out.sort_by(|a, b| a.alias.to_lowercase().cmp(&b.alias.to_lowercase()));
        out
    }

    pub fn device(&self, key: &str) -> Option<(DeviceInfo, String)> {
        self.devices
            .lock()
            .unwrap()
            .get(key)
            .map(|device| (device.info.clone(), device.ip.clone()))
    }

    pub fn begin_prepare(&self) -> bool {
        self.preparing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn end_prepare(&self) {
        self.preparing.store(false, Ordering::SeqCst);
    }

    pub fn has_active_session(&self) -> bool {
        !self.sessions.lock().unwrap().is_empty()
    }

    /// Sessions whose sender disappeared must not block new transfers forever.
    /// Returns the sessions that were dropped so the UI can stop showing them.
    pub fn prune_stale_sessions(&self, ttl: Duration) -> Vec<(String, String)> {
        let mut sessions = self.sessions.lock().unwrap();
        let stale: Vec<(String, String)> = sessions
            .iter()
            .filter(|(_, session)| session.idle() > ttl)
            .map(|(id, session)| (id.clone(), session.peer_alias.clone()))
            .collect();
        for (id, _) in &stale {
            sessions.remove(id);
        }
        stale
    }

    /// Lets the `/cancel` endpoint stop an outgoing transfer: the receiver can
    /// cancel while we are still streaming, and the sender must stop at once.
    pub fn register_outgoing(&self, session_id: &str, cancel: Arc<AtomicBool>) {
        self.outgoing
            .lock()
            .unwrap()
            .insert(session_id.to_string(), cancel);
    }

    pub fn unregister_outgoing(&self, session_id: &str) {
        self.outgoing.lock().unwrap().remove(session_id);
    }

    pub fn cancel_outgoing(&self, session_id: &str) -> bool {
        match self.outgoing.lock().unwrap().get(session_id) {
            Some(cancel) => {
                cancel.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    pub fn history_snapshot(&self) -> Vec<HistoryEntry> {
        self.history.lock().unwrap().entries().to_vec()
    }

    pub fn history_usage(&self) -> u64 {
        self.history.lock().unwrap().usage_bytes()
    }

    pub fn history_clear_all(&self) {
        self.history.lock().unwrap().clear_all();
        self.emit(Event::HistoryChanged);
    }

    pub fn history_add_entry(&self, entry: HistoryEntry) {
        self.history.lock().unwrap().add_entry(entry);
        self.emit(Event::HistoryChanged);
    }

    pub fn history_resolve(&self, item_id: &str) -> Resolution {
        self.history.lock().unwrap().resolve(item_id)
    }

    pub fn history_remove_item(&self, item_id: &str) -> RemoveOutcome {
        let outcome = self.history.lock().unwrap().remove_item(item_id);
        // Always notify so an open history view can reflect the current state,
        // even if the requested item was already gone.
        self.emit(Event::HistoryChanged);
        outcome
    }

    pub fn history_delete_file(&self, item_id: &str) -> Result<(), String> {
        let result = self.history.lock().unwrap().delete_item_file(item_id);
        self.emit(Event::HistoryChanged);
        result
    }

    pub fn history_cache_root(&self) -> PathBuf {
        self.history.lock().unwrap().cache_root().to_path_buf()
    }

    pub fn history_record_item(&self, session_id: &str, alias: &str, item: HistoryItem) {
        let mut history = self.history.lock().unwrap();
        if let Some(entry) = history.entries.iter_mut().find(|e| e.id == session_id) {
            if !entry.items.iter().any(|i| i.id() == item.id()) {
                entry.items.push(item);
            }
        } else {
            history.entries.insert(
                0,
                HistoryEntry {
                    id: session_id.to_string(),
                    alias: alias.to_string(),
                    timestamp: crate::history::now_unix(),
                    items: vec![item],
                },
            );
        }
        history.evict();
        let _ = history.save();
        drop(history);
        self.emit(Event::HistoryChanged);
    }

    pub fn insert_session(&self, session: Arc<ReceiveSession>) {
        self.sessions
            .lock()
            .unwrap()
            .insert(session.id.clone(), session);
    }

    pub fn take_session(&self, id: &str) -> Option<Arc<ReceiveSession>> {
        self.sessions.lock().unwrap().remove(id)
    }

    pub fn receive_file_names(&self, session_id: &str) -> Option<Vec<String>> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(session_id)?;
        Some(session.files.iter().map(|file| file.name.clone()).collect())
    }

    /// Names plus completion state, so the UI can mark which files of a
    /// multi-file receive are done and which are still waiting.
    pub fn receive_file_done(&self, session_id: &str) -> Option<Vec<(String, bool)>> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(session_id)?;
        Some(
            session
                .files
                .iter()
                .map(|file| (file.name.clone(), file.done.load(Ordering::SeqCst)))
                .collect(),
        )
    }

    pub fn find_session_file(
        &self,
        session_id: &str,
        file_id: &str,
        token: &str,
    ) -> Option<(Arc<ReceiveSession>, Arc<ReceiveFile>)> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(session_id)?.clone();
        let file = session.find_file(file_id)?;
        if file.token != token {
            return None;
        }
        Some((session, file))
    }
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn removes_device_after_failed_liveness_probes() {
        let core = Core::for_test(Config::default());
        let mut info = DeviceInfo::default();
        info.alias = "Unreachable Device".to_string();
        info.fingerprint = "unreachable-device-fingerprint".to_string();
        core.upsert_device(info, "192.0.2.2".to_string());
        let key = core.devices_sorted()[0].key.clone();

        assert!(!core.mark_device_probe_failed(&key));
        assert_eq!(core.devices_sorted().len(), 1);
        assert!(core.mark_device_probe_failed(&key));
        assert!(core.devices_sorted().is_empty());
    }

    #[test]
    fn successful_probe_resets_failure_count() {
        let core = Core::for_test(Config::default());
        let mut info = DeviceInfo::default();
        info.alias = "Flaky Device".to_string();
        info.fingerprint = "flaky-device-fingerprint".to_string();
        core.upsert_device(info.clone(), "192.0.2.3".to_string());
        let key = core.devices_sorted()[0].key.clone();

        assert!(!core.mark_device_probe_failed(&key));
        core.upsert_device_verified(info, "192.0.2.3".to_string());
        assert!(!core.mark_device_probe_failed(&key));
        assert_eq!(core.devices_sorted().len(), 1);
    }

    #[test]
    fn unreachable_device_stays_hidden_until_it_answers_again() {
        let core = Core::for_test(Config::default());
        let mut info = DeviceInfo::default();
        info.alias = "Refusing Device".to_string();
        info.fingerprint = "refusing-device-fingerprint".to_string();
        core.upsert_device(info.clone(), "192.0.2.4".to_string());
        let key = core.devices_sorted()[0].key.clone();

        core.mark_device_unreachable(&key);
        assert!(core.devices_sorted().is_empty());

        assert!(!core.upsert_device(info.clone(), "192.0.2.4".to_string()));
        assert!(core.devices_sorted().is_empty());

        assert!(core.upsert_device_verified(info, "192.0.2.4".to_string()));
        assert_eq!(core.devices_sorted().len(), 1);
    }

    #[test]
    fn cancel_endpoint_stops_an_outgoing_transfer() {
        let core = Core::for_test(Config::default());
        let flag = Arc::new(AtomicBool::new(false));
        core.register_outgoing("session-1", flag.clone());

        assert!(!core.cancel_outgoing("unknown-session"));
        assert!(!flag.load(Ordering::SeqCst));

        assert!(core.cancel_outgoing("session-1"));
        assert!(flag.load(Ordering::SeqCst));

        core.unregister_outgoing("session-1");
        assert!(!core.cancel_outgoing("session-1"));
    }

    #[test]
    fn removes_devices_that_stop_announcing() {
        let core = Core::for_test(Config::default());
        let mut info = DeviceInfo::default();
        info.alias = "Gone Device".to_string();
        info.fingerprint = "gone-device-fingerprint".to_string();
        assert!(core.upsert_device(info, "192.0.2.1".to_string()));
        assert_eq!(core.devices_sorted().len(), 1);

        {
            let mut devices = core.devices.lock().unwrap();
            for device in devices.values_mut() {
                device.last_seen = Instant::now() - Duration::from_secs(60);
            }
        }

        assert!(core.remove_stale_devices(Duration::from_secs(20)));
        assert!(core.devices_sorted().is_empty());
    }
}
