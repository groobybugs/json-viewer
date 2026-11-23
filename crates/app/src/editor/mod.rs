//! Zed-style large-file input editor (rp-editor).
//!
//! - [`buffer`]: rope-backed text buffer with grouped undo/redo and O(1)
//!   snapshots (E1).
//! - [`state`]: per-tab caret/selection/window state in a thread-local store,
//!   synced from `Tab::input` on external changes (E2).
//! - [`view`]: visible-line rendering over the rope with mouse selection and
//!   horizontal column windows (E2).
//! - `keys`: keyboard/clipboard/IME editing (E3, added in a later step).
//!
//! The editor replaces Rune's native `TextEdit` for tab input at every size;
//! `ui::text_view::TextView` stays the read-only Output component.

pub mod buffer;
pub mod keys;
pub mod lex;
pub mod replace;
pub mod state;
pub mod view;

pub use buffer::{Buffer, Edit, EditKind, EditText};
pub use state::EditorState;