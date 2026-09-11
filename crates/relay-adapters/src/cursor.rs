use crate::stepbuild::{append_assistant, flush_assistant, looks_like_failure};
use relay_compass::{SemanticStep, StepRole, ToolKind, UserOrigin};
use rusqlite::Connection;
use serde_json::Value;
use std::path::{Path, PathBuf};

const ROOT_ID_TAG: [u8; 2] = [0x0a, 0x20];
const BLOB_ID_LEN: usize = 32;

pub fn store_db(session_id: &str) -> Option<PathBuf> {
    let chats = dirs::home_dir()?.join(".cursor").join("chats");
    for workspace in std::fs::read_dir(chats).ok()?.flatten() {
        let path = workspace.path().join(session_id).join("store.db");
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

pub fn session_cwd(store_path: &Path) -> Option<PathBuf> {
    let meta = store_path.with_file_name("meta.json");
    let text = std::fs::read_to_string(meta).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

pub fn chat_name(store_path: &Path) -> Option<String> {
    let conn = crate::codex::open_ro(store_path).ok()?;
    let hex: String = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .ok()?;
    let raw = decode_hex(&hex)?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|name| !name.trim().is_empty())
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let hex = hex.trim();
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

fn root_blob_id(conn: &Connection) -> Option<String> {
    let hex: String = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .ok()?;
    let raw = decode_hex(&hex)?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    value
        .get("latestRootBlobId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn ordered_ids(root: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + ROOT_ID_TAG.len() + BLOB_ID_LEN <= root.len() {
        if root[at..at + 2] == ROOT_ID_TAG {
            let start = at + 2;
            let id: String = root[start..start + BLOB_ID_LEN]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            out.push(id);
            at = start + BLOB_ID_LEN;
        } else {
            at += 1;
        }
    }
    out
}

fn conversation_blobs(conn: &Connection) -> Vec<Vec<u8>> {
    let ordered = root_blob_id(conn)
        .and_then(|id| {
            conn.query_row("SELECT data FROM blobs WHERE id = ?1", [&id], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .ok()
        })
        .map(|root| ordered_ids(&root))
        .unwrap_or_default();

    if !ordered.is_empty() {
        let mut out = Vec::new();
        for id in ordered {
            if let Ok(data) = conn.query_row("SELECT data FROM blobs WHERE id = ?1", [&id], |row| {
                row.get::<_, Vec<u8>>(0)
            }) {
                out.push(data);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare("SELECT data FROM blobs ORDER BY rowid") {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0)) {
            for row in rows.flatten() {
                out.push(row);
            }
        }
    }
    out
}

pub fn semantic_steps(store_path: &Path) -> Vec<SemanticStep> {
    let Ok(conn) = crate::codex::open_ro(store_path) else {
        return Vec::new();
    };
    parse_cursor_blobs(conversation_blobs(&conn))
}

pub fn tail_messages(store_path: &Path, n: usize) -> Vec<(char, String)> {
    let steps = semantic_steps(store_path);
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

fn parse_cursor_blobs(blobs: Vec<Vec<u8>>) -> Vec<SemanticStep> {
    let mut steps: Vec<SemanticStep> = Vec::new();
    let mut index: u32 = 0;
    let mut pending: Option<String> = None;
    let mut call_kinds: Vec<(String, ToolKind)> = Vec::new();

    for blob in blobs {
        if blob.first() != Some(&b'{') {
            continue;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&blob) else {
            continue;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "system" {
            continue;
        }
        let content = message.get("content");

        if let Some(text) = content.and_then(Value::as_str) {
            if role == "user" && !is_environment_preamble(text) {
                flush_assistant(&mut steps, &mut index, &mut pending);
                push_user(&mut steps, &mut index, text);
            }
            continue;
        }

        for part in content.and_then(Value::as_array).into_iter().flatten() {
            match part.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                    if role == "user" {
                        if is_environment_preamble(text) {
                            continue;
                        }
                        flush_assistant(&mut steps, &mut index, &mut pending);
                        push_user(&mut steps, &mut index, text);
                    } else if !text.trim().is_empty() {
                        append_assistant(&mut pending, text.trim());
                    }
                }
                "tool-call" => {
                    flush_assistant(&mut steps, &mut index, &mut pending);
                    let name = part
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let args = part.get("args");
                    let mut step = SemanticStep::new(index, StepRole::ToolUse, name.clone());
                    step.tool_kind = tool_kind(&name, args);
                    step.tool_target = tool_target(args);
                    step.tool_name = Some(name);
                    step.correlation_id = part
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if let Some(id) = step.correlation_id.clone() {
                        call_kinds.push((id, step.tool_kind));
                    }
                    steps.push(step);
                    index += 1;
                }
                "tool-result" => {
                    flush_assistant(&mut steps, &mut index, &mut pending);
                    let correlation_id = part
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let text = result_text(part);
                    let kind = correlation_id
                        .as_ref()
                        .and_then(|id| {
                            call_kinds
                                .iter()
                                .find(|(known, _)| known == id)
                                .map(|(_, kind)| *kind)
                        })
                        .unwrap_or(ToolKind::Other);
                    let mut step = SemanticStep::new(index, StepRole::ToolResult, text.clone());
                    step.tool_kind = kind;
                    step.correlation_id = correlation_id;
                    step.is_error = result_failed(&text);
                    steps.push(step);
                    index += 1;
                }
                _ => {}
            }
        }
    }
    flush_assistant(&mut steps, &mut index, &mut pending);
    steps
}

fn push_user(steps: &mut Vec<SemanticStep>, index: &mut u32, text: &str) {
    let body = unwrap_user_query(text);
    if body.is_empty() {
        return;
    }
    let mut step = SemanticStep::new(*index, StepRole::User, body);
    step.user_origin = UserOrigin::Human;
    steps.push(step);
    *index += 1;
}

fn is_environment_preamble(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with("<user_info>") || head.starts_with("<environment")
}

fn unwrap_user_query(text: &str) -> String {
    let mut body = text.trim().to_string();
    if let (Some(a), Some(b)) = (body.find("<user_query>"), body.rfind("</user_query>")) {
        if b > a {
            body = body[a + "<user_query>".len()..b].trim().to_string();
        }
    } else if let Some(at) = body.rfind("</timestamp>") {
        body = body[at + "</timestamp>".len()..].trim().to_string();
    }
    body
}

fn result_text(part: &Value) -> String {
    if let Some(text) = part.get("result").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(items) = part.get("experimental_content").and_then(Value::as_array) {
        let joined: Vec<String> = items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if !joined.is_empty() {
            return joined.join("\n");
        }
    }
    part.get("result")
        .map(|value| value.to_string())
        .unwrap_or_default()
}

fn result_failed(text: &str) -> bool {
    if let Some(code) = exit_code_of(text) {
        return code != 0;
    }
    looks_like_failure(text)
}

fn exit_code_of(text: &str) -> Option<i64> {
    let marker = "Exit code:";
    let at = text.find(marker)?;
    let rest = text[at + marker.len()..].trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn tool_kind(name: &str, args: Option<&Value>) -> ToolKind {
    if let Some(args) = args {
        let has = |key: &str| args.get(key).is_some();
        if has("command") {
            return ToolKind::Execute;
        }
        if has("old_string") || has("new_string") || has("contents") || has("patch") {
            return ToolKind::Modify;
        }
        if has("pattern") || has("glob_pattern") || has("search_term") || has("query") {
            return ToolKind::Search;
        }
        if has("path") || has("file_path") {
            return ToolKind::Inspect;
        }
    }
    match name {
        "Shell" | "AwaitShell" | "Await" => ToolKind::Execute,
        "Write" | "StrReplace" | "ApplyPatch" | "Delete" | "EditNotebook" => ToolKind::Modify,
        "Grep" | "Glob" | "WebSearch" | "SemanticSearch" | "rg" => ToolKind::Search,
        "Read" | "ReadFile" | "ReadLints" | "WebFetch" => ToolKind::Inspect,
        "Task" | "Subagent" => ToolKind::Delegate,
        _ => ToolKind::Other,
    }
}

fn tool_target(args: Option<&Value>) -> Option<String> {
    let args = args?;
    if let Some(text) = args.as_str() {
        return Some(text.to_string());
    }
    for key in [
        "path",
        "file_path",
        "command",
        "glob_pattern",
        "pattern",
        "search_term",
        "target_directory",
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
    use serde_json::json;

    fn blobs() -> Vec<Vec<u8>> {
        [
            json!({"role": "system", "content": "you are cursor"}),
            json!({"role": "user", "content": "<user_info>OS Version: darwin</user_info>"}),
            json!({"role": "user", "content": [{"type": "text", "text": "<timestamp>Aug 6</timestamp>\n<user_query>\nWrite the frontend\n</user_query>"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "Reading the brief."},
                {"type": "tool-call", "toolCallId": "tool_a", "toolName": "Read", "args": {"path": "/w/HANDOFF.md"}}
            ]}),
            json!({"role": "tool", "content": [
                {"type": "tool-result", "toolCallId": "tool_a", "toolName": "Read", "result": "# brief"}
            ]}),
            json!({"role": "assistant", "content": [
                {"type": "tool-call", "toolCallId": "tool_b", "toolName": "Shell", "args": {"command": "python3 server.py"}}
            ]}),
            json!({"role": "tool", "content": [
                {"type": "tool-result", "toolCallId": "tool_b", "toolName": "Shell",
                 "result": "Exit code: 1\n\nCommand output:\n\nAddress already in use"}
            ]}),
            json!({"role": "assistant", "content": [{"type": "reasoning", "text": "hidden"}, {"type": "text", "text": "Done."}]}),
        ]
        .iter()
        .map(|v| serde_json::to_vec(v).unwrap())
        .collect()
    }

    #[test]
    fn the_contract_is_the_user_query_without_its_wrapper_or_the_environment_preamble() {
        let steps = parse_cursor_blobs(blobs());
        let users: Vec<_> = steps.iter().filter(|s| s.role == StepRole::User).collect();
        assert_eq!(
            users.len(),
            1,
            "the environment preamble is not a user turn"
        );
        assert_eq!(users[0].text, "Write the frontend");
    }

    #[test]
    fn a_tool_result_inherits_the_kind_of_its_call_and_reads_the_exit_code() {
        let steps = parse_cursor_blobs(blobs());
        let results: Vec<_> = steps
            .iter()
            .filter(|s| s.role == StepRole::ToolResult)
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_kind, ToolKind::Inspect);
        assert!(!results[0].is_error);
        assert_eq!(results[1].tool_kind, ToolKind::Execute);
        assert!(results[1].is_error, "exit code 1 is a failure");
        assert_eq!(results[1].correlation_id.as_deref(), Some("tool_b"));
    }

    #[test]
    fn assistant_prose_is_kept_and_reasoning_is_dropped() {
        let steps = parse_cursor_blobs(blobs());
        let text: Vec<&str> = steps
            .iter()
            .filter(|s| s.role == StepRole::Assistant)
            .map(|s| s.text.as_str())
            .collect();
        assert!(text.iter().any(|t| t.contains("Reading the brief")));
        assert!(text.iter().any(|t| t.contains("Done.")));
        assert!(
            !text.iter().any(|t| t.contains("hidden")),
            "reasoning blocks are not conversation"
        );
    }

    #[test]
    fn a_blob_that_is_not_json_is_skipped_rather_than_breaking_the_session() {
        let mut with_junk = blobs();
        with_junk.insert(0, vec![0x0a, 0x20, 0xff, 0xfe]);
        with_junk.push(b"\x03not-a-message".to_vec());
        let steps = parse_cursor_blobs(with_junk);
        assert_eq!(steps.iter().filter(|s| s.role == StepRole::User).count(), 1);
    }

    #[test]
    fn the_root_blob_yields_the_conversation_order() {
        let mut root = Vec::new();
        for byte in [0xaa_u8, 0xbb] {
            root.extend_from_slice(&ROOT_ID_TAG);
            root.extend_from_slice(&[byte; BLOB_ID_LEN]);
        }
        let ids = ordered_ids(&root);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], "aa".repeat(BLOB_ID_LEN));
        assert_eq!(ids[1], "bb".repeat(BLOB_ID_LEN));
    }
}
