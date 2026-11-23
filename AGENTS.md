# AGENTS.md — JSON Viewer (Rust + Rune)

JSON Viewer is a native desktop app built in Rust with
[Rune](https://github.com/anomalyco/rune), ported from an Electron + React app.
The original TypeScript/Electron sources were removed in the port cleanup;
their behavior lives in git history at `main@869ef92` (`src/`, `electron/`).
All current code is in `crates/app/`.

Rune is pinned to dev revision
`194126ab55d8fee0e8750535a08ca9650f5b7b9e`. Every Rune dependency must come
from that exact git revision (Cargo unifies them by source).

```
Cargo.toml                     workspace (edition 2021, stable)
rust-toolchain.toml            Rust 1.99.1 (stable; Rune's pinned toolchain)
crates/app/
  Cargo.toml                   json-viewer package, dependencies frozen
  src/
    lib.rs                     modules + re-exports
    main.rs                    window/App hooks, --headless mode, boot/persistence
    state.rs                   pure model, Msg, Effect, reducers, reducer tests
    json_ops.rs                format/minify + background job
    search.rs                  JSON search (worker semantics)
    persist.rs                 tab store (replaces IndexedDB)
    io.rs                      dialogs, file read/write, clipboard, drop
    runner.rs                  drains Msg queue, executes Effect values
    line_index.rs              revision-keyed line index + snapshot cache
    tree_index.rs              streaming JSON node index + tree row jobs
    fold_index.rs              fold index + fold set + visual-row map
    editor/                    Zed-style large-file input editor (rp-editor)
      buffer.rs                ropey buffer + grouped undo/redo + O(1) snapshots
      state.rs                 per-tab caret/selection store + semantic actions
      lex.rs                   exact window lexer state: 64 KB checkpoints
      view.rs                  visible-line rope rendering + mouse + cache
      keys.rs                  keyboard, clipboard routing, IME
    headless.rs                offscreen PNG + element-tree dump
    bin/gen_fixtures.rs        QA fixture generator
    bin/screenshot.rs          renders docs/screenshot.png
    ui/
      mod.rs                   fn view + composition
      shell.rs                 page frame, header, status line
      tabs.rs                  tab strip + active document
      toolbar.rs               view switch, Validate/actions, Output fold controls
      search_bar.rs            search field + results panel
      text_view.rs             TextView: editable or virtualized read-only
      tree_view.rs             tree explorer toolbar, rows and footer
      validation_panel.rs      validation result and source-error panel
  tests/
    smoke.rs                   PNG smoke test + title assertion
    headless.rs                scripted input QA suite
    perf.rs                    ignored 100 MB perf harness
    edit_threshold.rs          ignored native-editor threshold measurements
                               + ignored rope-editor performance proof (E5)
    editor.rs                  rope editor evidence (render/mouse/keys/IME)
    text_view.rs               TextView + line index rendering tests
    validate.rs                validation parser/worker/panel/marker tests
    tree_view.rs               tree explorer evidence + release timings
    folding.rs                 folding + error-marker evidence
    ui_shell.rs                shell layout, shortcuts, accessibility
  benches/json_ops.rs          format/minify throughput
```

## Data flow (the one rule)

Rune views are pure descriptions. View handlers **only** queue messages:

```text
view(&AppState, &mut Cx<AppState>) -> El
    handler calls state.dispatch(Msg::…)
runner::poll (AppSpec::poll)
    drain AppState::pending + background inbox
    AppState::update(msg) -> Vec<Effect>
    runner::execute(effects)
        SpawnJson/SpawnValidation/Search -> worker threads -> Msg back into the inbox
        Persist/Copy/Export/Open/ReadDroppedFile -> platform I/O
```

`AppState::update` is synchronous, does no I/O, and is unit-tested. Keep it
that way. Effects may block today (dialogs/clipboard); that is a known
scaffold limitation, not a pattern to copy.

## Frozen public API

The public types and function signatures in these files are frozen for the
duration of the port. Parallel workers implement bodies and add private
helpers; if a signature truly cannot work, raise it and update **all** call
sites and this document in one commit.

- `state.rs`: `TabId`, `Tab`, `TabView`, `AppState`, `Msg`, `Effect`, reducer
  methods, `AppState::update`. `Tab.input_revision`/`output_revision` and
  `Msg::JsonDone.revision` were added by the integration round so the line
  index can be keyed by a document generation instead of sampling content.
- `json_ops.rs`: `JsonAction`, `JsonError`, `process`, `JsonJob`,
  `spawn_process`, `JobHandle`. Additive since rp-validate: `ValidationInfo`,
  `validate`.
- `search.rs`: `SearchHit`, `SearchOptions`, `search`.
- `persist.rs`: `PersistError`, `data_path`, `TabsStore`, `load_tabs`,
  `save_tabs`.
- `io.rs`: `IoError`, `read_json_file`, `pick_open_file`,
  `pick_export_path`, `write_file`, `copy_to_clipboard`, `on_file_dropped`.
- `ui/mod.rs`: `view`, `on_key`, `TITLE`.
- `ui/shell.rs`: `shell`, `header`, `status_bar`.
- `ui/tabs.rs`: `tab_bar`, `tab_content`.
- `ui/toolbar.rs`: `toolbar`.
- `ui/search_bar.rs`: `search_bar`, `search_results`.
- `ui/text_view.rs`: `TextViewMode`, `TextView::{editable, read_only,
  first_line, visible_lines, line_numbers, highlight, line_count,
  visible_range, line, build, revision, row_key, build_revealing}`. Additive
  since rp-fold: `error_marker`, `editing_disabled`, `editable_limit`,
  `char_width`, `line_height`, `LONG_LINE_LIMIT_BYTES`, `FoldAction`,
  `apply_fold_action`.
- `fold_index.rs`: `Region`, `FoldIndex`, `FoldSet`, `FoldView`, `key`,
  `view_for`, `prewarm`, `toggle`, `toggle_near`, `unfold_near`, `fold_all`,
  `unfold_all`, `fold_level`, `note_reveal`, `note_hover`.
- `headless.rs`: `DEFAULT_SIZE`, `DEFAULT_SCALE`, `runtime`, `render_png`,
  `element_tree`, `write_tree`, `render_and_dump`.

The removed TypeScript app is the reference for semantics, especially
`tabsSlice.ts` (reducers), `jsonWorker.ts` (JSON and search), and
`JsonViewer.tsx` / `SearchBar.tsx` / `FileDropzone.tsx` (UI behavior). Read
them from git history: `git show main@869ef92:src/store/tabsSlice.ts`.

## Ownership map

One owner per file. Do not edit another owner's file; coordinate through the
frozen API or a PR comment.

| File | Owner / scope |
| --- | --- |
| `crates/app/src/state.rs` | model, reducers, messages, effects |
| `crates/app/src/json_ops.rs` | JSON parse/format/minify worker |
| `crates/app/src/search.rs` | search traversal and hit model |
| `crates/app/src/line_index.rs` | revision-keyed line index + snapshot cache |
| `crates/app/src/tree_index.rs` | tree explorer: node index, row jobs |
| `crates/app/src/fold_index.rs` | fold index, fold set, visual-row map |
| `crates/app/src/editor/*` | rope input editor: buffer, state store, view, keys (rp-editor) |
| `crates/app/tests/editor.rs` | rope editor evidence (render/mouse/keys/IME) |
| `crates/app/src/persist.rs` | tab persistence |
| `crates/app/src/io.rs` | dialogs, files, clipboard, drop |
| `crates/app/src/runner.rs` | message/effect loop |
| `crates/app/src/ui/mod.rs` | view composition |
| `crates/app/src/ui/shell.rs` | page frame/header/status |
| `crates/app/src/ui/tabs.rs` | tab strip/document area |
| `crates/app/src/ui/toolbar.rs` | action toolbar |
| `crates/app/src/ui/search_bar.rs` | search field/results |
| `crates/app/src/ui/text_view.rs` | text input/output virtualization |
| `crates/app/src/ui/tree_view.rs` | tree explorer: toolbar, rows, footer |
| `crates/app/src/ui/validation_panel.rs` | validation alerts, source context, error reveal |
| `crates/app/tests/tree_view.rs` | tree explorer evidence + timings |
| `crates/app/src/headless.rs` | headless render/tree helpers |
| `crates/app/src/main.rs` | binary, window wiring, `--headless` |
| `crates/app/tests/smoke.rs` | headless smoke test |
| `crates/app/tests/folding.rs` | folding + error-marker evidence |
| `crates/app/Cargo.toml`, `Cargo.toml` | dependencies/workspace (ask first) |

## Validation (rp-validate)

**Validate** (`Ctrl+Shift+V`) runs the same streaming parser as Format/Minify
without building an output string, and reports byte/line/depth/value counts
for valid input. Errors carry a 1-based line and UTF-8 byte column, a bounded
source excerpt and a **Go to error** action. The Input `TextView` receives
`.error_marker(line, column)` in both native-editable and read-only fallback
modes; Go to error sets the native caret, or reveals the virtual row (opening
any folded ancestors) in the fallback. Editing or a newer input revision
invalidates stale validation results. The tree view retains the validation
panel above its own toolbar.

## Tree explorer (rp-tree)

A classic JSON-editor tree view, per tab: the document-card header's
`Input | Output | Tree` switch (the "View" switch) enters it, and its own
toolbar offers **Expand all**, **Collapse all**, **Sort A→Z** (view order
only — data is never touched; case-insensitive *natural* order, digit runs
compared numerically so `alpha_2` sorts before `alpha_10`, ties stable by
document order) and a **Filter** box (keys and scalar values match
case-insensitively, `search.rs` rules; matched nodes keep their ancestors
and auto-expand; expansion is blocked while a filter is active). The filter
field is one shared `TextEdit` (`AppState::tree_filter_edit`) that always
displays the active tab's filter — the reducer re-syncs it on
`TreeOpen`, `SelectTab` and every filter change, including queries that
arrive without typing (scripts, restored runs). Rows show expand arrows,
indentation guides, keys (or `[index]` for array elements), type-coloured
values and `{n}`/`[n]` child counts; selecting a row shows its
` → `-joined path in a footer with **Copy path** / **Copy value** (the
whole subtree text for containers). Clicking a search hit while the tree
is open reveals and selects that node in place (`Msg::TreeRevealPath`
expands the path's containers; the hook is the one `select_hit` branch in
`ui/search_bar.rs`); with the tree closed, hit clicks jump to the code
output view as before.

New state (small frozen-API extension, documented here): `AppState` gains
`tree: HashMap<TabId, TreeUi>` and `tree_filter_edit: TextEdit`;
`TreeUi`/`TreeRow`/`TreeSelection`/`TreeJob` live in `tree_index.rs`.
`TabView` stays a two-variant enum **on purpose**: `persist.rs` has an
exhaustive `match tab.view` this branch does not own, so a tab "is in tree
mode" iff `state.tree.get(&id)` exists with `open` set (runtime-only, never
persisted). `Msg` gains `TreeOpen/TreeClose/TreeExpand/TreeCollapse/
TreeSelect/TreeRevealPath/TreeExpandAll/TreeCollapseAll/TreeSortToggled/
TreeFilterChanged/TreeCopyPath/TreeCopyValue/TreeIndexDone/TreeRowsReady`;
copy actions reuse the existing `Effect::Copy`, and choosing a code view
(`Msg::SwitchView`) leaves tree mode, so search-result jumps in code views
behave as before.

Other semantics: `TreeUi` retains its index while its tab is in a code view
and drops it on input change, clear or close; duplicate object keys remain
duplicate rows, matching `search.rs`; row depth saturates at `u16`
(`MAX_ROW_DEPTH`), with indentation guides drawn through 48 levels; values
show the raw JSON token elided at 160 bytes, and object keys are unescaped
(including surrogate-pair decoding).

Performance design (evidence: `cargo test --release -p json-viewer --test
tree_view -- --ignored --nocapture`, PNGs in `target/headless/tree-*.png`):

- **Index.** `tree_index::TreeIndex` is a struct-of-arrays, **24 bytes per
  node** (six `u32`: key offset, value start, value end, packed kind+child
  count, first child, next sibling), built by one streaming pass of a
  hand-rolled tokenizer — never a `serde_json::Value` DOM. Depth/parent are
  *not* stored; the flattened rows carry them. Measured: 6.9 MB single-line
  api.json-shaped doc → 7 ms / 323k nodes; 100 MB → 94 ms / 5.18M nodes
  (124 MB index).
- **Rows.** The flattened visible list lives in `TreeUi.rows` and is
  maintained incrementally (expand splices children, collapse drains the
  subtree); it renders through `El::virtual_rows` keyed by *node id* with
  `virtual_rows_version((index_revision, rows_gen))`, so commits are
  O(visible) at any size: fully expanded 100 MB (5.18M rows) scrolls at
  p95 ≈ 12 ms/frame.
- **Off-thread delivery without touching `runner.rs`** (which this branch
  does not own): `main.rs` registers the live `rune::AppHandle` with
  `tree_index::set_app_handle` before `run()`; index builds and big row jobs
  (expand-all, sort re-flatten, filter) run on `json-viewer-tree*` threads
  and deliver their `Msg` through `AppHandle::update`, which wakes the loop
  for `runner::poll` to drain — the same message flow as the JSON/search
  workers. Without a handle (tests, `--headless`) every job runs inline, so
  reducers stay deterministic.
- **Expand-all strategy:** at or below `tree_index::SYNC_ROW_LIMIT`
  (200k) nodes it re-flattens inline; above that it runs off-thread with a
  status spinner and a per-tab cancel flag (a newer job cancels the old).
  Measured: 100 MB expand-all 181 ms off the UI thread, no cap needed.
- **Filter** runs off-thread with the same cancel machinery; measured
  201 ms on 100 MB (259k matches). **Sort** is a per-object natural-order
  permutation (`tree_index::natural_cmp`: case-insensitive, digit runs
  numeric, ties stable by document order) computed lazily at enumeration
  time (cached doc-order children per expanded container), never mutating
  the index; big docs re-flatten
  off-thread when it toggles.


## Large-file input editor (rp-editor)

The Input view is a self-drawn, Zed-style editor over a `ropey` rope
(`crates/app/src/editor/`), editable at **every** size — the 5 MiB
`EDITABLE_LIMIT_BYTES` gate, the 128 KiB long-line gate and the read-only
banners no longer apply to tab input. `ui/text_view.rs` is untouched: it
remains the read-only Output component and its frozen `TextView::editable`
legacy component (with its gates) still exists for its own tests
(`tests/text_view.rs`, `tests/folding.rs`); the app simply never mounts it
for input anymore.

### Buffer and state

- `editor::buffer::Buffer` wraps `ropey::Rope`: insert/delete/replace by byte
  or char offset, line↔offset mapping, a revision counter, and undo/redo with
  grouped transactions (consecutive typing coalesces per word, a paste is one
  step, caret moves break the group). History is capped at 64 MiB of edit
  text. `snapshot()` is an O(1) `Rope` clone; `to_string()` (via `Display`) is
  O(n) and belongs on worker threads or explicit bulk actions only.
- `editor::state::EditorState` (caret, selection anchor, horizontal
  `first_column`, goal column, IME preedit, reveal request, viewport
  geometry) lives in a **thread-local store keyed by `TabId`** — the same
  pattern as `fold_index`, because the view only has `&AppState`. Every
  access goes through `state::with(id, &tab.input, tab.input_revision, …)`,
  which first re-syncs the rope from `Tab::input` when the text changed
  outside the editor (revision mismatch, or a length mismatch while clean —
  this covers direct assignments in tests/bins). `take_reveal`,
  `existing_snapshot` and `dirty_snapshot` read existing entries without
  creating one, so `runner::poll` never builds a rope for a tab no editor
  has shown.
- **`Tab::input` is a materialized cache, not the keystroke source of
  truth.** Editor edits set `EditorState::dirty` and bump
  `Tab::input_revision`; no per-keystroke `String` copy happens anywhere
  (`sync_active_editor` is no longer called by the app).
  `AppState::materialize_input(id)` refreshes the cache and is called by
  explicit consumers: paste, Copy/Export, tree index builds,
  `ValidationDone` (the error panel reads source context), and the
  empty↔non-empty boundary (so the toolbar's enabled state stays correct).
  The status line's line count reads the cache and can lag a typing burst
  by a few lines; it refreshes at the next bulk action.

### Rendering (E2)

`editor::view::build` renders only the visible lines as plain keyed rows
between two spacers — **not** `El::virtual_rows`, whose key list
rematerializes O(line count) on every count change (Enter/paste on a
multi-million-line document). The window comes from tracked layout reads:
`cx.window_layout_with("document-scroll-{id}", …)` and
`cx.window_layout_with("editor-{id}", …)` give viewport and content top;
their difference is the scroll offset, and the view re-runs when scrolling
moves it. Rows are exactly `text_view::ROW_HEIGHT` (16.0) tall, so every
spacer height is a multiple of 16 and f32 layout positions stay exact past
4M lines. Row keys are `editor-{id}-row-{line}` — the same shape as
`TextView::row_key`, so `validation_panel`'s reveal keeps working;
`runner::poll` parses those keys and scrolls by offset (far jumps work
without a mounted row) and places the caret on the validation error.

Very long lines are never materialized: each row slices only its visible
column window (`VISIBLE_COLUMNS`, narrowed to the viewport width) straight
from the rope and tokenizes that window. The window's starting lexer state
is **exact** (`editor/lex.rs`): per-line checkpoints every 64 KB
(`LexState { in_string, escaped }`), built lazily off the UI thread above
256 KB and invalidated through the buffer's bounded edit log
(`Buffer::edits_since`); checkpoints before the first edit touching the line
are salvaged, so an edited line lexes from the nearest valid checkpoint
(≤64 KB scan) while a worker rebuilds. Ink decisions that need bytes past the
window end (key `:` lookahead, literals cut by the edge, a string cut before
its closing quote) use a bounded tail/forward scan, so windowed colors equal
whole-line lexing — `tests/editor_lex.rs` asserts that at many columns
(inside strings, right after `\"` and `\\`, stride and window edges) and
after edits/undo. A thread-local content-hash cache (`(tab, line, window
bounds, hash, lex state, tail)`) skips re-slicing/re-tokenizing unchanged
windows; Rune's own shape cache keys on the display string, so unchanged
lines are not reshaped. Caret (inline
2 pt bar splitting the row text), selection (`Span::bg`), error marker
(row tint + underline + gutter dot + `editor-{id}-error-dot`/`-error-label`
strip, same keys as the old editable error view) and IME preedit are drawn
per visible row. Mouse: click/shift-click/drag (char), double-click drag
(word), triple-click drag (line) via `El::on_drag` + `Drag::clicks`;
horizontal wheel/shift-wheel pans `first_column` via `El::on_wheel`.
Caret moves auto-pan the column window and set `EditorState::reveal`,
which `runner::poll` turns into a minimal `Tx::scroll_to_y`.

### Editing (E3)

`editor::keys::handle` is the container's `on_key`: typing/Enter (auto-
indent copies the line's leading whitespace, +2 after `{`/`[`),
Backspace/Delete (CRLF pairs count as one), Tab (2 spaces), arrows /
Home/End / PageUp/PageDown / Ctrl+arrows (word) / Ctrl+Home/End, Shift
extends, Ctrl+A, Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y, Ctrl+C/X queue
`Msg::EditorCopy`. Unhandled keys delegate to `ui::on_key` (the same
shortcut dispatcher the native field bound), so app chords work with or
without a window-level hook. Ctrl/Cmd+V falls through to Rune's paste
action; the root `on_paste` in `ui/mod.rs` delivers it as
`Msg::EditorPaste` while the input view is showing (native fields take
their own pastes when focused). IME: `App::on_ime(editor::keys::on_ime)` —
preedit draws underlined at the caret (`El::ime_cursor` marks the surface),
commit inserts one undo step. Handlers queue semantic `Msg::EditorEdit`
values; the reducer applies them through `EditorState::apply` and does the
tab bookkeeping (revision bump, error/validation clear, tree refresh).
Presentation-only pointer/pan handlers mutate the store directly, like
fold toggles.

### Workers and persist (E4)

`Msg`/`Effect` grew additively (frozen signatures unchanged):
`Msg::EditorEdit/EditorPaste/EditorCopy/RopeMaterialized`,
`Effect::MaterializeRope { tab, input: Rope, job: RopeJob }` and
`RopeJob::{Json, Validate, Search}`. Format/Minify/Validate/Search take an
O(1) `existing_snapshot` when the editor owns the text; the runner
converts the rope to `Arc<str>` on a `json-viewer-rope` worker thread and
returns `Msg::RopeMaterialized`, which emits the classic
`SpawnJson/SpawnValidation/Search` effect (job handles stay registered on
the UI thread). Tabs with no editor entry (restored, never opened) keep
the classic `Arc::from(tab.input)` path. Persist's chunked copy snapshots
dirty ropes at `begin_copy` (`dirty_snapshot`, O(1)) and slices 4 MB
chunks from the immutable snapshot — a consistent document even while
typing continues; the chunk machinery stays because `Tab::output` is
still a `String` (measured: the 100 MB snapshot-step test stays <30 ms).

### Evidence

- `cargo test -p json-viewer --test editor` — render/mouse/keys/IME/cut/
  paste evidence, PNGs in `target/headless/editor-*.png`.
- `cargo test -p json-viewer --test editor_lex` — windowed colors equal
  whole-line lexing at many columns/widths (escaped quotes/backslashes,
  stride and window edges), checkpoint invalidation on edits, reuse on
  other-line edits, and off-thread publication.
- `cargo test --release -p json-viewer --test editor -- --ignored --nocapture`
  — 100 MB windowing: load 45 ms, first frame 90 ms, scroll-to-middle
  2.8 ms, caret-move p95 0.6 ms, 59 rows mounted (3.85M lines).
- `cargo test --release -p json-viewer --test edit_threshold rope_editor_bench
  -- --ignored --nocapture` — the E5 proof through the real `ui::view`
  (keystroke = key + dispatch frame + poll + render frame, 20 chars per
  position; release, scale 1, 1280×800):

  | Variant | MiB | Lines | Load ms | First frame ms | Key p95 ms | Paste 1KB ms | Paste 10MB ms | Undo 10MB ms | SelAll+Del ms | VmHWM MiB |
  | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
  | pretty 1 | 1.00 | 38917 | 0.1 | 12.6 | 1.04 | 1.2 | 27.1 | 5.0 | 1.9 | 192 |
  | pretty 10 | 10.00 | 387153 | 4.2 | 17.8 | 1.09 | 6.9 | 38.4 | 5.4 | 5.2 | 254 |
  | pretty 100 | 100.00 | 3851257 | 41.5 | 73.0 | 1.07 | 45.6 | 64.5 | 5.2 | 49.0 | 620 |
  | single-line 6.9 | 6.58 | 1 | 0.5 | 19.9 | 3.66 | 2.1 | 19.7 | 5.3 | 3.7 | 620 |

  Keystroke cost is size-independent (~1 ms p95 on pretty documents;
  start/middle/end within 0.4 ms of each other); the single-line row pays
  ≤3.7 ms p95 for exact checkpoint maintenance while editing a 6.9 MB line.
  Every row passes p95 <16 ms and first frame
  <250 ms. VmHWM is process-wide/cumulative across the run. PNGs:
  `target/headless/rope-100mb-middle-selection.png` (mid-file caret + word
  selection at line 1,927,667), `rope-single-line-col-3m.png` (editing at
  column 3,000,000 with elided column window), `rope-error-marker.png`
  (row tint, gutter dot, underline, error strip). The historical
  native-`TextEdit` threshold table still runs as `edit_threshold_bench`.
- The status bar's line count is `editor::state::display_line_count`
  (coordinator-approved one-line `ui/shell.rs` change): O(1) rope
  `len_lines` with `str::lines()` semantics when the editor owns the tab,
  else a scan memoized per (revision, length). The old
  `tab.input.lines().count()` cost 24.6 ms per view rebuild at 100 MB and
  was the only thing keeping 100 MB typing above the 16 ms budget
  (52.9 ms p95 before, 1.04 ms after). Tests:
  `line_count_matches_str_lines_semantics` (trailing `\n`/`\r\n`/`\r`,
  empty, memo invalidation, both paths) and
  `status_bar_line_count_stays_correct_after_edits`.
- `tests/validate.rs::go_to_error_reveals_the_error_line_in_the_rope_editor`
  replaces the old read-only-fallback banner test (coordinator-approved):
  the 6 MB single-line input is editable, Go to error reveals and marks
  the error line and puts the caret on it.

## Build, test, run

All commands from the repository root. Stable toolchain, edition 2021.
`rust-toolchain.toml` pins **Rust 1.99.1**, the stable release Rune's own
`rust-toolchain.toml` uses; rustup installs/selects it automatically. The
pinned Rune workspace itself uses edition 2024 / resolver 3, which Cargo
handles for a git dependency.

```sh
cargo build
cargo clippy --all-targets
cargo test
cargo test -p json-viewer --test smoke
```

Windowed:

```sh
cargo run -p json-viewer
```

Rune's built-in headless tooling (writes a PNG and an element tree; `--tree`
needs a path):

```sh
cargo run -p json-viewer -- --snap target/headless/app.png
cargo run -p json-viewer -- --snap target/headless/app.png --tree target/headless/tree.json
```

This crate's explicit headless mode (placeholder PNG + tree):

```sh
cargo run -p json-viewer -- --headless target/headless
# -> target/headless/placeholder.png
# -> target/headless/placeholder-tree.json
```

The smoke test always writes `target/headless/smoke.png` and
`target/headless/smoke-tree.json`, and asserts the tree contains
`JSON Viewer`.

Headless rendering needs a working wgpu backend. On Linux that means
Vulkan (lavapipe/`lvp_icd.json` is available here) or another wgpu backend;
there is no display server requirement for the offscreen path.

### Rune patches (required, not optional)

A dependency's `[patch]` does not apply to the workspace that depends on it,
so this workspace repeats the patches Rune needs in its root `Cargo.toml` as
git patches pointing at the pinned Rune revision. Cargo resolves a git patch
by package name anywhere in the repo, and Rune's `vendor/` tree holds the
already-patched sources, so no code is vendored here. Every entry Rune
declares is repeated (`winit`, `sctk-adwaita`, `parley_engine`, `wgpu-hal`,
`accesskit_windows`, `accesskit_macos`):

- `parley_engine` — crates.io 0.12.0 has no `line-dictionaries` feature,
  which `rune-text` enables, and lacks Rune's bidi/shaping fixes.
- `winit` / `sctk-adwaita` — Rune calls
  `WindowExtWayland::{content_corner_radius, has_client_side_frame}`, added by
  Rune's winit patch, and sctk-adwaita supplies the radius behind it.
- `wgpu-hal` — DX12/GL fixes (adapter exposure, partial presents, DLL
  loading, drain on removed devices). Rune vendors wgpu-hal 30.0.1, so
  `Cargo.lock` pins the whole wgpu family at 30.0.1 — the pairing Rune's own
  lockfile tests. A newer patch release (30.0.2) stays unused and warns,
  because `wgpu 30.0.x` requires a matching `wgpu-hal`; keep the lock at
  30.0.1 unless Rune bumps.
- `accesskit_windows` / `accesskit_macos` — headings/landmarks reach UIA /
  VoiceOver and long text keeps its ranges. Linux builds never compile these
  two; the entries are inert here and required for macOS/Windows builds.

When bumping the Rune revision, change every `rev = "…"` in
`crates/app/Cargo.toml` and the root `[patch.crates-io]` to the new revision
in one commit (Rune's `vendor/*.patch` files show what the patches change),
re-check the feature list above, and re-check the vendored versions against
the new `Cargo.lock`: a patch whose version no longer satisfies the graph's
requirements is silently unused with a `was not used in the crate graph`
warning. No `vendor/` directory exists here anymore; the former local
`[lints.rust]` allowances on `parley_engine` and `sctk-adwaita` proved
unnecessary with the git sources (the build shows no warnings without them).

## Rune notes (learned at the pinned revision)

### App shape

The smallest app is one state value, one view fn:

```rust
fn view(state: &State, cx: &mut Cx<State>) -> El;

rune::run(AppSpec::new("JSON Viewer", (1280.0, 800.0), boot, view));
```

`Cx` gives runtime context (theme/system dark, titlebar inset, focus). View
handlers get `&mut S` and are queued until between frames:

```rust
Button::new("Format")
    .on_press(|state: &mut AppState| state.dispatch(Msg::Format))
    .build(&kit)
```

There is no React-style rerender loop to manage: after a handler mutates
state, Rune re-runs the view and diffs keyed elements. Give dynamic elements
stable `.key(..)` values; `Runtime::find` / `presented` and tests address
elements by key.

### Wave-1 integration (wave-1 branches)

- **Fonts (zero bundle).** `headless::APP_FONTS` is empty, so Rune's embedded
  Inter stays in use; `crates/app/assets/` is gone. `APP_FAMILIES` is
  `("Inter", "IBM Plex Mono, ui-monospace")`: `main.rs` sets `spec.families`
  and `headless::text_system()` uses the same tuple. `TextStyle::mono()` is
  `Family::Mono` (the mono *slot*), which the default `("Inter","Inter")`
  leaves proportional. A non-empty `fonts` list is all-or-nothing and replaces
  the embedded Inter faces, so do not bundle a single mono TTF through it.
  IBM Plex Mono comes from fontconfig when installed; `ui-monospace` is the
  fixed-pitch fallback. Tests assert equal `i`/`W` advances, never exact font
  metrics.
- **Lifecycle.** `main.rs` runs
  `rune::App::new(spec).on_close(runner::on_close).on_quit(runner::on_quit).run()`.
  Both hooks call `runner::flush_persist`, which synchronously drains the
  debounced persist worker before the window/app goes away.
- **Line index.** The `Effect::SpawnJson` callback in `runner.rs` reserves
  `line_index::next_revision()`, calls
  `line_index::prewarm_off_thread(Arc::from(output), revision)` on the JSON
  worker thread, and sends `Msg::JsonDone { revision, .. }`. The reducer stores
  that revision on `Tab.output_revision`; `TextView::revision(..)` looks the
  snapshot up with `snapshot_for_revision`. Revisions are globally unique, so
  a different document can never reuse a stale index even when length and
  head/tail bytes match. A cold revision miss for a document >= 8 MiB is
  provisional (first 64 KiB) until the prewarm lands. Input edits bump
  `Tab.input_revision`, which keys the read-only fallback for very large
  inputs.
- **Row keys.** Read-only `TextView` rows are keyed
  `TextView::row_key(base, line)` -> `{base}-row-{line}`; use
  `TextView::build_revealing(base, line)` or `Tx::scroll_into_view` with that
  key to jump to a line.
- **Format result.** A successful `Msg::JsonDone` stores `output` and switches
  that tab to `TabView::Output`, matching the original
  `processJsonContent.fulfilled` reducer. Failures leave the current view and
  previous output untouched.
- **Search jump.** `search_bar::select_hit` dispatches `SwitchView(Output)` and
  queues `runner::queue_reveal(TextView::row_key("output-text", line - 1))`.
  `hit.line` is 1-based; rows are 0-based. The reveal waits for the next idle
  `runner::poll`, after the Output view has mounted, then calls
  `tx.instant(); tx.scroll_into_view(key)`.
- **Viewport axes.** Use `.scroll_xy()` for the document viewport.
  `.scroll(..).scroll_x()` replaces the axes with X-only, so vertical wheel,
  reveal and jump-to-line silently stop working.
- **Shortcuts.** `main.rs` sets `spec.on_key = Some(ui::on_key)` so shortcuts
  work before any control has focus. The root view and both text inputs also
  bind `ui::handle_shortcut`/`on_input_key`; focused text inputs route keys
  through `on_input_key`, not ancestor handlers.
- **Drop path.** `ui::view` wraps the shell root in `io::drop_zone(...)`: the
  single drop handler, with the `.json` filter, hover overlay, reject status,
  and `Msg::FileDropped`. Do not add another `.on_drop_files` handler.

### Theme and components

- `rune::tok::theme(true)` is the dark palette (`bg`, `card`, `text`,
  `text_secondary`, `separator`, …). `tok::theme(false)` is light. The port
  forces dark, matching the original app.
- `rune::tok::type_::TITLE`, `BODY`, `LABEL`, `MONO_CAPTION` are text styles;
  `tok::TextStyle::new(12.0, 400.0).mono()` for code.
- `rune_kit::Kit::neutral(true)` supplies component tokens.
  `rune_kit::Button::{new, variant, disabled, on_press, build}` covers the
  toolbar/tab buttons. `rune-kit` is built with `default-features = false` to
  leave out Markdown and the 80-language syntax highlighter.
- Layout methods used here: `col/row/el/text/spacer/icon`, `.w_full`,
  `.h_full`, `.grow`, `.min_h`, `.max_h`, `.gap`, `.padding`, `.fill`,
  `.radius`, `.border`, `.child`, `.children`, `.key`, `.scroll`.

### Headless

Rune's test primitives (used by `headless.rs`):

```rust
let mut rt = rune::Runtime::new(state, view, (w_points, h_points), Env::new(true, scale));
rt.set_srgb_blend(true);
rt.frame(0.0);
rt.settle_all();       // land growth/reveal/scroll animations
rt.frame(0.0);

let gpu = rune::render::Gpu::headless();
let format = rune::render::OFFSCREEN_FORMAT.remove_srgb_suffix();
let mut renderer = rune::render::Renderer::new(&gpu.device, format);
renderer.set_srgb_blend(true);
let target = rune::render::Offscreen::with_format(&gpu.device, px_w, px_h, format);
renderer.prepare(&gpu.device, &gpu.queue, &mut rt.text.atlas, &rt.instances, (px_w, px_h));
let mut encoder = gpu.device.create_command_encoder(&Default::default());
renderer.draw(&mut encoder, &target.view, rt.clear_color());
target.copy(&mut encoder);
renderer.submit(&gpu.queue, encoder);
let rgba = target.read(&gpu.device);
rune::render::write_png(path, px_w, px_h, &rgba)?;
```

- Viewport is in points; the offscreen target is `points * scale` pixels
  (`Env.scale`). `--snap` defaults to scale 2; our test uses 1 for speed.
- `Runtime::accessibility_json()` gives the accessibility tree used by
  `--tree`. `Runtime::elements()` returns `ElementInfo` with `path`, `key`,
  `role`, `label`, `text`, `rect`, `visible`, focus flags. `headless.rs`
  combines both.
- `Runtime::frame(dt)` returns whether state changed; `settle_all()` lands
  tracks before the snapshot.
- Built-in flags (`--snap PATH`, `--tree PATH`, `--script`, `--trace`) are
  available in debug builds and in release with the `tooling` feature. Our
  `--headless` is intercepted before `rune::run`, so Rune never parses it.
- Verified locally: `--snap target/headless/rune-snap.png --tree
  target/headless/rune-tree.json` writes a 2× PNG and a JSON object with
  `elements` (path, role, rect, fill, text/label, focus state) plus
  `accessibility`; `--tree` requires a path.

### Text input

- `rune::TextEdit` lives in app state (`AppState::editors`,
  `AppState::search_edit`). `rune::text_input(&edit, style)` draws it.
- `.on_edit(|s: &mut S| &mut s.field)` binds the element to the state field;
  without it the editor has nothing to write into.
- `.on_input_change(|s, tx| …)` runs after a focused field's text changes.
  Sync `edit.text` into the model there (`AppState::sync_active_editor`).
  Edits are immediate; the caret/selection stay in the editor, so do not
  replace the `TextEdit` on every keystroke.
- Text fields are multi-line by default. `.single_line()` switches; Return
  submits (`.on_submit`). `.placeholder(text, color)`, `.caret(caret,
  selection)`, `.on_input_key`, `.on_input_paste` are available.
- For file-set text, use `TextEdit::replace_text` (as `update_input`/`clear_tab`
  do) so the editor's invariants are repaired.
- A focusable text field needs `.label("…")` (or `labelled_by`); debug builds
  print `a focusable text field has no name a screen reader can say` and the
  accessibility tree has no name otherwise.

### Scrolling and virtualization

- A viewport needs a bounded height: `.h(...)` or `.grow(1.0)` inside a
  bounded column. Every flex ancestor between the bounded parent and the
  scroll view needs `.min_h(0.0)` or it grows to content. Our document area
  uses `col().grow(1.0).min_h(0.0).scroll(ScrollSpec { stick_bottom: false })`.
- `El::virtual_rows(count, estimated_row_height, key, build)` builds only the
  rows in the mounted window. It must sit inside a `.scroll(..)` viewport and
  itself use `.growth(GrowthSpec::default())`; the row builder receives
  `(&S, i)` and must find rows by key, not index. `on_visible_rows` reports
  the visible index range.
- `TextView` renders read-only JSON through `line_index::Snapshot` and
  `virtual_rows`, so a large document builds only the mounted window. It
  keeps the windowed `first_line`/`visible_lines` contract for tests and
  non-virtualized callers. Use `TextView::row_key` for row keys.
- `El::scroll_x()` / `scroll_xy()` for wide JSON; `Tx::scroll_into_view(key)`
  to reveal a search hit. Wheel/trackpad momentum is built in.
- Above `PLAIN_ROWS_LIMIT` (512) visible rows `TextView` uses that virtual
  list; at or below it, rows are plain keyed children. A fold action that
  changes the visible count makes Rune re-materialise the whole row-key list
  (`Vec<String>`) — the one O(visible rows) step, not ours. Folding a
  2M-line document down to a few rows therefore lands on the plain path and
  commits in ~0 ms; unfolding it back to 2M rows costs what the first mount
  of that document costs (~270 ms measured).

### Code folding

- `fold_index.rs` builds one struct-of-arrays entry per multi-line `{`/`[`:
  start line, matching close line, direct child count, depth, array flag.
  One O(bytes) pass, string/escape aware. It runs off the UI thread next to
  the line index (`line_index::prewarm_off_thread` calls
  `fold_index::prewarm`), or lazily on the UI thread below
  `LARGE_FOLD_BYTES` (= `line_index::LARGE_DOCUMENT_BYTES`, 8 MiB). A larger
  cold document gets a bounded provisional index with no regions until the
  prewarm lands; only the output path is prewarmed, so a ≥8 MiB *input* is
  not foldable.
- Several containers can open on one line (`{"a": [`); the index keeps the
  outermost per start line, since the gutter arrow and the fold set are
  line-keyed.
- `FoldSet` is a bitset over start lines (O(1) toggle/membership).
  `FoldView` turns it into disjoint visible runs (`Segment` prefix sums):
  `visible_count()` is the `virtual_rows` count, `visual_to_doc` is
  O(log runs), `doc_to_visual` returns `None` for hidden lines. Nested fold
  marks stay in the bitset and only the outermost hidden range counts, so
  unfolding a parent restores its children's marks.
- Fold state is process-wide in `fold_index` (interior mutability), keyed by
  `fold_index::key(tab.output_revision, &tab.output)`: the view only has
  `&AppState`, but a reveal must unfold before `Tx::scroll_into_view`. Each
  tab's output revision is its own fold set. Handlers bump
  `AppState::fold_epoch` so Rune re-runs the view; `virtual_rows_version`
  includes the fold epoch.
- Rows are keyed by **document line** (`row_key(base, line)`), never by
  visual row, so gutter numbers are document numbers and a reveal key stays
  stable across folds. `note_reveal` is keyed by the view base: the next
  `build` clears every folded ancestor of the target line. Search hits,
  `build_revealing` and `error_marker` callers all go through that path.
- UI: `▾`/`▸` arrows on foldable rows, a folded row renders
  `"key": {…}` / `[…]` plus a status-line-`MUTED` `· N keys|items` badge;
  clicking the arrow, the badge or the placeholder line toggles.
  `ui/toolbar.rs` keeps the view switch left and the seven actions right;
  Fold all / Unfold all / L1–L3 (`ui::text_view::apply_fold_action`) live in
  an Output-only sub-bar, so 1024 px layouts do not clip. Tree controls have
  their own Tree-only sub-bar.
  Shortcuts: `Ctrl+Shift+[` / `Ctrl+Shift+]` toggle/unfold at the hovered
  line (the read-only view has no caret), and `Ctrl+K` then
  `Ctrl+0` / `Ctrl+J` / `Ctrl+1..3` fold all / unfold all / fold to level.
  Rune hands `on_key` one `KeyEvent`, so the two-key chord lives in
  `AppState::fold_chord` (a modifier-less key after `Ctrl+K` cancels it).
- Measured (release, 2M-line / 102 MB document, 250_002 regions; fold index
  alone: 7 MB 5.5 ms, 100 MB flat 85 ms, this fixture 103 ms including the
  line index): fold all 0.4 ms + 1.2 ms commit, 60 frames after fold-all
  avg 0.110 ms / worst 0.407 ms, search jump into a folded region 106 ms
  commit + 3.2 ms scroll, unfold all 121 ms. The first mount of the same
  2M-row document is 302 ms, i.e. the virtual-row key list, not folding, is
  the large one-off cost.

### Error marker (Validate view half)

`TextView::error_marker(line, column)` (both 1-based) is additive and works
in both modes:

- Read-only/virtualized: red row tint, a small filled circular element in a
  dedicated gutter cell (reserved on every row so text columns stay aligned),
  and an underlined caret span at the column inside the shaped window.
- Editable: `TextEdit` decorations (`Span::bg` on the line, `Span::underline`
  at the column) so Rune maps them through wrapping and IME preedit, plus a
  circular-dot `line N, column C` strip above the field. Both dots are drawn
  shapes, not font glyphs, so missing fonts cannot turn them into tofu. A
  text field is one scroll target; the keyed strip is `row_key(base, line - 1)` so
  `Tx::scroll_into_view` still works, but the precise position is the
  underline.

### Editable thresholds

`EDITABLE_LIMIT_BYTES` is 5 MiB (rp-editbench: pretty 5 MiB p95 12.3 ms,
10 MiB 24.3 ms), and any line longer than `LONG_LINE_LIMIT_BYTES` (128 KiB)
also forces the read-only view (single-line 2 MiB was 250 ms/keystroke).
Both are O(1) from the cached line index
(`Snapshot::max_line_len_exact`, same pass as the line starts); a provisional
index has no exact maximum, so a cold ≥8 MiB document is reported as a large
file. Banners: `Large file (6.9 MB) — editing disabled …` and
`Very long line (6.9 MB on one line) — Format to edit`.

### Edit-threshold evidence (rp-editbench)

The measurement branch changed no source or frozen API; it added
`tests/edit_threshold.rs`. rp-fold then applied the thresholds above. The
integrated harness uses a test-local copy of the native `rune::text_input`
branch to bypass both read-only gates: `editable_limit` alone cannot lift the
new long-line gate, and the historical minified measurements must remain
reproducible.
Re-run the release measurements with:

```sh
cargo test --release -p json-viewer --test edit_threshold -- --ignored --nocapture
```

Method: headless 1280×800, scale 1, app fonts; generated ASCII JSON at
0.5/1/2/5/10/25/50 MiB pretty and 2/10 MiB minified. The forced view copies
the real composition except for the limit override; a 1 MiB control uses the
real `ui::view`. A keystroke sample is `Runtime::key(Key::Text)` +
`runner::poll` + one frame, with 20 characters at start/middle/end (60
samples). Numbers are CPU commit/layout/instance time, not GPU presentation.
Caret+scroll uses `set_caret(len/2)` and `scroll_to_y`; paste inserts 1 KiB.
Peak RSS is process-wide and cumulative (`/proc/self/status` VmHWM); VmRSS is
after dropping the runtime. Machine: 24 cores, 62 GiB RAM, Linux, one release
run (no variance study), whole bench about 108 s.

| Variant | View | MiB | Lines | Load ms | First frame ms | Settle ms | Key median ms | Key p95 ms | Key max ms | Caret+scroll ms | Paste 1KB ms | VmHWM MiB | VmRSS MiB |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pretty 1 control | real | 1.00 | 38917 | 0.3 | 37.3 | 3.4 | 3.38 | 4.33 | 4.80 | 2.7 | 4.7 | 97 | 97 |
| pretty 0.5 | forced | 0.50 | 19478 | 0.0 | 14.8 | 2.0 | 2.48 | 2.79 | 3.50 | 2.1 | 3.3 | 99 | 99 |
| pretty 1 | forced | 1.00 | 38917 | 0.1 | 14.1 | 2.4 | 3.07 | 4.03 | 4.73 | 2.4 | 3.9 | 115 | 115 |
| pretty 2 | forced | 2.00 | 77760 | 0.2 | 15.6 | 3.2 | 4.16 | 5.79 | 6.47 | 3.2 | 5.0 | 155 | 155 |
| pretty 5 | forced | 5.00 | 193778 | 0.5 | 20.1 | 5.6 | 9.95 | 12.26 | 14.73 | 7.3 | 14.6 | 177 | 168 |
| pretty 10 | forced | 10.00 | 387153 | 1.2 | 41.0 | 18.1 | 17.38 | 24.33 | 29.08 | 11.2 | 26.8 | 252 | 243 |
| pretty 25 | forced | 25.00 | 965864 | 9.9 | 72.2 | 37.0 | 36.26 | 49.68 | 51.30 | 20.1 | 60.4 | 617 | 468 |
| pretty 50 | forced | 50.00 | 1927664 | 19.9 | 134.6 | 72.4 | 150.97 | 163.22 | 166.30 | 70.3 | 147.4 | 653 | 554 |
| minified 2 | forced | 2.00 | 1 | 0.2 | 258.6 | 1.5 | 244.87 | 250.11 | 251.66 | 2.1 | 241.0 | 653 | 345 |
| minified 10 | forced | 10.00 | 1 | 2.2 | 1345.5 | 14.5 | 1208.49 | 1222.38 | 1247.98 | 8.5 | 1217.8 | 1352 | 1033 |

The control pays one-time process/font warm-up (37.3 ms first frame versus
14.1 ms forced); keystroke medians 3.38 versus 3.07 ms validate the harness.
Position barely matters because the whole buffer is reworked per edit:

| Variant | Start key median ms | Middle | End |
| --- | ---: | ---: | ---: |
| pretty 0.5 | 2.23 | 2.48 | 2.70 |
| pretty 1 | 2.69 | 3.07 | 3.36 |
| pretty 2 | 3.86 | 4.16 | 5.26 |
| pretty 5 | 9.13 | 9.79 | 10.56 |
| pretty 10 | 17.44 | 17.31 | 18.15 |
| pretty 25 | 35.82 | 36.26 | 37.45 |
| pretty 50 | 147.67 | 156.22 | 152.56 |
| minified 2 | 244.37 | 245.06 | 245.44 |
| minified 10 | 1206.96 | 1204.04 | 1213.71 |

Per-keystroke cost breakdown (ms). “Idle frame” is a full rebuild/re-shape
with no edit; residual = key median − (before-clone + joined insert + sync).
`update_input` is a changed-text clone + `replace_text`, measured for
completeness but not on the typing hot path.

| Variant | Key median | text.clone (Rune before) | insert first (undo snap) | insert join | sync_active_editor | TextEdit.clone (view) | display() | Idle frame | Residual | update_input |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pretty 1 control | 3.38 | 0.05 | 0.51 | 0.05 | 0.08 | 0.07 | 0.44 | 3.97 | 3.19 | 0.13 |
| pretty 0.5 | 2.48 | 0.24 | 0.03 | 0.02 | 0.02 | 0.03 | 0.01 | 2.86 | 2.20 | 0.03 |
| pretty 1 | 3.07 | 0.04 | 0.43 | 0.06 | 0.07 | 0.04 | 0.43 | 3.31 | 2.90 | 0.06 |
| pretty 2 | 4.16 | 0.09 | 0.31 | 0.12 | 0.18 | 0.14 | 0.15 | 4.23 | 3.76 | 0.18 |
| pretty 5 | 9.95 | 0.48 | 2.68 | 0.48 | 0.57 | 0.50 | 2.27 | 7.57 | 8.41 | 0.50 |
| pretty 10 | 17.38 | 1.08 | 5.14 | 1.03 | 1.28 | 0.95 | 1.05 | 13.34 | 13.99 | 1.38 |
| pretty 25 | 36.26 | 8.78 | 2.67 | 2.22 | 3.18 | 9.33 | 8.85 | 29.42 | 22.09 | 5.62 |
| pretty 50 | 150.97 | 14.92 | 6.15 | 7.12 | 18.75 | 17.34 | 17.32 | 88.34 | 110.19 | 25.43 |
| minified 2 | 244.87 | 0.20 | 0.21 | 0.12 | 0.20 | 0.14 | 0.15 | 245.69 | 244.35 | 0.17 |
| minified 10 | 1208.49 | 1.11 | 1.26 | 0.79 | 1.30 | 1.09 | 1.00 | 1224.81 | 1205.29 | 1.29 |

Findings behind the shipped gates:

1. Rune re-shapes/re-lays-out the whole buffer on every text change. The
   residual/idle-frame term dominates: about 80–90% for pretty ≤25 MiB and
   >99% for minified. `text_input` clones `TextEdit` and rebuilds the display
   string on each view build; caret position is irrelevant.
2. Wrapped minified input is catastrophic: 2 MiB costs about 245 ms/key and
   10 MiB about 1.2 s/key plus ~1 GiB RSS for wrap layout. A no-change frame
   is cheap because layout survives; editing invalidates all of it. The 16 ms
   budget is reached near 0.13 MiB on one line, hence 128 KiB.
3. App copies are secondary at the threshold: `sync_active_editor` costs
   0.02 ms at 0.5 MiB, 1.3 ms at 10 MiB and 18.8 ms at 50 MiB. Rune's
   pre-edit clone costs 14.9 ms at 50 MiB; with view clone/display those
   copies are ~45% there, but only ~2–3 ms total around 5–10 MiB.
4. Search and persistence do not run per keystroke. The editor syncs the
   model directly; search starts on `SearchChanged`, while persist is
   debounced and queued only by Clear/CloseTab/JsonDone.
5. The first key in a typing run snapshots all text for undo; consecutive
   keys coalesce. Caret moves close the run, so each position block pays one
   snapshot.
6. Pretty mounting stays below 250 ms even at 50 MiB (134.6 ms): the editing
   limit comes from keystroke latency, not initial mount. Minified 2 MiB
   fails both budgets.

The 5 MiB gate is the largest tested pretty size passing p95 <16 ms and first
frame <250 ms; interpolation passes near 6.5 MiB, while 10 MiB fails. Caveats:
ASCII fixtures, release CPU-only timings, one run on the machine above. A
real fix for larger editable input needs Rune-side incremental or
viewport-bounded wrapped-text layout, not another app-side threshold.

### Monospace metrics

`text_view::char_width()` measures one `'0'` through
`rune::text::advance` on the active text system instead of hard-coding a
font's advance; `line_height()` uses `rune::text::measure`. Tests assert
fixed pitch (equal-width `i`/`W`/digit runs) and that `char_width()` is that
measurement, never a specific IBM Plex/DejaVu/Consolas value. The editable
error strip sizes itself from `line_height()`.

### File drop

- `.on_drop_files(|s: &mut S, files: &[PathBuf], tx| …)` on an element accepts
  drops; `.accepts_files(|path| …)` filters before the handler.
  `.on_drop_move(|s, point, tx| …)` reports the pointer for a caret;
  `.on_drop_hover(false)` clears it.
- Put the handler on the root to accept drops anywhere. Drop zones take no
  pointer events.
- Wayland in winit 0.30 reports no file drags; macOS, Windows and X11 do.
  Headless scripts use `{"drop_files": {"paths": [...], "key": "..."}}`.
- `io::drop_zone(el)` is the app's only drop path. It accepts `.json`
  (case-insensitive), dispatches `Msg::FileDropped` for the first file,
  reports `Not a JSON file: {name}` for rejects, and paints a keyed
  `drop-highlight` overlay while a drag is over the window. Hover state is a
  thread-local because `AppState` has no hover field; the status write forces
  the rebuild that paints the overlay.

### Clipboard and dialogs

- Rune has its own clipboard stack: `Tx::copy(text)` requests a shell write
  (asynchronous, skipped in headless runs), `tx.copy_rich`,
  `tx.read_clipboard`, and `rune::clipboard` for HTML/RTF/images. On Linux it
  uses X11 or the Wayland data-control protocol.
- `io::start_copy(Some(tx), ..)` returns `CopyQueued::Rune` (handled by
  `Tx::copy`) or `CopyQueued::NeedsArboard`; the latter runs
  `io::spawn_clipboard_copy` on the `json-viewer-clipboard` thread. Plain
  `copy_to_clipboard` still blocks for callers without a `Tx`.
- Rune's native file dialogs are behind the `native-file-dialog` feature:
  `FileDialogs::native().open(options).await`, parented to the window and
  never blocking Cocoa's main thread. `runner::execute` uses
  `Tx::open_file`/`Tx::save_file` with `io::open_dialog_options`/
  `io::export_dialog_options`; reads and writes run on
  `json-viewer-read`/`json-viewer-write`. Never `pollster::block_on` a dialog
  on the UI thread.
- Headless clipboard uses an in-memory backend; no system clipboard writes.
- `Effect::Persist` is debounced onto a persist worker (`runner::spawn_persist`);
  `on_close`/`on_quit` call `flush_persist` so the last snapshot lands before
  the window or app goes away.

### Gotchas

- The Rune git workspace uses edition 2024 and resolver 3. Our crates stay on
  edition 2021; do not add `resolver = "3"` to the root without checking the
  stable toolchain.
- Rune's default features are broad (accessibility, D-Bus, Metal on Apple,
  media, SVG). The first `cargo build` compiles a large tree; incremental
  builds are normal afterwards.
- `serde_json` with `preserve_order` keeps key insertion order, matching JS
  `JSON.stringify` for objects. Number formatting still differs (`1.0`
  serializes as `1.0`, JS prints `1`), and serde does not escape non-ASCII the
  way JS does. Decide on a canonical round-trip policy before golden tests.
- `serde_json::Error::line()`/`column()` are 1-based; `JsonError` stores them
  and strips the duplicated "at line …" suffix from the message.
- Search line numbers point into the **pretty-printed** document, not the raw
  input. This is intentional: the output view is what users jump to.
- Search runs on a detached thread without a cancel handle in the scaffold.
  Add a per-tab `AtomicBool`/`JobHandle` when implementing large-document
  search; `search::search` already accepts `&AtomicBool`.
- `--tree` in Rune expects a file path, unlike `--snap` which also accepts a
  path. Passing just `--tree` is a parse error.
- Element text is visible to the accessibility dump only when it is an
  actual text element; CSS-like container labels do not appear. The smoke
  test asserts the header text node.
- Headless output is deterministic on the virtual clock; keep the two-frame
  + `settle_all` sequence when adding animations or the PNG may catch a
  transition mid-flight.
- Rune prints its chosen GPU and a one-frame timing summary on headless runs;
  that output is informational, not an error.
