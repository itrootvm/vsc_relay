use crate::auth::Auth;
use crate::telegram::{esc_html, keyboard, Telegram};
use crate::{actions, inject};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

pub struct PermPending {
    pub request_id: String,
    pub pid: u32,
    pub tool_name: String,
    pub tool_use_id: String,
    pub input: Value,
    pub cards: Vec<(i64, i64)>,
    pub alias: String,
    pub session_id: Option<String>,
}

pub type Permissions = Arc<Mutex<HashMap<String, PermPending>>>;

pub fn new() -> Permissions {
    Arc::new(Mutex::new(HashMap::new()))
}

#[allow(clippy::too_many_arguments)]
pub async fn on_request(
    perms: &Permissions,
    tg: &Arc<Telegram>,
    auth: &Arc<Auth>,
    pid: u32,
    alias: String,
    request_id: String,
    tool_use_id: String,
    tool_name: String,
    input: Value,
) {
    if request_id.is_empty() {
        warn!(target: "relay::perm", pid, tool = %tool_name, "can_use_tool without request_id; cannot answer remotely");
        return;
    }
    if perms.lock().await.contains_key(&request_id) {
        return;
    }
    let mut p = PermPending {
        request_id: request_id.clone(),
        pid,
        tool_name,
        tool_use_id,
        input,
        cards: Vec::new(),
        alias,
        session_id: inject::session_id_of(pid),
    };
    let (text, kb) = render(&p);
    for chat in auth.recipients().await {
        if let Ok(mid) = tg.send(chat, &text, Some(kb.clone())).await {
            p.cards.push((chat, mid));
        }
    }
    if p.cards.is_empty() {
        warn!(target: "relay::perm", pid, tool = %p.tool_name, request_id = %request_id,
            "permission needed but forwarded to 0 chats");
    } else {
        info!(target: "relay::perm", pid, tool = %p.tool_name, request_id = %request_id,
            chats = p.cards.len(), "permission card sent");
    }
    perms.lock().await.insert(request_id, p);
}

fn summarize(tool: &str, input: &Value) -> String {
    let target = input
        .get("command")
        .and_then(|x| x.as_str())
        .or_else(|| input.get("file_path").and_then(|x| x.as_str()))
        .or_else(|| input.get("path").and_then(|x| x.as_str()))
        .or_else(|| input.get("url").and_then(|x| x.as_str()))
        .or_else(|| input.get("description").and_then(|x| x.as_str()));
    match target {
        Some(t) => format!("{tool}: {}", relay_core::state::truncate(t, 300)),
        None => tool.to_string(),
    }
}

pub fn render(p: &PermPending) -> (String, Value) {
    let text = format!(
        "🔐 <b>Permission</b> - <b>{}</b>\n{}",
        esc_html(&p.alias),
        esc_html(&summarize(&p.tool_name, &p.input))
    );
    let kb = keyboard(vec![vec![
        (
            "✅ Allow".to_string(),
            format!("pm:a:{}", actions::encode(p.request_id.clone())),
        ),
        (
            "⛔ Deny".to_string(),
            format!("pm:d:{}", actions::encode(p.request_id.clone())),
        ),
    ]]);
    (text, kb)
}

pub async fn resolve(
    perms: &Permissions,
    request_id: &str,
    allow: bool,
) -> Result<Vec<(i64, i64)>, String> {
    let p = perms
        .lock()
        .await
        .remove(request_id)
        .ok_or_else(|| "permission is no longer active".to_string())?;
    if !inject::session_stable(p.pid, &p.session_id) {
        return Err("session closed - permission is no longer active".to_string());
    }
    let inner = if allow {
        serde_json::json!({ "behavior": "allow", "updatedInput": p.input })
    } else {
        serde_json::json!({ "behavior": "deny", "message": "denied via Telegram" })
    };
    let response = serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": p.request_id,
            "response": inner,
        }
    });
    let pid = p.pid;
    info!(target: "relay::perm", pid, request_id = %p.request_id, allow, "resolving permission from Telegram");
    tokio::task::spawn_blocking(move || inject::send_raw(pid, &response))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(p.cards)
}

pub async fn drain_pid(perms: &Permissions, pid: u32) -> Vec<Vec<(i64, i64)>> {
    let mut map = perms.lock().await;
    let ids: Vec<String> = map
        .iter()
        .filter(|(_, p)| p.pid == pid)
        .map(|(k, _)| k.clone())
        .collect();
    ids.into_iter()
        .filter_map(|id| map.remove(&id))
        .map(|p| p.cards)
        .collect()
}

pub async fn void_referenced(perms: &Permissions, pid: u32, line: &Value) -> Vec<Vec<(i64, i64)>> {
    let mut map = perms.lock().await;
    let ids: Vec<String> = map
        .iter()
        .filter(|(_, p)| {
            p.pid == pid
                && !p.tool_use_id.is_empty()
                && crate::dedup::refs_tool_use_id(line, &p.tool_use_id)
        })
        .map(|(k, _)| k.clone())
        .collect();
    ids.into_iter()
        .filter_map(|id| map.remove(&id))
        .map(|p| p.cards)
        .collect()
}
