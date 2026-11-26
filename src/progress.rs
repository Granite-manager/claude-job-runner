//! What a running job is doing, for the person waiting on it.
//!
//! Jobs run with `--output-format stream-json`, so Claude Code prints one
//! JSON event per line while it works. [`step_of`] turns each tool call in
//! those events into a few plain words ("Looking up deals", "Designing
//! sales.html"), and [`Progress`] keeps the latest ones for the question's
//! `payload.progress`, which the Agent page shows while it waits.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claude::GRANITE_MCP_NAME;

/// Most steps kept per question; older ones are dropped.
pub const MAX_STEPS: usize = 40;

/// Longest step text, in characters.
const MAX_STEP_CHARS: usize = 120;

/// One thing the agent did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Unix seconds.
    pub at: i64,
    /// A few words.
    pub text: String,
}

/// The latest steps of one job, oldest first.
#[derive(Debug, Default, Clone)]
pub struct Progress {
    steps: Vec<Step>,
}

impl Progress {
    /// Adds a step, unless it repeats the previous one.
    pub fn push(&mut self, at: i64, text: String) -> bool {
        if self.steps.last().is_some_and(|last| last.text == text) {
            return false;
        }
        self.steps.push(Step { at, text });
        if self.steps.len() > MAX_STEPS {
            self.steps.remove(0);
        }
        true
    }

    /// The steps so far.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// The steps as the JSON array stored in `payload.progress`.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.steps).unwrap_or_else(|_| "[]".to_owned())
    }
}

fn cut(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_STEP_CHARS {
        return one_line;
    }
    let mut short: String = one_line.chars().take(MAX_STEP_CHARS - 1).collect();
    short.push('…');
    short
}

/// `list_deal_activities` → `deal activities`.
fn words(operation: &str) -> String {
    operation.replace(['_', '-'], " ").trim().to_owned()
}

/// A Granite API operation id in plain words.
fn api_call(operation: &str) -> String {
    if operation == "get_current_user" {
        return "Checking who is asking".to_owned();
    }
    let (verb, rest) = operation.split_once('_').unwrap_or((operation, ""));
    let rest = words(rest);
    match verb {
        "list" | "get" | "search" | "find" | "read" if !rest.is_empty() => {
            format!("Looking up {rest}")
        }
        "count" if !rest.is_empty() => format!("Counting {rest}"),
        _ => format!("Calling {}", words(operation)),
    }
}

fn file_name(path: Option<&str>) -> String {
    path.and_then(|p| Path::new(p).file_name())
        .map_or_else(|| "a file".to_owned(), |n| n.to_string_lossy().into_owned())
}

/// The words for one tool call.
fn tool_step(name: &str, input: &Value) -> Option<String> {
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let granite = format!("mcp__{GRANITE_MCP_NAME}__");
    if let Some(tool) = name.strip_prefix(&granite) {
        return Some(match tool {
            "search_endpoints" => match text("query") {
                Some(query) => format!("Searching Granite Manager for “{query}”"),
                None => "Searching Granite Manager".to_owned(),
            },
            "list_categories" => "Looking around Granite Manager".to_owned(),
            "list_endpoints" => match text("category") {
                Some(category) => format!("Browsing {}", words(category)),
                None => "Browsing Granite Manager".to_owned(),
            },
            "describe_endpoint" => match text("operation_id") {
                Some(op) => format!("Reading how {} works", words(op)),
                None => "Reading the API".to_owned(),
            },
            "call_endpoint" => text("operation_id").map_or_else(
                || "Fetching data".to_owned(),
                api_call,
            ),
            other => format!("Using {}", words(other)),
        });
    }
    Some(match name {
        "Write" | "Edit" | "MultiEdit" => {
            let file = file_name(text("file_path"));
            if file.to_ascii_lowercase().ends_with(".html") {
                format!("Designing {file}")
            } else {
                format!("Writing {file}")
            }
        }
        "Read" => format!("Reading {}", file_name(text("file_path"))),
        "Bash" => text("description").map_or_else(|| "Crunching numbers".to_owned(), str::to_owned),
        "Grep" | "Glob" => "Searching files".to_owned(),
        "TodoWrite" => "Planning the next steps".to_owned(),
        "Task" | "Agent" => text("description").map_or_else(
            || "Working on a sub-task".to_owned(),
            str::to_owned,
        ),
        "NotebookEdit" => "Working in a notebook".to_owned(),
        _ => return None,
    })
}

/// The steps in one line of `--output-format stream-json` output: one per
/// tool call of an assistant message. Other events (and anything that is
/// not JSON) have none.
#[must_use]
pub fn steps_of(line: &str) -> Vec<String> {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if event.get("type").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    event
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| {
            let name = block.get("name").and_then(Value::as_str)?;
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            tool_step(name, &input).map(|text| cut(&text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(blocks: &Value) -> String {
        serde_json::json!({ "type": "assistant", "message": { "content": blocks } }).to_string()
    }

    #[test]
    fn tool_calls_read_as_plain_words() {
        let line = assistant(&serde_json::json!([
            { "type": "text", "text": "Let me look." },
            { "type": "tool_use", "name": "mcp__granite__call_endpoint",
              "input": { "operation_id": "list_deal_activities" } },
            { "type": "tool_use", "name": "mcp__granite__call_endpoint",
              "input": { "operation_id": "get_current_user" } },
            { "type": "tool_use", "name": "mcp__granite__search_endpoints",
              "input": { "query": "deals by stage" } },
            { "type": "tool_use", "name": "Write",
              "input": { "file_path": "/w/job-1/pages/sales.html" } },
            { "type": "tool_use", "name": "Bash",
              "input": { "command": "python3 x.py", "description": "Totaling sales by month" } },
            { "type": "tool_use", "name": "Unheard", "input": {} },
        ]));
        assert_eq!(
            steps_of(&line),
            vec![
                "Looking up deal activities",
                "Checking who is asking",
                "Searching Granite Manager for “deals by stage”",
                "Designing sales.html",
                "Totaling sales by month",
            ]
        );
    }

    #[test]
    fn other_events_have_no_steps() {
        assert_eq!(steps_of(r#"{"type":"result","result":"x"}"#), Vec::<String>::new());
        assert_eq!(steps_of(r#"{"type":"user","message":{"content":[]}}"#), Vec::<String>::new());
        assert_eq!(steps_of("not json"), Vec::<String>::new());
        assert_eq!(api_call("create_note"), "Calling create note");
    }

    #[test]
    fn progress_drops_repeats_and_keeps_the_latest() {
        let mut progress = Progress::default();
        assert!(progress.push(1, "a".into()));
        assert!(!progress.push(2, "a".into()));
        for i in 0..MAX_STEPS {
            progress.push(3, format!("s{i}"));
        }
        assert_eq!(progress.steps().len(), MAX_STEPS);
        assert_eq!(progress.steps()[0].text, "s0");
        assert!(progress.to_json().starts_with(r#"[{"at":3,"text":"s0"}"#));
        assert_eq!(cut(&"x".repeat(200)).chars().count(), MAX_STEP_CHARS);
    }
}
