use crate::auth::Auth;
use crate::shimctl;
use crate::telegram::{esc_html, Telegram};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

async fn notify(tg: &Arc<Telegram>, auth: &Arc<Auth>, text: &str) {
    for chat in auth.recipients().await {
        let _ = tg.send(chat, text, None).await;
    }
}

pub async fn watch(tg: Arc<Telegram>, auth: Arc<Auth>) {
    let mut last_version = shimctl::env_status().version;
    let mut fail_notified = false;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let st = shimctl::env_status();

        if let Some(v) = &st.version {
            if last_version.as_deref() != Some(v.as_str()) {
                if let Some(prev) = &last_version {
                    notify(
                        &tg,
                        &auth,
                        &format!(
                            "🔄 <b>Claude Code updated</b> {} → {}",
                            esc_html(prev),
                            esc_html(v)
                        ),
                    )
                    .await;
                }
                last_version = Some(v.clone());
            }
        }

        if st.extension && !st.shim_installed {
            match tokio::task::spawn_blocking(shimctl::install_shim).await {
                Ok(Ok(_)) => {
                    fail_notified = false;
                    notify(
                        &tg,
                        &auth,
                        &format!(
                            "🔧 <b>Shim reinstalled</b> after the Claude Code update ({}).\n\
                             Only <b>new</b> chats will use it - open a new chat.\n\
                             Existing chats keep working on their old binary.",
                            esc_html(st.version.as_deref().unwrap_or("-"))
                        ),
                    )
                    .await;
                }
                Ok(Err(e)) => {
                    warn!("shim reinstall failed: {e}");
                    if !fail_notified {
                        fail_notified = true;
                        notify(
                            &tg,
                            &auth,
                            &format!(
                                "⚠️ <b>Background control is off</b> - could not reinstall the shim: {}\n\
                                 Existing chats are unaffected. It will keep retrying.",
                                esc_html(&e.to_string())
                            ),
                        )
                        .await;
                    }
                }
                Err(e) => warn!("shim reinstall task join: {e}"),
            }
        }
    }
}

pub async fn marketplace_watch(tg: Arc<Telegram>, auth: Arc<Auth>) {
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut announced: Option<String> = None;
    loop {
        if let Some(latest) = fetch_latest(&http).await {
            let installed = shimctl::env_status().version;
            let inst_sv = installed
                .as_deref()
                .map(shimctl::parse_semver)
                .unwrap_or((0, 0, 0));
            if shimctl::parse_semver(&latest) > inst_sv
                && announced.as_deref() != Some(latest.as_str())
            {
                announced = Some(latest.clone());
                notify(
                    &tg,
                    &auth,
                    &format!(
                        "⬆️ <b>Claude Code {}</b> is available (you have {}).\n\
                         VS Code updates it automatically; the relay re-wraps the shim afterward.",
                        esc_html(&latest),
                        esc_html(installed.as_deref().unwrap_or("-"))
                    ),
                )
                .await;
            }
        }
        tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
    }
}

async fn fetch_latest(http: &reqwest::Client) -> Option<String> {
    let body = serde_json::json!({
        "filters": [{
            "criteria": [
                {"filterType": 8, "value": "Microsoft.VisualStudio.Code"},
                {"filterType": 7, "value": "anthropic.claude-code"}
            ],
            "pageNumber": 1,
            "pageSize": 1,
            "sortBy": 0,
            "sortOrder": 0
        }],
        "flags": 914
    });
    let resp = http
        .post("https://marketplace.visualstudio.com/_apis/public/gallery/extensionquery")
        .header("Content-Type", "application/json")
        .header("Accept", "application/json;api-version=3.0-preview.1")
        .json(&body)
        .send()
        .await
        .ok()?;
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("results")?
        .get(0)?
        .get("extensions")?
        .get(0)?
        .get("versions")?
        .get(0)?
        .get("version")?
        .as_str()
        .map(String::from)
}
