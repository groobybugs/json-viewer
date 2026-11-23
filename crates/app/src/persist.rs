//! Tab persistence, replacing the old IndexedDB `tabs` store.
//!
//! Tabs are serialized as one JSON array, keyed by [`Tab::id`]. The default
//! location is the platform data directory (`~/.local/share/json-viewer/` on
//! Linux); [`TabsStore`] accepts an explicit base directory so tests never
//! touch the real profile.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::state::Tab;

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

/// The tab file used by the app.
///
/// Falls back to `.json-viewer/tabs.json` in the current directory when the
/// platform data directory cannot be determined.
pub fn data_path() -> PathBuf {
    directories::ProjectDirs::from("dev", "anomaly", "json-viewer")
        .map(|dirs| dirs.data_dir().join("tabs.json"))
        .unwrap_or_else(|| PathBuf::from(".json-viewer").join("tabs.json"))
}

/// A tab store rooted at a directory. `TabsStore::new(dir)` stores
/// `dir/tabs.json`; `TabsStore::from_file(path)` stores exactly `path`.
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

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// Loads tabs. A missing file is an empty list (first run), not an error.
    pub fn load(&self) -> Result<Vec<Tab>, PersistError> {
        let text = match fs::read_to_string(&self.file) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(PersistError::Io(error)),
        };
        Ok(serde_json::from_str(&text)?)
    }

    /// Writes tabs atomically: a sibling temporary file is renamed over the
    /// destination, so a crash cannot leave a truncated store behind.
    pub fn save(&self, tabs: &[Tab]) -> Result<(), PersistError> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(tabs)?;
        let temporary = self.file.with_extension("json.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, &self.file)?;
        Ok(())
    }
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

    fn tab(id: u64, title: &str, input: &str) -> Tab {
        let mut tab = Tab::new(id, title);
        tab.input = input.into();
        tab.output = input.into();
        tab
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
        first.view = crate::state::TabView::Output;
        first.search_query = "a".into();
        first.search_results.push(SearchHit {
            path: "a".into(),
            key: Some("a".into()),
            value_preview: "1".into(),
            line: Some(2),
        });
        let tabs = vec![first, tab(9, "JSON 2", "[]")];

        store.save(&tabs).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, tabs);
        // The temporary file is gone after the rename.
        assert!(!dir.path().join("tabs.json.tmp").exists());
    }

    #[test]
    fn corrupt_store_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tabs.json"), "{not json").unwrap();
        let store = TabsStore::new(dir.path());
        assert!(matches!(store.load(), Err(PersistError::Json(_))));
    }

    #[test]
    fn save_creates_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let store = TabsStore::new(dir.path().join("nested/deeper"));
        store.save(&[tab(1, "JSON 1", "{}")]).unwrap();
        assert!(store.path().exists());
    }
}
