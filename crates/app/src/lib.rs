//! JSON Viewer: an Electron + React app ported to Rust + Rune.
//!
//! The crate is split into a small, pure model ([`state`]), background work
//! ([`json_ops`], [`search`]), platform services ([`io`], [`persist`]) and the
//! Rune view layer ([`ui`]). See `AGENTS.md` at the repository root for the
//! file-ownership map and the frozen-API rule.
//!
//! The binary lives in `main.rs`; everything reusable is here so integration
//! tests and future workers can use the same API.

#![allow(dead_code)]

pub mod fold_index;
pub mod headless;
pub mod io;
pub mod json_ops;
pub mod line_index;
pub mod persist;
pub mod runner;
pub mod search;
pub mod state;
pub mod ui;

pub use state::{AppState, Effect, Msg, Tab, TabId, TabView};
