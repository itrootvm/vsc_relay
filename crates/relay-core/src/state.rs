use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
    #[serde(default)]
    pub options: Vec<QOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestion {
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClaudeState {
    Idle,
    Working {
        open_tools: u16,
    },
    SubagentRunning,
    AwaitingPermission {
        tool: String,
        target: Option<String>,
        suspected: bool,
    },
    PendingQuestion(AskUserQuestion),
    Interrupted,
    Denied,
    Error {
        message: String,
    },
    Dead,
    #[default]
    Unknown,
}

impl ClaudeState {
    pub fn label(&self) -> &'static str {
        match self {
            ClaudeState::Idle => "idle",
            ClaudeState::Working { .. } => "working",
            ClaudeState::SubagentRunning => "subagent",
            ClaudeState::AwaitingPermission { .. } => "awaiting_permission",
            ClaudeState::PendingQuestion(_) => "pending_question",
            ClaudeState::Interrupted => "interrupted",
            ClaudeState::Denied => "denied",
            ClaudeState::Error { .. } => "error",
            ClaudeState::Dead => "dead",
            ClaudeState::Unknown => "unknown",
        }
    }
    pub fn actionable(&self) -> bool {
        matches!(
            self,
            ClaudeState::AwaitingPermission { .. }
                | ClaudeState::PendingQuestion(_)
                | ClaudeState::Error { .. }
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeReduction {
    pub state: ClaudeState,
    pub tip_uuid: Option<String>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub last_assistant_text: Option<String>,
    pub ai_title: Option<String>,
    pub open_tool_count: usize,
    pub pending_tool: Option<String>,
    pub pending_target: Option<String>,
}

const CLAUDE_SIDECARS: &[&str] = &[
    "ai-title",
    "last-prompt",
    "mode",
    "queue-operation",
    "file-history-snapshot",
    "summary",
    "system",
];

pub fn reduce_claude(transcript: &str) -> ClaudeReduction {
    use std::collections::BTreeMap;

    struct OpenTool {
        name: String,
        target: Option<String>,
        questions: Option<Vec<Question>>,
    }

    let mut open: BTreeMap<String, OpenTool> = BTreeMap::new();
    let mut r = ClaudeReduction::default();
    let mut last_kind: Option<&'static str> = None;
    let mut last_user_marker: Option<&'static str> = None;

    for (line_idx, line) in transcript.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        if ty == "last-prompt" {
            if let Some(leaf) = v.get("leafUuid").and_then(|x| x.as_str()) {
                r.tip_uuid = Some(leaf.to_string());
            }
            continue;
        }
        if ty == "ai-title" {
            if let Some(t) = v.get("aiTitle").and_then(|x| x.as_str()) {
                r.ai_title = Some(t.to_string());
            }
            continue;
        }
        if CLAUDE_SIDECARS.contains(&ty) || ty == "attachment" {
            continue;
        }

        if let Some(s) = v.get("sessionId").and_then(|x| x.as_str()) {
            r.session_id = Some(s.to_string());
        }
        if let Some(s) = v.get("cwd").and_then(|x| x.as_str()) {
            r.cwd = Some(s.to_string());
        }
        if let Some(s) = v.get("gitBranch").and_then(|x| x.as_str()) {
            r.git_branch = Some(s.to_string());
        }

        match ty {
            "assistant" => {
                let msg = v.get("message");
                let stop = msg
                    .and_then(|m| m.get("stop_reason"))
                    .and_then(|s| s.as_str());
                let content = msg
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array());
                if let Some(blocks) = content {
                    for (block_idx, b) in blocks.iter().enumerate() {
                        match b.get("type").and_then(|t| t.as_str()) {
                            Some("tool_use") => {
                                let id = b
                                    .get("id")
                                    .and_then(|x| x.as_str())
                                    .filter(|s| !s.is_empty())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| format!("missing:{line_idx}:{block_idx}"));
                                let name = b
                                    .get("name")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                let input = b.get("input");
                                let questions = if name == "AskUserQuestion" {
                                    input
                                        .and_then(|i| i.get("questions"))
                                        .and_then(|q| serde_json::from_value(q.clone()).ok())
                                } else {
                                    None
                                };
                                let target = claude_tool_target(&name, input);
                                open.insert(
                                    id,
                                    OpenTool {
                                        name,
                                        target,
                                        questions,
                                    },
                                );
                                last_kind = Some("assistant_tool");
                            }
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                    if !t.trim().is_empty() {
                                        r.last_assistant_text = Some(t.to_string());
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if matches!(stop, Some("end_turn") | Some("stop_sequence")) {
                    last_kind = Some("assistant_end");
                }
            }
            "user" => {
                last_user_marker = None;
                let content = v.get("message").and_then(|m| m.get("content"));
                match content {
                    Some(serde_json::Value::Array(blocks)) => {
                        for b in blocks {
                            match b.get("type").and_then(|t| t.as_str()) {
                                Some("tool_result") => {
                                    if let Some(id) = b.get("tool_use_id").and_then(|x| x.as_str())
                                    {
                                        open.remove(id);
                                    }
                                    let txt = tool_result_text(b);
                                    if let Some(marker) = classify_marker(&txt) {
                                        last_user_marker = Some(marker);
                                    }
                                    last_kind = Some("user_result");
                                }
                                Some("text") => {
                                    let t = b.get("text").and_then(|x| x.as_str()).unwrap_or("");
                                    if let Some(marker) = classify_marker(t) {
                                        last_user_marker = Some(marker);
                                    }
                                    last_kind = Some("user_text");
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(serde_json::Value::String(s)) => {
                        if let Some(marker) = classify_marker(s) {
                            last_user_marker = Some(marker);
                        }
                        last_kind = Some("user_text");
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    r.open_tool_count = open.len();

    if let Some((_, t)) = open.iter().find(|(_, t)| t.name != "AskUserQuestion") {
        r.pending_tool = Some(t.name.clone());
        r.pending_target = t.target.clone();
    }

    r.state = if let Some((_, q)) = open.iter().find(|(_, t)| t.name == "AskUserQuestion") {
        match &q.questions {
            Some(qs) if !qs.is_empty() => ClaudeState::PendingQuestion(AskUserQuestion {
                questions: qs.clone(),
            }),
            _ => ClaudeState::Working {
                open_tools: open_tool_count(open.len()),
            },
        }
    } else if !open.is_empty() {
        ClaudeState::Working {
            open_tools: open_tool_count(open.len()),
        }
    } else {
        match last_user_marker {
            Some("interrupted") => ClaudeState::Interrupted,
            Some("denied") => ClaudeState::Denied,
            _ => match last_kind {
                Some("assistant_end") => ClaudeState::Idle,
                Some("user_result") | Some("user_text") | Some("assistant_tool") => {
                    ClaudeState::Working { open_tools: 0 }
                }
                _ => ClaudeState::Idle,
            },
        }
    };

    r
}

fn open_tool_count(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}

fn claude_tool_target(name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    let input = input?;
    let s = match name {
        "Bash" => input.get("command").and_then(|x| x.as_str())?.to_string(),
        "Edit" | "Write" | "Read" => input.get("file_path").and_then(|x| x.as_str())?.to_string(),
        _ => return None,
    };
    Some(truncate(&s, 160))
}

fn tool_result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn classify_marker(text: &str) -> Option<&'static str> {
    if text.contains("[Request interrupted by user") {
        Some("interrupted")
    } else if text.contains("User rejected tool use") || text.contains("doesn't want to proceed") {
        Some("denied")
    } else {
        None
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CodexState {
    Idle,
    Working {
        turn_id: String,
    },
    Aborted {
        reason: String,
    },
    NeedsReplyMaybe {
        last_msg: String,
    },
    Error {
        message: String,
    },
    #[default]
    Unknown,
}

impl CodexState {
    pub fn label(&self) -> &'static str {
        match self {
            CodexState::Idle => "idle",
            CodexState::Working { .. } => "working",
            CodexState::Aborted { .. } => "aborted",
            CodexState::NeedsReplyMaybe { .. } => "needs_reply?",
            CodexState::Error { .. } => "error",
            CodexState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodexReduction {
    pub state: CodexState,
    pub last_agent_message: Option<String>,
    pub last_duration_ms: Option<u64>,
}

pub fn reduce_codex(rollout: &str) -> CodexReduction {
    let mut r = CodexReduction::default();
    let mut state = CodexState::Unknown;

    for line in rollout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
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
            Some("task_started") => {
                let turn_id = p
                    .get("turn_id")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
                    .to_string();
                state = CodexState::Working { turn_id };
            }
            Some("task_complete") => {
                if let Some(m) = p.get("last_agent_message").and_then(|x| x.as_str()) {
                    r.last_agent_message = Some(m.to_string());
                }
                r.last_duration_ms = p.get("duration_ms").and_then(value_u64);
                let msg = r.last_agent_message.clone().unwrap_or_default();
                state = if looks_like_question(&msg) {
                    CodexState::NeedsReplyMaybe {
                        last_msg: truncate(&msg, 300),
                    }
                } else {
                    CodexState::Idle
                };
            }
            Some("turn_aborted") => {
                let reason = p
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("aborted")
                    .to_string();
                state = CodexState::Aborted { reason };
            }
            Some("agent_message") => {
                if let Some(m) = p.get("message").and_then(|x| x.as_str()) {
                    r.last_agent_message = Some(m.to_string());
                }
            }
            Some("error") => {
                let m = p
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("error")
                    .to_string();
                state = CodexState::Error { message: m };
            }
            _ => {}
        }
    }

    r.state = state;
    r
}

fn value_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn looks_like_question(msg: &str) -> bool {
    let t = msg.trim_end();
    t.ends_with('?') || t.ends_with('？')
}

pub fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_idle_on_end_turn() {
        let t = r#"{"type":"assistant","sessionId":"s","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"#;
        let r = reduce_claude(t);
        assert_eq!(r.state, ClaudeState::Idle);
        assert_eq!(r.last_assistant_text.as_deref(), Some("done"));
    }

    #[test]
    fn claude_working_on_open_tool() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;
        let r = reduce_claude(t);
        assert_eq!(r.state, ClaudeState::Working { open_tools: 1 });
    }

    #[test]
    fn claude_tool_resolved_then_working() {
        let t = concat!(
            r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#
        );
        let r = reduce_claude(t);
        assert_eq!(r.state, ClaudeState::Working { open_tools: 0 });
    }

    #[test]
    fn claude_counts_tools_without_ids() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}},{"type":"tool_use","name":"Read","input":{"file_path":"Cargo.toml"}}]}}"#;
        let r = reduce_claude(t);
        assert_eq!(r.state, ClaudeState::Working { open_tools: 2 });
    }

    #[test]
    fn claude_preserves_marker_across_user_blocks() {
        let t = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"User rejected tool use"},{"type":"text","text":"ok"}]}}"#;
        let r = reduce_claude(t);
        assert_eq!(r.state, ClaudeState::Denied);
    }

    #[test]
    fn claude_pending_question() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{"questions":[{"header":"Scope","question":"How big?","multiSelect":false,"options":[{"label":"MVP","description":"small"},{"label":"Full","description":"big"}]}]}}]}}"#;
        let r = reduce_claude(t);
        match r.state {
            ClaudeState::PendingQuestion(q) => {
                assert_eq!(q.questions.len(), 1);
                assert_eq!(q.questions[0].options.len(), 2);
            }
            other => panic!("expected pending question, got {other:?}"),
        }
    }

    #[test]
    fn claude_pending_multi_question() {
        let t = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{"questions":[{"header":"Topology","question":"How to lay it out?","multiSelect":false,"options":[{"label":"Monorepo","description":"a"},{"label":"Separate git repos","description":"b"}]},{"header":"Mobile","question":"Stack?","multiSelect":false,"options":[{"label":"Capacitor + Vue3","description":"a"},{"label":"Ionic Vue + Capacitor","description":"b"},{"label":"Flutter / native","description":"c"},{"label":"Defer for now","description":"d"}]},{"header":"Scope","question":"What now?","multiSelect":false,"options":[{"label":"Backend","description":"a"},{"label":"All six","description":"b"}]}]}}]}}"#;
        let r = reduce_claude(t);
        match r.state {
            ClaudeState::PendingQuestion(q) => {
                assert_eq!(q.questions.len(), 3);
                assert_eq!(q.questions[0].options.len(), 2);
                assert_eq!(q.questions[1].options.len(), 4);
                assert_eq!(q.questions[1].options[0].label, "Capacitor + Vue3");
                assert_eq!(q.questions[2].options.len(), 2);
            }
            other => panic!("expected pending question, got {other:?}"),
        }
    }

    #[test]
    fn claude_skips_sidecars_and_captures_tip() {
        let t = concat!(
            r#"{"type":"last-prompt","leafUuid":"tip-123","lastPrompt":"x"}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"t"}"#,
            "\n",
            r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[]}}"#
        );
        let r = reduce_claude(t);
        assert_eq!(r.tip_uuid.as_deref(), Some("tip-123"));
        assert_eq!(r.state, ClaudeState::Idle);
    }

    #[test]
    fn codex_working_then_complete() {
        let t = concat!(
            r#"{"type":"event_msg","timestamp":"t","payload":{"type":"task_started","turn_id":"x"}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"t","payload":{"type":"agent_message","message":"here you go"}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"t","payload":{"type":"task_complete","last_agent_message":"here you go","duration_ms":1200}}"#
        );
        let r = reduce_codex(t);
        assert_eq!(r.state, CodexState::Idle);
        assert_eq!(r.last_duration_ms, Some(1200));
    }

    #[test]
    fn codex_accepts_string_duration() {
        let t = r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done","duration_ms":"1200"}}"#;
        let r = reduce_codex(t);
        assert_eq!(r.last_duration_ms, Some(1200));
    }

    #[test]
    fn codex_uses_unknown_for_missing_turn_id() {
        let t = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        let r = reduce_codex(t);
        assert_eq!(
            r.state,
            CodexState::Working {
                turn_id: "unknown".into()
            }
        );
    }

    #[test]
    fn codex_needs_reply_on_question() {
        let t = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"x"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Which approach do you prefer?"}}"#
        );
        let r = reduce_codex(t);
        assert!(matches!(r.state, CodexState::NeedsReplyMaybe { .. }));
    }

    #[test]
    fn codex_working_open_turn() {
        let t = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"abc"}}"#;
        let r = reduce_codex(t);
        assert_eq!(
            r.state,
            CodexState::Working {
                turn_id: "abc".into()
            }
        );
    }
}
