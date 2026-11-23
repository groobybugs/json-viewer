//! Zed-style large-file input editor (rp-editor).
//!
//! - [`buffer`]: rope-backed text buffer with grouped undo/redo and O(1)
//!   snapshots (E1).
//! - `view` / `keys`: visible-line rendering and editing over the rope (E2/E3,
//!   added in later steps).
//!
//! The editor replaces Rune's native `TextEdit` for tab input at every size;
//! `ui::text_view::TextView` stays the read-only Output component.

pub mod buffer;

pub use buffer::{Buffer, Edit, EditKind};