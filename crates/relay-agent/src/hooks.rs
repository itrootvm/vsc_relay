use anyhow::Result;
use relay_ipc::{BlockingConn, Endpoint};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static COUNTER: AtomicU64 = AtomicU64::new(1);
static HELD_TOOL_USES: OnceLock<Mutex<std::collections::HashMap<String, i64>>> = OnceLock::new();

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn mark_gate_hold(payload: &Value) {
    let Some(tool_use_id) = payload
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let mut held = HELD_TOOL_USES
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = epoch_secs();
    held.retain(|_, at| now - *at <= 130);
    held.insert(tool_use_id.to_string(), now);
}

pub fn gate_holds_tool_use(tool_use_id: &str) -> bool {
    if tool_use_id.is_empty() {
        return false;
    }
    let mut held = HELD_TOOL_USES
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = epoch_secs();
    held.retain(|_, at| now - *at <= 130);
    held.contains_key(tool_use_id)
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

    if event == "session-start" {
        if let Some(output) =
            session_protocol_output(&crate::automation::AutomationConfig::load(), &payload)
        {
            println!("{output}");
        }
    }

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
                    emit_decision(&dec, &payload);
                }
            }
        }
    }
    Ok(())
}

fn context_was_rebuilt(payload: &Value) -> bool {
    match payload.get("source").and_then(Value::as_str) {
        Some(source) => !source.eq_ignore_ascii_case("resume"),
        None => true,
    }
}

fn session_protocol_output(
    config: &crate::automation::AutomationConfig,
    payload: &Value,
) -> Option<Value> {
    (config.smart.enabled && config.smart.feedback_protocol && context_was_rebuilt(payload)).then(
        || {
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": relay_compass::session_health_protocol_context(),
                }
            })
        },
    )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookDialect {
    Claude,
    Codex,
}

fn hook_dialect(payload: &Value) -> HookDialect {
    if payload.get("turn_id").is_some()
        || payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.contains("/.codex/"))
    {
        HookDialect::Codex
    } else {
        HookDialect::Claude
    }
}

fn decision_output(dec: &Value, payload: &Value) -> Option<Value> {
    let decision = dec
        .get("decision")
        .and_then(|d| d.as_str())
        .unwrap_or("ask");
    let reason = dec.get("reason").and_then(|r| r.as_str()).unwrap_or("");
    match (hook_dialect(payload), decision) {
        (HookDialect::Codex, "deny" | "block") => Some(json!({
            "decision": "block",
            "reason": reason,
        })),
        (HookDialect::Codex, "allow") => Some(json!({
            "decision": "allow",
            "reason": reason,
        })),
        (HookDialect::Codex, "ask_user") => {
            let honest = if reason.is_empty() {
                "session-health gate blocked this change: read-only proof was not obtained and this agent cannot prompt the operator".to_string()
            } else {
                format!(
                    "{reason}; blocked automatically because this agent cannot prompt the operator"
                )
            };
            Some(json!({
                "decision": "block",
                "reason": honest,
            }))
        }
        (HookDialect::Claude, "allow" | "deny") => Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": reason,
            }
        })),
        (HookDialect::Claude, "ask_user") => Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "ask",
                "permissionDecisionReason": reason,
            }
        })),
        _ => None,
    }
}

fn emit_decision(dec: &Value, payload: &Value) {
    if let Some(out) = decision_output(dec, payload) {
        println!("{out}");
    }
}

pub fn next_request_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("r{n}")
}

pub fn native_identity_digest(payload: &Value) -> String {
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let turn = payload.get("turn_id").and_then(Value::as_str).unwrap_or("");
    let tool_use = payload
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let fallback = if tool_use.is_empty() {
        serde_json::to_string(payload.get("tool_input").unwrap_or(&Value::Null)).unwrap_or_default()
    } else {
        String::new()
    };
    blake3::hash(format!("{session}\0{turn}\0{tool_use}\0{fallback}").as_bytes())
        .to_hex()
        .as_str()[..32]
        .to_string()
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
            .or_else(|| i.get("path").and_then(|x| x.as_str()))
            .or_else(|| i.get("target").and_then(|x| x.as_str()))
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
    "dd of=",
    " > /dev/sd",
    ">/dev/sd",
    " > /dev/nvme",
    ">/dev/nvme",
    " > /dev/vd",
    ">/dev/vd",
    " > /dev/xvd",
    ">/dev/xvd",
    "tee /dev/sd",
    "tee /dev/nvme",
    "tee /dev/vd",
    "tee /dev/xvd",
    "wipefs",
    "shred ",
    "sgdisk",
    "lvremove",
    "vgremove",
    "pvremove",
    "userdel",
    "groupdel",
    "apt purge",
    "apt-get purge",
    "dnf remove",
    "yum remove",
    "zypper remove",
    "pacman -r",
    "apk del",
    "snap remove",
    "flatpak uninstall",
    "docker system prune",
    "docker volume rm",
    "docker rm -f",
    "nft flush ruleset",
    ":(){",
    "del /f",
    "del /q",
    "rd /s",
    "diskpart",
    "format c:",
    "cipher /w",
    "remove-item -recurse",
    "rm -recurse",
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
    matched_danger(target).is_some()
}

pub fn matched_danger(target: &Option<String>) -> Option<String> {
    let t = target.as_ref()?;
    let lower = t.to_lowercase();
    danger_patterns()
        .into_iter()
        .find(|pattern| lower.contains(pattern.as_str()))
}

#[cfg(test)]
mod danger_tests {
    use super::{is_destructive, matched_danger};

    fn verdict(command: &str) -> Option<String> {
        matched_danger(&Some(command.to_string()))
    }

    #[test]
    fn a_psql_display_setting_is_not_a_disk_format() {
        for harmless in [
            "psql -c '\\pset format unaligned' -f query.sql",
            "printf '\\pset format unaligned\\n' | psql -d db",
            "git log --format '%h %s' | head",
            "kubectl get pods -o custom-columns=NAME:.metadata.name --format wide",
        ] {
            assert_eq!(
                verdict(harmless),
                None,
                "held for approval with nothing destructive in it: {harmless}"
            );
        }
    }

    #[test]
    fn genuine_destruction_is_still_caught() {
        for destructive in [
            "rm -rf /tmp/build",
            "psql -c 'DROP DATABASE reports_db'",
            "kubectl delete pod postgres-0",
            "diskpart",
            "format c:",
        ] {
            assert!(
                is_destructive(&Some(destructive.to_string())),
                "must stay dangerous: {destructive}"
            );
        }
    }

    #[test]
    fn linux_disk_and_package_destruction_is_caught() {
        for destructive in [
            "cat /dev/urandom >/dev/sda",
            "dd if=/dev/zero of=/dev/nvme0n1 bs=1M",
            "cat image.img | sudo tee /dev/nvme0n1",
            "wipefs -a /dev/vda",
            "shred -n 3 -z /dev/xvdb",
            "sudo lvremove -f vg0/data",
            "sudo userdel -r deploy",
            "sudo apt purge --autoremove postgresql",
            "sudo pacman -Rns base-devel",
            "docker system prune -af --volumes",
            "sudo nft flush ruleset",
        ] {
            assert!(
                is_destructive(&Some(destructive.to_string())),
                "must be dangerous on Linux: {destructive}"
            );
        }
    }

    #[test]
    fn everyday_linux_commands_are_not_destruction() {
        for harmless in [
            "cargo build --release > /dev/null 2>&1",
            "ls -l /dev/sda",
            "lsblk -o NAME,SIZE /dev/nvme0n1",
            "systemctl --user status vsc-relay",
            "apt list --installed | grep xdotool",
            "docker ps -a",
            "dd --help",
        ] {
            assert_eq!(
                verdict(harmless),
                None,
                "held for approval with nothing destructive in it: {harmless}"
            );
        }
    }
}

#[cfg(test)]
mod redact_tests {
    use super::{
        decision_output, gate_holds_tool_use, mark_gate_hold, native_identity_digest, redact_raw,
        session_protocol_output,
    };
    use crate::automation::AutomationConfig;

    #[test]
    fn command_with_embedded_url_does_not_leak() {
        let cmd = "timeout 380 ssh -i ~/.ssh/deploy_key -o StrictHostKeyChecking=no builder@203.0.113.40 'G=http://203.0.113.40:31080; curl $G'";
        let out = redact_raw("Bash", Some(cmd));
        for secret in ["ssh", "deploy_key", "builder", "http", "curl", "StrictHost"] {
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
        let out = redact_raw("Edit", Some("/Users/dev/.ssh/id_rsa"));
        assert!(
            !out.contains("/Users") && !out.contains(".ssh"),
            "got {out:?}"
        );
    }

    #[test]
    fn none_target_is_tool_only() {
        assert_eq!(redact_raw("Workflow", None), "Workflow");
    }

    #[test]
    fn session_protocol_is_inert_until_smart_is_enabled() {
        let config = AutomationConfig::default();
        assert!(session_protocol_output(&config, &serde_json::json!({})).is_none());

        let mut disabled = config.clone();
        disabled.smart.enabled = true;
        disabled.smart.feedback_protocol = false;
        assert!(session_protocol_output(&disabled, &serde_json::json!({})).is_none());
    }

    #[test]
    fn session_protocol_is_sent_once_per_rebuilt_context_and_not_on_resume() {
        let mut config = AutomationConfig::default();
        config.smart.enabled = true;
        let resumed = serde_json::json!({ "source": "resume" });
        assert!(session_protocol_output(&config, &resumed).is_none());
        for source in ["startup", "clear", "compact"] {
            let payload = serde_json::json!({ "source": source });
            assert!(
                session_protocol_output(&config, &payload).is_some(),
                "expected the protocol on {source}"
            );
        }
    }

    #[test]
    fn session_protocol_uses_cross_agent_additional_context_shape() {
        let mut config = AutomationConfig::default();
        config.smart.enabled = true;
        let output = session_protocol_output(&config, &serde_json::json!({})).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        let context = output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains("current-contract"));
        assert!(!context.contains("session_id"));
    }

    #[test]
    fn deny_uses_each_provider_native_shape() {
        let decision = serde_json::json!({"decision":"deny", "reason":"gate"});
        let claude = decision_output(&decision, &serde_json::json!({"session_id":"s"})).unwrap();
        assert_eq!(claude["hookSpecificOutput"]["permissionDecision"], "deny");
        let codex = decision_output(
            &decision,
            &serde_json::json!({"session_id":"s", "turn_id":"t", "tool_use_id":"u"}),
        )
        .unwrap();
        assert_eq!(codex["decision"], "block");
    }

    #[test]
    fn codex_ask_user_blocks_with_an_honest_reason() {
        let decision =
            serde_json::json!({"decision":"ask_user", "reason":"return this decision to the user"});
        let codex = decision_output(
            &decision,
            &serde_json::json!({"session_id":"s", "turn_id":"t", "tool_use_id":"u"}),
        )
        .unwrap();
        assert_eq!(codex["decision"], "block");
        let reason = codex["reason"].as_str().unwrap();
        assert!(reason.contains("cannot prompt the operator"));
    }

    #[test]
    fn hook_identity_prefers_native_ids_and_payload_fallback() {
        let first = serde_json::json!({
            "session_id":"s", "turn_id":"t", "tool_use_id":"u",
            "tool_name":"old", "tool_input":{"opaque":1}
        });
        let renamed = serde_json::json!({
            "session_id":"s", "turn_id":"t", "tool_use_id":"u",
            "tool_name":"renamed", "tool_input":{"opaque":2}
        });
        assert_eq!(
            native_identity_digest(&first),
            native_identity_digest(&renamed)
        );

        let fallback_a = serde_json::json!({"session_id":"s", "turn_id":"t", "tool_input":{"x":1}});
        let fallback_b = serde_json::json!({"session_id":"s", "turn_id":"t", "tool_input":{"x":2}});
        assert_ne!(
            native_identity_digest(&fallback_a),
            native_identity_digest(&fallback_b)
        );
    }

    #[test]
    fn gate_hold_deduplicates_the_native_permission_path() {
        let id = format!("gate-dedup-{}", std::process::id());
        assert!(!gate_holds_tool_use(&id));
        mark_gate_hold(&serde_json::json!({"tool_use_id":id}));
        assert!(gate_holds_tool_use(&id));
    }
}
