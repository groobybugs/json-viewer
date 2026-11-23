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
    headless.rs                offscreen PNG + element-tree dump
    bin/gen_fixtures.rs        QA fixture generator
    bin/screenshot.rs          renders docs/screenshot.png
    ui/
      mod.rs                   fn view + composition
      shell.rs                 page frame, header, status line
      tabs.rs                  tab strip + active document
      toolbar.rs               Format/Minify/Copy/Export/Open/Clear
      search_bar.rs            search field + results panel
      text_view.rs             TextView: editable or virtualized read-only
  tests/
    smoke.rs                   PNG smoke test + title assertion
    headless.rs                scripted input QA suite
    perf.rs                    ignored 100 MB perf harness
    text_view.rs               TextView + line index rendering tests
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
        SpawnJson/Search -> worker threads -> Msg back into the inbox
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
  `spawn_process`, `JobHandle`.
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
  visible_range, line, build, revision, row_key, build_revealing}`.
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
| `crates/app/src/persist.rs` | tab persistence |
| `crates/app/src/io.rs` | dialogs, files, clipboard, drop |
| `crates/app/src/runner.rs` | message/effect loop |
| `crates/app/src/ui/mod.rs` | view composition |
| `crates/app/src/ui/shell.rs` | page frame/header/status |
| `crates/app/src/ui/tabs.rs` | tab strip/document area |
| `crates/app/src/ui/toolbar.rs` | action toolbar |
| `crates/app/src/ui/search_bar.rs` | search field/results |
| `crates/app/src/ui/text_view.rs` | text input/output virtualization |
| `crates/app/src/tree_index.rs` | tree explorer: node index, row jobs |
| `crates/app/src/ui/tree_view.rs` | tree explorer: toolbar, rows, footer |
| `crates/app/tests/tree_view.rs` | tree explorer evidence + timings |
| `crates/app/src/headless.rs` | headless render/tree helpers |
| `crates/app/src/main.rs` | binary, window wiring, `--headless` |
| `crates/app/tests/smoke.rs` | headless smoke test |
| `crates/app/Cargo.toml`, `Cargo.toml` | dependencies/workspace (ask first) |

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
