pub mod annotation;
mod cli;
pub mod config;
pub mod install;
#[cfg(not(target_os = "linux"))]
mod local;
#[cfg(target_os = "linux")]
#[path = "local_unavailable.rs"]
mod local;
mod remote;

pub const fn local_backend_available() -> bool {
    !cfg!(target_os = "linux")
}

use anyhow::{bail, Context, Result};
use config::{SemanticBackend, SemanticConfig};
use relay_compass::{SemanticFacts, SemanticInputFrame};

pub async fn classify(
    config: &SemanticConfig,
    frames: Vec<SemanticInputFrame>,
    api_key: Option<String>,
) -> Result<Vec<SemanticFacts>> {
    if frames.is_empty() {
        return Ok(Vec::new());
    }
    match config.backend {
        SemanticBackend::Off => Ok(Vec::new()),
        SemanticBackend::Local => classify_local(config, frames).await,
        SemanticBackend::Ollama | SemanticBackend::OpenAiCompatible => {
            remote::classify(config, &frames, api_key.as_deref()).await
        }
        SemanticBackend::AgentCli => cli::classify(config, &frames).await,
    }
}

pub async fn check(config: &SemanticConfig, api_key: Option<String>) -> Result<String> {
    let probe = || SemanticInputFrame {
        episode: 0,
        goal: "Fix the requested defect".to_string(),
        user_contract: "Fix it and verify the result".to_string(),
        assistant: "I am still investigating; this is not complete.".to_string(),
        tools: Vec::new(),
        runtime: Vec::new(),
        runtime_error: false,
    };
    match config.backend {
        SemanticBackend::Off => Ok("semantic backend off".to_string()),
        SemanticBackend::Local => check_local(config).await,
        SemanticBackend::Ollama | SemanticBackend::OpenAiCompatible => {
            if config.model.trim().is_empty() {
                bail!("semantic model is not configured");
            }
            let facts = remote::classify(config, &[probe()], api_key.as_deref()).await?;
            if facts.len() != 1 {
                bail!("semantic provider probe returned no aligned fact frame");
            }
            Ok(format!(
                "ready: {} model={} (provider confidence is uncalibrated)",
                config.backend.label(),
                config.model
            ))
        }
        SemanticBackend::AgentCli => {
            let cli = config.cli_provider().context(
                "semantic CLI is not configured (claude|codex|gemini|cursor|antigravity)",
            )?;
            let facts = cli::classify(config, &[probe()]).await?;
            if facts.len() != 1 {
                bail!("semantic CLI probe returned no aligned fact frame");
            }
            Ok(format!(
                "ready: agent_cli={cli} (CLI confidence is uncalibrated)"
            ))
        }
    }
}

async fn classify_local(
    config: &SemanticConfig,
    frames: Vec<SemanticInputFrame>,
) -> Result<Vec<SemanticFacts>> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || local::classify(&config, &frames)).await?
}

async fn check_local(config: &SemanticConfig) -> Result<String> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let status = local::status(&config)?;
        let facts = local::classify(&config, &[local_probe()])?;
        if facts.len() != 1 {
            bail!("local semantic inference probe returned no aligned fact frame");
        }
        Ok(format!("{status}; inference probe ok"))
    })
    .await?
}

fn local_probe() -> SemanticInputFrame {
    SemanticInputFrame {
        episode: 0,
        goal: "Fix the requested defect".to_string(),
        user_contract: "Fix it and verify the result".to_string(),
        assistant: "I am still investigating; this is not complete.".to_string(),
        tools: Vec::new(),
        runtime: Vec::new(),
        runtime_error: false,
    }
}
