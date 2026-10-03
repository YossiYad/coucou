// Starts the local model server only when a question needs it, and stops it
// again once it sits idle, so nothing runs in the background between uses.
//
// Both commands come from the settings (e.g. `podman start ollama` and
// `podman stop ollama`). A server Coucou did not start itself is never stopped:
// the user may be running it for something else.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::ai;
use crate::settings::Settings;

/// Stopped this long after the last question, if Coucou started it.
pub const IDLE_STOP: Duration = Duration::from_secs(10 * 60);
/// A cold start pulls the container up and, for some servers, loads a model.
const START_WAIT: Duration = Duration::from_secs(90);
const WATCH_EVERY: Duration = Duration::from_secs(30);

static STARTED_BY_US: AtomicBool = AtomicBool::new(false);
static WATCHING: AtomicBool = AtomicBool::new(false);
static LAST_USE: Mutex<Option<Instant>> = Mutex::new(None);
static STOP_COMMAND: Mutex<String> = Mutex::new(String::new());

/// Marks the server as in use, pushing the idle stop back.
pub fn touch() {
    *LAST_USE.lock().unwrap() = Some(Instant::now());
}

/// Makes sure the server answers, starting it with the configured command when
/// it does not. Without a start command an unreachable server is left to fail
/// the request with its usual message.
pub async fn ensure_running(base: &str, settings: &Settings) -> Result<(), String> {
    touch();
    if reachable(base).await {
        return Ok(());
    }
    let start = settings.local_start_command.trim();
    if start.is_empty() {
        return Ok(());
    }
    crate::log::line(format!("local model server down, running: {start}"));
    run(start).map_err(|e| format!("Could not run \"{start}\": {e}"))?;

    let deadline = Instant::now() + START_WAIT;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if reachable(base).await {
            *STOP_COMMAND.lock().unwrap() = settings.local_stop_command.trim().to_string();
            STARTED_BY_US.store(true, Ordering::SeqCst);
            touch();
            watch_idle();
            return Ok(());
        }
    }
    Err(format!(
        "Ran \"{start}\", but nothing answered at {base} within {} seconds.",
        START_WAIT.as_secs()
    ))
}

/// Stops the server now if Coucou started it (on quit, or once idle).
pub fn stop_if_ours() {
    if !STARTED_BY_US.swap(false, Ordering::SeqCst) {
        return;
    }
    let stop = STOP_COMMAND.lock().unwrap().clone();
    if stop.is_empty() {
        return;
    }
    crate::log::line(format!("stopping the local model server: {stop}"));
    if let Err(err) = run(&stop) {
        crate::log::line(format!("could not run \"{stop}\": {err}"));
    }
}

pub async fn reachable(base: &str) -> bool {
    let Ok(client) = ai::client(2) else { return false };
    client
        .get(format!("{base}/models"))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// One watcher at a time, alive only while there is a server of ours to stop.
fn watch_idle() {
    if WATCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async {
        loop {
            tokio::time::sleep(WATCH_EVERY).await;
            if !STARTED_BY_US.load(Ordering::SeqCst) {
                break;
            }
            let idle = LAST_USE.lock().unwrap().map(|t| t.elapsed()).unwrap_or(IDLE_STOP);
            if idle >= IDLE_STOP {
                stop_if_ours();
                break;
            }
        }
        WATCHING.store(false, Ordering::SeqCst);
    });
}

/// Runs a command without a shell: the program and its arguments, split on
/// whitespace. The child is reaped on a thread so it never lingers as a zombie.
fn run(command: &str) -> std::io::Result<()> {
    let (program, args) = split_command(command)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // An AppImage's bundled libraries must not leak into podman and friends.
    #[cfg(target_os = "linux")]
    crate::linux::clean_env(&mut cmd);
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn split_command(command: &str) -> Option<(&str, Vec<&str>)> {
    let mut parts = command.split_whitespace();
    let program = parts.next()?;
    Some((program, parts.collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_split_into_program_and_arguments() {
        assert_eq!(split_command("podman start ollama"), Some(("podman", vec!["start", "ollama"])));
        assert_eq!(split_command("  lms   server start "), Some(("lms", vec!["server", "start"])));
        assert_eq!(split_command("   "), None);
    }
}

