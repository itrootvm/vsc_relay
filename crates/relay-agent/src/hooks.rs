use anyhow::Result;
use relay_ipc::{BlockingConn, Endpoint};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn is_hook_command(arg: &str) -> bool {
    arg == "hook"
}

pub fn run_hook(args: &[String]) -> Result<()> {
    let event = args.get(1).cloned().unwrap_or_default();
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let request = json!({ "event": event, "payload": payload });

    let mut conn = match relay_ipc::connect_blocking(&Endpoint::Hook) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    writeln!(conn, "{request}").ok();
    conn.flush().ok();

    if event == "pre-tool-use" {
        if let Some(resp) = read_decision_line(conn, Duration::from_secs(125)) {
            if !resp.trim().is_empty() {
                if let Ok(dec) = serde_json::from_str::<Value>(&resp) {
                    emit_decision(&dec);
                }
            }
        }
    }
    Ok(())
}

fn read_decision_line(conn: BlockingConn, timeout: Duration) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut resp = String::new();
        let _ = BufReader::new(conn).read_line(&mut resp);
        let _ = tx.send(resp);
    });
    rx.recv_timeout(timeout).ok()
}

fn emit_decision(dec: &Value) {
    let decision = dec
        .get("decision")
        .and_then(|d| d.as_str())
        .unwrap_or("ask");
    let reason = dec.get("reason").and_then(|r| r.as_str()).unwrap_or("");
    match decision {
        "allow" | "deny" => {
            let out = json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": decision,
                    "permissionDecisionReason": reason,
                }
            });
            println!("{out}");
        }
        _ => {}
    }
}

pub fn next_request_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("r{n}")
}

pub struct HookRequest {
    pub event: String,
    pub payload: Value,
}

pub fn parse_request(line: &str) -> Option<HookRequest> {
    let v: Value = serde_json::from_str(line).ok()?;
    Some(HookRequest {
        event: v.get("event")?.as_str()?.to_string(),
        payload: v.get("payload").cloned().unwrap_or(Value::Null),
    })
}

fn is_bare_url(t: &str) -> bool {
    let has_scheme = t.starts_with("http://")
        || t.starts_with("https://")
        || t.starts_with("ws://")
        || t.starts_with("wss://");
    has_scheme && !t.chars().any(char::is_whitespace)
}

pub fn redact_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or(authority);
        return format!("{scheme}://{host}");
    }
    "url".to_string()
}

pub fn redact_raw(tool: &str, target: Option<&str>) -> String {
    let Some(t) = target else {
        return tool.to_string();
    };
    if is_bare_url(t.trim()) {
        return format!("{tool} {}", redact_url(t.trim()));
    }
    let first = t.split_whitespace().next().unwrap_or("");
    let name = std::path::Path::new(first)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first);
    format!("{tool} {name} ({}b)", t.len())
}

pub fn tool_summary(payload: &Value) -> (String, Option<String>, String) {
    let tool = payload
        .get("tool_name")
        .and_then(|x| x.as_str())
        .unwrap_or("tool")
        .to_string();
    let input = payload.get("tool_input");
    let target = input.and_then(|i| {
        i.get("command")
            .and_then(|x| x.as_str())
            .or_else(|| i.get("file_path").and_then(|x| x.as_str()))
            .map(|s| s.to_string())
    });
    let cwd = payload
        .get("cwd")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    (tool, target, cwd)
}

const DEFAULT_DANGER: &[&str] = &[
    "rm -rf",
    "rm -r ",
    "rm -fr",
    "rmdir",
    "drop table",
    "drop database",
    "truncate ",
    "delete from",
    "git push --force",
    "git push -f",
    "kubectl delete",
    "terraform destroy",
    "mkfs",
    "dd if=",
    " > /dev/sd",
    "format ",
];

pub fn danger_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("danger.txt")
}

pub fn danger_patterns() -> Vec<String> {
    if let Ok(txt) = std::fs::read_to_string(danger_file()) {
        let v: Vec<String> = txt
            .lines()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        if !v.is_empty() {
            return v;
        }
    }
    DEFAULT_DANGER.iter().map(|s| s.to_string()).collect()
}

pub fn is_destructive(target: &Option<String>) -> bool {
    let Some(t) = target else { return false };
    let lower = t.to_lowercase();
    danger_patterns().iter().any(|d| lower.contains(d.as_str()))
}

#[cfg(test)]
mod redact_tests {
    use super::redact_raw;

    #[test]
    fn command_with_embedded_url_does_not_leak() {
        let cmd = "timeout 380 ssh -i ~/.ssh/sent -o StrictHostKeyChecking=no vmashnin@10.10.22.232 'G=http://10.10.22.232:31080; curl $G'";
        let out = redact_raw("Bash", Some(cmd));
        for secret in ["ssh", "sent", "vmashnin", "http", "curl", "StrictHost"] {
            assert!(!out.contains(secret), "leaked {secret:?} in {out:?}");
        }
        assert!(out.starts_with("Bash timeout ("), "got {out:?}");
    }

    #[test]
    fn bare_url_keeps_only_scheme_host() {
        let out = redact_raw(
            "WebFetch",
            Some("https://admin:S3cr3t@internal.example.com/api?token=abc"),
        );
        assert_eq!(out, "WebFetch https://internal.example.com");
    }

    #[test]
    fn file_path_is_basename_only() {
        let out = redact_raw("Edit", Some("/Users/itodev/.ssh/id_rsa"));
        assert!(
            !out.contains("/Users") && !out.contains(".ssh"),
            "got {out:?}"
        );
    }

    #[test]
    fn none_target_is_tool_only() {
        assert_eq!(redact_raw("Workflow", None), "Workflow");
    }
}
