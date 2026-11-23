//! Off-thread prepared Input replacements for Format/Minify
//! (rp-format-inplace).
//!
//! The JSON worker builds the replacement rope from its output before
//! posting `Msg::JsonDone`, so the reducer swaps it into the rope editor in
//! O(1) instead of re-chunking the document on the UI thread. Entries are
//! keyed by the globally unique output revision and always taken by the
//! reducer — stale, failed and closed-tab results take and drop them too, so
//! nothing leaks. Synchronous callers (tests, bins) find no entry and build
//! the rope inline.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use ropey::Rope;

/// What the JSON worker prepared for one successful job.
pub enum Prepared {
    /// The output equals the job's input; Input needs no replacement.
    Unchanged,
    /// The replacement rope, built off the UI thread.
    Rope(Rope),
}

static PREPARED: LazyLock<Mutex<HashMap<u64, Prepared>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Stores a prepared replacement for `revision` (JSON worker thread).
pub fn stash(revision: u64, prepared: Prepared) {
    PREPARED
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(revision, prepared);
}

/// Takes the prepared replacement, if any (UI thread, once per `JsonDone`).
pub fn take(revision: u64) -> Option<Prepared> {
    PREPARED
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remove(&revision)
}
