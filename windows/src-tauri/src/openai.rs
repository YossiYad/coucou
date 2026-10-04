// OpenAI through the Responses API: web search, images and PDFs, like the
// Claude chat. Nothing is stored on OpenAI's side (`store: false`); the history
// is sent back each turn.

use serde_json::{json, Value};

use crate::ai::{self, Answer, Attachment, ModelInfo, UserTurn};
use crate::secrets;

const ENDPOINT: &str = "https://api.openai.com/v1/responses";
const MODELS_ENDPOINT: &str = "https://api.openai.com/v1/models";

fn key() -> Result<String, String> {
    secrets::get("openai-api-key").ok_or_else(|| "OpenAI API key missing. Open settings.".into())
}

pub fn user_message(turn: &UserTurn) -> Value {
    let mut content: Vec<Value> = Vec::new();
    if let Some(file) = &turn.file {
        match &file.content {
            Some(Attachment::Image { media, data }) => content.push(json!({
                "type": "input_image",
                "image_url": ai::data_url(media, data),
            })),
            Some(Attachment::Pdf(data)) => content.push(json!({
                "type": "input_file",
                "filename": file.name,
                "file_data": ai::data_url("application/pdf", data),
            })),
            Some(Attachment::Text(text)) => content.push(json!({
                "type": "input_text",
                "text": format!("File contents:\n{text}"),
            })),
            None => {}
        }
        content.push(json!({ "type": "input_text", "text": file.label() }));
    }
    if let Some(window) = &turn.window {
        content.push(json!({ "type": "input_text", "text": window }));
    }
    content.push(json!({ "type": "input_text", "text": turn.text }));
    json!({ "role": "user", "content": content })
}

pub async fn complete(model: &str, history: &[Value]) -> Result<Answer, String> {
    let key = key()?;
    let model = ai::require_model(model)?;
    let mut body = json!({
        "model": model,
        "instructions": ai::system_prompt(true),
        "input": history,
        "tools": [{ "type": "web_search" }],
        "store": false,
    });
    let response = match post(&key, &body).await {
        // Not every model can search the web: ask again without the tool.
        Err(err) if err.is_tool_refusal() => {
            body.as_object_mut().unwrap().remove("tools");
            body["instructions"] = json!(ai::system_prompt(false));
            post(&key, &body).await
        }
        other => other,
    }
    .map_err(|e| e.describe("OpenAI API"))?;
    parse(&response)
}

async fn post(key: &str, body: &Value) -> Result<Value, ai::ApiError> {
    let client = ai::client(120).map_err(|message| ai::ApiError { status: 0, message })?;
    ai::send_json(client.post(ENDPOINT).bearer_auth(key).json(body)).await
}

fn parse(response: &Value) -> Result<Answer, String> {
    if let Some(message) = response
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return Err(message.to_string());
    }

    let text = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() {
        if response.get("status").and_then(Value::as_str) == Some("incomplete") {
            let reason = response
                .get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("unknown reason");
            return Err(format!("The answer was cut short ({reason})."));
        }
        return Err("No response text.".into());
    }
    Ok(Answer { stored: json!({ "role": "assistant", "content": text }), text })
}

pub async fn models() -> Result<Vec<ModelInfo>, String> {
    let key = key()?;
    let request = ai::client(20)?.get(MODELS_ENDPOINT).bearer_auth(&key);
    let response = ai::send_json(request).await.map_err(|e| e.describe("OpenAI API"))?;
    let mut models: Vec<(i64, String)> = response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?;
            is_chat_model(id).then(|| (m.get("created").and_then(Value::as_i64).unwrap_or(0), id.to_string()))
        })
        .collect();
    models.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(models.into_iter().map(|(_, id)| ModelInfo { label: id.clone(), id }).collect())
}

/// The list also holds embedding, speech, image and moderation models, which
/// the Responses API cannot chat with.
fn is_chat_model(id: &str) -> bool {
    let id = id.to_lowercase();
    let reasoning = id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit());
    let family = id.starts_with("gpt-") || id.starts_with("chatgpt-") || reasoning;
    const NOT_CHAT: &[&str] = &[
        "audio", "realtime", "transcribe", "tts", "image", "embedding", "moderation", "search", "instruct",
    ];
    family && !NOT_CHAT.iter().any(|word| id.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_chat_models_are_offered() {
        for id in ["gpt-5", "gpt-4.1-mini", "o3", "o4-mini", "chatgpt-4o-latest"] {
            assert!(is_chat_model(id), "{id} should be offered");
        }
        for id in [
            "text-embedding-3-large", "whisper-1", "dall-e-3", "gpt-4o-mini-tts",
            "gpt-4o-realtime-preview", "gpt-image-1", "omni-moderation-latest",
            "gpt-4o-search-preview", "gpt-3.5-turbo-instruct",
        ] {
            assert!(!is_chat_model(id), "{id} should be hidden");
        }
    }

    #[test]
    fn output_text_is_collected_past_search_calls() {
        let response = json!({
            "status": "completed",
            "output": [
                { "type": "web_search_call", "status": "completed" },
                { "type": "message", "role": "assistant", "content": [
                    { "type": "output_text", "text": "It is sunny.", "annotations": [] }
                ]}
            ]
        });
        let answer = parse(&response).ok().unwrap();
        assert_eq!(answer.text, "It is sunny.");
        assert_eq!(answer.stored, json!({ "role": "assistant", "content": "It is sunny." }));
    }

    #[test]
    fn a_cut_short_answer_says_why() {
        let response = json!({
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": [{ "type": "reasoning" }]
        });
        assert_eq!(parse(&response).err().unwrap(), "The answer was cut short (max_output_tokens).");
    }

    #[test]
    fn a_pdf_is_sent_as_an_input_file() {
        let turn = UserTurn {
            text: "what is this".into(),
            file: Some(ai::FileNote { name: "r.pdf".into(), origin: None, content: Some(Attachment::Pdf(b"%PDF".to_vec())) }),
            window: None,
        };
        let message = user_message(&turn);
        let file = &message["content"][0];
        assert_eq!(file["type"], "input_file");
        assert_eq!(file["filename"], "r.pdf");
        assert!(file["file_data"].as_str().unwrap().starts_with("data:application/pdf;base64,"));
    }
}
