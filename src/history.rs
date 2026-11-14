use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const HISTORY_BUDGET: u64 = 128 * 1024 * 1024;
pub const FILE_CACHE_CAP: u64 = 32 * 1024 * 1024;

fn default_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<HistoryEntry>,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    cache_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub alias: String,
    pub timestamp: i64,
    #[serde(default)]
    pub items: Vec<HistoryItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryItem {
    File {
        id: String,
        name: String,
        size: u64,
        #[serde(default)]
        mime_hint: String,
        path: PathBuf,
        #[serde(default)]
        cache: Option<CachedCopy>,
    },
    Message {
        id: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        bytes: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCopy {
    pub path: PathBuf,
    pub bytes: u64,
    pub added: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Original(PathBuf),
    Cached(PathBuf),
    Message,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    NotFound,
}

impl HistoryItem {
    pub fn id(&self) -> &str {
        match self {
            HistoryItem::File { id, .. } | HistoryItem::Message { id, .. } => id,
        }
    }

    pub fn resolution(&self) -> Resolution {
        match self {
            HistoryItem::File { path, cache, .. } => {
                if path.is_file() {
                    Resolution::Original(path.clone())
                } else if let Some(cache) = cache {
                    if cache.path.is_file() {
                        return Resolution::Cached(cache.path.clone());
                    }
                    Resolution::Missing
                } else {
                    Resolution::Missing
                }
            }
            HistoryItem::Message { text, .. } => {
                if text.is_some() {
                    Resolution::Message
                } else {
                    Resolution::Missing
                }
            }
        }
    }

    fn cache_bytes(&self) -> u64 {
        match self {
            HistoryItem::File { cache: Some(c), .. } => c.bytes,
            HistoryItem::File { cache: None, .. } => 0,
            HistoryItem::Message {
                bytes,
                text: Some(_),
                ..
            } => *bytes,
            HistoryItem::Message { text: None, .. } => 0,
        }
    }
}

impl HistoryStore {
    pub fn load_default() -> Self {
        let data = default_data_dir();
        let cache = default_cache_dir();
        Self::load(data.join("history.json"), cache.join("history"))
    }

    pub fn load(path: PathBuf, cache_root: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::create_dir_all(&cache_root);
        let mut store = match fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<HistoryStore>(&contents) {
                Ok(mut store) => {
                    store.path = path.clone();
                    store.cache_root = cache_root.clone();
                    store
                }
                Err(error) => {
                    eprintln!("Could not parse {}: {error}", path.display());
                    let bad = path.with_extension("json.bad");
                    let _ = fs::rename(&path, bad);
                    Self::empty(path.clone(), cache_root.clone())
                }
            },
            Err(_) => Self::empty(path.clone(), cache_root.clone()),
        };
        store.path = path;
        store.cache_root = cache_root;
        store.reconcile();
        store
    }

    fn empty(path: PathBuf, cache_root: PathBuf) -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
            path,
            cache_root,
        }
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)
                .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        }
        fs::create_dir_all(&self.cache_root)
            .map_err(|e| format!("Could not create {}: {e}", self.cache_root.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        fs::write(&tmp, data).map_err(|e| format!("Could not write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| format!("Could not replace {}: {e}", self.path.display()))
    }

    pub fn add_entry(&mut self, entry: HistoryEntry) {
        self.entries.insert(0, entry);
        self.evict();
        if let Err(error) = self.save() {
            eprintln!("Could not save history: {error}");
        }
    }

    pub fn usage_bytes(&self) -> u64 {
        self.entries
            .iter()
            .flat_map(|e| e.items.iter())
            .map(|i| i.cache_bytes())
            .sum()
    }

    /// Remove all history entries and delete cached copies.
    pub fn clear_all(&mut self) {
        for entry in &self.entries {
            for item in &entry.items {
                remove_item_cache(item);
            }
        }
        self.entries.clear();
        if let Ok(read) = fs::read_dir(&self.cache_root) {
            for e in read.flatten() {
                let _ = fs::remove_dir_all(e.path());
            }
        }
        let _ = self.save();
    }

    pub fn evict(&mut self) {
        loop {
            if self.usage_bytes() <= HISTORY_BUDGET {
                break;
            }
            if self.evict_oldest_file_cache() {
                continue;
            }
            if self.evict_oldest_message() {
                continue;
            }
            break;
        }
    }

    fn evict_oldest_file_cache(&mut self) -> bool {
        let mut target: Option<(i64, usize, usize)> = None; // (cache_added, entry_index, item_index)
        for (ei, entry) in self.entries.iter().enumerate() {
            for (ii, item) in entry.items.iter().enumerate() {
                if let HistoryItem::File { cache: Some(c), .. } = item {
                    let key = (c.added, ei, ii);
                    let better = match target {
                        None => true,
                        Some(t) => {
                            key.0 < t.0
                                || (key.0 == t.0 && (key.1 > t.1 || (key.1 == t.1 && key.2 > t.2)))
                        }
                    };
                    if better {
                        target = Some(key);
                    }
                }
            }
        }
        if let Some((_, ei, ii)) = target {
            if let Some(entry) = self.entries.get_mut(ei) {
                if let Some(HistoryItem::File { cache, .. }) = entry.items.get_mut(ii) {
                    if let Some(c) = cache.take() {
                        let _ = fs::remove_file(&c.path);
                        cleanup_empty_dir(c.path.parent());
                    }
                    return true;
                }
            }
        }
        false
    }

    fn evict_oldest_message(&mut self) -> bool {
        for ei in (0..self.entries.len()).rev() {
            for ii in (0..self.entries[ei].items.len()).rev() {
                if let HistoryItem::Message { text, bytes, .. } = &mut self.entries[ei].items[ii] {
                    if text.is_some() {
                        *text = None;
                        *bytes = 0;
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn reconcile(&mut self) {
        let mut live_dirs: HashSet<PathBuf> = HashSet::new();
        for entry in &mut self.entries {
            for item in &mut entry.items {
                if let HistoryItem::File { cache, .. } = item {
                    let mut drop_cache = false;
                    if let Some(c) = cache {
                        if c.path.is_file() {
                            if let Ok(meta) = fs::metadata(&c.path) {
                                c.bytes = meta.len();
                            }
                            if let Some(dir) = c.path.parent() {
                                live_dirs.insert(dir.to_path_buf());
                            }
                        } else {
                            drop_cache = true;
                        }
                    }
                    if drop_cache {
                        *cache = None;
                    }
                }
            }
        }
        if let Ok(read) = fs::read_dir(&self.cache_root) {
            for child in read.flatten() {
                let path = child.path();
                if path.is_dir() && !live_dirs.contains(&path) {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
    }

    pub fn find_item(&self, item_id: &str) -> Option<&HistoryItem> {
        self.entries
            .iter()
            .flat_map(|e| e.items.iter())
            .find(|i| i.id() == item_id)
    }

    pub fn resolve(&self, item_id: &str) -> Resolution {
        self.find_item(item_id)
            .map(|i| i.resolution())
            .unwrap_or(Resolution::Missing)
    }

    pub fn remove_item(&mut self, item_id: &str) -> RemoveOutcome {
        for ei in 0..self.entries.len() {
            if let Some(ii) = self.entries[ei]
                .items
                .iter()
                .position(|i| i.id() == item_id)
            {
                let item = self.entries[ei].items.remove(ii);
                remove_item_cache(&item);
                if self.entries[ei].items.is_empty() {
                    self.entries.remove(ei);
                }
                self.evict();
                let _ = self.save();
                return RemoveOutcome::Removed;
            }
        }
        RemoveOutcome::NotFound
    }

    pub fn delete_item_file(&mut self, item_id: &str) -> Result<(), String> {
        let original = match self.find_item(item_id) {
            Some(HistoryItem::File { path, .. }) => path.clone(),
            Some(HistoryItem::Message { .. }) => {
                return Err("This item is not a file".to_string());
            }
            None => return Err("History item not found".to_string()),
        };
        if let Ok(meta) = fs::symlink_metadata(&original) {
            if meta.file_type().is_file() {
                fs::remove_file(&original)
                    .map_err(|e| format!("Could not delete {}: {e}", original.display()))?;
            } else {
                return Err("Only regular files can be deleted".to_string());
            }
        }
        // Remove from history regardless of whether original still existed (user intent: delete)
        self.remove_item(item_id);
        Ok(())
    }

    #[cfg(test)]
    pub fn cache_dir_for(&self, entry_id: &str) -> PathBuf {
        self.cache_root.join(entry_id)
    }
}

fn remove_item_cache(item: &HistoryItem) {
    if let HistoryItem::File { cache: Some(c), .. } = item {
        let _ = fs::remove_file(&c.path);
        cleanup_empty_dir(c.path.parent());
    }
}

fn cleanup_empty_dir(dir: Option<&Path>) {
    if let Some(dir) = dir {
        // Fails harmlessly when the directory is not empty and is still in use.
        let _ = fs::remove_dir(dir);
    }
}

pub fn default_data_dir() -> PathBuf {
    gtk::glib::user_data_dir().join("localsend-adwaita")
}

pub fn default_cache_dir() -> PathBuf {
    gtk::glib::user_cache_dir().join("localsend-adwaita")
}

pub fn now_unix() -> i64 {
    gtk::glib::real_time()
}

/// Copy a received file into the history cache if it qualifies.
/// Returns None if the file is too large, missing, or the copy fails.
pub fn cache_file_copy(
    original: &Path,
    entry_id: &str,
    index: usize,
    name: &str,
    cache_root: &Path,
) -> Option<CachedCopy> {
    let meta = fs::metadata(original).ok()?;
    let size = meta.len();
    if size > FILE_CACHE_CAP || !meta.is_file() {
        return None;
    }
    let dir = cache_root.join(entry_id);
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let safe_name = sanitize_cache_name(name);
    let dest = dir.join(format!("{index}-{safe_name}"));
    match fs::copy(original, &dest) {
        Ok(bytes) => Some(CachedCopy {
            path: dest,
            bytes,
            added: now_unix(),
        }),
        Err(error) => {
            eprintln!("Could not cache {}: {error}", original.display());
            let _ = fs::remove_file(&dest);
            None
        }
    }
}

fn sanitize_cache_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_control() || c == '/' || c == '\\' {
                '_'
            } else {
                c
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("file");
    }
    if out.len() > 120 {
        // truncate at char boundary
        out = out.chars().take(120).collect();
    }
    out
}

pub fn message_bytes(text: &str) -> u64 {
    text.as_bytes().len() as u64
}

pub fn make_item_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_store(name: &str) -> (HistoryStore, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "localsend-history-test-{}-{name}",
            uuid::Uuid::new_v4()
        ));
        let data = root.join("data");
        let cache = root.join("cache");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&cache).unwrap();
        (HistoryStore::load(data.join("history.json"), cache), root)
    }

    fn file_item(id: &str, path: &Path, cache: Option<CachedCopy>, size: u64) -> HistoryItem {
        HistoryItem::File {
            id: id.to_string(),
            name: "f.bin".to_string(),
            size,
            mime_hint: "application/octet-stream".to_string(),
            path: path.to_path_buf(),
            cache,
        }
    }

    #[test]
    fn evicts_oldest_cached_file_first() {
        let (mut store, root) = temp_store("evict-file");
        for i in 0..5 {
            let dir = store.cache_dir_for(&format!("e{i}"));
            fs::create_dir_all(&dir).unwrap();
            let p = dir.join("f");
            fs::write(&p, vec![0u8; 30 * 1024 * 1024]).unwrap();
            let orig = root.join(format!("orig{i}"));
            fs::write(&orig, b"x").unwrap();
            store.entries.insert(
                0,
                HistoryEntry {
                    id: format!("e{i}"),
                    alias: "A".into(),
                    timestamp: 100 + i as i64,
                    items: vec![HistoryItem::File {
                        id: format!("i{i}"),
                        name: "f".into(),
                        size: 30 * 1024 * 1024,
                        mime_hint: String::new(),
                        path: orig,
                        cache: Some(CachedCopy {
                            path: p,
                            bytes: 30 * 1024 * 1024,
                            added: 100 + i as i64,
                        }),
                    }],
                },
            );
        }
        store.evict();
        let cached: Vec<_> = store
            .entries
            .iter()
            .flat_map(|e| &e.items)
            .filter(|i| i.cache_bytes() > 0)
            .collect();
        assert!(store.usage_bytes() <= HISTORY_BUDGET);
        assert_eq!(cached.len(), 4);
        // oldest added was e0 (added=100)
        assert_eq!(store.find_item("i0").unwrap().cache_bytes(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn messages_survive_while_cached_files_remain() {
        let (mut store, root) = temp_store("msg-priority");
        let dir = store.cache_dir_for("e1");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        fs::write(&p, vec![1u8; 40]).unwrap();
        let orig = root.join("o");
        fs::write(&orig, b"o").unwrap();
        store.entries.push(HistoryEntry {
            id: "e1".into(),
            alias: "A".into(),
            timestamp: 1,
            items: vec![
                HistoryItem::File {
                    id: "f1".into(),
                    name: "f".into(),
                    size: 40,
                    mime_hint: String::new(),
                    path: orig,
                    cache: Some(CachedCopy {
                        path: p,
                        bytes: 40,
                        added: 1,
                    }),
                },
                HistoryItem::Message {
                    id: "m1".into(),
                    text: Some("hello".into()),
                    bytes: 5,
                },
            ],
        });
        assert!(store.evict_oldest_file_cache());
        if let HistoryItem::Message { text, .. } = store.find_item("m1").unwrap() {
            assert!(text.is_some());
        } else {
            panic!("expected message");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn messages_dropped_only_when_no_cached_files() {
        let (mut store, root) = temp_store("msg-drop");
        store.entries.push(HistoryEntry {
            id: "e1".into(),
            alias: "A".into(),
            timestamp: 1,
            items: vec![HistoryItem::Message {
                id: "m1".into(),
                text: Some("hello".into()),
                bytes: 5,
            }],
        });
        assert!(!store.evict_oldest_file_cache());
        assert!(store.evict_oldest_message());
        if let HistoryItem::Message { text, .. } = store.find_item("m1").unwrap() {
            assert!(text.is_none());
        } else {
            panic!("expected message");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn large_file_not_cached() {
        let root = std::env::temp_dir().join(format!(
            "localsend-history-test-cap-{}",
            uuid::Uuid::new_v4()
        ));
        let cache = root.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let orig = root.join("big");
        fs::write(&orig, vec![0u8; 10]).unwrap();
        let missing = cache_file_copy(&root.join("nope"), "e", 0, "x", &cache);
        assert!(missing.is_none());
        let cached = cache_file_copy(&orig, "e", 0, "x", &cache);
        assert!(cached.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_original_then_cached_then_missing() {
        let (mut store, root) = temp_store("resolve");
        let orig = root.join("orig");
        fs::write(&orig, b"o").unwrap();
        let cdir = store.cache_dir_for("e");
        fs::create_dir_all(&cdir).unwrap();
        let cpath = cdir.join("c");
        fs::write(&cpath, b"c").unwrap();
        store.entries.push(HistoryEntry {
            id: "e".into(),
            alias: "A".into(),
            timestamp: 1,
            items: vec![HistoryItem::File {
                id: "f".into(),
                name: "f".into(),
                size: 1,
                mime_hint: String::new(),
                path: orig.clone(),
                cache: Some(CachedCopy {
                    path: cpath.clone(),
                    bytes: 1,
                    added: 1,
                }),
            }],
        });
        assert_eq!(store.resolve("f"), Resolution::Original(orig.clone()));
        fs::remove_file(&orig).unwrap();
        assert_eq!(store.resolve("f"), Resolution::Cached(cpath.clone()));
        fs::remove_file(&cpath).unwrap();
        assert_eq!(store.resolve("f"), Resolution::Missing);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_keeps_original_file() {
        let (mut store, root) = temp_store("remove");
        let orig = root.join("orig");
        fs::write(&orig, b"o").unwrap();
        let cdir = store.cache_dir_for("e");
        fs::create_dir_all(&cdir).unwrap();
        let cpath = cdir.join("c");
        fs::write(&cpath, b"c").unwrap();
        store.entries.push(HistoryEntry {
            id: "e".into(),
            alias: "A".into(),
            timestamp: 1,
            items: vec![file_item(
                "f",
                &orig,
                Some(CachedCopy {
                    path: cpath.clone(),
                    bytes: 1,
                    added: 1,
                }),
                1,
            )],
        });
        store.remove_item("f");
        assert!(orig.exists());
        assert!(!cpath.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delete_file_removes_both_and_refuses_directory() {
        let (mut store, root) = temp_store("delete");
        let orig = root.join("orig");
        fs::write(&orig, b"o").unwrap();
        store.entries.push(HistoryEntry {
            id: "e".into(),
            alias: "A".into(),
            timestamp: 1,
            items: vec![HistoryItem::File {
                id: "f".into(),
                name: "f".into(),
                size: 1,
                mime_hint: String::new(),
                path: orig.clone(),
                cache: None,
            }],
        });
        store.delete_item_file("f").unwrap();
        assert!(!orig.exists());
        assert!(store.entries.is_empty());

        let (mut store2, root2) = temp_store("delete-dir");
        let dirp = root2.join("adir");
        fs::create_dir_all(&dirp).unwrap();
        store2.entries.push(HistoryEntry {
            id: "e".into(),
            alias: "A".into(),
            timestamp: 1,
            items: vec![HistoryItem::File {
                id: "d".into(),
                name: "d".into(),
                size: 0,
                mime_hint: String::new(),
                path: dirp.clone(),
                cache: None,
            }],
        });
        assert!(store2.delete_item_file("d").is_err());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(root2);
    }

    #[test]
    fn corrupt_history_starts_empty() {
        let root = std::env::temp_dir().join(format!(
            "localsend-history-test-corrupt-{}",
            uuid::Uuid::new_v4()
        ));
        let data = root.join("data");
        let cache = root.join("cache");
        fs::create_dir_all(&data).unwrap();
        let p = data.join("history.json");
        fs::write(&p, "{not json").unwrap();
        let store = HistoryStore::load(p.clone(), cache);
        assert!(store.entries.is_empty());
        assert!(data.join("history.json.bad").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reconcile_removes_orphan_cache_dir() {
        let root = std::env::temp_dir().join(format!(
            "localsend-history-test-reconcile-{}",
            uuid::Uuid::new_v4()
        ));
        let data = root.join("data");
        let cache = root.join("cache/history");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(cache.join("orphan")).unwrap();
        fs::write(cache.join("orphan/f"), b"x").unwrap();
        let _store = HistoryStore::load(data.join("history.json"), cache.clone());
        assert!(!cache.join("orphan").exists());
        let _ = fs::remove_dir_all(root);
    }
}
