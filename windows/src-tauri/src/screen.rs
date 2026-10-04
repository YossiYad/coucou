// Seeing the user's screen, to guide them through something on it: a
// screenshot of the monitor the mouse is on (or every monitor), saved in the
// inbox like a pasted image. Taken only when the user turned screen sharing on
// in the chat, or when the model asks to look and the user allows that one
// screenshot after seeing it.

use std::path::PathBuf;

use crate::files::{self, DroppedFile};

/// One screenshot: the mouse's monitor, or all of them side by side.
pub fn capture(all_screens: bool) -> Result<DroppedFile, String> {
    let dir = files::inbox_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let (y, mo, d, h, mi, s) = crate::clock::local_now();
    let stem = format!("Screen {y:04}-{mo:02}-{d:02} {h:02}.{mi:02}.{s:02}");
    let mut dest: PathBuf = dir.join(format!("{stem}.jpg"));
    for i in 2..1000 {
        if !dest.exists() {
            break;
        }
        dest = dir.join(format!("{stem} ({i}).jpg"));
    }
    shoot(&dest, all_screens)?;
    let file = files::ingest_existing(&dest)?;
    Ok(file)
}

/// JPEG, so a wide monitor is half a megabyte rather than several.
#[cfg(target_os = "linux")]
fn shoot(dest: &std::path::Path, all_screens: bool) -> Result<(), String> {
    let out = dest.to_string_lossy().to_string();
    let scope = if all_screens { "-f" } else { "-m" };
    // KDE's Spectacle takes it silently; grim on wlroots desktops; GNOME's tool last.
    let attempts: [(&str, Vec<&str>); 3] = [
        ("spectacle", vec!["-b", "-n", scope, "-o", &out]),
        ("grim", vec!["-t", "jpeg", &out]),
        ("gnome-screenshot", vec!["-f", &out]),
    ];
    for (program, args) in attempts {
        let mut cmd = std::process::Command::new(program);
        cmd.args(&args).stdin(std::process::Stdio::null());
        crate::linux::clean_env(&mut cmd);
        let Ok(status) = cmd.status() else { continue };
        if status.success() && dest.metadata().is_ok_and(|m| m.len() > 0) {
            return Ok(());
        }
    }
    Err("Coucou could not take a screenshot (it uses Spectacle on KDE).".into())
}

#[cfg(not(target_os = "linux"))]
fn shoot(_dest: &std::path::Path, _all_screens: bool) -> Result<(), String> {
    Err("Seeing the screen is only supported on Linux for now.".into())
}
