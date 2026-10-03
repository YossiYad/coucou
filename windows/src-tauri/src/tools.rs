// Tools a local model can use to act on the computer: find, read and page
// through files, create and edit them (only once the user allows it in the
// island), read web pages, search the web and open things.
//
// Reads stay inside the home folder and never touch a hidden path, which keeps
// SSH keys, browser profiles, the keyring, .env files and every app's settings
// out of reach. Every write is shown in the island first and waits for a click.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::sync::oneshot;

use crate::island::WINDOW_LABEL;
use crate::{ai, extract};

/// One part of a long file or page, about two pages of text.
const PART_CHARS: usize = 6_000;
const MAX_READ_BYTES: u64 = 50 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const SEARCH_LIMIT: usize = 30;
const SEARCH_BUDGET: Duration = Duration::from_secs(8);
/// Folders full of generated files nobody searches for by name.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "__pycache__", "venv", "site-packages"];

/// The tools, in the OpenAI function-calling format Ollama and LM Studio take.
pub fn definitions() -> Value {
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({ "type": "function", "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required },
        }})
    };
    json!([
        tool("search_files",
            "Find files and folders in the user's home folder whose name contains the given words (any language, any case).",
            json!({
                "query": { "type": "string", "description": "Words from the file or folder name" },
                "folder": { "type": "string", "description": "Optional folder to search in, like ~/Documents" },
            }),
            &["query"]),
        tool("list_folder",
            "List what is inside a folder.",
            json!({ "path": { "type": "string", "description": "Folder path, like ~/Desktop" } }),
            &["path"]),
        tool("read_file",
            "Read a file: text, code, spreadsheets, Word, PowerPoint, LibreOffice or PDF. Long files come in parts \
(PDFs page by page); the answer says how many there are, so ask for the next part only if you need it.",
            json!({
                "path": { "type": "string", "description": "File path, like ~/Desktop/report.pdf" },
                "part": { "type": "integer", "description": "Which part or PDF page to read, starting at 1" },
            }),
            &["path"]),
        tool("create_file",
            "Create a file, or replace one with overwrite. For .xlsx write tab-separated rows, one row per line; a line \
\"Sheet: name\" starts a sheet. The user must approve before anything is written.",
            json!({
                "path": { "type": "string", "description": "Where to save it, like ~/Desktop/notes.txt" },
                "content": { "type": "string", "description": "The full content" },
                "overwrite": { "type": "boolean", "description": "Replace an existing file (a backup copy is kept)" },
            }),
            &["path", "content"]),
        tool("edit_file",
            "Change part of a text file by replacing an exact piece of text. The user must approve the change.",
            json!({
                "path": { "type": "string" },
                "find": { "type": "string", "description": "Exact text to replace; must appear exactly once" },
                "replace": { "type": "string", "description": "The new text" },
            }),
            &["path", "find", "replace"]),
        tool("read_web_page",
            "Read a web page as text, with its links numbered so you can follow them. Long pages come in parts.",
            json!({
                "url": { "type": "string" },
                "part": { "type": "integer", "description": "Which part to read, starting at 1" },
            }),
            &["url"]),
        tool("search_web",
            "Search the web and get the top results: title, link and a short snippet.",
            json!({ "query": { "type": "string" } }),
            &["query"]),
        tool("open",
            "Open a file or folder in its usual app, or a web link in the browser, for the user to see.",
            json!({ "target": { "type": "string", "description": "A path or an http(s) link" } }),
            &["target"]),
    ])
}

/// Runs one tool call. Failures come back as text for the model to read, so it
/// can correct itself or tell the user.
pub async fn run<R: Runtime>(app: &AppHandle<R>, name: &str, args: &Value) -> String {
    let arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let part = args.get("part").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let result = match name {
        "search_files" => {
            activity(app, format!("Searching for “{}”…", arg("query")));
            search_files(arg("query"), arg("folder")).await
        }
        "list_folder" => {
            activity(app, format!("Looking in {}…", arg("path")));
            list_folder(&arg("path"))
        }
        "read_file" => {
            activity(app, format!("Reading {}…", file_name(&arg("path"))));
            read_file(arg("path"), part).await
        }
        "create_file" => {
            let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
            create_file(app, &arg("path"), args.get("content").and_then(Value::as_str).unwrap_or(""), overwrite).await
        }
        "edit_file" => {
            let get = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            edit_file(app, &arg("path"), &get("find"), &get("replace")).await
        }
        "read_web_page" => {
            activity(app, format!("Opening {}…", arg("url")));
            read_web_page(&arg("url"), part).await
        }
        "search_web" => {
            activity(app, format!("Searching the web for “{}”…", arg("query")));
            search_web(&arg("query")).await
        }
        "open" => open(&arg("target")),
        other => Err(format!("There is no tool called {other}.")),
    };
    result.unwrap_or_else(|e| format!("Error: {e}"))
}

fn activity<R: Runtime>(app: &AppHandle<R>, text: String) {
    let _ = app.emit_to(WINDOW_LABEL, "tool-activity", json!({ "text": text }));
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

// ── Where the model may go ────────────────────────────────────────────────────

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `~/x`, a path relative to home, or an absolute one.
fn expand(path: &str) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home().join(rest)
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        home().join(path)
    }
}

/// Inside home, with no hidden component on the way: this is what keeps
/// ~/.ssh, ~/.config, ~/.local, browser profiles and .env files out of reach.
fn allowed(real: &Path) -> Result<(), String> {
    let home = home().canonicalize().map_err(|e| e.to_string())?;
    let rel = real
        .strip_prefix(&home)
        .map_err(|_| "Only files inside the home folder can be used.".to_string())?;
    for part in rel.components() {
        match part {
            Component::Normal(name) if name.to_string_lossy().starts_with('.') => {
                return Err("Hidden files and folders are off limits.".into())
            }
            Component::Normal(_) => {}
            _ => return Err("That path is not allowed.".into()),
        }
    }
    Ok(())
}

/// An existing file or folder the model may read.
fn readable(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("No path given.".into());
    }
    let real = expand(path).canonicalize().map_err(|_| format!("{path} does not exist."))?;
    allowed(&real)?;
    Ok(real)
}

/// Where the model may write: an existing folder it may read, and a visible name.
fn writable(path: &str) -> Result<PathBuf, String> {
    let target = expand(path);
    let name = target.file_name().ok_or_else(|| "No file name given.".to_string())?.to_owned();
    if name.to_string_lossy().starts_with('.') {
        return Err("Hidden files are off limits.".into());
    }
    let parent = target.parent().ok_or_else(|| "No folder given.".to_string())?;
    let parent = parent
        .canonicalize()
        .map_err(|_| format!("The folder {} does not exist.", parent.display()))?;
    allowed(&parent)?;
    let real = parent.join(name);
    // A symlink must not lead out of the allowed area.
    if let Ok(resolved) = real.canonicalize() {
        allowed(&resolved)?;
    }
    Ok(real)
}

fn tilde(path: &Path) -> String {
    match path.strip_prefix(home().canonicalize().unwrap_or_else(|_| home())) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

// ── Files ─────────────────────────────────────────────────────────────────────

async fn search_files(query: String, folder: String) -> Result<String, String> {
    if query.is_empty() {
        return Err("Say what to look for.".into());
    }
    let root = if folder.is_empty() { readable("~")? } else { readable(&folder)? };
    let q = query.clone();
    let mut found = tauri::async_runtime::spawn_blocking(move || walk_for(&root, &q))
        .await
        .map_err(|e| e.to_string())?;
    // KDE's index also matches words inside documents, when it has them.
    for path in baloo(&query).await {
        if found.len() >= SEARCH_LIMIT {
            break;
        }
        if !found.contains(&path) && allowed(&path).is_ok() {
            found.push(path);
        }
    }
    if found.is_empty() {
        return Ok(format!("Nothing found for “{query}”."));
    }
    Ok(found.iter().map(|p| describe_entry(p)).collect::<Vec<_>>().join("\n"))
}

fn walk_for(root: &Path, query: &str) -> Vec<PathBuf> {
    let terms: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_string).collect();
    let started = Instant::now();
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    // Breadth first: files near the top of home turn up before deep ones.
    while let Some(dir) = queue.pop_front() {
        if found.len() >= SEARCH_LIMIT || started.elapsed() > SEARCH_BUDGET {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            let lower = name.to_lowercase();
            if terms.iter().all(|t| lower.contains(t.as_str())) {
                found.push(entry.path());
                if found.len() >= SEARCH_LIMIT {
                    break;
                }
            }
            if kind.is_dir() && !SKIP_DIRS.contains(&name.as_str()) {
                queue.push_back(entry.path());
            }
        }
    }
    found
}

async fn baloo(query: &str) -> Vec<PathBuf> {
    let query = query.to_string();
    let output = tauri::async_runtime::spawn_blocking(move || {
        let mut cmd = std::process::Command::new("baloosearch6");
        cmd.args(["--limit", "20", &query]);
        #[cfg(target_os = "linux")]
        crate::linux::clean_env(&mut cmd);
        cmd.output()
    })
    .await;
    match output {
        Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| PathBuf::from(l.trim()))
            .filter(|p| p.is_absolute() && p.exists())
            .collect(),
        _ => Vec::new(),
    }
}

fn describe_entry(path: &Path) -> String {
    let meta = std::fs::metadata(path).ok();
    let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
    let size = meta.as_ref().filter(|m| !m.is_dir()).map(|m| format!(", {}", human_size(m.len()))).unwrap_or_default();
    format!("{}{}{size}", tilde(path), if is_dir { "/" } else { "" })
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} bytes"),
    }
}

fn list_folder(path: &str) -> Result<String, String> {
    let dir = readable(if path.is_empty() { "~" } else { path })?;
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    entries.sort_by_key(|p| (!p.is_dir(), p.file_name().map(|n| n.to_string_lossy().to_lowercase())));
    let total = entries.len();
    let mut out: Vec<String> = entries.iter().take(200).map(|p| describe_entry(p)).collect();
    if total > 200 {
        out.push(format!("... and {} more", total - 200));
    }
    if out.is_empty() {
        return Ok(format!("{} is empty.", tilde(&dir)));
    }
    Ok(format!("{}:\n{}", tilde(&dir), out.join("\n")))
}

async fn read_file(path: String, part: usize) -> Result<String, String> {
    let file = readable(&path)?;
    if file.is_dir() {
        return list_folder(&path);
    }
    let size = std::fs::metadata(&file).map_err(|e| e.to_string())?.len();
    if size > MAX_READ_BYTES {
        return Err(format!("{} is too big to read ({}).", tilde(&file), human_size(size)));
    }
    let label = tilde(&file);
    tauri::async_runtime::spawn_blocking(move || {
        let is_pdf = file.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        if is_pdf {
            let bytes = std::fs::read(&file).map_err(|e| e.to_string())?;
            let pages = extract::pdf_pages(&bytes)
                .ok_or_else(|| "This PDF has no text in it (probably a scan).".to_string())?;
            let page = pages.get(part - 1).ok_or_else(|| format!("The PDF has {} pages.", pages.len()))?;
            return Ok(format!("{label}, page {part} of {}:\n{}", pages.len(), extract::clip(page.trim(), PART_CHARS * 2)));
        }
        let text = match ai::read_attachment(&file.to_string_lossy()) {
            Some(ai::Attachment::Text(text)) => text,
            Some(ai::Attachment::Image { .. }) => return Ok(format!("{label} is an image; it can't be read as text.")),
            _ => return Err(format!("{label} can't be read as text.")),
        };
        let parts = split_parts(&text, PART_CHARS);
        let body = parts.get(part - 1).ok_or_else(|| format!("{label} has {} parts.", parts.len()))?;
        Ok(if parts.len() == 1 {
            format!("{label}:\n{body}")
        } else {
            format!("{label}, part {part} of {}:\n{body}", parts.len())
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Pieces of about `size` characters, cut at a line break where there is one.
fn split_parts(text: &str, size: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.chars().count() + line.chars().count() > size && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        if line.chars().count() > size {
            // One enormous line (minified files): cut it anyway.
            let chars: Vec<char> = line.chars().collect();
            for chunk in chars.chunks(size) {
                parts.push(chunk.iter().collect());
            }
            continue;
        }
        current.push_str(line);
    }
    if !current.is_empty() || parts.is_empty() {
        parts.push(current);
    }
    parts
}

async fn create_file<R: Runtime>(app: &AppHandle<R>, path: &str, content: &str, overwrite: bool) -> Result<String, String> {
    let target = writable(path)?;
    let exists = target.exists();
    if exists && !overwrite {
        return Err(format!("{} already exists. Set overwrite to replace it.", tilde(&target)));
    }
    let preview: String = content.lines().take(12).collect::<Vec<_>>().join("\n");
    let title = if exists { "Mochi wants to replace a file" } else { "Mochi wants to create a file" };
    let detail = format!("{}\n\n{}", tilde(&target), extract::clip(&preview, 600));
    if !ask(app, title, &detail).await {
        return Ok("The user declined. Nothing was written.".into());
    }
    let mut note = String::new();
    if exists {
        let backup = backup_path(&target);
        std::fs::copy(&target, &backup).map_err(|e| format!("Could not keep a backup: {e}"))?;
        note = format!(" The previous version is kept as {}.", tilde(&backup));
    }
    let is_xlsx = target.extension().is_some_and(|e| e.eq_ignore_ascii_case("xlsx"));
    if is_xlsx {
        extract::write_xlsx(&target, content)?;
    } else {
        std::fs::write(&target, content).map_err(|e| e.to_string())?;
    }
    Ok(format!("Saved {}.{note}", tilde(&target)))
}

/// `report.xlsx` → `report (before Mochi).xlsx`, numbered if that exists too.
fn backup_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut candidate = dir.join(format!("{stem} (before Mochi){ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} (before Mochi {n}){ext}"));
        n += 1;
    }
    candidate
}

async fn edit_file<R: Runtime>(app: &AppHandle<R>, path: &str, find: &str, replace: &str) -> Result<String, String> {
    let target = readable(path)?;
    writable(&target.to_string_lossy())?;
    if find.is_empty() {
        return Err("Say which text to replace.".into());
    }
    let text = std::fs::read_to_string(&target)
        .map_err(|_| format!("{} is not a text file; use create_file to rewrite it.", tilde(&target)))?;
    match text.matches(find).count() {
        0 => return Err("That text is not in the file. Read it again and copy the exact text.".into()),
        1 => {}
        n => return Err(format!("That text appears {n} times; include more around it so it is unique.")),
    }
    let detail = format!(
        "{}\n\n- {}\n+ {}",
        tilde(&target),
        extract::clip(find, 300).replace('\n', "\n- "),
        extract::clip(replace, 300).replace('\n', "\n+ ")
    );
    if !ask(app, "Mochi wants to edit a file", &detail).await {
        return Ok("The user declined. The file is unchanged.".into());
    }
    std::fs::write(&target, text.replacen(find, replace, 1)).map_err(|e| e.to_string())?;
    Ok(format!("Edited {}.", tilde(&target)))
}

fn open(target: &str) -> Result<String, String> {
    let what = if target.starts_with("http://") || target.starts_with("https://") {
        target.to_string()
    } else {
        readable(target)?.to_string_lossy().to_string()
    };
    #[cfg(target_os = "linux")]
    {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(&what);
        crate::linux::clean_env(&mut cmd).spawn().map_err(|e| e.to_string())?;
        Ok(format!("Opened {what}."))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = what;
        Err("Opening things is only supported on Linux for now.".into())
    }
}

// ── Web ───────────────────────────────────────────────────────────────────────

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

/// Public web only: a page must not reach the router, a local service or
/// anything else on the user's network.
async fn public_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| format!("{raw} is not a web address."))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only http and https links can be opened.".into());
    }
    let host = url.host_str().ok_or_else(|| "The link has no host.".to_string())?.to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| format!("Could not find {host}."))?
        .collect();
    if addrs.is_empty() || addrs.iter().any(|a| is_private(a.ip())) {
        return Err("Links to this computer or the local network can't be opened.".into());
    }
    Ok(url)
}

fn is_private(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) // carrier-grade NAT
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private(std::net::IpAddr::V4(v4)))
        }
    }
}

async fn fetch(url: reqwest::Url) -> Result<(reqwest::Url, String), String> {
    // Redirects are followed by hand so each hop is checked like the first.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| e.to_string())?;
    let mut url = url;
    for _ in 0..6 {
        let response = client.get(url.clone()).send().await.map_err(|e| format!("Could not load the page: {e}"))?;
        if response.status().is_redirection() {
            let next = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|loc| url.join(loc).ok())
                .ok_or_else(|| "The page redirected nowhere.".to_string())?;
            url = public_url(next.as_str()).await?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("The page answered {}.", response.status()));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_PAGE_BYTES)]).to_string();
        return Ok((url, body));
    }
    Err("Too many redirects.".into())
}

async fn read_web_page(raw: &str, part: usize) -> Result<String, String> {
    let (url, body) = fetch(public_url(raw).await?).await?;
    let page = extract::html_page(&body, &url);
    let parts = split_parts(&page.text, PART_CHARS);
    let text = parts.get(part - 1).ok_or_else(|| format!("The page has {} parts.", parts.len()))?;
    let mut out = format!("{}\n{url}", page.title);
    if parts.len() > 1 {
        out.push_str(&format!("\nPart {part} of {}", parts.len()));
    }
    out.push_str(&format!("\n\n{text}"));
    if part == 1 && !page.links.is_empty() {
        out.push_str("\n\nLinks:\n");
        for (i, (label, href)) in page.links.iter().take(40).enumerate() {
            out.push_str(&format!("[{}] {label} -> {href}\n", i + 1));
        }
    }
    Ok(out)
}

async fn search_web(query: &str) -> Result<String, String> {
    if query.is_empty() {
        return Err("Say what to search for.".into());
    }
    let mut url = reqwest::Url::parse("https://html.duckduckgo.com/html/").unwrap();
    url.query_pairs_mut().append_pair("q", query);
    let (_, body) = fetch(url).await?;
    let results = extract::duckduckgo_results(&body);
    if results.is_empty() {
        return Ok(format!("No results for “{query}”."));
    }
    Ok(results
        .iter()
        .take(8)
        .enumerate()
        .map(|(i, r)| format!("{}. {}\n{}\n{}", i + 1, r.0, r.1, r.2))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

// ── Asking the user ───────────────────────────────────────────────────────────

/// Writes wait here for the Allow / Deny click in the island.
#[derive(Default)]
pub struct Approvals {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
}

pub fn decide<R: Runtime>(app: &AppHandle<R>, id: u64, allow: bool) {
    if let Some(tx) = app.state::<Approvals>().pending.lock().unwrap().remove(&id) {
        let _ = tx.send(allow);
    }
}

async fn ask<R: Runtime>(app: &AppHandle<R>, title: &str, detail: &str) -> bool {
    let approvals = app.state::<Approvals>();
    let id = approvals.next.fetch_add(1, Ordering::SeqCst) + 1;
    let (tx, rx) = oneshot::channel();
    approvals.pending.lock().unwrap().insert(id, tx);
    let _ = app.emit_to(WINDOW_LABEL, "tool-approval", json!({ "id": id, "title": title, "detail": detail }));
    // No answer is a no.
    let allowed = matches!(tokio::time::timeout(APPROVAL_TIMEOUT, rx).await, Ok(Ok(true)));
    approvals.pending.lock().unwrap().remove(&id);
    let _ = app.emit_to(WINDOW_LABEL, "tool-approval-done", json!({ "id": id }));
    allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_paths_and_paths_outside_home_are_refused() {
        let home = home().canonicalize().unwrap();
        assert!(allowed(&home.join("Desktop/report.txt")).is_ok());
        assert!(allowed(&home.join(".ssh/id_ed25519")).is_err());
        assert!(allowed(&home.join("projects/app/.env")).is_err());
        assert!(allowed(&home.join(".config/coucou/settings.json")).is_err());
        assert!(allowed(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn writes_cannot_create_hidden_files() {
        assert!(writable("~/.bashrc").is_err());
        assert!(writable("~/no-such-folder/x.txt").is_err());
    }

    #[test]
    fn private_addresses_are_refused() {
        for ip in ["127.0.0.1", "10.0.0.5", "192.168.1.1", "172.16.0.1", "169.254.1.1", "100.64.0.1", "::1", "fd00::1", "fe80::1"] {
            assert!(is_private(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[test]
    fn long_text_splits_at_line_breaks() {
        let text = "a\n".repeat(10);
        let parts = split_parts(&text, 6);
        assert!(parts.iter().all(|p| p.chars().count() <= 6));
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn backups_get_a_readable_name() {
        let p = backup_path(Path::new("/tmp/does-not-exist-dir/report.xlsx"));
        assert_eq!(p, PathBuf::from("/tmp/does-not-exist-dir/report (before Mochi).xlsx"));
    }
}

