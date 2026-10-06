// Island window: placement on the chosen display, the two window sizes
// (full panel / invisible wake strip), click-through and the cursor poll.
//
// There is no notch on a PC, so the island is a black shape drawn at the top
// centre of the main display inside a borderless, transparent, always-on-top
// window that never takes focus.
//
// Windows: Win32 window styles, a 60 Hz cursor poll and WS_EX_TRANSPARENT for
// click-through.
// Linux: GTK hints for the window, and an input region shaped like the island
// for click-through. Wayland gives no app the global cursor position, so the
// island follows the mouse from its own DOM events instead, and the poll thread
// only watches for display changes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewWindow};

#[cfg(windows)]
use windows::core::BOOL;
#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM, POINT};
#[cfg(windows)]
use windows::Win32::System::Ole::RevokeDragDrop;
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetCursorPos, GetWindowLongPtrW, SetWindowLongPtrW,
    GWL_EXSTYLE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

/// Logical size of the full window — the largest island view, like the macOS panel.
pub const PANEL_W: f64 = 720.0;
pub const PANEL_H: f64 = 320.0;
/// Logical size of the invisible strip that wakes the island when it is hidden.
pub const STRIP_W: f64 = 240.0;
pub const STRIP_H: f64 = 6.0;

pub const WINDOW_LABEL: &str = "island";

/// The monitor the island was dragged to, from the settings (empty: none).
fn dragged_screen(app: &AppHandle) -> String {
    app.try_state::<crate::Shared>()
        .map(|s| s.settings.lock().unwrap().dock_screen.clone())
        .unwrap_or_default()
}


/// X pixels per CSS pixel of the island: what the webview actually draws at.
/// On Linux under XWayland that is the X server's scale (see linux.rs), which
/// GTK's whole-number scale factor only approximates; elsewhere the two agree.
fn ui_scale(fallback: f64) -> f64 {
    #[cfg(target_os = "linux")]
    {
        crate::linux::ui_scale().unwrap_or(fallback)
    }
    #[cfg(not(target_os = "linux"))]
    {
        fallback
    }
}

/// A size in CSS pixels as the logical pixels Tauri sizes a window in.
pub fn css_to_window(v: f64) -> f64 {
    #[cfg(target_os = "linux")]
    {
        crate::linux::css_to_gtk(v)
    }
    #[cfg(not(target_os = "linux"))]
    {
        v
    }
}

/// Margin around the island that still counts as "on the island", in logical px.
/// Wider than the macOS 6 pt because a click must never be swallowed.
const HIT_MARGIN: f64 = 14.0;

#[derive(Serialize, Clone)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct CursorPayload {
    pub x: f64,
    pub y: f64,
}

#[derive(Serialize, Clone)]
pub struct ScreenInfo {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
    /// What a drag position from the toolkit is multiplied by to land in CSS
    /// pixels: on Linux GTK reports drags in its own logical pixels, which are
    /// CSS pixels only when the page draws at GTK's scale factor (see linux.rs).
    #[serde(rename = "dragScale")]
    pub drag_scale: f64,
}

/// The island shape in window-logical coordinates, pushed by the front end.
/// The poll thread owns the click-through decision so it lands in the same 16 ms
/// tick as the cursor read — an IPC round trip here loses clicks.
#[derive(Clone, Copy, Default)]
pub struct IslandRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Wakes / parks the cursor poll thread so a hidden island costs literally nothing.
pub struct PollGate {
    active: Mutex<bool>,
    cv: Condvar,
    pub collapsed: AtomicBool,
    pub rect: Mutex<IslandRect>,
    /// Mirrors the window flag so we only call into Win32 when it changes.
    #[cfg_attr(not(windows), allow(dead_code))]
    ignoring: AtomicBool,
}

impl PollGate {
    pub fn new() -> Self {
        Self {
            active: Mutex::new(false),
            cv: Condvar::new(),
            collapsed: AtomicBool::new(true),
            rect: Mutex::new(IslandRect::default()),
            ignoring: AtomicBool::new(false),
        }
    }

    pub fn set_rect(&self, rect: IslandRect) {
        *self.rect.lock().unwrap() = rect;
    }

    pub fn rect(&self) -> IslandRect {
        *self.rect.lock().unwrap()
    }

    /// Forces the next poll tick to re-apply the flag (after a window resize).
    pub fn forget_ignore_state(&self) {
        self.ignoring.store(false, Ordering::Relaxed);
    }

    pub fn set_active(&self, on: bool) {
        let mut guard = self.active.lock().unwrap();
        *guard = on;
        self.cv.notify_all();
    }

    fn wait_until_active(&self) {
        let mut guard = self.active.lock().unwrap();
        while !*guard {
            guard = self.cv.wait(guard).unwrap();
        }
    }

    fn is_active(&self) -> bool {
        *self.active.lock().unwrap()
    }
}

pub fn window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(WINDOW_LABEL)
}

#[cfg(windows)]
fn cursor_physical(_app: &AppHandle) -> Option<(f64, f64)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x as f64, p.y as f64))
}

/// Under X11 (and XWayland, where Coucou runs by default) this is the real
/// pointer position, at least while it is over an X window; native Wayland has
/// no answer at all. Only used to pick the display, so stale is good enough.
#[cfg(not(windows))]
fn cursor_physical(app: &AppHandle) -> Option<(f64, f64)> {
    let p = app.cursor_position().ok()?;
    Some((p.x, p.y))
}

/// Lets dropped files reach the app again (Windows only).
///
/// wry installs its drop target by walking the webview's child windows **once**,
/// when the webview is created. WebView2 creates `Chrome_RenderWidgetHostHWND`
/// later and registers its own target on it; being the innermost window, that one
/// wins, and since the page has no HTML5 drop handler it refuses everything — the
/// "no drop" cursor, with nothing reaching Tauri. Revoking it makes OLE fall
/// through to the target wry registered on the parent widget, which is the one
/// that feeds Tauri's drag events.
///
/// Cheap and idempotent, so it is simply re-run whenever a drag might be starting.
#[cfg(windows)]
pub fn unblock_webview_drops(app: &AppHandle) {
    for label in [WINDOW_LABEL, "settings"] {
        let Some(win) = app.get_webview_window(label) else { continue };
        let Some(hwnd) = hwnd_of(&win) else { continue };
        unsafe {
            let _ = EnumChildWindows(Some(hwnd), Some(revoke_render_widget), LPARAM(0));
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn revoke_render_widget(hwnd: HWND, _: LPARAM) -> BOOL {
    let mut name = [0u16; 64];
    let len = unsafe { GetClassNameW(hwnd, &mut name) };
    if len > 0 {
        let class = String::from_utf16_lossy(&name[..len as usize]);
        if class == "Chrome_RenderWidgetHostHWND" {
            let _ = unsafe { RevokeDragDrop(hwnd) };
        }
    }
    true.into()
}

/// True while the left mouse button is held — the only signal we get that a
/// drag might be in flight before it reaches the window.
#[cfg(windows)]
fn left_button_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0 }
}

fn monitor_contains(m: &Monitor, x: f64, y: f64) -> bool {
    let p = m.position();
    let s = m.size();
    x >= p.x as f64
        && x < (p.x + s.width as i32) as f64
        && y >= p.y as f64
        && y < (p.y + s.height as i32) as f64
}

/// The display the island lives on: the primary one, or the one under the cursor.
fn target_monitor(app: &AppHandle, pref: &str) -> Option<Monitor> {
    let monitors = app.available_monitors().ok()?;
    // Dragged to a monitor: that one, while it is connected.
    let dock_screen = dragged_screen(app);
    if !dock_screen.is_empty() {
        if let Some(m) = monitors.iter().find(|m| m.name().is_some_and(|n| *n == dock_screen)) {
            return Some(m.clone());
        }
    }
    if pref == "cursor" {
        if let Some((cx, cy)) = cursor_physical(app) {
            if let Some(m) = monitors.iter().find(|m| monitor_contains(m, cx, cy)) {
                return Some(m.clone());
            }
        }
    }
    app.primary_monitor()
        .ok()
        .flatten()
        .or_else(|| monitors.into_iter().next())
}

pub fn screen_info(app: &AppHandle, pref: &str) -> ScreenInfo {
    match target_monitor(app, pref) {
        Some(m) => {
            let scale = ui_scale(m.scale_factor());
            let p = m.position();
            let s = m.size();
            ScreenInfo {
                x: p.x as f64 / scale,
                y: p.y as f64 / scale,
                width: s.width as f64 / scale,
                height: s.height as f64 / scale,
                scale,
                drag_scale: if cfg!(target_os = "linux") { m.scale_factor() / scale } else { 1.0 },
            }
        }
        None => ScreenInfo { x: 0.0, y: 0.0, width: 1920.0, height: 1080.0, scale: 1.0, drag_scale: 1.0 },
    }
}

/// Places and sizes the window. `collapsed` picks the wake strip instead of the panel.
pub fn apply_geometry(app: &AppHandle, pref: &str, collapsed: bool) {
    place(app, pref, collapsed, true);
}

/// `apply_geometry`, checking a moment later that the window really took the
/// size and asking once more if not: at launch the first configure can land
/// after the resize and put the window back to the 240×6 of the config (GTK
/// then holds it at its natural 240×28), and the island would open into a
/// strip nobody can see.
fn place(app: &AppHandle, pref: &str, collapsed: bool, verify: bool) {
    let Some(win) = window(app) else { return };
    let Some(m) = target_monitor(app, pref) else { return };

    let scale = ui_scale(m.scale_factor());
    let mp = *m.position();
    let ms = *m.size();

    let (lw, lh) = if collapsed { (STRIP_W, STRIP_H) } else { (PANEL_W, PANEL_H) };
    let pw = (lw * scale).round().max(1.0) as u32;
    let ph = (lh * scale).round().max(1.0) as u32;
    let x = mp.x + (ms.width as i32 - pw as i32) / 2;
    let y = mp.y;

    // GTK never sizes a non-resizable window below its natural size (200 px
    // here), so on Linux the 6 px wake strip would stay a 200 px block. tao
    // re-applies the config's `resizable: false` after the first configure, so
    // this is asked every time, in order, just before the resize. Undecorated,
    // the window still offers the user nothing to resize it by.
    #[cfg(target_os = "linux")]
    let _ = win.set_resizable(true);
    let _ = win.set_size(PhysicalSize::new(pw, ph));
    let _ = win.set_position(PhysicalPosition::new(x, y));
    // Moving across displays can rescale the window: re-assert the physical size.
    let _ = win.set_size(PhysicalSize::new(pw, ph));
    let _ = win.set_always_on_top(true);

    if !verify {
        return;
    }
    let app = app.clone();
    let pref = pref.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let Some(win) = window(&app) else { return };
        let Ok(size) = win.inner_size() else { return };
        let still_wanted = app
            .try_state::<crate::Shared>()
            .map(|s| s.gate.collapsed.load(Ordering::Relaxed) == collapsed)
            .unwrap_or(true);
        if still_wanted && (size.width != pw || size.height != ph) {
            crate::log::line(format!(
                "window stayed {}x{} after asking for {pw}x{ph} (collapsed={collapsed}), asking again",
                size.width, size.height
            ));
            place(&app, &pref, collapsed, false);
        }
    });
}

#[cfg(windows)]
fn hwnd_of(win: &WebviewWindow) -> Option<HWND> {
    let raw = win.hwnd().ok()?.0 as isize;
    if raw == 0 {
        return None;
    }
    Some(HWND(raw as *mut _))
}

/// WS_EX_NOACTIVATE keeps clicks from stealing focus; WS_EX_TOOLWINDOW keeps the
/// island out of Alt-Tab.
#[cfg(windows)]
pub fn make_non_activating(win: &WebviewWindow) {
    let Some(hwnd) = hwnd_of(win) else { return };
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = ex | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
    }
}

/// Temporarily allow activation so a text field inside the island can be typed in.
#[cfg(windows)]
pub fn set_activating(win: &WebviewWindow, activating: bool) {
    let Some(hwnd) = hwnd_of(win) else { return };
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let want = if activating {
            ex & !(WS_EX_NOACTIVATE.0 as isize)
        } else {
            ex | WS_EX_NOACTIVATE.0 as isize
        };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
    }
}

/// Position, size and scale of every monitor, in a fixed order. Any change here
/// means the layout moved and the island has to be placed again. The island's
/// own monitor would not do: set to follow the cursor, it changes on every
/// crossing, and the open island would be dragged along to the other screen.
fn current_screen_key(app: &AppHandle) -> Option<ScreenKey> {
    let mut key: ScreenKey = app
        .available_monitors()
        .ok()?
        .iter()
        .map(|m| {
            let p = m.position();
            let size = m.size();
            (p.x, p.y, size.width, size.height, m.scale_factor().to_bits())
        })
        .collect();
    key.sort_unstable();
    (!key.is_empty()).then_some(key)
}

type ScreenKey = Vec<(i32, i32, u32, u32, u64)>;

/// Emits `cursor` (window-logical coordinates) at ~60 Hz while the island is
/// visible. Parked on a condvar the rest of the time.
#[cfg(windows)]
pub fn spawn_cursor_poll(app: AppHandle, gate: Arc<PollGate>) {
    std::thread::spawn(move || {
        let mut was_down = false;
        // Remembered across wakes so a display change while hidden is noticed the
        // moment the island comes back.
        let mut last_screen: Option<ScreenKey> = None;
        loop {
            gate.wait_until_active();
            let mut last = (f64::MIN, f64::MIN);
            let mut ticks: u32 = 0;
            while gate.is_active() {
                std::thread::sleep(Duration::from_millis(16));

                // Monitors get plugged in, unplugged, rearranged and rescaled, and
                // an island pinned to coordinates that no longer exist is an island
                // nobody can reach. Checked about twice a second — the cursor poll
                // is already running, so this costs one monitor query.
                ticks = ticks.wrapping_add(1);
                if ticks % 30 == 0 {
                    let now = current_screen_key(&app);
                    if now.is_some() && now != last_screen {
                        let first = last_screen.is_none();
                        last_screen = now;
                        if !first {
                            crate::log::line("display layout changed, repositioning".to_string());
                            let _ = app.emit_to(WINDOW_LABEL, "screen-changed", ());
                        }
                    }
                }

                let Some(win) = window(&app) else { continue };
                let Ok(origin) = win.outer_position() else { continue };
                let scale = win.scale_factor().unwrap_or(1.0);
                let Some((cx, cy)) = cursor_physical(&app) else { continue };
                let x = (cx - origin.x as f64) / scale;
                let y = (cy - origin.y as f64) / scale;
                let size = match win.inner_size() {
                    Ok(s) => (s.width as f64 / scale, s.height as f64 / scale),
                    Err(_) => (PANEL_W, PANEL_H),
                };
                if (x - last.0).abs() < 1.0 && (y - last.1).abs() < 1.0 {
                    continue;
                }
                last = (x, y);

                // Click-through: the window only takes the mouse over the island
                // shape. A small entry margin means the flag is already off by the
                // time a moving cursor reaches a button.
                let r = *gate.rect.lock().unwrap();
                let on_island = r.w > 0.0
                    && x >= r.x - HIT_MARGIN
                    && x <= r.x + r.w + HIT_MARGIN
                    && y >= r.y - HIT_MARGIN
                    && y <= r.y + r.h + HIT_MARGIN;

                // A file being dragged has to be able to find us. WS_EX_TRANSPARENT
                // — what click-through is on Windows — hides the window from
                // WindowFromPoint, so OLE finds no drop target and shows the "no
                // drop" cursor. macOS has no such problem: AppKit delivers drags to
                // registered destinations whatever ignoresMouseEvents says. So while
                // a button is held anywhere over the panel, the whole panel takes
                // the mouse, which also makes the drop zone as forgiving as the Mac's.
                // A press may be the start of a drag: make sure the drop target is
                // ours before the file arrives.
                let down = left_button_down();
                if down && !was_down {
                    let handle = app.clone();
                    let _ = app.run_on_main_thread(move || unblock_webview_drops(&handle));
                }
                was_down = down;

                let dragging = down
                    && x >= 0.0
                    && x <= size.0
                    && y >= 0.0
                    && y <= size.1;

                let accept = on_island || dragging;
                if gate.ignoring.load(Ordering::Relaxed) == accept {
                    gate.ignoring.store(!accept, Ordering::Relaxed);
                    let _ = win.set_ignore_cursor_events(!accept);
                }

                let _ = win.emit("cursor", CursorPayload { x, y });
            }
        }
    });
}

#[cfg(windows)]
pub fn set_ignore_cursor(app: &AppHandle, ignore: bool) {
    if let Some(win) = window(app) {
        let _ = win.set_ignore_cursor_events(ignore);
    }
}

// ── Linux ─────────────────────────────────────────────────────────────────────

/// Keeps the island out of the taskbar, the pager and the focus chain, and above
/// other windows. The window manager reads these as ordinary X11/GTK hints.
#[cfg(target_os = "linux")]
pub fn make_non_activating(win: &WebviewWindow) {
    use gtk::prelude::*;
    let Ok(gtk_win) = win.gtk_window() else { return };
    gtk_win.set_accept_focus(false);
    gtk_win.set_focus_on_map(false);
    gtk_win.set_skip_taskbar_hint(true);
    gtk_win.set_skip_pager_hint(true);
    gtk_win.set_keep_above(true);
}

/// The WebKitWebView inside the island window.
#[cfg(target_os = "linux")]
pub fn island_webview(win: &WebviewWindow) -> Option<gtk::Widget> {
    use gtk::prelude::*;
    fn find(w: &gtk::Widget) -> Option<gtk::Widget> {
        if w.type_().name() == "WebKitWebView" {
            return Some(w.clone());
        }
        w.downcast_ref::<gtk::Container>()?.children().iter().find_map(find)
    }
    find(win.gtk_window().ok()?.upcast_ref())
}

/// Tells the front end when the pointer leaves and re-enters the island.
///
/// The window only takes the mouse inside the island's input region, so the
/// pointer leaves it while still inside the webview's bounds. WebKit then
/// reports the exit as an ordinary move at the edge and the page never gets a
/// `mouseleave`, so the island would believe it is hovered forever.
#[cfg(target_os = "linux")]
pub fn watch_pointer_crossing(app: &AppHandle, win: &WebviewWindow) {
    use gtk::gdk::{CrossingMode, NotifyType};
    use gtk::prelude::*;
    let Some(webview) = island_webview(win) else { return };
    {
        let app = app.clone();
        webview.connect_leave_notify_event(move |_, ev| {
            if ev.mode() == CrossingMode::Normal && ev.detail() != NotifyType::Inferior {
                let _ = app.emit_to(WINDOW_LABEL, "pointer-left", ());
            }
            gtk::glib::Propagation::Proceed
        });
    }
    let app = app.clone();
    webview.connect_enter_notify_event(move |_, ev| {
        if ev.detail() != NotifyType::Inferior {
            let _ = app.emit_to(WINDOW_LABEL, "pointer-entered", ());
        }
        gtk::glib::Propagation::Proceed
    });
}

/// Temporarily accept focus so a text field inside the island can be typed in.
#[cfg(target_os = "linux")]
pub fn set_activating(win: &WebviewWindow, activating: bool) {
    use gtk::prelude::*;
    if let Ok(gtk_win) = win.gtk_window() {
        gtk_win.set_accept_focus(activating);
    }
}

/// Click-through on Linux: the window only takes the mouse inside the island
/// shape (plus the usual margin), or inside the wake strip while hidden.
///
/// Toggling the whole window like Windows does cannot work here: under Wayland
/// nobody can see the cursor over another app's window, so a fully click-through
/// island would never learn that the mouse came back. An input region needs no
/// cursor at all — the compositor does the hit test, and the webview's own
/// mouse events tell the island where the pointer is.
#[cfg(target_os = "linux")]
pub fn update_input_region(app: &AppHandle, gate: &PollGate) {
    let Some(win) = window(app) else { return };
    let collapsed = gate.collapsed.load(Ordering::Relaxed);
    let r = gate.rect();
    let _ = app.run_on_main_thread(move || {
        use gtk::cairo::{RectangleInt, Region};
        use gtk::prelude::*;
        let Ok(gtk_win) = win.gtk_window() else { return };
        // The shape is in GTK's logical pixels; the island measures itself in
        // CSS pixels, which are the same thing only when the page draws at
        // GTK's own scale factor (see linux.rs).
        let gtk_scale = gtk_win.scale_factor().max(1) as f64;
        let px = |css: f64| css * ui_scale(gtk_scale) / gtk_scale;
        let region = if crate::linux_dnd::REPICK.load(Ordering::Relaxed) {
            // A drag just arrived: step out from under it so KWin picks us again
            // (see linux_dnd.rs).
            Region::create_rectangle(&RectangleInt::new(0, 0, 1, 1))
        } else if collapsed {
            // Only the wake strip, even if the window manager kept the window
            // larger than asked: an invisible block at the top of the screen
            // swallowing clicks is the one thing this must never be.
            Region::create_rectangle(&RectangleInt::new(0, 0, px(STRIP_W).ceil() as i32, px(STRIP_H).ceil() as i32))
        } else if r.w > 0.0 {
            let x = px((r.x - HIT_MARGIN).max(0.0)).floor() as i32;
            let y = px((r.y - HIT_MARGIN).max(0.0)).floor() as i32;
            let right = px((r.x + r.w + HIT_MARGIN).min(PANEL_W)).ceil() as i32;
            let bottom = px((r.y + r.h + HIT_MARGIN).min(PANEL_H)).ceil() as i32;
            Region::create_rectangle(&RectangleInt::new(x, y, (right - x).max(1), (bottom - y).max(1)))
        } else {
            // Nothing on screen yet: take no clicks at all (GTK has no truly
            // empty input shape, so one corner pixel it is, as in tao).
            Region::create_rectangle(&RectangleInt::new(0, 0, 1, 1))
        };
        gtk_win.input_shape_combine_region(Some(&region));
    });
}

/// Watches for display changes about twice a second while the island is
/// visible, and parks on the condvar while it is hidden. There is no cursor to
/// poll on Linux (see the top of this file).
#[cfg(target_os = "linux")]
pub fn spawn_cursor_poll(app: AppHandle, gate: Arc<PollGate>) {
    std::thread::spawn(move || {
        let mut last_screen: Option<ScreenKey> = None;
        // Ticks left before acting on a layout change: XWayland's new scale can
        // land a moment after the monitors do, so settle first.
        let mut settle: u32 = 0;
        loop {
            gate.wait_until_active();
            while gate.is_active() {
                std::thread::sleep(Duration::from_millis(500));
                let now = current_screen_key(&app);
                if now.is_some() && now != last_screen {
                    let first = last_screen.is_none();
                    last_screen = now;
                    if !first {
                        crate::log::line("display layout changed, repositioning");
                        let _ = app.emit_to(WINDOW_LABEL, "screen-changed", ());
                        settle = 3;
                    }
                }
                // Once the layout has settled, a changed XWayland scale means
                // GDK_SCALE no longer fits; relaunch picks up the new one (it is
                // fixed for the life of a process). No-op on Windows.
                if settle > 0 {
                    settle -= 1;
                    if settle == 0 && relaunch_if_scale_changed() {
                        return;
                    }
                }
            }
        }
    });
}

/// Relaunches Coucou when XWayland's scale has changed, so GDK_SCALE matches
/// again; true when a relaunch was started. Only ever does anything on Linux.
fn relaunch_if_scale_changed() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::linux::relaunch_if_scale_changed()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The screen a dragged island lands on: the one its centre is over, or the
/// nearest when it was dropped past the edge of them all.
pub fn snap_target(monitors: &[(String, i32, i32, u32, u32)], cx: f64, cy: f64) -> Option<String> {
    let distance = |&(_, x, y, w, h): &(String, i32, i32, u32, u32)| {
        let dx = (cx - (x as f64 + w as f64 / 2.0)).abs() - w as f64 / 2.0;
        let dy = (cy - (y as f64 + h as f64 / 2.0)).abs() - h as f64 / 2.0;
        dx.max(0.0).hypot(dy.max(0.0))
    };
    monitors
        .iter()
        .min_by(|a, b| distance(a).partial_cmp(&distance(b)).unwrap_or(std::cmp::Ordering::Equal))
        .map(|m| m.0.clone())
}

static DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static LAST_MOVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Counts drags, so a timer left over from one never snaps the next.
static DRAG_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The user started dragging the island: the window manager moves it, and
/// when the mouse button is let go it goes to the top centre of that screen. Snapping on
/// a pause instead fired mid-drag when the hand stopped for a moment, and the
/// island then stayed wherever it was dropped.
pub fn start_drag(app: &AppHandle) {
    let Some(win) = window(app) else { return };
    let generation = DRAG_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    DRAGGING.store(true, Ordering::SeqCst);
    BUTTON_WATCH.store(false, Ordering::SeqCst);
    let _ = win.start_dragging();
    #[cfg(target_os = "linux")]
    watch_release(app, &win);
    // A drag the window manager refused never moves anything: stop waiting.
    // Only for this drag, though: a later one has its own timers.
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(20));
        if DRAG_GEN.load(Ordering::SeqCst) == generation && DRAGGING.swap(false, Ordering::SeqCst) {
            snap(&app);
        }
    });
}

/// Polls the mouse button while the window manager drags the island, and
/// snaps once it is up. When the button state cannot be read at all, the
/// window's own moves decide instead (`moved`).
#[cfg(target_os = "linux")]
fn watch_release(app: &AppHandle, win: &WebviewWindow) {
    use gtk::prelude::*;
    let app = app.clone();
    let handle = win.clone();
    let _ = win.run_on_main_thread(move || {
        let Ok(gtk_win) = handle.gtk_window() else { return };
        let mut seen_down = false;
        let mut ticks = 0u32;
        gtk::glib::timeout_add_local(Duration::from_millis(60), move || {
            ticks += 1;
            if !DRAGGING.load(Ordering::SeqCst) {
                return gtk::glib::ControlFlow::Break;
            }
            let pointer = gtk_win
                .window()
                .zip(gtk::gdk::Display::default().and_then(|d| d.default_seat()).and_then(|s| s.pointer()));
            let down = pointer.map(|(gdk_win, device)| {
                let (_, _, _, mask) = gdk_win.device_position(&device);
                mask.contains(gtk::gdk::ModifierType::BUTTON1_MASK)
            });
            match down {
                Some(true) => {
                    // The button can be read: it alone decides when the drag ends.
                    seen_down = true;
                    BUTTON_WATCH.store(true, Ordering::SeqCst);
                }
                Some(false) if seen_down => {
                    if DRAGGING.swap(false, Ordering::SeqCst) {
                        // Let the window manager finish placing it first.
                        let app = app.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(120));
                            snap(&app);
                        });
                    }
                    return gtk::glib::ControlFlow::Break;
                }
                // Never seen pressed: this display does not tell, the moves decide.
                _ if !seen_down && ticks > 16 => return gtk::glib::ControlFlow::Break,
                _ => {}
            }
            gtk::glib::ControlFlow::Continue
        });
    });
}

/// Set while the button can be read during a drag: pauses then never snap.
static BUTTON_WATCH: AtomicBool = AtomicBool::new(false);

/// Called for every move of the island window. Only decides when the mouse
/// button cannot be read (see `watch_release`), and then waits long enough
/// that a short pause mid-drag does not count as letting go.
pub fn moved(app: &AppHandle) {
    if !DRAGGING.load(Ordering::SeqCst) || BUTTON_WATCH.load(Ordering::SeqCst) {
        return;
    }
    let stamp = LAST_MOVE.fetch_add(1, Ordering::SeqCst) + 1;
    let generation = DRAG_GEN.load(Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        if LAST_MOVE.load(Ordering::SeqCst) == stamp
            && DRAG_GEN.load(Ordering::SeqCst) == generation
            && DRAGGING.swap(false, Ordering::SeqCst)
        {
            snap(&app);
        }
    });
}

fn snap(app: &AppHandle) {
    let Some(win) = window(app) else { return };
    let (Ok(pos), Ok(size)) = (win.outer_position(), win.outer_size()) else { return };
    let cx = pos.x as f64 + size.width as f64 / 2.0;
    let cy = pos.y as f64 + size.height as f64 / 2.0;
    let monitors: Vec<(String, i32, i32, u32, u32)> = app
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let (p, s) = (*m.position(), *m.size());
            (m.name().cloned().unwrap_or_default(), p.x, p.y, s.width, s.height)
        })
        .collect();
    let Some(screen) = snap_target(&monitors, cx, cy) else { return };
    crate::log::line(format!("island moved to {screen}"));
    let settings = {
        let shared = app.state::<crate::Shared>();
        let mut s = shared.settings.lock().unwrap();
        s.dock_screen = screen;
        s.clone()
    };
    let _ = crate::settings::save(&settings);
    let collapsed = app.state::<crate::Shared>().gate.collapsed.load(Ordering::Relaxed);
    apply_geometry(app, &settings.screen, collapsed);
    #[cfg(target_os = "linux")]
    update_input_region(app, &app.state::<crate::Shared>().gate);
    let _ = app.emit("settings-changed", settings);
}

#[cfg(test)]
mod dock_tests {
    use super::*;

    #[test]
    fn a_dragged_island_goes_to_the_screen_it_was_dropped_on() {
        let monitors = vec![("DP-1".to_string(), 0, 0, 3440, 1440), ("HDMI-A-2".to_string(), 3440, 0, 1920, 1080)];
        assert_eq!(snap_target(&monitors, 1720.0, 150.0).as_deref(), Some("DP-1"));
        assert_eq!(snap_target(&monitors, 300.0, 1300.0).as_deref(), Some("DP-1"));
        assert_eq!(snap_target(&monitors, 4400.0, 100.0).as_deref(), Some("HDMI-A-2"));
        // Dropped past the last screen: the nearest one.
        assert_eq!(snap_target(&monitors, 5600.0, 500.0).as_deref(), Some("HDMI-A-2"));
    }
}
