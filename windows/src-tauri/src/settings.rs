// Preferences, stored as plain JSON in %APPDATA%\Coucou\settings.json on Windows
// and ~/.config/coucou/settings.json on Linux. No secret ever lands here — API
// keys live in the Windows Credential Manager or the Linux Secret Service.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub sound_enabled: bool,
    pub sound_volume: f64,
    pub auto_close_interval: f64,
    pub absence_interval: f64,
    pub active_integrations: Vec<String>,
    /// "primary" = the main display, "cursor" = whichever display the mouse is on.
    pub screen: String,
    pub autostart: bool,
    pub hooks_installed: bool,
    /// Claude model used by the chat. Changeable in the settings window.
    /// Defaulted explicitly so a settings.json written by an older build still loads.
    #[serde(default = "default_model")]
    pub model: String,
    /// Who answers the chat: "anthropic", "openai", "gemini" or "local".
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Models for the other providers, picked from the provider's own list.
    #[serde(default)]
    pub openai_model: String,
    #[serde(default)]
    pub gemini_model: String,
    #[serde(default)]
    pub local_model: String,
    /// Address of the local OpenAI-compatible server (Ollama, LM Studio...).
    #[serde(default = "default_local_url")]
    pub local_url: String,
    /// Starts the local server when a question finds it down, e.g.
    /// `podman start ollama`. Empty: Coucou never starts anything.
    #[serde(default)]
    pub local_start_command: String,
    /// Stops it again once idle, e.g. `podman stop ollama`.
    #[serde(default)]
    pub local_stop_command: String,
    /// Start a fresh conversation every time the island hides, instead of
    /// picking up where it left off.
    #[serde(default)]
    pub clear_chat_on_hide: bool,
    /// Lets the chat model act: search, read and change files, browse the web.
    #[serde(default)]
    pub tools_enabled: bool,
    /// Who approves the chat model's changes, like Claude Code's modes:
    /// "manual" asks every time, "auto" asks only for drastic changes,
    /// "acceptEdits" never asks, "plan" changes nothing and proposes instead.
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    /// When the chosen model cannot answer (quota, overload, network, no
    /// key), ask the next one this computer can use.
    #[serde(default = "default_true")]
    pub ai_fallback: bool,
    /// Screen sharing, from the chat's screen button: every question carries
    /// a screenshot, and the model may look at the screen without asking.
    #[serde(default)]
    pub screen_sharing: bool,
    /// "mouse" (the monitor the mouse is on) or "all".
    #[serde(default = "default_screen_scope")]
    pub screen_scope: String,
    /// Where the island sits: "top" (centre of the top edge), "left" or
    /// "right" (middle of that edge). Set by dragging it.
    #[serde(default = "default_dock")]
    pub dock: String,
    /// The monitor it was dragged to, by name; empty follows `screen`.
    #[serde(default)]
    pub dock_screen: String,
    /// On every start the island comes up on the main screen, wherever it was
    /// dragged before; off, it returns to the screen it was left on.
    #[serde(default = "default_true")]
    pub start_on_main_screen: bool,
    /// Read the answer to a spoken question out loud.
    #[serde(default = "default_true")]
    pub speak_answers: bool,
    /// The microphone to listen with (a PipeWire source name); empty is the
    /// system default.
    #[serde(default)]
    pub microphone: String,
}

fn default_dock() -> String {
    "top".into()
}

fn default_screen_scope() -> String {
    "mouse".into()
}

fn default_true() -> bool {
    true
}

fn default_permission_mode() -> String {
    "manual".into()
}

fn default_model() -> String {
    crate::claude::DEFAULT_MODEL.to_string()
}

fn default_provider() -> String {
    "anthropic".into()
}

fn default_local_url() -> String {
    crate::local_llm::DEFAULT_URL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sound_enabled: true,
            sound_volume: 0.12,
            auto_close_interval: 15.0,
            absence_interval: 180.0,
            active_integrations: vec![
                "integration_resend".into(),
                "integration_n8n".into(),
                "integration_vercel".into(),
                "integration_github".into(),
            ],
            screen: "primary".into(),
            autostart: false,
            hooks_installed: false,
            model: default_model(),
            provider: default_provider(),
            openai_model: String::new(),
            gemini_model: String::new(),
            local_model: String::new(),
            local_url: default_local_url(),
            local_start_command: String::new(),
            local_stop_command: String::new(),
            clear_chat_on_hide: false,
            tools_enabled: false,
            permission_mode: default_permission_mode(),
            ai_fallback: true,
            screen_sharing: false,
            screen_scope: default_screen_scope(),
            dock: default_dock(),
            dock_screen: String::new(),
            start_on_main_screen: true,
            speak_answers: true,
            microphone: String::new(),
        }
    }
}

/// %APPDATA%\Coucou on Windows.
#[cfg(windows)]
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Coucou")
}

/// %LOCALAPPDATA%\Coucou — where coucou-hook.exe and the log live.
#[cfg(windows)]
pub fn local_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Coucou")
}

/// $XDG_CONFIG_HOME/coucou, usually ~/.config/coucou.
#[cfg(not(windows))]
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("coucou")
}

/// $XDG_DATA_HOME/coucou, usually ~/.local/share/coucou — where coucou-hook,
/// the inbox and the log live.
#[cfg(not(windows))]
pub fn local_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").join("coucou")
}

/// An XDG base directory: the variable when it holds an absolute path (the spec
/// says relative ones must be ignored), else its default under $HOME.
#[cfg(not(windows))]
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
        return dir;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(fallback)
}

/// The relay's file name, as bundled and as installed.
#[cfg(windows)]
pub const HOOK_FILE_NAME: &str = "coucou-hook.exe";
#[cfg(not(windows))]
pub const HOOK_FILE_NAME: &str = "coucou-hook";

pub fn hook_exe_path() -> PathBuf {
    local_dir().join("bin").join(HOOK_FILE_NAME)
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn load() -> Settings {
    match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_vec_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(settings_path(), json)
}
