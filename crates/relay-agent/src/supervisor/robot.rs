use super::context::{build_context, SessionSnapshot};
use super::decision::{Action, Decision};
use super::discover;
use super::pool::Pool;
use super::review::{self, ReviewPlan};
use super::SUPERVISOR_SYSTEM;
use crate::automation::{now_secs, AutomationConfig, Mode};
use crate::compass::{self, CompletionSnapshot, StagedAssessment};
use crate::reactor::NotifyCtx;
use crate::{inject, Emitted};
use chrono::Utc;
use relay_adapters::claude;
use relay_adapters::family::Family;
use relay_compass::{predictive::PredictiveAction, GateState};
use relay_core::event::{EventKind, EventSource, RelayEvent};
use relay_core::ids::AgentKind;
use relay_core::state::{ClaudeState, CodexState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

const LADDER_HEAD_BYTES: u64 = 1024 * 1024;
const LADDER_TAIL_BYTES: u64 = 8 * 1024 * 1024;
const CONTEXT_BUDGET: usize = 8000;
const MAX_TERMINAL_RECEIPTS: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TerminalReceipt {
    observation_signature: String,
    action: String,
    recorded_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TerminalReceiptStore {
    entries: HashMap<String, TerminalReceipt>,
}

fn terminal_receipt_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".vsc-relay")
            .join("robot-terminal-receipts.json"),
    )
}

fn terminal_receipt_key(session_id: &str) -> String {
    blake3::hash(session_id.as_bytes()).to_hex().as_str()[..32].to_string()
}

fn read_terminal_receipts(path: &Path) -> TerminalReceiptStore {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn terminal_receipt_seen(path: &Path, session_id: &str, sig: &str, action: &str) -> bool {
    read_terminal_receipts(path)
        .entries
        .get(&terminal_receipt_key(session_id))
        .is_some_and(|receipt| receipt.observation_signature == sig && receipt.action == action)
}

fn record_terminal_receipt(
    path: &Path,
    session_id: &str,
    sig: &str,
    action: &str,
) -> anyhow::Result<()> {
    let mut store = read_terminal_receipts(path);
    store.entries.insert(
        terminal_receipt_key(session_id),
        TerminalReceipt {
            observation_signature: sig.to_string(),
            action: action.to_string(),
            recorded_at: now_secs(),
        },
    );
    if store.entries.len() > MAX_TERMINAL_RECEIPTS {
        let mut oldest = store
            .entries
            .iter()
            .map(|(key, receipt)| (key.clone(), receipt.recorded_at))
            .collect::<Vec<_>>();
        oldest.sort_by_key(|(_, recorded_at)| *recorded_at);
        for (key, _) in oldest
            .into_iter()
            .take(store.entries.len() - MAX_TERMINAL_RECEIPTS)
        {
            store.entries.remove(&key);
        }
    }
    crate::fsutil::secure_write(path, &serde_json::to_vec(&store)?)?;
    Ok(())
}

#[derive(Default)]
struct RobotSt {
    steps: u32,
    gate_probe_steps_charged: u8,
    completion_probe_steps: u8,
    acted_sig: Option<String>,
    armed_gap: Option<String>,
    armed_tip: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum SmartPlan {
    Skip,
    Budget,
    Arm,
    Act,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionDisposition {
    Verified,
    NeedsProof,
    Unproven,
}

fn completion_disposition(snapshot: CompletionSnapshot) -> CompletionDisposition {
    let fully_covered = snapshot.active > 0
        && snapshot.verified == snapshot.active
        && snapshot.unresolved() == 0
        && snapshot.layer_mismatches == 0
        && !snapshot.completion_scope_gap
        && !snapshot.proof_deficit
        && snapshot.coverage >= 0.999;
    if fully_covered {
        CompletionDisposition::Verified
    } else if snapshot.active > 0 {
        CompletionDisposition::NeedsProof
    } else {
        CompletionDisposition::Unproven
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionPlan {
    Skip,
    Probe,
    SafePartial,
}

const STEER_ESCALATION: [&str; 10] = [
    "sudo ",
    "--dangerously",
    "bypass",
    "disable the sandbox",
    "grant access",
    "rm -rf",
    "force push",
    "--no-verify",
    "skip permission",
    "chmod 777",
];

pub(crate) fn steer_is_safe(text: &str) -> bool {
    let lower = text.to_lowercase();
    !STEER_ESCALATION.iter().any(|bad| lower.contains(bad))
}

#[derive(Debug, PartialEq, Eq)]
enum StepPlan {
    Skip,
    Budget,
    Act,
}

fn step_plan(steps: u32, max_steps: u32, acted_sig: Option<&str>, sig: &str) -> StepPlan {
    if acted_sig == Some(sig) {
        return StepPlan::Skip;
    }
    if steps >= max_steps {
        return StepPlan::Budget;
    }
    StepPlan::Act
}

fn is_actionable(state: &ClaudeState) -> bool {
    matches!(state, ClaudeState::Idle | ClaudeState::Error { .. })
}

fn observation_signature(
    path: &Path,
    state: &ClaudeState,
    tip_uuid: Option<&str>,
    last_message: &str,
) -> String {
    let metadata = std::fs::metadata(path).ok();
    let modified = metadata
        .as_ref()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let len = metadata.as_ref().map(|meta| meta.len()).unwrap_or_default();
    let material = format!(
        "{modified}:{len}:{}:{}:{last_message}",
        state.label(),
        tip_uuid.unwrap_or("")
    );
    blake3::hash(material.as_bytes()).to_hex().as_str()[..16].to_string()
}

fn drive_text(d: &Decision) -> Option<String> {
    match d.action {
        Action::Continue | Action::Retry => Some("continue".to_string()),
        Action::Feedback => Some(
            d.message
                .clone()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| "continue".to_string()),
        ),
        Action::Stop | Action::Wait | Action::AcceptPlan => None,
    }
}

fn terminal_event_action(action: Action) -> &'static str {
    if action == Action::Stop {
        "robot_claimed_complete"
    } else {
        "robot"
    }
}

pub struct Robot {
    state: Mutex<HashMap<String, RobotSt>>,
    pool: Pool,
    control: Arc<dyn relay_control::Control>,
    tx: mpsc::UnboundedSender<Emitted>,
}

impl Robot {
    pub fn new(tx: mpsc::UnboundedSender<Emitted>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(HashMap::new()),
            pool: Pool::new(),
            control: Arc::from(relay_control::platform()),
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
        last_message: &str,
        cfg: &AutomationConfig,
        nctx: &NotifyCtx,
    ) {
        let mode = cfg.resolve(Some(session_id), &nctx.alias, now_secs());
        if mode == Mode::Auto {
            self.observe_ladder_only(session_id, jsonl_path, state, tip_uuid, last_message)
                .await;
            self.maybe_cross_review(
                pid,
                session_id,
                jsonl_path,
                Family::Claude,
                mode,
                matches!(state, ClaudeState::Idle),
                cfg,
                nctx,
            )
            .await;
            return;
        }
        if mode != Mode::Robot {
            return;
        }
        if !is_actionable(state) {
            return;
        }
        if !jsonl_path.is_file() {
            return;
        }
        self.maybe_cross_review(
            pid,
            session_id,
            jsonl_path,
            Family::Claude,
            mode,
            matches!(state, ClaudeState::Idle),
            cfg,
            nctx,
        )
        .await;
        if cfg.smart.enabled
            && cfg.smart.gate
            && self
                .observe_gate_claude(pid, session_id, cfg.robot.max_steps, nctx)
                .await
        {
            return;
        }
        let sig = observation_signature(jsonl_path, state, tip_uuid, last_message);
        if self.already_seen(session_id, &sig).await {
            return;
        }
        if cfg.smart.enabled
            && self
                .observe_smart(
                    pid,
                    session_id,
                    jsonl_path,
                    &sig,
                    matches!(state, ClaudeState::Idle),
                    cfg,
                    nctx,
                )
                .await
        {
            return;
        }
        let max_steps = cfg.robot.max_steps;
        let decision = {
            let mut map = self.state.lock().await;
            let entry = map.entry(session_id.to_string()).or_default();
            match step_plan(entry.steps, max_steps, entry.acted_sig.as_deref(), &sig) {
                StepPlan::Skip => return,
                StepPlan::Budget => {
                    entry.acted_sig = Some(sig.clone());
                    StepPlan::Budget
                }
                StepPlan::Act => {
                    entry.acted_sig = Some(sig.clone());
                    entry.steps += 1;
                    StepPlan::Act
                }
            }
        };
        if decision == StepPlan::Budget {
            warn!(
                target: "relay::trace", pipeline = "robot", stage = "budget",
                alias = %nctx.alias, max_steps,
                "robot step budget reached"
            );
            self.notice("robot", &format!("step budget reached ({max_steps})"), nctx)
                .await;
            return;
        }

        let context = build_context(
            &SessionSnapshot {
                alias: &nctx.alias,
                agent: "claude",
                state_label: state.label(),
                last_message,
                tail: &claude::tail_messages(jsonl_path, 6),
                question: None,
                goal: None,
            },
            CONTEXT_BUDGET,
        );

        self.clone().spawn_decision(
            pid,
            session_id.to_string(),
            jsonl_path.to_path_buf(),
            context,
            nctx.clone(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn observe_smart(
        self: &Arc<Self>,
        pid: Option<u32>,
        session_id: &str,
        jsonl_path: &Path,
        sig: &str,
        terminal_idle: bool,
        cfg: &AutomationConfig,
        nctx: &NotifyCtx,
    ) -> bool {
        let staged: StagedAssessment = match compass::assess_smart(session_id, &cfg.smart.semantic)
            .await
        {
            Ok(Some(staged)) => staged,
            Ok(None) => {
                self.clear_arm(session_id).await;
                return false;
            }
            Err(error) => {
                self.clear_arm(session_id).await;
                warn!(
                    target: "relay::trace", pipeline = "compass", stage = "semantic_failed",
                    alias = %nctx.alias, "semantic backend unavailable; using base Robot: {error}"
                );
                return false;
            }
        };
        info!(
            target: "relay::trace", pipeline = "compass", stage = "staged",
            alias = %nctx.alias, action = ?staged.action, factor = ?staged.dominant_factor,
            posterior = staged.gap_posterior, sources = staged.corroborating_sources,
            "compass staged decision"
        );
        if terminal_idle {
            match completion_disposition(staged.completion) {
                CompletionDisposition::Verified => {
                    self.mark_seen(session_id, sig, false).await;
                    let terminal_action = "robot_verified_final";
                    if terminal_receipt_path().is_some_and(|path| {
                        terminal_receipt_seen(&path, session_id, sig, terminal_action)
                    }) {
                        info!(
                            target: "relay::trace",
                            pipeline = "robot",
                            stage = "terminal_dedup",
                            alias = %nctx.alias,
                            "persistent verified-final receipt suppressed a restart replay"
                        );
                        return true;
                    }
                    self.notice(
                        terminal_action,
                        &format!(
                            "Verified {}/{} active obligations with fresh ledger evidence (coverage {:.3})",
                            staged.completion.verified,
                            staged.completion.active,
                            staged.completion.coverage,
                        ),
                        nctx,
                    )
                    .await;
                    if let Some(path) = terminal_receipt_path() {
                        if let Err(error) =
                            record_terminal_receipt(&path, session_id, sig, terminal_action)
                        {
                            warn!(
                                target: "relay::trace",
                                pipeline = "robot",
                                stage = "terminal_receipt_write_failed",
                                alias = %nctx.alias,
                                "could not persist verified-final dedup receipt: {error}"
                            );
                        }
                    }
                    return true;
                }
                CompletionDisposition::NeedsProof => {
                    return self
                        .resolve_completion_gap(
                            pid,
                            session_id,
                            sig,
                            &staged,
                            cfg.robot.max_steps,
                            nctx,
                        )
                        .await;
                }
                CompletionDisposition::Unproven => {
                    info!(
                        target: "relay::trace",
                        pipeline = "robot",
                        stage = "completion_unproven",
                        alias = %nctx.alias,
                        "no active deterministic contract; helper may advise but cannot emit a verified final"
                    );
                }
            }
        }
        match staged.action {
            PredictiveAction::Observe => {
                self.clear_arm(session_id).await;
                info!(
                    target: "relay::trace",
                    pipeline = "compass",
                    stage = "observe_fallback",
                    alias = %nctx.alias,
                    "Compass found no corrective steer; base Robot still owns terminal control"
                );
                return false;
            }
            PredictiveAction::Escalate => {
                self.clear_arm(session_id).await;
                info!(
                    target: "relay::trace",
                    pipeline = "compass",
                    stage = "escalate_fallback",
                    alias = %nctx.alias,
                    "Compass escalation is advisory; base Robot will resolve autonomously"
                );
                return false;
            }
            _ => {}
        }
        let is_steer = matches!(
            staged.action,
            PredictiveAction::MicroSteer | PredictiveAction::FullSteer
        );
        if !staged.semantic_calibrated
            && !staged.deterministic_feedback
            && !cfg.smart.semantic.allow_uncalibrated_steer
        {
            info!(
                target: "relay::trace",
                pipeline = "compass",
                stage = "shadow_uncalibrated",
                alias = %nctx.alias,
                backend = %staged.semantic_backend,
                model = %staged.semantic_model,
                "uncalibrated semantic action suppressed locally; no Telegram notification"
            );
            return false;
        }
        let plan = self
            .semantic_plan(
                session_id,
                sig,
                &staged.gap_signature,
                is_steer,
                cfg.robot.max_steps,
            )
            .await;
        match plan {
            SmartPlan::Skip => return false,
            SmartPlan::Budget => {
                return false;
            }
            SmartPlan::Arm => {
                info!(
                    target: "relay::trace", pipeline = "compass", stage = "arm",
                    alias = %nctx.alias, "compass steer armed; awaiting a distinct confirming chat frame"
                );
                return false;
            }
            SmartPlan::Act => {}
        }
        let Some(directive) = staged.directive.clone() else {
            return false;
        };
        if !steer_is_safe(&directive) {
            warn!(
                target: "relay::trace", pipeline = "compass", stage = "unsafe",
                alias = %nctx.alias, "compass directive rejected: would escalate scope or permissions"
            );
            return false;
        }
        if !cfg.smart.steer {
            info!(
                target: "relay::trace",
                pipeline = "compass",
                stage = "steer_disabled_fallback",
                alias = %nctx.alias,
                "Compass steering is disabled; base Robot retains terminal control"
            );
            return false;
        }
        let live_pid = inject::available_pid(session_id).or(pid);
        let still_actionable = match claude::read_state(jsonl_path, live_pid) {
            Ok(res) => is_actionable(&res.state),
            Err(_) => false,
        };
        if !still_actionable {
            info!(
                target: "relay::trace", pipeline = "compass", stage = "stale",
                alias = %nctx.alias, "session advanced before compass steered; skipping"
            );
            self.mark_seen(session_id, sig, false).await;
            return true;
        }
        let Some(tapped) = inject::available_pid(session_id) else {
            return false;
        };
        let directive2 = directive.clone();
        match tokio::task::spawn_blocking(move || inject::send_user_message(tapped, &directive2))
            .await
        {
            Ok(Ok(())) => {
                self.mark_seen(session_id, sig, true).await;
                let session = session_id.to_string();
                let signature = staged.gap_signature.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    compass::record_steer(&session, &signature)
                })
                .await;
                info!(
                    target: "relay::trace", pipeline = "compass", stage = "steered",
                    alias = %nctx.alias, factor = ?staged.dominant_factor,
                    "compass steered session in-band"
                );
                self.notice("compass", &format!("steered: {}", staged.explanation), nctx)
                    .await;
            }
            Ok(Err(e)) => {
                warn!(
                    target: "relay::trace", pipeline = "compass", stage = "steer_failed",
                    alias = %nctx.alias, "compass inject failed: {e}"
                )
            }
            Err(e) => {
                warn!(
                    target: "relay::trace", pipeline = "compass", stage = "steer_failed",
                    alias = %nctx.alias, "compass join failed: {e}"
                )
            }
        }
        self.already_seen(session_id, sig).await
    }

    async fn resolve_completion_gap(
        &self,
        pid: Option<u32>,
        session_id: &str,
        sig: &str,
        staged: &StagedAssessment,
        max_steps: u32,
        nctx: &NotifyCtx,
    ) -> bool {
        match self
            .reserve_completion_probe(session_id, sig, max_steps)
            .await
        {
            CompletionPlan::Skip => return true,
            CompletionPlan::SafePartial => {
                self.notice(
                    "robot_safe_partial",
                    &format!(
                        "Verification budget exhausted: {}/{} obligations verified; {} unresolved; coverage {:.3}",
                        staged.completion.verified,
                        staged.completion.active,
                        staged.completion.unresolved(),
                        staged.completion.coverage,
                    ),
                    nctx,
                )
                .await;
                return true;
            }
            CompletionPlan::Probe => {}
        }

        let Some(directive) = staged.proof_request.as_deref() else {
            self.notice(
                "robot_safe_partial",
                "Ledger is incomplete but did not produce an admissible proof request",
                nctx,
            )
            .await;
            return true;
        };
        if !steer_is_safe(directive) {
            self.notice(
                "robot_safe_partial",
                "Ledger proof request failed the non-escalation safety guard",
                nctx,
            )
            .await;
            return true;
        }
        let Some(tapped) = inject::available_pid(session_id).or(pid) else {
            self.notice(
                "robot_safe_partial",
                "Ledger proof is missing and the session has no live injection channel",
                nctx,
            )
            .await;
            return true;
        };
        let text = directive.to_string();
        match tokio::task::spawn_blocking(move || inject::send_user_message(tapped, &text)).await {
            Ok(Ok(())) => {
                info!(
                    target: "relay::trace",
                    pipeline = "robot",
                    stage = "completion_probe",
                    alias = %nctx.alias,
                    obligation = staged.obligation_id.as_deref().unwrap_or("unknown"),
                    verified = staged.completion.verified,
                    active = staged.completion.active,
                    "Robot injected a bounded Ledger verification probe"
                );
                self.notice(
                    "robot_completion_probe",
                    &format!(
                        "Ledger requires fresh proof for {}; {}/{} obligations verified",
                        staged
                            .obligation_id
                            .as_deref()
                            .unwrap_or("the active contract"),
                        staged.completion.verified,
                        staged.completion.active,
                    ),
                    nctx,
                )
                .await;
            }
            Ok(Err(error)) => {
                warn!(
                    target: "relay::trace",
                    pipeline = "robot",
                    stage = "completion_probe_failed",
                    alias = %nctx.alias,
                    "Ledger proof injection failed: {error}"
                );
                self.notice(
                    "robot_safe_partial",
                    "Ledger proof is missing and its bounded probe could not be delivered",
                    nctx,
                )
                .await;
            }
            Err(error) => {
                warn!(
                    target: "relay::trace",
                    pipeline = "robot",
                    stage = "completion_probe_failed",
                    alias = %nctx.alias,
                    "Ledger proof injection task failed: {error}"
                );
                self.notice(
                    "robot_safe_partial",
                    "Ledger proof is missing and its bounded probe could not be started",
                    nctx,
                )
                .await;
            }
        }
        true
    }

    async fn reserve_completion_probe(
        &self,
        session_id: &str,
        sig: &str,
        max_steps: u32,
    ) -> CompletionPlan {
        let mut map = self.state.lock().await;
        let entry = map.entry(session_id.to_string()).or_default();
        if entry.acted_sig.as_deref() == Some(sig) {
            return CompletionPlan::Skip;
        }
        entry.acted_sig = Some(sig.to_string());
        if entry.completion_probe_steps >= 3 || entry.steps >= max_steps {
            return CompletionPlan::SafePartial;
        }
        entry.completion_probe_steps = entry.completion_probe_steps.saturating_add(1);
        entry.steps = entry.steps.saturating_add(1);
        CompletionPlan::Probe
    }

    async fn semantic_plan(
        &self,
        session_id: &str,
        sig: &str,
        gap_signature: &str,
        is_steer: bool,
        max_steps: u32,
    ) -> SmartPlan {
        {
            let mut map = self.state.lock().await;
            let entry = map.entry(session_id.to_string()).or_default();
            if entry.acted_sig.as_deref() == Some(sig) {
                SmartPlan::Skip
            } else if entry.steps >= max_steps {
                SmartPlan::Budget
            } else if is_steer {
                match (entry.armed_gap.as_deref(), entry.armed_tip.as_deref()) {
                    (Some(gap), Some(tip)) if gap == gap_signature && tip != sig => SmartPlan::Act,
                    (Some(gap), Some(tip)) if gap == gap_signature && tip == sig => SmartPlan::Skip,
                    _ => {
                        entry.armed_gap = Some(gap_signature.to_string());
                        entry.armed_tip = Some(sig.to_string());
                        SmartPlan::Arm
                    }
                }
            } else {
                entry.armed_gap = None;
                entry.armed_tip = None;
                SmartPlan::Act
            }
        }
    }

    pub async fn observe_codex(
        self: &Arc<Self>,
        thread_id: &str,
        rollout_path: &Path,
        state: &CodexState,
        last_message: &str,
        cfg: &AutomationConfig,
        nctx: &NotifyCtx,
    ) {
        let codex_mode = cfg.resolve(Some(thread_id), &nctx.alias, now_secs());
        if matches!(codex_mode, Mode::Auto | Mode::Robot) {
            self.maybe_cross_review(
                None,
                thread_id,
                rollout_path,
                Family::Codex,
                codex_mode,
                matches!(state, CodexState::Idle),
                cfg,
                nctx,
            )
            .await;
        }
        if codex_mode != Mode::Robot
            || !matches!(
                state,
                CodexState::Idle | CodexState::NeedsReplyMaybe { .. } | CodexState::Error { .. }
            )
            || !cfg.smart.enabled
        {
            return;
        }
        if cfg.smart.gate
            && self
                .observe_gate_codex(thread_id, cfg.robot.max_steps, nctx)
                .await
        {
            return;
        }
        let sig = observation_signature(rollout_path, &ClaudeState::Idle, None, last_message);
        if self.already_seen(thread_id, &sig).await {
            return;
        }
        let staged = match compass::assess_codex_smart(
            thread_id,
            rollout_path,
            state,
            &cfg.smart.semantic,
        )
        .await
        {
            Ok(Some(staged)) => staged,
            Ok(None) => {
                self.clear_arm(thread_id).await;
                return;
            }
            Err(error) => {
                self.clear_arm(thread_id).await;
                warn!(target: "relay::trace", pipeline = "compass", stage = "codex_semantic_failed", alias = %nctx.alias, "{error}");
                return;
            }
        };
        if staged.action == PredictiveAction::Observe {
            self.clear_arm(thread_id).await;
            self.mark_seen(thread_id, &sig, false).await;
            return;
        }
        if staged.action == PredictiveAction::Escalate {
            self.clear_arm(thread_id).await;
            self.notice(
                "compass",
                &format!("Codex: escalate to you: {}", staged.explanation),
                nctx,
            )
            .await;
            return;
        }
        if !staged.semantic_calibrated
            && !staged.deterministic_feedback
            && !cfg.smart.semantic.allow_uncalibrated_steer
        {
            self.mark_seen(thread_id, &sig, false).await;
            info!(
                target: "relay::trace",
                pipeline = "compass",
                stage = "codex_shadow_uncalibrated",
                alias = %nctx.alias,
                backend = %staged.semantic_backend,
                model = %staged.semantic_model,
                "Codex uncalibrated semantic action suppressed locally; no Telegram notification"
            );
            return;
        }
        let is_steer = matches!(
            staged.action,
            PredictiveAction::MicroSteer | PredictiveAction::FullSteer
        );
        match self
            .semantic_plan(
                thread_id,
                &sig,
                &staged.gap_signature,
                is_steer,
                cfg.robot.max_steps,
            )
            .await
        {
            SmartPlan::Skip => return,
            SmartPlan::Arm => {
                self.mark_seen(thread_id, &sig, false).await;
                return;
            }
            SmartPlan::Budget => {
                self.notice("compass", "Codex steer budget reached", nctx)
                    .await;
                return;
            }
            SmartPlan::Act => {}
        }
        let Some(directive) = staged.directive.clone() else {
            self.mark_seen(thread_id, &sig, false).await;
            return;
        };
        if !steer_is_safe(&directive) || !cfg.smart.steer {
            self.mark_seen(thread_id, &sig, false).await;
            self.notice("compass", &format!("Codex would steer: {directive}"), nctx)
                .await;
            return;
        }
        let still_actionable = std::fs::read_to_string(rollout_path)
            .ok()
            .map(|text| relay_core::state::reduce_codex(&text).state)
            .map(|state| {
                matches!(
                    state,
                    CodexState::Idle
                        | CodexState::NeedsReplyMaybe { .. }
                        | CodexState::Error { .. }
                )
            })
            .unwrap_or(false);
        if !still_actionable {
            self.mark_seen(thread_id, &sig, false).await;
            return;
        }
        let control = Arc::clone(&self.control);
        let alias = nctx.alias.clone();
        let directive2 = directive.clone();
        match tokio::task::spawn_blocking(move || {
            control.send_prompt(&alias, AgentKind::Codex, &directive2)
        })
        .await
        {
            Ok(Ok(())) => {
                self.mark_seen(thread_id, &sig, true).await;
                let session = thread_id.to_string();
                let signature = staged.gap_signature.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    compass::record_steer(&session, &signature)
                })
                .await;
                self.notice(
                    "compass",
                    &format!("steered Codex: {}", staged.explanation),
                    nctx,
                )
                .await;
            }
            Ok(Err(error)) => {
                self.mark_seen(thread_id, &sig, false).await;
                warn!(target: "relay::trace", pipeline = "compass", stage = "codex_steer_failed", alias = %nctx.alias, "{error}")
            }
            Err(error) => {
                self.mark_seen(thread_id, &sig, false).await;
                warn!(target: "relay::trace", pipeline = "compass", stage = "codex_steer_join_failed", alias = %nctx.alias, "{error}")
            }
        }
    }

    async fn clear_arm(&self, session_id: &str) {
        if let Some(entry) = self.state.lock().await.get_mut(session_id) {
            entry.armed_gap = None;
            entry.armed_tip = None;
        }
    }

    async fn reserve_gate_step(&self, session_id: &str, text: &str, max_steps: u32) -> bool {
        let signature = blake3::hash(text.as_bytes()).to_hex().as_str()[..16].to_string();
        let mut map = self.state.lock().await;
        let entry = map.entry(session_id.to_string()).or_default();
        if entry.acted_sig.as_deref() == Some(&signature) || entry.steps >= max_steps {
            return false;
        }
        entry.acted_sig = Some(signature);
        entry.steps += 1;
        true
    }

    async fn charge_gate_probes(&self, session_id: &str, probe_steps: u8, max_steps: u32) -> bool {
        let mut map = self.state.lock().await;
        let entry = map.entry(session_id.to_string()).or_default();
        let delta = probe_steps.saturating_sub(entry.gate_probe_steps_charged) as u32;
        entry.gate_probe_steps_charged = probe_steps;
        entry.steps = entry.steps.saturating_add(delta);
        entry.steps <= max_steps
    }

    async fn observe_gate_claude(
        &self,
        pid: Option<u32>,
        session_id: &str,
        max_steps: u32,
        nctx: &NotifyCtx,
    ) -> bool {
        let advance_session = session_id.to_string();
        match tokio::task::spawn_blocking(move || {
            crate::gate_controller::advance_robot_probe(&advance_session)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "controller_probe_failed_open", "{error}");
                return false;
            }
            Err(error) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "controller_probe_join_failed", "{error}");
                return false;
            }
        }
        let session = session_id.to_string();
        let control = match tokio::task::spawn_blocking(move || {
            crate::artifact_store::robot_gate_control(&session)
        })
        .await
        {
            Ok(Ok(control)) => control,
            Ok(Err(error)) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "failed_open", "{error}");
                return false;
            }
            Err(error) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "join_failed", "{error}");
                return false;
            }
        };
        if !control.active {
            return false;
        }
        if !self
            .charge_gate_probes(session_id, control.probe_steps, max_steps)
            .await
        {
            self.notice(
                "robot_safe_partial",
                "SafeBlocked: Robot step budget exhausted during proof ladder",
                nctx,
            )
            .await;
            return true;
        }
        let Some(directive) = control.directive else {
            return true;
        };
        if control.terminal {
            self.notice("robot_safe_partial", &directive, nctx).await;
            return true;
        }
        if !self
            .reserve_gate_step(session_id, &directive, max_steps)
            .await
        {
            return true;
        }
        let Some(tapped) = inject::available_pid(session_id).or(pid) else {
            self.notice(
                "robot_gate",
                "autonomous probe is active but the session is not tapped",
                nctx,
            )
            .await;
            return true;
        };
        let text = directive.clone();
        let control_state = control.state;
        match tokio::task::spawn_blocking(move || inject::send_user_message(tapped, &text)).await {
            Ok(Ok(())) => {
                if matches!(
                    control_state,
                    Some(
                        GateState::AwaitingProof
                            | GateState::TargetProbe
                            | GateState::ControllerProbe
                    )
                ) {
                    crate::gate_controller::mark_target_probe_delivered(session_id);
                } else if control_state == Some(GateState::RejectedPresent) {
                    let delivered_session = session_id.to_string();
                    if let Err(error) = tokio::task::spawn_blocking(move || {
                        crate::artifact_store::mark_replan_delivered(&delivered_session)
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.into()))
                    {
                        warn!(target: "relay::trace", pipeline = "robot_gate", stage = "replan_release_failed", "{error}");
                    }
                }
                self.notice("robot_gate", &directive, nctx).await
            }
            Ok(Err(error)) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "drive_failed", "{error}")
            }
            Err(error) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "join_failed", "{error}")
            }
        }
        true
    }

    async fn observe_gate_codex(&self, session_id: &str, max_steps: u32, nctx: &NotifyCtx) -> bool {
        let advance_session = session_id.to_string();
        match tokio::task::spawn_blocking(move || {
            crate::gate_controller::advance_robot_probe(&advance_session)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "controller_probe_failed_open", "{error}");
                return false;
            }
            Err(error) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "controller_probe_join_failed", "{error}");
                return false;
            }
        }
        let session = session_id.to_string();
        let control = match tokio::task::spawn_blocking(move || {
            crate::artifact_store::robot_gate_control(&session)
        })
        .await
        {
            Ok(Ok(control)) => control,
            Ok(Err(error)) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "failed_open", "{error}");
                return false;
            }
            Err(error) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "join_failed", "{error}");
                return false;
            }
        };
        if !control.active {
            return false;
        }
        if !self
            .charge_gate_probes(session_id, control.probe_steps, max_steps)
            .await
        {
            self.notice(
                "robot_safe_partial",
                "SafeBlocked: Robot step budget exhausted during proof ladder",
                nctx,
            )
            .await;
            return true;
        }
        let Some(directive) = control.directive else {
            return true;
        };
        if control.terminal {
            self.notice("robot_safe_partial", &directive, nctx).await;
            return true;
        }
        if !self
            .reserve_gate_step(session_id, &directive, max_steps)
            .await
        {
            return true;
        }
        let platform = Arc::clone(&self.control);
        let alias = nctx.alias.clone();
        let text = directive.clone();
        let control_state = control.state;
        match tokio::task::spawn_blocking(move || {
            platform.send_prompt(&alias, AgentKind::Codex, &text)
        })
        .await
        {
            Ok(Ok(())) => {
                if matches!(
                    control_state,
                    Some(
                        GateState::AwaitingProof
                            | GateState::TargetProbe
                            | GateState::ControllerProbe
                    )
                ) {
                    crate::gate_controller::mark_target_probe_delivered(session_id);
                } else if control_state == Some(GateState::RejectedPresent) {
                    let delivered_session = session_id.to_string();
                    if let Err(error) = tokio::task::spawn_blocking(move || {
                        crate::artifact_store::mark_replan_delivered(&delivered_session)
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.into()))
                    {
                        warn!(target: "relay::trace", pipeline = "robot_gate", stage = "replan_release_failed", "{error}");
                    }
                }
                self.notice("robot_gate", &directive, nctx).await
            }
            Ok(Err(error)) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "drive_failed", "{error}")
            }
            Err(error) => {
                warn!(target: "relay::trace", pipeline = "robot_gate", stage = "join_failed", "{error}")
            }
        }
        true
    }

    async fn mark_seen(&self, session_id: &str, sig: &str, count_step: bool) {
        let mut map = self.state.lock().await;
        let entry = map.entry(session_id.to_string()).or_default();
        entry.acted_sig = Some(sig.to_string());
        entry.armed_gap = None;
        entry.armed_tip = None;
        if count_step {
            entry.steps += 1;
        }
    }

    async fn observe_ladder_only(
        self: &Arc<Self>,
        session_id: &str,
        jsonl_path: &Path,
        state: &ClaudeState,
        tip_uuid: Option<&str>,
        last_message: &str,
    ) {
        if !matches!(state, ClaudeState::Idle) || !jsonl_path.is_file() {
            return;
        }
        let sig = observation_signature(jsonl_path, state, tip_uuid, last_message);
        let key = format!("auto-ladder:{session_id}");
        if self.already_seen(&key, &sig).await {
            return;
        }
        {
            let mut map = self.state.lock().await;
            map.entry(key).or_default().acted_sig = Some(sig);
        }
        let weight = std::fs::metadata(jsonl_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let read_path = jsonl_path.to_path_buf();
        let Ok(steps) = tokio::task::spawn_blocking(move || {
            claude::semantic_steps_window(&read_path, LADDER_HEAD_BYTES, LADDER_TAIL_BYTES)
        })
        .await
        else {
            return;
        };
        let truncated = weight > LADDER_HEAD_BYTES + LADDER_TAIL_BYTES;
        let input = relay_compass::contract_input_from_steps(&steps);
        if input.anchor.is_none() {
            crate::decision_log::record(
                "robot",
                "ladder_unanchored",
                serde_json::json!({"kind": "completion", "steps": steps.len(),
                "bytes": weight, "truncated": truncated,
                "reason": "no contract anchor in the readable window"}),
            );
            return;
        }
        let ledger = relay_compass::build_contract_ledger_from_input(&steps, &[], &input);
        let snapshot = CompletionSnapshot::from_ledger(&ledger);
        let disposition = completion_disposition(snapshot);
        crate::decision_log::record(
            "robot",
            match disposition {
                CompletionDisposition::Verified => "would_verify_final",
                CompletionDisposition::NeedsProof => "would_need_proof",
                CompletionDisposition::Unproven => "would_be_unproven",
            },
            serde_json::json!({
                "kind": "completion",
                "observed_only": true,
                "truncated": truncated,
                "active": snapshot.active,
                "verified": snapshot.verified,
                "unresolved": snapshot.unresolved(),
                "coverage": snapshot.coverage,
                "layer_mismatches": snapshot.layer_mismatches,
                "scope_gap": snapshot.completion_scope_gap,
                "proof_deficit": snapshot.proof_deficit,
                "dossier_label": snapshot.status_label(),
                "reason": "ladder observed in Auto; it did not act",
            }),
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn maybe_cross_review(
        self: &Arc<Self>,
        pid: Option<u32>,
        session_id: &str,
        transcript: &Path,
        family: Family,
        mode: Mode,
        terminal_idle: bool,
        cfg: &AutomationConfig,
        nctx: &NotifyCtx,
    ) {
        if !terminal_idle || !transcript.is_file() {
            return;
        }
        let Some(state_path) = review::review_state_path() else {
            return;
        };
        let mark = review::review_mark(&state_path, session_id);
        let last = (mark.last_review_at > 0).then_some(mark.last_review_at);
        let plan = review::plan_review(&cfg.review, family, mark.reviews_done, last, now_secs());
        if plan != ReviewPlan::Run {
            if plan != ReviewPlan::Disabled && plan != ReviewPlan::TooSoon {
                crate::decision_log::record(
                    "review",
                    "held",
                    serde_json::json!({
                        "alias": nctx.alias,
                        "family": family.id(),
                        "reason": match plan {
                            ReviewPlan::NoReviewer => "no cross-family reviewer is enabled",
                            ReviewPlan::BudgetSpent => "review budget for this session is spent",
                            _ => "held",
                        },
                    }),
                );
            }
            return;
        }
        if review::record_review(&state_path, session_id, now_secs()).is_err() {
            return;
        }
        self.clone().spawn_cross_review(
            pid,
            session_id.to_string(),
            transcript.to_path_buf(),
            family,
            mode,
            cfg.review.clone(),
            cfg.robot.providers.clone(),
            nctx.clone(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_cross_review(
        self: Arc<Self>,
        pid: Option<u32>,
        session_id: String,
        transcript: PathBuf,
        family: Family,
        mode: Mode,
        rules: crate::automation::ReviewRules,
        shared: crate::automation::Providers,
        nctx: NotifyCtx,
    ) {
        tokio::spawn(async move {
            let depth = rules.depth;
            let read_path = transcript.clone();
            let Ok((goal, tail)) = tokio::task::spawn_blocking(move || {
                review::session_view(family, &read_path, depth)
            })
            .await
            else {
                return;
            };
            if tail.is_empty() {
                return;
            }
            let prompt = review::review_prompt(family, goal.as_deref(), &tail, depth);
            let providers = review::reviewer_providers(family, &rules, &shared);
            let discovered = discover::discover_all().await;
            let outcome = review::ask_reviewer(
                &self.pool,
                &session_id,
                &providers,
                &discovered,
                &prompt,
                now_secs(),
            )
            .await;
            let (backend, review) = match outcome {
                Ok(pair) => pair,
                Err(error) => {
                    warn!(
                        target: "relay::trace", pipeline = "review", stage = "unavailable",
                        alias = %nctx.alias, "cross review produced no verdict: {error}"
                    );
                    crate::decision_log::record(
                        "review",
                        "unavailable",
                        serde_json::json!({
                            "alias": nctx.alias,
                            "family": family.id(),
                            "reason": error.to_string(),
                        }),
                    );
                    return;
                }
            };
            info!(
                target: "relay::trace", pipeline = "review", stage = "verdict",
                alias = %nctx.alias, reviewer = backend.id(),
                reviewed = family.id(), verdict = review.verdict.label(),
                "cross review verdict"
            );
            crate::decision_log::record(
                "review",
                review.verdict.label(),
                serde_json::json!({
                    "alias": nctx.alias,
                    "reviewer": backend.id(),
                    "reviewed": family.id(),
                    "depth": depth.label(),
                    "goal_known": goal.is_some(),
                    "goal_restated": review.goal_restated,
                    "gaps": review.gaps,
                    "confidence": review.confidence,
                    "has_correction": review.correction.is_some(),
                    "mode": mode.label(),
                }),
            );
            self.notice(
                "cross_review",
                &format!("{} says: {}", backend.id(), review.headline()),
                &nctx,
            )
            .await;
            let Some(correction) = review.correction.clone() else {
                return;
            };
            if mode != Mode::Robot || !rules.steer {
                crate::decision_log::record(
                    "review",
                    "correction_withheld",
                    serde_json::json!({
                        "alias": nctx.alias,
                        "reviewer": backend.id(),
                        "mode": mode.label(),
                        "reason": if mode == Mode::Robot {
                            "review steering is off"
                        } else {
                            "Auto observes reviews; only Robot may act on them"
                        },
                    }),
                );
                return;
            }
            if !steer_is_safe(&correction) {
                warn!(
                    target: "relay::trace", pipeline = "review", stage = "unsafe",
                    alias = %nctx.alias,
                    "reviewer correction rejected: it would widen scope or permissions"
                );
                crate::decision_log::record(
                    "review",
                    "correction_unsafe",
                    serde_json::json!({"alias": nctx.alias, "reviewer": backend.id()}),
                );
                return;
            }
            let text = correction.clone();
            let delivery = if family == Family::Codex {
                let control = Arc::clone(&self.control);
                let alias = nctx.alias.clone();
                tokio::task::spawn_blocking(move || {
                    control.send_prompt(&alias, AgentKind::Codex, &text)
                })
                .await
            } else {
                let Some(tapped) = inject::available_pid(&session_id).or(pid) else {
                    crate::decision_log::record(
                        "review",
                        "correction_undeliverable",
                        serde_json::json!({
                            "alias": nctx.alias,
                            "reason": "the session has no live injection channel",
                        }),
                    );
                    return;
                };
                tokio::task::spawn_blocking(move || inject::send_user_message(tapped, &text)).await
            };
            match delivery {
                Ok(Ok(())) => {
                    info!(
                        target: "relay::trace", pipeline = "review", stage = "corrected",
                        alias = %nctx.alias, reviewer = backend.id(),
                        "cross review corrected the session in-band"
                    );
                    crate::decision_log::record(
                        "review",
                        "corrected",
                        serde_json::json!({"alias": nctx.alias, "reviewer": backend.id()}),
                    );
                    self.notice(
                        "cross_review_correction",
                        &format!("{}: {correction}", backend.id()),
                        &nctx,
                    )
                    .await;
                }
                Ok(Err(error)) => warn!(
                    target: "relay::trace", pipeline = "review", stage = "correction_failed",
                    alias = %nctx.alias, "cross review inject failed: {error}"
                ),
                Err(error) => warn!(
                    target: "relay::trace", pipeline = "review", stage = "correction_failed",
                    alias = %nctx.alias, "cross review join failed: {error}"
                ),
            }
        });
    }

    async fn already_seen(&self, session_id: &str, sig: &str) -> bool {
        self.state
            .lock()
            .await
            .get(session_id)
            .and_then(|entry| entry.acted_sig.as_deref())
            == Some(sig)
    }

    fn spawn_decision(
        self: Arc<Self>,
        pid: Option<u32>,
        session_id: String,
        jsonl_path: PathBuf,
        context: String,
        nctx: NotifyCtx,
    ) {
        tokio::spawn(async move {
            let cfg = AutomationConfig::load();
            let discovered = discover::discover_all().await;
            let d = match self
                .pool
                .ask(
                    &session_id,
                    &cfg.robot.providers,
                    &discovered,
                    SUPERVISOR_SYSTEM,
                    &context,
                    now_secs(),
                )
                .await
            {
                Ok((_, d)) => d,
                Err(e) => {
                    warn!(
                        target: "relay::trace", pipeline = "robot", stage = "no_decision",
                        alias = %nctx.alias, "supervisor produced no decision: {e}"
                    );
                    self.notice("robot", "no reachable supervisor provider", &nctx)
                        .await;
                    return;
                }
            };
            self.execute(pid, &session_id, &jsonl_path, &d, &nctx).await;
        });
    }

    async fn execute(
        &self,
        pid: Option<u32>,
        session_id: &str,
        jsonl_path: &Path,
        d: &Decision,
        nctx: &NotifyCtx,
    ) {
        let text = drive_text(d);
        info!(
            target: "relay::trace", pipeline = "robot", stage = "decision",
            alias = %nctx.alias, action = d.action.label(),
            "robot decision"
        );
        let Some(text) = text else {
            let action = terminal_event_action(d.action);
            self.notice(action, &format!("{}: {}", d.action.label(), d.reason), nctx)
                .await;
            return;
        };

        let live_pid = inject::available_pid(session_id).or(pid);
        let still_actionable = match claude::read_state(jsonl_path, live_pid) {
            Ok(res) => is_actionable(&res.state),
            Err(_) => false,
        };
        if !still_actionable {
            info!(
                target: "relay::trace", pipeline = "robot", stage = "stale",
                alias = %nctx.alias, "session advanced before robot acted; skipping"
            );
            return;
        }
        let Some(tapped) = inject::available_pid(session_id) else {
            self.notice("robot", "session not tapped; cannot drive in-band", nctx)
                .await;
            return;
        };
        let text2 = text.clone();
        match tokio::task::spawn_blocking(move || inject::send_user_message(tapped, &text2)).await {
            Ok(Ok(())) => {
                info!(
                    target: "relay::trace", pipeline = "robot", stage = "drove",
                    alias = %nctx.alias, action = d.action.label(),
                    "robot drove session in-band"
                );
                self.notice(
                    "robot",
                    &format!("{}: {}", d.action.label(), d.reason),
                    nctx,
                )
                .await;
            }
            Ok(Err(e)) => warn!(
                target: "relay::trace", pipeline = "robot", stage = "drive_failed",
                alias = %nctx.alias, "robot inject failed: {e}"
            ),
            Err(e) => warn!(
                target: "relay::trace", pipeline = "robot", stage = "drive_failed",
                alias = %nctx.alias, "robot join failed: {e}"
            ),
        }
    }

    async fn notice(&self, action: &str, detail: &str, nctx: &NotifyCtx) {
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

    fn completion(
        active: usize,
        open: usize,
        claimed_unverified: usize,
        verified: usize,
        coverage: f32,
    ) -> CompletionSnapshot {
        CompletionSnapshot {
            active,
            open,
            claimed_unverified,
            verified,
            contradicted: 0,
            disputed: 0,
            stale: 0,
            layer_mismatches: 0,
            coverage,
            completion_scope_gap: false,
            proof_deficit: false,
            current_claimed_complete: false,
        }
    }

    #[test]
    fn step_plan_skips_same_sig() {
        assert_eq!(step_plan(0, 20, Some("s"), "s"), StepPlan::Skip);
    }

    #[test]
    fn step_plan_acts_then_budgets() {
        assert_eq!(step_plan(0, 2, None, "s"), StepPlan::Act);
        assert_eq!(step_plan(2, 2, Some("old"), "new"), StepPlan::Budget);
        assert_eq!(step_plan(1, 2, Some("old"), "new"), StepPlan::Act);
    }

    #[test]
    fn actionable_states() {
        assert!(is_actionable(&ClaudeState::Idle));
        assert!(is_actionable(&ClaudeState::Error {
            message: "x".into()
        }));
        assert!(!is_actionable(&ClaudeState::Working { open_tools: 1 }));
        assert!(!is_actionable(&ClaudeState::SubagentRunning));
    }

    fn dec(action: Action, message: Option<&str>) -> Decision {
        Decision {
            action,
            message: message.map(str::to_string),
            option_index: None,
            wait_seconds: None,
            reason: "r".into(),
            confidence: None,
        }
    }

    #[test]
    fn steer_rejects_escalation_directives() {
        assert!(steer_is_safe(
            "Run the real check for users.sql at the required layer and report the output."
        ));
        assert!(!steer_is_safe("sudo rm -rf / to fix it"));
        assert!(!steer_is_safe("bypass the sandbox and continue"));
        assert!(!steer_is_safe("git push --no-verify to skip the hook"));
    }

    #[test]
    fn drive_text_maps_actions() {
        assert_eq!(
            drive_text(&dec(Action::Continue, None)).as_deref(),
            Some("continue")
        );
        assert_eq!(
            drive_text(&dec(Action::Retry, None)).as_deref(),
            Some("continue")
        );
        assert_eq!(
            drive_text(&dec(Action::Feedback, Some("fix the test"))).as_deref(),
            Some("fix the test")
        );
        assert_eq!(
            drive_text(&dec(Action::Feedback, Some("  "))).as_deref(),
            Some("continue")
        );
        assert!(drive_text(&dec(Action::Stop, None)).is_none());
        assert!(drive_text(&dec(Action::Wait, None)).is_none());
        assert!(drive_text(&dec(Action::AcceptPlan, None)).is_none());
    }

    #[test]
    fn helper_stop_is_only_a_claim_not_a_verified_final() {
        assert_eq!(
            terminal_event_action(Action::Stop),
            "robot_claimed_complete"
        );
        assert_eq!(terminal_event_action(Action::Wait), "robot");
        assert_ne!(terminal_event_action(Action::Stop), "robot_verified_final");
    }

    #[test]
    fn flowchart_like_open_contract_requires_proof() {
        let snapshot = completion(1, 1, 0, 0, 0.0);
        assert_eq!(
            completion_disposition(snapshot),
            CompletionDisposition::NeedsProof
        );
    }

    #[test]
    fn only_full_fresh_ledger_coverage_can_verify() {
        let verified = completion(2, 0, 0, 2, 1.0);
        assert_eq!(
            completion_disposition(verified),
            CompletionDisposition::Verified
        );

        let mut stale = verified;
        stale.stale = 1;
        assert_eq!(
            completion_disposition(stale),
            CompletionDisposition::NeedsProof
        );

        let mut scope_gap = verified;
        scope_gap.completion_scope_gap = true;
        assert_eq!(
            completion_disposition(scope_gap),
            CompletionDisposition::NeedsProof
        );

        let mut layer_gap = verified;
        layer_gap.layer_mismatches = 1;
        assert_eq!(
            completion_disposition(layer_gap),
            CompletionDisposition::NeedsProof
        );
    }

    #[test]
    fn no_deterministic_contract_cannot_be_verified() {
        assert_eq!(
            completion_disposition(completion(0, 0, 0, 0, 0.0)),
            CompletionDisposition::Unproven
        );
    }

    #[test]
    fn the_dossier_label_and_the_runtime_verdict_never_disagree() {
        let snapshot = |active, verified, claimed_unverified, coverage| CompletionSnapshot {
            active,
            open: 0,
            claimed_unverified,
            verified,
            contradicted: 0,
            disputed: 0,
            stale: 0,
            layer_mismatches: 0,
            coverage,
            completion_scope_gap: false,
            proof_deficit: false,
            current_claimed_complete: false,
        };
        for snap in [
            snapshot(2, 2, 0, 1.0),
            snapshot(27, 0, 1, 0.19),
            snapshot(3, 1, 0, 0.5),
            snapshot(0, 0, 0, 0.0),
        ] {
            let expected = match completion_disposition(snap) {
                CompletionDisposition::Verified => "verified_final",
                CompletionDisposition::NeedsProof => "needs_proof",
                CompletionDisposition::Unproven => "unproven",
            };
            assert_eq!(
                snap.status_label(),
                expected,
                "the file and the verdict must name the same thing for {snap:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_auto_observer_keeps_its_own_dedup_and_never_spends_the_robot_budget() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let robot = Robot::new(tx);
        let session = "sid-1";

        {
            let mut map = robot.state.lock().await;
            let entry = map.entry(format!("auto-ladder:{session}")).or_default();
            entry.acted_sig = Some("signature".to_string());
        }

        assert!(
            robot
                .already_seen(&format!("auto-ladder:{session}"), "signature")
                .await,
            "the observer remembers what it has already looked at"
        );
        assert!(
            !robot.already_seen(session, "signature").await,
            "observing in Auto must leave the Robot budget for that session untouched"
        );
        assert_eq!(
            robot
                .state
                .lock()
                .await
                .get(session)
                .map(|entry| entry.steps),
            None,
            "the observer never opens a step budget for the session it watched"
        );
    }

    #[tokio::test]
    async fn completion_probe_is_bounded_and_never_repeats_a_signature() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let robot = Robot::new(tx);
        assert_eq!(
            robot.reserve_completion_probe("sid", "s1", 20).await,
            CompletionPlan::Probe
        );
        assert_eq!(
            robot.reserve_completion_probe("sid", "s1", 20).await,
            CompletionPlan::Skip
        );
        assert_eq!(
            robot.reserve_completion_probe("sid", "s2", 20).await,
            CompletionPlan::Probe
        );
        assert_eq!(
            robot.reserve_completion_probe("sid", "s3", 20).await,
            CompletionPlan::Probe
        );
        assert_eq!(
            robot.reserve_completion_probe("sid", "s4", 20).await,
            CompletionPlan::SafePartial
        );
    }

    #[tokio::test]
    async fn semantic_steer_requires_a_distinct_confirming_frame() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let robot = Robot::new(tx);

        assert_eq!(
            robot
                .semantic_plan("sid", "frame-1", "same-gap", true, 20)
                .await,
            SmartPlan::Arm
        );
        assert_eq!(
            robot
                .semantic_plan("sid", "frame-1", "same-gap", true, 20)
                .await,
            SmartPlan::Skip
        );
        assert_eq!(
            robot
                .semantic_plan("sid", "frame-2", "same-gap", true, 20)
                .await,
            SmartPlan::Act
        );
    }

    #[tokio::test]
    async fn semantic_non_steer_action_does_not_need_hysteresis() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let robot = Robot::new(tx);

        assert_eq!(
            robot
                .semantic_plan("sid", "frame-1", "ask-gap", false, 20)
                .await,
            SmartPlan::Act
        );
    }

    #[test]
    fn terminal_receipt_is_restart_persistent_and_redacts_session_id() {
        let path = std::env::temp_dir().join(format!(
            "vsc-relay-terminal-receipt-{}-{}.json",
            std::process::id(),
            now_secs()
        ));
        let session_id = "sensitive-native-session-id";
        let action = "robot_verified_final";

        assert!(!terminal_receipt_seen(&path, session_id, "sig-1", action));
        record_terminal_receipt(&path, session_id, "sig-1", action).unwrap();
        assert!(terminal_receipt_seen(&path, session_id, "sig-1", action));
        assert!(!terminal_receipt_seen(&path, session_id, "sig-2", action));

        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(!persisted.contains(session_id));
        assert!(persisted.contains("sig-1"));
        let _ = std::fs::remove_file(path);
    }
}
