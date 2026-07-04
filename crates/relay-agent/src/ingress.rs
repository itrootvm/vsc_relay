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

const APPROVAL_TIMEOUT_SECS: u64 = 110;

#[derive(Clone)]
pub struct IngressCtx {
    pub machine: String,
    pub tg: Option<Arc<Telegram>>,
    pub auth: Arc<Auth>,
    pub pending: Pending,
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
        _ => {}
    }
}

async fn decide_pretool(ctx: &IngressCtx, payload: &Value) -> Value {
    let (tool, target, cwd) = hooks::tool_summary(payload);
    let alias = basename(&cwd);
    let danger = hooks::is_destructive(&target);
    info!(target: "relay::hook", tool = %tool, alias = %alias, danger,
        target = %hooks::redact_raw(&tool, target.as_deref()), "pre-tool-use hook received");

    if !danger {
        return json!({ "decision": "ask" });
    }

    let chats = recipients(ctx).await;
    let Some(tg) = &ctx.tg else {
        return json!({ "decision": "ask" });
    };
    if chats.is_empty() {
        return json!({ "decision": "ask" });
    }

    let reqid = hooks::next_request_id();
    let (txd, rxd) = oneshot::channel();
    ctx.pending.lock().await.insert(reqid.clone(), txd);

    let head = if danger { "⛔ <b>DANGEROUS</b> " } else { "" };
    let text = format!(
        "🔴 {head}permission · {}\n📁 <code>{}</code>\n🤖 {} <code>{}</code>",
        esc_html(&ctx.machine),
        esc_html(&alias),
        esc_html(&tool),
        esc_html(&target.clone().unwrap_or_default()),
    );
    let kb = keyboard(vec![vec![
        ("✅ Approve", format!("approve|{reqid}")),
        ("⛔ Deny", format!("deny|{reqid}")),
    ]]);
    for chat in &chats {
        let _ = tg.send(*chat, &text, Some(kb.clone())).await;
    }

    info!(target: "relay::hook", tool = %tool, alias = %alias, reqid = %reqid,
        chats = chats.len(), "dangerous permission forwarded; awaiting Telegram decision");
    match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), rxd).await {
        Ok(Ok(dec)) => dec,
        _ => {
            ctx.pending.lock().await.remove(&reqid);
            warn!(target: "relay::hook", tool = %tool, alias = %alias, reqid = %reqid,
                "no Telegram response; failing safe to ask (local prompt)");
            json!({ "decision": "ask", "reason": "no Telegram response" })
        }
    }
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
