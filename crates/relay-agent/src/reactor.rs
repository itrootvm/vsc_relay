use crate::automation::{AutomationConfig, Mode, RetryRules};
use crate::inject;
use crate::Emitted;
use chrono::Utc;
use relay_adapters::claude;
use relay_core::event::{EventKind, EventSource, RelayEvent};
use relay_core::ids::{AgentKind, MachineId};
use relay_core::state::ClaudeState;
use relay_core::{classify_error, ErrorClass};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

#[derive(Default)]
struct Retry {
    attempts: u32,
    acted_sig: Option<String>,
}

#[derive(Clone)]
pub struct NotifyCtx {
    pub machine_id: MachineId,
    pub workspace: PathBuf,
    pub branch: Option<String>,
    pub agent: AgentKind,
    pub session_ref: String,
    pub title: Option<String>,
    pub alias: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Skip,
    Terminal,
    RateLimit,
    GaveUp { attempts: u32 },
    Retry { attempt: u32, delay: u64 },
}

pub struct Reactor {
    state: Mutex<HashMap<String, Retry>>,
    tx: mpsc::UnboundedSender<Emitted>,
}

fn backoff(attempt: u32, base: u64, cap: u64, jitter: f64) -> u64 {
    let shift = attempt.saturating_sub(1).min(16);
    let exp = base.saturating_mul(1u64 << shift);
    let capped = exp.min(cap).max(1);
    let half = capped / 2;
    half + ((capped - half) as f64 * jitter.clamp(0.0, 1.0)) as u64
}

fn plan(class: ErrorClass, attempts: u32, rules: &RetryRules, jitter: f64) -> Plan {
    match class {
        ErrorClass::Terminal => Plan::Terminal,
        ErrorClass::RateLimit => {
            if rules.wait_for_reset {
                Plan::RateLimit
            } else {
                Plan::Skip
            }
        }
        ErrorClass::Transient => {
            if attempts >= rules.max_attempts {
                Plan::GaveUp { attempts }
            } else {
                let attempt = attempts + 1;
                Plan::Retry {
                    attempt,
                    delay: backoff(attempt, rules.base_secs, rules.cap_secs, jitter),
                }
            }
        }
    }
}

fn rand_unit() -> f64 {
    let mut b = [0u8; 8];
    let _ = getrandom::getrandom(&mut b);
    let n = u64::from_le_bytes(b);
    (n >> 11) as f64 / (1u64 << 53) as f64
}

fn sig_of(tip_uuid: Option<&str>, message: &str) -> String {
    match tip_uuid {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => blake3::hash(message.as_bytes()).to_hex().as_str()[..16].to_string(),
    }
}

impl Reactor {
    pub fn new(tx: mpsc::UnboundedSender<Emitted>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(HashMap::new()),
            tx,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn observe_claude(
        self: &Arc<Self>,
        pid: Option<u32>,
        session_id: &str,
        jsonl_path: &Path,
        state: &ClaudeState,
        tip_uuid: Option<&str>,
        cfg: &AutomationConfig,
        nctx: &NotifyCtx,
    ) {
        let message = match state {
            ClaudeState::Idle => {
                self.state.lock().await.remove(session_id);
                return;
            }
            ClaudeState::Error { message } => message.clone(),
            _ => return,
        };
        if cfg.resolve(Some(session_id), &nctx.alias, crate::automation::now_secs()) != Mode::Auto {
            return;
        }
        let rules = &cfg.auto.retry;
        if !rules.enabled {
            return;
        }
        let class = classify_error(&message);
        let sig = sig_of(tip_uuid, &message);
        let jitter = rand_unit();

        let decision = {
            let mut map = self.state.lock().await;
            let entry = map.entry(session_id.to_string()).or_default();
            if entry.acted_sig.as_deref() == Some(sig.as_str()) {
                return;
            }
            let p = plan(class, entry.attempts, rules, jitter);
            entry.acted_sig = Some(sig.clone());
            if let Plan::Retry { attempt, .. } = p {
                entry.attempts = attempt;
            }
            p
        };

        match decision {
            Plan::Skip => {}
            Plan::Terminal => {
                info!(
                    target: "relay::trace", pipeline = "disk", stage = "reaction",
                    kind = "terminal_error", alias = %nctx.alias,
                    "terminal error; no auto-retry"
                );
            }
            Plan::RateLimit => {
                info!(
                    target: "relay::trace", pipeline = "disk", stage = "reaction",
                    kind = "rate_limit", alias = %nctx.alias,
                    "rate/usage limit; auto-retry paused"
                );
                if rules.notify {
                    self.notice(
                        nctx,
                        "rate-limit",
                        "hit a usage/rate limit - auto-retry paused until it resets",
                    )
                    .await;
                }
            }
            Plan::GaveUp { attempts } => {
                warn!(
                    target: "relay::trace", pipeline = "disk", stage = "reaction",
                    kind = "retry_gave_up", alias = %nctx.alias, attempts,
                    "gave up auto-retry"
                );
                if rules.notify {
                    self.notice(
                        nctx,
                        "retry",
                        &format!("gave up after {attempts} auto-retries"),
                    )
                    .await;
                }
            }
            Plan::Retry { attempt, delay } => {
                info!(
                    target: "relay::trace", pipeline = "disk", stage = "reaction",
                    kind = "retry_scheduled", alias = %nctx.alias,
                    attempt, delay_s = delay,
                    "auto-retry scheduled"
                );
                self.spawn_retry(
                    pid,
                    session_id.to_string(),
                    jsonl_path.to_path_buf(),
                    attempt,
                    rules.max_attempts,
                    delay,
                    rules.notify,
                    nctx.clone(),
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_retry(
        self: &Arc<Self>,
        pid: Option<u32>,
        session_id: String,
        jsonl_path: PathBuf,
        attempt: u32,
        max: u32,
        delay: u64,
        notify: bool,
        nctx: NotifyCtx,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(delay)).await;
            let live_pid = inject::available_pid(&session_id).or(pid);
            let still_error = match claude::read_state(&jsonl_path, live_pid) {
                Ok(res) => matches!(res.state, ClaudeState::Error { .. }),
                Err(_) => false,
            };
            if !still_error {
                info!(
                    target: "relay::trace", pipeline = "disk", stage = "reaction",
                    kind = "retry_stale", alias = %nctx.alias, attempt,
                    "session advanced before retry; skipping inject"
                );
                return;
            }
            let Some(tapped) = inject::available_pid(&session_id) else {
                info!(
                    target: "relay::trace", pipeline = "disk", stage = "reaction",
                    kind = "retry_untapped", alias = %nctx.alias, attempt,
                    "session not tapped; cannot in-band retry"
                );
                if notify {
                    this.notice(
                        &nctx,
                        "retry",
                        &format!("transient error but session not tapped - retry manually ({attempt}/{max})"),
                    )
                    .await;
                }
                return;
            };
            match tokio::task::spawn_blocking(move || inject::send_user_message(tapped, "continue"))
                .await
            {
                Ok(Ok(())) => {
                    info!(
                        target: "relay::trace", pipeline = "disk", stage = "reaction",
                        kind = "retry_fired", alias = %nctx.alias, attempt,
                        "auto-retry injected continue"
                    );
                    if notify {
                        this.notice(
                            &nctx,
                            "retry",
                            &format!("auto-retry {attempt}/{max} after transient error"),
                        )
                        .await;
                    }
                }
                Ok(Err(e)) => warn!(
                    target: "relay::trace", pipeline = "disk", stage = "reaction",
                    kind = "retry_failed", alias = %nctx.alias, attempt,
                    "auto-retry inject failed: {e}"
                ),
                Err(e) => warn!(
                    target: "relay::trace", pipeline = "disk", stage = "reaction",
                    kind = "retry_failed", alias = %nctx.alias, attempt,
                    "auto-retry join failed: {e}"
                ),
            }
        });
    }

    async fn notice(&self, nctx: &NotifyCtx, action: &str, detail: &str) {
        let event = RelayEvent::new(
            nctx.machine_id.clone(),
            nctx.workspace.clone(),
            nctx.branch.clone(),
            nctx.agent,
            nctx.session_ref.clone(),
            nctx.title.clone(),
            Utc::now(),
            EventKind::AutoAction {
                action: action.to_string(),
                detail: detail.to_string(),
            },
            EventSource::Tail,
        );
        let _ = self.tx.send(Emitted {
            alias: nctx.alias.clone(),
            machine: nctx.machine_id.0.clone(),
            event,
            usage: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> RetryRules {
        RetryRules::default()
    }

    #[test]
    fn transient_schedules_bounded_retries() {
        let r = rules();
        assert!(matches!(
            plan(ErrorClass::Transient, 0, &r, 0.5),
            Plan::Retry { attempt: 1, .. }
        ));
        assert!(matches!(
            plan(ErrorClass::Transient, 2, &r, 0.5),
            Plan::Retry { attempt: 3, .. }
        ));
        assert_eq!(
            plan(ErrorClass::Transient, 3, &r, 0.5),
            Plan::GaveUp { attempts: 3 }
        );
    }

    #[test]
    fn terminal_never_retries() {
        assert_eq!(plan(ErrorClass::Terminal, 0, &rules(), 0.5), Plan::Terminal);
    }

    #[test]
    fn rate_limit_respects_wait_flag() {
        let mut r = rules();
        assert_eq!(plan(ErrorClass::RateLimit, 0, &r, 0.5), Plan::RateLimit);
        r.wait_for_reset = false;
        assert_eq!(plan(ErrorClass::RateLimit, 0, &r, 0.5), Plan::Skip);
    }

    #[test]
    fn backoff_grows_and_caps_with_jitter_band() {
        let (base, cap) = (2, 60);
        let d1 = backoff(1, base, cap, 0.0);
        let d1h = backoff(1, base, cap, 1.0);
        assert!(d1 >= 1 && d1 <= d1h && d1h <= 2);
        let big = backoff(20, base, cap, 1.0);
        assert!(big <= cap, "delay {big} exceeds cap");
        let low = backoff(20, base, cap, 0.0);
        assert!(
            low >= cap / 2,
            "jitter floor should be ~half cap, got {low}"
        );
    }

    #[test]
    fn sig_prefers_tip_then_hashes_message() {
        assert_eq!(sig_of(Some("tip-1"), "err"), "tip-1");
        assert_eq!(sig_of(Some(""), "err"), sig_of(None, "err"));
        assert_ne!(sig_of(None, "err a"), sig_of(None, "err b"));
        assert_eq!(sig_of(None, "err a").len(), 16);
    }
}
