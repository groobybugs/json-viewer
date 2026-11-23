# AGENTS.md — JSON Viewer (Rust + Rune)

Wave 0 scaffold for porting the Electron + React app in `src/` and `electron/`
to Rust with [Rune](https://github.com/anomalyco/rune). The old TypeScript and
Electron files are **read-only reference** during this port: do not edit or
delete them. Rust lives in `crates/app/`.

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
    main.rs                    window, --headless mode, boot/persistence
    state.rs                   pure model, Msg, Effect, reducers, reducer tests
    json_ops.rs                format/minify + background job
    search.rs                  JSON search (worker semantics)
    persist.rs                 tab store (replaces IndexedDB)
    io.rs                      dialogs, file read/write, clipboard, drop
    runner.rs                  drains Msg queue, executes Effect values
    headless.rs                offscreen PNG + element-tree dump
    ui/
      mod.rs                   fn view + composition
      shell.rs                 page frame, header, status line
      tabs.rs                  tab strip + active document
      toolbar.rs               Format/Minify/Copy/Export/Open/Clear
      search_bar.rs            search field + results panel
      text_view.rs             TextView: editable or virtualized read-only
  tests/
    smoke.rs                   PNG smoke test + title assertion
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
  methods, `AppState::update`.
- `json_ops.rs`: `JsonAction`, `JsonError`, `process`, `JsonJob`,
  `spawn_process`, `JobHandle`.
- `search.rs`: `SearchHit`, `SearchOptions`, `search`.
- `persist.rs`: `PersistError`, `data_path`, `TabsStore`, `load_tabs`,
  `save_tabs`.
- `io.rs`: `IoError`, `read_json_file`, `pick_open_file`,
  `pick_export_path`, `write_file`, `copy_to_clipboard`, `on_file_dropped`.
- `ui/mod.rs`: `view`, `TITLE`.
- `ui/shell.rs`: `shell`, `header`, `status_bar`.
- `ui/tabs.rs`: `tab_bar`, `tab_content`.
- `ui/toolbar.rs`: `toolbar`.
- `ui/search_bar.rs`: `search_bar`, `search_results`.
- `ui/text_view.rs`: `TextViewMode`, `TextView::{editable, read_only,
  first_line, visible_lines, line_numbers, highlight, line_count,
  visible_range, line, build}`.
- `headless.rs`: `DEFAULT_SIZE`, `DEFAULT_SCALE`, `runtime`, `render_png`,
  `element_tree`, `write_tree`, `render_and_dump`.

`src/` and `electron/` behavior is the reference for semantics, especially
`tabsSlice.ts` (reducers), `jsonWorker.ts` (JSON and search), and
`JsonViewer.tsx` / `SearchBar.tsx` / `FileDropzone.tsx` (UI behavior).

## Ownership map

One owner per file. Do not edit another owner's file; coordinate through the
frozen API or a PR comment.

| File | Owner / scope |
| --- | --- |
| `crates/app/src/state.rs` | model, reducers, messages, effects |
| `crates/app/src/json_ops.rs` | JSON parse/format/minify worker |
| `crates/app/src/search.rs` | search traversal and hit model |
| `crates/app/src/persist.rs` | tab persistence |
| `crates/app/src/io.rs` | dialogs, files, clipboard, drop |
| `crates/app/src/runner.rs` | message/effect loop |
| `crates/app/src/ui/mod.rs` | view composition |
| `crates/app/src/ui/shell.rs` | page frame/header/status |
| `crates/app/src/ui/tabs.rs` | tab strip/document area |
| `crates/app/src/ui/toolbar.rs` | action toolbar |
| `crates/app/src/ui/search_bar.rs` | search field/results |
| `crates/app/src/ui/text_view.rs` | text input/output virtualization |
| `crates/app/src/headless.rs` | headless render/tree helpers |
| `crates/app/src/main.rs` | binary, window wiring, `--headless` |
| `crates/app/tests/smoke.rs` | headless smoke test |
| `crates/app/Cargo.toml`, `Cargo.toml` | dependencies/workspace (ask first) |

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
- `TextView` uses the simpler windowed contract (`first_line` +
  `visible_lines`) because JSON lines are fixed-height monospace. A later
  worker can wire a scroll observer to those fields or switch the read-only
  path to `virtual_rows`; both preserve the frozen API.
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
- Our view does not yet attach `on_drop_files`; `FileDropped`/`Effect::ReadDroppedFile`
  are ready for the worker who owns `ui/mod.rs`.

### Clipboard and dialogs

- Rune has its own clipboard stack: `Tx::copy(text)` requests a shell write
  (asynchronous, skipped in headless runs), `tx.copy_rich`,
  `tx.read_clipboard`, and `rune::clipboard` for HTML/RTF/images. On Linux it
  uses X11 or the Wayland data-control protocol.
- We also depend on `arboard` directly because `io::copy_to_clipboard` is
  called from the runner thread without a `Tx`. Consider migrating to
  `Tx::copy` once copy moves into a view handler.
- Rune's native file dialogs are behind the `native-file-dialog` feature:
  `FileDialogs::native().open(options).await`, parented to the window and
  never blocking Cocoa's main thread. We currently use `rfd` synchronously
  from the runner. That is acceptable for the scaffold but should become an
  async task or a worker before release (Rune explicitly warns against
  `pollster::block_on` on the main thread).
- Headless clipboard uses an in-memory backend; no system clipboard writes.

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
