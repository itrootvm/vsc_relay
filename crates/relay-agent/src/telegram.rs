use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

pub struct Telegram {
    token: String,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub message: Option<Message>,
    pub from: User,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

impl Telegram {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(70))
            .build()?;
        Ok(Self { token, http })
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let url = format!("https://api.telegram.org/bot{}/{}", self.token, method);
        let req_bytes = serde_json::to_vec(&body).map(|v| v.len()).unwrap_or(0);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.without_url())
            .with_context(|| format!("telegram {method}"))?;
        let status = resp.status().as_u16();
        let raw = resp
            .text()
            .await
            .map_err(|e| e.without_url())
            .with_context(|| format!("telegram {method} body"))?;
        tracing::debug!(
            target: "relay::trace",
            stage = "tg_call",
            method,
            req_bytes,
            resp_bytes = raw.len(),
            status
        );
        let v: Value =
            serde_json::from_str(&raw).with_context(|| format!("telegram {method} json"))?;
        if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
            let desc = v
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("request failed");
            bail!("telegram {method}: {desc}");
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn send(&self, chat_id: i64, text: &str, keyboard: Option<Value>) -> Result<i64> {
        self.send_ex(chat_id, text, keyboard, false).await
    }

    pub async fn send_ex(
        &self,
        chat_id: i64,
        text: &str,
        keyboard: Option<Value>,
        silent: bool,
    ) -> Result<i64> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
            "disable_notification": silent,
        });
        if let Some(kb) = keyboard {
            body["reply_markup"] = kb;
        }
        let res = self.call("sendMessage", body).await?;
        Ok(res.get("message_id").and_then(|x| x.as_i64()).unwrap_or(0))
    }

    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        keyboard: Option<Value>,
    ) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        if let Some(kb) = keyboard {
            body["reply_markup"] = kb;
        }
        self.call("editMessageText", body).await?;
        Ok(())
    }

    pub async fn answer_callback(&self, id: &str, text: &str) -> Result<()> {
        self.call(
            "answerCallbackQuery",
            json!({ "callback_query_id": id, "text": text }),
        )
        .await?;
        Ok(())
    }

    pub async fn set_my_commands(&self, commands: &[(&str, &str)]) -> Result<()> {
        let list: Vec<Value> = commands
            .iter()
            .map(|(command, description)| json!({ "command": command, "description": description }))
            .collect();
        self.call("setMyCommands", json!({ "commands": list }))
            .await?;
        Ok(())
    }

    pub async fn get_updates(&self, offset: i64, timeout: u32) -> Result<Vec<Update>> {
        let res = self
            .call(
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": timeout,
                    "allowed_updates": ["message", "callback_query"],
                }),
            )
            .await?;
        let updates: Vec<Update> = serde_json::from_value(res)?;
        Ok(updates)
    }
}

pub fn keyboard<L: Into<String>>(rows: Vec<Vec<(L, String)>>) -> Value {
    let kb: Vec<Vec<Value>> = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|(label, data)| {
                    json!({ "text": label.into(), "callback_data": crate::actions::encode(data) })
                })
                .collect()
        })
        .collect();
    json!({ "inline_keyboard": kb })
}

pub fn force_reply() -> Value {
    json!({ "force_reply": true, "input_field_placeholder": "type your prompt" })
}

pub fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
