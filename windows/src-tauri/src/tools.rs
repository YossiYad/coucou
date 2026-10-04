// Tools the chat model can use to act on the computer, whichever model it is:
// find, read and page through files, create and change them, read web pages,
// search the web and open things.
//
// Reads stay inside the home folder and never touch a hidden path, which keeps
// SSH keys, browser profiles, the keyring, .env files and every app's settings
// out of reach. Every step shows in the island's work view, every change as a
// diff before it is made; whether it then waits for a click depends on the
// permission mode.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::sync::oneshot;

use crate::island::WINDOW_LABEL;
use crate::preview::{self, Cell, Head, Mark, Preview, Row};
use crate::{ai, docx, extract};

/// One part of a long file or page, about two pages of text.
const PART_CHARS: usize = 6_000;
const MAX_READ_BYTES: u64 = 50 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const SEARCH_LIMIT: usize = 30;
const SEARCH_BUDGET: Duration = Duration::from_secs(8);
/// Folders full of generated files nobody searches for by name.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "__pycache__", "venv", "site-packages"];
/// Lines of a file or result shown in the work view while it is read.
const EXCERPT_LINES: usize = 14;
pub const DECLINED: &str = "The user declined.";
const PLAN_REFUSAL: &str = "Plan mode is on, so nothing was changed.";

/// Who approves a change, like Claude Code's permission modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Every change waits for Allow.
    Manual,
    /// Asks only before what cannot be undone: file changes go through (a
    /// backup is always kept), and commands run unless they delete, wipe,
    /// force-push, close apps or shut down.
    Auto,
    /// Every change goes through; each is still shown, and a backup kept.
    AcceptEdits,
    /// Nothing is changed: the model reads what it needs and proposes a plan.
    Plan,
}

impl Mode {
    pub fn parse(id: &str) -> Self {
        match id {
            "auto" => Self::Auto,
            "acceptEdits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            _ => Self::Manual,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
        }
    }
}

/// Whether a tool changes something on the computer (a file, or whatever a
/// command does), as opposed to only reading or looking.
pub fn changes_things(name: &str) -> bool {
    WRITE_TOOLS.contains(&name)
}

/// Tools that change something, which plan mode never offers.
const WRITE_TOOLS: &[&str] = &["create_file", "edit_spreadsheet", "edit_document", "edit_file", "run_command"];

/// The tools, in the OpenAI function-calling format Ollama and LM Studio take;
/// the cloud providers convert it. Plan mode offers only the ones that read.
pub fn definitions(mode: Mode) -> Value {
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({ "type": "function", "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required },
        }})
    };
    let mut all = json!([
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
            "Read a file: text, code, spreadsheets, Word, PowerPoint, LibreOffice or PDF. Spreadsheets come with row numbers \
and Word documents with paragraph (¶) and table row numbers, which the edit tools take. Long files come in parts \
(PDFs page by page); the answer says how many there are, so ask for the next part only if you need it.",
            json!({
                "path": { "type": "string", "description": "File path, like ~/Desktop/report.pdf" },
                "part": { "type": "integer", "description": "Which part or PDF page to read, starting at 1" },
            }),
            &["path"]),
        tool("create_file",
            "Create a file, or replace one with overwrite. A spreadsheet (.xlsx or .csv) takes rows: a list of rows, each a \
list of cell values. Any other file takes lines: a list of text lines. The user sees the file before it is written.",
            json!({
                "path": { "type": "string", "description": "Where to save it, like ~/Desktop/notes.txt" },
                "rows": {
                    "type": "array",
                    "description": "For spreadsheets: the rows, the first usually being the column titles",
                    "items": { "type": "array", "items": { "type": "string" } },
                },
                "lines": { "type": "array", "description": "For text files: the lines", "items": { "type": "string" } },
                "sheet": { "type": "string", "description": "Optional name for the spreadsheet's sheet" },
                "overwrite": { "type": "boolean", "description": "Replace an existing file (a backup copy is kept)" },
            }),
            &["path"]),
        tool("edit_spreadsheet",
            "Change an existing spreadsheet (.xlsx) in place, keeping all its formatting: delete rows or columns and set \
cell values. Use the row numbers and column letters read_file shows. Never rewrite a spreadsheet with create_file. The \
user sees the change before it is made.",
            json!({
                "path": { "type": "string" },
                "sheet": { "type": "string", "description": "Sheet name; the first sheet if left out" },
                "delete_rows": {
                    "type": "array",
                    "description": "Row numbers to delete, as read_file shows them",
                    "items": { "type": "integer" },
                },
                "delete_columns": {
                    "type": "array",
                    "description": "Column letters to delete, like C",
                    "items": { "type": "string" },
                },
                "set_cells": {
                    "type": "array",
                    "description": "Cells to fill or change",
                    "items": {
                        "type": "object",
                        "properties": {
                            "cell": { "type": "string", "description": "Like C5" },
                            "value": { "type": "string" },
                        },
                        "required": ["cell", "value"],
                    },
                },
            }),
            &["path"]),
        tool("edit_document",
            "Change an existing Word document (.docx) in place, keeping all its formatting: delete paragraphs or table \
rows, replace text, or add paragraphs. Use the ¶ paragraph numbers and table row numbers read_file shows. Never rewrite \
a document with create_file. The user sees the change before it is made.",
            json!({
                "path": { "type": "string" },
                "delete_paragraphs": {
                    "type": "array",
                    "description": "Paragraph numbers (¶) to delete",
                    "items": { "type": "integer" },
                },
                "delete_table_rows": {
                    "type": "array",
                    "description": "Table rows to delete",
                    "items": {
                        "type": "object",
                        "properties": { "table": { "type": "integer" }, "row": { "type": "integer" } },
                        "required": ["table", "row"],
                    },
                },
                "replace": {
                    "type": "array",
                    "description": "Text to replace; each find must appear once (in the given paragraph, if one is given)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "find": { "type": "string" },
                            "replace": { "type": "string" },
                            "paragraph": { "type": "integer" },
                        },
                        "required": ["find", "replace"],
                    },
                },
                "insert": {
                    "type": "array",
                    "description": "New paragraphs, after paragraph number `after` (0 for the very start)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "after": { "type": "integer" },
                            "lines": { "type": "array", "items": { "type": "string" } },
                        },
                        "required": ["after", "lines"],
                    },
                },
            }),
            &["path"]),
        tool("edit_file",
            "Change part of a text file by replacing an exact piece of text. The user sees the change before it is made.",
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
        tool("run_command",
            "Run a terminal command (bash) on the user's computer and get its output: update or install apps, check the \
system, anything the terminal can do. The user sees the command and approves it first. No keyboard input reaches it.",
            json!({
                "command": { "type": "string", "description": "The command, like flatpak update -y" },
                "why": { "type": "string", "description": "One short sentence for the user on what it does" },
            }),
            &["command", "why"]),
        tool("look_at_screen",
            "Take a screenshot of the user's screen and see it. Use it whenever the user asks for help with something on \
their screen (an app, a setting, an error, where to click), then guide them one step at a time from what is visible.",
            json!({
                "all_screens": { "type": "boolean", "description": "Every monitor instead of the one the mouse is on" },
            }),
            &[]),
        tool("open",
            "Open a file or folder in its usual app, or a web link in the browser, for the user to see.",
            json!({ "target": { "type": "string", "description": "A path or an http(s) link" } }),
            &["target"]),
    ]);
    if mode == Mode::Plan {
        if let Some(list) = all.as_array_mut() {
            list.retain(|t| !WRITE_TOOLS.contains(&t.pointer("/function/name").and_then(Value::as_str).unwrap_or("")));
        }
    }
    all
}

static STEPS: AtomicU64 = AtomicU64::new(0);

/// A tool's answer: text for the model, and sometimes a picture it should see.
pub struct ToolOutput {
    pub text: String,
    pub image: Option<Picture>,
}

pub struct Picture {
    pub media: &'static str,
    pub data: Vec<u8>,
}

impl Picture {
    pub fn data_url(&self) -> String {
        ai::data_url(self.media, &self.data)
    }

    pub fn base64(&self) -> String {
        ai::base64(&self.data)
    }
}

/// Runs one tool call. Failures come back as text for the model to read, so it
/// can correct itself or tell the user. Each call is a step in the work view.
pub async fn run<R: Runtime>(app: &AppHandle<R>, name: &str, args: &Value, mode: Mode) -> ToolOutput {
    let arg = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let part = args.get("part").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let step = STEPS.fetch_add(1, Ordering::SeqCst) + 1;
    let target = ["path", "query", "url", "target"].iter().map(|k| arg(k)).find(|v| !v.is_empty()).unwrap_or_default();
    let shown = if target.starts_with("http") || !(name.contains("file") || name.contains("edit") || name == "list_folder") {
        target.clone()
    } else {
        file_name(&target)
    };
    let _ = app.emit_to(
        WINDOW_LABEL,
        "agent-step",
        json!({ "id": step, "tool": name, "target": shown, "path": target, "state": "running" }),
    );
    let mut image = None;
    let text = if mode == Mode::Plan && WRITE_TOOLS.contains(&name) {
        format!("{PLAN_REFUSAL} Describe this change in your plan instead.")
    } else if name == "look_at_screen" {
        let all = args.get("all_screens").and_then(Value::as_bool).unwrap_or(false);
        match look_at_screen(app, step, all, mode).await {
            Ok((text, picture)) => {
                image = picture;
                text
            }
            Err(e) => format!("Error: {e}"),
        }
    } else {
        run_tool(app, name, args, step, mode, &arg, part).await.unwrap_or_else(|e| format!("Error: {e}"))
    };
    let state = if text.starts_with("Error:") || text.starts_with("Failed with exit code") || text.starts_with("Stopped") {
        "failed"
    } else if text.starts_with(DECLINED) {
        "declined"
    } else if text.starts_with(PLAN_REFUSAL) {
        "skipped"
    } else {
        "done"
    };
    let mut done = json!({ "id": step, "state": state });
    if state == "done" && !WRITE_TOOLS.contains(&name) && name != "open" && name != "look_at_screen" {
        done["lines"] = json!(preview::excerpt(&text, EXCERPT_LINES));
    }
    let _ = app.emit_to(WINDOW_LABEL, "agent-step", done);
    ToolOutput { text, image }
}

/// A screenshot for the model. With screen sharing on it is taken and shown in
/// the work view; otherwise the user sees it first and allows this one.
async fn look_at_screen<R: Runtime>(
    app: &AppHandle<R>,
    step: u64,
    all: bool,
    mode: Mode,
) -> Result<(String, Option<Picture>), String> {
    let (sharing, scope_all) = {
        let shared = app.state::<crate::Shared>();
        let settings = shared.settings.lock().unwrap();
        (settings.screen_sharing, settings.screen_scope == "all")
    };
    let shot = tauri::async_runtime::spawn_blocking(move || crate::screen::capture(all || scope_all))
        .await
        .map_err(|e| e.to_string())??;
    crate::frame::flash(app, all || scope_all);
    let data = std::fs::read(&shot.path).map_err(|e| e.to_string())?;
    let picture = Picture { media: "image/jpeg", data };
    let shown = Preview::Image { src: shot.preview.clone().unwrap_or_else(|| picture.data_url()) };
    // Mode::Manual here only means "this may wait": plan mode may look too.
    let ask = !sharing && mode != Mode::Auto;
    let allowed = present(app, step, Mode::Manual, "Let it see your screen?", "Screen", "", shown, ask).await;
    if !allowed {
        let _ = std::fs::remove_file(&shot.path);
        return Ok((format!("{DECLINED} The screen was not shared. Ask the user to describe what they see."), None));
    }
    Ok((
        "Here is the user's screen right now (the screenshot is attached). Guide them from what is visible.".into(),
        Some(picture),
    ))
}

async fn run_tool<R: Runtime>(
    app: &AppHandle<R>,
    name: &str,
    args: &Value,
    step: u64,
    mode: Mode,
    arg: &(dyn Fn(&str) -> String + Sync),
    part: usize,
) -> Result<String, String> {
    match name {
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
            create_file(app, (step, mode), &arg("path"), file_body(args), &arg("sheet"), overwrite).await
        }
        "edit_spreadsheet" => edit_spreadsheet(app, (step, mode), &arg("path"), &arg("sheet"), args).await,
        "edit_document" => edit_document(app, (step, mode), &arg("path"), args).await,
        "edit_file" => {
            let get = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            edit_file(app, (step, mode), &arg("path"), &get("find"), &get("replace")).await
        }
        "read_web_page" => {
            activity(app, format!("Opening {}…", arg("url")));
            read_web_page(&arg("url"), part).await
        }
        "search_web" => {
            activity(app, format!("Searching the web for “{}”…", arg("query")));
            search_web(&arg("query")).await
        }
        "run_command" => {
            let raw = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            crate::shell::run_command(app, step, mode, &raw("command"), &raw("why")).await
        }
        "open" => open(&arg("target")),
        other => Err(format!("There is no tool called {other}.")),
    }
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

/// `~/x`, a path relative to home, or an absolute one, without the stray spaces
/// models leave around a component ("Desktop/ report.pdf").
fn expand(path: &str) -> PathBuf {
    expand_exact(&path.split('/').map(str::trim).collect::<Vec<_>>().join("/"))
}

fn expand_exact(path: &str) -> PathBuf {
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

/// An existing file or folder the model may read. Models often give just a
/// name ("report.pdf"): that is looked for on the desktop, in documents and in
/// downloads too.
fn readable(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("No path given.".into());
    }
    let mut candidates = vec![expand_exact(path), expand(path)];
    if !path.contains('/') {
        for dir in ["Desktop", "Documents", "Downloads"] {
            candidates.push(home().join(dir).join(path.trim()));
        }
    }
    let real = candidates
        .iter()
        .find_map(|c| c.canonicalize().ok())
        .ok_or_else(|| format!("{path} does not exist. Search for it with search_files."))?;
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
        let is_docx = file.extension().is_some_and(|e| e.eq_ignore_ascii_case("docx"));
        let attachment = if is_docx {
            docx::open(&file).ok().map(|d| ai::Attachment::Text(d.numbered_text()))
        } else {
            ai::read_attachment(&file.to_string_lossy())
        };
        let text = match attachment {
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

/// What to write. Models pass tables as rows and text as lines: one multi-line
/// string is what Qwen3 most often garbles into a call the server drops.
enum FileBody {
    Rows(Vec<Vec<String>>),
    Text(String),
}

fn file_body(args: &Value) -> FileBody {
    let cell = |v: &Value| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if let Some(rows) = args.get("rows").and_then(Value::as_array) {
        let rows = rows
            .iter()
            .map(|r| r.as_array().map(|cells| cells.iter().map(cell).collect()).unwrap_or_else(|| vec![cell(r)]))
            .collect();
        return FileBody::Rows(rows);
    }
    if let Some(lines) = args.get("lines").and_then(Value::as_array) {
        return FileBody::Text(lines.iter().map(cell).collect::<Vec<_>>().join("\n"));
    }
    FileBody::Text(args.get("content").and_then(Value::as_str).unwrap_or("").to_string())
}

fn csv_line(cells: &[String]) -> String {
    cells
        .iter()
        .map(|c| {
            if c.contains([',', '"', '\n']) {
                format!("\"{}\"", c.replace('"', "\"\""))
            } else {
                c.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

async fn create_file<R: Runtime>(
    app: &AppHandle<R>,
    (step, mode): (u64, Mode),
    path: &str,
    body: FileBody,
    sheet: &str,
    overwrite: bool,
) -> Result<String, String> {
    let target = writable(path)?;
    let exists = target.exists();
    let is_xlsx = target.extension().is_some_and(|e| e.eq_ignore_ascii_case("xlsx") || e.eq_ignore_ascii_case("xlsm"));
    let is_docx = target.extension().is_some_and(|e| e.eq_ignore_ascii_case("docx"));
    if exists && is_xlsx {
        return Err(format!(
            "{} already exists. Change it with edit_spreadsheet, which keeps its formatting; rewriting it would lose that.",
            tilde(&target)
        ));
    }
    if exists && is_docx {
        return Err(format!(
            "{} already exists. Change it with edit_document, which keeps its formatting; rewriting it would lose that.",
            tilde(&target)
        ));
    }
    if exists && !overwrite {
        return Err(format!("{} already exists. Set overwrite to replace it.", tilde(&target)));
    }
    let ext = target.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let old = if exists { std::fs::read_to_string(&target).unwrap_or_default() } else { String::new() };
    let preview = match &body {
        FileBody::Rows(rows) if ext == "csv" && exists => {
            let csv = rows.iter().map(|r| csv_line(r)).collect::<Vec<_>>().join("\n");
            Preview::Text { lines: preview::text_diff(&old, &csv) }
        }
        FileBody::Rows(rows) => preview::new_table(if sheet.is_empty() { "Sheet1" } else { sheet }, rows, 40),
        FileBody::Text(text) => Preview::Text { lines: preview::text_diff(&old, text) },
    };
    let title = if exists { "Replace the file" } else { "Create the file" };
    if !approve(app, step, mode, title, &target, preview).await {
        return Ok(format!("{DECLINED} Nothing was written."));
    }
    let mut note = String::new();
    if exists {
        let backup = backup_path(&target);
        std::fs::copy(&target, &backup).map_err(|e| format!("Could not keep a backup: {e}"))?;
        note = format!(" The previous version is kept as {}.", tilde(&backup));
    }
    match (body, ext.as_str()) {
        (FileBody::Rows(rows), "xlsx") => extract::write_rows_xlsx(&target, sheet, &rows)?,
        (FileBody::Rows(rows), "csv") => {
            let text: Vec<String> = rows.iter().map(|r| csv_line(r)).collect();
            std::fs::write(&target, text.join("\n") + "\n").map_err(|e| e.to_string())?
        }
        (FileBody::Rows(rows), _) => {
            let text: Vec<String> = rows.iter().map(|r| r.join("\t")).collect();
            std::fs::write(&target, text.join("\n") + "\n").map_err(|e| e.to_string())?
        }
        (FileBody::Text(text), "xlsx") => extract::write_xlsx(&target, &text)?,
        (FileBody::Text(text), _) => std::fs::write(&target, text).map_err(|e| e.to_string())?,
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

/// "C5" as (column 3, row 5), both from 1.
fn cell_ref(cell: &str) -> Option<(u32, u32)> {
    let cell = cell.trim().to_uppercase();
    let split = cell.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = cell.split_at(split);
    if letters.is_empty() || !letters.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let col = letters.chars().fold(0u32, |n, c| n * 26 + (c as u32 - 'A' as u32 + 1));
    let row: u32 = digits.parse().ok().filter(|r| *r > 0)?;
    Some((col, row))
}

fn open_sheet<'a>(
    book: &'a mut umya_spreadsheet::Workbook,
    sheet: &str,
) -> Result<&'a mut umya_spreadsheet::Worksheet, String> {
    if sheet.is_empty() {
        book.sheet_mut(0).map_err(|e| e.to_string())
    } else {
        book.sheet_by_name_mut(sheet).map_err(|_| format!("There is no sheet called {sheet}."))
    }
}

/// `C` or `3` as column 3.
fn column_ref(value: &Value) -> Option<u32> {
    if let Some(n) = value.as_u64() {
        return (n > 0).then_some(n as u32);
    }
    let text = value.as_str()?.trim().to_uppercase();
    if let Ok(n) = text.parse::<u32>() {
        return (n > 0).then_some(n);
    }
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_uppercase()) || text.len() > 3 {
        return None;
    }
    Some(text.chars().fold(0u32, |n, c| n * 26 + (c as u32 - 'A' as u32 + 1)))
}

/// The rows and columns a spreadsheet change touches, as they read now.
fn sheet_preview(
    ws: &umya_spreadsheet::Worksheet,
    rows: &[u32],
    columns: &[u32],
    cells: &[((u32, u32), String, String)],
) -> Preview {
    let width = ws.highest_column().max(columns.iter().copied().max().unwrap_or(1)).max(1);
    let height = ws.highest_row().max(rows.iter().copied().max().unwrap_or(1)).max(1);
    let mut touched_cols: Vec<u32> = columns.to_vec();
    touched_cols.extend(cells.iter().map(|((c, _), _, _)| *c));
    let mut touched_rows: Vec<u32> = rows.to_vec();
    touched_rows.extend(cells.iter().map(|((_, r), _, _)| *r));
    let show_cols = preview::pick(width, &touched_cols, 7);
    let show_rows = preview::pick(height, &touched_rows, 10);
    // As the sheet shows it: dates as dates, not day counts.
    let value = |c: u32, r: u32| ws.cell((c, r)).map(|cell| cell.formatted_value()).unwrap_or_default();
    let heads = show_cols
        .iter()
        .map(|c| match c {
            Some(c) => Head {
                label: preview::column_letters(*c),
                mark: if columns.contains(c) { Mark::Removed } else { Mark::Same },
            },
            None => Head { label: "…".into(), mark: Mark::Gap },
        })
        .collect();
    let lines = show_rows
        .iter()
        .map(|r| {
            let Some(r) = *r else { return Row { label: String::new(), mark: Mark::Gap, cells: Vec::new() } };
            let row_gone = rows.contains(&r);
            let cells = show_cols
                .iter()
                .map(|c| {
                    let Some(c) = *c else { return Cell { text: "…".into(), old: None, mark: Mark::Gap } };
                    let old = value(c, r);
                    if let Some((_, _, new)) = cells.iter().find(|((cc, rr), _, _)| (*cc, *rr) == (c, r)) {
                        let mark = if old.is_empty() { Mark::Added } else { Mark::Changed };
                        let old = (!old.is_empty()).then(|| preview::clip_cell(&old));
                        return Cell { text: preview::clip_cell(new), old, mark };
                    }
                    let gone = row_gone || columns.contains(&c);
                    Cell { text: preview::clip_cell(&old), old: None, mark: if gone { Mark::Removed } else { Mark::Same } }
                })
                .collect();
            Row { label: r.to_string(), mark: if row_gone { Mark::Removed } else { Mark::Same }, cells }
        })
        .collect();
    Preview::Table { sheet: ws.name().to_string(), columns: heads, rows: lines }
}

/// Changes a spreadsheet in place. create_file would rewrite it from bare
/// values and lose its styles, column widths, merged cells and direction.
async fn edit_spreadsheet<R: Runtime>(
    app: &AppHandle<R>,
    (step, mode): (u64, Mode),
    path: &str,
    sheet: &str,
    args: &Value,
) -> Result<String, String> {
    let target = readable(path)?;
    writable(&target.to_string_lossy())?;
    let is_xlsx = target.extension().is_some_and(|e| e.eq_ignore_ascii_case("xlsx") || e.eq_ignore_ascii_case("xlsm"));
    if !is_xlsx {
        return Err("edit_spreadsheet works on .xlsx files.".into());
    }
    let mut rows: Vec<u32> = args
        .get("delete_rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .filter(|r| *r > 0)
        .map(|r| r as u32)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    let mut columns: Vec<u32> = Vec::new();
    for value in args.get("delete_columns").and_then(Value::as_array).into_iter().flatten() {
        columns.push(column_ref(value).ok_or_else(|| format!("{value} is not a column like C."))?);
    }
    columns.sort_unstable();
    columns.dedup();
    let mut cells: Vec<((u32, u32), String, String)> = Vec::new();
    for entry in args.get("set_cells").and_then(Value::as_array).into_iter().flatten() {
        let name = entry.get("cell").and_then(Value::as_str).unwrap_or("");
        let at = cell_ref(name).ok_or_else(|| format!("\"{name}\" is not a cell like C5."))?;
        let value = match entry.get("value") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        cells.push((at, name.trim().to_uppercase(), value));
    }
    if rows.is_empty() && columns.is_empty() && cells.is_empty() {
        return Err("Say which rows or columns to delete or which cells to set.".into());
    }

    let mut book = umya_spreadsheet::reader::xlsx::read(&target).map_err(|e| format!("Could not open it: {e}"))?;
    let preview = sheet_preview(open_sheet(&mut book, sheet)?, &rows, &columns, &cells);
    if !approve(app, step, mode, "Change the spreadsheet", &target, preview).await {
        return Ok(format!("{DECLINED} The spreadsheet is unchanged."));
    }

    // Read again: the file may have changed while the change waited for a click.
    let mut book = umya_spreadsheet::reader::xlsx::read(&target).map_err(|e| format!("Could not open it: {e}"))?;
    let ws = open_sheet(&mut book, sheet)?;
    // Cells first, at the row numbers the model read; then rows bottom up and
    // columns right to left, so each deletion leaves the numbers before it as
    // they were.
    for ((c, r), _, value) in &cells {
        let empty = ws.cell((*c, *r)).is_none_or(|cell| cell.value().is_empty());
        if empty && *r > 1 {
            // A new entry looks like the one above it.
            let above = ws.style((*c, *r - 1)).clone();
            ws.set_style((*c, *r), above);
        }
        let leading_zero = value.len() > 1 && value.starts_with('0') && !value.starts_with("0.");
        let cell = ws.cell_mut((*c, *r));
        match value.trim().parse::<f64>() {
            Ok(n) if !leading_zero && !value.trim().is_empty() => cell.set_value_number(n),
            _ => cell.set_value(value.clone()),
        };
    }
    for r in rows.iter().rev() {
        extract::remove_row_keeping_formulas(ws, *r);
    }
    for c in columns.iter().rev() {
        extract::remove_column_keeping_formulas(ws, *c);
    }
    let backup = backup_path(&target);
    std::fs::copy(&target, &backup).map_err(|e| format!("Could not keep a backup: {e}"))?;
    umya_spreadsheet::writer::xlsx::write(&book, &target).map_err(|e| format!("Could not save it: {e}"))?;
    extract::repair_saved_xlsx(&backup, &target)?;
    Ok(format!(
        "Changed {}. Formulas, such as totals, recompute when the file is next opened. The previous version is kept \
as {}.",
        tilde(&target),
        tilde(&backup)
    ))
}

/// Changes a Word document in place, touching only the paragraphs and rows
/// that change.
async fn edit_document<R: Runtime>(app: &AppHandle<R>, (step, mode): (u64, Mode), path: &str, args: &Value) -> Result<String, String> {
    let target = readable(path)?;
    writable(&target.to_string_lossy())?;
    if !target.extension().is_some_and(|e| e.eq_ignore_ascii_case("docx")) {
        return Err("edit_document works on Word .docx files.".into());
    }
    let list = |key: &str| args.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let number = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().trim_start_matches('¶').parse().ok()));
    let text = |v: &Value, key: &str| match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let mut edits = docx::Edits::default();
    for v in list("delete_paragraphs") {
        edits.delete_paragraphs.push(number(&v).ok_or_else(|| format!("{v} is not a paragraph number."))? as u32);
    }
    edits.delete_paragraphs.sort_unstable();
    edits.delete_paragraphs.dedup();
    for v in list("delete_table_rows") {
        let (t, r) = (v.get("table").and_then(number), v.get("row").and_then(number));
        match (t, r) {
            (Some(t), Some(r)) if t > 0 && r > 0 => edits.delete_rows.push((t as u32, r as u32)),
            _ => return Err(format!("{v} is not a table row like {{\"table\": 1, \"row\": 2}}.")),
        }
    }
    edits.delete_rows.sort_unstable();
    edits.delete_rows.dedup();
    for v in list("replace") {
        let paragraph = v.get("paragraph").and_then(number).map(|n| n as u32);
        edits.replace.push((paragraph, text(&v, "find"), text(&v, "replace")));
    }
    for v in list("insert") {
        let after = v.get("after").and_then(number).unwrap_or(0) as u32;
        let lines = match v.get("lines") {
            Some(Value::Array(lines)) => lines.iter().map(|l| l.as_str().map_or_else(|| l.to_string(), str::to_string)).collect(),
            Some(Value::String(s)) => s.lines().map(str::to_string).collect(),
            _ => Vec::new(),
        };
        edits.insert.push((after, lines));
    }

    let doc = docx::open(&target)?;
    let plan = doc.plan(&edits)?;
    if !approve(app, step, mode, "Change the document", &target, plan.preview).await {
        return Ok(format!("{DECLINED} The document is unchanged."));
    }
    // Planned again on the file as it is now, in case it changed meanwhile.
    let plan = docx::open(&target)?.plan(&edits)?;
    let backup = backup_path(&target);
    std::fs::copy(&target, &backup).map_err(|e| format!("Could not keep a backup: {e}"))?;
    docx::write(&target, &plan.xml)?;
    Ok(format!(
        "Changed {}. Paragraph numbers after the change have moved; read it again before another edit. The previous \
version is kept as {}.",
        tilde(&target),
        tilde(&backup)
    ))
}

async fn edit_file<R: Runtime>(
    app: &AppHandle<R>,
    (step, mode): (u64, Mode),
    path: &str,
    find: &str,
    replace: &str,
) -> Result<String, String> {
    let target = readable(path)?;
    writable(&target.to_string_lossy())?;
    if find.is_empty() {
        return Err("Say which text to replace.".into());
    }
    let ext = target.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext == "xlsx" || ext == "xlsm" {
        return Err("That is a spreadsheet: change it with edit_spreadsheet, which keeps its formatting.".into());
    }
    if ext == "docx" {
        return Err("That is a Word document: change it with edit_document, which keeps its formatting.".into());
    }
    let text = std::fs::read_to_string(&target)
        .map_err(|_| format!("{} is not a text file, so it can't be edited here.", tilde(&target)))?;
    match text.matches(find).count() {
        0 => return Err("That text is not in the file. Read it again and copy the exact text.".into()),
        1 => {}
        n => return Err(format!("That text appears {n} times; include more around it so it is unique.")),
    }
    let changed = text.replacen(find, replace, 1);
    let lines = preview::text_diff(&text, &changed);
    if !approve(app, step, mode, "Edit the file", &target, Preview::Text { lines }).await {
        return Ok(format!("{DECLINED} The file is unchanged."));
    }
    let backup = backup_path(&target);
    std::fs::copy(&target, &backup).map_err(|e| format!("Could not keep a backup: {e}"))?;
    std::fs::write(&target, changed).map_err(|e| e.to_string())?;
    Ok(format!("Edited {}. The previous version is kept as {}.", tilde(&target), tilde(&backup)))
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

/// Shows a change in the work view and, when the mode says so, waits for the
/// Allow / Deny click. No answer is a no.
async fn approve<R: Runtime>(
    app: &AppHandle<R>,
    step: u64,
    mode: Mode,
    title: &str,
    target: &Path,
    preview: Preview,
) -> bool {
    // Every file change keeps a backup, so in Auto none of them has to wait.
    let waiting = match mode {
        Mode::Manual => true,
        Mode::Auto => false,
        Mode::AcceptEdits => false,
        Mode::Plan => return false,
    };
    let file = file_name(&target.to_string_lossy());
    present(app, step, mode, title, &file, &tilde(target), preview, waiting).await
}

/// Shows a change in the work view and waits for the click when `waiting`.
#[allow(clippy::too_many_arguments)]
pub async fn present<R: Runtime>(
    app: &AppHandle<R>,
    step: u64,
    mode: Mode,
    title: &str,
    file: &str,
    path: &str,
    preview: Preview,
    waiting: bool,
) -> bool {
    if mode == Mode::Plan {
        return false;
    }
    let approvals = app.state::<Approvals>();
    let id = approvals.next.fetch_add(1, Ordering::SeqCst) + 1;
    let rx = waiting.then(|| {
        let (tx, rx) = oneshot::channel();
        approvals.pending.lock().unwrap().insert(id, tx);
        rx
    });
    let _ = app.emit_to(
        WINDOW_LABEL,
        "agent-change",
        json!({
            "id": id,
            "step": step,
            "title": title,
            "file": file,
            "path": path,
            "preview": preview,
            "waiting": waiting,
            "mode": mode.as_str(),
        }),
    );
    let allowed = match rx {
        Some(rx) => matches!(tokio::time::timeout(APPROVAL_TIMEOUT, rx).await, Ok(Ok(true))),
        None => true,
    };
    approvals.pending.lock().unwrap().remove(&id);
    let _ = app.emit_to(WINDOW_LABEL, "agent-change-done", json!({ "id": id, "allowed": allowed }));
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
    fn rows_and_lines_become_the_file_body() {
        match file_body(&json!({ "rows": [["a", "b"], ["1", 2.5]] })) {
            FileBody::Rows(rows) => assert_eq!(rows, vec![vec!["a", "b"], vec!["1", "2.5"]]),
            FileBody::Text(_) => panic!("rows expected"),
        }
        match file_body(&json!({ "lines": ["one", "two"] })) {
            FileBody::Text(t) => assert_eq!(t, "one\ntwo"),
            FileBody::Rows(_) => panic!("text expected"),
        }
        assert_eq!(csv_line(&["a,b".into(), "say \"hi\"".into(), "c".into()]), "\"a,b\",\"say \"\"hi\"\"\",c");
    }

    #[test]
    fn stray_spaces_in_a_path_are_dropped() {
        assert_eq!(expand("~/Desktop/ report.pdf"), home().join("Desktop/report.pdf"));
    }

    #[test]
    fn cell_references_parse_like_excel() {
        assert_eq!(cell_ref("C5"), Some((3, 5)));
        assert_eq!(cell_ref(" aa10 "), Some((27, 10)));
        assert_eq!(cell_ref("5C"), None);
        assert_eq!(cell_ref("C0"), None);
    }

    #[test]
    fn backups_get_a_readable_name() {
        let p = backup_path(Path::new("/tmp/does-not-exist-dir/report.xlsx"));
        assert_eq!(p, PathBuf::from("/tmp/does-not-exist-dir/report (before Mochi).xlsx"));
    }
}

