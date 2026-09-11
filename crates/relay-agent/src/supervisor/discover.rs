use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Ollama,
    OpenRouter,
    ClaudeCli,
    CodexCli,
    GeminiCli,
    CursorCli,
    Antigravity,
}

impl Backend {
    pub fn id(&self) -> &'static str {
        match self {
            Backend::Ollama => "ollama",
            Backend::OpenRouter => "openrouter",
            Backend::ClaudeCli => "claude-cli",
            Backend::CodexCli => "codex-cli",
            Backend::GeminiCli => "gemini-cli",
            Backend::CursorCli => "cursor-cli",
            Backend::Antigravity => "antigravity",
        }
    }

    pub fn all() -> [Backend; 7] {
        [
            Backend::Ollama,
            Backend::OpenRouter,
            Backend::ClaudeCli,
            Backend::CodexCli,
            Backend::GeminiCli,
            Backend::CursorCli,
            Backend::Antigravity,
        ]
    }

    pub fn parse(s: &str) -> Option<Backend> {
        Backend::all().into_iter().find(|b| b.id() == s.trim())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Discovered {
    pub id: String,
    pub available: bool,
    pub reason: String,
    pub local: bool,
    pub needs_key: bool,
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

fn find_in_paths(path_var: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn which_bin(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    find_in_paths(&path, name)
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

fn newest_ext_dir(prefix: &str) -> Option<PathBuf> {
    let root = home_dir().join(".vscode").join("extensions");
    let mut best: Option<(String, PathBuf)> = None;
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(prefix) {
            match &best {
                Some((bn, _)) if bn.as_str() >= name.as_str() => {}
                _ => best = Some((name, e.path())),
            }
        }
    }
    best.map(|(_, p)| p)
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn claude_cli_path() -> Option<PathBuf> {
    if let Some(p) = which_bin("claude") {
        return Some(p);
    }
    let nb = newest_ext_dir("anthropic.claude-code-")?
        .join("resources")
        .join("native-binary");
    for name in [exe("claude.real"), exe("claude")] {
        let c = nb.join(&name);
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

fn codex_cli_path() -> Option<PathBuf> {
    if let Some(p) = which_bin("codex") {
        return Some(p);
    }
    let bin = newest_ext_dir("openai.chatgpt-")?.join("bin");
    for e in std::fs::read_dir(&bin).ok()?.flatten() {
        let c = e.path().join(exe("codex"));
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

fn probe_cli(id: &str, path: Option<PathBuf>) -> Discovered {
    let reason = match &path {
        Some(p) => format!(
            "installed ({})",
            p.file_name().and_then(|n| n.to_str()).unwrap_or("cli")
        ),
        None => "not installed".to_string(),
    };
    Discovered {
        id: id.to_string(),
        available: path.is_some(),
        reason,
        local: false,
        needs_key: false,
        models: Vec::new(),
        path: path.map(|p| p.to_string_lossy().into_owned()),
    }
}

fn probe_openrouter() -> Discovered {
    let has = super::keys::key_for("openrouter").is_some();
    Discovered {
        id: Backend::OpenRouter.id().to_string(),
        available: has,
        reason: if has {
            "OpenRouter key configured".to_string()
        } else {
            "no OpenRouter key (set OPENROUTER_API_KEY or automation provider-key)".to_string()
        },
        local: false,
        needs_key: true,
        models: Vec::new(),
        path: None,
    }
}

fn normalize_host(raw: &str) -> String {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return "http://localhost:11434".to_string();
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    }
}

pub fn ollama_host() -> String {
    let raw = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://localhost:11434".to_string());
    normalize_host(&raw)
}

fn parse_ollama_models(body: &serde_json::Value) -> Vec<String> {
    body.get("models")
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

async fn probe_ollama(client: &reqwest::Client) -> Discovered {
    let host = ollama_host();
    let url = format!("{host}/api/tags");
    let unavailable = |reason: String| Discovered {
        id: Backend::Ollama.id().to_string(),
        available: false,
        reason,
        local: true,
        needs_key: false,
        models: Vec::new(),
        path: None,
    };
    match client
        .get(&url)
        .timeout(Duration::from_millis(1200))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let models = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .map(|v| parse_ollama_models(&v))
                .unwrap_or_default();
            Discovered {
                id: Backend::Ollama.id().to_string(),
                available: true,
                reason: format!("reachable at {host}"),
                local: true,
                needs_key: false,
                models,
                path: None,
            }
        }
        Ok(resp) => unavailable(format!("{host} returned {}", resp.status().as_u16())),
        Err(_) => unavailable(format!("not reachable at {host}")),
    }
}

pub async fn discover_all() -> Vec<Discovered> {
    let client = reqwest::Client::new();
    let ollama = probe_ollama(&client).await;
    vec![
        ollama,
        probe_openrouter(),
        probe_cli(Backend::ClaudeCli.id(), claude_cli_path()),
        probe_cli(Backend::CodexCli.id(), codex_cli_path()),
        probe_cli(Backend::GeminiCli.id(), which_bin("gemini")),
        probe_cli(Backend::CursorCli.id(), which_bin("cursor-agent")),
        probe_cli(Backend::Antigravity.id(), which_bin("agy")),
    ]
}

pub fn is_auth_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "not logged in",
        "log in",
        "please login",
        "login",
        "sign in",
        "unauthorized",
        "authentication",
        "auth method",
        "auth required",
        "set an auth",
        "api key",
        "api_key",
        "credentials",
        "401",
        "403",
        "no auth",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

pub fn suggested_models(id: &str) -> &'static [&'static str] {
    match id {
        "claude-cli" => &["sonnet", "opus", "haiku"],
        "codex-cli" => &["o3", "gpt-5-codex"],
        "gemini-cli" => &["gemini-2.5-pro", "gemini-2.5-flash"],
        "cursor-cli" => &["gpt-5", "sonnet-4-thinking"],
        _ => &[],
    }
}

pub async fn print_models(id: &str) -> anyhow::Result<()> {
    let b = Backend::parse(id).ok_or_else(|| anyhow::anyhow!("unknown backend '{id}'"))?;
    match b {
        Backend::Ollama => {
            let client = reqwest::Client::new();
            let d = probe_ollama(&client).await;
            if d.models.is_empty() {
                println!("(ollama not reachable or no models pulled)");
            } else {
                for m in &d.models {
                    println!("{m}");
                }
            }
        }
        Backend::OpenRouter => {
            println!("set any slug with: automation provider-model openrouter <slug>");
            println!("browse: https://openrouter.ai/models");
        }
        Backend::CursorCli => {
            for m in suggested_models(id) {
                println!("{m}");
            }
            println!("(live list: cursor-agent models)");
        }
        Backend::Antigravity => {
            println!("(live list: agy models)");
        }
        _ => {
            for m in suggested_models(id) {
                println!("{m}");
            }
        }
    }
    Ok(())
}

pub async fn print_discovery(json: bool, only: Option<&str>) -> anyhow::Result<()> {
    let mut found = discover_all().await;
    if let Some(id) = only {
        let b = Backend::parse(id)
            .ok_or_else(|| anyhow::anyhow!("unknown backend '{id}' (try: ollama, openrouter, claude-cli, codex-cli, gemini-cli, cursor-cli, antigravity)"))?;
        found.retain(|d| d.id == b.id());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&found)?);
    } else {
        for d in &found {
            let mark = if d.available { "✓" } else { "·" };
            let models = if d.models.is_empty() {
                String::new()
            } else {
                format!(" [{}]", d.models.join(", "))
            };
            println!("{mark} {:<12} {}{}", d.id, d.reason, models);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_errors_detected() {
        assert!(is_auth_error(
            "cursor-agent exited Some(1): Error: Authentication required. Run 'agent login'"
        ));
        assert!(is_auth_error(
            "codex exited Some(1): Please login with codex login"
        ));
        assert!(is_auth_error("gemini: 401 Unauthorized"));
        assert!(is_auth_error(
            "gemini exited Some(1): Please set an Auth method OR specify GEMINI_API_KEY"
        ));
        assert!(!is_auth_error("claude exited Some(1): stream disconnected"));
        assert!(!is_auth_error("connection refused"));
    }

    #[test]
    fn backend_id_roundtrip() {
        for b in Backend::all() {
            assert_eq!(Backend::parse(b.id()), Some(b));
        }
        assert_eq!(Backend::parse("nope"), None);
        assert_eq!(Backend::parse(" ollama "), Some(Backend::Ollama));
    }

    #[test]
    fn probe_cli_present_is_available() {
        let present = probe_cli("claude-cli", Some(PathBuf::from("/usr/bin/claude")));
        assert!(present.available);
        assert_eq!(present.path.as_deref(), Some("/usr/bin/claude"));
        assert!(present.reason.contains("claude"));
        let absent = probe_cli("codex-cli", None);
        assert!(!absent.available);
        assert!(absent.reason.contains("not installed"));
    }

    #[test]
    fn find_in_paths_locates_file() {
        let dir = std::env::temp_dir().join(format!("vre_which_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("fakebin"), b"x").unwrap();
        let path_var = std::env::join_paths([&dir]).unwrap();
        assert!(find_in_paths(&path_var, "fakebin").is_some());
        assert!(find_in_paths(&path_var, "does-not-exist-xyz").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_ollama_models() {
        let v = serde_json::json!({"models":[{"name":"llama3.2"},{"name":"qwen2.5-coder"}]});
        assert_eq!(parse_ollama_models(&v), vec!["llama3.2", "qwen2.5-coder"]);
        assert!(parse_ollama_models(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn normalize_host_cases() {
        assert_eq!(normalize_host(""), "http://localhost:11434");
        assert_eq!(normalize_host("localhost:11434"), "http://localhost:11434");
        assert_eq!(normalize_host("http://box:1234/"), "http://box:1234");
        assert_eq!(normalize_host("https://x/"), "https://x");
    }
}
