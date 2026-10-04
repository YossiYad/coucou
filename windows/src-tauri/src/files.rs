// Dropped files are copied into the inbox (%LOCALAPPDATA%\Coucou\inbox on
// Windows, ~/.local/share/coucou/inbox on Linux) so the original is
// never touched and the copy survives the drag source going away.
// The inbox is swept of anything older than a week, as on macOS.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::settings;

const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DroppedFile {
    pub name: String,
    pub path: String,
    pub size: u64,
}

pub fn inbox_dir() -> PathBuf {
    settings::local_dir().join("inbox")
}

pub fn ingest(source: &str) -> Result<DroppedFile, String> {
    let src = Path::new(source);
    let meta = std::fs::metadata(src).map_err(|e| format!("cannot read {source}: {e}"))?;
    if meta.is_dir() {
        return Err("Folders can't be dropped yet.".into());
    }

    let dir = inbox_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());

    let mut dest = dir.join(&name);
    if dest.exists() {
        let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ext = src.extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
        for i in 2..1000 {
            let candidate = dir.join(format!("{stem} ({i}){ext}"));
            if !candidate.exists() {
                dest = candidate;
                break;
            }
        }
    }

    std::fs::copy(src, &dest).map_err(|e| format!("cannot copy: {e}"))?;
    // CopyFileEx carries the source's timestamps across, so a file last edited
    // three years ago would arrive already older than the sweep window and be
    // deleted on the spot. The inbox ages from when *we* copied it.
    if let Ok(file) = std::fs::File::options().write(true).open(&dest) {
        let _ = file.set_modified(SystemTime::now());
    }
    sweep(&dir);

    Ok(DroppedFile {
        name,
        path: dest.to_string_lossy().to_string(),
        size: meta.len(),
    })
}

/// Largest pasted image taken: a full-screen screenshot is a few MB.
const MAX_PASTE: usize = 25 * 1024 * 1024;

/// An image pasted into the chat, saved in the inbox like a dropped file.
pub fn ingest_bytes(kind: &str, bytes: &[u8]) -> Result<DroppedFile, String> {
    if bytes.is_empty() {
        return Err("The clipboard image is empty.".into());
    }
    if bytes.len() > MAX_PASTE {
        return Err("That image is too big to paste (over 25 MB).".into());
    }
    let ext = match kind.trim().to_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        other => return Err(format!("{other} can't be pasted as an image.")),
    };
    let dir = inbox_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let (y, mo, d, h, mi, s) = crate::clock::local_now();
    let stem = format!("Pasted image {y:04}-{mo:02}-{d:02} {h:02}.{mi:02}.{s:02}");
    let mut dest = dir.join(format!("{stem}.{ext}"));
    for i in 2..1000 {
        if !dest.exists() {
            break;
        }
        dest = dir.join(format!("{stem} ({i}).{ext}"));
    }
    std::fs::write(&dest, bytes).map_err(|e| format!("cannot save the image: {e}"))?;
    sweep(&dir);
    Ok(DroppedFile {
        name: dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        path: dest.to_string_lossy().to_string(),
        size: bytes.len() as u64,
    })
}

/// The image on the system clipboard, for when the web view does not hand it
/// over itself: wl-paste on Wayland.
#[cfg(target_os = "linux")]
pub fn clipboard_image() -> Result<DroppedFile, String> {
    let run = |args: &[&str]| -> Result<Vec<u8>, String> {
        let mut cmd = std::process::Command::new("wl-paste");
        cmd.args(args);
        crate::linux::clean_env(&mut cmd);
        let out = cmd.output().map_err(|_| "Pasting images needs wl-paste (wl-clipboard).".to_string())?;
        if !out.status.success() {
            return Err("There is no image on the clipboard.".into());
        }
        Ok(out.stdout)
    };
    let types = String::from_utf8_lossy(&run(&["--list-types"])?).to_string();
    let kind = ["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp"]
        .into_iter()
        .find(|t| types.lines().any(|l| l.trim() == *t))
        .ok_or_else(|| "There is no image on the clipboard.".to_string())?;
    ingest_bytes(kind, &run(&["--no-newline", "--type", kind])?)
}

#[cfg(not(target_os = "linux"))]
pub fn clipboard_image() -> Result<DroppedFile, String> {
    Err("Paste the image straight into the chat box.".into())
}

/// Drops anything copied here more than a week ago. `ingest` stamps every copy
/// with the time it landed, so this really is the age of the copy and not the
/// age of whatever the user happened to drag in.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(copied) = meta.modified() else { continue };
        if now.duration_since(copied).map(|age| age > KEEP_FOR).unwrap_or(false) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_images_are_saved_with_their_type_and_unknown_types_refused() {
        let png = ingest_bytes("image/png", b"\x89PNG fake").unwrap();
        assert!(png.name.starts_with("Pasted image ") && png.name.ends_with(".png"), "{}", png.name);
        assert_eq!(std::fs::read(&png.path).unwrap(), b"\x89PNG fake");
        let again = ingest_bytes("image/png", b"second").unwrap();
        assert_ne!(png.path, again.path, "a second paste in the same second must not overwrite the first");
        assert!(ingest_bytes("text/html", b"<b>x</b>").is_err());
        assert!(ingest_bytes("image/png", b"").is_err());
        let _ = std::fs::remove_file(&png.path);
        let _ = std::fs::remove_file(&again.path);
    }

    #[test]
    fn ingest_copies_and_never_overwrites() {
        let tmp = std::env::temp_dir().join(format!("coucou-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let source = tmp.join("note.txt");
        std::fs::write(&source, b"hello").unwrap();

        let first = ingest(source.to_str().unwrap()).unwrap();
        assert_eq!(first.name, "note.txt");
        assert_eq!(std::fs::read(&first.path).unwrap(), b"hello");

        // A second drop of the same name must not clobber the first copy.
        std::fs::write(&source, b"second").unwrap();
        let second = ingest(source.to_str().unwrap()).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(std::fs::read(&first.path).unwrap(), b"hello");
        assert_eq!(std::fs::read(&second.path).unwrap(), b"second");

        // Folders are refused rather than silently ignored.
        assert!(ingest(tmp.to_str().unwrap()).is_err());

        // An ancient source must not arrive already older than the sweep window.
        let old_source = tmp.join("ancient.txt");
        std::fs::write(&old_source, b"old").unwrap();
        let long_ago = SystemTime::now() - KEEP_FOR - Duration::from_secs(60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&old_source)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let aged = ingest(old_source.to_str().unwrap()).unwrap();
        assert!(
            Path::new(&aged.path).exists(),
            "a file copied just now was swept as if it were a week old"
        );
        let _ = std::fs::remove_file(&aged.path);

        let _ = std::fs::remove_file(&first.path);
        let _ = std::fs::remove_file(&second.path);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
