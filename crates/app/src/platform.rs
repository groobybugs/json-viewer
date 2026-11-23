//! Linux window-backend selection.
//!
//! winit 0.30 (the copy Rune vendors at the pinned revision) has no
//! `WINIT_UNIX_BACKEND` variable. It prefers Wayland whenever `WAYLAND_DISPLAY`
//! or `WAYLAND_SOCKET` is non-empty, even if `DISPLAY` points at XWayland.
//! Rune builds the event loop itself and never calls
//! `EventLoopBuilderExtX11::with_x11`, so the only force-X11 path is to clear
//! those two variables before `App::run`, leaving `DISPLAY` set.
//!
//! Native Wayland stays the default: fractional scaling and IME are better
//! there. X11/XWayland is opt-in because that is the backend where winit 0.30
//! reports file drags (XDND). macOS and Windows ignore the choice.

/// Requested window backend. `Auto` keeps the platform default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Wayland,
    X11,
}

/// Where the request came from. `--x11` wins over the environment variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendSource {
    Default,
    Flag,
    Env,
}

/// Parsed backend request. Pure: does not read or write the process environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendRequest {
    pub backend: Backend,
    pub source: BackendSource,
}

/// `JSON_VIEWER_BACKEND`, if set to valid Unicode. Unset is `Ok(None)`.
pub fn backend_env() -> Result<Option<String>, String> {
    match std::env::var("JSON_VIEWER_BACKEND") {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("JSON_VIEWER_BACKEND is not valid Unicode".into())
        }
    }
}

/// Parse `--x11` and an optional `JSON_VIEWER_BACKEND` value (`x11`, `wayland`,
/// or `auto`, ASCII case-insensitive, surrounding whitespace ignored).
///
/// The flag wins, including over `wayland`. An unknown or empty value is an
/// error. Other arguments are ignored so Rune flags (`--snap`, `--headless`)
/// can share the command line.
pub fn resolve(args: &[String], env_value: Option<&str>) -> Result<BackendRequest, String> {
    if args.iter().any(|arg| arg == "--x11") {
        return Ok(BackendRequest {
            backend: Backend::X11,
            source: BackendSource::Flag,
        });
    }
    let Some(raw) = env_value else {
        return Ok(BackendRequest {
            backend: Backend::Auto,
            source: BackendSource::Default,
        });
    };
    let token = raw.trim();
    if token.is_empty() {
        return Err(unknown_backend(raw));
    }
    let backend = match token.to_ascii_lowercase().as_str() {
        "auto" => Backend::Auto,
        "wayland" => Backend::Wayland,
        "x11" => Backend::X11,
        _ => return Err(unknown_backend(raw)),
    };
    Ok(BackendRequest {
        backend,
        source: BackendSource::Env,
    })
}

fn unknown_backend(raw: &str) -> String {
    format!("JSON_VIEWER_BACKEND must be x11, wayland, or auto (got {raw:?})")
}

/// Variables winit 0.30 reads before preferring Wayland. X11 clears only these.
/// `DISPLAY` is never cleared: without it there is no XWayland fallback.
pub fn wayland_vars_to_clear(backend: Backend) -> &'static [&'static str] {
    match backend {
        Backend::X11 => &["WAYLAND_DISPLAY", "WAYLAND_SOCKET"],
        Backend::Auto | Backend::Wayland => &[],
    }
}

/// Apply `request` on Linux before the event loop is built. No-op on other
/// operating systems (a request for X11 is logged and ignored).
pub fn apply(request: &BackendRequest) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        if request.backend == Backend::X11 {
            eprintln!("json-viewer: --x11 / JSON_VIEWER_BACKEND=x11 is Linux-only; ignored");
        }
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    linux_apply(request)
}

#[cfg(target_os = "linux")]
fn linux_apply(request: &BackendRequest) -> Result<(), String> {
    if std::env::var_os("WINIT_UNIX_BACKEND").is_some() && request.backend != Backend::X11 {
        eprintln!(
            "json-viewer: WINIT_UNIX_BACKEND is ignored by winit 0.30; use --x11 or JSON_VIEWER_BACKEND=x11"
        );
    }
    match request.backend {
        Backend::Auto => Ok(()),
        Backend::Wayland => {
            if std::env::var_os("WAYLAND_DISPLAY").is_none()
                && std::env::var_os("WAYLAND_SOCKET").is_none()
            {
                eprintln!(
                    "json-viewer: JSON_VIEWER_BACKEND=wayland but WAYLAND_DISPLAY and WAYLAND_SOCKET are unset; winit will use X11 if DISPLAY is set"
                );
            } else {
                eprintln!(
                    "json-viewer: window backend wayland (JSON_VIEWER_BACKEND=wayland); file drag-and-drop unavailable (winit 0.30)"
                );
            }
            Ok(())
        }
        Backend::X11 => force_x11(request.source),
    }
}

#[cfg(target_os = "linux")]
fn force_x11(source: BackendSource) -> Result<(), String> {
    let display = match std::env::var("DISPLAY") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            return Err(
                "--x11 / JSON_VIEWER_BACKEND=x11 needs DISPLAY (X11 or XWayland). It is unset."
                    .into(),
            );
        }
    };
    let had_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("WAYLAND_SOCKET").is_some();
    // SAFETY: called on the main thread before any other thread starts and
    // before winit reads these variables in `EventLoop::build`. `remove_var`
    // is unsafe since Rust 1.87 because the process environment is shared
    // with C `getenv` without a lock.
    for key in wayland_vars_to_clear(Backend::X11) {
        unsafe { std::env::remove_var(key) };
    }
    let why = match source {
        BackendSource::Flag => "--x11",
        BackendSource::Env => "JSON_VIEWER_BACKEND=x11",
        BackendSource::Default => "auto",
    };
    let via = if had_wayland { "; XWayland" } else { "" };
    eprintln!(
        "json-viewer: window backend x11 ({why}); cleared WAYLAND_DISPLAY and WAYLAND_SOCKET; DISPLAY={display}{via}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn default_is_auto() {
        let request = resolve(&[], None).unwrap();
        assert_eq!(request.backend, Backend::Auto);
        assert_eq!(request.source, BackendSource::Default);
    }

    #[test]
    fn flag_selects_x11_and_wins_over_env() {
        let flagged = resolve(&args(&["--x11"]), None).unwrap();
        assert_eq!(flagged.backend, Backend::X11);
        assert_eq!(flagged.source, BackendSource::Flag);

        let wins = resolve(&args(&["--headless", "--x11"]), Some("wayland")).unwrap();
        assert_eq!(wins.backend, Backend::X11);
        assert_eq!(wins.source, BackendSource::Flag);
    }

    #[test]
    fn env_values() {
        assert_eq!(resolve(&[], Some("x11")).unwrap().backend, Backend::X11);
        assert_eq!(
            resolve(&[], Some("x11")).unwrap().source,
            BackendSource::Env
        );
        assert_eq!(
            resolve(&[], Some("wayland")).unwrap().backend,
            Backend::Wayland
        );
        assert_eq!(resolve(&[], Some("auto")).unwrap().backend, Backend::Auto);
        assert_eq!(resolve(&[], Some("X11")).unwrap().backend, Backend::X11);
        assert_eq!(
            resolve(&[], Some("  Wayland  ")).unwrap().backend,
            Backend::Wayland
        );
    }

    #[test]
    fn unknown_or_empty_env_errors() {
        let err = resolve(&[], Some("xwayland")).unwrap_err();
        assert!(err.contains("JSON_VIEWER_BACKEND"), "{err}");
        assert!(err.contains("xwayland"), "{err}");
        assert!(resolve(&[], Some("")).is_err());
        assert!(resolve(&[], Some("   ")).is_err());
        assert!(resolve(&[], Some("x11-wayland")).is_err());
    }

    #[test]
    fn unrelated_args_stay_auto() {
        let request = resolve(&args(&["--snap", "out.png", "--x11-mode"]), None).unwrap();
        assert_eq!(request.backend, Backend::Auto);
        assert_eq!(request.source, BackendSource::Default);
    }

    #[test]
    fn x11_clears_only_wayland_selectors() {
        assert_eq!(
            wayland_vars_to_clear(Backend::X11),
            &["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
        );
        assert!(wayland_vars_to_clear(Backend::Auto).is_empty());
        assert!(wayland_vars_to_clear(Backend::Wayland).is_empty());
        assert!(!wayland_vars_to_clear(Backend::X11).contains(&"DISPLAY"));
    }
}
