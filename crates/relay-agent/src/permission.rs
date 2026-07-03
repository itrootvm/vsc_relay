use crate::auth::Auth;
use crate::inject;
use crate::telegram::{esc_html, keyboard, Telegram};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct PermPending {
    pub request_id: String,
    pub tool_name: String,
    pub input: Value,
    pub cards: Vec<(i64, i64)>,
    pub alias: String,
}

pub type Permissions = Arc<Mutex<HashMap<u32, PermPending>>>;

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
    tool_name: String,
    input: Value,
) {
    let mut p = PermPending {
        request_id,
        tool_name,
        input,
        cards: Vec::new(),
        alias,
    };
    let (text, kb) = render(pid, &p);
    for chat in auth.recipients().await {
        if let Ok(mid) = tg.send(chat, &text, Some(kb.clone())).await {
            p.cards.push((chat, mid));
        }
    }
    perms.lock().await.insert(pid, p);
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

pub fn render(pid: u32, p: &PermPending) -> (String, Value) {
    let text = format!(
        "🔐 <b>Permission</b> - <b>{}</b>\n{}",
        esc_html(&p.alias),
        esc_html(&summarize(&p.tool_name, &p.input))
    );
    let kb = keyboard(vec![vec![
        ("✅ Allow".to_string(), format!("pm:a:{pid}")),
        ("⛔ Deny".to_string(), format!("pm:d:{pid}")),
    ]]);
    (text, kb)
}

pub async fn resolve(
    perms: &Permissions,
    pid: u32,
    allow: bool,
) -> Result<Vec<(i64, i64)>, String> {
    let p = perms
        .lock()
        .await
        .remove(&pid)
        .ok_or_else(|| "permission is no longer active".to_string())?;
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
    tokio::task::spawn_blocking(move || inject::send_raw(pid, &response))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(p.cards)
}
