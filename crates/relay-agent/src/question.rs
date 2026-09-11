use crate::auth::Auth;
use crate::dedup::{self, PromptDedup};
use crate::inject;
use crate::permission::{self, Permissions};
use crate::supervisor::{discover, pool};
use crate::telegram::{esc_html, keyboard, Telegram};
use relay_ipc::Endpoint;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tracing::info;

pub struct Pending {
    pub request_id: String,
    pub tool_use_id: String,
    pub questions: Value,
    pub answers: HashMap<usize, Vec<String>>,
    pub cards: Vec<(i64, i64)>,
    pub alias: String,
    pub session_id: Option<String>,
    pub started: Instant,
}

fn is_multi(qv: &Value) -> bool {
    qv.get("multiSelect")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

pub type Questions = Arc<Mutex<HashMap<u32, Pending>>>;

pub fn new() -> Questions {
    Arc::new(Mutex::new(HashMap::new()))
}

pub async fn start(
    q: Questions,
    perms: Permissions,
    tg: Arc<Telegram>,
    auth: Arc<Auth>,
    dedup: PromptDedup,
) {
    let watched: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    loop {
        for pid in relay_ipc::live_out_pids() {
            let mut w = watched.lock().await;
            if !w.contains(&pid) {
                w.insert(pid);
                drop(w);
                tokio::spawn(reader(
                    pid,
                    q.clone(),
                    perms.clone(),
                    tg.clone(),
                    auth.clone(),
                    dedup.clone(),
                    watched.clone(),
                ));
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn reader(
    pid: u32,
    q: Questions,
    perms: Permissions,
    tg: Arc<Telegram>,
    auth: Arc<Auth>,
    dedup: PromptDedup,
    watched: Arc<Mutex<HashSet<u32>>>,
) {
    if let Ok(conn) = relay_ipc::connect_async(&Endpoint::Out(pid)).await {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let proc = {
            let (q, perms, tg, auth, dedup) = (
                q.clone(),
                perms.clone(),
                tg.clone(),
                auth.clone(),
                dedup.clone(),
            );
            tokio::spawn(async move {
                while let Some(v) = rx.recv().await {
                    handle_stream_line(pid, &v, &q, &perms, &tg, &auth, &dedup).await;
                }
            })
        };
        let mut lines = BufReader::new(conn).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
        drop(tx);
        let _ = proc.await;
    }
    watched.lock().await.remove(&pid);
    if inject::pid_alive(pid) {
        return;
    }
    if let Some(p) = q.lock().await.remove(&pid) {
        for (c, m) in dedup.forget(&p.tool_use_id).await {
            let _ = tg
                .edit_message_text(c, m, "session closed - question is no longer active", None)
                .await;
        }
        for (c, m) in p.cards {
            let _ = tg
                .edit_message_text(c, m, "session closed - question is no longer active", None)
                .await;
        }
    }
    for cards in permission::drain_pid(&perms, pid).await {
        for (c, m) in cards {
            let _ = tg
                .edit_message_text(
                    c,
                    m,
                    "session closed - permission is no longer active",
                    None,
                )
                .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_stream_line(
    pid: u32,
    v: &Value,
    q: &Questions,
    perms: &Permissions,
    tg: &Arc<Telegram>,
    auth: &Arc<Auth>,
    dedup: &PromptDedup,
) {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if ty == "control_request" {
        let req = v.get("request");
        let subtype = req
            .and_then(|r| r.get("subtype"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let tool = req
            .and_then(|r| r.get("tool_name"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        if subtype == "can_use_tool" {
            let request_id = v
                .get("request_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let alias = session_alias(pid).unwrap_or_else(|| format!("pid {pid}"));
            info!(target: "relay::perm", pid, tool = %tool, request_id = %request_id, "can_use_tool observed");
            if tool == "AskUserQuestion" {
                let tool_use_id = req
                    .and_then(|r| r.get("tool_use_id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                for (c, m) in dedup.mark_received(&tool_use_id).await {
                    let _ = tg.edit_message_text(c, m, dedup::COLLAPSE_NOTE, None).await;
                }
                let already_live = q
                    .lock()
                    .await
                    .get(&pid)
                    .map(|p| p.request_id == request_id)
                    .unwrap_or(false);
                if already_live {
                    return;
                }
                let questions = req
                    .and_then(|r| r.get("input"))
                    .and_then(|i| i.get("questions"))
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                let session_id = inject::session_id_of(pid);
                let cfg = crate::automation::AutomationConfig::load();
                if auto_answer_eligible(&cfg, session_id.as_deref(), &alias, &questions) {
                    let sid = session_id.clone();
                    let alias_c = alias.clone();
                    let questions_c = questions.clone();
                    let request_id_c = request_id.clone();
                    let tool_use_id_c = tool_use_id.clone();
                    let min_confidence = cfg.auto.auto_answer_min_confidence;
                    let providers = cfg.robot.providers.clone();
                    let q_c = q.clone();
                    let tg_c = tg.clone();
                    let auth_c = auth.clone();
                    let dedup_c = dedup.clone();
                    tokio::spawn(async move {
                        let handled = match sid.as_deref() {
                            Some(s) => {
                                commit_auto_answer(
                                    pid,
                                    &alias_c,
                                    s,
                                    &request_id_c,
                                    &tool_use_id_c,
                                    &questions_c,
                                    min_confidence,
                                    &providers,
                                    &tg_c,
                                    &auth_c,
                                )
                                .await
                            }
                            None => false,
                        };
                        if !handled {
                            send_question_card(
                                pid,
                                alias_c,
                                sid,
                                request_id_c,
                                tool_use_id_c,
                                questions_c,
                                &q_c,
                                &tg_c,
                                &auth_c,
                                &dedup_c,
                            )
                            .await;
                        }
                    });
                    return;
                }
                send_question_card(
                    pid,
                    alias,
                    session_id,
                    request_id,
                    tool_use_id,
                    questions,
                    q,
                    tg,
                    auth,
                    dedup,
                )
                .await;
            } else {
                let tool_use_id = req
                    .and_then(|r| r.get("tool_use_id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = req
                    .and_then(|r| r.get("input"))
                    .cloned()
                    .unwrap_or(Value::Null);
                permission::on_request(
                    perms,
                    tg,
                    auth,
                    dedup,
                    pid,
                    alias,
                    request_id,
                    tool_use_id,
                    tool.to_string(),
                    input,
                )
                .await;
            }
        }
        return;
    }
    let refs_tuid = {
        let map = q.lock().await;
        map.get(&pid)
            .map(|p| !p.tool_use_id.is_empty() && dedup::refs_tool_use_id(v, &p.tool_use_id))
            .unwrap_or(false)
    };
    if refs_tuid && ty != "control_request" {
        if let Some(p) = q.lock().await.remove(&pid) {
            info!(
                target: "relay::trace",
                pipeline = "out", stage = "answered_vscode", kind = "question",
                corr = %p.tool_use_id, pid, alias = %p.alias,
                latency_ms = p.started.elapsed().as_millis() as u64,
                "question answered in VS Code"
            );
            for (c, m) in dedup.forget(&p.tool_use_id).await {
                let _ = tg
                    .edit_message_text(c, m, "answered in VS Code", None)
                    .await;
            }
            for (c, m) in p.cards {
                let _ = tg
                    .edit_message_text(c, m, "answered in VS Code", None)
                    .await;
            }
        }
    }
    for cards in permission::void_referenced(perms, pid, v).await {
        for (c, m) in cards {
            let _ = tg
                .edit_message_text(c, m, "answered in VS Code", None)
                .await;
        }
    }
}

fn session_alias(pid: u32) -> Option<String> {
    let f = dirs::home_dir()?
        .join(".claude")
        .join("sessions")
        .join(format!("{pid}.json"));
    let txt = std::fs::read_to_string(f).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    let cwd = v.get("cwd").and_then(|x| x.as_str())?;
    std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
}

pub fn render(pid: u32, p: &Pending) -> (String, Value) {
    let empty = vec![];
    let qs = p.questions.as_array().unwrap_or(&empty);
    let multi = qs.len() > 1;
    let mut text = format!("🟡 <b>Question</b> - <b>{}</b>", esc_html(&p.alias));
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    let mut answered = 0usize;
    for (qi, qv) in qs.iter().enumerate() {
        let qtext = qv.get("question").and_then(|x| x.as_str()).unwrap_or("");
        let header = qv.get("header").and_then(|x| x.as_str()).unwrap_or("");
        let ms = is_multi(qv);
        let empty_sel: Vec<String> = Vec::new();
        let chosen = p.answers.get(&qi).unwrap_or(&empty_sel);
        if !chosen.is_empty() {
            answered += 1;
        }
        text.push_str(&format!(
            "\n\n<b>{}{}</b>{}\n{}",
            if multi {
                format!("Q{} · ", qi + 1)
            } else {
                String::new()
            },
            esc_html(header),
            if ms { " <i>(multiple)</i>" } else { "" },
            esc_html(qtext)
        ));
        if !chosen.is_empty() {
            text.push_str(&format!("\n➡️ <b>{}</b>", esc_html(&chosen.join(", "))));
        }
        let opts = qv.get("options").and_then(|o| o.as_array());
        let mut row: Vec<(String, String)> = Vec::new();
        if let Some(opts) = opts {
            for (oi, ov) in opts.iter().enumerate() {
                let label = ov.get("label").and_then(|x| x.as_str()).unwrap_or("?");
                let mark = if chosen.iter().any(|c| c == label) {
                    "✓ "
                } else {
                    ""
                };
                let disp = format!("{mark}{}", relay_core::state::truncate(label, 18));
                row.push((disp, format!("aq:p:{pid}:{qi}:{oi}")));
                if row.len() == 2 {
                    rows.push(std::mem::take(&mut row));
                }
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    let total = qs.len();
    rows.push(vec![(
        format!("✅ Submit ({answered}/{total})"),
        format!("aq:s:{pid}"),
    )]);
    (text, keyboard(rows))
}

pub async fn handle_pick(q: &Questions, pid: u32, qi: usize, oi: usize) -> Option<(String, Value)> {
    let mut map = q.lock().await;
    let (ms, label) = {
        let p = map.get(&pid)?;
        let qs = p.questions.as_array()?;
        let qv = qs.get(qi)?;
        let ms = is_multi(qv);
        let label = qv
            .get("options")
            .and_then(|o| o.as_array())
            .and_then(|a| a.get(oi))
            .and_then(|ov| ov.get("label"))
            .and_then(|x| x.as_str())?
            .to_string();
        (ms, label)
    };
    {
        let p = map.get_mut(&pid)?;
        let entry = p.answers.entry(qi).or_default();
        if ms {
            if let Some(pos) = entry.iter().position(|l| l == &label) {
                entry.remove(pos);
            } else {
                entry.push(label);
            }
        } else {
            *entry = vec![label];
        }
    }
    let p = map.get(&pid)?;
    Some(render(pid, p))
}

pub async fn handle_submit(q: &Questions, pid: u32) -> Result<Vec<(i64, i64)>, String> {
    let mut map = q.lock().await;
    let p = map
        .get(&pid)
        .ok_or_else(|| "question is no longer active".to_string())?;
    if !inject::session_stable(pid, &p.session_id) {
        map.remove(&pid);
        return Err("session changed - answer not sent".to_string());
    }
    let empty = vec![];
    let qs = p.questions.as_array().unwrap_or(&empty);
    let total = qs.len();
    let answered = (0..total)
        .filter(|i| p.answers.get(i).map(|v| !v.is_empty()).unwrap_or(false))
        .count();
    if answered < total {
        return Err(format!("answer every tab ({answered}/{total})"));
    }
    let mut answers_map = serde_json::Map::new();
    for (qi, qv) in qs.iter().enumerate() {
        let qtext = qv
            .get("question")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let sel = p.answers.get(&qi).cloned().unwrap_or_default();
        let val = if is_multi(qv) {
            Value::Array(sel.into_iter().map(Value::String).collect())
        } else {
            Value::String(sel.into_iter().next().unwrap_or_default())
        };
        answers_map.insert(qtext, val);
    }
    let response = build_answer_response(&p.request_id, &p.tool_use_id, &p.questions, answers_map);
    let cards = p.cards.clone();
    let corr = p.tool_use_id.clone();
    let latency_ms = p.started.elapsed().as_millis() as u64;
    let pid_c = pid;
    let res = tokio::task::spawn_blocking(move || inject::send_raw(pid_c, &response))
        .await
        .map_err(|e| e.to_string())?;
    res.map_err(|e| e.to_string())?;
    info!(
        target: "relay::trace",
        pipeline = "out", stage = "resolved", kind = "question", direction = "to_vscode",
        corr = %corr, pid, latency_ms, tabs = total,
        "question answer injected to VS Code"
    );
    map.remove(&pid);
    Ok(cards)
}

fn build_answer_response(
    request_id: &str,
    tool_use_id: &str,
    questions: &Value,
    answers_map: serde_json::Map<String, Value>,
) -> Value {
    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": { "questions": questions.clone(), "answers": answers_map },
                "updatedPermissions": [],
                "toolUseID": tool_use_id,
            }
        }
    })
}

struct AutoAnswer {
    answer: String,
    confidence: f64,
    reason: String,
    backend: String,
}

fn pick_answer(
    option_index: Option<usize>,
    message: Option<&str>,
    confidence: f64,
    min_confidence: f64,
    options: &[(String, String)],
) -> Option<String> {
    if confidence < min_confidence {
        return None;
    }
    if let Some(index) = option_index {
        return options.get(index).map(|(label, _)| label.clone());
    }
    let text = message.unwrap_or("").trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn answer_prompt(header: &str, qtext: &str, options: &[(String, String)], context: &str) -> String {
    let mut out = String::new();
    if !context.trim().is_empty() {
        out.push_str("Recent session context (oldest first):\n");
        out.push_str(context.trim());
        out.push_str("\n\n");
    }
    out.push_str("The coding agent asked its user this single-select question:\n");
    if !header.is_empty() {
        out.push_str(&format!("Topic: {header}\n"));
    }
    out.push_str(&format!("Question: {qtext}\nOptions:\n"));
    for (i, (label, desc)) in options.iter().enumerate() {
        if desc.trim().is_empty() {
            out.push_str(&format!("  {i}: {label}\n"));
        } else {
            out.push_str(&format!("  {i}: {label} - {desc}\n"));
        }
    }
    out.push_str(
        "\nChoose on the user's behalf. Set option_index to the best option's 0-based index, \
         or put a short free-text answer in message if no option fits. Set confidence (0-1) and reason.",
    );
    out
}

const ANSWER_SYSTEM: &str =
    "You answer a multiple-choice question that a coding agent asked its user, deciding on the \
     user's behalf from the session context. Reply ONLY with the decision JSON. Set action to \
     \"feedback\". To pick an option set option_index to its 0-based index; if no option fits and a \
     short free-text reply is better, set message and omit option_index. Always set confidence \
     (0-1) reflecting certainty and a brief reason.";

async fn resolve_auto_answer(
    session_id: &str,
    header: &str,
    qtext: &str,
    options: &[(String, String)],
    min_confidence: f64,
    providers: &crate::automation::Providers,
) -> Option<AutoAnswer> {
    let mut context = String::new();
    if let Some(jsonl) = crate::compass::find_claude_jsonl(session_id) {
        for (role, text) in relay_adapters::claude::tail_messages(&jsonl, 8) {
            let who = if role == 'A' || role == 'T' {
                "agent"
            } else {
                "user"
            };
            let line = relay_core::state::truncate(text.trim(), 400);
            context.push_str(&format!("{who}: {line}\n"));
        }
    }
    let user = answer_prompt(header, qtext, options, &context);
    let discovered = discover::discover_all().await;
    let now = crate::automation::now_secs();
    let (backend, decision) = pool::Pool::new()
        .ask(
            session_id,
            providers,
            &discovered,
            ANSWER_SYSTEM,
            &user,
            now,
        )
        .await
        .ok()?;
    let confidence = decision.confidence.unwrap_or(0.0);
    let answer = pick_answer(
        decision.option_index,
        decision.message.as_deref(),
        confidence,
        min_confidence,
        options,
    )?;
    Some(AutoAnswer {
        answer,
        confidence,
        reason: decision.reason,
        backend: backend.id().to_string(),
    })
}

fn auto_answer_eligible(
    cfg: &crate::automation::AutomationConfig,
    session_id: Option<&str>,
    alias: &str,
    questions: &Value,
) -> bool {
    if !cfg.auto.auto_answer_questions {
        return false;
    }
    let Some(sid) = session_id else {
        return false;
    };
    if cfg.resolve(Some(sid), alias, crate::automation::now_secs()) != crate::automation::Mode::Auto
    {
        return false;
    }
    let empty = vec![];
    let qs = questions.as_array().unwrap_or(&empty);
    if qs.len() != 1 {
        return false;
    }
    let q0 = &qs[0];
    if is_multi(q0) {
        return false;
    }
    q0.get("options")
        .and_then(|o| o.as_array())
        .map(|opts| !opts.is_empty())
        .unwrap_or(false)
}

const AUTO_ANSWER_BUDGET: Duration = Duration::from_secs(30);

#[allow(clippy::too_many_arguments)]
async fn commit_auto_answer(
    pid: u32,
    alias: &str,
    session_id: &str,
    request_id: &str,
    tool_use_id: &str,
    questions: &Value,
    min_confidence: f64,
    providers: &crate::automation::Providers,
    tg: &Arc<Telegram>,
    auth: &Arc<Auth>,
) -> bool {
    let empty = vec![];
    let qs = questions.as_array().unwrap_or(&empty);
    let Some(q0) = qs.first() else {
        crate::decision_log::record(
            "out",
            "auto_answer_declined",
            serde_json::json!({"alias": alias, "pid": pid, "kind": "question",
                               "reason": "no question in the payload"}),
        );
        return false;
    };
    let options: Vec<(String, String)> = match q0.get("options").and_then(|o| o.as_array()) {
        Some(opts) if !opts.is_empty() => opts
            .iter()
            .map(|ov| {
                (
                    ov.get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    ov.get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .collect(),
        _ => {
            crate::decision_log::record(
                "out",
                "auto_answer_declined",
                serde_json::json!({"alias": alias, "pid": pid, "kind": "question",
                               "reason": "question carried no options to choose from"}),
            );
            return false;
        }
    };
    let qtext = q0.get("question").and_then(Value::as_str).unwrap_or("");
    let header = q0.get("header").and_then(Value::as_str).unwrap_or("");
    let resolved = match tokio::time::timeout(
        AUTO_ANSWER_BUDGET,
        resolve_auto_answer(
            session_id,
            header,
            qtext,
            &options,
            min_confidence,
            providers,
        ),
    )
    .await
    {
        Ok(Some(resolved)) => resolved,
        other => {
            crate::decision_log::record(
                "out",
                "auto_answer_declined",
                serde_json::json!({"alias": alias, "pid": pid, "kind": "question",
                "min_confidence": min_confidence,
                "reason": if other.is_err() {
                    "provider budget expired"
                } else {
                    "no provider answer above the confidence floor"
                }}),
            );
            return false;
        }
    };
    let owner = Some(session_id.to_string());
    if !inject::session_stable(pid, &owner) {
        crate::decision_log::record(
            "out",
            "auto_answer_declined",
            serde_json::json!({"alias": alias, "pid": pid, "kind": "question",
                               "reason": "session not stable at delivery time"}),
        );
        return false;
    }
    let mut answers_map = serde_json::Map::new();
    answers_map.insert(qtext.to_string(), Value::String(resolved.answer.clone()));
    let response = build_answer_response(request_id, tool_use_id, questions, answers_map);
    let injected = tokio::task::spawn_blocking(move || inject::send_raw(pid, &response)).await;
    if !matches!(injected, Ok(Ok(()))) {
        crate::decision_log::record(
            "out",
            "auto_answer_declined",
            serde_json::json!({"alias": alias, "pid": pid, "kind": "question",
                               "reason": "answer could not be injected into the session"}),
        );
        return false;
    }
    info!(
        target: "relay::trace",
        pipeline = "out", stage = "auto_answered", kind = "question", direction = "to_vscode",
        corr = %tool_use_id, pid, alias = %alias,
        backend = %resolved.backend, confidence = resolved.confidence,
        "question auto-answered by provider"
    );
    crate::decision_log::record(
        "out",
        "auto_answered",
        serde_json::json!({"alias": alias, "pid": pid, "kind": "question", "mode": "auto",
                           "backend": resolved.backend, "confidence": resolved.confidence,
                           "reason": resolved.reason}),
    );
    let note = format!(
        "🤖 <b>Auto-answered</b> · <b>{}</b>\n{}\n➡️ <b>{}</b>\n<i>{} · {} · conf {:.2}</i>",
        esc_html(alias),
        esc_html(qtext),
        esc_html(&resolved.answer),
        esc_html(&resolved.backend),
        esc_html(&resolved.reason),
        resolved.confidence
    );
    for chat in auth.recipients().await {
        let _ = tg.send(chat, &note, None).await;
    }
    true
}

#[allow(clippy::too_many_arguments)]
async fn send_question_card(
    pid: u32,
    alias: String,
    session_id: Option<String>,
    request_id: String,
    tool_use_id: String,
    questions: Value,
    q: &Questions,
    tg: &Arc<Telegram>,
    auth: &Arc<Auth>,
    dedup: &PromptDedup,
) {
    dedup.mark_live_card(&alias).await;
    let mut pending = Pending {
        request_id,
        tool_use_id,
        questions,
        answers: HashMap::new(),
        cards: Vec::new(),
        alias,
        session_id,
        started: Instant::now(),
    };
    let (text, kb) = render(pid, &pending);
    for chat in auth.recipients().await {
        if let Ok(mid) = tg.send(chat, &text, Some(kb.clone())).await {
            pending.cards.push((chat, mid));
        }
    }
    info!(
        target: "relay::trace",
        pipeline = "out", stage = "sent", kind = "question",
        corr = %pending.tool_use_id, pid, alias = %pending.alias,
        chats = pending.cards.len(),
        "question card sent"
    );
    q.lock().await.insert(pid, pending);
}

#[cfg(test)]
mod auto_answer_tests {
    use super::*;

    fn opts() -> Vec<(String, String)> {
        vec![
            ("Keep".to_string(), "leave as is".to_string()),
            ("Rewrite".to_string(), "start over".to_string()),
        ]
    }

    #[test]
    fn option_index_maps_to_label() {
        let o = opts();
        assert_eq!(
            pick_answer(Some(1), None, 0.9, 0.7, &o).as_deref(),
            Some("Rewrite")
        );
    }

    #[test]
    fn out_of_range_option_index_is_rejected() {
        let o = opts();
        assert_eq!(pick_answer(Some(9), None, 0.9, 0.7, &o), None);
    }

    #[test]
    fn free_text_used_when_no_option_index() {
        let o = opts();
        assert_eq!(
            pick_answer(None, Some("  do X instead  "), 0.9, 0.7, &o).as_deref(),
            Some("do X instead")
        );
    }

    #[test]
    fn empty_free_text_is_rejected() {
        let o = opts();
        assert_eq!(pick_answer(None, Some("   "), 0.9, 0.7, &o), None);
        assert_eq!(pick_answer(None, None, 0.9, 0.7, &o), None);
    }

    #[test]
    fn low_confidence_falls_back_even_with_a_valid_option() {
        let o = opts();
        assert_eq!(pick_answer(Some(0), None, 0.5, 0.7, &o), None);
    }

    #[test]
    fn auto_answer_is_off_by_default() {
        let cfg = crate::automation::AutoRules::default();
        assert!(!cfg.auto_answer_questions);
        assert!((cfg.auto_answer_min_confidence - 0.7).abs() < f64::EPSILON);
    }
}
