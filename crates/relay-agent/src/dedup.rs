use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub type CardRef = (i64, i64);

pub const COLLAPSE_NOTE: &str = "🟡 answer this on the interactive question card";

const TTL: Duration = Duration::from_secs(30 * 60);

struct DiskEntry {
    cards: Vec<CardRef>,
    at: Instant,
}

const LIVE_CARD_WINDOW: Duration = Duration::from_secs(90);

#[derive(Default)]
struct Inner {
    received: HashMap<String, Instant>,
    disk: HashMap<String, DiskEntry>,
    live_by_alias: HashMap<String, Instant>,
}

impl Inner {
    fn prune(&mut self, now: Instant) {
        self.received.retain(|_, t| now.duration_since(*t) < TTL);
        self.disk.retain(|_, e| now.duration_since(e.at) < TTL);
        self.live_by_alias
            .retain(|_, t| now.duration_since(*t) < LIVE_CARD_WINDOW);
    }
}

#[derive(Clone, Default)]
pub struct PromptDedup {
    inner: Arc<Mutex<Inner>>,
}

impl PromptDedup {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn out_owns(&self, tool_use_id: &str) -> bool {
        if tool_use_id.is_empty() {
            return false;
        }
        let mut g = self.inner.lock().await;
        g.prune(Instant::now());
        g.received.contains_key(tool_use_id)
    }

    pub async fn mark_received(&self, tool_use_id: &str) -> Vec<CardRef> {
        if tool_use_id.is_empty() {
            return Vec::new();
        }
        let now = Instant::now();
        let mut g = self.inner.lock().await;
        g.prune(now);
        g.received.insert(tool_use_id.to_string(), now);
        g.disk
            .remove(tool_use_id)
            .map(|e| e.cards)
            .unwrap_or_default()
    }

    pub async fn register_disk(
        &self,
        tool_use_id: &str,
        cards: Vec<CardRef>,
    ) -> Option<Vec<CardRef>> {
        if tool_use_id.is_empty() || cards.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut g = self.inner.lock().await;
        if g.received.contains_key(tool_use_id) {
            return Some(cards);
        }
        g.disk
            .insert(tool_use_id.to_string(), DiskEntry { cards, at: now });
        None
    }

    pub async fn mark_live_card(&self, alias: &str) {
        if alias.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut g = self.inner.lock().await;
        g.prune(now);
        g.live_by_alias.insert(alias.to_string(), now);
    }

    pub async fn has_live_card(&self, alias: &str) -> bool {
        if alias.is_empty() {
            return false;
        }
        let now = Instant::now();
        let mut g = self.inner.lock().await;
        g.prune(now);
        g.live_by_alias.contains_key(alias)
    }

    pub async fn forget(&self, tool_use_id: &str) -> Vec<CardRef> {
        if tool_use_id.is_empty() {
            return Vec::new();
        }
        let mut g = self.inner.lock().await;
        g.received.remove(tool_use_id);
        g.disk
            .remove(tool_use_id)
            .map(|e| e.cards)
            .unwrap_or_default()
    }
}

pub fn refs_tool_use_id(v: &Value, id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    match v {
        Value::Object(map) => map.iter().any(|(k, val)| {
            (matches!(k.as_str(), "tool_use_id" | "toolUseID" | "tool_use_ids")
                && val_has_id(val, id))
                || refs_tool_use_id(val, id)
        }),
        Value::Array(arr) => arr.iter().any(|e| refs_tool_use_id(e, id)),
        _ => false,
    }
}

fn val_has_id(val: &Value, id: &str) -> bool {
    match val {
        Value::String(s) => s == id,
        Value::Array(a) => a.iter().any(|e| e.as_str() == Some(id)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn out_first_suppresses_disk() {
        let d = PromptDedup::new();
        assert!(d.mark_received("tu_1").await.is_empty());
        assert!(d.out_owns("tu_1").await);
        assert_eq!(
            d.register_disk("tu_1", vec![(1, 2)]).await,
            Some(vec![(1, 2)])
        );
    }

    #[tokio::test]
    async fn disk_first_collapses_on_receipt() {
        let d = PromptDedup::new();
        assert!(!d.out_owns("tu_2").await);
        assert_eq!(d.register_disk("tu_2", vec![(3, 4)]).await, None);
        assert_eq!(d.mark_received("tu_2").await, vec![(3, 4)]);
    }

    #[tokio::test]
    async fn forget_drops_state() {
        let d = PromptDedup::new();
        d.mark_received("tu_3").await;
        d.register_disk("tu_4", vec![(5, 6)]).await;
        assert_eq!(d.forget("tu_4").await, vec![(5, 6)]);
        assert!(!d.out_owns("tu_3").await || d.forget("tu_3").await.is_empty());
    }

    #[test]
    fn refs_matches_snake_and_camel() {
        let result = json!({"type":"tool_result","tool_use_id":"tu_x","content":"ok"});
        assert!(refs_tool_use_id(&result, "tu_x"));
        let resp = json!({"response":{"toolUseID":"tu_y"}});
        assert!(refs_tool_use_id(&resp, "tu_y"));
    }

    #[test]
    fn refs_rejects_substring_and_text() {
        let text = json!({"message":{"content":[{"type":"text","text":"see tu_x here"}]}});
        assert!(!refs_tool_use_id(&text, "tu_x"));
        let longer = json!({"tool_use_id":"tu_xy"});
        assert!(!refs_tool_use_id(&longer, "tu_x"));
    }

    #[test]
    fn refs_matches_id_array() {
        let v = json!({"tool_use_ids":["a","tu_z"]});
        assert!(refs_tool_use_id(&v, "tu_z"));
    }

    #[tokio::test]
    async fn a_notification_is_only_suppressed_while_an_interactive_card_is_live() {
        let d = PromptDedup::new();
        assert!(!d.has_live_card("demo_app").await);
        assert!(!d.has_live_card("").await);

        d.mark_live_card("demo_app").await;
        assert!(d.has_live_card("demo_app").await);
        assert!(
            !d.has_live_card("other_app").await,
            "other workspaces stay untouched"
        );

        d.mark_live_card("").await;
        assert!(
            !d.has_live_card("").await,
            "an empty alias never owns anything"
        );
    }
}
