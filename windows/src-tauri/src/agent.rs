// The chat model acting with tools, whichever cloud model answers: it asks for
// tool calls, Coucou runs them (each one a step in the work view, each change
// shown before it is made) and hands back the results until the model answers.
// The local model has its own loop in local_llm.rs, for its quirks, and shares
// the prompt and the tools.

use serde_json::Value;
use tauri::{AppHandle, Runtime};

use crate::ai::{Answer, Provider};
use crate::settings::Settings;
use crate::tools::{self, Mode};
use crate::{claude, gemini, openai};

/// Cloud models plan further ahead than the local one: a document edit is
/// often read, read the next part, edit, check.
const MAX_STEPS: usize = 20;

/// Added to the second and later web searches of a question: models kept
/// rephrasing a search whose snippets lacked the answer (five searches and a
/// curl for a dollar rate).
pub const SEARCH_ENOUGH: &str = "\n\n(You have searched enough for this question. Do not search again: open the most \
promising result with read_web_page, or answer from what you have.)";

/// One tool call, in every provider's terms.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// What a provider answered: the final text, or tool calls to run. `assistant`
/// is what goes back into the conversation for the next request.
pub enum Turn {
    Answer(Answer),
    Calls { assistant: Vec<Value>, calls: Vec<Call> },
}

pub fn prompt(web_search: bool, mode: Mode) -> String {
    let (y, mo, d, ..) = crate::clock::local_now();
    let home = std::env::var("HOME").unwrap_or_default();
    let mut text = format!(
        "{}\n\nYou can see the user's screen with look_at_screen and act on their computer with tools: find files, list folders, read files (long ones part by part, \
PDFs page by page), create and change files (text, spreadsheets and Word documents, in place; the user sees every change), \
read web pages and follow their links, search the web, and open files or links for the user. Use them whenever the answer \
depends on the user's files or on current information, instead of guessing or saying you can't. When the user asks you to \
do something (create, change, delete, find, open, look up), do it with the tools rather than explaining how, then say \
briefly what you did. A file the user dropped comes with the place it was dropped from: read or change it there. Read a \
file before changing it, and use the row, column and paragraph numbers read_file shows. The user's home folder is {home}; \
their desktop is {home}/Desktop and their documents are in {home}/Documents. Today is {y:04}-{mo:02}-{d:02}. Read only as \
much of a long file as the question needs. Match the effort to the question: a simple question (a fact, a \
conversion, a definition, a quick how-to) gets a direct answer, with at most one search when it needs current data; \
never repeat a search in other words.",
        crate::ai::system_prompt(web_search)
    );
    text.push_str(
        "\n\nWhen the user asks for help with something on their screen, look at it, then guide them like a patient \
friend sitting next to them: one step at a time, naming exactly what to click and where it is on the screen (top left, \
the blue button at the bottom...), and ask them to say when it is done. A message that arrives with a screenshot of \
their screen shows where they are now.",
    );
    text.push_str("\n\n");
    text.push_str(&crate::shell::system_note());
    if mode == Mode::Plan {
        text.push_str(
            "\n\nPlan mode is on: you cannot change anything now. Read what you need, then answer with a short numbered plan \
of exactly what you would change (which file, which rows, columns or paragraphs, what text) and ask the user to approve \
it. Never say you have changed anything.",
        );
    }
    text
}

/// One chat turn with tools, for Claude, OpenAI and Gemini.
pub async fn run<R: Runtime>(
    app: &AppHandle<R>,
    provider: Provider,
    settings: &Settings,
    history: &[Value],
) -> Result<Answer, String> {
    let mode = Mode::parse(&settings.permission_mode);
    let mut definitions = tools::definitions(mode);
    // With screen sharing on, every question already carries a screenshot:
    // looking again would only cost a second one.
    if settings.screen_sharing {
        if let Some(list) = definitions.as_array_mut() {
            list.retain(|t| t.pointer("/function/name").and_then(Value::as_str) != Some("look_at_screen"));
        }
    }
    // Gemini's own search costs a quota a free key runs out of; it searches
    // through the search_web tool instead.
    let system = prompt(matches!(provider, Provider::Anthropic | Provider::OpenAi), mode);
    let mut messages = history.to_vec();
    let mut used: Vec<String> = Vec::new();
    // Whether anything was changed yet: until then another model can simply
    // start the question over. After that it carries on from `progress`.
    let mut changed = false;
    let mut progress: Vec<String> = Vec::new();
    let mut searches = 0usize;

    for _ in 0..MAX_STEPS {
        let step = match provider {
            Provider::Anthropic => claude::step(&settings.model, &system, &messages, &definitions).await,
            Provider::OpenAi => openai::step(&settings.openai_model, &system, &messages, &definitions).await,
            Provider::Gemini => gemini::step(&settings.gemini_model, &system, &messages, &definitions).await,
            Provider::Local => return Err("The local model runs its own loop.".into()),
        };
        let turn = match step {
            Ok(turn) => turn,
            Err(err) if !changed => return Err(err),
            // Something was already changed: the next model gets told what, so
            // it carries on instead of doing it twice.
            Err(err) => {
                crate::log::line(format!("agent: stopped half-way: {}", err.chars().take(160).collect::<String>()));
                return Err(crate::fallback::with_progress(&err, &progress));
            }
        };
        match turn {
            Turn::Answer(mut answer) => {
                if used.is_empty() {
                    crate::log::line("agent: answered without using tools");
                } else {
                    // The next turn sees what was done, not every intermediate call.
                    let text = format!("{}\n\n[Tools used: {}]", answer.text, used.join("; "));
                    answer.stored = crate::fallback::stored_text(provider, &text);
                }
                return Ok(answer);
            }
            Turn::Calls { assistant, calls } => {
                messages.extend(assistant);
                if calls.is_empty() {
                    continue; // a paused server-side search, picked up again
                }
                let mut outputs = Vec::with_capacity(calls.len());
                for call in &calls {
                    let mut output = tools::run(app, &call.name, &call.args, mode).await;
                    searches += usize::from(call.name == "search_web");
                    if searches >= 2 && call.name == "search_web" {
                        output.text.push_str(SEARCH_ENOUGH);
                    }
                    let outcome = if output.text.starts_with("Error:") { output.text.as_str() } else { "ok" };
                    crate::log::line(format!("tool: {} {} -> {outcome}", call.name, summary(&call.args)));
                    used.push(format!("{} {}", call.name, summary(&call.args)));
                    let command = call.args.get("command").and_then(Value::as_str).unwrap_or("");
                    let ran = !output.text.starts_with(tools::DECLINED);
                    let did_change = ran
                        && if call.name == "run_command" { !crate::shell::only_looks(command) } else { tools::changes_things(&call.name) };
                    changed |= did_change;
                    // Something that cannot be undone ran: nobody takes this task
                    // over (a hibernate was done twice that way).
                    let mark = if ran && call.name == "run_command" && crate::shell::is_dangerous(command) {
                        crate::fallback::IRREVERSIBLE
                    } else {
                        ""
                    };
                    progress.push(format!(
                        "{mark}{} {} -> {}",
                        call.name,
                        summary(&call.args),
                        output.text.lines().next().unwrap_or("").chars().take(200).collect::<String>()
                    ));
                    outputs.push(output);
                }
                messages.extend(results(provider, &calls, &outputs));
            }
        }
    }
    Err("That took too many steps. Try asking for something narrower.".into())
}

fn results(provider: Provider, calls: &[Call], outputs: &[tools::ToolOutput]) -> Vec<Value> {
    match provider {
        Provider::Anthropic => claude::tool_results(calls, outputs),
        Provider::OpenAi => openai::tool_results(calls, outputs),
        _ => gemini::tool_results(calls, outputs),
    }
}


/// The function definitions alone, out of the OpenAI chat format the tools
/// are written in: (name, description, JSON schema).
pub fn functions(definitions: &Value) -> Vec<(String, String, Value)> {
    definitions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some((
                f.get("name")?.as_str()?.to_string(),
                f.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
                f.get("parameters").cloned().unwrap_or_else(|| serde_json::json!({ "type": "object" })),
            ))
        })
        .collect()
}

/// A call's arguments, whether they arrive as an object or as JSON text.
pub fn arguments(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({})),
        Some(v @ Value::Object(_)) => v.clone(),
        _ => serde_json::json!({}),
    }
}

/// What a call was about, for the log and the conversation: paths and queries,
/// never file contents.
pub fn summary(args: &Value) -> String {
    ["path", "query", "url", "target", "command"]
        .iter()
        .filter_map(|k| args.get(*k).and_then(Value::as_str))
        .map(|v| v.chars().take(80).collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_mode_offers_only_the_tools_that_read() {
        let names = |mode| functions(&tools::definitions(mode)).into_iter().map(|f| f.0).collect::<Vec<_>>();
        let all = names(Mode::Manual);
        assert!(all.contains(&"edit_document".to_string()));
        assert!(all.contains(&"edit_spreadsheet".to_string()));
        let plan = names(Mode::Plan);
        assert!(plan.contains(&"read_file".to_string()));
        for write in ["create_file", "edit_spreadsheet", "edit_document", "edit_file"] {
            assert!(!plan.contains(&write.to_string()), "{write} offered in plan mode");
        }
        assert!(prompt(false, Mode::Plan).contains("Plan mode is on"));
        assert!(!prompt(false, Mode::Manual).contains("Plan mode is on"));
    }

    #[test]
    fn arguments_come_as_objects_or_json_text() {
        assert_eq!(arguments(Some(&serde_json::json!("{\"path\":\"a\"}"))), serde_json::json!({ "path": "a" }));
        assert_eq!(arguments(Some(&serde_json::json!({ "path": "a" }))), serde_json::json!({ "path": "a" }));
        assert_eq!(arguments(Some(&serde_json::json!("not json"))), serde_json::json!({}));
        assert_eq!(arguments(None), serde_json::json!({}));
    }
}
