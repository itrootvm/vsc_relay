use crate::auth::Auth;
use crate::telegram::{esc_html, keyboard, Telegram};
use crate::{actions, inject};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
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
    pub started: Instant,
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
    dedup: &crate::dedup::PromptDedup,
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
    if crate::hooks::gate_holds_tool_use(&tool_use_id) {
        info!(target: "relay::perm", pid, tool = %tool_name, tool_use_id = %tool_use_id,
            "permission path suppressed because deterministic PreToolUse gate owns the native action");
        return;
    }
    if perms.lock().await.contains_key(&request_id) {
        return;
    }
    let session_id = inject::session_id_of(pid);
    if should_auto_approve(&alias, &session_id, &tool_name, &input)
        && try_auto_approve(
            tg,
            auth,
            pid,
            &alias,
            &session_id,
            &request_id,
            &tool_name,
            &input,
        )
        .await
    {
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
        session_id,
        started: Instant::now(),
    };
    dedup.mark_live_card(&p.alias).await;
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
        info!(
            target: "relay::trace",
            pipeline = "out", stage = "sent", kind = "permission",
            corr = %p.tool_use_id, pid, alias = %p.alias, tool = %p.tool_name,
            chats = p.cards.len(),
            "permission card sent"
        );
    }
    perms.lock().await.insert(request_id, p);
}

fn danger_target(input: &Value) -> Option<String> {
    input
        .get("command")
        .and_then(|x| x.as_str())
        .or_else(|| input.get("file_path").and_then(|x| x.as_str()))
        .or_else(|| input.get("path").and_then(|x| x.as_str()))
        .or_else(|| input.get("url").and_then(|x| x.as_str()))
        .or_else(|| input.get("description").and_then(|x| x.as_str()))
        .map(str::to_string)
}

fn auto_decision(
    cfg: &crate::automation::AutomationConfig,
    mode: crate::automation::Mode,
    dangerous: bool,
    tool_name: &str,
) -> bool {
    if mode != crate::automation::Mode::Auto {
        return false;
    }
    if dangerous {
        return false;
    }
    if tool_name == "ExitPlanMode" {
        return cfg.auto.auto_accept_plan_exit;
    }
    cfg.auto.auto_approve
}

fn should_auto_approve(
    alias: &str,
    session_id: &Option<String>,
    tool_name: &str,
    input: &Value,
) -> bool {
    let cfg = crate::automation::AutomationConfig::load();
    let mode = cfg.resolve(session_id.as_deref(), alias, crate::automation::now_secs());
    let dangerous = crate::hooks::is_destructive(&danger_target(input));
    auto_decision(&cfg, mode, dangerous, tool_name)
}

#[allow(clippy::too_many_arguments)]
async fn try_auto_approve(
    tg: &Arc<Telegram>,
    auth: &Arc<Auth>,
    pid: u32,
    alias: &str,
    session_id: &Option<String>,
    request_id: &str,
    tool_name: &str,
    input: &Value,
) -> bool {
    if !inject::session_stable(pid, session_id) {
        crate::decision_log::record(
            "out",
            "auto_approve_declined",
            serde_json::json!({"tool": tool_name, "alias": alias, "pid": pid,
                               "reason": "session not stable; fell back to a human card"}),
        );
        return false;
    }
    let response = serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": { "behavior": "allow", "updatedInput": input.clone() },
        }
    });
    let sent = tokio::task::spawn_blocking(move || inject::send_raw(pid, &response)).await;
    match sent {
        Ok(Ok(())) => {}
        _ => {
            warn!(target: "relay::perm", pid, tool = %tool_name,
                "auto-approve inject failed; falling back to human card");
            crate::decision_log::record(
                "out",
                "auto_approve_declined",
                serde_json::json!({"tool": tool_name, "alias": alias, "pid": pid,
                                   "reason": "inject failed; fell back to a human card"}),
            );
            return false;
        }
    }
    info!(
        target: "relay::trace",
        pipeline = "out", stage = "auto_approved", kind = "permission", direction = "to_vscode",
        pid, alias = %alias, tool = %tool_name,
        "permission auto-approved under Auto mode"
    );
    crate::decision_log::record(
        "out",
        "auto_approved",
        serde_json::json!({"tool": tool_name, "alias": alias, "pid": pid, "mode": "auto",
                           "reason": "Auto mode approved without asking"}),
    );
    let note = format!(
        "🤖 <b>Auto-approved</b> - <b>{}</b>\n{}",
        esc_html(alias),
        esc_html(&summarize(tool_name, input))
    );
    for chat in auth.recipients().await {
        let _ = tg.send(chat, &note, None).await;
    }
    true
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
    info!(
        target: "relay::trace",
        pipeline = "out", stage = "resolved", kind = "permission", direction = "to_vscode",
        corr = %p.tool_use_id, pid, allow, tool = %p.tool_name,
        latency_ms = p.started.elapsed().as_millis() as u64,
        "permission decision injected to VS Code"
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::{AutomationConfig, Mode};

    #[test]
    fn manual_never_auto_approves() {
        let cfg = AutomationConfig::default();
        assert!(!auto_decision(&cfg, Mode::Manual, false, "Bash"));
    }

    #[test]
    fn robot_defers_not_auto_approved() {
        let cfg = AutomationConfig::default();
        assert!(!auto_decision(&cfg, Mode::Robot, false, "Bash"));
    }

    #[test]
    fn auto_approves_safe_tool() {
        let cfg = AutomationConfig::default();
        assert!(auto_decision(&cfg, Mode::Auto, false, "Bash"));
    }

    #[test]
    fn auto_never_approves_danger() {
        let cfg = AutomationConfig::default();
        assert!(!auto_decision(&cfg, Mode::Auto, true, "Bash"));
    }

    #[test]
    fn plan_exit_gated_by_its_own_rule() {
        let mut cfg = AutomationConfig::default();
        assert!(auto_decision(&cfg, Mode::Auto, false, "ExitPlanMode"));
        cfg.auto.auto_accept_plan_exit = false;
        assert!(!auto_decision(&cfg, Mode::Auto, false, "ExitPlanMode"));
        assert!(auto_decision(&cfg, Mode::Auto, false, "Bash"));
    }

    #[test]
    fn auto_approve_rule_off_blocks_generic_tools() {
        let mut cfg = AutomationConfig::default();
        cfg.auto.auto_approve = false;
        assert!(!auto_decision(&cfg, Mode::Auto, false, "Bash"));
        assert!(auto_decision(&cfg, Mode::Auto, false, "ExitPlanMode"));
    }

    #[test]
    fn danger_target_prefers_command() {
        let v = serde_json::json!({"command": "rm -rf /", "file_path": "/x"});
        assert_eq!(danger_target(&v).as_deref(), Some("rm -rf /"));
        let v2 = serde_json::json!({"file_path": "/x"});
        assert_eq!(danger_target(&v2).as_deref(), Some("/x"));
        let v3 = serde_json::json!({"plan": "do stuff"});
        assert_eq!(danger_target(&v3), None);
    }
}
