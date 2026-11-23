# Linux launcher

Native Wayland is the default (`cargo run -p json-viewer`). Fractional scaling
and IME are better there. File drag-and-drop is not: winit 0.30 never reports
file drags on Wayland.

`json-viewer.desktop` opts into X11/XWayland with `--x11`. On a Wayland
session that is an XWayland window, so XDND drops work. Cost: fractional
scaling is often blurry, and IME (fcitx, ibus) is weaker than native Wayland.
macOS and Windows ignore `--x11` and `JSON_VIEWER_BACKEND`.

This winit has no `WINIT_UNIX_BACKEND`. The flag (or
`JSON_VIEWER_BACKEND=x11`) clears `WAYLAND_DISPLAY` and `WAYLAND_SOCKET`
before the event loop starts. `DISPLAY` must already be set (XWayland
provides it).

## Install

Edit `Exec=` first if the binary is not on `PATH`. Point it at the release
binary, still with `--x11`:

```
Exec=/absolute/path/to/json-viewer --x11
```

Then:

```sh
install -Dm644 packaging/linux/json-viewer.desktop \
  ~/.local/share/applications/json-viewer.desktop
update-desktop-database ~/.local/share/applications || true
```

Equivalent without the desktop file:

```sh
json-viewer --x11
JSON_VIEWER_BACKEND=x11 json-viewer
```

`--x11` wins over the environment variable. `JSON_VIEWER_BACKEND=wayland`
forces the native default explicitly. `auto` (or omitting both) does not
touch the environment.

The window class / Wayland app id is `json-viewer` (`StartupWMClass`).

Manual drag steps are in the repository README, section "Wayland".
