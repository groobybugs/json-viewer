//! Tab persistence, replacing the old IndexedDB `tabs` store.
//!
//! Layout (versioned, debounce-friendly):
//!
//! * `<base>/tabs.json` — a small **index**: `{ version, tabs: [{ id, hash }] }`
//!   recording tab order and a content fingerprint per tab.
//! * `<base>/tabs.d/<id>.json` — one **payload** file per tab holding the full
//!   [`Tab`].
//!
//! [`TabsStore::save`] only rewrites payloads whose fingerprint changed, so
//! calling it often (e.g. after every debounced keystroke) is cheap: unchanged
//! tabs are skipped entirely and a 100 MB tab is never re-serialized just
//! because a *different* tab changed. Every file is written atomically
//! (sibling temp file + rename), so a crash or a failed save never leaves a
//! truncated store behind. The whole operation is safe to run on a background
//! thread.
//!
//! The default location is the platform data directory
//! (`~/.local/share/json-viewer/` on Linux); [`TabsStore`] accepts an explicit
//! base directory so tests never touch the real profile. A missing store loads
//! as an empty list (first run). A corrupt or inconsistent store is returned as
//! a [`PersistError`] so the caller can surface it and still start the app.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hasher;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustc_hash::FxHasher;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::state::{Tab, TabId, TabView};

/// On-disk format version. Bump when the index/payload shape changes; an
/// unknown version is treated as a corrupt store rather than guessed at.
const FORMAT_VERSION: u32 = 1;

/// Why loading or saving tabs failed.
#[derive(Debug, Error)]
pub enum PersistError {
    /// Reading or writing the file failed.
    #[error("cannot access tab store: {0}")]
    Io(#[from] io::Error),
    /// The file exists but is not a valid tab list.
    #[error("invalid tab store: {0}")]
    Json(#[from] serde_json::Error),
}

/// The tab index file used by the app.
///
/// Falls back to `.json-viewer/tabs.json` in the current directory when the
/// platform data directory cannot be determined.
pub fn data_path() -> PathBuf {
    directories::ProjectDirs::from("dev", "anomaly", "json-viewer")
        .map(|dirs| dirs.data_dir().join("tabs.json"))
        .unwrap_or_else(|| PathBuf::from(".json-viewer").join("tabs.json"))
}

/// Versioned index written to the store's main file.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Index {
    version: u32,
    tabs: Vec<IndexEntry>,
}

/// One tab's id and content fingerprint, in display order.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct IndexEntry {
    id: TabId,
    hash: u64,
}

/// A tab store rooted at a directory. `TabsStore::new(dir)` stores
/// `dir/tabs.json` (index) plus `dir/tabs.d/<id>.json` (payloads);
/// `TabsStore::from_file(path)` stores exactly `path` as the index and derives
/// the payload directory from its stem.
#[derive(Debug, Clone)]
pub struct TabsStore {
    file: PathBuf,
}

impl TabsStore {
    /// Creates a store writing `base_dir/tabs.json`.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        Self {
            file: base_dir.as_ref().join("tabs.json"),
        }
    }

    /// Creates a store writing an exact path.
    pub fn from_file(file: impl Into<PathBuf>) -> Self {
        Self { file: file.into() }
    }

    /// The index file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// Loads tabs in index order. A missing index is an empty list (first
    /// run), not an error. A corrupt index, an unsupported version, or a
    /// missing/corrupt payload is a [`PersistError`] the caller can surface
    /// while still starting the app.
    pub fn load(&self) -> Result<Vec<Tab>, PersistError> {
        let text = match fs::read_to_string(&self.file) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(PersistError::Io(error)),
        };
        let index: Index = serde_json::from_str(&text)?;
        if index.version != FORMAT_VERSION {
            return Err(PersistError::Json(serde_json::Error::custom(format!(
                "unsupported tab store version {}",
                index.version
            ))));
        }
        let mut tabs = Vec::with_capacity(index.tabs.len());
        for entry in &index.tabs {
            let payload = fs::read_to_string(self.payload_path(entry.id))?;
            let tab: Tab = serde_json::from_str(&payload)?;
            if tab.id != entry.id {
                return Err(PersistError::Json(serde_json::Error::custom(format!(
                    "tab payload id {} does not match index entry {}",
                    tab.id, entry.id
                ))));
            }
            tabs.push(tab);
        }
        Ok(tabs)
    }

    /// Writes tabs atomically, rewriting only changed payloads.
    ///
    /// Each payload and the index are written to a unique sibling temp file
    /// and renamed over the destination, so a crash cannot leave a truncated
    /// file. Payloads are written before the index; if any write fails the
    /// index is left untouched and the error is returned. Payloads for tabs no
    /// longer present are removed. Safe to call from a background thread.
    pub fn save(&self, tabs: &[Tab]) -> Result<(), PersistError> {
        let parent = self
            .file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let payload_dir = self.payload_dir();
        fs::create_dir_all(&payload_dir)?;

        // A corrupt/old index is treated as "nothing saved yet": rewrite all.
        let old = self.read_index();
        let old_hashes: HashMap<TabId, u64> = old
            .as_ref()
            .map(|index| index.tabs.iter().map(|e| (e.id, e.hash)).collect())
            .unwrap_or_default();

        let mut entries = Vec::with_capacity(tabs.len());
        let mut keep = HashSet::with_capacity(tabs.len());
        for tab in tabs {
            let hash = fingerprint(tab);
            entries.push(IndexEntry { id: tab.id, hash });
            keep.insert(tab.id);
            let path = self.payload_path(tab.id);
            if old_hashes.get(&tab.id) == Some(&hash) && path.exists() {
                continue; // unchanged: skip serialization and write entirely
            }
            let bytes = serde_json::to_vec(tab)?;
            atomic_write(&path, &bytes)?;
        }

        // Drop payload files for tabs that are gone (best effort).
        if let Ok(read_dir) = fs::read_dir(&payload_dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(id) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<TabId>().ok()) {
                    if !keep.contains(&id) {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }

        let index = Index { version: FORMAT_VERSION, tabs: entries };
        if old.as_ref() != Some(&index) {
            let bytes = serde_json::to_vec_pretty(&index)?;
            atomic_write(&self.file, &bytes)?;
        }
        Ok(())
    }

    /// Directory holding per-tab payload files, derived from the index stem.
    fn payload_dir(&self) -> PathBuf {
        let stem = self.file.file_stem().and_then(|s| s.to_str()).unwrap_or("tabs");
        let parent = self
            .file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        parent.join(format!("{stem}.d"))
    }

    /// Payload file for one tab id.
    fn payload_path(&self, id: TabId) -> PathBuf {
        self.payload_dir().join(format!("{id}.json"))
    }

    /// Reads the index, returning `None` when missing, corrupt, or an
    /// unsupported version (so [`Self::save`] rewrites everything).
    fn read_index(&self) -> Option<Index> {
        let text = fs::read_to_string(&self.file).ok()?;
        let index: Index = serde_json::from_str(&text).ok()?;
        if index.version != FORMAT_VERSION {
            return None;
        }
        Some(index)
    }
}

/// Deterministic content fingerprint of a tab. Covers every persisted field so
/// any edit triggers a rewrite; uses a fast fixed-seed hasher so fingerprinting
/// a 100 MB tab stays cheap and matches across process restarts.
fn fingerprint(tab: &Tab) -> u64 {
    let mut h = FxHasher::default();
    h.write_u64(tab.id);
    hash_str(&mut h, &tab.title);
    hash_str(&mut h, &tab.input);
    hash_str(&mut h, &tab.output);
    hash_opt_str(&mut h, tab.error.as_deref());
    h.write_u8(u8::from(tab.is_processing));
    h.write_u8(match tab.view {
        TabView::Input => 0,
        TabView::Output => 1,
    });
    hash_str(&mut h, &tab.search_query);
    h.write_usize(tab.search_results.len());
    for hit in &tab.search_results {
        hash_str(&mut h, &hit.path);
        hash_opt_str(&mut h, hit.key.as_deref());
        hash_str(&mut h, &hit.value_preview);
        match hit.line {
            Some(line) => {
                h.write_u8(1);
                h.write_usize(line);
            }
            None => h.write_u8(0),
        }
    }
    h.finish()
}

fn hash_str(h: &mut FxHasher, s: &str) {
    h.write_usize(s.len());
    h.write(s.as_bytes());
}

fn hash_opt_str(h: &mut FxHasher, s: Option<&str>) {
    match s {
        Some(s) => {
            h.write_u8(1);
            hash_str(h, s);
        }
        None => h.write_u8(0),
    }
}

/// Monotonic counter making temp file names unique across concurrent saves.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_temp(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("store");
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!("{name}.{}.{}.tmp", std::process::id(), seq))
}

/// Writes `bytes` to `path` atomically: a unique sibling temp file is renamed
/// over the destination. On any failure the temp file is removed and the
/// destination is left exactly as it was.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = unique_temp(path);
    let result = fs::write(&temp, bytes).and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Loads tabs from [`data_path`].
pub fn load_tabs() -> Result<Vec<Tab>, PersistError> {
    TabsStore::from_file(data_path()).load()
}

/// Saves tabs to [`data_path`].
pub fn save_tabs(tabs: &[Tab]) -> Result<(), PersistError> {
    TabsStore::from_file(data_path()).save(tabs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::SearchHit;
    use std::time::Instant;

    fn tab(id: TabId, title: &str, input: &str) -> Tab {
        let mut tab = Tab::new(id, title);
        tab.input = input.into();
        tab.output = input.into();
        tab
    }

    /// File identity that changes when a file is replaced by rename (inode on
    /// unix, size+mtime elsewhere). Used to prove only changed files rewrite.
    fn identity(path: &Path) -> (u64, std::time::SystemTime) {
        let meta = fs::metadata(path).unwrap();
        let modified = meta.modified().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            (meta.ino(), modified)
        }
        #[cfg(not(unix))]
        {
            (meta.len(), modified)
        }
    }

    fn has_tmp(dir: &Path) -> bool {
        fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            })
            .unwrap_or(false)
    }

    #[test]
    fn missing_store_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        assert!(store.load().unwrap().is_empty());
        assert_eq!(store.path(), dir.path().join("tabs.json"));
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        let mut first = tab(3, "JSON 1", "{\"a\":1}");
        first.error = Some("bad".into());
        first.is_processing = true;
        first.view = TabView::Output;
        first.search_query = "a".into();
        first.search_results.push(SearchHit {
            path: "a".into(),
            key: Some("a".into()),
            value_preview: "1".into(),
            line: Some(2),
        });
        let tabs = vec![first, tab(9, "JSON 2", "[]")];

        store.save(&tabs).unwrap();
        assert_eq!(store.load().unwrap(), tabs);
        // Per-tab payloads exist; no temp debris remains after the renames.
        assert!(store.payload_path(3).exists());
        assert!(store.payload_path(9).exists());
        assert!(!has_tmp(dir.path()));
        assert!(!has_tmp(&store.payload_dir()));
    }

    #[test]
    fn corrupt_index_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tabs.json"), "{not json").unwrap();
        let store = TabsStore::new(dir.path());
        assert!(matches!(store.load(), Err(PersistError::Json(_))));
    }

    #[test]
    fn corrupt_payload_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        store.save(&[tab(1, "JSON 1", "{}")]).unwrap();
        fs::write(store.payload_path(1), "{not json").unwrap();
        assert!(matches!(store.load(), Err(PersistError::Json(_))));
    }

    #[test]
    fn missing_payload_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        store.save(&[tab(1, "JSON 1", "{}")]).unwrap();
        fs::remove_file(store.payload_path(1)).unwrap();
        assert!(matches!(store.load(), Err(PersistError::Io(_))));
    }

    #[test]
    fn unsupported_version_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        fs::write(store.path(), r#"{"version":99,"tabs":[]}"#).unwrap();
        assert!(matches!(store.load(), Err(PersistError::Json(_))));
    }

    #[test]
    fn save_creates_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path().join("nested/deeper"));
        // Missing directory loads empty, then save creates the whole tree.
        assert!(store.load().unwrap().is_empty());
        store.save(&[tab(1, "JSON 1", "{}")]).unwrap();
        assert!(store.path().exists());
        assert!(store.payload_path(1).exists());
        assert_eq!(store.load().unwrap(), vec![tab(1, "JSON 1", "{}")]);
    }

    #[test]
    fn only_changed_tabs_are_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        let a = tab(1, "A", "{\"a\":1}");
        let b = tab(2, "B", "{\"b\":2}");
        store.save(&[a.clone(), b.clone()]).unwrap();

        let a_id = identity(&store.payload_path(1));
        let b_id = identity(&store.payload_path(2));
        let index_id = identity(store.path());

        // Identical save: nothing is rewritten at all.
        store.save(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(identity(&store.payload_path(1)), a_id);
        assert_eq!(identity(&store.payload_path(2)), b_id);
        assert_eq!(identity(store.path()), index_id);

        // Change only A: A's payload and the index rewrite, B is untouched.
        let mut a2 = a.clone();
        a2.input = "{\"a\":999}".into();
        store.save(&[a2.clone(), b.clone()]).unwrap();
        assert_ne!(identity(&store.payload_path(1)), a_id);
        assert_eq!(identity(&store.payload_path(2)), b_id);
        assert_ne!(identity(store.path()), index_id);
        assert_eq!(store.load().unwrap(), vec![a2, b]);
    }

    #[test]
    fn removed_tab_payload_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        let a = tab(1, "A", "{}");
        let b = tab(2, "B", "[]");
        store.save(&[a.clone(), b]).unwrap();
        assert!(store.payload_path(2).exists());

        store.save(std::slice::from_ref(&a)).unwrap();
        assert!(!store.payload_path(2).exists());
        assert_eq!(store.load().unwrap(), vec![a]);
    }

    #[test]
    fn failed_save_leaves_no_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        let a = tab(1, "A", "one");
        let b = tab(2, "B", "two");
        store.save(&[a.clone(), b.clone()]).unwrap();

        let index_bytes = fs::read(store.path()).unwrap();
        let b_bytes = fs::read(store.payload_path(2)).unwrap();

        // Sabotage A's payload path with a non-empty directory so the atomic
        // rename fails mid-save (POSIX rename of a file onto a dir is EISDIR).
        let a_path = store.payload_path(1);
        fs::remove_file(&a_path).unwrap();
        fs::create_dir(&a_path).unwrap();
        fs::write(a_path.join("blocker"), b"x").unwrap();

        let mut a2 = a.clone();
        a2.input = "changed".into();
        assert!(store.save(&[a2, b.clone()]).is_err());

        // The failure happened before the index rewrite: index and the
        // untouched payload are byte-identical, and no temp file survives.
        assert_eq!(fs::read(store.path()).unwrap(), index_bytes);
        assert_eq!(fs::read(store.payload_path(2)).unwrap(), b_bytes);
        assert!(!has_tmp(dir.path()));
        assert!(!has_tmp(&store.payload_dir()));

        // Removing the obstruction lets a normal save fully recover.
        fs::remove_dir_all(&a_path).unwrap();
        store.save(&[a, b]).unwrap();
        assert_eq!(store.load().unwrap(), vec![tab(1, "A", "one"), tab(2, "B", "two")]);
    }

    #[test]
    fn save_is_safe_on_a_background_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        let tabs = vec![tab(1, "A", "{}"), tab(2, "B", "[]")];
        std::thread::scope(|scope| {
            scope.spawn(|| store.save(&tabs).unwrap());
        });
        assert_eq!(store.load().unwrap(), tabs);
    }

    #[test]
    fn from_file_derives_payload_dir_from_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.json");
        let store = TabsStore::from_file(&path);
        assert_eq!(store.path(), path);
        store.save(&[tab(7, "Seven", "{}")]).unwrap();
        assert!(dir.path().join("custom.d").join("7.json").exists());
        assert_eq!(store.load().unwrap(), vec![tab(7, "Seven", "{}")]);
    }

    /// Release-mode timing for a single ~100 MB tab. Run with:
    /// `cargo test --release -p json-viewer persist::tests::save_100mb_tab_release_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn save_100mb_tab_release_timing() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path());
        let chunk = "{\"key\":\"value\",\"n\":12345},";
        let repeat = (100 * 1024 * 1024) / chunk.len() + 1;
        let mut big = tab(1, "Big", "");
        big.input = chunk.repeat(repeat);
        let tabs = vec![big];
        assert!(tabs[0].input.len() >= 100 * 1024 * 1024);

        let start = Instant::now();
        store.save(&tabs).unwrap();
        let cold = start.elapsed();

        // Unchanged save: fingerprint only, no serialization or write.
        let start = Instant::now();
        store.save(&tabs).unwrap();
        let warm = start.elapsed();

        println!("100 MB tab save: cold={cold:?} warm(no-change)={warm:?}");
        assert!(cold.as_secs() < 10, "cold save too slow: {cold:?}");
        assert!(warm < cold, "unchanged save must be cheaper than a full write");

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].input.len(), tabs[0].input.len());
    }
}
