use crate::stepbuild::{append_assistant, flush_assistant, looks_like_failure};
use relay_compass::{SemanticStep, StepRole, ToolKind, UserOrigin};
use serde_json::Value;
use std::path::{Path, PathBuf};

const BRAIN_DIRS: &[&str] = &["antigravity-cli", "antigravity-ide"];
const TRANSCRIPT_TAIL: &str = ".system_generated/logs/transcript_full.jsonl";
const EPHEMERAL_TYPES: &[&str] = &[
    "EPHEMERAL_MESSAGE",
    "CONVERSATION_HISTORY",
    "CHECKPOINT",
    "SYSTEM_MESSAGE",
];
const RESULT_TYPES: &[&str] = &[
    "RUN_COMMAND",
    "VIEW_FILE",
    "GREP_SEARCH",
    "LIST_DIRECTORY",
    "GENERIC",
    "ERROR_MESSAGE",
];

pub fn transcript(session_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?.join(".gemini");
    for dir in BRAIN_DIRS {
        let path = home
            .join(dir)
            .join("brain")
            .join(session_id)
            .join(TRANSCRIPT_TAIL);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

pub fn session_cwd(transcript_path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(transcript_path).ok()?;
    let mut best: Option<(usize, PathBuf)> = None;
    let mut counts: Vec<(PathBuf, usize)> = Vec::new();
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        for dir in row_directories(&row) {
            if dir == home_root() {
                continue;
            }
            match counts.iter_mut().find(|(known, _)| *known == dir) {
                Some((_, n)) => *n += 1,
                None => counts.push((dir, 1)),
            }
        }
    }
    for (dir, n) in counts {
        if best.as_ref().map(|(best_n, _)| n > *best_n).unwrap_or(true) {
            best = Some((n, dir));
        }
    }
    best.map(|(_, dir)| dir)
}

fn home_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

fn row_directories(row: &Value) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(cwd) = row.get("cwd").and_then(Value::as_str) {
        out.push(PathBuf::from(cwd));
    }
    for call in row
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let args = call.get("args");
        for key in ["Cwd", "DirectoryPath", "SearchDirectory"] {
            if let Some(value) = args.and_then(|a| a.get(key)).and_then(Value::as_str) {
                out.push(PathBuf::from(value));
            }
        }
        if let Some(file) = args
            .and_then(|a| a.get("AbsolutePath"))
            .and_then(Value::as_str)
        {
            if let Some(parent) = Path::new(file).parent() {
                out.push(parent.to_path_buf());
            }
        }
    }
    out
}

pub fn tail_messages(transcript_path: &Path, n: usize) -> Vec<(char, String)> {
    let steps = semantic_steps(transcript_path);
    let start = steps.len().saturating_sub(n);
    steps[start..]
        .iter()
        .map(|step| {
            let role = match step.role {
                StepRole::User => 'U',
                StepRole::ToolUse | StepRole::ToolResult => 'T',
                _ => 'A',
            };
            (role, step.text.clone())
        })
        .collect()
}

pub fn semantic_steps(transcript_path: &Path) -> Vec<SemanticStep> {
    match std::fs::read_to_string(transcript_path) {
        Ok(text) => parse_antigravity_steps(&text),
        Err(_) => Vec::new(),
    }
}

fn parse_antigravity_steps(text: &str) -> Vec<SemanticStep> {
    let mut steps: Vec<SemanticStep> = Vec::new();
    let mut index: u32 = 0;
    let mut pending: Option<String> = None;
    let mut seen: Vec<String> = Vec::new();
    let mut awaiting: Vec<(String, ToolKind)> = Vec::new();

    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let step_index = row
            .get("step_index")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !step_index.is_empty() {
            if seen.contains(&step_index) {
                continue;
            }
            seen.push(step_index);
        }
        let kind = row.get("type").and_then(Value::as_str).unwrap_or_default();
        if EPHEMERAL_TYPES.contains(&kind) {
            continue;
        }
        let source = row
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let content = row
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();

        if kind == "USER_INPUT" && source == "USER_EXPLICIT" {
            flush_assistant(&mut steps, &mut index, &mut pending);
            let body = unwrap_user_request(&content);
            if body.is_empty() {
                continue;
            }
            let mut step = SemanticStep::new(index, StepRole::User, body);
            step.user_origin = UserOrigin::Human;
            steps.push(step);
            index += 1;
            continue;
        }

        if kind == "PLANNER_RESPONSE" {
            if !content.is_empty() {
                append_assistant(&mut pending, &content);
            }
            for call in row
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                flush_assistant(&mut steps, &mut index, &mut pending);
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let args = call.get("args");
                let correlation_id = format!("agy-{index}");
                let mut step = SemanticStep::new(index, StepRole::ToolUse, name.clone());
                step.tool_kind = tool_kind(&name, args);
                step.tool_target = tool_target(args);
                step.tool_name = Some(name);
                step.correlation_id = Some(correlation_id.clone());
                awaiting.push((correlation_id, step.tool_kind));
                steps.push(step);
                index += 1;
            }
            continue;
        }

        if RESULT_TYPES.contains(&kind) {
            flush_assistant(&mut steps, &mut index, &mut pending);
            let (correlation_id, called_kind) = match awaiting.is_empty() {
                true => (None, fallback_kind(kind)),
                false => {
                    let (id, kind) = awaiting.remove(0);
                    (Some(id), kind)
                }
            };
            let mut step = SemanticStep::new(index, StepRole::ToolResult, content.clone());
            step.tool_kind = called_kind;
            step.correlation_id = correlation_id;
            step.is_error = command_failed(&row, &content);
            steps.push(step);
            index += 1;
            continue;
        }

        if source == "MODEL" && !content.is_empty() {
            append_assistant(&mut pending, &content);
        }
    }
    flush_assistant(&mut steps, &mut index, &mut pending);
    steps
}

fn fallback_kind(kind: &str) -> ToolKind {
    match kind {
        "RUN_COMMAND" => ToolKind::Execute,
        "VIEW_FILE" | "LIST_DIRECTORY" => ToolKind::Inspect,
        "GREP_SEARCH" => ToolKind::Search,
        _ => ToolKind::Other,
    }
}

fn unwrap_user_request(text: &str) -> String {
    let open = "<USER_REQUEST>";
    let close = "</USER_REQUEST>";
    match (text.find(open), text.rfind(close)) {
        (Some(a), Some(b)) if b > a + open.len() => text[a + open.len()..b].trim().to_string(),
        _ => text.to_string(),
    }
}

fn command_failed(row: &Value, content: &str) -> bool {
    if let Some(code) = row.get("exit_code").and_then(Value::as_str) {
        return code.trim() != "0";
    }
    if row.get("error").is_some() {
        return true;
    }
    if let Some(code) = exit_code_in_prose(content) {
        return code != 0;
    }
    let status = row
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status.eq_ignore_ascii_case("CANCELED") || status.eq_ignore_ascii_case("FAILED") {
        return true;
    }
    looks_like_failure(content)
}

fn exit_code_in_prose(content: &str) -> Option<i64> {
    for marker in [
        "The command failed with exit code:",
        "The command exited with code",
        "Exit code:",
    ] {
        if let Some(at) = content.find(marker) {
            let rest = content[at + marker.len()..].trim_start();
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    None
}

fn tool_kind(name: &str, args: Option<&Value>) -> ToolKind {
    if let Some(args) = args {
        let has = |key: &str| args.get(key).is_some();
        if has("CommandLine") {
            return ToolKind::Execute;
        }
        if has("ReplacementChunks") || has("CodeMarkdownLanguage") || has("TargetFile") {
            return ToolKind::Modify;
        }
        if has("Query") || has("SearchTerm") || has("SearchDirectory") {
            return ToolKind::Search;
        }
        if has("AbsolutePath") || has("DirectoryPath") {
            return ToolKind::Inspect;
        }
    }
    match name {
        "run_command" | "command_status" => ToolKind::Execute,
        "write_to_file" | "replace_file_content" | "edit_file" => ToolKind::Modify,
        "grep_search" | "codebase_search" | "find_by_name" | "search_web" => ToolKind::Search,
        "view_file" | "list_dir" | "read_url_content" => ToolKind::Inspect,
        _ => ToolKind::Other,
    }
}

fn tool_target(args: Option<&Value>) -> Option<String> {
    let args = args?;
    for key in [
        "AbsolutePath",
        "TargetFile",
        "CommandLine",
        "DirectoryPath",
        "Query",
        "SearchDirectory",
    ] {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(row: serde_json::Value) -> String {
        serde_json::to_string(&row).unwrap()
    }

    fn fixture() -> String {
        [
            line(serde_json::json!({
                "step_index": "0", "source": "USER_EXPLICIT", "type": "USER_INPUT",
                "status": "DONE",
                "content": "<USER_REQUEST>\nAdd f_to_c to temp.py\n</USER_REQUEST>"
            })),
            line(serde_json::json!({
                "step_index": "1", "source": "SYSTEM", "type": "EPHEMERAL_MESSAGE",
                "status": "DONE",
                "content": "The following is an <EPHEMERAL_MESSAGE> not actually sent by the user."
            })),
            line(serde_json::json!({
                "step_index": "2", "source": "MODEL", "type": "PLANNER_RESPONSE",
                "status": "DONE", "content": "I will read the file first.",
                "tool_calls": [{"name": "view_file", "args": {"AbsolutePath": "/w/temp.py"}}]
            })),
            line(serde_json::json!({
                "step_index": "3", "source": "MODEL", "type": "RUN_COMMAND",
                "status": "DONE", "exit_code": "2",
                "content": "The command exited with code 2.\nOutput:\nboom"
            })),
            line(serde_json::json!({
                "step_index": "3", "source": "MODEL", "type": "RUN_COMMAND",
                "status": "RUNNING", "content": "duplicate line for the same step"
            })),
            line(serde_json::json!({
                "step_index": "4", "source": "MODEL", "type": "RUN_COMMAND",
                "status": "DONE",
                "content": "The command exited with code 0.\nOutput:\nok"
            })),
        ]
        .join("\n")
    }

    #[test]
    fn a_session_becomes_steps_with_the_contract_first_and_noise_dropped() {
        let steps = parse_antigravity_steps(&fixture());
        assert_eq!(steps[0].role, StepRole::User);
        assert_eq!(steps[0].text, "Add f_to_c to temp.py");
        assert!(
            !steps.iter().any(|s| s.text.contains("EPHEMERAL_MESSAGE")),
            "injected system material must never reach the ledger"
        );
        let tools: Vec<_> = steps
            .iter()
            .filter(|s| s.role == StepRole::ToolUse)
            .collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_name.as_deref(), Some("view_file"));
        assert_eq!(tools[0].tool_kind, ToolKind::Inspect);
        assert_eq!(tools[0].tool_target.as_deref(), Some("/w/temp.py"));
    }

    #[test]
    fn a_failed_command_is_marked_and_a_repeated_step_is_counted_once() {
        let steps = parse_antigravity_steps(&fixture());
        let results: Vec<_> = steps
            .iter()
            .filter(|s| s.role == StepRole::ToolResult)
            .collect();
        assert_eq!(
            results.len(),
            2,
            "the duplicated step_index must not double"
        );
        assert!(results[0].is_error, "exit code 2 is a failure");
        assert!(!results[1].is_error, "exit code 0 is not");
    }

    #[test]
    fn an_exit_code_is_read_from_prose_when_the_field_is_absent() {
        assert_eq!(
            exit_code_in_prose("The command exited with code 0.\nOutput:"),
            Some(0)
        );
        assert_eq!(
            exit_code_in_prose("The command failed with exit code: 127"),
            Some(127)
        );
        assert_eq!(exit_code_in_prose("no code here"), None);
    }

    #[test]
    fn the_working_directory_is_the_one_the_session_actually_used() {
        let home = home_root();
        let text = [
            line(serde_json::json!({
                "step_index": "0", "source": "MODEL", "type": "RUN_COMMAND",
                "tool_calls": [{"name": "run_command", "args": {"CommandLine": "ls", "Cwd": home.display().to_string()}}]
            })),
            line(serde_json::json!({
                "step_index": "1", "source": "MODEL", "type": "RUN_COMMAND",
                "tool_calls": [{"name": "run_command", "args": {"CommandLine": "ls", "Cwd": "/Users/x/devs/carcost"}}]
            })),
        ]
        .join("\n");
        let dir = std::env::temp_dir().join(format!("agy-cwd-{}.jsonl", std::process::id()));
        std::fs::write(&dir, text).unwrap();
        assert_eq!(
            session_cwd(&dir),
            Some(PathBuf::from("/Users/x/devs/carcost"))
        );
        let _ = std::fs::remove_file(&dir);
    }
}
