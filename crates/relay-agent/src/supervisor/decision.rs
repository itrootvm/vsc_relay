use anyhow::{anyhow, Result};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Continue,
    Retry,
    AcceptPlan,
    Feedback,
    Stop,
    Wait,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        match s.trim().to_lowercase().as_str() {
            "continue" => Some(Action::Continue),
            "retry" => Some(Action::Retry),
            "accept_plan" | "accept" => Some(Action::AcceptPlan),
            "feedback" => Some(Action::Feedback),
            "stop" => Some(Action::Stop),
            "wait" => Some(Action::Wait),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Action::Continue => "continue",
            Action::Retry => "retry",
            Action::AcceptPlan => "accept_plan",
            Action::Feedback => "feedback",
            Action::Stop => "stop",
            Action::Wait => "wait",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub action: Action,
    pub message: Option<String>,
    pub option_index: Option<usize>,
    pub wait_seconds: Option<u64>,
    pub reason: String,
    pub confidence: Option<f64>,
}

pub fn decision_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["continue", "retry", "accept_plan", "feedback", "stop", "wait"]
            },
            "message": { "type": "string" },
            "option_index": { "type": "integer", "minimum": 0 },
            "wait_seconds": { "type": "integer", "minimum": 0 },
            "reason": { "type": "string" },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "required": ["action", "reason"],
        "additionalProperties": false
    })
}

pub(crate) fn extract_json(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

pub fn parse_decision(text: &str) -> Result<Decision> {
    let json =
        extract_json(text).ok_or_else(|| anyhow!("no JSON object in supervisor response"))?;
    let v: serde_json::Value = serde_json::from_str(&json)?;
    let action = v
        .get("action")
        .and_then(|a| a.as_str())
        .and_then(Action::parse)
        .ok_or_else(|| anyhow!("missing or invalid action"))?;
    Ok(Decision {
        action,
        message: v
            .get("message")
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        option_index: v
            .get("option_index")
            .and_then(|x| x.as_u64())
            .map(|n| n as usize),
        wait_seconds: v.get("wait_seconds").and_then(|x| x.as_u64()),
        reason: v
            .get("reason")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        confidence: v.get("confidence").and_then(|x| x.as_f64()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_json() {
        let d = parse_decision(r#"{"action":"continue","reason":"looks stuck"}"#).unwrap();
        assert_eq!(d.action, Action::Continue);
        assert_eq!(d.reason, "looks stuck");
        assert!(d.message.is_none());
    }

    #[test]
    fn parses_fenced_json_with_prose() {
        let text = "Sure, here is my call:\n```json\n{\"action\":\"feedback\",\"message\":\"tighten the loop\",\"reason\":\"needs fix\",\"confidence\":0.7}\n```\nhope that helps";
        let d = parse_decision(text).unwrap();
        assert_eq!(d.action, Action::Feedback);
        assert_eq!(d.message.as_deref(), Some("tighten the loop"));
        assert_eq!(d.confidence, Some(0.7));
    }

    #[test]
    fn handles_braces_inside_strings() {
        let d = parse_decision(r#"{"action":"feedback","message":"use {} not []","reason":"x"}"#)
            .unwrap();
        assert_eq!(d.message.as_deref(), Some("use {} not []"));
    }

    #[test]
    fn parses_option_index_and_wait() {
        let d = parse_decision(r#"{"action":"wait","wait_seconds":300,"reason":"limit"}"#).unwrap();
        assert_eq!(d.wait_seconds, Some(300));
        let d2 = parse_decision(r#"{"action":"accept","option_index":2,"reason":"pick"}"#).unwrap();
        assert_eq!(d2.action, Action::AcceptPlan);
        assert_eq!(d2.option_index, Some(2));
    }

    #[test]
    fn rejects_missing_or_bad_action() {
        assert!(parse_decision(r#"{"reason":"no action"}"#).is_err());
        assert!(parse_decision(r#"{"action":"nuke","reason":"bad"}"#).is_err());
        assert!(parse_decision("not json at all").is_err());
    }

    #[test]
    fn schema_lists_all_actions() {
        let s = decision_schema();
        let en = s["properties"]["action"]["enum"].as_array().unwrap();
        assert_eq!(en.len(), 6);
    }
}
