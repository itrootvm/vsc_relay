use crate::auth::Auth;
use crate::hooks;
use crate::telegram::{esc_html, keyboard, Telegram};
use relay_ipc::{AsyncListener, Endpoint};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tracing::{info, warn};

pub type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

enum HookDedupState {
    Pending(Vec<oneshot::Sender<Value>>),
    Complete(Value, i64),
}

#[derive(Default)]
pub struct HookDedup {
    states: Mutex<HashMap<String, HookDedupState>>,
}

const APPROVAL_TIMEOUT_SECS: u64 = 110;

#[derive(Clone)]
pub struct IngressCtx {
    pub machine: String,
    pub tg: Option<Arc<Telegram>>,
    pub auth: Arc<Auth>,
    pub pending: Pending,
    pub hook_dedup: Arc<HookDedup>,
    pub dedup: crate::dedup::PromptDedup,
}

pub async fn serve(ctx: IngressCtx) {
    let mut listener = match AsyncListener::bind(&Endpoint::Hook) {
        Ok(l) => l,
        Err(e) => {
            warn!("hook ingress bind failed: {e}");
            return;
        }
    };
    info!("hook ingress listening");
    loop {
        match listener.accept().await {
            Ok(conn) => {
                let ctx = ctx.clone();
                tokio::spawn(handle_conn(conn, ctx));
            }
            Err(e) => {
                warn!("hook accept: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn recipients(ctx: &IngressCtx) -> Vec<i64> {
    ctx.auth.recipients().await
}

async fn handle_conn(conn: relay_ipc::AsyncConn, ctx: IngressCtx) {
    let (r, mut w) = conn.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() {
        return;
    }
    let Some(req) = hooks::parse_request(line.trim()) else {
        return;
    };

    match req.event.as_str() {
        "pre-tool-use" => {
            let decision = decide_pretool(&ctx, &req.payload).await;
            let _ = w.write_all(format!("{decision}\n").as_bytes()).await;
            let _ = w.flush().await;
        }
        "notification" => {
            notify(&ctx, &req.payload).await;
        }
        "post-tool-use" => {
            let config = crate::automation::AutomationConfig::load();
            if let Err(error) = crate::gate_controller::post_tool(&req.payload, &config) {
                warn!(target: "relay::hook", "post-tool gate observer failed open: {error}");
            }
        }
        _ => {}
    }
}

async fn decide_pretool(ctx: &IngressCtx, payload: &Value) -> Value {
    let identity = hooks::native_identity_digest(payload);
    let follower = {
        let mut states = ctx.hook_dedup.states.lock().await;
        let now = crate::automation::now_secs();
        states.retain(
            |_, state| !matches!(state, HookDedupState::Complete(_, at) if now - *at > 130),
        );
        match states.get_mut(&identity) {
            Some(HookDedupState::Complete(decision, _)) => {
                let decision = decision.clone();
                let (tool, _, cwd) = hooks::tool_summary(payload);
                crate::decision_log::record(
                    "hook",
                    decision
                        .get("decision")
                        .and_then(Value::as_str)
                        .unwrap_or("ask"),
                    json!({"tool": tool, "alias": basename(&cwd), "replayed": true,
                           "reason": "repeat of a decision already made for this tool call"}),
                );
                return decision;
            }
            Some(HookDedupState::Pending(waiters)) => {
                let (sender, receiver) = oneshot::channel();
                waiters.push(sender);
                Some(receiver)
            }
            None => {
                states.insert(identity.clone(), HookDedupState::Pending(Vec::new()));
                None
            }
        }
    };
    if let Some(follower) = follower {
        return follower.await.unwrap_or_else(|_| {
            let (tool, _, cwd) = hooks::tool_summary(payload);
            crate::decision_log::record(
                "hook",
                "ask",
                json!({"tool": tool, "alias": basename(&cwd),
                       "reason": "leader task vanished; degraded to the local prompt"}),
            );
            json!({"decision":"ask"})
        });
    }
    let decision = decide_pretool_once(ctx, payload).await;
    let waiters = {
        let mut states = ctx.hook_dedup.states.lock().await;
        let waiters = match states.remove(&identity) {
            Some(HookDedupState::Pending(waiters)) => waiters,
            Some(HookDedupState::Complete(_, _)) | None => Vec::new(),
        };
        states.insert(
            identity,
            HookDedupState::Complete(decision.clone(), crate::automation::now_secs()),
        );
        waiters
    };
    for waiter in waiters {
        let _ = waiter.send(decision.clone());
    }
    decision
}

fn decision_fields(
    tool: &str,
    alias: &str,
    payload: &Value,
    danger: bool,
    mode: crate::automation::Mode,
) -> Value {
    json!({
        "tool": tool,
        "alias": alias,
        "session": payload.get("session_id").and_then(Value::as_str).unwrap_or(""),
        "danger": danger,
        "mode": mode.label(),
    })
}

fn with_danger_pattern(mut fields: Value, pattern: Option<&String>) -> Value {
    if let Some(pattern) = pattern {
        fields["danger_pattern"] = Value::from(pattern.clone());
    }
    fields
}

async fn decide_pretool_once(ctx: &IngressCtx, payload: &Value) -> Value {
    let (tool, target, cwd) = hooks::tool_summary(payload);
    let alias = basename(&cwd);
    let danger_pattern = hooks::matched_danger(&target);
    let danger = danger_pattern.is_some();
    info!(target: "relay::hook", tool = %tool, alias = %alias, danger,
        target = %hooks::redact_raw(&tool, target.as_deref()), "pre-tool-use hook received");

    let config = crate::automation::AutomationConfig::load();
    let mode = config.resolve(
        payload.get("session_id").and_then(Value::as_str),
        &alias,
        crate::automation::now_secs(),
    );
    let gate_result = crate::gate_controller::pre_tool(payload, &config);
    if let Err(error) = &gate_result {
        warn!(target: "relay::hook", "pre-tool gate failed open to base policy: {error}");
    }
    if let Some(decision) = fail_open_gate(gate_result) {
        let gate_state = decision
            .get("gate_state")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_string();
        let hook_decision = decision
            .get("decision")
            .and_then(|value| value.as_str())
            .unwrap_or("none")
            .to_string();
        let gate_reason = decision
            .get("reason")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "hook_decision",
            gate_state = %gate_state,
            hook_decision = %hook_decision,
            "[gate] hook decision returned"
        );
        hooks::mark_gate_hold(payload);
        if hook_decision == "ask_user" {
            info!(
                target: "relay::gate",
                pipeline = "gate",
                stage = "escalation_forwarded",
                gate_state = %gate_state,
                "[gate] proof window exhausted; forwarding decision to Telegram instead of a local-only prompt"
            );
            return forward_permission_and_wait(
                ctx,
                &alias,
                &tool,
                target.as_deref(),
                danger,
                gate_reason.as_deref(),
                danger_pattern.as_deref(),
            )
            .await;
        }
        if matches!(gate_state.as_str(), "target_probe" | "controller_probe") {
            if let Some(session_id) = payload.get("session_id").and_then(Value::as_str) {
                crate::gate_controller::mark_target_probe_delivered(session_id);
            }
        }
        let mut fields = with_danger_pattern(
            decision_fields(&tool, &alias, payload, danger, mode),
            danger_pattern.as_ref(),
        );
        fields["gate_state"] = Value::from(gate_state);
        fields["reason"] = Value::from(gate_reason.unwrap_or_default());
        crate::decision_log::record("hook", &hook_decision, fields);
        return decision;
    }

    if let Some(decision) = autonomous_danger_decision(
        &config,
        payload.get("session_id").and_then(Value::as_str),
        &alias,
        danger,
        crate::automation::now_secs(),
    ) {
        info!(
            target: "relay::hook",
            tool = %tool,
            alias = %alias,
            "dangerous Robot action denied immediately for autonomous replan"
        );
        let mut fields = with_danger_pattern(
            decision_fields(&tool, &alias, payload, danger, mode),
            danger_pattern.as_ref(),
        );
        fields["reason"] = Value::from("Robot safety policy");
        crate::decision_log::record("hook", "deny", fields);
        return decision;
    }

    if danger && config.auto.guard_dangerous && mode == crate::automation::Mode::Auto {
        if let Some(tg) = &ctx.tg {
            let note = format!(
                "🛡 <b>Blocked dangerous</b> · <b>{}</b>\n📁 <code>{}</code>\n🤖 {} <code>{}</code>\n<i>auto-guard nudged the agent to a safe path</i>",
                esc_html(&ctx.machine),
                esc_html(&alias),
                esc_html(&tool),
                esc_html(&target.clone().unwrap_or_default()),
            );
            for chat in recipients(ctx).await {
                let _ = tg.send(chat, &note, None).await;
            }
        }
        info!(
            target: "relay::hook", tool = %tool, alias = %alias,
            "dangerous action auto-guarded in Auto mode; denied with safe-path nudge"
        );
        let mut fields = with_danger_pattern(
            decision_fields(&tool, &alias, payload, danger, mode),
            danger_pattern.as_ref(),
        );
        fields["reason"] = Value::from("auto-guard blocked a dangerous command");
        crate::decision_log::record("hook", "deny", fields);
        return json!({
            "decision": "deny",
            "reason": dangerous_guard_nudge(&tool, target.as_deref()),
        });
    }

    if !danger {
        let mut fields = decision_fields(&tool, &alias, payload, danger, mode);
        fields["reason"] = Value::from("not dangerous; gate returned no decision");
        crate::decision_log::record("hook", "ask", fields);
        return json!({ "decision": "ask" });
    }

    forward_permission_and_wait(
        ctx,
        &alias,
        &tool,
        target.as_deref(),
        true,
        None,
        danger_pattern.as_deref(),
    )
    .await
}

fn dangerous_guard_nudge(tool: &str, target: Option<&str>) -> String {
    let what = target
        .map(|t| format!(" ({})", relay_core::state::truncate(t, 200)))
        .unwrap_or_default();
    format!(
        "Auto-guard blocked a dangerous {tool}{what}. Do not run it - it is destructive or \
         irreversible. Reach the goal a safer, reversible way; if it is genuinely required, stop \
         and explain to the user why it is needed instead of running it."
    )
}

#[allow(clippy::too_many_arguments)]
async fn forward_permission_and_wait(
    ctx: &IngressCtx,
    alias: &str,
    tool: &str,
    target: Option<&str>,
    danger: bool,
    gate_reason: Option<&str>,
    danger_pattern: Option<&str>,
) -> Value {
    let chats = recipients(ctx).await;
    let Some(tg) = &ctx.tg else {
        crate::decision_log::record(
            "hook",
            "ask",
            json!({"tool": tool, "alias": alias, "danger": danger,
                   "reason": "no Telegram bot configured; local prompt only"}),
        );
        return json!({ "decision": "ask" });
    };
    if chats.is_empty() {
        crate::decision_log::record(
            "hook",
            "ask",
            json!({"tool": tool, "alias": alias, "danger": danger,
                   "reason": "no authorized chats; local prompt only"}),
        );
        return json!({ "decision": "ask" });
    }

    let reqid = hooks::next_request_id();
    let (txd, rxd) = oneshot::channel();
    ctx.pending.lock().await.insert(reqid.clone(), txd);

    let head = if danger { "⛔ <b>DANGEROUS</b> " } else { "" };
    let reason_line = gate_reason
        .map(|reason| format!("\n⚖️ <i>{}</i>", esc_html(reason)))
        .unwrap_or_default();
    let pattern_line = danger_pattern
        .map(|pattern| format!("\n🔎 <i>matched</i> <code>{}</code>", esc_html(pattern)))
        .unwrap_or_default();
    let text = format!(
        "🔴 {head}permission · {}\n📁 <code>{}</code>\n🤖 {} <code>{}</code>",
        esc_html(&ctx.machine),
        esc_html(alias),
        esc_html(tool),
        esc_html(target.unwrap_or_default()),
    );
    let text = format!("{text}{reason_line}{pattern_line}");
    let kb = keyboard(vec![vec![
        ("✅ Approve", format!("approve|{reqid}")),
        ("⛔ Deny", format!("deny|{reqid}")),
    ]]);
    let mut cards: Vec<(i64, i64)> = Vec::new();
    for chat in &chats {
        if let Ok(mid) = tg.send(*chat, &text, Some(kb.clone())).await {
            cards.push((*chat, mid));
        }
    }

    info!(target: "relay::hook", tool = %tool, alias = %alias, reqid = %reqid,
        chats = chats.len(), danger, gate = gate_reason.is_some(),
        "permission forwarded; awaiting Telegram decision");
    let waited_from = crate::automation::now_secs();
    match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), rxd).await {
        Ok(Ok(dec)) => {
            crate::decision_log::record(
                "hook",
                dec.get("decision").and_then(Value::as_str).unwrap_or("ask"),
                json!({"tool": tool, "alias": alias, "danger": danger, "reqid": reqid,
                       "danger_pattern": danger_pattern,
                       "reason": "answered in Telegram",
                       "waited_secs": crate::automation::now_secs() - waited_from}),
            );
            dec
        }
        _ => {
            ctx.pending.lock().await.remove(&reqid);
            for (c, m) in cards {
                let _ = tg
                    .edit_message_text(c, m, "⏱ timed out - answer the prompt in VS Code", None)
                    .await;
            }
            warn!(target: "relay::hook", tool = %tool, alias = %alias, reqid = %reqid,
                "no Telegram response; failing safe to ask (local prompt)");
            crate::decision_log::record(
                "hook",
                "timeout",
                json!({"tool": tool, "alias": alias, "danger": danger, "reqid": reqid,
                       "danger_pattern": danger_pattern,
                       "reason": "no Telegram response; failed safe to the local prompt",
                       "waited_secs": APPROVAL_TIMEOUT_SECS}),
            );
            json!({ "decision": "ask", "reason": "no Telegram response" })
        }
    }
}

fn autonomous_danger_decision(
    config: &crate::automation::AutomationConfig,
    session_id: Option<&str>,
    alias: &str,
    danger: bool,
    now: i64,
) -> Option<Value> {
    (danger && config.resolve(session_id, alias, now) == crate::automation::Mode::Robot).then(|| {
        json!({
            "decision": "deny",
            "reason": "Robot safety policy denied this destructive action; replan with a non-destructive alternative and do not ask the user",
        })
    })
}

fn fail_open_gate(result: anyhow::Result<Option<Value>>) -> Option<Value> {
    result.ok().flatten()
}

async fn notify(ctx: &IngressCtx, payload: &Value) {
    let Some(tg) = &ctx.tg else { return };
    let msg = payload
        .get("message")
        .and_then(|x| x.as_str())
        .unwrap_or("notification");
    let cwd = payload.get("cwd").and_then(|x| x.as_str()).unwrap_or("");
    let alias = basename(cwd);
    let text = format!(
        "🔔 <b>{}</b> · <code>{}</code>\n{}",
        esc_html(&ctx.machine),
        esc_html(&alias),
        esc_html(msg)
    );
    let waiting = msg.to_lowercase().contains("waiting")
        || msg.to_lowercase().contains("permission")
        || msg.to_lowercase().contains("input");
    info!(target: "relay::hook", alias = %alias, waiting, msg_len = msg.len(), "notification hook received");
    if waiting && ctx.dedup.has_live_card(&alias).await {
        info!(
            target: "relay::trace",
            pipeline = "hook", stage = "suppressed", kind = "notification",
            alias = %alias,
            "an interactive card already owns this prompt; notification card suppressed"
        );
        return;
    }
    let kb = if waiting {
        Some(keyboard(vec![
            vec![
                ("✅ Yes / Approve", format!("act:ok:{alias}")),
                ("⛔ No / Deny", format!("act:stop:{alias}:claude")),
            ],
            vec![("👁 Open window", format!("act:focus:{alias}"))],
        ]))
    } else {
        None
    };
    for chat in recipients(ctx).await {
        let _ = tg.send(chat, &text, kb.clone()).await;
    }
}

pub async fn resolve(pending: &Pending, reqid: &str, allow: bool) -> bool {
    if let Some(tx) = pending.lock().await.remove(reqid) {
        let dec = if allow {
            json!({ "decision": "allow", "reason": "approved via Telegram" })
        } else {
            json!({ "decision": "deny", "reason": "denied via Telegram" })
        };
        tx.send(dec).is_ok()
    } else {
        false
    }
}

fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_gate_failure_has_no_blocking_output() {
        let failed = Err(anyhow::anyhow!("adapter/store failure"));
        assert_eq!(fail_open_gate(failed), None);
    }

    #[test]
    fn robot_danger_is_denied_without_a_user_permission_path() {
        let mut config = crate::automation::AutomationConfig::default();
        config.set(
            crate::automation::Scope::Session("robot-session".to_string()),
            crate::automation::Mode::Robot,
            None,
        );
        let decision =
            autonomous_danger_decision(&config, Some("robot-session"), "workspace", true, 100)
                .unwrap();
        assert_eq!(decision["decision"], "deny");
        assert!(decision["reason"].as_str().unwrap().contains("do not ask"));
        assert!(autonomous_danger_decision(
            &config,
            Some("manual-session"),
            "workspace",
            true,
            100,
        )
        .is_none());
        assert!(autonomous_danger_decision(
            &config,
            Some("robot-session"),
            "workspace",
            false,
            100,
        )
        .is_none());
    }
}
