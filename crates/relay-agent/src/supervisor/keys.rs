use std::collections::BTreeMap;
use std::path::PathBuf;

fn keys_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("provider-keys.json")
}

pub fn load() -> BTreeMap<String, String> {
    std::fs::read_to_string(keys_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(map: &BTreeMap<String, String>) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(map).unwrap_or_else(|_| "{}".to_string());
    crate::fsutil::secure_write(&keys_path(), text.as_bytes())
}

pub fn set(id: &str, key: &str) -> std::io::Result<()> {
    let mut m = load();
    m.insert(id.to_string(), key.to_string());
    save(&m)
}

pub fn clear(id: &str) -> std::io::Result<()> {
    let mut m = load();
    m.remove(id);
    save(&m)
}

pub fn env_var(id: &str) -> Option<&'static str> {
    match id {
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "claude-cli" => Some("ANTHROPIC_API_KEY"),
        "codex-cli" => Some("OPENAI_API_KEY"),
        "gemini-cli" => Some("GEMINI_API_KEY"),
        "cursor-cli" => Some("CURSOR_API_KEY"),
        "antigravity" => Some("GEMINI_API_KEY"),
        _ => None,
    }
}

pub fn key_for(id: &str) -> Option<String> {
    if let Some(k) = load().get(id) {
        if !k.trim().is_empty() {
            return Some(k.clone());
        }
    }
    env_var(id)
        .and_then(|v| std::env::var(v).ok())
        .filter(|s| !s.trim().is_empty())
}

pub fn cli_env(id: &str) -> Vec<(String, String)> {
    match (env_var(id), stored_key(id)) {
        (Some(var), Some(key)) => vec![(var.to_string(), key)],
        _ => Vec::new(),
    }
}

fn stored_key(id: &str) -> Option<String> {
    load()
        .get(id)
        .filter(|k| !k.trim().is_empty())
        .map(String::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_mapping() {
        assert_eq!(env_var("cursor-cli"), Some("CURSOR_API_KEY"));
        assert_eq!(env_var("claude-cli"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(env_var("ollama"), None);
    }
}
