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
    /// Lets a local model that supports it act: search, read and create files,
    /// browse the web. Every write still waits for a click.
    #[serde(default)]
    pub tools_enabled: bool,
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
