use super::backends;
use super::decision::Decision;
use super::discover::{ollama_host, Backend, Discovered};
use crate::automation::{Providers, Strategy};
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use tracing::{info, warn};

const COOLDOWN_SECS: i64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCall {
    pub backend: Backend,
    pub model: String,
    pub path: String,
}

fn model_for(id: &str, cfg: &Providers, discovered: &Discovered) -> Option<String> {
    if let Some(opts) = cfg.per_provider.get(id) {
        if let Some(m) = &opts.model {
            if !m.trim().is_empty() {
                return Some(m.clone());
            }
        }
    }
    discovered.models.first().cloned()
}

fn requires_model(b: Backend) -> bool {
    matches!(b, Backend::Ollama | Backend::OpenRouter)
}

pub fn plan_calls(
    cfg: &Providers,
    discovered: &[Discovered],
    cooling: &HashSet<String>,
    rr_cursor: usize,
) -> Vec<PlannedCall> {
    let by_id: HashMap<&str, &Discovered> = discovered.iter().map(|d| (d.id.as_str(), d)).collect();
    let mut eligible: Vec<PlannedCall> = Vec::new();
    for id in &cfg.enabled {
        if cooling.contains(id) {
            continue;
        }
        let Some(d) = by_id.get(id.as_str()) else {
            continue;
        };
        if !d.available {
            continue;
        }
        let Some(backend) = Backend::parse(id) else {
            continue;
        };
        let model = model_for(id, cfg, d);
        if requires_model(backend) && model.is_none() {
            continue;
        }
        eligible.push(PlannedCall {
            backend,
            model: model.unwrap_or_default(),
            path: d.path.clone().unwrap_or_default(),
        });
    }

    match cfg.strategy {
        Strategy::Single => eligible.truncate(1),
        Strategy::Priority => {}
        Strategy::RoundRobin => {
            let n = eligible.len();
            if n > 0 {
                eligible.rotate_left(rr_cursor % n);
            }
        }
        Strategy::CostOptimized => {
            eligible.sort_by_key(|c| if c.backend == Backend::Ollama { 0 } else { 1 });
        }
    }
    eligible
}

struct PoolState {
    rr_cursor: usize,
    cooldown: HashMap<String, i64>,
}

pub struct Pool {
    inner: Mutex<PoolState>,
}

impl Default for Pool {
    fn default() -> Self {
        Pool::new()
    }
}

impl Pool {
    pub fn new() -> Self {
        Pool {
            inner: Mutex::new(PoolState {
                rr_cursor: 0,
                cooldown: HashMap::new(),
            }),
        }
    }

    fn cooling(&self, now: i64) -> (HashSet<String>, usize) {
        let s = self.inner.lock().unwrap();
        let cooling = s
            .cooldown
            .iter()
            .filter(|(_, until)| **until > now)
            .map(|(k, _)| k.clone())
            .collect();
        (cooling, s.rr_cursor)
    }

    fn penalize(&self, id: &str, now: i64) {
        let mut s = self.inner.lock().unwrap();
        s.cooldown.insert(id.to_string(), now + COOLDOWN_SECS);
    }

    fn advance(&self) {
        let mut s = self.inner.lock().unwrap();
        s.rr_cursor = s.rr_cursor.wrapping_add(1);
    }

    pub async fn ask(
        &self,
        session_id: &str,
        cfg: &Providers,
        discovered: &[Discovered],
        system: &str,
        user: &str,
        now: i64,
    ) -> Result<(Backend, Decision)> {
        let (cooling, rr) = self.cooling(now);
        let plan = plan_calls(cfg, discovered, &cooling, rr);
        if plan.is_empty() {
            bail!("no eligible provider (enabled + available + not cooling + has model)");
        }
        let (spent_total, per_provider, _) = crate::compass::usage_totals(session_id);
        let client = reqwest::Client::new();
        let mut budget_skipped = 0usize;
        for call in &plan {
            let id = call.backend.id();
            if !within_budget(
                id,
                cfg,
                spent_total,
                per_provider.get(id).copied().unwrap_or(0.0),
            ) {
                budget_skipped += 1;
                info!(
                    target: "relay::trace", pipeline = "robot", stage = "budget_skip",
                    backend = id, "provider skipped: spend ceiling reached"
                );
                continue;
            }
            let started = std::time::Instant::now();
            match dispatch(&client, call, system, user).await {
                Ok((d, tokens)) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    let est = est_usd(id, cfg, tokens);
                    let _ = crate::compass::record_usage(
                        session_id,
                        id,
                        &call.model,
                        tokens,
                        est,
                        latency_ms,
                        true,
                    );
                    self.advance();
                    info!(
                        target: "relay::trace", pipeline = "robot", stage = "ask",
                        backend = id, action = d.action.label(),
                        "supervisor decision"
                    );
                    return Ok((call.backend, d));
                }
                Err(e) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    let _ = crate::compass::record_usage(
                        session_id,
                        id,
                        &call.model,
                        None,
                        0.0,
                        latency_ms,
                        false,
                    );
                    self.penalize(id, now);
                    warn!(
                        target: "relay::trace", pipeline = "robot", stage = "ask_failed",
                        backend = id,
                        "provider failed, cooling down: {e}"
                    );
                }
            }
        }
        if budget_skipped == plan.len() {
            bail!(
                "spend ceiling reached: all {} provider(s) over budget",
                plan.len()
            );
        }
        bail!(
            "all {} eligible providers failed",
            plan.len() - budget_skipped
        )
    }

    pub async fn probe(
        &self,
        backend: Backend,
        model: &str,
        path: &str,
        system: &str,
        user: &str,
    ) -> Result<Decision> {
        let client = reqwest::Client::new();
        let call = PlannedCall {
            backend,
            model: model.to_string(),
            path: path.to_string(),
        };
        dispatch(&client, &call, system, user)
            .await
            .map(|(decision, _)| decision)
    }

    pub async fn improve(
        &self,
        cfg: &Providers,
        discovered: &[Discovered],
        system: &str,
        user: &str,
        now: i64,
    ) -> Result<(Backend, String)> {
        let (cooling, rr) = self.cooling(now);
        let plan = plan_calls(cfg, discovered, &cooling, rr);
        if plan.is_empty() {
            bail!("no eligible provider (enabled + available + not cooling + has model)");
        }
        let client = reqwest::Client::new();
        for call in &plan {
            match dispatch_text(&client, call, system, user).await {
                Ok(text) if !text.trim().is_empty() => {
                    self.advance();
                    info!(
                        target: "relay::trace", pipeline = "robot", stage = "improve",
                        backend = call.backend.id(),
                        "supervisor rewrote prompt"
                    );
                    return Ok((call.backend, text));
                }
                Ok(_) => {
                    self.penalize(call.backend.id(), now);
                    warn!(
                        target: "relay::trace", pipeline = "robot", stage = "improve_empty",
                        backend = call.backend.id(),
                        "provider returned empty rewrite, cooling down"
                    );
                }
                Err(e) => {
                    self.penalize(call.backend.id(), now);
                    warn!(
                        target: "relay::trace", pipeline = "robot", stage = "improve_failed",
                        backend = call.backend.id(),
                        "provider failed, cooling down: {e}"
                    );
                }
            }
        }
        bail!("all {} eligible providers failed", plan.len())
    }
}

fn provider_price(id: &str, cfg: &Providers) -> Option<f64> {
    cfg.per_provider
        .get(id)
        .and_then(|opts| opts.usd_per_ktok)
        .filter(|price| *price > 0.0)
}

fn est_usd(id: &str, cfg: &Providers, tokens: Option<u64>) -> f64 {
    match (provider_price(id, cfg), tokens) {
        (Some(price), Some(count)) => (count as f64 / 1000.0) * price,
        _ => 0.0,
    }
}

fn within_budget(id: &str, cfg: &Providers, spent_total: f64, spent_provider: f64) -> bool {
    if let Some(max) = cfg.per_provider.get(id).and_then(|opts| opts.max_usd) {
        if max > 0.0 && spent_provider >= max {
            return false;
        }
    }
    if let Some(budget) = cfg.budget_usd {
        if budget > 0.0 && spent_total >= budget && provider_price(id, cfg).is_some() {
            return false;
        }
    }
    true
}

async fn dispatch(
    client: &reqwest::Client,
    call: &PlannedCall,
    system: &str,
    user: &str,
) -> Result<(Decision, Option<u64>)> {
    let env = super::keys::cli_env(call.backend.id());
    match call.backend {
        Backend::Ollama => {
            backends::ollama_ask(client, &ollama_host(), &call.model, system, user).await
        }
        Backend::OpenRouter => {
            let key = super::keys::key_for("openrouter")
                .ok_or_else(|| anyhow::anyhow!("no OpenRouter key configured"))?;
            backends::openrouter_ask(
                client,
                &key,
                std::slice::from_ref(&call.model),
                system,
                user,
            )
            .await
        }
        Backend::ClaudeCli
        | Backend::CodexCli
        | Backend::GeminiCli
        | Backend::CursorCli
        | Backend::Antigravity => backends::cli_ask(
            call.backend.id(),
            &call.path,
            &call.model,
            &env,
            system,
            user,
        )
        .await
        .map(|decision| (decision, None)),
    }
}

async fn dispatch_text(
    client: &reqwest::Client,
    call: &PlannedCall,
    system: &str,
    user: &str,
) -> Result<String> {
    let env = super::keys::cli_env(call.backend.id());
    match call.backend {
        Backend::Ollama => {
            backends::ollama_text(client, &ollama_host(), &call.model, system, user).await
        }
        Backend::OpenRouter => {
            let key = super::keys::key_for("openrouter")
                .ok_or_else(|| anyhow::anyhow!("no OpenRouter key configured"))?;
            backends::openrouter_text(
                client,
                &key,
                std::slice::from_ref(&call.model),
                system,
                user,
            )
            .await
        }
        Backend::ClaudeCli
        | Backend::CodexCli
        | Backend::GeminiCli
        | Backend::CursorCli
        | Backend::Antigravity => {
            backends::cli_improve(
                call.backend.id(),
                &call.path,
                &call.model,
                &env,
                system,
                user,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::ProviderOpts;

    fn disc(id: &str, available: bool, models: &[&str]) -> Discovered {
        Discovered {
            id: id.to_string(),
            available,
            reason: String::new(),
            local: id == "ollama",
            needs_key: false,
            models: models.iter().map(|s| s.to_string()).collect(),
            path: if id.ends_with("-cli") || id == "antigravity" {
                Some(format!("/usr/bin/{id}"))
            } else {
                None
            },
        }
    }

    fn cfg(enabled: &[&str], strategy: Strategy) -> Providers {
        Providers {
            enabled: enabled.iter().map(|s| s.to_string()).collect(),
            strategy,
            per_provider: Default::default(),
            budget_usd: None,
        }
    }

    #[test]
    fn skips_unavailable_cooling_and_modelless() {
        let d = vec![
            disc("ollama", true, &["llama3.2"]),
            disc("openrouter", false, &[]),
        ];
        let c = cfg(&["ollama", "openrouter"], Strategy::Priority);
        let plan = plan_calls(&c, &d, &HashSet::new(), 0);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].backend, Backend::Ollama);
        assert_eq!(plan[0].model, "llama3.2");

        let cooling: HashSet<String> = ["ollama".to_string()].into_iter().collect();
        assert!(plan_calls(&c, &d, &cooling, 0).is_empty());
    }

    #[test]
    fn budget_ceiling_reads_fields_and_only_halts_metered_providers() {
        let mut c = cfg(&["openrouter", "ollama"], Strategy::Priority);
        c.budget_usd = Some(0.05);
        c.per_provider.insert(
            "openrouter".to_string(),
            ProviderOpts {
                model: Some("x/y".to_string()),
                weight: 1,
                max_usd: Some(0.10),
                usd_per_ktok: Some(2.0),
            },
        );

        assert!((est_usd("openrouter", &c, Some(1000)) - 2.0).abs() < 1e-9);
        assert!((est_usd("openrouter", &c, Some(500)) - 1.0).abs() < 1e-9);
        assert_eq!(est_usd("ollama", &c, Some(1000)), 0.0);
        assert_eq!(est_usd("openrouter", &c, None), 0.0);

        assert!(within_budget("openrouter", &c, 0.0, 0.05));
        assert!(!within_budget("openrouter", &c, 0.0, 0.10));

        assert!(!within_budget("openrouter", &c, 0.05, 0.0));
        assert!(within_budget("ollama", &c, 0.05, 0.0));
    }

    #[test]
    fn cli_backend_eligible_without_model() {
        let d = vec![disc("gemini-cli", true, &[])];
        let c = cfg(&["gemini-cli"], Strategy::Priority);
        let plan = plan_calls(&c, &d, &HashSet::new(), 0);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].backend, Backend::GeminiCli);
        assert_eq!(plan[0].model, "");
    }

    #[test]
    fn openrouter_needs_configured_model() {
        let d = vec![disc("openrouter", true, &[])];
        let mut c = cfg(&["openrouter"], Strategy::Priority);
        assert!(plan_calls(&c, &d, &HashSet::new(), 0).is_empty());
        c.per_provider.insert(
            "openrouter".to_string(),
            ProviderOpts {
                model: Some("anthropic/claude-sonnet-5".to_string()),
                weight: 1,
                max_usd: None,
                usd_per_ktok: None,
            },
        );
        let plan = plan_calls(&c, &d, &HashSet::new(), 0);
        assert_eq!(plan[0].model, "anthropic/claude-sonnet-5");
    }

    #[test]
    fn single_takes_one_costopt_puts_local_first() {
        let d = vec![
            disc("openrouter", true, &["x/y"]),
            disc("ollama", true, &["llama3.2"]),
        ];
        let single = plan_calls(
            &cfg(&["openrouter", "ollama"], Strategy::Single),
            &d,
            &HashSet::new(),
            0,
        );
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].backend, Backend::OpenRouter);

        let co = plan_calls(
            &cfg(&["openrouter", "ollama"], Strategy::CostOptimized),
            &d,
            &HashSet::new(),
            0,
        );
        assert_eq!(co[0].backend, Backend::Ollama);
        assert_eq!(co[1].backend, Backend::OpenRouter);
    }

    #[test]
    fn round_robin_rotates() {
        let d = vec![
            disc("ollama", true, &["a"]),
            disc("openrouter", true, &["b"]),
        ];
        let mut c = cfg(&["ollama", "openrouter"], Strategy::RoundRobin);
        c.per_provider.insert(
            "openrouter".to_string(),
            ProviderOpts {
                model: Some("b".into()),
                weight: 1,
                max_usd: None,
                usd_per_ktok: None,
            },
        );
        let p0 = plan_calls(&c, &d, &HashSet::new(), 0);
        let p1 = plan_calls(&c, &d, &HashSet::new(), 1);
        assert_eq!(p0[0].backend, Backend::Ollama);
        assert_eq!(p1[0].backend, Backend::OpenRouter);
    }
}
