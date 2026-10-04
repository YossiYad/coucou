// Talking to Coucou: the microphone records while the chat's mic button is
// on (PipeWire's pw-record), the recording becomes text through Gemini (its
// light model, on each of the user's accounts in turn), and an answer to a
// spoken question can be read out loud (speech-dispatcher).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use crate::{fallback, files, gemini, secrets};

/// A question is not a speech: recording stops by itself after this.
const MAX_RECORDING: Duration = Duration::from_secs(120);
/// Below this a WAV holds a click, not words (16 kHz mono 16-bit: 0.3 s).
const MIN_BYTES: u64 = 10_000;

#[derive(serde::Serialize)]
pub struct Microphone {
    pub id: String,
    pub label: String,
}

/// The microphones PipeWire knows, for the setting (monitors of outputs left out).
pub fn microphones() -> Vec<Microphone> {
    let mut cmd = Command::new("pactl");
    cmd.args(["--format=json", "list", "sources"]).stdin(Stdio::null());
    #[cfg(target_os = "linux")]
    crate::linux::clean_env(&mut cmd);
    let Ok(out) = cmd.output() else { return Vec::new() };
    let list: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    list.iter()
        .filter_map(|s| {
            let id = s.get("name")?.as_str()?.to_string();
            if id.ends_with(".monitor") || s.get("monitor_source").and_then(|m| m.as_str()).is_some_and(|m| !m.is_empty()) {
                return None;
            }
            let label = s.get("description").and_then(|d| d.as_str()).unwrap_or(&id).to_string();
            Some(Microphone { id, label })
        })
        .collect()
}

#[derive(Default)]
pub struct Recorder {
    current: Mutex<Option<(Child, PathBuf, Instant)>>,
}

pub fn start(app: &AppHandle) -> Result<(), String> {
    let recorder = app.state::<Recorder>();
    let mut current = recorder.current.lock().unwrap();
    if current.is_some() {
        return Ok(());
    }
    let dir = files::inbox_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let (y, mo, d, h, mi, s) = crate::clock::local_now();
    let path = dir.join(format!("Voice {y:04}-{mo:02}-{d:02} {h:02}.{mi:02}.{s:02}.wav"));
    let microphone = app.state::<crate::Shared>().settings.lock().unwrap().microphone.clone();
    let mut cmd = Command::new("pw-record");
    if !microphone.is_empty() {
        cmd.args(["--target", &microphone]);
    }
    cmd.args(["--rate", "16000", "--channels", "1", "--format", "s16"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    crate::linux::clean_env(&mut cmd);
    let child = cmd.spawn().map_err(|_| "Talking needs PipeWire's pw-record, which is missing.".to_string())?;
    crate::log::line("voice: recording");
    *current = Some((child, path, Instant::now()));
    drop(current);
    // A forgotten microphone is switched off.
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(MAX_RECORDING);
        let recorder = app.state::<Recorder>();
        let still = recorder.current.lock().unwrap().as_ref().is_some_and(|(_, _, at)| at.elapsed() >= MAX_RECORDING);
        if still {
            let _ = finish(&app);
        }
    });
    Ok(())
}

/// Stops the microphone and returns the recording.
fn finish(app: &AppHandle) -> Result<PathBuf, String> {
    let recorder = app.state::<Recorder>();
    let (mut child, path, _) = recorder.current.lock().unwrap().take().ok_or("Nothing is being recorded.")?;
    // SIGINT lets pw-record write the WAV header; killing it outright would not.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    crate::log::line("voice: stopped");
    Ok(path)
}

/// Throws the recording away (Escape while listening).
pub fn cancel(app: &AppHandle) {
    if let Ok(path) = finish(app) {
        let _ = std::fs::remove_file(path);
    }
}

/// Stops the microphone and turns what was said into text.
pub async fn stop(app: &AppHandle) -> Result<String, String> {
    let path = {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || finish(&app)).await.map_err(|e| e.to_string())??
    };
    let audio = std::fs::read(&path).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&path);
    if (audio.len() as u64) < MIN_BYTES {
        return Err("I didn't hear anything. Click the microphone and speak, then click it again.".into());
    }
    let model = {
        let shared = app.state::<crate::Shared>();
        let s = shared.settings.lock().unwrap();
        s.gemini_model.clone()
    };
    transcribe(&audio, &model).await
}

/// Gemini hears the recording: its light model first (the bigger free
/// quota), on each account with a key, then the chosen Gemini model.
async fn transcribe(audio: &[u8], chosen: &str) -> Result<String, String> {
    let accounts = secrets::accounts("gemini-api-key");
    if accounts.is_empty() {
        return Err("Talking needs a Gemini API key: Gemini turns your voice into text. Add one in Settings.".into());
    }
    let mut models = vec![fallback::GEMINI_LIGHT.to_string()];
    if !chosen.trim().is_empty() && chosen != fallback::GEMINI_LIGHT {
        models.push(chosen.to_string());
    }
    let mut last = String::new();
    for model in &models {
        for &account in &accounts {
            match secrets::on_account(account, gemini::transcribe(model, audio)).await {
                Ok(text) => {
                    crate::log::line(format!("voice: transcribed ({} chars)", text.chars().count()));
                    return Ok(text);
                }
                Err(e) => {
                    crate::log::line(format!("voice: transcription failed on {model} (account {account}): {e}"));
                    last = e;
                }
            }
        }
    }
    Err(last)
}

/// Reads an answer out loud in its language (Hebrew or English voices).
pub fn speak(text: &str) {
    stop_speaking();
    let hebrew = text.chars().filter(|c| ('\u{0590}'..='\u{05FF}').contains(c)).count();
    let latin = text.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let lang = if hebrew > latin { "he" } else { "en" };
    // Spoken, a long answer is a lecture: the first part, the rest is on screen.
    let spoken: String = text.chars().take(1_200).collect();
    let mut cmd = Command::new("spd-say");
    cmd.args(["-l", lang, "-r", "10", "--"]).arg(spoken).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    crate::linux::clean_env(&mut cmd);
    if cmd.spawn().is_err() {
        crate::log::line("voice: spd-say is missing, answers can't be read out loud");
    }
}

pub fn stop_speaking() {
    let mut cmd = Command::new("spd-say");
    cmd.arg("-C").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    crate::linux::clean_env(&mut cmd);
    let _ = cmd.status();
}
