pub mod backends;
pub mod context;
pub mod decision;
pub mod discover;
pub mod keys;
pub mod pool;
pub mod review;
pub mod robot;

use crate::automation::AutomationConfig;
use anyhow::{bail, Context, Result};

pub const SUPERVISOR_SYSTEM: &str =
    "You supervise a coding agent's session. Read the state and reply ONLY with the decision JSON: \
     action is one of continue|retry|accept_plan|feedback|stop|wait, plus a short reason.";

pub const IMPROVE_SYSTEM: &str =
    "You rewrite a user's prompt to a coding agent so it is clearer, more specific, and aligned with \
     the session's ongoing plan and observed behavior, without changing the user's intent. Output \
     ONLY the rewritten prompt text - no preamble, no quotes, no explanation.";

pub fn improve_user_prompt(context: &str, original: &str) -> String {
    if context.trim().is_empty() {
        format!("User prompt to rewrite:\n{original}")
    } else {
        format!("Session context:\n{context}\n\nUser prompt to rewrite:\n{original}")
    }
}

fn print_decision(backend: Option<&str>, d: &decision::Decision) {
    if let Some(b) = backend {
        println!("backend:    {b}");
    }
    println!("action:     {}", d.action.label());
    println!("reason:     {}", d.reason);
    if let Some(m) = &d.message {
        println!("message:    {m}");
    }
    if let Some(i) = d.option_index {
        println!("option:     {i}");
    }
    if let Some(w) = d.wait_seconds {
        println!("wait_secs:  {w}");
    }
    if let Some(c) = d.confidence {
        println!("confidence: {c:.2}");
    }
}

pub async fn run_ask(backend: &str, model: &str, prompt: &str) -> Result<()> {
    if model.is_empty() || prompt.is_empty() {
        bail!("usage: automation ask <ollama|openrouter> <model> <prompt>");
    }
    let client = reqwest::Client::new();
    let b = discover::Backend::parse(backend)
        .ok_or_else(|| anyhow::anyhow!("unknown backend '{backend}'"))?;
    let (d, _tokens) = match b {
        discover::Backend::Ollama => {
            backends::ollama_ask(
                &client,
                &discover::ollama_host(),
                model,
                SUPERVISOR_SYSTEM,
                prompt,
            )
            .await?
        }
        discover::Backend::OpenRouter => {
            let key = keys::key_for("openrouter").context("no OpenRouter key configured")?;
            backends::openrouter_ask(
                &client,
                &key,
                &[model.to_string()],
                SUPERVISOR_SYSTEM,
                prompt,
            )
            .await?
        }
        _ => bail!("ask currently supports ollama or openrouter"),
    };
    print_decision(None, &d);
    Ok(())
}

pub async fn run_route(prompt: &str) -> Result<()> {
    if prompt.is_empty() {
        bail!("usage: automation route <prompt>");
    }
    let cfg = AutomationConfig::load();
    if cfg.robot.providers.enabled.is_empty() {
        bail!("no Robot providers enabled (set robot.providers.enabled in automation.json)");
    }
    let discovered = discover::discover_all().await;
    let pool = pool::Pool::new();
    let (backend, d) = pool
        .ask(
            "cli-ask",
            &cfg.robot.providers,
            &discovered,
            SUPERVISOR_SYSTEM,
            prompt,
            crate::automation::now_secs(),
        )
        .await?;
    print_decision(Some(backend.id()), &d);
    Ok(())
}

fn login_command(id: &str) -> Option<&'static [&'static str]> {
    match id {
        "claude-cli" => Some(&["auth"]),
        "codex-cli" => Some(&["login"]),
        "cursor-cli" => Some(&["login"]),
        _ => None,
    }
}

pub async fn run_login(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("usage: automation login <backend>");
    }
    let discovered = discover::discover_all().await;
    let d = discovered
        .iter()
        .find(|d| d.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown backend '{id}'"))?;
    let Some(path) = d.path.clone() else {
        bail!("{id} has no CLI to log into; set a key with: automation provider-key {id} <key>");
    };
    let Some(args) = login_command(id) else {
        let ev = keys::env_var(id).unwrap_or("<API_KEY>");
        bail!(
            "{id} has no login command; authenticate by setting {ev} or: automation provider-key {id} <key>"
        );
    };
    println!("launching: {path} {}", args.join(" "));
    let status = std::process::Command::new(&path)
        .args(args)
        .status()
        .with_context(|| format!("spawn {path}"))?;
    if status.success() {
        println!("{id} login finished");
        Ok(())
    } else {
        bail!("{id} login exited {:?}", status.code())
    }
}

const HEALTH_PROMPT: &str =
    "The coding session is idle with no pending work and nothing to do. Reply with the decision \
     JSON, action stop.";

pub async fn run_health(json: bool, only: Option<&str>) -> Result<()> {
    let cfg = AutomationConfig::load();
    let discovered = discover::discover_all().await;
    let pool = pool::Pool::new();
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for d in &discovered {
        if only.is_some_and(|id| id != d.id) {
            continue;
        }
        let Some(backend) = discover::Backend::parse(&d.id) else {
            continue;
        };
        if !d.available {
            rows.push((d.id.clone(), "unavailable".to_string(), d.reason.clone()));
            continue;
        }
        let model = cfg
            .robot
            .providers
            .per_provider
            .get(&d.id)
            .and_then(|o| o.model.clone())
            .filter(|m| !m.trim().is_empty())
            .or_else(|| d.models.first().cloned())
            .unwrap_or_default();
        let needs_model = matches!(
            backend,
            discover::Backend::Ollama | discover::Backend::OpenRouter
        );
        if needs_model && model.is_empty() {
            rows.push((
                d.id.clone(),
                "no-model".to_string(),
                "set one with: automation provider-model".to_string(),
            ));
            continue;
        }
        let path = d.path.clone().unwrap_or_default();
        match pool
            .probe(backend, &model, &path, SUPERVISOR_SYSTEM, HEALTH_PROMPT)
            .await
        {
            Ok(dec) => rows.push((
                d.id.clone(),
                "ok".to_string(),
                format!("action={}", dec.action.label()),
            )),
            Err(e) => {
                let msg = e.to_string();
                let status = if discover::is_auth_error(&msg) {
                    "needs-login"
                } else {
                    "error"
                };
                rows.push((d.id.clone(), status.to_string(), msg));
            }
        }
    }
    if let Some(id) = only {
        if rows.is_empty() {
            bail!("unknown backend '{id}'");
        }
    }
    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|(id, status, detail)| {
                serde_json::json!({ "id": id, "status": status, "detail": detail })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else {
        for (id, status, detail) in &rows {
            let mark = match status.as_str() {
                "ok" => "✓",
                "needs-login" => "⚠",
                "unavailable" | "no-model" => "·",
                _ => "✗",
            };
            println!("{mark} {id:<12} {status:<12} {detail}");
        }
    }
    Ok(())
}

pub async fn run_improve(prompt: &str) -> Result<()> {
    if prompt.is_empty() {
        bail!("usage: automation improve <prompt>");
    }
    let cfg = AutomationConfig::load();
    if cfg.robot.providers.enabled.is_empty() {
        bail!("no Robot providers enabled (set robot.providers.enabled in automation.json)");
    }
    let discovered = discover::discover_all().await;
    let pool = pool::Pool::new();
    let (backend, text) = pool
        .improve(
            &cfg.robot.providers,
            &discovered,
            IMPROVE_SYSTEM,
            &improve_user_prompt("", prompt),
            crate::automation::now_secs(),
        )
        .await?;
    println!("backend:  {}", backend.id());
    println!("improved: {text}");
    Ok(())
}
