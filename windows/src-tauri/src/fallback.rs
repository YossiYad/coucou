// When the chosen model cannot answer - its quota is used up, it is
// overloaded, the network or the local server is down, its key is missing -
// the question goes to the next model or account this computer can use,
// instead of ending in an error: the same model on the user's other accounts,
// then Gemini's light model (a quota of its own) on each account, then every
// other cloud provider on each of its accounts, and the local model last,
// since it has to start and is the slowest.
//
// Each provider keeps the conversation in its own format, so the fallback gets
// it as plain text: what was said, without attached images or PDFs.

use serde_json::{json, Value};

use crate::ai::{Answer, Provider, UserTurn};
use crate::settings::Settings;
use crate::{claude, gemini, local_llm, openai, secrets};

/// Gemini's light model: a separate, larger free quota than the flash models.
pub const GEMINI_LIGHT: &str = "gemini-flash-lite-latest";

/// A model to ask: the provider, the model setting it runs with, and which of
/// the user's accounts (API keys) pays for it, 1 being the main one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub provider: Provider,
    pub model: String,
    pub account: u8,
}

impl Target {
    pub fn new(provider: Provider, model: &str, account: u8) -> Self {
        Self { provider, model: model.to_string(), account }
    }

    pub fn name(&self) -> String {
        let model = self.model_name();
        if self.account > 1 {
            format!("{model} (account {})", self.account)
        } else {
            model
        }
    }

    fn model_name(&self) -> String {
        match self.provider {
            Provider::Anthropic => "Claude".into(),
            Provider::OpenAi => "ChatGPT".into(),
            Provider::Gemini if self.model == GEMINI_LIGHT => "Gemini Flash-Lite".into(),
            Provider::Gemini => "Gemini".into(),
            Provider::Local => self.model.split(':').next().unwrap_or("the local model").to_string(),
        }
    }

    /// The settings with this target's model in place.
    pub fn settings(&self, base: &Settings) -> Settings {
        let mut s = base.clone();
        match self.provider {
            Provider::Anthropic => s.model = self.model.clone(),
            Provider::OpenAi => s.openai_model = self.model.clone(),
            Provider::Gemini => s.gemini_model = self.model.clone(),
            Provider::Local => s.local_model = self.model.clone(),
        }
        s
    }
}

/// An error another model could get past. A refused request, a bad file or a
/// safety decline would fail the same way anywhere.
pub fn worth_another(err: &str) -> bool {
    let e = err.to_lowercase();
    [
        " 429", "quota", "rate limit", "too many requests", " 500", " 502", " 503", " 504", " 529", "overloaded",
        "high demand", "unavailable", "network error", "can't reach the local", "local server is off", "key missing",
        "timed out", "timeout",
    ]
    .iter()
    .any(|p| e.contains(p))
}

/// The accounts with a key stored for a provider, main one first; the local
/// model needs none.
pub fn accounts(provider: Provider) -> Vec<u8> {
    match provider {
        Provider::Anthropic => secrets::accounts("anthropic-api-key"),
        Provider::OpenAi => secrets::accounts("openai-api-key"),
        Provider::Gemini => secrets::accounts("gemini-api-key"),
        Provider::Local => vec![1],
    }
}

/// Who to ask after `failed`, in order. `accounts` lists the accounts with a
/// key for each provider.
pub fn candidates(settings: &Settings, failed: &Target, accounts: &dyn Fn(Provider) -> Vec<u8>) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    let mut add = |provider: Provider, model: &str| {
        if model.trim().is_empty() {
            return;
        }
        for account in accounts(provider) {
            let target = Target::new(provider, model, account);
            if target != *failed && !out.contains(&target) {
                out.push(target);
            }
        }
    };
    if failed.provider != Provider::Local {
        // The same model, paid by the user's other accounts.
        add(failed.provider, &failed.model);
    }
    if failed.provider == Provider::Gemini {
        add(Provider::Gemini, GEMINI_LIGHT);
    }
    for (provider, model) in [
        (Provider::Anthropic, &settings.model),
        (Provider::OpenAi, &settings.openai_model),
        (Provider::Gemini, &settings.gemini_model),
    ] {
        if provider != failed.provider {
            add(provider, model);
            if provider == Provider::Gemini {
                add(Provider::Gemini, GEMINI_LIGHT);
            }
        }
    }
    if failed.provider != Provider::Local {
        add(Provider::Local, &settings.local_model);
    }
    out
}

/// The conversation as (is the user, text), from any provider's format.
pub fn transcript(history: &[Value]) -> Vec<(bool, String)> {
    history
        .iter()
        .filter_map(|m| {
            let user = match m.get("role").and_then(Value::as_str)? {
                "user" => true,
                "assistant" | "model" => false,
                _ => return None,
            };
            let mut texts: Vec<String> = Vec::new();
            match m.get("content") {
                Some(Value::String(s)) => texts.push(s.clone()),
                Some(Value::Array(parts)) => {
                    texts.extend(parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).map(str::to_string))
                }
                _ => {}
            }
            if let Some(Value::Array(parts)) = m.get("parts") {
                texts.extend(parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).map(str::to_string));
            }
            let text = texts.join("\n\n").trim().to_string();
            // Tool results travel as user messages with no text of their own.
            (!text.is_empty()).then_some((user, text))
        })
        .collect()
}

/// The same conversation in `provider`'s format.
pub fn rebuild(provider: Provider, turns: &[(bool, String)]) -> Vec<Value> {
    turns
        .iter()
        .map(|(user, text)| {
            if *user {
                let turn = UserTurn { text: text.clone(), file: None, window: None };
                match provider {
                    Provider::Anthropic => claude::user_message(&turn),
                    Provider::OpenAi => openai::user_message(&turn),
                    Provider::Gemini => gemini::user_message(&turn),
                    Provider::Local => local_llm::user_message(&turn).unwrap_or_else(|_| json!({ "role": "user", "content": text })),
                }
            } else {
                stored_text(provider, text)
            }
        })
        .collect()
}

/// An answer as `provider` keeps it in its history.
pub fn stored_text(provider: Provider, text: &str) -> Value {
    match provider {
        Provider::Gemini => json!({ "role": "model", "parts": [{ "text": text }] }),
        _ => json!({ "role": "assistant", "content": text }),
    }
}

/// The answer, kept in the format of the conversation it joins.
pub fn adopt(answer: Answer, provider: Provider) -> Answer {
    Answer { stored: stored_text(provider, &answer.text), text: answer.text }
}

/// Models and accounts that just said no, and until when to leave them be.
static RESTING: std::sync::Mutex<Vec<(Target, std::time::Instant, &'static str)>> = std::sync::Mutex::new(Vec::new());
/// A quota with no time given rests this long; an overloaded model a minute.
const QUOTA_REST: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const BUSY_REST: std::time::Duration = std::time::Duration::from_secs(60);

/// "try again in 7h5m2.38s" as a duration.
pub fn retry_after(err: &str) -> Option<std::time::Duration> {
    let rest = err.split("try again in ").nth(1)?;
    let spec: String = rest.chars().take_while(|c| c.is_ascii_digit() || matches!(c, 'h' | 'm' | 's' | '.')).collect();
    let mut total = 0f64;
    let mut number = String::new();
    for c in spec.chars() {
        match c {
            'h' => total += number.parse::<f64>().ok()? * 3600.0,
            'm' => total += number.parse::<f64>().ok()? * 60.0,
            's' => total += number.parse::<f64>().ok()?,
            _ => {
                number.push(c);
                continue;
            }
        }
        number.clear();
    }
    (total > 0.0).then(|| std::time::Duration::from_secs_f64(total))
}

/// Remembers a model that ran out of quota or was overloaded, so the next
/// questions go straight past it.
pub fn note_failure(target: &Target, err: &str) {
    let lower = err.to_lowercase();
    let (rest, why) = if lower.contains("quota") || lower.contains(" 429") {
        (retry_after(err).unwrap_or(QUOTA_REST).min(std::time::Duration::from_secs(24 * 3600)), "out of quota")
    } else if lower.contains("high demand") || lower.contains("overloaded") || lower.contains(" 503") {
        (BUSY_REST, "overloaded")
    } else {
        return;
    };
    let mut list = RESTING.lock().unwrap();
    list.retain(|(t, _, _)| t != target);
    list.push((target.clone(), std::time::Instant::now() + rest, why));
}

/// Why `target` is being left alone right now, if it is.
pub fn resting(target: &Target) -> Option<&'static str> {
    let mut list = RESTING.lock().unwrap();
    let now = std::time::Instant::now();
    list.retain(|(_, until, _)| *until > now);
    list.iter().find(|(t, _, _)| t == target).map(|(_, _, why)| *why)
}

/// Marks a step that cannot be undone in the list handed to the next model.
pub const IRREVERSIBLE: &str = "(cannot be undone) ";

/// Whether something that cannot be undone was already done: then the task
/// stops with an account of it instead of passing to another model.
pub fn has_irreversible(steps: &[String]) -> bool {
    steps.iter().any(|s| s.starts_with(IRREVERSIBLE))
}

/// Separates an error from the steps already done, inside one error string.
const PROGRESS: char = '\u{1e}';

/// An error from a model that had already changed things, with what it did.
pub fn with_progress(err: &str, steps: &[String]) -> String {
    format!("{err}{PROGRESS}{}", steps.join("\n"))
}

/// The error alone, and the steps already done (empty when none).
pub fn split_progress(err: &str) -> (String, Vec<String>) {
    match err.split_once(PROGRESS) {
        Some((message, steps)) => (message.to_string(), steps.lines().map(str::to_string).collect()),
        None => (err.to_string(), Vec::new()),
    }
}

/// What the next model is told when it takes over half-way.
pub fn carry_on(steps: &[String]) -> String {
    format!(
        "\n\n(Another assistant started on this and stopped half-way. It already did these steps; do not repeat \
them, check where things stand if you need to, and finish the request:\n- {})",
        steps.join("\n- ")
    )
}

/// A short reason for the note under the answer.
pub fn reason(err: &str) -> &'static str {
    let e = err.to_lowercase();
    if e.contains("quota") || e.contains(" 429") || e.contains("rate limit") {
        "ran out of quota"
    } else if e.contains("key missing") {
        "has no API key"
    } else if e.contains("network error") || e.contains("can't reach") || e.contains("server is off") {
        "could not be reached"
    } else {
        "is overloaded"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        let mut s = Settings::default();
        s.gemini_model = "gemini-3.8-flash".into();
        s.local_model = "qwen3:8b".into();
        s.openai_model = String::new();
        s
    }

    #[test]
    fn quota_overload_and_network_errors_go_elsewhere_but_refusals_do_not() {
        assert!(worth_another("Gemini's free quota for gemini-3.8-flash is used up for now"));
        assert!(worth_another("Gemini API 503: This model is currently experiencing high demand."));
        assert!(worth_another("Network error: connection refused"));
        assert!(worth_another("Claude API key missing. Open settings."));
        assert!(worth_another("Can't reach the local model server at http://localhost:11434/v1."));
        assert!(!worth_another("Gemini declined this request (SAFETY)."));
        assert!(!worth_another("This PDF has no text in it (probably a scan)"));
        assert!(!worth_another("Gemini API 400: Request contains an invalid argument."));
    }

    #[test]
    fn other_accounts_then_the_light_model_then_other_providers_then_the_local_model() {
        let s = settings();
        let flash = "gemini-3.8-flash";
        let failed = Target::new(Provider::Gemini, flash, 1);
        // Two Gemini accounts, no other cloud keys.
        let gemini_two = |p: Provider| match p {
            Provider::Gemini => vec![1, 2],
            Provider::Local => vec![1],
            _ => vec![],
        };
        assert_eq!(
            candidates(&s, &failed, &gemini_two),
            vec![
                Target::new(Provider::Gemini, flash, 2),
                Target::new(Provider::Gemini, GEMINI_LIGHT, 1),
                Target::new(Provider::Gemini, GEMINI_LIGHT, 2),
                Target::new(Provider::Local, "qwen3:8b", 1),
            ]
        );
        // Every provider with a key: Claude's accounts come before the local model.
        let all = |p: Provider| if p == Provider::Local { vec![1] } else { vec![1, 2] };
        let list = candidates(&s, &failed, &all);
        assert_eq!(list[3], Target::new(Provider::Anthropic, "claude-opus-5", 1));
        assert_eq!(list[4], Target::new(Provider::Anthropic, "claude-opus-5", 2));
        assert_eq!(list.last().unwrap().provider, Provider::Local);
        assert!(!list.iter().any(|t| t.provider == Provider::OpenAi), "no OpenAI model chosen yet");
        // The local model failing has no local fallback, and starts with Gemini.
        let local = Target::new(Provider::Local, "qwen3:8b", 1);
        let list = candidates(&s, &local, &gemini_two);
        assert_eq!(list[0], Target::new(Provider::Gemini, flash, 1));
        assert!(!list.iter().any(|t| t.provider == Provider::Local));
        assert_eq!(Target::new(Provider::Gemini, flash, 2).name(), "Gemini (account 2)");
    }

    #[test]
    fn a_used_up_quota_is_left_alone_until_google_says_it_is_back() {
        assert_eq!(retry_after("Google says to try again in 7h5m2.38s. Each step"), Some(std::time::Duration::from_secs_f64(25502.38)));
        assert_eq!(retry_after("try again in 41.2s."), Some(std::time::Duration::from_secs_f64(41.2)));
        assert_eq!(retry_after("no time here"), None);
        let target = Target::new(Provider::Gemini, "gemini-test-model", 7);
        assert_eq!(resting(&target), None);
        note_failure(&target, "Gemini's free quota for gemini-test-model is used up for now. Google says to try again in 2h.");
        assert_eq!(resting(&target), Some("out of quota"));
        let other = Target::new(Provider::Gemini, "gemini-test-model", 8);
        assert_eq!(resting(&other), None, "another account is not affected");
        note_failure(&other, "Gemini API 400: bad request");
        assert_eq!(resting(&other), None, "a refused request does not rest a model");
    }

    #[test]
    fn steps_already_done_travel_with_the_error() {
        let err = with_progress("Gemini's free quota is used up", &["run_command  -> Finished".into(), "edit_file a.txt -> Edited".into()]);
        assert!(worth_another(&err));
        let (message, steps) = split_progress(&err);
        assert_eq!(message, "Gemini's free quota is used up");
        assert_eq!(steps.len(), 2);
        assert_eq!(split_progress("plain"), ("plain".to_string(), vec![]));
        assert!(carry_on(&steps).contains("do not repeat"));
    }

    #[test]
    fn a_conversation_crosses_formats_as_text() {
        let history = vec![
            json!({ "role": "user", "parts": [{ "inline_data": {} }, { "text": "File: a.pdf" }, { "text": "Sum it up" }] }),
            json!({ "role": "model", "parts": [{ "text": "It says hi.", "thoughtSignature": "x" }] }),
            json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "1", "content": "..." }] }),
            json!({ "role": "assistant", "content": [{ "type": "text", "text": "Done." }] }),
            json!({ "role": "user", "content": "Thanks" }),
        ];
        let turns = transcript(&history);
        assert_eq!(
            turns,
            vec![
                (true, "File: a.pdf\n\nSum it up".to_string()),
                (false, "It says hi.".to_string()),
                (false, "Done.".to_string()),
                (true, "Thanks".to_string()),
            ]
        );
        let claude = rebuild(Provider::Anthropic, &turns);
        assert_eq!(claude[1], json!({ "role": "assistant", "content": "It says hi." }));
        let gemini = rebuild(Provider::Gemini, &turns);
        assert_eq!(gemini[1], json!({ "role": "model", "parts": [{ "text": "It says hi." }] }));
        assert_eq!(gemini[3]["role"], "user");
    }
}
