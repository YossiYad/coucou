// Linux specifics: the display backend, the environment handed to programs we
// launch, and whether a tray icon is possible at all.
//
// Coucou needs three things Wayland deliberately keeps from ordinary windows:
// placing itself at the top centre of the screen, staying above other windows,
// and staying out of the taskbar. XWayland still allows all three, and every
// Wayland desktop Bazzite ships (KDE Plasma, GNOME) runs it, so on a Wayland
// session we ask GTK for its X11 backend. `COUCOU_NATIVE_WAYLAND=1` opts out,
// and a GDK_BACKEND set by the user always wins.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Variables we set for ourselves and must not leak into launched programs —
/// a browser forced onto XWayland would be blurry for no reason.
static OWN_VARS: OnceLock<Vec<&'static str>> = OnceLock::new();

/// Must run before Tauri (and so GTK) starts, while the process is still
/// single-threaded.
pub fn prepare_env() {
    let mut own = Vec::new();
    let unset = |name: &str| std::env::var_os(name).map(|v| v.is_empty()).unwrap_or(true);

    let wayland = !unset("WAYLAND_DISPLAY");
    let xwayland = !unset("DISPLAY");
    let use_x11 = wayland && xwayland && unset("GDK_BACKEND") && unset("COUCOU_NATIVE_WAYLAND");
    if use_x11 {
        std::env::set_var("GDK_BACKEND", "x11");
        own.push("GDK_BACKEND");

        // XWayland runs the whole X server at a single scale: the largest
        // monitor's (200% once a 4K TV is attached). It never passes that to
        // GTK, so GTK draws at scale 1 and KWin then shrinks every window to a
        // fraction of its size on any less-scaled screen (the island came out
        // tiny). KDE records the scale in Xft.dpi (96 per 100%); matching
        // GDK_SCALE to it makes GTK draw natively at the right size, with a
        // normal cursor and no page zoom. Integer only, which is what X uses.
        if unset("GDK_SCALE") {
            if let Some(scale) = xwayland_scale() {
                if scale > 1 {
                    std::env::set_var("GDK_SCALE", scale.to_string());
                    own.push("GDK_SCALE");
                }
            }
        }
    }

    // WebKitGTK's DMA-BUF renderer draws blank or garbled windows on the NVIDIA
    // driver, which Bazzite ships on its -nvidia images.
    let nvidia = std::path::Path::new("/sys/module/nvidia").exists();
    if nvidia && unset("WEBKIT_DISABLE_DMABUF_RENDERER") {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        own.push("WEBKIT_DISABLE_DMABUF_RENDERER");
    }

    let _ = OWN_VARS.set(own);
}

/// Strips what we set for ourselves, and what an AppImage's launcher sets for
/// its bundled libraries, from a program we are about to start.
pub fn clean_env(cmd: &mut Command) -> &mut Command {
    for var in OWN_VARS.get().map(Vec::as_slice).unwrap_or(&[]) {
        cmd.env_remove(var);
    }
    if std::env::var_os("APPIMAGE").is_some() {
        for var in [
            "LD_LIBRARY_PATH",
            "GDK_PIXBUF_MODULE_FILE",
            "GIO_MODULE_DIR",
            "GIO_EXTRA_MODULES",
            "GSETTINGS_SCHEMA_DIR",
            "GST_PLUGIN_SYSTEM_PATH",
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            "GTK_PATH",
            "GTK_EXE_PREFIX",
            "GTK_DATA_PREFIX",
            "PYTHONHOME",
            "PYTHONPATH",
        ] {
            cmd.env_remove(var);
        }
    }
    cmd
}

/// XWayland's scale, read from Xft.dpi (96 dpi is 100%) and rounded to a whole
/// number, since GDK_SCALE takes only integers. `None` if xrdb cannot be read
/// or carries no Xft.dpi, in which case GTK's own value (scale 1) stands.
pub fn xwayland_scale() -> Option<i32> {
    let mut cmd = Command::new("xrdb");
    cmd.arg("-query").stdin(Stdio::null()).stderr(Stdio::null());
    let out = cmd.output().ok()?;
    scale_from_resources(&String::from_utf8_lossy(&out.stdout))
}

/// The scale an `xrdb -query` dump implies: Xft.dpi over the standard 96 dpi,
/// rounded, at least 1.
fn scale_from_resources(resources: &str) -> Option<i32> {
    let dpi: f64 = resources
        .lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:"))?
        .trim()
        .parse()
        .ok()?;
    Some((dpi / 96.0).round().max(1.0) as i32)
}

/// XWayland's scale jumps when monitors are plugged in, unplugged or rescaled
/// (the X server follows the largest screen). GDK_SCALE is fixed for the life of
/// a process, so once it no longer matches, every window is the wrong size until
/// a restart. When that happens, relaunch ourselves so the fresh process reads
/// the new scale. `true` when a relaunch was started. Does nothing unless we
/// set GDK_SCALE ourselves in the first place.
pub fn relaunch_if_scale_changed() -> bool {
    if !OWN_VARS.get().map(|v| v.contains(&"GDK_SCALE")).unwrap_or(false) {
        return false;
    }
    let want = match xwayland_scale() {
        Some(s) => s,
        None => return false,
    };
    let have: i32 = std::env::var("GDK_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    if want == have {
        return false;
    }
    let exe = match std::env::var_os("APPIMAGE").map(std::path::PathBuf::from).or_else(|| std::env::current_exe().ok()) {
        Some(p) => p,
        None => return false,
    };
    crate::log::line(format!("screen scale changed {have} -> {want}, restarting to match"));
    // Hand the new process a clean slate so it works the scale out from scratch
    // and the AppImage runtime sets its own library paths.
    let mut cmd = Command::new("setsid");
    cmd.arg("-f").arg(&exe).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for var in ["GDK_SCALE", "GDK_BACKEND", "APPDIR", "APPIMAGE", "APPRUN", "OWD", "ARGV0"] {
        cmd.env_remove(var);
    }
    clean_env(&mut cmd);
    if cmd.spawn().is_err() {
        crate::log::line("restart for new scale failed; staying up at the old one");
        return false;
    }
    // Give the replacement a moment to come up before this one goes.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(600));
        std::process::exit(0);
    });
    true
}

/// The tray icon goes through libayatana-appindicator (or the older
/// libappindicator), loaded at run time — and a missing library is a panic deep
/// inside the tray crate, not an error. So look first, and run without a tray
/// rather than not at all: the island has its own settings view.
pub fn tray_available() -> bool {
    const LIBS: &[&str] = &[
        "libayatana-appindicator3.so.1\0",
        "libayatana-appindicator3.so\0",
        "libappindicator3.so.1\0",
        "libappindicator3.so\0",
    ];
    LIBS.iter().any(|name| unsafe {
        let handle = libc::dlopen(name.as_ptr().cast(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if handle.is_null() {
            false
        } else {
            libc::dlclose(handle);
            true
        }
    })
}

/// `which`, without a shell.
pub fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dirs = std::env::var_os("PATH")?;
    std::env::split_paths(&dirs)
        .map(|dir| dir.join(name))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::scale_from_resources;

    #[test]
    fn the_scale_comes_from_xft_dpi() {
        // KDE with a 200% screen: XWayland doubled, Xft.dpi 192.
        assert_eq!(scale_from_resources("Xcursor.size:\t24\nXft.dpi:\t192\n"), Some(2));
        // 100%; a laptop at 125% or 150% rounds to the nearest whole scale
        // (150% goes up, so it stays sharp and KWin shrinks it a little).
        assert_eq!(scale_from_resources("Xft.dpi:\t96\n"), Some(1));
        assert_eq!(scale_from_resources("Xft.dpi:\t120\n"), Some(1));
        assert_eq!(scale_from_resources("Xft.dpi:\t144\n"), Some(2));
        assert_eq!(scale_from_resources("Xft.dpi:\t168\n"), Some(2));
        // Nothing to read: leave GTK's own value alone.
        assert_eq!(scale_from_resources(""), None);
        assert_eq!(scale_from_resources("Xcursor.size:\t24\n"), None);
    }
}
