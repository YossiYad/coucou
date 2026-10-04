// Claude through the Messages API - the same integration as ClaudeService.swift:
// multi-turn chat with web search, PDFs as documents, images as images.

use serde_json::{json, Value};

use crate::ai::{self, Answer, Attachment, ModelInfo, UserTurn};
use crate::secrets;

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const MODELS_ENDPOINT: &str = "https://api.anthropic.com/v1/models?limit=100";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server-side fallback: on a policy decline the API retries the same request on
/// a fallback model inside the same call, so the island never shows a dead end.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 4096;

pub const DEFAULT_MODEL: &str = "claude-opus-5";

fn key() -> Result<String, String> {
    secrets::get("anthropic-api-key").ok_or_else(|| "Claude API key missing. Open settings.".into())
}

pub fn user_message(turn: &UserTurn) -> Value {
    let mut content: Vec<Value> = Vec::new();
    if let Some(file) = &turn.file {
        match &file.content {
            Some(Attachment::Image { media, data }) => content.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media, "data": ai::base64(data) },
            })),
            Some(Attachment::Pdf(data)) => content.push(json!({
                "type": "document",
                "source": { "type": "base64", "media_type": "application/pdf", "data": ai::base64(data) },
            })),
            Some(Attachment::Text(text)) => {
                content.push(json!({ "type": "text", "text": format!("File contents:\n{text}") }))
            }
            None => {}
        }
        content.push(json!({ "type": "text", "text": file.label() }));
    }
    if let Some(window) = &turn.window {
        content.push(json!({ "type": "text", "text": window }));
    }
    content.push(json!({ "type": "text", "text": turn.text }));
    json!({ "role": "user", "content": content })
}

pub async fn complete(model: &str, history: &[Value]) -> Result<Answer, String> {
    let key = key()?;
    let body = json!({
        "model": ai::require_model(model)?,
        "max_tokens": MAX_TOKENS,
        "system": ai::system_prompt(true),
        "tools": [{ "type": "web_search_20260209", "name": "web_search", "max_uses": 5 }],
        "fallbacks": "default",
        "messages": history,
    });
    let request = ai::client(90)?
        .post(ENDPOINT)
        .header("x-api-key", &key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", FALLBACK_BETA)
        .json(&body);
    let response = ai::send_json(request).await.map_err(|e| e.describe("Claude API"))?;
    parse(&response)
}

fn parse(response: &Value) -> Result<Answer, String> {
    // A policy decline comes back as HTTP 200 with stop_reason "refusal".
    if response.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        let why = response
            .get("stop_details")
            .and_then(|d| d.get("explanation"))
            .and_then(Value::as_str)
            .unwrap_or("Claude declined this one.");
        return Err(why.to_string());
    }

    let Some(blocks) = response.get("content").and_then(Value::as_array) else {
        return Err("Unexpected API response.".into());
    };
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("No response text.".into());
    }
    // The whole content - tool_use / tool_result blocks included - so the next
    // turn has the right context.
    Ok(Answer { stored: json!({ "role": "assistant", "content": blocks }), text })
}

pub async fn models() -> Result<Vec<ModelInfo>, String> {
    let key = key()?;
    let request = ai::client(20)?
        .get(MODELS_ENDPOINT)
        .header("x-api-key", &key)
        .header("anthropic-version", ANTHROPIC_VERSION);
    let response = ai::send_json(request).await.map_err(|e| e.describe("Claude API"))?;
    Ok(response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let label = m.get("display_name").and_then(Value::as_str).unwrap_or(&id).to_string();
            Some(ModelInfo { id, label })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::FileNote;

    #[test]
    fn a_pdf_rides_along_as_a_document_before_the_question() {
        let turn = UserTurn {
            text: "summarise".into(),
            file: Some(FileNote { name: "a.pdf".into(), origin: None, content: Some(Attachment::Pdf(b"%PDF".to_vec())) }),
            window: None,
        };
        let message = user_message(&turn);
        let content = message["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "document");
        assert_eq!(content[1]["text"], "File: a.pdf");
        assert_eq!(content[2]["text"], "summarise");
    }

    #[test]
    fn a_refusal_is_an_error_with_its_explanation() {
        let response = json!({
            "stop_reason": "refusal",
            "stop_details": { "explanation": "Not this one." },
        });
        assert_eq!(parse(&response).err().unwrap(), "Not this one.");
    }

    #[test]
    fn text_blocks_are_joined_and_tool_blocks_kept_in_the_history() {
        let response = json!({ "content": [
            { "type": "server_tool_use", "id": "x" },
            { "type": "text", "text": "Hello" },
            { "type": "text", "text": "there" },
        ]});
        let answer = parse(&response).ok().unwrap();
        assert_eq!(answer.text, "Hello\nthere");
        assert_eq!(answer.stored["content"].as_array().unwrap().len(), 3);
    }
}
