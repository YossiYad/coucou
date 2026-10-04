// Gemini through Google's own generateContent API rather than its OpenAI
// compatibility layer: that is the one that reads PDFs and searches with Google.

use serde_json::{json, Value};

use crate::ai::{self, Answer, Attachment, ModelInfo, UserTurn};
use crate::secrets;

const BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

fn key() -> Result<String, String> {
    secrets::get("gemini-api-key").ok_or_else(|| "Gemini API key missing. Open settings.".into())
}

pub fn user_message(turn: &UserTurn) -> Value {
    let mut parts: Vec<Value> = Vec::new();
    if let Some(file) = &turn.file {
        match &file.content {
            Some(Attachment::Image { media, data }) => parts.push(inline(media, data)),
            Some(Attachment::Pdf(data)) => parts.push(inline("application/pdf", data)),
            Some(Attachment::Text(text)) => parts.push(json!({ "text": format!("File contents:\n{text}") })),
            None => {}
        }
        parts.push(json!({ "text": file.label() }));
    }
    if let Some(window) = &turn.window {
        parts.push(json!({ "text": window }));
    }
    parts.push(json!({ "text": turn.text }));
    json!({ "role": "user", "parts": parts })
}

fn inline(media: &str, data: &[u8]) -> Value {
    json!({ "inline_data": { "mime_type": media, "data": ai::base64(data) } })
}

pub async fn complete(model: &str, history: &[Value]) -> Result<Answer, String> {
    let key = key()?;
    let model = model_path(ai::require_model(model)?)?;
    let mut body = json!({
        "systemInstruction": { "parts": [{ "text": ai::system_prompt(true) }] },
        "contents": history,
        "tools": [{ "google_search": {} }],
    });
    let response = match post(&key, &model, &body).await {
        // Not every model can search, and a free key runs out of search
        // quota while plain answers still work: ask again without grounding.
        Err(err) if search_refused(&err) => {
            body.as_object_mut().unwrap().remove("tools");
            body["systemInstruction"]["parts"][0]["text"] = json!(ai::system_prompt(false));
            post(&key, &model, &body).await
        }
        other => other,
    }
    .map_err(|e| e.describe("Gemini API"))?;
    parse(&response)
}

async fn post(key: &str, model: &str, body: &Value) -> Result<Value, ai::ApiError> {
    let client = ai::client(120).map_err(|message| ai::ApiError { status: 0, message })?;
    let url = format!("{BASE}/models/{model}:generateContent");
    let mut waits = RETRY_WAITS.iter();
    loop {
        let result = ai::send_json(client.post(&url).header("x-goog-api-key", key).json(body)).await;
        match (result, waits.next()) {
            (Err(err), Some(&secs)) if overloaded(&err) => {
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
            (result, _) => return result,
        }
    }
}

/// Google answers 503 "high demand" in short spikes; the same request usually
/// goes through seconds later.
const RETRY_WAITS: &[u64] = &[2, 5];

fn overloaded(err: &ai::ApiError) -> bool {
    err.status == 503 || err.status == 500
}

/// A 429 on a grounded request is Google's search quota, which a free key
/// has far less of than plain answers.
fn search_refused(err: &ai::ApiError) -> bool {
    err.is_tool_refusal() || err.status == 429
}

/// The model id goes into the URL path, so only plain id characters pass.
fn model_path(model: &str) -> Result<String, String> {
    let id = model.strip_prefix("models/").unwrap_or(model);
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err(format!("\"{model}\" is not a Gemini model name."));
    }
    Ok(id.to_string())
}

fn parse(response: &Value) -> Result<Answer, String> {
    if let Some(reason) = response
        .get("promptFeedback")
        .and_then(|f| f.get("blockReason"))
        .and_then(Value::as_str)
    {
        return Err(format!("Gemini declined this request ({reason})."));
    }
    let candidate = response
        .get("candidates")
        .and_then(|c| c.get(0))
        .ok_or_else(|| "Unexpected API response.".to_string())?;

    let text = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| !p.get("thought").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string();

    if text.is_empty() {
        return match candidate.get("finishReason").and_then(Value::as_str) {
            Some(reason) if reason != "STOP" => Err(format!("Gemini stopped without an answer ({reason}).")),
            _ => Err("No response text.".into()),
        };
    }
    // Kept as returned, thought signatures included, as Google asks for
    // multi-turn chats.
    let mut stored = candidate.get("content").cloned().unwrap_or_else(|| json!({ "parts": [] }));
    stored["role"] = json!("model");
    Ok(Answer { stored, text })
}

pub async fn models() -> Result<Vec<ModelInfo>, String> {
    let key = key()?;
    let request = ai::client(20)?
        .get(format!("{BASE}/models?pageSize=1000"))
        .header("x-goog-api-key", &key);
    let response = ai::send_json(request).await.map_err(|e| e.describe("Gemini API"))?;
    Ok(response
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| {
            m.get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .is_some_and(|methods| methods.iter().any(|x| x == "generateContent"))
        })
        .filter_map(|m| {
            let id = m.get("name")?.as_str()?.strip_prefix("models/")?.to_string();
            const NOT_CHAT: &[&str] = &["tts", "image", "embedding", "aqa"];
            if NOT_CHAT.iter().any(|word| id.contains(word)) {
                return None;
            }
            let label = m.get("displayName").and_then(Value::as_str).unwrap_or(&id).to_string();
            Some(ModelInfo { id, label })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_cannot_escape_the_url_path() {
        assert_eq!(model_path("models/gemini-2.5-flash").unwrap(), "gemini-2.5-flash");
        assert!(model_path("../../evil").is_err());
        assert!(model_path("gemini?key=x").is_err());
    }

    #[test]
    fn a_search_quota_or_refusal_falls_back_to_plain_answers() {
        let err = |status, message: &str| ai::ApiError { status, message: message.into() };
        assert!(search_refused(&err(429, "You exceeded your current quota")));
        assert!(search_refused(&err(400, "Search grounding is not supported")));
        assert!(!search_refused(&err(400, "API key not valid")));
        assert!(!search_refused(&err(403, "permission denied")));
    }

    #[test]
    fn only_a_busy_server_is_asked_again() {
        let err = |status| ai::ApiError { status, message: String::new() };
        assert!(overloaded(&err(503)));
        assert!(overloaded(&err(500)));
        assert!(!overloaded(&err(429)));
        assert!(!overloaded(&err(400)));
        assert!(!overloaded(&err(0)));
    }

    #[test]
    fn thoughts_are_hidden_and_the_reply_kept_as_the_model_turn() {
        let response = json!({ "candidates": [{
            "content": { "parts": [
                { "text": "thinking...", "thought": true },
                { "text": "Paris.", "thoughtSignature": "abc" }
            ]},
            "finishReason": "STOP"
        }]});
        let answer = parse(&response).ok().unwrap();
        assert_eq!(answer.text, "Paris.");
        assert_eq!(answer.stored["role"], "model");
        assert_eq!(answer.stored["parts"][1]["thoughtSignature"], "abc");
    }

    #[test]
    fn a_blocked_prompt_or_a_safety_stop_is_explained() {
        let blocked = json!({ "promptFeedback": { "blockReason": "SAFETY" } });
        assert_eq!(parse(&blocked).err().unwrap(), "Gemini declined this request (SAFETY).");
        let stopped = json!({ "candidates": [{ "finishReason": "RECITATION" }] });
        assert_eq!(parse(&stopped).err().unwrap(), "Gemini stopped without an answer (RECITATION).");
    }

    #[test]
    fn files_go_inline_with_their_mime_type() {
        let turn = UserTurn {
            text: "describe".into(),
            file: Some(ai::FileNote {
                name: "cat.png".into(),
                origin: None,
                content: Some(Attachment::Image { media: "image/png", data: vec![1, 2, 3] }),
            }),
            window: None,
        };
        let message = user_message(&turn);
        assert_eq!(message["parts"][0]["inline_data"]["mime_type"], "image/png");
        assert_eq!(message["parts"][2]["text"], "describe");
    }
}
