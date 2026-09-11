use crate::stepbuild::{append_assistant, flush_assistant, looks_like_failure};
use relay_compass::{health_steer_id, SemanticStep, StepRole, ToolKind};
use relay_core::model::CodexAgent;
use relay_core::state::{reduce_codex, truncate, CodexState};
use relay_discovery::registry::mtime_secs;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct CodexThreadRow {
    pub id: String,
    pub rollout_path: PathBuf,
    pub source: String,
    pub approval_mode: String,
    pub model: Option<String>,
    pub title: String,
    pub recency_at_ms: i64,
}

pub fn tail_messages(rollout_path: &Path, n: usize) -> Vec<(char, String)> {
    let text = match std::fs::read_to_string(rollout_path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut msgs: Vec<(char, String)> = Vec::new();
    for line in text.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        let p = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        match p.get("type").and_then(|t| t.as_str()) {
            Some("user_message") => {
                if let Some(m) = p.get("message").and_then(|x| x.as_str()) {
                    let m = m.trim();
                    if !m.is_empty() && !m.starts_with('<') {
                        msgs.push(('U', m.to_string()));
                    }
                }
            }
            Some("agent_message") => {
                if let Some(m) = p.get("message").and_then(|x| x.as_str()) {
                    if !m.trim().is_empty() {
                        msgs.push(('A', m.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    let start = msgs.len().saturating_sub(n);
    msgs.split_off(start)
}

fn codex_content_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn codex_tool_input(payload: &serde_json::Value) -> Option<serde_json::Value> {
    let raw = payload
        .get("arguments")
        .and_then(|x| x.as_str())
        .or_else(|| payload.get("input").and_then(|x| x.as_str()))?;
    serde_json::from_str(raw).ok()
}

fn codex_tool_target(payload: &serde_json::Value) -> Option<String> {
    let raw = payload
        .get("arguments")
        .and_then(|x| x.as_str())
        .or_else(|| payload.get("input").and_then(|x| x.as_str()))?;
    let input = codex_tool_input(payload);
    let extracted = input.as_ref().and_then(|input| {
        [
            "description",
            "message",
            "prompt",
            "objective",
            "cmd",
            "command",
            "path",
            "file_path",
            "uri",
            "query",
            "pattern",
        ]
        .into_iter()
        .find_map(|key| input.get(key).and_then(|value| value.as_str()))
    });
    Some(truncate(extracted.unwrap_or(raw), 160))
}

fn codex_tool_kind(name: &str, payload: &serde_json::Value) -> ToolKind {
    if let Some(input) = codex_tool_input(payload) {
        let has = |key: &str| input.get(key).is_some();
        if (has("message") || has("prompt") || has("objective"))
            && (has("task_name") || has("fork_turns") || has("target") || has("agent_id"))
        {
            return ToolKind::Delegate;
        }
        if has("patch") || has("edits") || has("old_string") || has("new_string") {
            return ToolKind::Modify;
        }
        if has("cmd") || has("command") || has("session_id") && has("chars") {
            return ToolKind::Execute;
        }
        if has("query") || has("search_query") || has("pattern") || has("cursor") {
            return ToolKind::Search;
        }
        if has("path") || has("file_path") || has("uri") || has("ref_id") {
            return ToolKind::Inspect;
        }
    }

    match name {
        "shell" | "exec_command" | "write_stdin" => ToolKind::Execute,
        "apply_patch" => ToolKind::Modify,
        "view_image" | "read_mcp_resource" => ToolKind::Inspect,
        "web__run" | "list_mcp_resources" | "list_mcp_resource_templates" => ToolKind::Search,
        "spawn_agent"
        | "collaboration.spawn_agent"
        | "followup_task"
        | "collaboration.followup_task" => ToolKind::Delegate,
        _ => ToolKind::Other,
    }
}

pub fn semantic_steps(rollout_path: &Path) -> Vec<SemanticStep> {
    match std::fs::read_to_string(rollout_path) {
        Ok(text) => parse_codex_steps(&text),
        Err(_) => Vec::new(),
    }
}

fn parse_codex_steps(text: &str) -> Vec<SemanticStep> {
    let mut steps: Vec<SemanticStep> = Vec::new();
    let mut index: u32 = 0;
    let mut pending: Option<String> = None;
    let mut call_kinds = BTreeMap::<String, ToolKind>::new();
    for line in text.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        match payload.get("type").and_then(|t| t.as_str()) {
            Some("message") => {
                let role = payload.get("role").and_then(|x| x.as_str()).unwrap_or("");
                let body = codex_content_text(payload.get("content"));
                let body = body.trim();
                if relay_compass::is_transcript_noise(body)
                    && !(role == "user" && health_steer_id(body).is_some())
                {
                    continue;
                }
                match role {
                    "user" => {
                        flush_assistant(&mut steps, &mut index, &mut pending);
                        steps.push(SemanticStep::new(index, StepRole::User, body.to_string()));
                        index += 1;
                    }
                    "assistant" => append_assistant(&mut pending, body),
                    _ => {}
                }
            }
            Some("function_call") | Some("custom_tool_call") => {
                flush_assistant(&mut steps, &mut index, &mut pending);
                let name = payload
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let target = codex_tool_target(payload);
                let mut step = SemanticStep::new(index, StepRole::ToolUse, name.clone());
                step.tool_kind = codex_tool_kind(&name, payload);
                step.tool_name = Some(name);
                step.correlation_id = payload
                    .get("call_id")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
                if let Some(call_id) = step.correlation_id.as_ref() {
                    call_kinds.insert(call_id.clone(), step.tool_kind);
                }
                step.tool_target = target;
                steps.push(step);
                index += 1;
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                flush_assistant(&mut steps, &mut index, &mut pending);
                let correlation_id = payload
                    .get("call_id")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);

                if correlation_id.as_ref().and_then(|id| call_kinds.get(id))
                    == Some(&ToolKind::Delegate)
                {
                    continue;
                }
                let output = codex_content_text(payload.get("output"));
                let is_error = looks_like_failure(&output);
                let mut step = SemanticStep::new(index, StepRole::ToolResult, output);
                step.tool_kind = correlation_id
                    .as_ref()
                    .and_then(|id| call_kinds.get(id))
                    .copied()
                    .unwrap_or(ToolKind::Other);
                step.correlation_id = correlation_id;
                step.is_error = is_error;
                steps.push(step);
                index += 1;
            }
            _ => {}
        }
    }
    flush_assistant(&mut steps, &mut index, &mut pending);
    steps
}

pub fn open_ro(db_path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(2000))?;
    Ok(conn)
}

pub fn active_thread_for_cwd(
    conn: &Connection,
    cwd: &Path,
) -> anyhow::Result<Option<CodexThreadRow>> {
    let cwd_s = cwd.to_string_lossy().to_string();
    let row = conn
        .query_row(
            "SELECT id, rollout_path, source, approval_mode, model, title, recency_at_ms \
             FROM threads WHERE archived = 0 AND cwd = ?1 AND source <> 'exec' \
             ORDER BY recency_at_ms DESC LIMIT 1",
            [cwd_s],
            row_to_thread,
        )
        .optional()?;
    Ok(row)
}

pub fn recent_threads_for_cwd(
    conn: &Connection,
    cwd: &Path,
    limit: u32,
) -> anyhow::Result<Vec<CodexThreadRow>> {
    let cwd_s = cwd.to_string_lossy().to_string();
    let mut stmt = conn.prepare(
        "SELECT id, rollout_path, source, approval_mode, model, title, recency_at_ms \
         FROM threads WHERE archived = 0 AND cwd = ?1 AND source <> 'exec' \
         ORDER BY recency_at_ms DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![cwd_s, limit], row_to_thread)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn row_to_thread(r: &rusqlite::Row<'_>) -> rusqlite::Result<CodexThreadRow> {
    let id: String = r.get(0)?;
    let rollout_path = r
        .get::<_, Option<String>>(1)?
        .map(PathBuf::from)
        .unwrap_or_default();
    let approval_mode = r
        .get::<_, Option<String>>(3)?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let title = r
        .get::<_, Option<String>>(5)?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    Ok(CodexThreadRow {
        id,
        rollout_path,
        source: r.get(2)?,
        approval_mode,
        model: r.get(4)?,
        title,
        recency_at_ms: r.get(6)?,
    })
}

pub struct CodexAttach {
    pub agent: CodexAgent,
    pub last_message: Option<String>,
    pub last_duration_ms: Option<u64>,
    pub usage: Option<relay_core::state::TokenUsage>,
    pub approval_policy: Option<String>,
    pub sandbox: Option<String>,
}

fn build_attach(row: CodexThreadRow) -> CodexAttach {
    let reduction = match std::fs::read_to_string(&row.rollout_path) {
        Ok(rollout) => reduce_codex(&rollout),
        Err(e) => relay_core::state::CodexReduction {
            state: CodexState::Error {
                message: format!("rollout read failed: {e}"),
            },
            last_agent_message: None,
            last_duration_ms: None,
            last_turn_tokens: None,
            approval_policy: None,
            sandbox: None,
        },
    };
    let agent = CodexAgent {
        thread_id: row.id,
        rollout_path: row.rollout_path.clone(),
        approval_mode: row.approval_mode,
        model: row.model,
        title: row.title,
        state: reduction.state,
        recency_at_ms: row.recency_at_ms,
        last_activity_secs: mtime_secs(&row.rollout_path),
    };
    CodexAttach {
        agent,
        last_message: reduction.last_agent_message,
        last_duration_ms: reduction.last_duration_ms,
        usage: reduction.last_turn_tokens,
        approval_policy: reduction.approval_policy,
        sandbox: reduction.sandbox,
    }
}

pub fn attach(db_path: &Path, cwd: &Path, max_age_ms: i64, now_ms: i64) -> Option<CodexAttach> {
    if max_age_ms < 0 {
        return None;
    }
    let conn = open_ro(db_path).ok()?;
    let row = active_thread_for_cwd(&conn, cwd).ok().flatten()?;
    if now_ms.saturating_sub(row.recency_at_ms) > max_age_ms {
        return None;
    }
    Some(build_attach(row))
}

pub fn attach_all(
    db_path: &Path,
    cwd: &Path,
    max_age_ms: i64,
    now_ms: i64,
    limit: u32,
) -> Vec<CodexAttach> {
    if max_age_ms < 0 || limit == 0 {
        return Vec::new();
    }
    let conn = match open_ro(db_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let rows = recent_threads_for_cwd(&conn, cwd, limit).unwrap_or_default();
    rows.into_iter()
        .filter(|r| now_ms.saturating_sub(r.recency_at_ms) <= max_age_ms)
        .map(build_attach)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_exec_threads_are_not_user_sessions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT, source TEXT, approval_mode TEXT,
                model TEXT, title TEXT, recency_at_ms INTEGER, archived INTEGER, cwd TEXT
             );
             INSERT INTO threads VALUES
                ('helper', '/tmp/helper', 'exec', 'never', NULL, 'controller prompt', 20, 0, '/repo'),
                ('human', '/tmp/human', 'vscode', 'never', NULL, 'real chat', 10, 0, '/repo');",
        )
        .unwrap();

        let rows = recent_threads_for_cwd(&conn, Path::new("/repo"), 8).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "human");
        assert_eq!(rows[0].source, "vscode");
    }

    #[test]
    fn semantic_steps_unified_schema() {
        let lines = [
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"build the flowchart parser"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"permissions instructions"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"working on it"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":\"cargo test\"}","call_id":"c1"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":[{"type":"input_text","text":"error: assertion failed"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"ignored duplicate"}}"#,
        ]
        .join("\n");
        let steps = parse_codex_steps(&lines);
        let roles: Vec<StepRole> = steps.iter().map(|s| s.role).collect();
        assert_eq!(
            roles,
            vec![
                StepRole::User,
                StepRole::Assistant,
                StepRole::ToolUse,
                StepRole::ToolResult,
                StepRole::Assistant,
            ]
        );
        assert_eq!(steps[0].text, "build the flowchart parser");
        assert_eq!(steps[2].tool_name.as_deref(), Some("shell"));
        assert_eq!(steps[2].correlation_id.as_deref(), Some("c1"));
        assert_eq!(steps[3].correlation_id.as_deref(), Some("c1"));
        assert!(steps[2]
            .tool_target
            .as_deref()
            .unwrap()
            .contains("cargo test"));
        assert!(steps[3].is_error);
    }

    #[test]
    fn codex_semantic_steps_retain_controller_challenge() {
        let lines = [
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"build and verify parser.rs"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"[VSC_RELAY_HEALTH_STEER v2 id=obl-0-1]\nRun one bounded check."}]}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":\"fresh smoke\"}","call_id":"call-7"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call-7","output":"ok"}}"#,
        ]
        .join("\n");

        let steps = parse_codex_steps(&lines);

        assert_eq!(steps[1].role, StepRole::User);
        assert_eq!(health_steer_id(&steps[1].text), Some("obl-0-1"));
        assert_eq!(steps[2].correlation_id.as_deref(), Some("call-7"));
        assert_eq!(steps[3].correlation_id.as_deref(), Some("call-7"));
    }

    #[test]
    fn codex_tool_output_never_becomes_user() {
        let lines = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"plain output"}}"#;
        let steps = parse_codex_steps(lines);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].role, StepRole::ToolResult);
    }

    #[test]
    fn codex_spawn_ack_is_not_a_completed_delegate_report() {
        let lines = [
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"spawn_agent","input":"{\"message\":\"inspect parser.rs\"}","call_id":"delegate-1"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"delegate-1","output":"{\"agent_id\":\"child-7\"}"}}"#,
        ]
        .join("\n");
        let steps = parse_codex_steps(&lines);

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].role, StepRole::ToolUse);
        assert_eq!(steps[0].tool_kind, ToolKind::Delegate);
        assert_eq!(steps[0].correlation_id.as_deref(), Some("delegate-1"));
    }

    #[test]
    fn renamed_tool_is_typed_from_payload_not_name() {
        let lines = r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"future.dispatch.v8","input":"{\"task_name\":\"audit\",\"message\":\"inspect parser\",\"fork_turns\":\"all\"}","call_id":"d9"}}"#;
        let steps = parse_codex_steps(lines);

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].tool_kind, ToolKind::Delegate);
        assert_eq!(steps[0].tool_target.as_deref(), Some("inspect parser"));
    }
}
