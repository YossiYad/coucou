// Coucou for Windows and Linux — app wiring and the commands the island calls.

mod agent;
mod ai;
mod claude;
mod clock;
mod docx;
mod extract;
mod fallback;
mod files;
mod frame;
mod gemini;
mod hooks;
mod integrations;
mod island;
mod local_llm;
mod local_server;
mod openai;
mod preview;
mod tools;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod linux_dnd;
mod log;
mod pipe;
mod secrets;
mod settings;
mod screen;
mod shell;
mod tray;
mod voice;
#[cfg(windows)]
mod win_user;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{ManagerExt, MacosLauncher};

use ai::{Chat, ChatContext, ChatReply, ModelInfo};
use files::DroppedFile;
use hooks::{HookPreview, HookStatus};
use island::{PollGate, ScreenInfo};
use pipe::Pending;
use settings::Settings;

/// Keeps spawned helpers from flashing a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Shared {
    pub settings: Mutex<Settings>,
    pub gate: Arc<PollGate>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootInfo {
    settings: Settings,
    screen: ScreenInfo,
    version: String,
    hook_path: String,
    /// "windows" or "linux" — the front end words a few things differently.
    platform: &'static str,
}

#[tauri::command]
fn boot(app: AppHandle, shared: State<Shared>) -> BootInfo {
    let mut settings = shared.settings.lock().unwrap().clone();
    // The real state of ~/.claude/settings.json wins over whatever we stored.
    settings.hooks_installed = hooks::status().installed;
    let screen = island::screen_info(&app, &settings.screen);
    BootInfo {
        settings,
        screen,
        version: env!("CARGO_PKG_VERSION").to_string(),
        hook_path: settings::hook_exe_path().to_string_lossy().to_string(),
        platform: std::env::consts::OS,
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, shared: State<Shared>, settings: Settings) {
    let (screen_changed, autostart_changed) = {
        let mut current = shared.settings.lock().unwrap();
        // A new edge or screen (from Settings) places the island again.
        let screen_changed = current.screen != settings.screen || current.dock_screen != settings.dock_screen;
        let autostart_changed = current.autostart != settings.autostart;
        *current = settings.clone();
        (screen_changed, autostart_changed)
    };
    if let Err(err) = settings::save(&settings) {
        eprintln!("[coucou] could not save settings: {err}");
    }
    if autostart_changed {
        let manager = app.autolaunch();
        let result = if settings.autostart { manager.enable() } else { manager.disable() };
        if let Err(err) = result {
            eprintln!("[coucou] autostart: {err}");
        }
    }
    if screen_changed {
        let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
        island::apply_geometry(&app, &settings.screen, collapsed);
        #[cfg(target_os = "linux")]
        island::update_input_region(&app, &shared.gate);
    }
    // Keep the other window in step (island ⇄ settings window).
    let _ = app.emit("settings-changed", settings);
}

/// Hidden island → shrink the window to the invisible wake strip and park the
/// cursor poll; anything else → full panel and 60 Hz polling.
#[tauri::command]
fn set_collapsed(app: AppHandle, shared: State<Shared>, collapsed: bool) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    shared.gate.collapsed.store(collapsed, Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
    // The wake strip must always take the mouse, and a resize invalidates the flag.
    #[cfg(windows)]
    {
        island::set_ignore_cursor(&app, false);
        shared.gate.forget_ignore_state();
    }
    #[cfg(target_os = "linux")]
    island::update_input_region(&app, &shared.gate);
    shared.gate.set_active(!collapsed);
}

/// The front end pushes the island shape; Rust decides click-through from it.
#[tauri::command]
fn set_island_rect(app: AppHandle, shared: State<Shared>, x: f64, y: f64, width: f64, height: f64) {
    shared.gate.set_rect(island::IslandRect { x, y, w: width, h: height });
    #[cfg(target_os = "linux")]
    island::update_input_region(&app, &shared.gate);
    #[cfg(not(target_os = "linux"))]
    let _ = app;
}

#[tauri::command]
fn focus_window(app: AppHandle, focused: bool) {
    let Some(win) = island::window(&app) else { return };
    #[cfg(target_os = "linux")]
    linux_dnd::TEXT_FOCUS.store(focused, Ordering::Relaxed);
    island::set_activating(&win, focused);
    if focused {
        let _ = win.set_focus();
    }
}

#[tauri::command]
fn reposition(app: AppHandle, shared: State<Shared>) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
}

#[tauri::command]
fn open_url(url: String) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    #[cfg(windows)]
    let _ = Command::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", &url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = linux::clean_env(&mut Command::new("xdg-open")).arg(&url).spawn();
}

/// "Open terminal" opens the working folder in VS Code when `code` is on PATH,
/// and falls back to the file manager otherwise.
#[cfg(windows)]
#[tauri::command]
fn open_in_vscode(path: Option<String>) -> bool {
    // No `cmd /C` anywhere near this. The path is a project folder chosen by
    // whoever is using Claude Code, and cmd would happily read `&`, `^` and `%`
    // in a folder name as syntax. Finding the launcher ourselves and handing the
    // path over as a separate argument keeps it a path.
    if let Some(code) = find_on_path("code") {
        let mut cmd = Command::new(code);
        if let Some(p) = path.as_deref().filter(|p| !p.is_empty()) {
            cmd.arg(p);
        }
        if cmd.creation_flags(CREATE_NO_WINDOW).spawn().is_ok() {
            return true;
        }
    }
    if let Some(p) = path.as_deref().filter(|p| !p.is_empty()) {
        let _ = Command::new("explorer").arg(p).spawn();
    }
    false
}

/// Our own `where`: walks %PATH% against %PATHEXT%, no shell involved.
/// Rust quotes arguments correctly for `.cmd`/`.bat` targets since 1.77, so
/// spawning `code.cmd` directly is safe.
#[cfg(windows)]
fn find_on_path(stem: &str) -> Option<std::path::PathBuf> {
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let dirs = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&dirs) {
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            let candidate = dir.join(format!("{stem}{}", ext.to_lowercase()));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Linux: VS Code (or VSCodium) when one is on PATH, else the folder in the
/// file manager through xdg-open. The path is always a separate argument —
/// never through a shell.
#[cfg(target_os = "linux")]
#[tauri::command]
fn open_in_vscode(path: Option<String>) -> bool {
    let path = path.filter(|p| !p.is_empty());
    for name in ["code", "codium", "code-oss"] {
        let Some(code) = linux::find_on_path(name) else { continue };
        let mut cmd = Command::new(code);
        if let Some(p) = path.as_deref() {
            cmd.arg(p);
        }
        if linux::clean_env(&mut cmd).spawn().is_ok() {
            return true;
        }
    }
    if let Some(p) = path.as_deref() {
        let _ = linux::clean_env(&mut Command::new("xdg-open")).arg(p).spawn();
    }
    false
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Tray → Pause. Paused means paused: the pollers stop talking to the network,
/// not just the island stopping showing things.
#[tauri::command]
fn set_paused(paused: bool) {
    integrations::set_paused(paused);
}

// ── Claude Code hooks ─────────────────────────────────────────────────────────

#[tauri::command]
fn hooks_status() -> HookStatus {
    hooks::status()
}

/// Returns the diff the user has to look at before anything is written.
#[tauri::command]
fn hooks_preview(install: bool) -> Result<HookPreview, String> {
    hooks::preview(install)
}

/// Only ever called from an explicit click in the settings window.
#[tauri::command]
fn hooks_apply(
    app: AppHandle,
    shared: State<Shared>,
    install: bool,
    fingerprint: String,
) -> Result<String, String> {
    // The fingerprint comes from the preview the user actually looked at, so a
    // settings.json that changed in between is refused rather than overwritten.
    let backup = hooks::write(install, &fingerprint)?;
    let updated = {
        let mut current = shared.settings.lock().unwrap();
        current.hooks_installed = install;
        let _ = settings::save(&current);
        current.clone()
    };
    let _ = app.emit("settings-changed", updated);
    Ok(backup)
}

#[tauri::command]
fn approval_decision(app: AppHandle, request_id: String, decision: String) {
    pipe::answer(&app, &request_id, &decision);
}

/// The island has the card on screen, so the long wait for a human may begin.
/// Until this arrives the relay only waits a few hundred milliseconds, which is
/// what stops a paused or unresponsive island from freezing Claude Code.
#[tauri::command]
fn approval_ack(app: AppHandle, request_id: String) {
    pipe::acknowledge(&app, &request_id);
}

/// Nobody can act on this request — the island is paused, or another card is
/// already up. Claude Code falls back to asking in the terminal immediately.
#[tauri::command]
fn approval_decline(app: AppHandle, request_id: String) {
    pipe::decline(&app, &request_id);
}

// ── Chat, files and secrets ───────────────────────────────────────────────────

/// One chat turn. The API key and any file bytes stay on the Rust side.
#[tauri::command]
async fn chat_send(
    app: AppHandle,
    shared: State<'_, Shared>,
    chat: State<'_, Chat>,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let settings = shared.settings.lock().unwrap().clone();
    ai::send(&app, &chat, &settings, query, context).await
}

/// Allow / Deny on a change the model wants to make to a file.
#[tauri::command]
fn tool_decision(app: AppHandle, id: u64, allow: bool) {
    tools::decide(&app, id, allow);
}

/// The chat's microphone button: start listening.
#[tauri::command]
fn voice_start(app: AppHandle) -> Result<(), String> {
    voice::start(&app)
}

/// Stop listening and turn what was said into text.
#[tauri::command]
async fn voice_stop(app: AppHandle) -> Result<String, String> {
    voice::stop(&app).await
}

#[tauri::command]
fn voice_cancel(app: AppHandle) {
    voice::cancel(&app);
}

/// The microphones to choose from in Settings.
#[tauri::command]
fn list_microphones() -> Vec<voice::Microphone> {
    voice::microphones()
}

/// Reads an answer out loud.
#[tauri::command]
fn speak(text: String) {
    voice::speak(&text);
}

#[tauri::command]
fn stop_speaking() {
    voice::stop_speaking();
}

/// The island is being dragged by the mouse: the window manager moves it and
/// it snaps to the nearest dock when let go.
#[tauri::command]
fn start_drag(app: AppHandle) {
    island::start_drag(&app);
}

/// The work view's Stop button on a running command.
#[tauri::command]
fn stop_command(app: AppHandle, step: u64) {
    shell::stop(&app, step);
}

/// What a Claude Code edit will do to its file, for the work view.
#[tauri::command]
fn change_preview(tool: String, input: serde_json::Value) -> Option<preview::Preview> {
    preview::for_hook(&tool, &input)
}

/// The models a provider offers, asked from the provider with the stored key.
/// `start` lets an explicit Refresh wake a local server that is off.
#[tauri::command]
async fn ai_models(
    shared: State<'_, Shared>,
    provider: String,
    start: Option<bool>,
) -> Result<Vec<ModelInfo>, String> {
    let settings = shared.settings.lock().unwrap().clone();
    ai::models(ai::Provider::parse(&provider), &settings, start.unwrap_or(false)).await
}

#[tauri::command]
fn chat_reset(chat: State<Chat>) {
    chat.reset();
}

/// Copies a dropped file into the inbox and reports its name back.
#[tauri::command]
fn ingest_file(path: String) -> Result<DroppedFile, String> {
    files::ingest(&path)
}

/// An image pasted into the chat: its bytes as the raw request body, its type
/// in the x-type header, so a screenshot does not travel as JSON numbers.
#[tauri::command]
fn ingest_pasted(request: tauri::ipc::Request<'_>) -> Result<DroppedFile, String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("No image data arrived.".into());
    };
    let kind = request.headers().get("x-type").and_then(|v| v.to_str().ok()).unwrap_or("image/png");
    files::ingest_bytes(kind, bytes)
}

/// Opens a pasted or dropped file from the inbox in its usual app, to see an
/// image full size. Paths outside the inbox are refused.
#[tauri::command]
fn open_inbox_file(path: String) -> Result<(), String> {
    let file = files::in_inbox(&path).ok_or_else(|| "Only files Coucou received can be opened.".to_string())?;
    #[cfg(target_os = "linux")]
    {
        linux::clean_env(&mut Command::new("xdg-open")).arg(&file).spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    {
        Command::new("explorer.exe").arg(&file).creation_flags(CREATE_NO_WINDOW).spawn().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A screenshot for the chat, from the screen button: saved in the inbox and
/// attached to the question like a pasted image.
#[tauri::command]
async fn capture_screen(app: AppHandle, all: bool) -> Result<DroppedFile, String> {
    let shot = tauri::async_runtime::spawn_blocking(move || screen::capture(all)).await.map_err(|e| e.to_string())??;
    frame::flash(&app, all);
    Ok(shot)
}

/// The image on the system clipboard, when the web view did not pass it on.
#[tauri::command]
fn paste_clipboard_image() -> Result<DroppedFile, String> {
    files::clipboard_image()
}

/// The island may only ask whether a key exists — never read it.
#[tauri::command]
fn secret_present(key: String) -> bool {
    secrets::present(&key)
}

#[tauri::command]
fn secret_set(key: String, value: String) -> Result<(), String> {
    secrets::set(&key, &value)
}

#[tauri::command]
fn secret_clear(key: String) -> Result<(), String> {
    secrets::clear(&key)
}

/// Opens the configured n8n instance — the URL lives in the key store.
#[tauri::command]
fn open_n8n() {
    if let Some(url) = secrets::get("n8n-url") {
        open_url(url);
    }
}

/// Refresh buttons in the integration cards.
#[tauri::command]
async fn refresh_integration(app: AppHandle, id: String) {
    integrations::poll_once(app, &id).await;
}

/// Lets the island write to the same log as the Rust side.
#[tauri::command]
fn log_line(message: String) {
    log::line(format!("ui  {message}"));
}

// ── Settings window ───────────────────────────────────────────────────────────

/// WebView2 allows exactly one browser environment per app, and its options are
/// fixed by whichever webview is created first. Every window must therefore ask
/// for the *same* arguments as the island (see `additionalBrowserArgs` in
/// tauri.conf.json) — a mismatch makes the second window come up blank, with no
/// error anywhere.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required";

/// In a dev build the pages are served by Vite, so the second window needs the
/// absolute dev URL; a bundled build resolves it inside the app bundle.
fn settings_page_url(app: &AppHandle) -> WebviewUrl {
    #[cfg(dev)]
    if let Some(mut base) = app.config().build.dev_url.clone() {
        base.set_path("/settings.html");
        return WebviewUrl::External(base);
    }
    let _ = app;
    WebviewUrl::App("settings.html".into())
}

/// The settings window is created hidden at launch and only ever shown and
/// hidden afterwards. A WebView2 window created later — on the main thread or
/// not — silently comes up blank in this app, so the window that works is the
/// one that exists before the island's webview does.
fn create_settings_window(app: &AppHandle) {
    let url = settings_page_url(app);
    match WebviewWindowBuilder::new(app, "settings", url)
        .additional_browser_args(BROWSER_ARGS)
        .title("Settings — Coucou")
        .inner_size(560.0, 680.0)
        .min_inner_size(460.0, 480.0)
        .resizable(true)
        .visible(false)
        .center()
        .build()
    {
        Ok(win) => {
            // Closing it must only hide it, or it could never be reopened.
            let hidden = win.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = hidden.hide();
                }
            });
        }
        Err(err) => log::line(format!("settings window failed: {err}")),
    }
}

pub fn show_settings_window(app: &AppHandle) {
    let Some(win) = app.get_webview_window("settings") else {
        log::line("settings window missing");
        return;
    };
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();
}

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    show_settings_window(&app);
}

pub fn run() {
    #[cfg(target_os = "linux")]
    linux::prepare_env();

    let mut loaded = settings::load();
    // Every start begins on the main screen unless the user chose otherwise;
    // the edge it was docked to is kept.
    if loaded.start_on_main_screen && !loaded.dock_screen.is_empty() {
        loaded.dock_screen.clear();
        let _ = settings::save(&loaded);
    }
    let gate = Arc::new(PollGate::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let _ = app.emit_to(island::WINDOW_LABEL, "tray", "open".to_string());
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .manage(Shared {
            settings: Mutex::new(loaded.clone()),
            gate: gate.clone(),
        })
        .manage(Pending::default())
        .manage(Chat::default())
        .manage(tools::Approvals::default())
        .manage(shell::Running::default())
        .manage(voice::Recorder::default())
        .invoke_handler(tauri::generate_handler![
            boot,
            change_preview,
            stop_command,
            start_drag,
            voice_start,
            voice_stop,
            voice_cancel,
            speak,
            stop_speaking,
            list_microphones,
            ingest_pasted,
            paste_clipboard_image,
            open_inbox_file,
            capture_screen,
            save_settings,
            set_collapsed,
            set_island_rect,
            focus_window,
            reposition,
            open_url,
            open_in_vscode,
            quit_app,
            hooks_status,
            hooks_preview,
            hooks_apply,
            approval_decision,
            approval_ack,
            approval_decline,
            log_line,
            chat_send,
            chat_reset,
            ai_models,
            tool_decision,
            ingest_file,
            secret_present,
            secret_set,
            secret_clear,
            refresh_integration,
            open_n8n,
            open_settings_window,
            set_paused,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            #[cfg(target_os = "linux")]
            let tray_ok = linux::tray_available();
            #[cfg(not(target_os = "linux"))]
            let tray_ok = true;
            if tray_ok {
                tray::build(&handle)?;
            } else {
                log::line("no appindicator library — running without a tray icon");
            }
            // Before the island: see create_settings_window.
            create_settings_window(&handle);
            frame::create(&handle);

            if let Some(win) = island::window(&handle) {
                island::make_non_activating(&win);
                #[cfg(target_os = "linux")]
                {
                    linux_dnd::attach(&handle, &win, gate.clone());
                    island::watch_pointer_crossing(&handle, &win);
                }
                island::apply_geometry(&handle, &loaded.screen, false);
                let moved = handle.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::Moved(_) = event {
                        island::moved(&moved);
                    }
                });
                let _ = win.show();
            }
            gate.collapsed.store(false, Ordering::Relaxed);
            gate.set_active(true);
            island::spawn_cursor_poll(handle.clone(), gate.clone());

            log::line(format!("--- Coucou {} started ---", env!("CARGO_PKG_VERSION")));
            hooks::ensure_hook_exe(&handle);
            pipe::start(handle.clone());
            integrations::start(handle.clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Coucou");
}
