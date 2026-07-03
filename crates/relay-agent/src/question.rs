use crate::auth::Auth;
use crate::inject;
use crate::permission::{self, Permissions};
use crate::telegram::{esc_html, keyboard, Telegram};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

pub struct Pending {
    pub request_id: String,
    pub tool_use_id: String,
    pub questions: Value,
    pub answers: HashMap<usize, Vec<String>>,
    pub cards: Vec<(i64, i64)>,
    pub alias: String,
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

fn out_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("out")
}

fn out_sock(pid: u32) -> PathBuf {
    out_dir().join(format!("{pid}.sock"))
}

pub async fn start(q: Questions, perms: Permissions, tg: Arc<Telegram>, auth: Arc<Auth>) {
    let watched: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    loop {
        if let Ok(rd) = std::fs::read_dir(out_dir()) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if let Some(pid) = name
                    .strip_suffix(".sock")
                    .and_then(|s| s.parse::<u32>().ok())
                {
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
                            watched.clone(),
                        ));
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn reader(
    pid: u32,
    q: Questions,
    perms: Permissions,
    tg: Arc<Telegram>,
    auth: Arc<Auth>,
    watched: Arc<Mutex<HashSet<u32>>>,
) {
    if let Ok(stream) = UnixStream::connect(out_sock(pid)).await {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            handle_stream_line(pid, &v, &q, &perms, &tg, &auth).await;
        }
    }
    watched.lock().await.remove(&pid);
    let mut map = q.lock().await;
    if let Some(p) = map.remove(&pid) {
        for (c, m) in p.cards {
            let _ = tg
                .edit_message_text(c, m, "session closed - question is no longer active", None)
                .await;
        }
    }
}

async fn handle_stream_line(
    pid: u32,
    v: &Value,
    q: &Questions,
    perms: &Permissions,
    tg: &Arc<Telegram>,
    auth: &Arc<Auth>,
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
            if tool == "AskUserQuestion" {
                let tool_use_id = req
                    .and_then(|r| r.get("tool_use_id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let questions = req
                    .and_then(|r| r.get("input"))
                    .and_then(|i| i.get("questions"))
                    .cloned()
                    .unwrap_or(Value::Array(vec![]));
                let mut pending = Pending {
                    request_id,
                    tool_use_id,
                    questions,
                    answers: HashMap::new(),
                    cards: Vec::new(),
                    alias,
                };
                let (text, kb) = render(pid, &pending);
                for chat in auth.recipients().await {
                    if let Ok(mid) = tg.send(chat, &text, Some(kb.clone())).await {
                        pending.cards.push((chat, mid));
                    }
                }
                q.lock().await.insert(pid, pending);
            } else {
                let input = req
                    .and_then(|r| r.get("input"))
                    .cloned()
                    .unwrap_or(Value::Null);
                permission::on_request(
                    perms,
                    tg,
                    auth,
                    pid,
                    alias,
                    request_id,
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
            .map(|p| !p.tool_use_id.is_empty() && line_refs(v, &p.tool_use_id))
            .unwrap_or(false)
    };
    if refs_tuid && ty != "control_request" {
        if let Some(p) = q.lock().await.remove(&pid) {
            for (c, m) in p.cards {
                let _ = tg
                    .edit_message_text(c, m, "answered in VS Code", None)
                    .await;
            }
        }
    }
}

fn line_refs(v: &Value, tuid: &str) -> bool {
    serde_json::to_string(v)
        .map(|s| s.contains(tuid))
        .unwrap_or(false)
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
    let response = serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": p.request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": { "questions": p.questions.clone(), "answers": answers_map },
                "updatedPermissions": [],
                "toolUseID": p.tool_use_id,
            }
        }
    });
    let cards = p.cards.clone();
    let pid_c = pid;
    let res = tokio::task::spawn_blocking(move || inject::send_raw(pid_c, &response))
        .await
        .map_err(|e| e.to_string())?;
    res.map_err(|e| e.to_string())?;
    map.remove(&pid);
    Ok(cards)
}
