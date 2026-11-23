# JSON Viewer

A native desktop app for viewing, formatting, and searching JSON, built in Rust
with [Rune](https://github.com/anomalyco/rune), a GPU-rendered UI framework.
Ported from the original Electron + React app; dark theme by default.

## Features

- Multiple document tabs, persisted across restarts
- Format (pretty-print) and minify JSON on background worker threads
- Full-text search with a results panel; hits jump to lines in the formatted document
- Open files via dialog or drag-and-drop, export to file, copy to clipboard
- Virtualized read-only view: only visible lines are built, so large documents stay responsive
- Cross-platform: Linux, macOS, Windows

## Requirements

- **Rust 1.99.1** — pinned by `rust-toolchain.toml`; rustup installs/selects it automatically.
- **Linux build dependencies** — Rune links `libxkbcommon` and `libwayland-client`:
  - Debian/Ubuntu: `sudo apt-get install libxkbcommon-dev libwayland-dev`
  - Fedora: `sudo dnf install libxkbcommon-devel wayland-devel`
  - Arch: `sudo pacman -S libxkbcommon wayland`
- **Linux runtime (windowed)** — the XKB X11 library:
  `libxkbcommon-x11-0` (Debian/Ubuntu), `libxkbcommon-x11` (Fedora/Arch).
  Without it the window panics at startup with `Library libxkbcommon-x11.so could not be loaded`.
- **Vulkan driver** — rendering uses wgpu/Vulkan. Without a physical GPU, install a
  software driver (lavapipe): `mesa-vulkan-drivers` (Debian/Ubuntu, Fedora) or
  `vulkan-swrast` (Arch), then run with
  `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json`.
- File dialogs use `xdg-desktop-portal` over D-Bus (no GTK required).
- Headless rendering needs no display server, only a working Vulkan backend.
- macOS and Windows need no extra system packages.

## Build and run

```sh
cargo run --release -p json-viewer
```

The first build compiles the full Rune dependency tree and takes a while;
incremental builds are fast. For a debug build: `cargo run -p json-viewer`.

## Headless snapshots

Render offscreen to a PNG and dump the element tree (debug builds; release
builds need Rune's `tooling` feature for `--snap`/`--tree`):

```sh
cargo run -p json-viewer -- --snap target/headless/app.png --tree target/headless/tree.json
```

The crate's own headless mode (works in debug and release):

```sh
cargo run -p json-viewer -- --headless target/headless
# -> target/headless/placeholder.png and placeholder-tree.json
```

## Tests

```sh
cargo test                              # unit + integration tests
cargo test -p json-viewer --test smoke  # headless PNG smoke test
cargo clippy --all-targets
```

The smoke test writes `target/headless/smoke.png` and `smoke-tree.json` and
asserts the rendered tree contains the app header.

## Wayland

File drag-and-drop is not available on Wayland (winit 0.30 reports no file
drags); use the Open button instead. See Wayland section.

## Project layout

- `crates/app/` — the Rust application (model, workers, persistence, UI, headless helpers)
- `AGENTS.md` — architecture, data-flow rules, and Rune notes for contributors

## License

MIT — see [LICENSE](LICENSE).
