use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

pub struct Telegram {
    token: String,
    http: reqwest::Client,
    fallback: Option<reqwest::Client>,
    route: Route,
    api_base: String,
}

const FAILURES_BEFORE_SWITCH: u32 = 2;
const PRIMARY_RETRY: Duration = Duration::from_secs(15 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const SEND_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

fn retry_delay(method: &str, attempt: u32, connect_error: bool) -> Option<Duration> {
    if method == "getUpdates" || !connect_error || attempt >= SEND_ATTEMPTS {
        return None;
    }
    RETRY_BACKOFF
        .get(attempt.saturating_sub(1) as usize)
        .copied()
}

pub struct Route {
    on_fallback: std::sync::atomic::AtomicBool,
    failures: std::sync::atomic::AtomicU32,
    switched_at: std::sync::Mutex<Option<std::time::Instant>>,
}

impl Route {
    pub fn new(start_on_fallback: bool) -> Self {
        Route {
            on_fallback: std::sync::atomic::AtomicBool::new(start_on_fallback),
            failures: std::sync::atomic::AtomicU32::new(0),
            switched_at: std::sync::Mutex::new(None),
        }
    }

    pub fn on_fallback(&self) -> bool {
        self.on_fallback.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn note_success(&self) {
        self.failures.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn note_failure(&self, has_fallback: bool) -> bool {
        if !has_fallback {
            return false;
        }
        let seen = self
            .failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if seen < FAILURES_BEFORE_SWITCH {
            return false;
        }
        self.failures.store(0, std::sync::atomic::Ordering::Relaxed);
        let now_on_fallback = !self.on_fallback();
        self.on_fallback
            .store(now_on_fallback, std::sync::atomic::Ordering::Relaxed);
        *self.switched_at.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(std::time::Instant::now());
        true
    }

    pub fn due_to_retry_primary(&self) -> bool {
        if !self.on_fallback() {
            return false;
        }
        let mut guard = self.switched_at.lock().unwrap_or_else(|p| p.into_inner());
        match *guard {
            Some(at) if at.elapsed() >= PRIMARY_RETRY => {
                *guard = Some(std::time::Instant::now());
                true
            }
            Some(_) => false,
            None => {
                *guard = Some(std::time::Instant::now());
                false
            }
        }
    }

    pub fn back_to_primary(&self) {
        self.on_fallback
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.failures.store(0, std::sync::atomic::Ordering::Relaxed);
    }
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
    pub caption: Option<String>,
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
    #[serde(default)]
    pub photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    pub document: Option<Document>,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub audio: Option<Audio>,
    #[serde(default)]
    pub video: Option<Video>,
    #[serde(default)]
    pub video_note: Option<VideoNote>,
    #[serde(default)]
    pub forward_origin: Option<Value>,
    #[serde(default)]
    pub forward_sender_name: Option<String>,
    #[serde(default)]
    pub forward_from: Option<User>,
    #[serde(default)]
    pub forward_from_chat: Option<Chat>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
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
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    #[serde(default)]
    pub width: i64,
    #[serde(default)]
    pub height: i64,
    #[serde(default)]
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    pub file_id: String,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    pub file_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Audio {
    pub file_id: String,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Video {
    pub file_id: String,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VideoNote {
    pub file_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Photo,
    Video,
    VideoNote,
    Voice,
    Audio,
    Document,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Photo => "photo",
            MediaKind::Video => "video",
            MediaKind::VideoNote => "video note",
            MediaKind::Voice => "voice message",
            MediaKind::Audio => "audio",
            MediaKind::Document => "document",
        }
    }

    pub fn default_ext(self) -> &'static str {
        match self {
            MediaKind::Photo => "jpg",
            MediaKind::Video => "mp4",
            MediaKind::VideoNote => "mp4",
            MediaKind::Voice => "ogg",
            MediaKind::Audio => "mp3",
            MediaKind::Document => "bin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaRef {
    pub kind: MediaKind,
    pub file_id: String,
    pub file_name: Option<String>,
}

impl Message {
    pub fn has_media(&self) -> bool {
        self.photo.is_some()
            || self.document.is_some()
            || self.voice.is_some()
            || self.audio.is_some()
            || self.video.is_some()
            || self.video_note.is_some()
    }

    pub fn media_refs(&self) -> Vec<MediaRef> {
        let mut out = Vec::new();
        if let Some(photos) = &self.photo {
            if let Some(best) = photos
                .iter()
                .max_by_key(|p| p.file_size.unwrap_or(p.width.saturating_mul(p.height)))
            {
                out.push(MediaRef {
                    kind: MediaKind::Photo,
                    file_id: best.file_id.clone(),
                    file_name: None,
                });
            }
        }
        if let Some(d) = &self.document {
            out.push(MediaRef {
                kind: MediaKind::Document,
                file_id: d.file_id.clone(),
                file_name: d.file_name.clone(),
            });
        }
        if let Some(v) = &self.voice {
            out.push(MediaRef {
                kind: MediaKind::Voice,
                file_id: v.file_id.clone(),
                file_name: None,
            });
        }
        if let Some(a) = &self.audio {
            out.push(MediaRef {
                kind: MediaKind::Audio,
                file_id: a.file_id.clone(),
                file_name: a.file_name.clone(),
            });
        }
        if let Some(v) = &self.video {
            out.push(MediaRef {
                kind: MediaKind::Video,
                file_id: v.file_id.clone(),
                file_name: v.file_name.clone(),
            });
        }
        if let Some(v) = &self.video_note {
            out.push(MediaRef {
                kind: MediaKind::VideoNote,
                file_id: v.file_id.clone(),
                file_name: None,
            });
        }
        out
    }

    pub fn forward_label(&self) -> Option<String> {
        if let Some(origin) = &self.forward_origin {
            if let Some(label) = forward_origin_label(origin) {
                return Some(label);
            }
        }
        if let Some(name) = &self.forward_sender_name {
            return Some(name.clone());
        }
        if let Some(user) = &self.forward_from {
            let mut parts: Vec<String> = Vec::new();
            if let Some(f) = &user.first_name {
                parts.push(f.clone());
            }
            if let Some(l) = &user.last_name {
                parts.push(l.clone());
            }
            if !parts.is_empty() {
                return Some(parts.join(" "));
            }
            if let Some(u) = &user.username {
                return Some(format!("@{u}"));
            }
        }
        if let Some(chat) = &self.forward_from_chat {
            if let Some(t) = &chat.title {
                return Some(t.clone());
            }
            if let Some(u) = &chat.username {
                return Some(format!("@{u}"));
            }
        }
        None
    }
}

fn forward_origin_label(origin: &Value) -> Option<String> {
    match origin.get("type").and_then(|x| x.as_str()) {
        Some("user") => origin
            .get("sender_user")
            .and_then(|u| u.get("first_name"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        Some("hidden_user") => origin
            .get("sender_user_name")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        Some("chat") => origin
            .get("sender_chat")
            .and_then(|c| c.get("title"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        Some("channel") => origin
            .get("chat")
            .and_then(|c| c.get("title"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

const PROXY_VARS: &[&str] = &[
    "VSC_RELAY_PROXY",
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
];

fn proxy_from(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    PROXY_VARS.iter().find_map(|name| {
        let value = lookup(name)?;
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

pub fn configured_proxy() -> Option<String> {
    proxy_from(|name| std::env::var(name).ok())
}

pub fn redact_proxy(address: &str) -> String {
    let Some((scheme, rest)) = address.split_once("://") else {
        return address.to_string();
    };
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}"),
        None => address.to_string(),
    }
}

pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";

fn api_base_from(lookup: impl Fn(&str) -> Option<String>) -> String {
    lookup("VSC_RELAY_TELEGRAM_API")
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

pub fn configured_api_base() -> String {
    api_base_from(|name| std::env::var(name).ok())
}

impl Telegram {
    pub fn new(token: String) -> Result<Self> {
        let base = || {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(70))
                .connect_timeout(CONNECT_TIMEOUT)
        };
        let direct = base().build()?;
        let fallback = match configured_proxy() {
            Some(address) => Some(
                base()
                    .proxy(reqwest::Proxy::all(&address).with_context(|| {
                        format!("proxy {} is not a usable address", redact_proxy(&address))
                    })?)
                    .build()?,
            ),
            None => None,
        };
        Ok(Self {
            token,
            http: direct,
            route: Route::new(fallback.is_some()),
            fallback,
            api_base: configured_api_base(),
        })
    }

    fn client(&self) -> &reqwest::Client {
        match (self.route.on_fallback(), self.fallback.as_ref()) {
            (true, Some(proxied)) => proxied,
            _ => &self.http,
        }
    }

    fn route_label(&self) -> &'static str {
        if self.route.on_fallback() {
            "proxy"
        } else {
            "direct"
        }
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let url = format!("{}/bot{}/{}", self.api_base, self.token, method);
        let req_bytes = serde_json::to_vec(&body).map(|v| v.len()).unwrap_or(0);
        if self.route.due_to_retry_primary()
            && self
                .http
                .post(format!("{}/bot{}/getMe", self.api_base, self.token))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false)
        {
            self.route.back_to_primary();
            tracing::info!(
                target: "relay::trace",
                pipeline = "telegram", stage = "route", route = "direct",
                "the direct route answered again, leaving the proxy"
            );
        }
        let mut attempt: u32 = 0;
        let resp = loop {
            attempt += 1;
            let was = self.route_label();
            let sent = self
                .client()
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| e.without_url());
            match sent {
                Ok(resp) => {
                    self.route.note_success();
                    if attempt > 1 {
                        tracing::info!(
                            target: "relay::trace",
                            pipeline = "telegram", stage = "recovered",
                            method, attempt, route = was,
                            "telegram call went through on a retry"
                        );
                        crate::decision_log::record(
                            "telegram",
                            "send_recovered",
                            json!({"method": method, "attempts": attempt, "route": was}),
                        );
                    }
                    break resp;
                }
                Err(err) => {
                    if self.route.note_failure(self.fallback.is_some()) {
                        tracing::warn!(
                            target: "relay::trace",
                            pipeline = "telegram", stage = "route",
                            from = was, to = self.route_label(),
                            "telegram is unreachable on this route, switching"
                        );
                    }
                    match retry_delay(method, attempt, err.is_connect()) {
                        Some(delay) => {
                            tracing::warn!(
                                target: "relay::trace",
                                pipeline = "telegram", stage = "retry",
                                method, attempt, route = was,
                                "telegram call never reached the server, retrying: {err}"
                            );
                            tokio::time::sleep(delay).await;
                        }
                        None => {
                            if method != "getUpdates" {
                                crate::decision_log::record(
                                    "telegram",
                                    "send_lost",
                                    json!({
                                        "method": method,
                                        "attempts": attempt,
                                        "route": was,
                                        "connect_error": err.is_connect(),
                                        "reason": err.to_string(),
                                    }),
                                );
                            }
                            return Err(err)
                                .with_context(|| format!("telegram {method} over {was}"));
                        }
                    }
                }
            }
        };
        let status = resp.status().as_u16();
        let raw = resp
            .text()
            .await
            .map_err(|e| e.without_url())
            .with_context(|| format!("telegram {method} body"))?;
        let v: Value =
            serde_json::from_str(&raw).with_context(|| format!("telegram {method} json"))?;
        let ok = v.get("ok").and_then(|x| x.as_bool()) == Some(true);
        if method == "sendMessage" {
            tracing::info!(
                target: "relay::trace",
                pipeline = "telegram",
                stage = "api_call",
                method,
                req_bytes,
                resp_bytes = raw.len(),
                status,
                ok,
                "telegram API delivery receipt"
            );
        } else {
            tracing::debug!(
                target: "relay::trace",
                pipeline = "telegram",
                stage = "api_call",
                method,
                req_bytes,
                resp_bytes = raw.len(),
                status,
                ok,
                "telegram API call"
            );
        }
        if !ok {
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

    pub async fn get_file(&self, file_id: &str) -> Result<String> {
        let res = self.call("getFile", json!({ "file_id": file_id })).await?;
        res.get("file_path")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .context("telegram getFile: missing file_path")
    }

    pub async fn download_file(&self, file_path: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let url = format!("{}/file/bot{}/{}", self.api_base, self.token, file_path);
        let mut resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| e.without_url())
            .context("telegram download")?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("telegram download: status {status}");
        }
        if let Some(len) = resp.content_length() {
            if len > max_bytes {
                bail!("telegram download: file too large ({len} > {max_bytes})");
            }
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| e.without_url())
            .context("telegram download chunk")?
        {
            if buf.len() as u64 + chunk.len() as u64 > max_bytes {
                bail!("telegram download: exceeds cap {max_bytes}");
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Message {
        serde_json::from_value(v).expect("parse message")
    }

    #[test]
    fn a_connect_failure_is_retried_with_growing_backoff_then_given_up() {
        assert_eq!(
            retry_delay("sendMessage", 1, true),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            retry_delay("sendMessage", 2, true),
            Some(Duration::from_secs(3))
        );
        assert_eq!(retry_delay("sendMessage", 3, true), None);
    }

    #[test]
    fn a_failure_after_the_request_left_is_never_retried_so_nothing_is_sent_twice() {
        assert_eq!(retry_delay("sendMessage", 1, false), None);
        assert_eq!(retry_delay("editMessageText", 1, false), None);
    }

    #[test]
    fn the_long_poll_is_not_retried_inside_the_call_because_its_loop_repeats_it() {
        assert_eq!(retry_delay("getUpdates", 1, true), None);
    }

    #[test]
    fn two_failed_attempts_move_the_third_one_onto_the_other_route() {
        let route = Route::new(false);
        assert!(!route.note_failure(true));
        assert!(route.note_failure(true));
        assert!(
            route.on_fallback(),
            "the last retry of a message goes out over the other route"
        );
    }

    #[test]
    fn photo_message_selects_largest_size() {
        let m = parse(json!({
            "message_id": 10,
            "chat": {"id": 42},
            "caption": "look at this",
            "photo": [
                {"file_id": "small", "width": 90, "height": 60, "file_size": 1200},
                {"file_id": "big", "width": 1280, "height": 720, "file_size": 240000}
            ]
        }));
        assert!(m.has_media());
        assert_eq!(m.caption.as_deref(), Some("look at this"));
        let refs = m.media_refs();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, MediaKind::Photo);
        assert_eq!(refs[0].file_id, "big");
    }

    #[test]
    fn document_and_voice_are_captured() {
        let doc = parse(json!({
            "message_id": 11,
            "chat": {"id": 42},
            "document": {"file_id": "doc1", "file_name": "spec.pdf", "mime_type": "application/pdf", "file_size": 5000}
        }));
        let refs = doc.media_refs();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, MediaKind::Document);
        assert_eq!(refs[0].file_name.as_deref(), Some("spec.pdf"));

        let voice = parse(json!({
            "message_id": 12,
            "chat": {"id": 42},
            "voice": {"file_id": "v1", "duration": 3, "mime_type": "audio/ogg", "file_size": 8000}
        }));
        let vrefs = voice.media_refs();
        assert_eq!(vrefs.len(), 1);
        assert_eq!(vrefs[0].kind, MediaKind::Voice);
    }

    #[test]
    fn text_only_message_has_no_media() {
        let m = parse(json!({
            "message_id": 13,
            "chat": {"id": 42},
            "text": "just text"
        }));
        assert!(!m.has_media());
        assert!(m.media_refs().is_empty());
    }

    #[test]
    fn forward_label_reads_modern_and_legacy() {
        let modern = parse(json!({
            "message_id": 14,
            "chat": {"id": 42},
            "text": "fwd",
            "forward_origin": {"type": "user", "sender_user": {"id": 7, "first_name": "Alex"}}
        }));
        assert_eq!(modern.forward_label().as_deref(), Some("Alex"));

        let legacy = parse(json!({
            "message_id": 15,
            "chat": {"id": 42},
            "text": "fwd",
            "forward_sender_name": "Hidden Person"
        }));
        assert_eq!(legacy.forward_label().as_deref(), Some("Hidden Person"));
    }

    #[test]
    fn the_relays_own_proxy_setting_wins_over_the_shell_environment() {
        let env = |name: &str| match name {
            "VSC_RELAY_PROXY" => Some("socks5://127.0.0.1:1080".to_string()),
            "ALL_PROXY" => Some("http://corp:3128".to_string()),
            _ => None,
        };
        assert_eq!(proxy_from(env).as_deref(), Some("socks5://127.0.0.1:1080"));

        let only_shell =
            |name: &str| (name == "HTTPS_PROXY").then(|| "  http://corp:3128  ".to_string());
        assert_eq!(proxy_from(only_shell).as_deref(), Some("http://corp:3128"));

        let blank = |name: &str| (name == "ALL_PROXY").then(String::new);
        assert_eq!(proxy_from(blank), None);
        assert_eq!(proxy_from(|_| None), None);
    }

    #[test]
    fn a_custom_api_endpoint_replaces_the_default_and_never_keeps_a_trailing_slash() {
        assert_eq!(api_base_from(|_| None), DEFAULT_API_BASE);
        assert_eq!(api_base_from(|_| Some("   ".to_string())), DEFAULT_API_BASE);
        let worker = |name: &str| {
            (name == "VSC_RELAY_TELEGRAM_API")
                .then(|| "https://tg.example.workers.dev/".to_string())
        };
        assert_eq!(api_base_from(worker), "https://tg.example.workers.dev");
        let local = |name: &str| {
            (name == "VSC_RELAY_TELEGRAM_API").then(|| "http://127.0.0.1:8081".to_string())
        };
        assert_eq!(api_base_from(local), "http://127.0.0.1:8081");
    }

    #[test]
    fn a_proxy_password_never_reaches_the_log() {
        assert_eq!(
            redact_proxy("socks5://user:secret@proxy.example:1080"),
            "socks5://***@proxy.example:1080"
        );
        assert_eq!(
            redact_proxy("http://corp.example:3128"),
            "http://corp.example:3128"
        );
        assert_eq!(redact_proxy("direct"), "direct");
    }

    #[test]
    fn one_hiccup_keeps_the_route_and_two_failures_switch_it() {
        let route = Route::new(false);
        assert!(!route.on_fallback());
        assert!(
            !route.note_failure(true),
            "a single failure is not a verdict"
        );
        assert!(!route.on_fallback());
        route.note_success();
        assert!(
            !route.note_failure(true),
            "a success clears what came before it"
        );
        assert!(route.note_failure(true), "two in a row switch the route");
        assert!(route.on_fallback());
        assert!(
            !route.note_failure(true),
            "the counter starts over after a switch"
        );
        assert!(
            route.note_failure(true),
            "if the other route dies too, it switches back rather than giving up"
        );
        assert!(!route.on_fallback());
    }

    #[test]
    fn without_a_proxy_there_is_nothing_to_switch_to() {
        let route = Route::new(false);
        for _ in 0..5 {
            assert!(!route.note_failure(false));
        }
        assert!(!route.on_fallback());
        assert!(!route.due_to_retry_primary());
    }

    #[test]
    fn the_primary_is_retried_only_after_the_cooldown_and_a_win_returns_to_it() {
        let route = Route::new(true);
        assert!(route.on_fallback());
        assert!(
            !route.due_to_retry_primary(),
            "the clock starts on first ask"
        );
        assert!(!route.due_to_retry_primary(), "and does not fire at once");
        route.back_to_primary();
        assert!(!route.on_fallback());
        assert!(
            !route.due_to_retry_primary(),
            "on the primary there is nothing to probe"
        );
    }
}
