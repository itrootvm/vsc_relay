use crate::config::{canonical_cli, SemanticConfig};
use crate::remote::{clean_json, facts_payload, prompt_text, validate};
use anyhow::{bail, Context, Result};
use relay_compass::{SemanticFacts, SemanticInputFrame};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

struct CliSpec {
    binary: &'static str,
    args: Vec<String>,
}

fn spec(cli: &str, prompt: &str, model: Option<&str>) -> Result<CliSpec> {
    let prompt = prompt.to_string();
    let model = model.map(str::trim).filter(|model| !model.is_empty());
    let (binary, args) = match cli {
        "claude" => {
            let mut args = vec![
                "-p".into(),
                prompt,
                "--output-format".into(),
                "text".into(),
                "--tools".into(),
                "".into(),
                "--safe-mode".into(),
                "--disable-slash-commands".into(),
                "--no-session-persistence".into(),
                "--permission-mode".into(),
                "plan".into(),
            ];
            if let Some(model) = model {
                args.push("--model".into());
                args.push(model.to_string());
            }
            ("claude", args)
        }
        "codex" => {
            let mut args = vec![
                "exec".into(),
                prompt,
                "--sandbox".into(),
                "read-only".into(),
                "--skip-git-repo-check".into(),
                "--ephemeral".into(),
                "--ignore-rules".into(),
                "-c".into(),
                "model_reasoning_effort=\"low\"".into(),
            ];
            if let Some(model) = model {
                args.push("-m".into());
                args.push(model.to_string());
            }
            ("codex", args)
        }
        "gemini" => {
            let mut args = vec!["-p".into(), prompt, "--sandbox".into()];
            if let Some(model) = model {
                args.push("--model".into());
                args.push(model.to_string());
            }
            ("gemini", args)
        }
        "cursor" => {
            let mut args = vec![
                "-p".into(),
                prompt,
                "--output-format".into(),
                "text".into(),
                "--mode".into(),
                "ask".into(),
                "--sandbox".into(),
                "enabled".into(),
                "--trust".into(),
            ];
            if let Some(model) = model {
                args.push("--model".into());
                args.push(model.to_string());
            }
            ("cursor-agent", args)
        }
        "antigravity" => {
            let mut args = vec!["--mode".into(), "plan".into(), "--sandbox".into()];
            if let Some(model) = model {
                args.push("--model".into());
                args.push(model.to_string());
            }
            args.push("-p".into());
            args.push(prompt);
            ("agy", args)
        }
        other => bail!("unknown semantic CLI '{other}'"),
    };
    Ok(CliSpec { binary, args })
}

fn resolve(binary: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let local = home.join(".local/bin").join(binary);
        if local.exists() {
            return local.to_string_lossy().into_owned();
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(binary);
            if candidate.exists() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    if let Some(home) = dirs::home_dir() {
        if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut candidates: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path().join("bin").join(binary))
                .filter(|path| path.exists())
                .collect();
            candidates.sort();
            if let Some(path) = candidates.pop() {
                return path.to_string_lossy().into_owned();
            }
        }
    }
    binary.to_string()
}

struct IsolatedWorkdir(PathBuf);

impl IsolatedWorkdir {
    fn create() -> Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("vsc-relay-semantic-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).context("create isolated semantic CLI workdir")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self(path))
    }
}

impl Drop for IsolatedWorkdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn extract_json(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut search_from = 0usize;
    while let Some(relative) = text[search_from..].find('{') {
        let start = search_from + relative;
        if let Some(end) = balanced_end(bytes, start) {
            return Some(&text[start..=end]);
        }
        search_from = start + 1;
    }
    None
}

fn balanced_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, &ch) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == b'\\' {
                escaped = true;
            } else if ch == b'"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) async fn invoke_json(config: &SemanticConfig, prompt: &str) -> Result<Value> {
    let cli = canonical_cli(&config.model)
        .context("semantic CLI is not configured (claude|codex|gemini|cursor|antigravity)")?;
    let pinned = config
        .cli_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let spec = spec(cli, prompt, pinned)?;
    let workdir = IsolatedWorkdir::create()?;
    let mut command = Command::new(resolve(spec.binary));
    command
        .args(&spec.args)
        .current_dir(&workdir.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .with_context(|| format!("spawn semantic CLI '{cli}'"))?;
    let output = tokio::time::timeout(
        Duration::from_secs(config.timeout_secs.clamp(10, 300)),
        child.wait_with_output(),
    )
    .await
    .with_context(|| format!("semantic CLI '{cli}' timed out"))?
    .with_context(|| format!("semantic CLI '{cli}'"))?;
    if !output.status.success() {
        bail!("semantic CLI '{cli}' exited unsuccessfully");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = extract_json(&stdout).context("semantic CLI returned no JSON object")?;

    serde_json::from_str(clean_json(json)).context("semantic CLI JSON")
}

async fn classify_batch(
    config: &SemanticConfig,
    frames: &[SemanticInputFrame],
) -> Result<Vec<SemanticFacts>> {
    let cli = canonical_cli(&config.model)
        .context("semantic CLI is not configured (claude|codex|gemini|cursor|antigravity)")?;
    let pinned = config
        .cli_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let value = invoke_json(config, &prompt_text(frames)).await?;
    let payload = facts_payload(value, frames).context("semantic CLI facts payload")?;
    let facts: Vec<SemanticFacts> =
        serde_json::from_value(payload).context("semantic CLI facts payload")?;

    let attribution = match pinned {
        Some(model) => format!("{cli}:{model}"),
        None => format!("{cli}:account-default(unpinned)"),
    };
    validate(
        facts,
        frames,
        "agent_cli",
        &attribution,
        config.min_confidence,
    )
}

pub async fn classify(
    config: &SemanticConfig,
    frames: &[SemanticInputFrame],
) -> Result<Vec<SemanticFacts>> {
    let mut facts = Vec::with_capacity(frames.len());
    for batch in frames.chunks(4) {
        match classify_batch(config, batch).await {
            Ok(batch_facts) => facts.extend(batch_facts),
            Err(_) if batch.len() > 1 => {
                for frame in batch {
                    facts.extend(classify_batch(config, std::slice::from_ref(frame)).await?);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_object_amid_log_noise() {
        let stdout = "starting claude...\nthinking\n{\"facts\":[{\"episode\":0}]}\ndone\n";
        assert_eq!(extract_json(stdout), Some("{\"facts\":[{\"episode\":0}]}"));
    }

    #[test]
    fn braces_inside_strings_do_not_miscount() {
        let stdout = "log {ignored\n{\"note\":\"a } brace \\\" inside\",\"n\":1}\ntail";
        let json = extract_json(stdout).unwrap();
        let value: Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["n"], 1);
        assert_eq!(value["note"], "a } brace \" inside");
    }

    #[test]
    fn missing_object_returns_none() {
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn spec_covers_every_canonical_cli() {
        for cli in ["claude", "codex", "gemini", "cursor", "antigravity"] {
            let spec = spec(cli, "PROMPT", None).unwrap();
            assert!(spec.args.iter().any(|arg| arg == "PROMPT"));
        }
        assert!(spec("bogus", "PROMPT", None).is_err());
    }

    #[test]
    fn pinned_model_uses_each_cli_flag_spelling() {
        let codex = spec("codex", "PROMPT", Some("gpt-5-mini")).unwrap();
        let position = codex.args.iter().position(|arg| arg == "-m").unwrap();
        assert_eq!(codex.args[position + 1], "gpt-5-mini");

        assert!(codex
            .args
            .iter()
            .any(|arg| arg == "model_reasoning_effort=\"low\""));

        for cli in ["claude", "gemini", "cursor", "antigravity"] {
            let spec = spec(cli, "PROMPT", Some("cheap-model")).unwrap();
            let position = spec.args.iter().position(|arg| arg == "--model").unwrap();
            assert_eq!(spec.args[position + 1], "cheap-model");
        }
    }

    #[test]
    fn antigravity_flags_precede_the_positional_prompt() {
        let spec = spec("antigravity", "PROMPT", Some("gemini-3-flash")).unwrap();
        let prompt_pos = spec.args.iter().position(|arg| arg == "PROMPT").unwrap();
        assert_eq!(prompt_pos, spec.args.len() - 1);
        for flag in ["--mode", "--sandbox", "--model"] {
            let flag_pos = spec.args.iter().position(|arg| arg == flag).unwrap();
            assert!(flag_pos < prompt_pos, "{flag} must precede the prompt");
        }
    }

    #[test]
    fn blank_model_is_treated_as_unpinned() {
        let spec = spec("claude", "PROMPT", Some("   ")).unwrap();
        assert!(!spec.args.iter().any(|arg| arg == "--model"));
    }

    #[test]
    fn specs_enforce_read_only_ephemeral_extractor_modes() {
        let claude = spec("claude", "PROMPT", None).unwrap();
        assert!(claude.args.windows(2).any(|pair| pair == ["--tools", ""]));
        assert!(claude.args.iter().any(|arg| arg == "--safe-mode"));
        assert!(claude
            .args
            .iter()
            .any(|arg| arg == "--no-session-persistence"));

        let codex = spec("codex", "PROMPT", None).unwrap();
        assert!(codex
            .args
            .windows(2)
            .any(|pair| pair == ["--sandbox", "read-only"]));
        assert!(codex.args.iter().any(|arg| arg == "--ephemeral"));
        assert!(codex.args.iter().any(|arg| arg == "--ignore-rules"));

        let cursor = spec("cursor", "PROMPT", None).unwrap();
        assert!(cursor.args.windows(2).any(|pair| pair == ["--mode", "ask"]));
        let antigravity = spec("antigravity", "PROMPT", None).unwrap();
        assert!(antigravity
            .args
            .windows(2)
            .any(|pair| pair == ["--mode", "plan"]));
    }
}
