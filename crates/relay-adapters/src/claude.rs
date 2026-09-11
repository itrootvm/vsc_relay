use crate::stepbuild::{append_assistant, flush_assistant, looks_like_failure};
use relay_compass::{
    health_steer_id, ContextFlags, SemanticStep, SourceTurnId, StepRole, ToolKind, UserOrigin,
};
#[cfg(test)]
use relay_core::state::reduce_claude;
use relay_core::state::{truncate, ClaudeReducer, ClaudeReduction, ClaudeState};
use relay_discovery::registry::{mtime_secs, now_secs};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::Cursor;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

const SUBAGENT_ACTIVE_SECS: u64 = 5;
const USER_STEP_CHARS: usize = 262_144;
const ASSISTANT_STEP_CHARS: usize = 65_536;
const TOOL_RESULT_CHARS: usize = 32_768;
const FULL_FIDELITY_RECENT_STEPS: usize = 512;
const AGED_NARRATIVE_CHARS: usize = 2_048;
const AGED_RECEIPT_CHARS: usize = 512;

fn push_tail_message(
    messages: &mut VecDeque<(char, String)>,
    limit: usize,
    message: (char, String),
) {
    if limit == 0 {
        return;
    }
    messages.push_back(message);
    if messages.len() > limit {
        messages.pop_front();
    }
}

fn answered_questions(v: &serde_json::Value) -> Option<String> {
    let answers = v.pointer("/toolUseResult/answers")?.as_object()?;
    let text = answers
        .values()
        .filter_map(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

pub fn session_cwd(jsonl_path: &Path) -> Option<std::path::PathBuf> {
    let file = File::open(jsonl_path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    for _ in 0..200 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(cwd) = value.get("cwd").and_then(|cwd| cwd.as_str()) {
            if !cwd.trim().is_empty() {
                return Some(std::path::PathBuf::from(cwd));
            }
        }
    }
    None
}

pub fn latest_plan(jsonl_path: &Path) -> Option<String> {
    let file = File::open(jsonl_path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut latest: Option<String> = None;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if !line.contains("planContent") {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let Some(plan) = value
            .pointer("/attachment/planContent")
            .and_then(|plan| plan.as_str())
        {
            if !plan.trim().is_empty() {
                latest = Some(plan.to_string());
            }
        }
    }
    latest
}

pub fn tail_messages(jsonl_path: &Path, n: usize) -> Vec<(char, String)> {
    let file = match File::open(jsonl_path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    tail_messages_from(BufReader::new(file), n)
}

pub fn tail_messages_window(jsonl_path: &Path, n: usize, max_bytes: u64) -> Vec<(char, String)> {
    let Ok(mut file) = File::open(jsonl_path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if len <= max_bytes {
        return tail_messages_from(BufReader::new(file), n);
    }
    if file.seek(SeekFrom::Start(len - max_bytes)).is_err() {
        return Vec::new();
    }
    let mut reader = BufReader::new(file);
    let mut partial = String::new();
    if reader.read_line(&mut partial).is_err() {
        return Vec::new();
    }
    tail_messages_from(reader, n)
}

fn tail_messages_from<R: BufRead>(mut reader: R, n: usize) -> Vec<(char, String)> {
    let mut msgs = VecDeque::<(char, String)>::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
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
                                        push_tail_message(&mut msgs, n, ('A', t.to_string()));
                                    }
                                }
                            }
                            Some("tool_use") => {
                                let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("tool");
                                push_tail_message(&mut msgs, n, ('T', name.to_string()));
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
                            push_tail_message(&mut msgs, n, ('U', s.clone()));
                        }
                    }
                    Some(serde_json::Value::Array(arr)) => {
                        let mut has_tool_result = false;
                        for b in arr {
                            match b.get("type").and_then(|t| t.as_str()) {
                                Some("text") => {
                                    if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                        let t = t.trim();
                                        if !t.is_empty() && !t.starts_with('<') {
                                            push_tail_message(&mut msgs, n, ('U', t.to_string()));
                                        }
                                    }
                                }
                                Some("tool_result") => has_tool_result = true,
                                _ => {}
                            }
                        }
                        if has_tool_result {
                            if let Some(answer) = answered_questions(&v) {
                                push_tail_message(&mut msgs, n, ('U', answer));
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    msgs.into_iter().collect()
}

#[derive(Debug, Clone)]
pub struct UserTurn {
    pub source_turn_id: SourceTurnId,
    pub block_index: u16,
    pub text: String,
    pub origin: UserOrigin,
    pub context_flags: ContextFlags,
}

const CONTEXT_MARKERS: &[(&str, ContextFlags)] = &[
    ("<ide_opened_file>", ContextFlags::IDE_OPENED_FILE),
    ("<ide_selection>", ContextFlags::IDE_SELECTION),
    ("<ide_diagnostics>", ContextFlags::IDE_DIAGNOSTICS),
    ("<local-command-caveat>", ContextFlags::LOCAL_COMMAND),
    ("<command-name>", ContextFlags::LOCAL_COMMAND),
    ("<command-message>", ContextFlags::LOCAL_COMMAND),
    ("<command-args>", ContextFlags::LOCAL_COMMAND),
    ("<local-command-stdout>", ContextFlags::LOCAL_COMMAND),
    ("<system-reminder>", ContextFlags::SYSTEM_REMINDER),
];

fn marker_flags(text: &str) -> Option<ContextFlags> {
    let trimmed = text.trim_start();
    CONTEXT_MARKERS
        .iter()
        .find_map(|(prefix, flag)| trimmed.starts_with(prefix).then_some(*flag))
}

fn turn_context_flags(blocks: &[serde_json::Value]) -> ContextFlags {
    let mut flags = ContextFlags::empty();
    for block in blocks {
        if block.get("type").and_then(|kind| kind.as_str()) == Some("text") {
            if let Some(text) = block.get("text").and_then(|value| value.as_str()) {
                if let Some(flag) = marker_flags(text) {
                    flags.insert(flag);
                }
            }
        }
    }
    flags
}

fn user_block_origin(text: &str) -> UserOrigin {
    if health_steer_id(text).is_some() {
        UserOrigin::Controller
    } else {
        UserOrigin::Human
    }
}

fn tagged_user_step(
    index: u32,
    text: String,
    turn: SourceTurnId,
    block_index: u16,
    flags: ContextFlags,
) -> SemanticStep {
    let mut step = SemanticStep::new(index, StepRole::User, text);
    step.source_turn_id = turn;
    step.block_index = block_index;
    step.user_origin = user_block_origin(&step.text);
    step.context_flags = flags;
    step
}

pub fn user_turns(jsonl_path: &Path) -> Vec<UserTurn> {
    match File::open(jsonl_path) {
        Ok(file) => user_turns_reader(BufReader::new(file)),
        Err(_) => Vec::new(),
    }
}

fn user_turns_reader<R: BufRead>(mut reader: R) -> Vec<UserTurn> {
    let mut turns = Vec::new();
    let mut line = String::new();
    let mut turn_id: SourceTurnId = 0;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(|kind| kind.as_str()) != Some("user")
            || value.get("isMeta").and_then(|flag| flag.as_bool()) == Some(true)
        {
            continue;
        }
        let this = turn_id;
        turn_id = turn_id.wrapping_add(1);
        match value.pointer("/message/content") {
            Some(serde_json::Value::String(text)) => {
                let text = text.trim();
                if !text.is_empty() && retain_typed_user_step(text) {
                    turns.push(UserTurn {
                        source_turn_id: this,
                        block_index: 0,
                        text: text.to_string(),
                        origin: user_block_origin(text),
                        context_flags: ContextFlags::empty(),
                    });
                }
            }
            Some(serde_json::Value::Array(blocks)) => {
                let flags = turn_context_flags(blocks);
                let mut has_tool_result = false;
                for (block_index, block) in blocks.iter().enumerate() {
                    match block.get("type").and_then(|kind| kind.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|value| value.as_str()) {
                                let text = text.trim();
                                if !text.is_empty() && retain_typed_user_step(text) {
                                    turns.push(UserTurn {
                                        source_turn_id: this,
                                        block_index: block_index as u16,
                                        text: text.to_string(),
                                        origin: user_block_origin(text),
                                        context_flags: flags,
                                    });
                                }
                            }
                        }
                        Some("tool_result") => has_tool_result = true,
                        _ => {}
                    }
                }
                if has_tool_result {
                    if let Some(answer) = answered_questions(&value) {
                        turns.push(UserTurn {
                            source_turn_id: this,
                            block_index: 0,
                            text: answer,
                            origin: UserOrigin::ToolAnswer,
                            context_flags: ContextFlags::empty(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    turns
}

pub fn user_messages(jsonl_path: &Path) -> Vec<String> {
    user_turns(jsonl_path)
        .into_iter()
        .filter(|turn| turn.origin == UserOrigin::Human)
        .map(|turn| turn.text)
        .collect()
}

fn first_input_string<'a>(input: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| input.get(*key).and_then(|value| value.as_str()))
}

fn claude_tool_target(_name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    let input = input?;
    let parts = [
        first_input_string(input, &["description", "task", "title"]),
        first_input_string(input, &["prompt", "message", "instructions"]),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let s = if !parts.is_empty() {
        parts.join(" ")
    } else {
        first_input_string(
            input,
            &[
                "command",
                "cmd",
                "file_path",
                "path",
                "uri",
                "pattern",
                "query",
            ],
        )?
        .to_string()
    };
    Some(truncate(&s, 160))
}

fn claude_tool_kind(name: &str, input: Option<&serde_json::Value>) -> ToolKind {
    if let Some(input) = input {
        let has = |key: &str| input.get(key).is_some();
        let delegated_shape = (has("prompt") || has("message") || has("instructions"))
            && (has("description")
                || has("subagent_type")
                || has("agent_id")
                || has("team_name")
                || has("fork_turns"));
        if delegated_shape || has("tasks") || has("delegation") {
            return ToolKind::Delegate;
        }
        if has("old_string")
            || has("new_string")
            || has("patch")
            || has("edits")
            || has("content") && (has("file_path") || has("path"))
        {
            return ToolKind::Modify;
        }
        if has("command") || has("cmd") {
            return ToolKind::Execute;
        }
        if has("pattern") || has("glob") || has("query") {
            return ToolKind::Search;
        }
        if has("file_path") || has("path") || has("uri") {
            return ToolKind::Inspect;
        }
    }

    match name {
        "Read" => ToolKind::Inspect,
        "Grep" | "Glob" => ToolKind::Search,
        "Bash" => ToolKind::Execute,
        "Edit" | "Write" => ToolKind::Modify,
        "Agent" | "Task" | "Workflow" => ToolKind::Delegate,
        _ => ToolKind::Other,
    }
}

fn xml_tag<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

fn async_delegate_notification(text: &str) -> Option<(&str, &str, bool)> {
    let text = text.trim();
    if !text.starts_with("<task-notification>") {
        return None;
    }
    let correlation_id = xml_tag(text, "tool-use-id")?.trim();
    let status = xml_tag(text, "status")?.trim();
    let result = xml_tag(text, "result").unwrap_or("").trim();
    if correlation_id.is_empty() {
        return None;
    }
    Some((correlation_id, result, status != "completed"))
}

fn tool_result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => semantic_excerpt(s, TOOL_RESULT_CHARS),
        Some(serde_json::Value::Array(a)) => {
            let mut output = String::new();
            for text in a
                .iter()
                .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
            {
                let used = output.chars().count();
                if used >= TOOL_RESULT_CHARS {
                    break;
                }
                if !output.is_empty() {
                    output.push(' ');
                }
                output.push_str(&truncate(text, TOOL_RESULT_CHARS - used));
            }
            truncate(&output, TOOL_RESULT_CHARS)
        }
        _ => String::new(),
    }
}

fn semantic_excerpt(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.trim().to_string();
    }
    let head = max_chars / 2;
    let tail = max_chars.saturating_sub(head);
    let mut output = text.chars().take(head).collect::<String>();
    output.push('…');
    let mut suffix = text.chars().rev().take(tail).collect::<Vec<_>>();
    suffix.reverse();
    output.extend(suffix);
    output
}

fn tool_result_looks_like_failure(block: &serde_json::Value) -> bool {
    match block.get("content") {
        Some(serde_json::Value::String(text)) => looks_like_failure(text),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|text| text.as_str()))
            .any(looks_like_failure),
        _ => false,
    }
}

fn append_semantic_assistant(pending: &mut Option<String>, text: &str) {
    let used = pending
        .as_deref()
        .map(|value| value.chars().count())
        .unwrap_or(0);
    if used >= ASSISTANT_STEP_CHARS {
        return;
    }
    let text = truncate(text, ASSISTANT_STEP_CHARS - used);
    append_assistant(pending, &text);
    if let Some(value) = pending {
        *value = truncate(value, ASSISTANT_STEP_CHARS);
    }
}

fn compact_aged_step(step: &mut SemanticStep) {
    let limit = match step.role {
        StepRole::User => return,
        StepRole::Assistant | StepRole::DelegateResult => AGED_NARRATIVE_CHARS,
        StepRole::ToolResult | StepRole::ToolUse => AGED_RECEIPT_CHARS,
    };
    if step.text.chars().count() > limit {
        step.text = semantic_excerpt(&step.text, limit);
    }
}

fn retain_typed_user_step(text: &str) -> bool {
    if health_steer_id(text).is_some() {
        return true;
    }
    if marker_flags(text).is_some() {
        return false;
    }
    !relay_compass::is_context_noise(text)
}

pub fn semantic_steps(jsonl_path: &Path) -> Vec<SemanticStep> {
    match File::open(jsonl_path) {
        Ok(file) => parse_claude_reader(BufReader::new(file)),
        Err(_) => Vec::new(),
    }
}

pub fn semantic_steps_tail(jsonl_path: &Path, max_bytes: u64) -> Vec<SemanticStep> {
    semantic_steps_window(jsonl_path, 0, max_bytes)
}

pub fn semantic_steps_window(
    jsonl_path: &Path,
    head_bytes: u64,
    tail_bytes: u64,
) -> Vec<SemanticStep> {
    let Ok(mut file) = File::open(jsonl_path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if len <= head_bytes.saturating_add(tail_bytes) {
        return parse_claude_reader(BufReader::new(file));
    }
    let mut window = String::new();
    if head_bytes > 0 {
        let mut head = vec![0u8; head_bytes as usize];
        if file.read_exact(&mut head).is_ok() {
            let text = String::from_utf8_lossy(&head);
            if let Some(last_newline) = text.rfind('\n') {
                window.push_str(&text[..=last_newline]);
            }
        }
    }
    if file.seek(SeekFrom::Start(len - tail_bytes)).is_err() {
        return parse_claude_reader(Cursor::new(window));
    }
    let mut reader = BufReader::new(file);
    let mut partial = String::new();
    if reader.read_line(&mut partial).is_err() {
        return parse_claude_reader(Cursor::new(window));
    }
    let mut tail = String::new();
    if reader.read_to_string(&mut tail).is_err() {
        return parse_claude_reader(Cursor::new(window));
    }
    window.push_str(&tail);
    parse_claude_reader(Cursor::new(window))
}

pub fn first_human_goal(jsonl_path: &Path, head_bytes: u64) -> Option<String> {
    semantic_steps_window(jsonl_path, head_bytes, 0)
        .into_iter()
        .find(|step| {
            step.role == relay_compass::StepRole::User
                && step.user_origin == relay_compass::UserOrigin::Human
                && relay_compass::is_substantive_goal(&step.text)
        })
        .map(|step| step.text)
}

#[cfg(test)]
fn parse_claude_steps(text: &str) -> Vec<SemanticStep> {
    parse_claude_reader(Cursor::new(text))
}

fn parse_claude_reader<R: BufRead>(mut reader: R) -> Vec<SemanticStep> {
    let mut steps: Vec<SemanticStep> = Vec::new();
    let mut index: u32 = 0;
    let mut user_turn: u32 = 0;
    let mut pending: Option<String> = None;
    let mut delegate_targets = BTreeMap::<String, String>::new();
    let mut call_kinds = BTreeMap::<String, ToolKind>::new();
    let mut compacted_until = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if v.get("isApiErrorMessage").and_then(|x| x.as_bool()) == Some(true) {
                    flush_assistant(&mut steps, &mut index, &mut pending);
                    let message = v
                        .pointer("/message/content")
                        .and_then(|c| c.as_array())
                        .and_then(|a| a.first())
                        .and_then(|b| b.get("text"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("API error");
                    let message = truncate(message, TOOL_RESULT_CHARS);
                    let mut step = SemanticStep::new(index, StepRole::ToolResult, message);
                    step.is_error = true;
                    steps.push(step);
                    index += 1;
                    continue;
                }
                if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                    for b in content {
                        match b.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                    let t = t.trim();
                                    if !relay_compass::is_transcript_noise(t) {
                                        append_semantic_assistant(&mut pending, t);
                                    }
                                }
                            }
                            Some("tool_use") => {
                                flush_assistant(&mut steps, &mut index, &mut pending);
                                let name = b
                                    .get("name")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("tool")
                                    .to_string();
                                let target = claude_tool_target(&name, b.get("input"));
                                let mut step =
                                    SemanticStep::new(index, StepRole::ToolUse, name.clone());
                                step.tool_kind = claude_tool_kind(&name, b.get("input"));
                                step.tool_name = Some(name);
                                step.correlation_id =
                                    b.get("id").and_then(|x| x.as_str()).map(str::to_string);
                                step.tool_target = target;
                                if let Some(id) = step.correlation_id.as_ref() {
                                    call_kinds.insert(id.clone(), step.tool_kind);
                                }
                                if step.tool_kind == ToolKind::Delegate {
                                    if let (Some(id), Some(target)) =
                                        (step.correlation_id.as_ref(), step.tool_target.as_ref())
                                    {
                                        delegate_targets.insert(id.clone(), target.clone());
                                    }
                                }
                                steps.push(step);
                                index += 1;
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
                let turn_id = user_turn;
                user_turn = user_turn.wrapping_add(1);
                match v.pointer("/message/content") {
                    Some(serde_json::Value::String(s)) => {
                        let s = s.trim();
                        if let Some((correlation_id, result, is_error)) =
                            async_delegate_notification(s)
                        {
                            flush_assistant(&mut steps, &mut index, &mut pending);
                            let mut step = SemanticStep::new(
                                index,
                                StepRole::DelegateResult,
                                truncate(result, ASSISTANT_STEP_CHARS),
                            );
                            step.tool_kind = ToolKind::Delegate;
                            step.correlation_id = Some(correlation_id.to_string());
                            step.tool_target = delegate_targets.get(correlation_id).cloned();
                            step.is_error = is_error;
                            steps.push(step);
                            index += 1;
                        } else if retain_typed_user_step(s) {
                            flush_assistant(&mut steps, &mut index, &mut pending);
                            steps.push(tagged_user_step(
                                index,
                                truncate(s, USER_STEP_CHARS),
                                turn_id,
                                0,
                                ContextFlags::empty(),
                            ));
                            index += 1;
                        }
                    }
                    Some(serde_json::Value::Array(arr)) => {
                        let context_flags = turn_context_flags(arr);
                        let mut has_tool_result = false;
                        for (block_index, b) in arr.iter().enumerate() {
                            match b.get("type").and_then(|t| t.as_str()) {
                                Some("text") => {
                                    if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                        let t = t.trim();
                                        if let Some((correlation_id, result, is_error)) =
                                            async_delegate_notification(t)
                                        {
                                            flush_assistant(&mut steps, &mut index, &mut pending);
                                            let mut step = SemanticStep::new(
                                                index,
                                                StepRole::DelegateResult,
                                                truncate(result, ASSISTANT_STEP_CHARS),
                                            );
                                            step.tool_kind = ToolKind::Delegate;
                                            step.correlation_id = Some(correlation_id.to_string());
                                            step.tool_target =
                                                delegate_targets.get(correlation_id).cloned();
                                            step.is_error = is_error;
                                            steps.push(step);
                                            index += 1;
                                        } else if retain_typed_user_step(t) {
                                            flush_assistant(&mut steps, &mut index, &mut pending);
                                            steps.push(tagged_user_step(
                                                index,
                                                truncate(t, USER_STEP_CHARS),
                                                turn_id,
                                                block_index as u16,
                                                context_flags,
                                            ));
                                            index += 1;
                                        }
                                    }
                                }
                                Some("tool_result") => {
                                    has_tool_result = true;
                                    let async_launched = v
                                        .pointer("/toolUseResult/status")
                                        .and_then(|value| value.as_str())
                                        == Some("async_launched")
                                        || v.pointer("/toolUseResult/isAsync")
                                            .and_then(|value| value.as_bool())
                                            == Some(true);
                                    if async_launched {
                                        continue;
                                    }
                                    flush_assistant(&mut steps, &mut index, &mut pending);
                                    let txt = tool_result_text(b);
                                    let explicit_error =
                                        b.get("is_error").and_then(|x| x.as_bool());
                                    let interrupted = v
                                        .pointer("/toolUseResult/interrupted")
                                        .and_then(|x| x.as_bool());
                                    let exit_code = ["exitCode", "exit_code", "code"]
                                        .into_iter()
                                        .find_map(|key| {
                                            v.pointer(&format!("/toolUseResult/{key}"))
                                                .and_then(|x| x.as_i64())
                                        });
                                    let has_structured_status = explicit_error.is_some()
                                        || interrupted.is_some()
                                        || exit_code.is_some();
                                    let is_error = explicit_error == Some(true)
                                        || interrupted == Some(true)
                                        || exit_code.is_some_and(|code| code != 0)
                                        || (!has_structured_status
                                            && tool_result_looks_like_failure(b));
                                    let correlation_id = b
                                        .get("tool_use_id")
                                        .and_then(|x| x.as_str())
                                        .map(str::to_string);
                                    let kind = correlation_id
                                        .as_ref()
                                        .and_then(|id| call_kinds.get(id))
                                        .copied()
                                        .unwrap_or(ToolKind::Other);
                                    let role = if kind == ToolKind::Delegate {
                                        StepRole::DelegateResult
                                    } else {
                                        StepRole::ToolResult
                                    };
                                    let mut step = SemanticStep::new(index, role, txt);
                                    step.correlation_id = correlation_id;
                                    step.tool_kind = kind;
                                    step.is_error = is_error;
                                    steps.push(step);
                                    index += 1;
                                }
                                _ => {}
                            }
                        }
                        if has_tool_result {
                            if let Some(answer) = answered_questions(&v) {
                                flush_assistant(&mut steps, &mut index, &mut pending);
                                let mut step = tagged_user_step(
                                    index,
                                    truncate(&answer, USER_STEP_CHARS),
                                    turn_id,
                                    0,
                                    ContextFlags::empty(),
                                );
                                step.user_origin = UserOrigin::ToolAnswer;
                                steps.push(step);
                                index += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        let stable_end = steps.len().saturating_sub(FULL_FIDELITY_RECENT_STEPS);
        for step in &mut steps[compacted_until..stable_end] {
            compact_aged_step(step);
        }
        compacted_until = stable_end;
    }
    flush_assistant(&mut steps, &mut index, &mut pending);
    steps
}

pub struct ClaudeReadResult {
    pub reduction: ClaudeReduction,
    pub state: ClaudeState,
    pub subagents_active: bool,
}

const PENDING_RETAIN_BYTES: usize = 256 * 1024;
const REDUCTION_CACHE_CAP: usize = 64;

struct CachedReduction {
    len: u64,
    modified_ns: u128,
    pending: Vec<u8>,
    scanned: usize,
    reducer: ClaudeReducer,
    used: u64,
}

impl CachedReduction {
    fn fresh() -> Self {
        CachedReduction {
            len: 0,
            modified_ns: 0,
            pending: Vec::new(),
            scanned: 0,
            reducer: ClaudeReducer::default(),
            used: 0,
        }
    }
}

static REDUCTION_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedReduction>>> = OnceLock::new();
static REDUCTION_TICK: AtomicU64 = AtomicU64::new(0);

fn modified_ns(metadata: &std::fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn release_pending(entry: &mut CachedReduction) {
    if entry.pending.capacity() <= PENDING_RETAIN_BYTES {
        return;
    }
    if entry.pending.is_empty() {
        entry.pending = Vec::new();
        entry.scanned = 0;
        return;
    }
    if entry.pending.capacity() > entry.pending.len().saturating_mul(2) {
        entry.pending.shrink_to_fit();
    }
}

fn push_complete_lines(entry: &mut CachedReduction, bytes: &[u8]) {
    let mut index = entry.scanned.min(entry.pending.len());
    entry.pending.extend_from_slice(bytes);
    let mut start = 0usize;
    while index < entry.pending.len() {
        if entry.pending[index] == b'\n' {
            if let Ok(line) = std::str::from_utf8(&entry.pending[start..index]) {
                entry.reducer.push_line(line);
            }
            start = index + 1;
        }
        index += 1;
    }
    if start > 0 {
        entry.pending.drain(..start);
    }
    entry.scanned = entry.pending.len();
    release_pending(entry);
}

fn flush_trailing_line(entry: &mut CachedReduction) {
    if entry.pending.is_empty() {
        return;
    }
    let complete = std::str::from_utf8(&entry.pending)
        .ok()
        .is_some_and(|line| serde_json::from_str::<serde::de::IgnoredAny>(line).is_ok());
    if !complete {
        return;
    }
    if let Ok(line) = std::str::from_utf8(&entry.pending) {
        entry.reducer.push_line(line);
    }
    entry.pending.clear();
    entry.scanned = 0;
    release_pending(entry);
}

fn evict_cold_entry(cache: &mut HashMap<PathBuf, CachedReduction>, keep: &Path) {
    while cache.len() >= REDUCTION_CACHE_CAP {
        let victim = cache
            .iter()
            .filter(|(path, _)| path.as_path() != keep)
            .min_by_key(|(_, entry)| entry.used)
            .map(|(path, _)| path.clone());
        match victim {
            Some(path) => {
                cache.remove(&path);
            }
            None => return,
        }
    }
}

fn incremental_reduction(jsonl_path: &Path) -> anyhow::Result<ClaudeReduction> {
    let metadata = std::fs::metadata(jsonl_path)?;
    let len = metadata.len();
    let modified = modified_ns(&metadata);
    let cache = REDUCTION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tick = REDUCTION_TICK.fetch_add(1, Ordering::Relaxed);
    evict_cold_entry(&mut cache, jsonl_path);
    let entry = cache
        .entry(jsonl_path.to_path_buf())
        .or_insert_with(CachedReduction::fresh);
    if len < entry.len || (len == entry.len && modified != entry.modified_ns) {
        *entry = CachedReduction::fresh();
    }
    entry.used = tick;
    if len > entry.len {
        let mut file = File::open(jsonl_path)?;
        file.seek(SeekFrom::Start(entry.len))?;
        let delta = len - entry.len;
        let mut remaining = delta;
        let mut appended = vec![0u8; 1024 * 1024];
        while remaining > 0 {
            let limit = remaining.min(appended.len() as u64) as usize;
            let read = file.read(&mut appended[..limit])?;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Claude transcript changed during incremental read",
                )
                .into());
            }
            push_complete_lines(entry, &appended[..read]);
            remaining -= read as u64;
        }
        flush_trailing_line(entry);
        entry.len = len;
    }
    entry.modified_ns = modified;
    Ok(entry.reducer.reduction())
}

pub fn read_state(jsonl_path: &Path, pid: Option<u32>) -> anyhow::Result<ClaudeReadResult> {
    let reduction = incremental_reduction(jsonl_path)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tail_read_never_starts_mid_record() {
        let dir = std::env::temp_dir().join(format!("claude-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("session.jsonl");

        let record = |n: u32, text: &str| {
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": [{"type": "text", "text": format!("{text} {n}")}]}
            })
            .to_string()
        };
        let body: String = (0..200)
            .map(|n| record(n, "a padded line of session history"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{body}\n")).expect("write");
        let whole = std::fs::metadata(&path).expect("meta").len();

        let full = semantic_steps(&path);
        let tail = semantic_steps_tail(&path, whole / 4);
        let untouched = semantic_steps_tail(&path, whole * 2);

        assert!(!full.is_empty(), "the fixture parses at all");
        assert!(
            !tail.is_empty() && tail.len() < full.len(),
            "a quarter window returns recent steps only: {} of {}",
            tail.len(),
            full.len()
        );
        assert_eq!(
            untouched.len(),
            full.len(),
            "a window larger than the file reads the whole file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_only_interactive_answers() {
        let value = serde_json::json!({
            "toolUseResult": {
                "answers": {
                    "What now?": "Build the complete dossier",
                    "Where?": "Both interfaces"
                }
            }
        });
        assert_eq!(
            answered_questions(&value).as_deref(),
            Some("Build the complete dossier\nBoth interfaces")
        );
        assert!(answered_questions(&serde_json::json!({"toolUseResult": {}})).is_none());
    }

    #[test]
    fn incremental_state_matches_full_reduction_after_append() {
        let unique = format!(
            "relay-claude-incremental-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        let first = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"build it\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Read\"}]}}\n"
        );
        std::fs::write(&path, first).unwrap();
        let first_incremental = incremental_reduction(&path).unwrap();
        assert_eq!(first_incremental, reduce_claude(first));

        let second = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"ok\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n"
        );
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(second.as_bytes()).unwrap();
        let incremental = incremental_reduction(&path).unwrap();
        assert_eq!(incremental, reduce_claude(&format!("{first}{second}")));
        assert_eq!(incremental.state, ClaudeState::Idle);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_multi_megabyte_line_is_reduced_once_and_releases_its_buffer() {
        let unique = format!(
            "relay-claude-huge-line-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        let filler = "x".repeat(4 * 1024 * 1024);
        let transcript = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"build it\"}}}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{filler}\"}}]}}}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"end_turn\",\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}}}}\n"
        );
        std::fs::write(&path, &transcript).unwrap();

        let incremental = incremental_reduction(&path).unwrap();
        assert_eq!(incremental, reduce_claude(&transcript));
        let _ = std::fs::remove_file(path);

        let mut entry = CachedReduction::fresh();
        for chunk in transcript.as_bytes().chunks(1024 * 1024) {
            push_complete_lines(&mut entry, chunk);
        }
        flush_trailing_line(&mut entry);
        assert_eq!(entry.reducer.reduction(), reduce_claude(&transcript));
        assert!(entry.pending.is_empty());
        assert!(
            entry.pending.capacity() <= PENDING_RETAIN_BYTES,
            "pending kept {} bytes of capacity",
            entry.pending.capacity()
        );
    }

    #[test]
    fn the_reduction_cache_stays_bounded_and_keeps_the_live_transcript() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let line = "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n";
        let mut paths = Vec::new();
        for index in 0..(REDUCTION_CACHE_CAP + 8) {
            let path = std::env::temp_dir().join(format!(
                "relay-claude-cap-{}-{stamp}-{index}.jsonl",
                std::process::id()
            ));
            std::fs::write(&path, line).unwrap();
            incremental_reduction(&path).unwrap();
            paths.push(path);
        }

        let cache = REDUCTION_CACHE.get().unwrap().lock().unwrap();
        assert!(
            cache.len() <= REDUCTION_CACHE_CAP,
            "cache grew to {}",
            cache.len()
        );
        assert!(cache.contains_key(paths.last().unwrap()));
        drop(cache);
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn semantic_steps_golden() {
        let lines = [
            r#"{"type":"user","message":{"content":"fix the postgres login bug"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"session resumed context"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking at it."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Found the cause."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"error: assertion failed"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"All tests pass now."}]}}"#,
        ]
        .join("\n");
        let steps = parse_claude_steps(&lines);
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
        for (i, s) in steps.iter().enumerate() {
            assert_eq!(s.index as usize, i);
        }
        assert_eq!(steps[0].text, "fix the postgres login bug");
        assert_eq!(steps[1].text, "Looking at it.\nFound the cause.");
        assert_eq!(steps[2].tool_name.as_deref(), Some("Bash"));
        assert_eq!(steps[2].correlation_id.as_deref(), Some("t1"));
        assert_eq!(steps[3].correlation_id.as_deref(), Some("t1"));
        assert_eq!(steps[2].tool_target.as_deref(), Some("cargo test"));
        assert!(steps[3].is_error);
        assert_eq!(steps[4].text, "All tests pass now.");
    }

    #[test]
    fn semantic_steps_retain_controller_challenge_for_ledger() {
        let lines = [
            r#"{"type":"user","message":{"content":"build and verify parser.rs"}}"#,
            r#"{"type":"user","message":{"content":"[VSC_RELAY_HEALTH_STEER v2 id=obl-0-1]\nRun one bounded check."}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call-7","name":"Bash","input":{"command":"fresh smoke"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call-7","content":"ok"}]}}"#,
        ]
        .join("\n");

        let steps = parse_claude_steps(&lines);

        assert_eq!(steps[0].role, StepRole::User);
        assert_eq!(steps[1].role, StepRole::User);
        assert_eq!(health_steer_id(&steps[1].text), Some("obl-0-1"));
        assert_eq!(steps[2].correlation_id.as_deref(), Some("call-7"));
        assert_eq!(steps[3].correlation_id.as_deref(), Some("call-7"));
    }

    #[test]
    fn structured_success_is_not_overridden_by_error_word_in_stdout() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call-7","name":"Bash","input":{"command":"browser smoke"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call-7","is_error":false,"content":"{\"consoleErrors\":[]}"}]},"toolUseResult":{"stdout":"{\"consoleErrors\":[]}","stderr":"","interrupted":false}}"#,
        ]
        .join("\n");

        let steps = parse_claude_steps(&lines);

        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].role, StepRole::ToolResult);
        assert!(!steps[1].is_error);
    }

    #[test]
    fn tool_result_never_becomes_user() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/main.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"file contents"}]}}"#,
        ]
        .join("\n");
        let steps = parse_claude_steps(&lines);
        assert!(steps.iter().all(|s| s.role != StepRole::User));
        assert_eq!(
            steps
                .iter()
                .filter(|s| s.role == StepRole::ToolResult)
                .count(),
            1
        );
    }

    #[test]
    fn consecutive_assistant_text_aggregates() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"one"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}"#,
        ]
        .join("\n");
        let steps = parse_claude_steps(&lines);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].role, StepRole::Assistant);
        assert_eq!(steps[0].text, "one\ntwo");
    }

    #[test]
    fn answered_question_is_user_intent() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{"questions":[]}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"q1","content":"Your questions have been answered:"}]},"toolUseResult":{"answers":{"Scope":"Both interfaces"}}}"#,
        ]
        .join("\n");
        let steps = parse_claude_steps(&lines);
        let users: Vec<&str> = steps
            .iter()
            .filter(|s| s.role == StepRole::User)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(users, vec!["Both interfaces"]);
    }

    #[test]
    fn api_error_is_runtime_result_not_user() {
        let lines = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"API Error: Connection closed mid-response."}]}}"#;
        let steps = parse_claude_steps(lines);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].role, StepRole::ToolResult);
        assert!(steps[0].is_error);
    }

    #[test]
    fn async_subagent_notification_is_delegate_report_not_user_intent() {
        let launch = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "id": "agent-call-7",
                "name": "Agent",
                "input": {"prompt": "inspect parser.rs acceptance"}
            }]}
        });
        let ack = serde_json::json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result",
                "tool_use_id": "agent-call-7",
                "content": "launch metadata"
            }]},
            "toolUseResult": {"status": "async_launched"}
        });
        let notification = serde_json::json!({
            "type": "user",
            "message": {"content": concat!(
                "<task-notification><tool-use-id>agent-call-7</tool-use-id>",
                "<status>completed</status><result>found a conflicting state</result>",
                "</task-notification>"
            )}
        });
        let lines = [launch, ack, notification]
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let steps = parse_claude_steps(&lines);

        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].role, StepRole::ToolUse);
        assert_eq!(steps[0].tool_kind, ToolKind::Delegate);
        assert_eq!(steps[1].role, StepRole::DelegateResult);
        assert_eq!(steps[1].tool_kind, ToolKind::Delegate);
        assert_eq!(steps[1].correlation_id.as_deref(), Some("agent-call-7"));
        assert_eq!(
            steps[1].tool_target.as_deref(),
            Some("inspect parser.rs acceptance")
        );
        assert_eq!(steps[1].text, "found a conflicting state");
        assert!(!steps[1].is_error);
        assert!(steps.iter().all(|step| step.role != StepRole::User));
    }

    #[test]
    fn renamed_tools_are_typed_from_capability_shape() {
        let lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x1","name":"FutureWorkerV9","input":{"task_name":"audit","prompt":"inspect parser","subagent_type":"reviewer"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"x2","name":"UnknownFinder","input":{"pattern":"state_conflict","path":"src"}}]}}"#,
        ]
        .join("\n");
        let steps = parse_claude_steps(&lines);

        assert_eq!(steps[0].tool_kind, ToolKind::Delegate);
        assert_eq!(steps[1].tool_kind, ToolKind::Search);
    }

    fn human_texts(turns: &[UserTurn]) -> Vec<String> {
        turns
            .iter()
            .filter(|turn| turn.origin == UserOrigin::Human)
            .map(|turn| turn.text.clone())
            .collect()
    }

    #[test]
    fn ide_marker_sibling_is_admitted_as_human_with_context_flag() {
        let event = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "text", "text": "<ide_opened_file>The user opened /tmp/x.rs in the IDE.</ide_opened_file>"},
                {"type": "text", "text": "also fix the worker timeout"}
            ]}
        })
        .to_string();

        let turns = user_turns_reader(Cursor::new(&event));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].origin, UserOrigin::Human);
        assert_eq!(turns[0].text, "also fix the worker timeout");
        assert_eq!(turns[0].block_index, 1);
        assert!(turns[0]
            .context_flags
            .contains(ContextFlags::IDE_OPENED_FILE));

        let steps = parse_claude_steps(&event);
        let user_steps: Vec<&SemanticStep> =
            steps.iter().filter(|s| s.role == StepRole::User).collect();
        assert_eq!(user_steps.len(), 1);
        assert_eq!(user_steps[0].user_origin, UserOrigin::Human);
        assert!(user_steps[0]
            .context_flags
            .contains(ContextFlags::IDE_OPENED_FILE));
        assert_eq!(user_steps[0].source_turn_id, turns[0].source_turn_id);
    }

    #[test]
    fn context_only_event_yields_no_user_step() {
        let event = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "text", "text": "<ide_opened_file>The user opened /tmp/x.rs in the IDE.</ide_opened_file>"}
            ]}
        })
        .to_string();
        assert!(user_turns_reader(Cursor::new(&event)).is_empty());
        assert!(parse_claude_steps(&event)
            .iter()
            .all(|s| s.role != StepRole::User));
    }

    #[test]
    fn ordinary_user_xml_is_admitted_as_human() {
        let event = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "<service><port>8080</port></service> keep this endpoint stable"}
        })
        .to_string();
        let turns = user_turns_reader(Cursor::new(&event));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].origin, UserOrigin::Human);
    }

    #[test]
    fn controller_health_steer_is_typed_but_not_human() {
        let event = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "[VSC_RELAY_HEALTH_STEER v2 id=obl-0-1]\nRun one bounded check."}
        })
        .to_string();
        let turns = user_turns_reader(Cursor::new(&event));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].origin, UserOrigin::Controller);
        assert!(human_texts(&turns).is_empty());
    }

    #[test]
    fn user_turns_and_semantic_steps_agree_on_human_projection() {
        let lines = [
            r#"{"type":"user","message":{"content":"build and verify parser.rs"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"<ide_opened_file>opened /tmp/a</ide_opened_file>"},{"type":"text","text":"also add retries"}]}}"#,
        ]
        .join("\n");
        let human_turns = human_texts(&user_turns_reader(Cursor::new(&lines)));
        let human_steps: Vec<String> = parse_claude_steps(&lines)
            .iter()
            .filter(|s| s.role == StepRole::User && s.user_origin == UserOrigin::Human)
            .map(|s| s.text.clone())
            .collect();
        assert_eq!(human_turns, human_steps);
        assert_eq!(
            human_turns,
            vec![
                "build and verify parser.rs".to_string(),
                "also add retries".to_string()
            ]
        );
    }
}
