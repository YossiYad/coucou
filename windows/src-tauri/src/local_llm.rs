// A model running on this machine, through the OpenAI-compatible chat API that
// Ollama, LM Studio, llama.cpp and vLLM all serve. No key, no web search, and
// nothing leaves the computer. Images reach vision models; PDFs are refused,
// since none of these servers read them.

use serde_json::{json, Value};

use crate::ai::{self, Answer, Attachment, ModelInfo, UserTurn};
use crate::settings::Settings;
use crate::{extract, local_server, tools};
use tauri::{AppHandle, Runtime};

pub const DEFAULT_URL: &str = "http://localhost:11434";

/// A CPU-only model can take a while on a long answer.
const TIMEOUT_SECS: u64 = 300;

/// Local models have small context windows (Ollama defaults to 4096 tokens and
/// silently drops the start of anything longer), so a file is cut well before.
const MAX_FILE_CHARS: usize = 24_000;

/// `http://localhost:11434` → `http://localhost:11434/v1`; a URL that already
/// has a path (`.../v1`, `.../api/v1`) is used as given.
pub fn base_url(raw: &str) -> Result<String, String> {
    let url = raw.trim().trim_end_matches('/');
    let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) else {
        return Err("The local server address must start with http:// or https://.".into());
    };
    if rest.is_empty() {
        return Err("The local server address is empty.".into());
    }
    Ok(if rest.contains('/') { url.to_string() } else { format!("{url}/v1") })
}

pub fn user_message(turn: &UserTurn) -> Result<Value, String> {
    let mut texts: Vec<String> = Vec::new();
    let mut image: Option<String> = None;
    if let Some(file) = &turn.file {
        match &file.content {
            Some(Attachment::Image { media, data }) => image = Some(ai::data_url(media, data)),
            // These servers take no PDFs, so the model gets the PDF's text.
            Some(Attachment::Pdf(data)) => match extract::pdf_text(data) {
                Some(text) => texts.push(format!("File contents:\n{}", extract::clip(&text, MAX_FILE_CHARS))),
                None => {
                    return Err("This PDF has no text in it (probably a scan), and local models can't read the file itself. \
                        Switch to Claude, ChatGPT or Gemini for this one."
                        .into())
                }
            },
            Some(Attachment::Text(text)) => {
                texts.push(format!("File contents:\n{}", extract::clip(text, MAX_FILE_CHARS)))
            }
            None => {}
        }
        texts.push(file.label());
    }
    if let Some(window) = &turn.window {
        texts.push(window.clone());
    }
    texts.push(turn.text.clone());

    // Plain string content unless there is an image: some servers accept
    // nothing else.
    let content = match image {
        None => json!(texts.join("\n\n")),
        Some(url) => {
            let mut parts = vec![json!({ "type": "image_url", "image_url": { "url": url } })];
            parts.extend(texts.into_iter().map(|t| json!({ "type": "text", "text": t })));
            json!(parts)
        }
    };
    Ok(json!({ "role": "user", "content": content }))
}

pub async fn complete<R: Runtime>(app: &AppHandle<R>, settings: &Settings, history: &[Value]) -> Result<Answer, String> {
    let base = base_url(&settings.local_url)?;
    let model = ai::require_model(&settings.local_model)?;
    local_server::ensure_running(&base, settings).await?;
    if settings.tools_enabled {
        // None: this model cannot use tools, so it answers as a plain chat.
        if let Some(answer) = agent(app, settings, &base, model, history).await? {
            return Ok(answer);
        }
    }
    let mut messages = vec![json!({ "role": "system", "content": ai::system_prompt(false) })];
    messages.extend_from_slice(history);
    let body = json!({
        "model": model,
        "messages": messages,
        "stream": false,
        // Qwen3 otherwise thinks for minutes and can loop until the server aborts.
        "reasoning_effort": "none",
    });
    let request = ai::client(TIMEOUT_SECS)?
        .post(format!("{base}/chat/completions"))
        .json(&body);
    let response = ai::send_json(request).await;
    // A long answer must not count as idle time.
    local_server::touch();
    parse(&response.map_err(|e| describe(&e, &base, settings))?)
}

/// A request may take this many rounds of tool use before it must answer.
const MAX_STEPS: usize = 12;

/// The model works with the tools in tools.rs until it has an answer: each
/// round it either answers or asks for tools, whose results go back to it.
async fn agent<R: Runtime>(
    app: &AppHandle<R>,
    settings: &Settings,
    base: &str,
    model: &str,
    history: &[Value],
) -> Result<Option<Answer>, String> {
    let mode = tools::Mode::parse(&settings.permission_mode);
    let mut messages = vec![json!({ "role": "system", "content": crate::agent::prompt(false, mode) })];
    messages.extend_from_slice(history);
    let definitions = tools::definitions(mode);
    let mut used: Vec<String> = Vec::new();
    // Whether the last tool call failed, and whether the model was already
    // pushed once to act on what it announced.
    let mut last_failed = false;
    let mut nudged = false;

    for _ in 0..MAX_STEPS {
        // Qwen3 sometimes writes a tool call the server cannot parse, and the
        // reply comes back empty. Sampling again usually fixes it; thinking is
        // the last resort, since it tends to garble the data it passes on. A low
        // temperature keeps tool calls well formed. Thinking can loop until the
        // server aborts it, and then the fast way is tried again.
        let mut attempt = 0;
        let response = loop {
            attempt += 1;
            let thinking = attempt == 3;
            let body = json!({
                "model": model,
                "messages": messages,
                "tools": definitions,
                "stream": false,
                "temperature": 0.3,
                "reasoning_effort": if thinking { "low" } else { "none" },
            });
            let request = ai::client(TIMEOUT_SECS)?.post(format!("{base}/chat/completions")).json(&body);
            match ai::send_json(request).await {
                Err(e) if e.message.contains("does not support tools") => return Ok(None),
                Err(e) if e.message.contains("repeat limit") && attempt < 4 => {
                    crate::log::line("agent: the model looped, asking again");
                }
                Err(e) => return Err(describe(&e, base, settings)),
                Ok(v) if is_empty_reply(&v) && attempt < 3 => {
                    crate::log::line(format!("agent: empty reply, asking again ({})", if attempt == 2 { "with thinking" } else { "again" }));
                }
                Ok(v) => break v,
            }
        };
        local_server::touch();

        let message = response
            .pointer("/choices/0/message")
            .cloned()
            .ok_or_else(|| "Unexpected response from the local model.".to_string())?;
        let calls = message.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        if calls.is_empty() {
            let mut answer = parse(&response)?;
            // Qwen3 often answers a failed step with "Let me try another way"
            // and stops there. Once per turn, it is told to go ahead.
            if last_failed && !nudged && announces_more(&answer.text) {
                nudged = true;
                crate::log::line("agent: the model announced a step without taking it, asking it to");
                messages.push(json!({ "role": "assistant", "content": answer.text }));
                messages.push(json!({ "role": "user", "content": "Go ahead and do that now with the tools." }));
                continue;
            }
            if used.is_empty() {
                crate::log::line("agent: answered without using tools");
            } else {
                // The next turn needs to know what was done, not the bulk of it.
                answer.stored = json!({
                    "role": "assistant",
                    "content": format!("{}\n\n[Tools used: {}]", answer.text, used.join("; ")),
                });
            }
            return Ok(Some(answer));
        }

        messages.push(json!({
            "role": "assistant",
            "content": message.get("content").cloned().unwrap_or_else(|| json!("")),
            "tool_calls": calls,
        }));
        for call in &calls {
            let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
            // Arguments arrive as a JSON string from some servers, an object from others.
            let args = match call.pointer("/function/arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                Some(v) => v.clone(),
                None => json!({}),
            };
            let output = tools::run(app, name, &args, mode).await;
            let result = output.text;
            last_failed = result.starts_with("Error:") || result.starts_with("Failed with exit code");
            let outcome = if result.starts_with("Error:") { result.as_str() } else { "ok" };
            crate::log::line(format!("tool: {name} {} -> {outcome}", crate::agent::summary(&args)));
            local_server::touch();
            used.push(format!("{name} {}", crate::agent::summary(&args)));
            let mut reply = json!({ "role": "tool", "name": name, "content": result });
            if let Some(id) = call.get("id") {
                reply["tool_call_id"] = id.clone();
            }
            messages.push(reply);
            if let Some(picture) = output.image {
                // Only a vision model makes anything of it; others read the text above.
                messages.push(json!({
                    "role": "user",
                    "content": [{ "type": "image_url", "image_url": { "url": picture.data_url() } }],
                }));
            }
        }
    }
    Err("That took too many steps. Try asking for something narrower.".into())
}

/// No text and no tool call: what is left of a tool call the server could not parse.
/// An answer that ends by saying what it will do next instead of doing it.
fn announces_more(text: &str) -> bool {
    let lower = text.to_lowercase();
    let tail: String = lower.chars().rev().take(240).collect::<Vec<_>>().into_iter().rev().collect();
    ["let me ", "i'll ", "i will ", "i'm going to ", "i am going to ", "אנסה", "אריץ", "אבדוק", "בוא נ"]
        .iter()
        .any(|p| tail.contains(p))
}

fn is_empty_reply(response: &Value) -> bool {
    let message = response.pointer("/choices/0/message");
    let text = message.and_then(|m| m.get("content")).and_then(Value::as_str).unwrap_or("");
    let calls = message.and_then(|m| m.get("tool_calls")).and_then(Value::as_array).is_some_and(|c| !c.is_empty());
    strip_thinking(text).trim().is_empty() && !calls
}



fn describe(err: &ai::ApiError, base: &str, settings: &Settings) -> String {
    if err.status != 0 {
        return err.describe("Local model");
    }
    if settings.local_start_command.trim().is_empty() {
        format!("Can't reach the local model server at {base}. Start it, or set a start command in Settings.")
    } else {
        format!("Can't reach the local model server at {base}.")
    }
}

fn parse(response: &Value) -> Result<Answer, String> {
    let content = response
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| "Unexpected response from the local model.".to_string())?;
    let text = strip_thinking(content).trim().to_string();
    if text.is_empty() {
        return Err("No response text.".into());
    }
    Ok(Answer { stored: json!({ "role": "assistant", "content": text }), text })
}

/// Reasoning models (Qwen3, DeepSeek-R1...) put their thinking in the answer
/// between <think> tags.
fn strip_thinking(content: &str) -> String {
    let mut out = content.to_string();
    while let Some(start) = out.find("<think>") {
        match out[start..].find("</think>") {
            Some(end) => out.replace_range(start..start + end + "</think>".len(), ""),
            None => {
                out.truncate(start);
                break;
            }
        }
    }
    out
}

/// Shown in Settings while a server Coucou can start by itself is off.
pub const SERVER_OFF: &str =
    "The local server is off. It starts by itself when you ask something; Refresh starts it now.";

/// `start`: only an explicit Refresh may start the server. Opening Settings
/// (which also happens, unseen, at launch) must not wake it up.
pub async fn models(settings: &Settings, start: bool) -> Result<Vec<ModelInfo>, String> {
    let base = base_url(&settings.local_url)?;
    if start {
        local_server::ensure_running(&base, settings).await?;
    } else if !settings.local_start_command.trim().is_empty() && !local_server::reachable(&base).await {
        return Err(SERVER_OFF.into());
    }
    let request = ai::client(10)?.get(format!("{base}/models"));
    let response = ai::send_json(request).await.map_err(|e| describe(&e, &base, settings))?;
    Ok(response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .map(|id| ModelInfo { id: id.to_string(), label: id.to_string() })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_announced_next_step_is_recognised() {
        assert!(announces_more("It seems there was an issue. I'll use flatpak remote-ls instead. Let me proceed with that."));
        assert!(announces_more("הפקודה נכשלה, אנסה דרך אחרת."));
        assert!(!announces_more("Done: two Flatpak apps have updates, Firefox and Steam."));
    }

    #[test]
    fn server_addresses_are_normalised() {
        assert_eq!(base_url("http://localhost:11434").unwrap(), "http://localhost:11434/v1");
        assert_eq!(base_url(" http://localhost:1234/v1/ ").unwrap(), "http://localhost:1234/v1");
        assert_eq!(base_url("https://box.lan/api/v1").unwrap(), "https://box.lan/api/v1");
        assert!(base_url("localhost:11434").is_err());
        assert!(base_url("file:///etc/passwd").is_err());
        assert!(base_url("http://").is_err());
    }

    #[test]
    fn thinking_is_removed_from_the_answer() {
        assert_eq!(strip_thinking("<think>hmm</think>Hello"), "Hello");
        assert_eq!(strip_thinking("A<think>x</think>B<think>y</think>C"), "ABC");
        assert_eq!(strip_thinking("Answer<think>never closed"), "Answer");
    }

    #[test]
    fn text_goes_as_a_plain_string_and_pdfs_are_refused() {
        let turn = UserTurn { text: "hi".into(), file: None, window: None };
        assert_eq!(user_message(&turn).unwrap()["content"], "hi");

        let pdf = UserTurn {
            text: "read".into(),
            file: Some(ai::FileNote { name: "a.pdf".into(), origin: None, content: Some(Attachment::Pdf(vec![])) }),
            window: None,
        };
        assert!(user_message(&pdf).is_err());
    }

    #[test]
    fn an_image_switches_to_content_parts() {
        let turn = UserTurn {
            text: "what is it".into(),
            file: Some(ai::FileNote {
                name: "x.jpg".into(),
                origin: None,
                content: Some(Attachment::Image { media: "image/jpeg", data: vec![9] }),
            }),
            window: None,
        };
        let content = &user_message(&turn).unwrap()["content"];
        assert_eq!(content[0]["type"], "image_url");
        assert_eq!(content[2]["text"], "what is it");
    }
}

