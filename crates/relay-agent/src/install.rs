use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

pub fn is_install_command(arg: &str) -> bool {
    arg == "install-hooks"
}

pub fn run() -> Result<()> {
    let exe = std::env::current_exe()
        .context("current exe")?
        .to_string_lossy()
        .to_string();
    let home = dirs::home_dir().context("no home dir")?;
    let settings = home.join(".claude").join("settings.json");

    let mut root: Value = if settings.exists() {
        let text = std::fs::read_to_string(&settings)?;
        std::fs::write(settings.with_extension("json.bak"), &text).ok();
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    } else {
        if let Some(dir) = settings.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        json!({})
    };

    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();

    for (event, flag, matcher) in [
        ("SessionStart", "session-start", None),
        ("Stop", "stop", None),
        ("Notification", "notification", None),
        (
            "PreToolUse",
            "pre-tool-use",
            Some("Bash|Write|Edit|MultiEdit|NotebookEdit"),
        ),
    ] {
        let cmd = format!("{exe} hook {flag}");
        upsert(hooks, event, &cmd, matcher);
    }

    let pretty = serde_json::to_string_pretty(&root)?;
    std::fs::write(&settings, pretty)?;
    println!("installed relay hooks into {}", settings.display());
    println!("restart Claude Code sessions to pick them up");
    Ok(())
}

fn upsert(hooks: &mut Map<String, Value>, event: &str, cmd: &str, matcher: Option<&str>) {
    let entry = hooks.entry(event).or_insert_with(|| json!([]));
    if !entry.is_array() {
        *entry = json!([]);
    }
    let arr = entry.as_array_mut().unwrap();

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
