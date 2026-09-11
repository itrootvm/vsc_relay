use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::path::Path;

const CLAUDE_MUTATION_MATCHER: &str = "Bash|Write|Edit|MultiEdit|NotebookEdit";

pub fn is_install_command(arg: &str) -> bool {
    arg == "install-hooks"
}

pub fn run() -> Result<()> {
    let exe = quote_exe(
        &std::env::current_exe()
            .context("current exe")?
            .to_string_lossy(),
    );
    let home = dirs::home_dir().context("no home dir")?;
    let settings = home.join(".claude").join("settings.json");
    let mut root = load_json_with_backup(&settings)?;
    let hooks = hooks_map(&mut root);

    for (event, flag, matcher) in [
        ("SessionStart", "session-start", None),
        ("Stop", "stop", None),
        ("Notification", "notification", None),
        ("PreToolUse", "pre-tool-use", Some(CLAUDE_MUTATION_MATCHER)),
        (
            "PostToolUse",
            "post-tool-use",
            Some(CLAUDE_MUTATION_MATCHER),
        ),
    ] {
        let cmd = format!("{exe} hook {flag}");
        upsert(hooks, event, &cmd, matcher);
    }

    save_json(&settings, &root)?;
    println!("installed relay hooks into {}", settings.display());

    install_session_start_hooks()?;
    install_gate_hooks()?;
    println!("restart Claude Code sessions; in Codex open /hooks and review/trust the new hook");
    Ok(())
}

pub fn install_session_start_hooks() -> Result<()> {
    let exe = quote_exe(
        &std::env::current_exe()
            .context("current exe")?
            .to_string_lossy(),
    );
    let home = dirs::home_dir().context("no home dir")?;

    let claude_settings = home.join(".claude").join("settings.json");
    let mut claude_root = load_json_with_backup(&claude_settings)?;
    let codex_hooks_path = home.join(".codex").join("hooks.json");
    let mut codex_root = load_json_with_backup(&codex_hooks_path)?;
    upsert(
        hooks_map(&mut claude_root),
        "SessionStart",
        &format!("{exe} hook session-start"),
        None,
    );

    upsert(
        hooks_map(&mut codex_root),
        "SessionStart",
        &format!("{exe} hook session-start"),
        Some("startup|resume|clear|compact"),
    );
    save_json(&claude_settings, &claude_root)?;
    save_json(&codex_hooks_path, &codex_root)?;
    println!(
        "installed relay SessionStart hooks into {} and {}",
        claude_settings.display(),
        codex_hooks_path.display(),
    );
    Ok(())
}

pub fn install_gate_hooks() -> Result<()> {
    let exe = quote_exe(
        &std::env::current_exe()
            .context("current exe")?
            .to_string_lossy(),
    );
    let home = dirs::home_dir().context("no home dir")?;
    for (path, matcher) in [
        (
            home.join(".claude").join("settings.json"),
            Some(CLAUDE_MUTATION_MATCHER),
        ),
        (home.join(".codex").join("hooks.json"), None),
    ] {
        let mut root = load_json_with_backup(&path)?;
        let hooks = hooks_map(&mut root);
        upsert(
            hooks,
            "PreToolUse",
            &format!("{exe} hook pre-tool-use"),
            matcher,
        );
        upsert(
            hooks,
            "PostToolUse",
            &format!("{exe} hook post-tool-use"),
            matcher,
        );
        save_json(&path, &root)?;
    }
    println!("installed provider-parity PreToolUse/PostToolUse hooks");
    Ok(())
}

fn load_json_with_backup(path: &Path) -> Result<Value> {
    let root = if path.exists() {
        let text = std::fs::read_to_string(path)?;
        crate::fsutil::secure_write(&path.with_extension("json.bak"), text.as_bytes()).ok();
        serde_json::from_str(&text).with_context(|| {
            format!("invalid JSON in {}; refusing to replace it", path.display())
        })?
    } else {
        json!({})
    };
    if !root.is_object() {
        bail!(
            "{} must contain a JSON object; refusing to replace it",
            path.display()
        );
    }
    Ok(root)
}

fn hooks_map(root: &mut Value) -> &mut Map<String, Value> {
    let obj = root.as_object_mut().expect("root normalized to an object");
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    hooks
        .as_object_mut()
        .expect("hooks normalized to an object")
}

fn save_json(path: &Path, root: &Value) -> Result<()> {
    let pretty = serde_json::to_string_pretty(root)?;
    crate::fsutil::secure_write(path, pretty.as_bytes())?;
    Ok(())
}

#[cfg(windows)]
fn quote_exe(exe: &str) -> String {
    format!("\"{exe}\"")
}

#[cfg(not(windows))]
fn quote_exe(exe: &str) -> String {
    exe.to_string()
}

fn upsert(hooks: &mut Map<String, Value>, event: &str, cmd: &str, matcher: Option<&str>) {
    let entry = hooks.entry(event).or_insert_with(|| json!([]));
    if !entry.is_array() {
        *entry = json!([]);
    }
    let arr = entry.as_array_mut().unwrap();

    let flag = cmd.split_whitespace().last().unwrap_or_default();
    let relay_suffix = format!("vsc-relay-agent hook {flag}");
    for group in arr.iter_mut() {
        if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            handlers.retain(|handler| {
                let Some(command) = handler.get("command").and_then(Value::as_str) else {
                    return true;
                };
                command == cmd
                    || !command
                        .trim_end_matches(['\'', '"'])
                        .ends_with(&relay_suffix)
            });
        }
    }
    arr.retain(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|handlers| !handlers.is_empty())
    });

    let already = arr.iter().any(|group| {
        group
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hs| {
                hs.iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(cmd))
            })
            .unwrap_or(false)
    });
    if already {
        return;
    }

    let mut inner = json!({ "type": "command", "command": cmd });
    if event == "PreToolUse" {
        inner["timeout"] = json!(120);
    }
    let mut group = Map::new();
    if let Some(m) = matcher {
        group.insert("matcher".to_string(), json!(m));
    }
    group.insert("hooks".to_string(), json!([inner]));
    arr.push(Value::Object(group));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretooluse_matcher_scopes_to_mutation_tools_only() {
        let mut root = json!({});
        let hooks = hooks_map(&mut root);
        upsert(
            hooks,
            "PreToolUse",
            "/bin/vsc-relay-agent hook pre-tool-use",
            Some(CLAUDE_MUTATION_MATCHER),
        );
        let matcher = root["hooks"]["PreToolUse"][0]["matcher"].as_str().unwrap();
        assert_eq!(matcher, CLAUDE_MUTATION_MATCHER);
        for read_only in ["Read", "Grep", "Glob", "WebFetch", "Task"] {
            assert!(
                !matcher.contains(read_only),
                "{read_only} must not be intercepted when the smart layer is off"
            );
        }
        for mutation in ["Bash", "Write", "Edit"] {
            assert!(matcher.contains(mutation));
        }
    }

    #[test]
    fn upsert_preserves_unrelated_hooks_and_is_idempotent() {
        let mut root = json!({
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "keep-me"}]}]
            },
            "unrelated": {"keep": true}
        });
        let hooks = hooks_map(&mut root);
        upsert(
            hooks,
            "SessionStart",
            "/bin/relay hook session-start",
            Some("startup|resume|clear|compact"),
        );
        upsert(
            hooks,
            "SessionStart",
            "/bin/relay hook session-start",
            Some("startup|resume|clear|compact"),
        );

        assert_eq!(root["unrelated"]["keep"], true);
        assert_eq!(root["hooks"]["Stop"][0]["hooks"][0]["command"], "keep-me");
        let session = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session.len(), 1);
        assert_eq!(session[0]["matcher"], "startup|resume|clear|compact");
    }

    #[test]
    fn malformed_hook_container_is_repaired_without_touching_other_keys() {
        let mut root = json!({"hooks": "bad", "keep": 7});
        upsert(
            hooks_map(&mut root),
            "SessionStart",
            "relay hook session-start",
            None,
        );
        assert_eq!(root["keep"], 7);
        assert!(root["hooks"]["SessionStart"].is_array());
    }

    #[test]
    fn provider_parity_hooks_have_no_tool_name_matcher() {
        let mut claude = json!({});
        let mut codex = json!({});
        for root in [&mut claude, &mut codex] {
            let hooks = hooks_map(root);
            upsert(hooks, "PreToolUse", "relay hook pre-tool-use", None);
            upsert(hooks, "PostToolUse", "relay hook post-tool-use", None);
            assert!(root["hooks"]["PreToolUse"][0].get("matcher").is_none());
            assert!(root["hooks"]["PostToolUse"][0].get("matcher").is_none());
            assert_eq!(root["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"], 120);
        }
    }

    #[test]
    fn upsert_migrates_stale_relay_binary_but_preserves_other_hooks() {
        let mut root = json!({
            "hooks": {
                "PreToolUse": [
                    {"matcher":"Write", "hooks":[
                        {"type":"command", "command":"/old/target/release/vsc-relay-agent hook pre-tool-use"},
                        {"type":"command", "command":"keep-third-party"}
                    ]}
                ]
            }
        });
        upsert(
            hooks_map(&mut root),
            "PreToolUse",
            "/stable/VSCRelay.app/vsc-relay-agent hook pre-tool-use",
            None,
        );
        let serialized = serde_json::to_string(&root).unwrap();
        assert!(!serialized.contains("/old/target/release"));
        assert!(serialized.contains("keep-third-party"));
        assert_eq!(
            serialized
                .matches("/stable/VSCRelay.app/vsc-relay-agent hook pre-tool-use")
                .count(),
            1
        );
    }

    #[test]
    fn invalid_json_is_never_replaced() {
        let dir = std::env::temp_dir().join(format!(
            "vsc-relay-hook-config-{}-{}",
            std::process::id(),
            crate::automation::now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hooks.json");
        std::fs::write(&path, b"{broken").unwrap();

        assert!(load_json_with_backup(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
        let _ = std::fs::remove_dir_all(dir);
    }
}
