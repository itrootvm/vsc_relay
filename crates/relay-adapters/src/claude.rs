use relay_core::state::{reduce_claude, ClaudeReduction, ClaudeState};
use relay_discovery::registry::{mtime_secs, now_secs};
use std::path::Path;

const SUBAGENT_ACTIVE_SECS: u64 = 5;

pub fn tail_messages(jsonl_path: &Path, n: usize) -> Vec<(char, String)> {
    let text = match std::fs::read_to_string(jsonl_path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut msgs: Vec<(char, String)> = Vec::new();
    for line in text.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                    for b in content {
                        match b.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                    if !t.trim().is_empty() {
                                        msgs.push(('A', t.to_string()));
                                    }
                                }
                            }
                            Some("tool_use") => {
                                let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                                msgs.push(('T', name.to_string()));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("user") => {
                if v.get("isMeta").and_then(|x| x.as_bool()) == Some(true) {
                    continue;
                }
                match v.pointer("/message/content") {
                    Some(serde_json::Value::String(s)) => {
                        if !s.trim().is_empty() {
                            msgs.push(('U', s.clone()));
                        }
                    }
                    Some(serde_json::Value::Array(arr)) => {
                        for b in arr {
                            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                    let t = t.trim();
                                    if !t.is_empty() && !t.starts_with('<') {
                                        msgs.push(('U', t.to_string()));
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let start = msgs.len().saturating_sub(n);
    msgs.split_off(start)
}

pub struct ClaudeReadResult {
    pub reduction: ClaudeReduction,
    pub state: ClaudeState,
    pub subagents_active: bool,
}

pub fn read_state(jsonl_path: &Path, pid: Option<u32>) -> anyhow::Result<ClaudeReadResult> {
    let text = std::fs::read_to_string(jsonl_path)?;
    let reduction = reduce_claude(&text);

    let subagents_active = subagents_active(jsonl_path);
    let state = refine(&reduction, jsonl_path, pid, subagents_active);

    Ok(ClaudeReadResult {
        reduction,
        state,
        subagents_active,
    })
}

fn refine(
    reduction: &ClaudeReduction,
    _jsonl_path: &Path,
    _pid: Option<u32>,
    subagents_active: bool,
) -> ClaudeState {
    match &reduction.state {
        ClaudeState::Working { open_tools } if *open_tools > 0 => {
            if subagents_active {
                return ClaudeState::SubagentRunning;
            }
            reduction.state.clone()
        }
        other => other.clone(),
    }
}

fn subagents_active(jsonl_path: &Path) -> bool {
    let dir = match (jsonl_path.parent(), jsonl_path.file_stem()) {
        (Some(parent), Some(stem)) => parent.join(stem).join("subagents"),
        _ => return false,
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let now = now_secs();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(m) = mtime_secs(&path) {
            if now.saturating_sub(m) <= SUBAGENT_ACTIVE_SECS {
                return true;
            }
        }
    }
    false
}
