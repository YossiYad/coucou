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

/// X pixels per CSS pixel of our pages: the scale WebKitGTK draws at once
/// `prepare_env` has had its say (see there). Unset when the X server's scale
/// is unknown, and GTK's own scale factor is then the best guess.
static UI_SCALE: OnceLock<f64> = OnceLock::new();

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// Must run before Tauri (and so GTK) starts, while the process is still
/// single-threaded.
pub fn prepare_env() {
    wait_for_predecessor();
    let mut own = Vec::new();
    let unset = |name: &str| std::env::var_os(name).map(|v| v.is_empty()).unwrap_or(true);

    let wayland = !unset("WAYLAND_DISPLAY");
    let xwayland = !unset("DISPLAY");
    let use_x11 = wayland && xwayland && unset("GDK_BACKEND") && unset("COUCOU_NATIVE_WAYLAND");
    if use_x11 {
        std::env::set_var("GDK_BACKEND", "x11");
        own.push("GDK_BACKEND");

        // XWayland runs the whole X server at a single scale, the largest
        // monitor's (200% once a 4K TV is attached), and KWin then shows every
        // X window on each screen at that screen's own size. KDE records the
        // scale in Xft.dpi (96 per 100%). Draw at exactly that scale and the
        // island comes out right on every monitor at once: sharp on the
        // largest, shrunk by KWin on the others, whatever is plugged in.
        //
        // A WebKitGTK page draws at GTK's whole-number scale factor times the
        // font resolution over 96 dpi. Left alone, GTK reads Xft.dpi 192 as
        // scale 2 at 96 dpi. A GDK_SCALE of 2 on its own pins the scale at 2
        // but leaves the 192 dpi, and the page then draws at 4: the island
        // overflowed its window on both sides. So the two are set together:
        // GDK_SCALE to the nearest whole scale, GDK_DPI_SCALE to cancel that
        // scale's share of the dpi, and the page lands on the X server's scale
        // exactly, even a fractional 135%. Either variable set by the user
        // stands, and the scale is worked out from what it leaves.
        if let Some(xscale) = xwayland_scale() {
            let (gdk, dpi_scale) = gdk_split(xscale);
            let gdk = match env_f64("GDK_SCALE") {
                Some(g) if g >= 1.0 => g,
                _ => {
                    std::env::set_var("GDK_SCALE", gdk.to_string());
                    own.push("GDK_SCALE");
                    gdk as f64
                }
            };
            let dpi_scale = match env_f64("GDK_DPI_SCALE") {
                Some(d) if d > 0.0 => d,
                _ => {
                    std::env::set_var("GDK_DPI_SCALE", &dpi_scale);
                    own.push("GDK_DPI_SCALE");
                    dpi_scale.parse().unwrap_or(1.0 / gdk)
                }
            };
            let _ = UI_SCALE.set(gdk * dpi_scale * xscale);
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

/// XWayland's scale, read from Xft.dpi (96 dpi is 100%), fractional when the
/// largest screen is. `None` if xrdb cannot be read or carries no Xft.dpi, in
/// which case GTK's own value stands.
pub fn xwayland_scale() -> Option<f64> {
    let mut cmd = Command::new("xrdb");
    cmd.arg("-query").stdin(Stdio::null()).stderr(Stdio::null());
    let out = cmd.output().ok()?;
    scale_from_resources(&String::from_utf8_lossy(&out.stdout))
}

/// The scale an `xrdb -query` dump implies: Xft.dpi over the standard 96 dpi.
fn scale_from_resources(resources: &str) -> Option<f64> {
    let dpi: f64 = resources
        .lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:"))?
        .trim()
        .parse()
        .ok()?;
    (dpi > 0.0).then(|| dpi / 96.0)
}

/// GTK's share of the X server's scale: the nearest whole scale factor, and
/// the GDK_DPI_SCALE (as the text the variable takes) that cancels that
/// factor's share of the font dpi, so that a page draws at `xscale` exactly.
fn gdk_split(xscale: f64) -> (i32, String) {
    let gdk = xscale.round().max(1.0) as i32;
    (gdk, format!("{:.6}", 1.0 / gdk as f64))
}

/// X pixels per CSS pixel of our pages, once `prepare_env` has run. `None` when
/// the X server's scale is unknown; GTK's own scale factor is then the best guess.
pub fn ui_scale() -> Option<f64> {
    UI_SCALE.get().copied()
}

/// A size in CSS pixels as the GTK logical pixels Tauri sizes windows in. The
/// two differ by the dpi share of the scale (see `prepare_env`): 1.35 CSS
/// pixels to the logical pixel on a 135% screen.
pub fn css_to_gtk(v: f64) -> f64 {
    match (ui_scale(), env_f64("GDK_SCALE")) {
        (Some(s), Some(g)) if g >= 1.0 => v * s / g,
        _ => v,
    }
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
    let (Some(have), Some(want)) = (ui_scale(), xwayland_scale()) else {
        return false;
    };
    if (want - have).abs() < 0.01 {
        return false;
    }
    let exe = match std::env::var_os("APPIMAGE").map(std::path::PathBuf::from).or_else(|| std::env::current_exe().ok()) {
        Some(p) => p,
        None => return false,
    };
    crate::log::line(format!("screen scale changed {have:.2} -> {want:.2}, restarting to match"));
    // Hand the new process a clean slate so it works the scale out from scratch
    // and the AppImage runtime sets its own library paths.
    let mut cmd = Command::new("setsid");
    cmd.arg("-f").arg(&exe).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for var in ["GDK_SCALE", "GDK_DPI_SCALE", "GDK_BACKEND", "APPDIR", "APPIMAGE", "APPRUN", "OWD", "ARGV0"] {
        cmd.env_remove(var);
    }
    clean_env(&mut cmd);
    // The replacement waits for this process to be gone before it claims the
    // single instance (see `wait_for_predecessor`), so go right away: two of us
    // alive at once would make the new one bow out and leave nothing running.
    cmd.env(REPLACES_PID, std::process::id().to_string());
    if cmd.spawn().is_err() {
        crate::log::line("restart for new scale failed; staying up at the old one");
        return false;
    }
    std::process::exit(0);
}

/// Set on a relaunched process: the pid of the one it replaces.
const REPLACES_PID: &str = "COUCOU_REPLACES_PID";

/// A relaunch (see `relaunch_if_scale_changed`) must not meet its predecessor's
/// single-instance claim, which the plugin answers by quitting the newcomer.
/// So wait for the old process to be gone first, a few seconds at most.
fn wait_for_predecessor() {
    let Some(pid) = std::env::var(REPLACES_PID).ok().and_then(|p| p.parse::<u32>().ok()) else {
        return;
    };
    std::env::remove_var(REPLACES_PID);
    let started = std::time::Instant::now();
    while process_alive(pid) && started.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Whether `pid` is still running (a zombie waiting to be reaped is not).
fn process_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map(|stat| stat_says_alive(&stat)).unwrap_or(false)
}

/// Reads a /proc/<pid>/stat line: "pid (name) S ..."; the state letter follows
/// the parenthesised name, which may itself hold spaces and parentheses.
fn stat_says_alive(stat: &str) -> bool {
    stat.rsplit_once(')').map(|(_, rest)| !rest.trim_start().starts_with('Z')).unwrap_or(false)
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
    use super::{gdk_split, scale_from_resources, stat_says_alive};

    #[test]
    fn a_zombie_predecessor_counts_as_gone() {
        assert!(stat_says_alive("4141 (coucou) S 1 4141 4141 0 -1 4194560"));
        assert!(stat_says_alive("4141 (co (u) cou) R 1 4141"));
        assert!(!stat_says_alive("4141 (coucou) Z 1 4141 4141"));
        assert!(!stat_says_alive(""));
    }

    #[test]
    fn the_scale_comes_from_xft_dpi() {
        // KDE with a 200% screen: XWayland doubled, Xft.dpi 192.
        assert_eq!(scale_from_resources("Xcursor.size:\t24\nXft.dpi:\t192\n"), Some(2.0));
        assert_eq!(scale_from_resources("Xft.dpi:\t96\n"), Some(1.0));
        // A laptop at 150% or 135%: kept fractional, the page draws at just that.
        assert_eq!(scale_from_resources("Xft.dpi:\t144\n"), Some(1.5));
        assert!((scale_from_resources("Xft.dpi:\t129.6\n").unwrap() - 1.35).abs() < 1e-9);
        // Nothing to read, or nonsense: leave GTK's own value alone.
        assert_eq!(scale_from_resources(""), None);
        assert_eq!(scale_from_resources("Xcursor.size:\t24\n"), None);
        assert_eq!(scale_from_resources("Xft.dpi:\t0\n"), None);
    }

    #[test]
    fn gtk_takes_the_whole_part_and_the_dpi_the_rest() {
        // GDK_SCALE × GDK_DPI_SCALE × Xft.dpi/96 is what the page draws at:
        // always the X server's scale itself.
        assert_eq!(gdk_split(2.0), (2, "0.500000".into()));
        assert_eq!(gdk_split(1.0), (1, "1.000000".into()));
        assert_eq!(gdk_split(1.35), (1, "1.000000".into()));
        assert_eq!(gdk_split(1.5), (2, "0.500000".into()));
        assert_eq!(gdk_split(2.5), (3, "0.333333".into()));
        for xscale in [1.0, 1.35, 1.5, 2.0, 2.5] {
            let (gdk, dpi) = gdk_split(xscale);
            let drawn = gdk as f64 * dpi.parse::<f64>().unwrap() * xscale;
            assert!((drawn - xscale).abs() < 1e-5, "{xscale}: {drawn}");
        }
    }
}
