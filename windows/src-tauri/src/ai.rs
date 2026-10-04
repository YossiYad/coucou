// The chat behind the island, whichever model answers it: Claude, OpenAI,
// Gemini, or a local OpenAI-compatible server (Ollama, LM Studio, llama.cpp).
//
// API keys never leave the key store and file bytes never cross the IPC
// boundary: the island sends a query, and the provider's request is built here.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::settings::Settings;
use crate::{agent, claude, extract, fallback, gemini, local_llm, openai};

/// Text and code files are inlined; anything larger is skipped, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;

pub fn system_prompt(web_search: bool) -> String {
    let reach = if web_search {
        "You have web search access and can help with absolutely anything"
    } else {
        "You can help with absolutely anything"
    };
    format!(
        "You are Mochi, a personal AI assistant living at the top of the user's screen. \
{reach}: research, coding, finding places, recommendations, tasks, questions. \
Always reply in the language the user wrote their latest message in, whatever that language is, \
even when attached files, web results or earlier messages are in another language. \
When a file is attached, answer from its actual contents. \
Be thorough and complete, with as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks."
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
    Gemini,
    Local,
}

impl Provider {
    pub fn parse(id: &str) -> Self {
        match id {
            "openai" => Self::OpenAi,
            "gemini" => Self::Gemini,
            "local" => Self::Local,
            _ => Self::Anthropic,
        }
    }
}

/// The conversation so far, in the format of the provider that holds it.
#[derive(Default)]
pub struct Chat {
    messages: Mutex<Vec<Value>>,
    provider: Mutex<Option<Provider>>,
    /// Last language the user was recognised writing in, for short follow-ups.
    language: Mutex<Option<&'static str>>,
    /// Bumped by every reset, so an answer that arrives after the conversation
    /// was cleared is dropped instead of starting the new one mid-reply.
    epoch: std::sync::atomic::AtomicU64,
}

impl Chat {
    pub fn reset(&self) {
        self.messages.lock().unwrap().clear();
        *self.language.lock().unwrap() = None;
        self.epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The language to answer this message in: recognised from the message, or
    /// carried over from earlier in the conversation when it is too short to tell.
    fn reply_language(&self, text: &str) -> Option<&'static str> {
        let mut known = self.language.lock().unwrap();
        if let Some(lang) = detect_language(text) {
            *known = Some(lang);
        }
        *known
    }

    /// Each provider has its own message format, so switching starts over.
    fn begin(&self, provider: Provider) {
        let mut current = self.provider.lock().unwrap();
        if *current != Some(provider) {
            self.reset();
            *current = Some(provider);
        }
    }

    fn is_empty(&self) -> bool {
        self.messages.lock().unwrap().is_empty()
    }

    fn push(&self, message: Value) {
        self.messages.lock().unwrap().push(message);
    }

    fn pop(&self) {
        self.messages.lock().unwrap().pop();
    }

    fn snapshot(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File {
        name: String,
        /// The copy in Coucou's inbox, which is what gets read.
        path: String,
        /// Where the user dropped it from, so a model with tools can work on it.
        #[serde(default)]
        original: Option<String>,
    },
    Window { app_name: String, title: String, url: Option<String> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
    /// Set when another model answered because the chosen one could not.
    pub note: Option<String>,
}

#[derive(Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
}

/// A file's contents, read once and shaped by each provider.
#[derive(Clone)]
pub enum Attachment {
    Image { media: &'static str, data: Vec<u8> },
    Pdf(Vec<u8>),
    Text(String),
}

#[derive(Clone)]
pub struct FileNote {
    pub name: String,
    /// None when the file could not be read or is too large to inline.
    pub content: Option<Attachment>,
    /// Where it came from, told to models that can act on files.
    pub origin: Option<String>,
}

impl FileNote {
    /// "File: name", with a warning when its contents never made it, so the
    /// model says so instead of inventing them.
    pub fn label(&self) -> String {
        let origin = self.origin.as_ref().map(|o| format!(" (dropped from {o})")).unwrap_or_default();
        match self.content {
            Some(_) => format!("File: {}{origin}", self.name),
            None => format!(
                "File: {} (Coucou could not read this file's contents. Tell the user so; do not guess what is in it.)",
                self.name
            ),
        }
    }
}

/// What the user says this turn, before a provider shapes it. File and window
/// context ride along with the first message only, like ClaudeService.chat().
pub struct UserTurn {
    pub text: String,
    pub file: Option<FileNote>,
    pub window: Option<String>,
}

impl UserTurn {
    fn new(text: String, context: Option<ChatContext>, with_origin: bool) -> Self {
        let mut turn = Self { text, file: None, window: None };
        match context {
            Some(ChatContext::File { name, path, original }) => {
                let origin = original.filter(|_| with_origin).map(|o| home_relative(&o));
                turn.file = Some(FileNote { name, content: read_attachment(&path), origin });
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut note = format!("Context -App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    note.push_str(&format!(", URL: {url}"));
                }
                turn.window = Some(note);
            }
            None => {}
        }
        turn
    }
}

/// A provider's answer: the message kept in the history, and the text shown.
pub struct Answer {
    pub stored: Value,
    pub text: String,
}

/// One chat turn. Returns the assistant's text, or a message the island shows
/// in the note view.
pub async fn send<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    chat: &Chat,
    settings: &Settings,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let provider = Provider::parse(&settings.provider);
    chat.begin(provider);
    let hint = match chat.reply_language(&query) {
        Some(lang) => format!("(Answer in {lang}.)"),
        None => "(Reply in the same language I used in this message.)".to_string(),
    };
    let text = format!("{query}\n\n{hint}");
    let acts = settings.tools_enabled;
    // A file (dropped or pasted) rides along whenever the island sends one;
    // the window the user was in only opens a conversation.
    let context = match context {
        Some(file @ ChatContext::File { .. }) => Some(file),
        other if chat.is_empty() => other,
        _ => None,
    };
    let turn = UserTurn::new(text, context, acts);

    // What was asked of whom, never the words themselves.
    let file = turn.file.as_ref().map(|f| format!(", file {}", f.name)).unwrap_or_default();
    crate::log::line(format!("chat: asking {provider:?}{file}"));
    let user = match provider {
        Provider::Anthropic => claude::user_message(&turn),
        Provider::OpenAi => openai::user_message(&turn),
        Provider::Gemini => gemini::user_message(&turn),
        Provider::Local => local_llm::user_message(&turn).inspect_err(|e| crate::log::line(format!("chat: refused: {e}")))?,
    };
    chat.push(user);
    let history = chat.snapshot();
    let epoch = chat.epoch();

    let started = std::time::Instant::now();
    let chosen = fallback::Target::new(provider, &chosen_model(provider, settings), 1);
    let mut result = ask(app, &chosen, settings, &history, acts).await;
    let mut note = None;
    if let Some(err) = result.as_ref().err().cloned() {
        // A model that stopped half-way says what it had already done.
        let (first_error, mut done) = fallback::split_progress(&err);
        if settings.ai_fallback && fallback::worth_another(&first_error) {
            // Earlier turns go across as text; this question keeps its file or
            // image, shaped for the model that takes it.
            let earlier = fallback::transcript(&history[..history.len().saturating_sub(1)]);
            for target in fallback::candidates(settings, &chosen, &fallback::accounts) {
                // Taking over half-way: the question comes with what was done.
                let resumed = (!done.is_empty()).then(|| UserTurn {
                    text: format!("{}{}", turn.text, fallback::carry_on(&done)),
                    file: turn.file.clone(),
                    window: turn.window.clone(),
                });
                let this_turn = resumed.as_ref().unwrap_or(&turn);
                let question = match target.provider {
                    Provider::Anthropic => claude::user_message(this_turn),
                    Provider::OpenAi => openai::user_message(this_turn),
                    Provider::Gemini => gemini::user_message(this_turn),
                    Provider::Local => match local_llm::user_message(this_turn) {
                        Ok(message) => message,
                        Err(_) => continue, // a scanned PDF the local model cannot read
                    },
                };
                let mut asked = fallback::rebuild(target.provider, &earlier);
                asked.push(question);
                crate::log::line(format!("chat: {} could not answer ({first_error}), asking {}", chosen.name(), target.name()));
                let _ = tauri::Emitter::emit_to(app, crate::island::WINDOW_LABEL, "ai-fallback", serde_json::json!({ "name": target.name() }));
                match ask(app, &target, settings, &asked, acts).await {
                    Ok(answer) => {
                        note = Some(format!("{} answered because {} {}.", target.name(), chosen.name(), fallback::reason(&first_error)));
                        // Kept in the chosen provider's format, so the conversation carries on there.
                        result = Ok(fallback::adopt(answer, provider));
                        break;
                    }
                    Err(e) => {
                        let (message, more) = fallback::split_progress(&e);
                        done.extend(more);
                        crate::log::line(format!("chat: {} failed too: {message}", target.name()));
                        if !fallback::worth_another(&message) {
                            break;
                        }
                    }
                }
            }
        }
        if result.is_err() {
            result = if done.is_empty() {
                Err(first_error)
            } else {
                // Nobody could finish, but things were changed: say what, and
                // keep it in the conversation.
                let text = format!("I could not finish: {first_error}\n\nWhat was already done stays done:\n- {}", done.join("\n- "));
                Ok(Answer { stored: fallback::stored_text(provider, &text), text })
            };
        }
    }
    let secs = started.elapsed().as_secs_f32();
    // Cleared while the model was answering: the question is gone already.
    let current = chat.epoch() == epoch;
    match result {
        Ok(answer) => {
            crate::log::line(format!("chat: answered in {secs:.0}s ({} chars)", answer.text.chars().count()));
            if current {
                chat.push(answer.stored);
            }
            Ok(ChatReply { text: plain_text(&answer.text), note })
        }
        Err(err) => {
            crate::log::line(format!("chat: failed after {secs:.0}s: {err}"));
            if current {
                chat.pop(); // keep the history consistent with what the model saw
            }
            Err(err)
        }
    }
}

/// One provider and model answering the conversation as it is.
async fn ask<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    target: &fallback::Target,
    settings: &Settings,
    history: &[Value],
    acts: bool,
) -> Result<Answer, String> {
    let s = target.settings(settings);
    let work = async {
        match target.provider {
            Provider::Local => local_llm::complete(app, &s, history).await,
            cloud if acts => agent::run(app, cloud, &s, history).await,
            Provider::Anthropic => claude::complete(&s.model, history).await,
            Provider::OpenAi => openai::complete(&s.openai_model, history).await,
            Provider::Gemini => gemini::complete(&s.gemini_model, history).await,
        }
    };
    // Every request inside is made with this account's key.
    crate::secrets::on_account(target.account, work).await
}

fn chosen_model(provider: Provider, settings: &Settings) -> String {
    match provider {
        Provider::Anthropic => settings.model.clone(),
        Provider::OpenAi => settings.openai_model.clone(),
        Provider::Gemini => settings.gemini_model.clone(),
        Provider::Local => settings.local_model.clone(),
    }
}

/// The island shows plain text, and some models write Markdown whatever they
/// are told: drop the emphasis and headings, and turn list markers into bullets.
fn plain_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            let line = line.replace("**", "").replace("__", "");
            let indent = line.len() - line.trim_start().len();
            let mut body = line.trim_start();
            // "## Title" is a heading; "#tag" is not.
            if let Some(n) = body.find(|c| c != '#') {
                if (1..=6).contains(&n) && body[n..].starts_with(' ') {
                    body = body[n..].trim_start();
                }
            }
            let body = match body.split_once(' ') {
                Some(("*" | "-" | "+", rest)) => format!("• {}", rest.trim_start()),
                _ => body.to_string(),
            };
            format!("{}{body}", &line[..indent])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The models a provider offers, asked from the provider itself so the list
/// never goes stale.
pub async fn models(provider: Provider, settings: &Settings, start: bool) -> Result<Vec<ModelInfo>, String> {
    match provider {
        Provider::Anthropic => claude::models().await,
        Provider::OpenAi => openai::models().await,
        Provider::Gemini => gemini::models().await,
        Provider::Local => local_llm::models(settings, start).await,
    }
}

/// Small models drift into the language of an attached file whatever the system
/// prompt says. Naming the language right after the question is what holds
/// them: with gemma3, French, Russian and English questions about a Hebrew
/// sheet were all answered in Hebrew until the language was named.
fn detect_language(text: &str) -> Option<&'static str> {
    use whatlang::Script;
    let info = whatlang::detect(text)?;
    // A script only one language writes settles it, however short the text.
    let by_script = match info.script() {
        Script::Hebrew => Some("Hebrew"),
        Script::Greek => Some("Greek"),
        Script::Hangul => Some("Korean"),
        Script::Hiragana | Script::Katakana => Some("Japanese"),
        Script::Thai => Some("Thai"),
        Script::Georgian => Some("Georgian"),
        Script::Armenian => Some("Armenian"),
        _ => None,
    };
    // Below this, guesses on short texts were mostly wrong ("Merci" as
    // Indonesian, "ok" as Hungarian), so a short message names nothing.
    by_script.or_else(|| (info.confidence() >= 0.25).then(|| info.lang().eng_name()))
}

/// `/home/me/Desktop/a.xlsx` as `~/Desktop/a.xlsx`, the form the tools take.
fn home_relative(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

pub fn require_model(model: &str) -> Result<&str, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("Choose a model in Settings.".into());
    }
    Ok(model)
}

/// PDF, image, or text/code inlined as text. Mirrors readFileAsBlock() in
/// ClaudeService.swift.
pub fn read_attachment(path: &str) -> Option<Attachment> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let image = match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    };
    if let Some(media) = image {
        return Some(Attachment::Image { media, data: std::fs::read(path).ok()? });
    }
    let extracted = match ext.as_str() {
        "pdf" => return Some(Attachment::Pdf(std::fs::read(path).ok()?)),
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => extract::spreadsheet(path),
        "docx" | "pptx" | "odt" | "odp" => extract::office(path, &ext),
        _ => {
            if std::fs::metadata(path).ok()?.len() > MAX_INLINE_TEXT {
                return None;
            }
            std::fs::read_to_string(path).ok()
        }
    };
    extracted.map(|text| Attachment::Text(extract::clip(&text, MAX_INLINE_TEXT as usize)))
}

pub fn data_url(media: &str, data: &[u8]) -> String {
    format!("data:{media};base64,{}", base64(data))
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

pub fn client(timeout_secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| e.to_string())
}

pub struct ApiError {
    /// 0 when the request never got an answer.
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn describe(&self, who: &str) -> String {
        if self.status == 0 {
            format!("Network error: {}", self.message)
        } else {
            format!("{who} {}: {}", self.status, self.message)
        }
    }

    /// A 400 that is about the web search tool, which some models refuse.
    pub fn is_tool_refusal(&self) -> bool {
        let m = self.message.to_lowercase();
        self.status == 400 && (m.contains("tool") || m.contains("search"))
    }
}

/// Sends a request and returns the JSON body, or the API's own error message,
/// which is what makes a bad key or a wrong model name obvious.
pub async fn send_json(request: reqwest::RequestBuilder) -> Result<Value, ApiError> {
    let response = request
        .send()
        .await
        .map_err(|e| ApiError { status: 0, message: e.to_string() })?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| ApiError { status: status.as_u16(), message: e.to_string() })?;
    if !status.is_success() {
        return Err(ApiError { status: status.as_u16(), message: error_message(&text) });
    }
    serde_json::from_str(&text).map_err(|e| ApiError {
        status: status.as_u16(),
        message: format!("bad response: {e}"),
    })
}

/// `{"error": {"message": ...}}` (Anthropic, OpenAI, Gemini) or
/// `{"error": "..."}` (Ollama), else the start of the body.
fn error_message(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    error
        .and_then(|e| e.get("message").and_then(Value::as_str).or_else(|| e.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(200).collect())
}

// ── Base64 ────────────────────────────────────────────────────────────────────

/// Small standalone base64 encoder -not worth another dependency. Also used
/// for Stripe's basic auth.
pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn error_messages_come_from_every_provider_shape() {
        assert_eq!(error_message(r#"{"error":{"message":"bad key"}}"#), "bad key");
        assert_eq!(error_message(r#"{"error":"model not found"}"#), "model not found");
        assert_eq!(error_message("plain failure"), "plain failure");
    }

    #[test]
    fn switching_provider_starts_a_new_conversation() {
        let chat = Chat::default();
        chat.begin(Provider::Anthropic);
        chat.push(serde_json::json!({"role": "user"}));
        chat.begin(Provider::Anthropic);
        assert!(!chat.is_empty(), "same provider keeps the history");
        chat.begin(Provider::Local);
        assert!(chat.is_empty(), "another provider cannot read this history");
    }

    #[test]
    fn the_reply_language_is_named_when_it_can_be_told() {
        assert_eq!(detect_language("מה קורה?"), Some("Hebrew"));
        assert_eq!(detect_language("このファイルには何がありますか"), Some("Japanese"));
        assert_eq!(detect_language("Qu'est-ce qu'il y a dans ce fichier ?"), Some("French"));
        assert_eq!(detect_language("Что в этом файле?"), Some("Russian"));
        assert_eq!(detect_language("what's in this file?"), Some("English"));
        assert_eq!(detect_language("ok"), None);
    }

    #[test]
    fn a_short_follow_up_keeps_the_conversations_language() {
        let chat = Chat::default();
        assert_eq!(chat.reply_language("¿Qué hay en este archivo?"), Some("Spanish"));
        assert_eq!(chat.reply_language("ok"), Some("Spanish"));
        chat.reset();
        assert_eq!(chat.reply_language("ok"), None);
    }

    #[test]
    fn markdown_becomes_plain_text() {
        let md = "## סיכום\n*   **שם השירות:** נטפליקס\n- second\n#tag stays\n  + nested";
        assert_eq!(plain_text(md), "סיכום\n• שם השירות: נטפליקס\n• second\n#tag stays\n  • nested");
    }

    #[test]
    fn an_answer_to_a_cleared_conversation_is_not_kept() {
        let chat = Chat::default();
        chat.push(serde_json::json!({ "role": "user" }));
        let epoch = chat.epoch();
        chat.reset();
        assert_ne!(chat.epoch(), epoch, "send() checks this before storing the answer");
    }

    #[test]
    fn a_dropped_file_tells_an_acting_model_where_it_came_from() {
        let home = std::env::var("HOME").unwrap();
        let note = FileNote { name: "a.xlsx".into(), content: Some(Attachment::Text("x".into())), origin: Some(home_relative(&format!("{home}/Desktop/a.xlsx"))) };
        assert_eq!(note.label(), "File: a.xlsx (dropped from ~/Desktop/a.xlsx)");
        assert_eq!(home_relative("/etc/hosts"), "/etc/hosts");
    }

    #[test]
    fn unknown_providers_fall_back_to_claude() {
        assert_eq!(Provider::parse("openai"), Provider::OpenAi);
        assert_eq!(Provider::parse(""), Provider::Anthropic);
        assert_eq!(Provider::parse("something-new"), Provider::Anthropic);
    }
}




