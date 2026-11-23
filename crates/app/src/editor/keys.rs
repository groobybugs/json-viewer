//! Keyboard, clipboard and IME editing for the rope editor (rp-editor E3).
//!
//! [`handle`] is the editor container's `on_key`: it consumes editing keys
//! and queues semantic [`crate::state::Msg`] values (the reducer applies
//! them through `EditorState::apply`); anything it does not take falls
//! through to [`crate::ui::on_key`], the same shortcut dispatcher the native
//! field used, so app chords work with or without a window-level hook.
//!
//! Paste arrives through the root `on_paste` (Rune routes the clipboard
//! action there when no native text field has focus) as
//! `Msg::EditorPaste`. IME compositions arrive through `AppSpec::on_ime`
//! ([`on_ime`]): preedit draws at the caret, commit inserts one undo step.

use rune::{Ime, Key, KeyEvent, Tx};

use crate::state::{AppState, Msg, TabView};

use super::buffer::EditKind;
use super::state::{self, EditorAction, Move};

/// The editor container's key handler. Returns true when the key was taken.
pub fn handle(state: &mut AppState, event: &KeyEvent, tx: &mut Tx) -> bool {
    let id = state.active;
    let Some(tab) = state.tab(id) else {
        return false;
    };
    if tab.view != TabView::Input || state.tree.get(&id).is_some_and(|tree| tree.open) {
        return false;
    }
    let mods = event.mods;
    let word_mod = mods.ctrl || mods.cmd;
    let extend = mods.shift;
    let action = match &event.key {
        Key::Text(text) => {
            // Cmd/Ctrl chords belong to the app (AltGr on Windows/Linux is
            // Ctrl+Alt and still types; `is_command_chord` excludes it).
            if event.is_command_chord() {
                return delegate(state, event, tx);
            }
            EditorAction::Insert(text.clone(), EditKind::Typing)
        }
        Key::Enter => EditorAction::Enter,
        Key::Backspace => EditorAction::Backspace,
        Key::Delete => EditorAction::DeleteForward,
        Key::Tab => EditorAction::Tab,
        Key::Left => EditorAction::Move(
            if word_mod { Move::WordLeft } else { Move::Left },
            extend,
        ),
        Key::Right => EditorAction::Move(
            if word_mod { Move::WordRight } else { Move::Right },
            extend,
        ),
        Key::Up => EditorAction::Move(Move::Up, extend),
        Key::Down => EditorAction::Move(Move::Down, extend),
        Key::Home => EditorAction::Move(
            if word_mod { Move::DocHome } else { Move::Home },
            extend,
        ),
        Key::End => EditorAction::Move(
            if word_mod { Move::DocEnd } else { Move::End },
            extend,
        ),
        Key::PageUp => EditorAction::Move(Move::PageUp, extend),
        Key::PageDown => EditorAction::Move(Move::PageDown, extend),
        Key::Escape => {
            let has_selection = state::with(id, &tab.input, tab.input_revision, |ed| {
                ed.preedit.is_some() || ed.selection().is_some()
            });
            if has_selection {
                state::with(id, &tab.input, tab.input_revision, |ed| {
                    ed.preedit = None;
                    ed.anchor = None;
                });
                return true;
            }
            return delegate(state, event, tx);
        }
        Key::Command(c) => match c.to_ascii_lowercase() {
            'a' if !word_mod_other(mods) => EditorAction::SelectAll,
            'z' if !word_mod_other(mods) => {
                if mods.shift {
                    EditorAction::Redo
                } else {
                    EditorAction::Undo
                }
            }
            'y' if !word_mod_other(mods) => EditorAction::Redo,
            'c' if !word_mod_other(mods) => {
                state.dispatch(Msg::EditorCopy { tab: id, cut: false });
                return true;
            }
            'x' if !word_mod_other(mods) => {
                state.dispatch(Msg::EditorCopy { tab: id, cut: true });
                return true;
            }
            // Ctrl/Cmd+V falls through to Rune's paste action, which the
            // root `on_paste` handler delivers as `Msg::EditorPaste`.
            _ => return delegate(state, event, tx),
        },
        _ => return delegate(state, event, tx),
    };
    state.dispatch(Msg::EditorEdit { tab: id, action });
    true
}

/// Whether modifiers beyond the primary (and Shift) are held: such chords
/// are the app's, not the editor's edit commands.
fn word_mod_other(mods: rune::Mods) -> bool {
    mods.alt || mods.logo || (mods.ctrl && mods.cmd)
}

fn delegate(state: &mut AppState, event: &KeyEvent, tx: &mut Tx) -> bool {
    crate::ui::on_key(state, event, tx)
}

/// `AppSpec::on_ime`: compositions for the self-drawn editor. Plain typing
/// arrives as `Key::Text`; only what the input method composed comes here.
pub fn on_ime(state: &mut AppState, ime: &Ime, _tx: &mut Tx) -> bool {
    let id = state.active;
    let Some(tab) = state.tab(id) else {
        return false;
    };
    if tab.view != TabView::Input || state.tree.get(&id).is_some_and(|tree| tree.open) {
        return false;
    }
    match ime {
        Ime::Preedit(text, _, _) => {
            let preedit = (!text.is_empty()).then(|| text.clone());
            state::with(id, &tab.input, tab.input_revision, |ed| {
                ed.preedit = preedit;
            });
            true
        }
        Ime::Commit(text) => {
            state::with(id, &tab.input, tab.input_revision, |ed| {
                ed.preedit = None;
            });
            if !text.is_empty() {
                state.dispatch(Msg::EditorEdit {
                    tab: id,
                    action: EditorAction::Insert(text.clone(), EditKind::Ime),
                });
            }
            true
        }
        Ime::Select(range) => {
            state::with(id, &tab.input, tab.input_revision, |ed| {
                ed.anchor = Some(range.start);
                ed.caret = range.end.min(ed.buffer.len_bytes());
            });
            true
        }
        Ime::Enabled | Ime::Disabled => true,
    }
}