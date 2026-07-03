use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn socket_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(".vsc-relay").join("hook.sock")
}

pub fn is_hook_command(arg: &str) -> bool {
    arg == "hook"
}

pub fn run_hook(args: &[String]) -> Result<()> {
    let event = args.get(1).cloned().unwrap_or_default();
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let request = json!({ "event": event, "payload": payload });

    let stream = match UnixStream::connect(socket_path()) {
        Ok(s) => s,
        Err(_) => {
            return Ok(());
        }
    };
    let mut writer = stream.try_clone().context("clone hook socket")?;
    writeln!(writer, "{request}").ok();
    writer.flush().ok();

    if event == "pre-tool-use" {
        stream.set_read_timeout(Some(Duration::from_secs(125))).ok();
        let mut resp = String::new();
        if BufReader::new(stream).read_line(&mut resp).is_ok() && !resp.trim().is_empty() {
            if let Ok(dec) = serde_json::from_str::<Value>(&resp) {
                emit_decision(&dec);
            }
        }
    }
    Ok(())
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
