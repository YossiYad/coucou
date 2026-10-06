// A glowing frame around the screen Coucou can see, like a screen-sharing
// indicator: on for as long as screen sharing is on (following the screen the
// mouse is on, which is the one a screenshot would take), and flashed when a
// single screenshot is taken. One transparent, click-through window per
// monitor, made at launch and only shown, moved and hidden afterwards.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, Monitor, PhysicalPosition, PhysicalSize, Runtime, WebviewUrl, WebviewWindowBuilder};

use crate::Shared;

const LABEL: &str = "frame";
/// How long a single screenshot keeps the frame up.
const FLASH: Duration = Duration::from_millis(2500);
const FOLLOW_EVERY: Duration = Duration::from_millis(700);

static FLASHING: AtomicBool = AtomicBool::new(false);

fn label(i: usize) -> String {
    format!("{LABEL}-{i}")
}

fn page_url<R: Runtime>(app: &AppHandle<R>) -> WebviewUrl {
    #[cfg(dev)]
    if let Some(mut base) = app.config().build.dev_url.clone() {
        base.set_path("/frame.html");
        return WebviewUrl::External(base);
    }
    let _ = app;
    WebviewUrl::App("frame.html".into())
}

/// One hidden frame window per monitor, at launch (see create_settings_window
/// for why windows are made up front).
pub fn create(app: &AppHandle) {
    let count = app.available_monitors().map(|m| m.len()).unwrap_or(1).max(1);
    for i in 0..count {
        build(app, i);
    }
    let handle = app.clone();
    tauri::async_runtime::spawn(async move { follow(handle).await });
}

/// The monitors to frame: every one, or the one the mouse is on.
fn targets<R: Runtime>(app: &AppHandle<R>, all: bool) -> Vec<Monitor> {
    let monitors = app.available_monitors().unwrap_or_default();
    if all {
        return monitors;
    }
    if let Some(name) = active_output() {
        if let Some(m) = monitors.iter().find(|m| m.name().is_some_and(|n| *n == name)) {
            return vec![m.clone()];
        }
    }
    app.primary_monitor().ok().flatten().into_iter().collect()
}

/// KWin's active output: the screen the mouse is on, the one Spectacle's
/// "current monitor" takes.
#[cfg(target_os = "linux")]
fn active_output() -> Option<String> {
    for program in ["qdbus-qt6", "qdbus6", "qdbus"] {
        let mut cmd = std::process::Command::new(program);
        cmd.args(["org.kde.KWin", "/KWin", "org.kde.KWin.activeOutputName"]);
        crate::linux::clean_env(&mut cmd);
        if let Ok(out) = cmd.output() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn active_output() -> Option<String> {
    None
}

/// The hidden frame window for monitor `i`.
fn build<R: Runtime>(app: &AppHandle<R>, i: usize) {
    let built = WebviewWindowBuilder::new(app, label(i), page_url(app))
        .title("Coucou can see this screen")
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .resizable(true)
        .visible(false)
        .build();
    // Clicks are let through once it is shown: before that GTK has no
    // window to set it on, and tao panics.
    if let Err(err) = built {
        crate::log::line(format!("frame window failed: {err}"));
    }
}

/// Frames exactly `monitors`, hides the rest.
fn show_on<R: Runtime>(app: &AppHandle<R>, monitors: &[Monitor]) {
    // A monitor plugged in since launch has no window yet: it gets one now,
    // or it would be the one screen shared without the frame saying so.
    for i in 0..monitors.len() {
        if app.get_webview_window(&label(i)).is_none() {
            crate::log::line(format!("frame window for a new screen ({i})"));
            build(app, i);
        }
    }
    let mut i = 0;
    while let Some(win) = app.get_webview_window(&label(i)) {
        match monitors.get(i) {
            Some(m) => {
                let (p, s) = (*m.position(), *m.size());
                let _ = win.set_size(PhysicalSize::new(s.width, s.height));
                let _ = win.set_position(PhysicalPosition::new(p.x, p.y));
                let _ = win.set_size(PhysicalSize::new(s.width, s.height));
                if !win.is_visible().unwrap_or(false) {
                    let _ = win.show();
                    let _ = win.set_ignore_cursor_events(true);
                }
                let _ = win.set_always_on_top(true);
            }
            None => {
                let _ = win.hide();
            }
        }
        i += 1;
    }
}

fn hide_all<R: Runtime>(app: &AppHandle<R>) {
    show_on(app, &[]);
}

fn sharing<R: Runtime>(app: &AppHandle<R>) -> (bool, bool) {
    let shared = app.state::<Shared>();
    let s = shared.settings.lock().unwrap();
    (s.screen_sharing, s.screen_scope == "all")
}

/// While screen sharing is on, the frame stays on the screen that would be
/// shared, following the mouse from screen to screen.
async fn follow(app: AppHandle) {
    let mut shown = false;
    loop {
        tokio::time::sleep(FOLLOW_EVERY).await;
        if FLASHING.load(Ordering::SeqCst) {
            continue;
        }
        let (on, all) = sharing(&app);
        if on {
            let monitors = {
                let app = app.clone();
                tauri::async_runtime::spawn_blocking(move || targets(&app, all)).await.unwrap_or_default()
            };
            show_on(&app, &monitors);
            shown = true;
        } else if shown {
            hide_all(&app);
            shown = false;
        }
    }
}

/// A screenshot was just taken: frame that screen for a moment and pulse.
pub fn flash<R: Runtime>(app: &AppHandle<R>, all: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        FLASHING.store(true, Ordering::SeqCst);
        let monitors = {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || targets(&app, all)).await.unwrap_or_default()
        };
        show_on(&app, &monitors);
        let _ = app.emit("frame-pulse", ());
        tokio::time::sleep(FLASH).await;
        FLASHING.store(false, Ordering::SeqCst);
        if !sharing(&app).0 {
            hide_all(&app);
        }
    });
}
