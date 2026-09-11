use super::decision::{decision_schema, parse_decision, Decision};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

const ASK_TIMEOUT: Duration = Duration::from_secs(45);
const CLI_TIMEOUT: Duration = Duration::from_secs(90);

fn combined_prompt(system: &str, user: &str) -> String {
    format!("{system}\n\n{user}\n\nRespond with ONLY the decision JSON object, no prose.")
}

fn plain_prompt(system: &str, user: &str) -> String {
    format!("{system}\n\n{user}")
}

fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    s.chars().skip(n - max).collect()
}

fn cli_args(id: &str, model: &str, prompt: &str) -> Vec<String> {
    let model = model.trim();
    if id == "antigravity" {
        let mut args: Vec<String> = vec!["--mode".into(), "plan".into(), "--sandbox".into()];
        if !model.is_empty() {
            args.push("--model".into());
            args.push(model.to_string());
        }
        args.push("-p".into());
        args.push(prompt.to_string());
        return args;
    }
    let parts: &[&str] = match id {
        "codex-cli" => &[
            "exec",
            prompt,
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
            "--ephemeral",
            "--ignore-rules",
        ],
        "claude-cli" => &[
            "-p",
            prompt,
            "--output-format",
            "text",
            "--tools",
            "",
            "--safe-mode",
            "--disable-slash-commands",
            "--no-session-persistence",
            "--permission-mode",
            "plan",
        ],
        "cursor-cli" => &[
            "-p",
            prompt,
            "--output-format",
            "text",
            "--mode",
            "ask",
            "--sandbox",
            "enabled",
            "--trust",
        ],

        "gemini-cli" => &["-p", prompt],
        _ => &["-p", prompt],
    };
    let mut v: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
    if !model.is_empty() {
        v.push("--model".to_string());
        v.push(model.to_string());
    }
    v
}

struct HelperWorkdir(PathBuf);

impl HelperWorkdir {
    fn create() -> Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("vsc-relay-helper-{}-{nonce}", std::process::id()));
        let gemini = path.join(".gemini");
        std::fs::create_dir_all(&gemini).context("create isolated helper workspace")?;

        std::fs::write(
            gemini.join("settings.json"),
            br#"{"coreTools":["list_directory","read_file","search_file_content","glob","read_many_files"]}"#,
        )
        .context("write isolated Gemini read-only policy")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .context("secure isolated helper workspace")?;
        }
        Ok(Self(path))
    }
}

impl Drop for HelperWorkdir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn run_cli(
    bin: &str,
    args: &[&str],
    env: &[(String, String)],
    timeout: Duration,
) -> Result<String> {
    let workdir = HelperWorkdir::create()?;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .current_dir(&workdir.0)
        .env("PATH", super::discover::path_env())
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().with_context(|| format!("spawn {bin}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("{bin} timed out"))?
        .with_context(|| format!("{bin} wait"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{bin} exited {:?}: {}",
            out.status.code(),
            tail_chars(err.trim(), 500)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn cli_ask(
    id: &str,
    bin: &str,
    model: &str,
    env: &[(String, String)],
    system: &str,
    user: &str,
) -> Result<Decision> {
    let p = combined_prompt(system, user);
    let args = cli_args(id, model, &p);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_cli(bin, &refs, env, CLI_TIMEOUT).await?;
    parse_decision(&out)
}

pub async fn cli_improve(
    id: &str,
    bin: &str,
    model: &str,
    env: &[(String, String)],
    system: &str,
    user: &str,
) -> Result<String> {
    let p = plain_prompt(system, user);
    let args = cli_args(id, model, &p);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_cli(bin, &refs, env, CLI_TIMEOUT).await?;
    Ok(out.trim().to_string())
}

fn chat_messages(system: &str, user: &str) -> Value {
    json!([
        { "role": "system", "content": system },
        { "role": "user", "content": user }
    ])
}

pub fn ollama_body(model: &str, system: &str, user: &str) -> Value {
    json!({
        "model": model,
        "messages": chat_messages(system, user),
        "stream": false,
        "format": decision_schema(),
        "options": { "temperature": 0.2 }
    })
}

pub fn ollama_text_body(model: &str, system: &str, user: &str) -> Value {
    json!({
        "model": model,
        "messages": chat_messages(system, user),
        "stream": false,
        "options": { "temperature": 0.3 }
    })
}

pub fn openrouter_text_body(models: &[String], system: &str, user: &str) -> Value {
    let mut body = json!({
        "messages": chat_messages(system, user),
        "temperature": 0.3
    });
    if models.len() == 1 {
        body["model"] = json!(models[0]);
    } else {
        body["models"] = json!(models);
    }
    body
}

pub fn openrouter_body(models: &[String], system: &str, user: &str) -> Value {
    let mut body = json!({
        "messages": chat_messages(system, user),
        "response_format": {
            "type": "json_schema",
            "json_schema": { "name": "decision", "strict": true, "schema": decision_schema() }
        },
        "temperature": 0.2
    });
    if models.len() == 1 {
        body["model"] = json!(models[0]);
    } else {
        body["models"] = json!(models);
    }
    body
}

fn ollama_content(resp: &Value) -> Option<&str> {
    resp.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
}

fn openai_content(resp: &Value) -> Option<&str> {
    resp.get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
}

fn ollama_tokens(resp: &Value) -> Option<u64> {
    match (
        resp.get("prompt_eval_count").and_then(Value::as_u64),
        resp.get("eval_count").and_then(Value::as_u64),
    ) {
        (Some(prompt), Some(completion)) => Some(prompt + completion),
        (Some(prompt), None) => Some(prompt),
        (None, Some(completion)) => Some(completion),
        (None, None) => None,
    }
}

fn openai_tokens(resp: &Value) -> Option<u64> {
    resp.get("usage")
        .and_then(|usage| usage.get("total_tokens"))
        .and_then(Value::as_u64)
}

pub async fn ollama_ask(
    client: &reqwest::Client,
    host: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<(Decision, Option<u64>)> {
    let url = format!("{}/api/chat", host.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .timeout(ASK_TIMEOUT)
        .json(&ollama_body(model, system, user))
        .send()
        .await
        .map_err(|e| e.without_url())
        .context("ollama request")?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| e.without_url())
        .context("ollama body")?;
    if !status.is_success() {
        return Err(anyhow!("ollama http {}", status.as_u16()));
    }
    let content = ollama_content(&body).ok_or_else(|| anyhow!("ollama: no message content"))?;
    Ok((parse_decision(content)?, ollama_tokens(&body)))
}

pub async fn openrouter_ask(
    client: &reqwest::Client,
    key: &str,
    models: &[String],
    system: &str,
    user: &str,
) -> Result<(Decision, Option<u64>)> {
    if models.is_empty() {
        return Err(anyhow!("openrouter: no model configured"));
    }
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .timeout(ASK_TIMEOUT)
        .bearer_auth(key)
        .json(&openrouter_body(models, system, user))
        .send()
        .await
        .map_err(|e| e.without_url())
        .context("openrouter request")?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| e.without_url())
        .context("openrouter body")?;
    if !status.is_success() {
        return Err(anyhow!("openrouter http {}", status.as_u16()));
    }
    let content = openai_content(&body).ok_or_else(|| anyhow!("openrouter: no message content"))?;
    Ok((parse_decision(content)?, openai_tokens(&body)))
}

pub async fn ollama_text(
    client: &reqwest::Client,
    host: &str,
    model: &str,
    system: &str,
    user: &str,
) -> Result<String> {
    let url = format!("{}/api/chat", host.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .timeout(ASK_TIMEOUT)
        .json(&ollama_text_body(model, system, user))
        .send()
        .await
        .map_err(|e| e.without_url())
        .context("ollama request")?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| e.without_url())
        .context("ollama body")?;
    if !status.is_success() {
        return Err(anyhow!("ollama http {}", status.as_u16()));
    }
    let content = ollama_content(&body).ok_or_else(|| anyhow!("ollama: no message content"))?;
    Ok(content.trim().to_string())
}

pub async fn openrouter_text(
    client: &reqwest::Client,
    key: &str,
    models: &[String],
    system: &str,
    user: &str,
) -> Result<String> {
    if models.is_empty() {
        return Err(anyhow!("openrouter: no model configured"));
    }
    let resp = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .timeout(ASK_TIMEOUT)
        .bearer_auth(key)
        .json(&openrouter_text_body(models, system, user))
        .send()
        .await
        .map_err(|e| e.without_url())
        .context("openrouter request")?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| e.without_url())
        .context("openrouter body")?;
    if !status.is_success() {
        return Err(anyhow!("openrouter http {}", status.as_u16()));
    }
    let content = openai_content(&body).ok_or_else(|| anyhow!("openrouter: no message content"))?;
    Ok(content.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_body_carries_schema_and_no_stream() {
        let b = ollama_body("llama3.2", "sys", "usr");
        assert_eq!(b["model"], "llama3.2");
        assert_eq!(b["stream"], false);
        assert!(b["format"]["properties"]["action"].is_object());
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["messages"][1]["content"], "usr");
    }

    #[test]
    fn openrouter_body_single_vs_multi_model() {
        let one = openrouter_body(&["a/b".to_string()], "s", "u");
        assert_eq!(one["model"], "a/b");
        assert!(one.get("models").is_none());
        let many = openrouter_body(&["a/b".to_string(), "c/d".to_string()], "s", "u");
        assert_eq!(many["models"][1], "c/d");
        assert_eq!(many["response_format"]["type"], "json_schema");
        assert_eq!(many["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn extracts_provider_contents() {
        let ol = json!({"message":{"role":"assistant","content":"{\"action\":\"stop\"}"}});
        assert_eq!(ollama_content(&ol), Some("{\"action\":\"stop\"}"));
        let or = json!({"choices":[{"message":{"content":"{\"action\":\"wait\"}"}}]});
        assert_eq!(openai_content(&or), Some("{\"action\":\"wait\"}"));
        assert_eq!(openai_content(&json!({"choices":[]})), None);
    }

    #[test]
    fn cli_args_per_backend() {
        assert_eq!(
            cli_args("claude-cli", "", "hi"),
            vec![
                "-p",
                "hi",
                "--output-format",
                "text",
                "--tools",
                "",
                "--safe-mode",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--permission-mode",
                "plan"
            ]
        );
        assert_eq!(
            cli_args("cursor-cli", "", "hi"),
            vec![
                "-p",
                "hi",
                "--output-format",
                "text",
                "--mode",
                "ask",
                "--sandbox",
                "enabled",
                "--trust"
            ]
        );
        assert_eq!(cli_args("gemini-cli", "", "hi"), vec!["-p", "hi"]);
        assert_eq!(
            cli_args("antigravity", "", "hi"),
            vec!["--mode", "plan", "--sandbox", "-p", "hi"]
        );
        assert_eq!(
            cli_args("antigravity", "gemini-3-flash", "hi"),
            vec![
                "--mode",
                "plan",
                "--sandbox",
                "--model",
                "gemini-3-flash",
                "-p",
                "hi"
            ]
        );
        assert_eq!(
            cli_args("codex-cli", "", "hi"),
            vec![
                "exec",
                "hi",
                "--sandbox",
                "read-only",
                "--skip-git-repo-check",
                "--ephemeral",
                "--ignore-rules"
            ]
        );
    }

    #[test]
    fn cli_args_appends_model_when_set() {
        assert_eq!(
            cli_args("claude-cli", "opus", "hi"),
            vec![
                "-p",
                "hi",
                "--output-format",
                "text",
                "--tools",
                "",
                "--safe-mode",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--permission-mode",
                "plan",
                "--model",
                "opus"
            ]
        );
        assert_eq!(
            cli_args("gemini-cli", "gemini-2.5-flash", "hi"),
            vec!["-p", "hi", "--model", "gemini-2.5-flash"]
        );
        assert_eq!(
            cli_args("claude-cli", "  ", "hi"),
            vec![
                "-p",
                "hi",
                "--output-format",
                "text",
                "--tools",
                "",
                "--safe-mode",
                "--disable-slash-commands",
                "--no-session-persistence",
                "--permission-mode",
                "plan"
            ]
        );
    }

    #[test]
    fn plain_prompt_has_no_json_directive() {
        let p = plain_prompt("sys", "usr");
        assert!(p.contains("sys"));
        assert!(p.contains("usr"));
        assert!(!p.to_lowercase().contains("json"));
    }

    #[test]
    fn text_bodies_omit_schema() {
        let ol = ollama_text_body("m", "s", "u");
        assert!(ol.get("format").is_none());
        assert_eq!(ol["stream"], false);
        let or = openrouter_text_body(&["a/b".to_string()], "s", "u");
        assert!(or.get("response_format").is_none());
        assert_eq!(or["model"], "a/b");
    }

    #[tokio::test]
    async fn cli_improve_returns_trimmed_stdout() {
        let out = cli_improve("gemini-cli", "echo", "", &[], "keep this", "")
            .await
            .unwrap();
        assert!(out.contains("keep this"), "got: {out:?}");
        assert_eq!(out, out.trim());
    }

    #[tokio::test]
    async fn run_cli_captures_and_parses() {
        let out = run_cli(
            "echo",
            &["{\"action\":\"stop\",\"reason\":\"done\"}"],
            &[],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let d = parse_decision(&out).unwrap();
        assert_eq!(d.action, super::super::decision::Action::Stop);
        assert_eq!(d.reason, "done");
    }

    #[tokio::test]
    async fn run_cli_passes_env() {
        let out = run_cli(
            "sh",
            &[
                "-c",
                "printf '{\"action\":\"continue\",\"reason\":\"%s\"}' \"$AGENT_KEY\"",
            ],
            &[("AGENT_KEY".to_string(), "sekret".to_string())],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let d = parse_decision(&out).unwrap();
        assert_eq!(d.reason, "sekret");
    }

    #[tokio::test]
    async fn run_cli_times_out_and_kills() {
        let r = run_cli("sleep", &["5"], &[], Duration::from_millis(200)).await;
        assert!(r.is_err(), "expected timeout error");
    }

    #[tokio::test]
    async fn run_cli_reports_nonzero_exit() {
        let r = run_cli("false", &[], &[], Duration::from_secs(5)).await;
        assert!(r.is_err(), "nonzero exit should be an error");
    }

    #[test]
    fn helper_workspace_disables_gemini_tools_and_is_private() {
        let workdir = HelperWorkdir::create().unwrap();
        let policy = std::fs::read_to_string(workdir.0.join(".gemini/settings.json")).unwrap();
        assert_eq!(
            policy,
            r#"{"coreTools":["list_directory","read_file","search_file_content","glob","read_many_files"]}"#
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&workdir.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
