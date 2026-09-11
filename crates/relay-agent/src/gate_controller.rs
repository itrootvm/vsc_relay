use crate::artifact_store;
use crate::automation::{AutomationConfig, Mode};
use crate::tool_effect;
use anyhow::{Context, Result};
use relay_compass::{
    build_contract_ledger_from_input, contract_input_from_steps, CoverageStatus, GateDecision,
    GateProof, GateState, SemanticStep, ToolEffect,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const AUTO_PROBE_LIMIT: u8 = 3;
const AUTO_TIMEOUT_SECS: i64 = 110;
const CENSUS_WINDOW_SECS: i64 = 300;

fn digest_tag(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

#[derive(Default)]
struct Counts {
    pre: u64,
    unknown_shape: u64,
    compatibility: u64,
    segmented: u64,
    structured: u64,
    unbound: u64,
    bypass_unknown_unbound: u64,
    post: u64,
    post_unbound_bypass: u64,
}

#[derive(Default)]
struct Census {
    opened_at: i64,
    tools: HashMap<String, Counts>,
}

fn census() -> &'static Mutex<Census> {
    static CENSUS: OnceLock<Mutex<Census>> = OnceLock::new();
    CENSUS.get_or_init(|| Mutex::new(Census::default()))
}

fn note_unreadable_shell(payload: &Value, input: &Value) {
    if payload.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return;
    }
    let Some(command) = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let head = command
        .split_whitespace()
        .find(|token| !token.contains('='))
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    crate::decision_log::record(
        "gate",
        "unreadable",
        serde_json::json!({
            "tool": "Bash",
            "head": head.chars().take(24).collect::<String>(),
            "heredoc": command.contains("<<"),
            "multiline": command.contains('\n'),
            "reason": "shell command could not be classified",
        }),
    );
}

fn tool_of(payload: &Value) -> String {
    payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn with_census(tool: String, edit: impl FnOnce(&mut Counts)) {
    let now = crate::automation::now_secs();
    let due = {
        let mut census = match census().lock() {
            Ok(census) => census,
            Err(poisoned) => poisoned.into_inner(),
        };
        if census.opened_at == 0 {
            census.opened_at = now;
        }
        edit(census.tools.entry(tool).or_default());
        if now - census.opened_at < CENSUS_WINDOW_SECS || census.tools.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *census))
        }
    };
    let Some(window) = due else { return };
    let elapsed = now - window.opened_at;
    let mut tools: Vec<(String, Counts)> = window.tools.into_iter().collect();
    tools.sort_by_key(|(_, counts)| std::cmp::Reverse(counts.pre));
    for (tool, counts) in tools {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "census",
            window_secs = elapsed,
            tool = %tool,
            pre = counts.pre,
            unknown_shape = counts.unknown_shape,
            compatibility = counts.compatibility,
            segmented = counts.segmented,
            structured = counts.structured,
            unbound = counts.unbound,
            bypass_unknown_unbound = counts.bypass_unknown_unbound,
            post = counts.post,
            post_unbound_bypass = counts.post_unbound_bypass,
            "[gate] classification census"
        );
    }
    let mut census = match census().lock() {
        Ok(census) => census,
        Err(poisoned) => poisoned.into_inner(),
    };
    census.opened_at = now;
}

fn note_pre_classified(payload: &Value, assessment: &tool_effect::EffectAssessment) {
    with_census(tool_of(payload), |counts| {
        counts.pre += 1;
        match assessment.source {
            tool_effect::EffectSource::UnknownShape => counts.unknown_shape += 1,
            tool_effect::EffectSource::CompatibilityFallback => counts.compatibility += 1,
            tool_effect::EffectSource::ShellSegmented => counts.segmented += 1,
            _ => counts.structured += 1,
        }
        if assessment.target.is_none() {
            counts.unbound += 1;
        }
    });
}

fn segmented_gate_enforced() -> bool {
    std::env::var("VSC_RELAY_SEGMENTED_GATE")
        .map(|value| matches!(value.trim(), "1" | "on" | "true"))
        .unwrap_or(false)
}

fn shadow_segmented(assessment: &tool_effect::EffectAssessment, stage: &str) -> bool {
    if assessment.source != tool_effect::EffectSource::ShellSegmented || segmented_gate_enforced() {
        return false;
    }
    crate::decision_log::record(
        "gate",
        "classify_only",
        serde_json::json!({
            "stage": stage,
            "effect": format!("{:?}", assessment.effect),
            "target_bound": assessment.target.is_some(),
            "reason": "segment-aware shell reading is recorded but not enforced yet",
        }),
    );
    true
}

fn trace_current_gate(
    cwd: &Path,
    session_id: &str,
    action_material: &str,
    stage: &'static str,
) -> Result<()> {
    if let Some(record) = artifact_store::gate_for_action(cwd, session_id, action_material)? {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage,
            session = %digest_tag(&record.session_digest),
            action = %digest_tag(&record.action_digest),
            state = ?record.state,
            decision = ?record.decision,
            attempts = record.attempts,
            probe_steps = record.probe_steps,
            clearance_consumed = record.clearance_consumed,
            "[gate] controller outcome"
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct RobotProbeLease {
    cwd: PathBuf,
    target: Option<String>,
    action_material: String,
    terminal_key: String,
    target_probe_delivered: bool,
    unknown_effect: bool,
}

static ROBOT_PROBE_LEASES: OnceLock<Mutex<HashMap<String, RobotProbeLease>>> = OnceLock::new();
static TERMINAL_ROBOT_TURNS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn robot_probe_leases() -> &'static Mutex<HashMap<String, RobotProbeLease>> {
    ROBOT_PROBE_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn terminal_turns() -> &'static Mutex<HashSet<String>> {
    TERMINAL_ROBOT_TURNS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn terminal_key(payload: &Value) -> String {
    format!(
        "{}\0{}",
        session_id(payload),
        payload.get("turn_id").and_then(Value::as_str).unwrap_or("")
    )
}

fn mark_terminal_turn(key: &str) {
    let mut turns = terminal_turns()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if turns.len() >= 512 && !turns.contains(key) {
        if let Some(oldest) = turns.iter().next().cloned() {
            turns.remove(&oldest);
        }
    }
    turns.insert(key.to_string());
}

fn terminal_turn_is_blocked(key: &str) -> bool {
    terminal_turns()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(key)
}

fn register_robot_probe(
    session_id: &str,
    cwd: &Path,
    target: Option<&str>,
    action_material: &str,
    terminal_key: &str,
    unknown_effect: bool,
) {
    let mut leases = robot_probe_leases()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if leases.len() >= 256 && !leases.contains_key(session_id) {
        if let Some(oldest) = leases.keys().next().cloned() {
            leases.remove(&oldest);
        }
    }
    let replacement = RobotProbeLease {
        cwd: cwd.to_path_buf(),
        target: target.map(str::to_string),
        action_material: action_material.to_string(),
        terminal_key: terminal_key.to_string(),
        target_probe_delivered: false,
        unknown_effect,
    };
    match leases.get_mut(session_id) {
        Some(existing) if existing.action_material == action_material => {
            let delivered = existing.target_probe_delivered;
            *existing = replacement;
            existing.target_probe_delivered = delivered;
        }
        _ => {
            leases.insert(session_id.to_string(), replacement);
        }
    }
}

pub fn mark_target_probe_delivered(session_id: &str) {
    if let Some(lease) = robot_probe_leases()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_mut(session_id)
    {
        lease.target_probe_delivered = true;
    }
}

pub fn advance_robot_probe(session_id: &str) -> Result<()> {
    let lease = robot_probe_leases()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(session_id)
        .cloned();
    let Some(lease) = lease else {
        return Ok(());
    };
    let Some(record) =
        artifact_store::gate_for_action(&lease.cwd, session_id, &lease.action_material)?
    else {
        robot_probe_leases()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
        return Ok(());
    };
    if !matches!(
        record.state,
        GateState::AwaitingProof | GateState::TargetProbe | GateState::ControllerProbe
    ) {
        robot_probe_leases()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
        return Ok(());
    }
    if !lease.target_probe_delivered {
        return Ok(());
    }

    if lease.unknown_effect {
        artifact_store::note_controller_probe(
            &lease.cwd,
            session_id,
            &lease.action_material,
            "adapter-resolution-v1",
        )?;
        artifact_store::set_gate_resolution(
            &lease.cwd,
            session_id,
            &lease.action_material,
            GateState::Quarantined,
            GateDecision::QuarantineBranch,
        )?;
        mark_terminal_turn(&lease.terminal_key);
        trace_current_gate(
            &lease.cwd,
            session_id,
            &lease.action_material,
            "controller_unknown_quarantined",
        )?;
    } else if let Some(target) = lease.target.as_deref() {
        artifact_store::note_controller_probe(
            &lease.cwd,
            session_id,
            &lease.action_material,
            "filesystem-metadata-v1",
        )?;
        let evidence_ref = format!("controller-v1\0{session_id}\0{}", lease.action_material);
        match artifact_store::controller_probe_exact(&lease.cwd, target, &evidence_ref)? {
            artifact_store::ControllerProbeOutcome::Present => {
                artifact_store::set_gate_resolution(
                    &lease.cwd,
                    session_id,
                    &lease.action_material,
                    GateState::RejectedPresent,
                    GateDecision::Deny,
                )?;
                trace_current_gate(
                    &lease.cwd,
                    session_id,
                    &lease.action_material,
                    "controller_present",
                )?;
            }
            artifact_store::ControllerProbeOutcome::Absent => {
                artifact_store::note_controller_probe(
                    &lease.cwd,
                    session_id,
                    &lease.action_material,
                    "filesystem-namespace-v1",
                )?;
                artifact_store::admit_absence_to_gate(
                    &lease.cwd,
                    session_id,
                    &lease.action_material,
                )?;
                trace_current_gate(
                    &lease.cwd,
                    session_id,
                    &lease.action_material,
                    "controller_absent",
                )?;
            }
            artifact_store::ControllerProbeOutcome::Unavailable => {
                artifact_store::note_controller_probe(
                    &lease.cwd,
                    session_id,
                    &lease.action_material,
                    "filesystem-namespace-v1",
                )?;
                artifact_store::set_gate_resolution(
                    &lease.cwd,
                    session_id,
                    &lease.action_material,
                    GateState::Quarantined,
                    GateDecision::QuarantineBranch,
                )?;
                mark_terminal_turn(&lease.terminal_key);
                trace_current_gate(
                    &lease.cwd,
                    session_id,
                    &lease.action_material,
                    "controller_unavailable_quarantined",
                )?;
            }
        }
    } else {
        artifact_store::set_gate_resolution(
            &lease.cwd,
            session_id,
            &lease.action_material,
            GateState::Quarantined,
            GateDecision::QuarantineBranch,
        )?;
        mark_terminal_turn(&lease.terminal_key);
        trace_current_gate(
            &lease.cwd,
            session_id,
            &lease.action_material,
            "controller_unbound_quarantined",
        )?;
    }
    robot_probe_leases()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_id);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PolicyResult {
    state: GateState,
    decision: GateDecision,
    hook_decision: Option<&'static str>,
    reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoveragePlan {
    ClearOnce,
    Evaluate(CoverageStatus),
}

fn coverage_plan(coverage: CoverageStatus, previous: Option<(GateState, bool)>) -> CoveragePlan {
    if coverage != CoverageStatus::AbsentFresh {
        CoveragePlan::Evaluate(coverage)
    } else if previous == Some((GateState::ClearedAbsent, true)) {
        CoveragePlan::Evaluate(CoverageStatus::Stale)
    } else {
        CoveragePlan::ClearOnce
    }
}

fn policy(
    mode: Mode,
    coverage: CoverageStatus,
    unknown_effect: bool,
    attempts: u8,
    probe_steps: u8,
    age_secs: i64,
) -> PolicyResult {
    if mode == Mode::Manual {
        return PolicyResult {
            state: GateState::Advisory,
            decision: GateDecision::Annotate,
            hook_decision: None,
            reason: "deterministic mutation risk annotated; Manual remains user-controlled",
        };
    }
    if coverage == CoverageStatus::PresentFresh && !unknown_effect {
        return PolicyResult {
            state: GateState::RejectedPresent,
            decision: GateDecision::Deny,
            hook_decision: Some("deny"),
            reason: "artifact already exists at the exact locator and version; reject duplicate novelty and continue through Modify, Wiring, Population, or Behavior",
        };
    }
    match mode {
        Mode::Auto if probe_steps >= AUTO_PROBE_LIMIT || age_secs >= AUTO_TIMEOUT_SECS => {
            PolicyResult {
                state: GateState::AwaitingProof,
                decision: GateDecision::AskProof,
                hook_decision: Some("ask_user"),
                reason: "Auto exhausted its bounded proof window; return this exact mutation decision to the user",
            }
        }
        Mode::Auto => PolicyResult {
            state: GateState::AwaitingProof,
            decision: GateDecision::AskProof,
            hook_decision: Some("deny"),
            reason: "hold novelty mutation; obtain an exact structured read-only receipt (maximum three unique probes / 110 seconds), then retry",
        },
        Mode::Robot if probe_steps >= AUTO_PROBE_LIMIT || attempts > AUTO_PROBE_LIMIT => {
            PolicyResult {
                state: GateState::Quarantined,
                decision: GateDecision::QuarantineBranch,
                hook_decision: Some("deny"),
                reason: "Terminal SafeBlocked for this Robot lease: stop this turn now, invoke no further tools, do not ask the user, and emit only the deterministic dossier/report",
            }
        }
        Mode::Robot => PolicyResult {
            state: if attempts > 1 {
                GateState::ControllerProbe
            } else {
                GateState::TargetProbe
            },
            decision: GateDecision::AutonomousResolve,
            hook_decision: Some("deny"),
            reason: "Robot must resolve autonomously: perform a unique typed read-only target probe, then an alternative capability if partial; helper output may choose a probe but is never evidence",
        },
        Mode::Manual => unreachable!("handled above"),
    }
}

#[cfg(test)]
fn effective_hold(danger_base: bool, gate: &PolicyResult) -> bool {
    danger_base || matches!(gate.hook_decision, Some("deny" | "ask_user"))
}

fn session_id(payload: &Value) -> &str {
    payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown-session")
}

fn cwd(payload: &Value) -> &Path {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .unwrap_or_else(|| Path::new("."))
}

fn alias(cwd: &Path) -> &str {
    cwd.file_name().and_then(|name| name.to_str()).unwrap_or("")
}

fn action_material(payload: &Value, effect: ToolEffect, target: Option<&str>) -> String {
    let mut identity = String::new();
    for field in ["session_id", "turn_id"] {
        if let Some(value) = payload.get(field).and_then(Value::as_str) {
            identity.push_str(field);
            identity.push('=');
            identity.push_str(value);
            identity.push('\0');
        }
    }
    if identity.is_empty() {
        identity.push_str(
            &serde_json::to_string(payload.get("tool_input").unwrap_or(&Value::Null))
                .unwrap_or_default(),
        );
    }
    identity.push_str(&format!("{effect:?}\0{}", target.unwrap_or("")));
    identity
}

fn native_identity(payload: &Value) -> String {
    let mut identity = String::new();
    for field in ["session_id", "turn_id", "tool_use_id"] {
        if let Some(value) = payload.get(field).and_then(Value::as_str) {
            identity.push_str(field);
            identity.push('=');
            identity.push_str(value);
            identity.push('\0');
        }
    }
    if identity.is_empty() {
        serde_json::to_string(payload.get("tool_input").unwrap_or(&Value::Null)).unwrap_or_default()
    } else {
        identity
    }
}

fn typed_steps(payload: &Value) -> Vec<SemanticStep> {
    let Some(path) = payload.get("transcript_path").and_then(Value::as_str) else {
        if payload.get("turn_id").is_some() {
            tracing::info!(
                target: "relay::gate",
                pipeline = "gate",
                stage = "obligation_lane_unavailable",
                "[gate] Codex frame without transcript_path; obligation-proof lane skipped, only duplicate and unknown-effect gating apply"
            );
        }
        return Vec::new();
    };
    if payload.get("turn_id").is_some() || path.contains("/.codex/") {
        relay_adapters::codex::semantic_steps(Path::new(path))
    } else {
        relay_adapters::claude::semantic_steps(Path::new(path))
    }
}

fn deterministic_ledger(payload: &Value) -> Option<relay_compass::ContractLedger> {
    let steps = typed_steps(payload);
    let input = contract_input_from_steps(&steps);
    input.anchor.as_ref()?;
    Some(build_contract_ledger_from_input(&steps, &[], &input))
}

fn validate_pin<'a>(pin: &'a Value, target: &str, epoch: u32) -> Option<&'a str> {
    matches!(
        pin.get("source").and_then(Value::as_str),
        Some("user" | "controller")
    )
    .then_some(())?;
    (pin.get("artifact_locator").and_then(Value::as_str) == Some(target)).then_some(())?;
    (pin.get("contract_epoch").and_then(Value::as_u64) == Some(epoch as u64)).then_some(())?;
    pin.get("obligation_id").and_then(Value::as_str)
}

fn trusted_pin<'a>(payload: &'a Value, target: &str, epoch: u32) -> Option<&'a str> {
    validate_pin(payload.get("relay_gate_pin")?, target, epoch)
}

fn resolve_pin(payload: &Value, sid: &str, target: &str, epoch: u32) -> Option<String> {
    if let Some(id) = trusted_pin(payload, target, epoch) {
        return Some(id.to_string());
    }
    crate::compass::gate_pins(sid)
        .iter()
        .rev()
        .find_map(|pin| validate_pin(pin, target, epoch).map(str::to_string))
}

fn make_hook_decision(policy: &PolicyResult) -> Option<Value> {
    policy.hook_decision.map(|decision| {
        json!({
            "decision": decision,
            "reason": policy.reason,
            "gate_state": policy.state,
        })
    })
}

fn sticky_resolution(mode: Mode, state: Option<GateState>) -> Option<PolicyResult> {
    if mode == Mode::Manual {
        return None;
    }
    match state {
        Some(GateState::Quarantined) => Some(PolicyResult {
            state: GateState::Quarantined,
            decision: GateDecision::QuarantineBranch,
            hook_decision: Some("deny"),
            reason: "Terminal SafeBlocked for this Robot lease: stop this turn now, invoke no further tools, do not ask the user, and emit only the deterministic dossier/report",
        }),
        Some(GateState::RejectedPresent) => Some(PolicyResult {
            state: GateState::RejectedPresent,
            decision: GateDecision::Deny,
            hook_decision: Some("deny"),
            reason: "artifact is already present; do not retry Create/Replace and replan to Modify, Wiring, Population, or Behavior",
        }),
        _ => None,
    }
}

pub fn pre_tool(payload: &Value, cfg: &AutomationConfig) -> Result<Option<Value>> {
    if !cfg.smart.enabled || !cfg.smart.gate {
        return Ok(None);
    }

    if tool_effect::is_relay_control_plane(payload) {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "control_plane_bypass",
            "[gate] relay control-plane command deferred to base policy"
        );
        return Ok(None);
    }
    let cwd = cwd(payload);
    let sid = session_id(payload);
    let mode = cfg.resolve(Some(sid), alias(cwd), crate::automation::now_secs());
    if mode == Mode::Robot {
        advance_robot_probe(sid)?;
    }
    let assessment = tool_effect::classify(payload);
    note_pre_classified(payload, &assessment);
    if shadow_segmented(&assessment, "pre_tool") {
        return Ok(None);
    }
    tracing::debug!(
        target: "relay::gate",
        pipeline = "gate",
        stage = "pre_classified",
        mode = mode.label(),
        effect = ?assessment.effect,
        source = ?assessment.source,
        target_bound = assessment.target.is_some(),
        "[gate] pre-tool classified"
    );
    let terminal_key = terminal_key(payload);
    if mode == Mode::Robot
        && assessment.effect != ToolEffect::ReadOnly
        && terminal_turn_is_blocked(&terminal_key)
    {
        return Ok(make_hook_decision(&PolicyResult {
            state: GateState::Quarantined,
            decision: GateDecision::QuarantineBranch,
            hook_decision: Some("deny"),
            reason: "Terminal SafeBlocked for this Robot turn: stop now, invoke no further tools, do not ask the user, and emit only the deterministic dossier/report",
        }));
    }
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let material_base = action_material(payload, assessment.effect, None);
    let native_identity = native_identity(payload);

    if assessment.effect == ToolEffect::Unknown {
        let shape_digest = artifact_store::record_unknown_shape(cwd, input)?;

        if assessment.target.is_none() {
            note_unreadable_shell(payload, input);
            with_census(tool_of(payload), |counts| {
                counts.bypass_unknown_unbound += 1
            });
            tracing::debug!(
                target: "relay::gate",
                pipeline = "gate",
                stage = "bypass_unknown_unbound",
                mode = mode.label(),
                shape = %shape_digest,
                "[gate] unknown unbound capability recorded; base behavior"
            );
            return Ok(None);
        }
        let locator = assessment
            .target
            .as_deref()
            .and_then(|target| artifact_store::snapshot(cwd, target).ok().flatten())
            .map(|snapshot| snapshot.locator_digest)
            .unwrap_or_else(|| "unbound".to_string());
        let material = format!(
            "{material_base}\0mode={}\0shape={shape_digest}\0locator={locator}",
            mode.label()
        );
        let previous = artifact_store::gate_for_action(cwd, sid, &material)?;
        if let Some(sticky) = sticky_resolution(mode, previous.as_ref().map(|record| record.state))
        {
            if mode == Mode::Robot && sticky.state == GateState::Quarantined {
                mark_terminal_turn(&terminal_key);
            }
            return Ok(make_hook_decision(&sticky));
        }
        let provisional = policy(
            mode,
            CoverageStatus::Missing,
            true,
            previous.as_ref().map_or(1, |record| record.attempts + 1),
            previous.as_ref().map_or(0, |record| record.probe_steps),
            previous
                .as_ref()
                .map_or(0, |record| crate::automation::now_secs() - record.opened_at),
        );
        let record = artifact_store::record_gate(
            cwd,
            sid,
            &material,
            &native_identity,
            None,
            provisional.state,
            provisional.decision,
            Vec::new(),
        )?;
        let result = policy(
            mode,
            CoverageStatus::Missing,
            true,
            record.attempts,
            record.probe_steps,
            crate::automation::now_secs() - record.opened_at,
        );
        if record.state != result.state || record.decision != result.decision {
            artifact_store::set_gate_resolution(
                cwd,
                sid,
                &material,
                result.state,
                result.decision,
            )?;
        }
        if mode == Mode::Robot && result.state != GateState::Quarantined {
            register_robot_probe(
                sid,
                cwd,
                assessment.target.as_deref(),
                &material,
                &terminal_key,
                true,
            );
        } else if mode == Mode::Robot && result.state == GateState::Quarantined {
            mark_terminal_turn(&terminal_key);
        }
        return Ok(make_hook_decision(&result));
    }

    let Some(target) = assessment.target.as_deref() else {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "bypass_unbound",
            mode = mode.label(),
            effect = ?assessment.effect,
            "[gate] no deterministic artifact binding; base behavior"
        );
        return Ok(None);
    };
    let Some(snapshot) = artifact_store::snapshot(cwd, target)? else {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "bypass_outside_repo",
            mode = mode.label(),
            effect = ?assessment.effect,
            "[gate] target is outside the admitted repository; base behavior"
        );
        return Ok(None);
    };
    let Some(ledger) = deterministic_ledger(payload) else {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "bypass_no_contract",
            mode = mode.label(),
            coverage = ?snapshot.coverage,
            "[gate] no active deterministic contract; base behavior"
        );
        return Ok(None);
    };
    let material = format!(
        "{material_base}\0mode={}\0locator={}\0epoch={}\0version={}",
        mode.label(),
        snapshot.locator_digest,
        ledger.epoch,
        snapshot.version_hash.as_deref().unwrap_or("unanchored")
    );
    let previous = artifact_store::gate_for_action(cwd, sid, &material)?;
    if let Some(sticky) = sticky_resolution(mode, previous.as_ref().map(|record| record.state)) {
        if mode == Mode::Robot && sticky.state == GateState::Quarantined {
            mark_terminal_turn(&terminal_key);
        }
        return Ok(make_hook_decision(&sticky));
    }
    let effective_coverage = match coverage_plan(
        snapshot.coverage,
        previous
            .as_ref()
            .map(|record| (record.state, record.clearance_consumed)),
    ) {
        CoveragePlan::ClearOnce => {
            let consumed = artifact_store::record_gate(
                cwd,
                sid,
                &material,
                &native_identity,
                Some(snapshot.locator_digest),
                GateState::ClearedAbsent,
                GateDecision::DeferToBase,
                Vec::new(),
            )?;
            tracing::info!(
                target: "relay::gate",
                pipeline = "gate",
                stage = "clearance_consumed",
                session = %digest_tag(&consumed.session_digest),
                action = %digest_tag(&consumed.action_digest),
                "[gate] one-shot mutation clearance consumed; returning to base policy"
            );
            return Ok(None);
        }
        CoveragePlan::Evaluate(coverage) => coverage,
    };
    let proof: Option<GateProof> = ledger
        .prove_pre_mutation(target, assessment.effect, effective_coverage)
        .or_else(|| {
            resolve_pin(payload, sid, target, ledger.epoch).and_then(|obligation_id| {
                ledger.prove_pre_mutation_pinned(
                    &obligation_id,
                    target,
                    assessment.effect,
                    effective_coverage,
                )
            })
        });
    let Some(proof) = proof else {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "bypass_no_proof",
            mode = mode.label(),
            effect = ?assessment.effect,
            coverage = ?effective_coverage,
            "[gate] Contract Ledger did not issue GateProof; base behavior"
        );
        return Ok(None);
    };
    tracing::info!(
        target: "relay::gate",
        pipeline = "gate",
        stage = "proof_admitted",
        mode = mode.label(),
        mutation = ?proof.mutation(),
        coverage = ?proof.coverage(),
        obligations = proof.obligation_ids().len(),
        "[gate] deterministic GateProof admitted"
    );
    let previous = artifact_store::gate_for_action(cwd, sid, &material)?;
    let provisional = policy(
        mode,
        proof.coverage(),
        false,
        previous.as_ref().map_or(1, |record| record.attempts + 1),
        previous.as_ref().map_or(0, |record| record.probe_steps),
        previous
            .as_ref()
            .map_or(0, |record| crate::automation::now_secs() - record.opened_at),
    );
    let record = artifact_store::record_gate(
        cwd,
        sid,
        &material,
        &native_identity,
        Some(snapshot.locator_digest),
        provisional.state,
        provisional.decision,
        proof.obligation_ids().to_vec(),
    )?;
    let result = policy(
        mode,
        proof.coverage(),
        false,
        record.attempts,
        record.probe_steps,
        crate::automation::now_secs() - record.opened_at,
    );
    if record.state != result.state || record.decision != result.decision {
        artifact_store::set_gate_resolution(cwd, sid, &material, result.state, result.decision)?;
    }
    if mode == Mode::Robot
        && matches!(
            result.state,
            GateState::TargetProbe | GateState::ControllerProbe
        )
    {
        register_robot_probe(sid, cwd, Some(target), &material, &terminal_key, false);
    } else if mode == Mode::Robot && result.state == GateState::Quarantined {
        mark_terminal_turn(&terminal_key);
    }
    Ok(make_hook_decision(&result))
}

pub fn post_tool(payload: &Value, cfg: &AutomationConfig) -> Result<()> {
    if !cfg.smart.enabled || !cfg.smart.gate {
        return Ok(());
    }

    if tool_effect::is_relay_control_plane(payload) {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "control_plane_post_bypass",
            "[gate] relay control-plane result ignored by artifact gate"
        );
        return Ok(());
    }
    let assessment = tool_effect::classify(payload);
    with_census(tool_of(payload), |counts| counts.post += 1);
    if shadow_segmented(&assessment, "post_tool") {
        return Ok(());
    }
    tracing::debug!(
        target: "relay::gate",
        pipeline = "gate",
        stage = "post_classified",
        effect = ?assessment.effect,
        source = ?assessment.source,
        target_bound = assessment.target.is_some(),
        "[gate] post-tool observed"
    );
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    if assessment.effect == ToolEffect::Unknown {
        artifact_store::record_unknown_shape(cwd(payload), input)?;
    }

    if assessment.target.is_none() {
        with_census(tool_of(payload), |counts| counts.post_unbound_bypass += 1);
        tracing::debug!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "post_unbound_bypass",
            "[gate] unbound result observed without changing gate state"
        );
        return Ok(());
    }
    let material = action_material(payload, assessment.effect, assessment.target.as_deref());
    artifact_store::note_post_tool(
        cwd(payload),
        session_id(payload),
        assessment.effect,
        &material,
        assessment.target.as_deref(),
    )?;

    let receipt = payload
        .get("tool_response")
        .or_else(|| payload.get("tool_output"));
    let receipt_id = payload
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or(&material);
    if assessment.effect == ToolEffect::ReadOnly {
        if let (Some(target), Some(receipt)) = (
            assessment.target.as_deref(),
            receipt.and_then(|value| value.get("artifact_state_receipt").or(Some(value))),
        ) {
            let outcome =
                artifact_store::record_typed_receipt(cwd(payload), target, receipt, receipt_id)?;
            let reason = artifact_store::rejection_reason(&outcome);
            tracing::info!(
                target: "relay::gate",
                pipeline = "gate",
                stage = "typed_receipt",
                admitted = outcome.admitted(),
                reason,
                "[gate] typed artifact receipt evaluated"
            );
            crate::decision_log::record(
                "gate",
                if outcome.admitted() {
                    "receipt_admitted"
                } else {
                    "receipt_rejected"
                },
                serde_json::json!({"stage": "typed_receipt", "reason": reason}),
            );
        }
    }
    if assessment.effect == ToolEffect::ReadOnly
        && receipt
            .and_then(|value| value.get("exists"))
            .and_then(Value::as_bool)
            == Some(false)
        && receipt
            .and_then(|value| value.get("path").or_else(|| value.get("target")))
            .and_then(Value::as_str)
            == assessment.target.as_deref()
    {
        let admitted = artifact_store::record_absence(
            cwd(payload),
            assessment
                .target
                .as_deref()
                .context("typed receipt target")?,
            receipt_id,
        )?;
        if admitted {
            tracing::info!(
                target: "relay::gate",
                pipeline = "gate",
                stage = "absence_receipt",
                admitted = true,
                "[gate] exact absence receipt admitted"
            );
            artifact_store::admit_absence_to_latest_gate(cwd(payload), session_id(payload))?;
        }
    }
    advance_robot_probe(session_id(payload))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_never_changes_hook_decision() {
        let result = policy(Mode::Manual, CoverageStatus::PresentFresh, false, 1, 0, 0);
        assert_eq!(result.hook_decision, None);
        assert_eq!(result.decision, GateDecision::Annotate);
    }

    #[test]
    fn manual_never_blocks_via_a_matching_quarantine_record() {
        for state in [GateState::Quarantined, GateState::RejectedPresent] {
            assert!(sticky_resolution(Mode::Manual, Some(state)).is_none());
            assert!(sticky_resolution(Mode::Robot, Some(state)).is_some());
        }
    }

    #[test]
    fn validate_pin_requires_trusted_source_exact_target_and_epoch() {
        let pin = json!({
            "source": "user",
            "artifact_locator": "src/lib.rs",
            "obligation_id": "obl-1",
            "contract_epoch": 3,
        });
        assert_eq!(validate_pin(&pin, "src/lib.rs", 3), Some("obl-1"));
        assert_eq!(validate_pin(&pin, "src/other.rs", 3), None);
        assert_eq!(validate_pin(&pin, "src/lib.rs", 4), None);

        let agent_pin = json!({
            "source": "assistant",
            "artifact_locator": "src/lib.rs",
            "obligation_id": "obl-1",
            "contract_epoch": 3,
        });
        assert_eq!(validate_pin(&agent_pin, "src/lib.rs", 3), None);
    }

    #[test]
    fn resolve_pin_falls_back_to_the_persisted_store() {
        let sid = format!(
            "gate-ctrl-pin-{}-{}",
            std::process::id(),
            crate::automation::now_secs()
        );
        crate::compass::record_gate_pin(&sid, "src/lib.rs", "obl-9", 5, true).unwrap();
        let empty = json!({});
        assert_eq!(
            resolve_pin(&empty, &sid, "src/lib.rs", 5),
            Some("obl-9".to_string())
        );
        assert_eq!(resolve_pin(&empty, &sid, "src/lib.rs", 6), None);
        crate::compass::clear_gate_pins(&sid, None).unwrap();
        assert_eq!(resolve_pin(&empty, &sid, "src/lib.rs", 5), None);
    }

    #[test]
    fn auto_returns_control_after_bounded_window() {
        let result = policy(Mode::Auto, CoverageStatus::Missing, false, 1, 3, 1);
        assert_eq!(result.hook_decision, Some("ask_user"));
    }

    #[test]
    fn robot_never_asks_user_for_any_gate_outcome() {
        for coverage in [
            CoverageStatus::PresentFresh,
            CoverageStatus::Missing,
            CoverageStatus::Partial,
            CoverageStatus::Stale,
        ] {
            for probes in 0..=3 {
                let result = policy(Mode::Robot, coverage, false, 4, probes, 200);
                assert_ne!(result.hook_decision, Some("ask_user"));
            }
        }
    }

    #[test]
    fn robot_quarantines_after_probe_exhaustion() {
        let result = policy(Mode::Robot, CoverageStatus::Missing, false, 2, 3, 1);
        assert_eq!(result.state, GateState::Quarantined);
        assert_eq!(result.decision, GateDecision::QuarantineBranch);
        assert_eq!(result.hook_decision, Some("deny"));
    }

    #[test]
    fn gate_can_never_cancel_base_danger() {
        let advisory = policy(Mode::Manual, CoverageStatus::Missing, false, 1, 0, 0);
        assert!(effective_hold(true, &advisory));
        assert!(!effective_hold(false, &advisory));
    }

    #[test]
    fn disabled_gate_is_an_exact_noop_even_for_invalid_payload() {
        let config = AutomationConfig::default();
        assert_eq!(pre_tool(&Value::Null, &config).unwrap(), None);
    }

    #[test]
    fn terminal_robot_turn_cannot_block_relay_control_plane() {
        let mut config = AutomationConfig::default();
        config.smart.enabled = true;
        config.smart.gate = true;
        let payload = json!({
            "session_id": "control-plane-session",
            "turn_id": "control-plane-turn",
            "tool_input": {
                "command": "/tmp/vsc-relay-agent automation smart steer off"
            }
        });
        mark_terminal_turn(&terminal_key(&payload));
        assert_eq!(pre_tool(&payload, &config).unwrap(), None);
    }

    #[test]
    fn unknown_unbound_action_is_recorded_but_never_held() {
        let _guard = crate::artifact_store::ARTIFACT_ENV.lock().unwrap();
        let unique = format!(
            "vsc-relay-unbound-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let repo = std::env::temp_dir().join(unique);
        let private_root = repo.join("private");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::env::set_var("VSC_RELAY_ARTIFACT_ROOT", &private_root);
        let config = AutomationConfig {
            default: Mode::Robot,
            smart: crate::automation::SmartConfig {
                enabled: true,
                gate: true,
                ..crate::automation::SmartConfig::default()
            },
            ..AutomationConfig::default()
        };
        let payload = json!({
            "session_id":"unbound-session",
            "turn_id":"turn-1",
            "cwd":repo,
            "tool_input":{"opaque_controller_state":true}
        });
        assert!(pre_tool(&payload, &config).unwrap().is_none());
        let mut post_payload = payload;
        post_payload["tool_response"] = json!({"opaque_result":true});
        post_tool(&post_payload, &config).unwrap();
        let control = crate::artifact_store::robot_gate_control("unbound-session").unwrap();
        assert!(!control.active);
        assert!(control.state.is_none());

        let readonly = json!({
            "session_id":"readonly-unbound-session",
            "turn_id":"turn-2",
            "cwd":repo,
            "tool_name":"Bash",
            "tool_input":{"command":"pwd"}
        });
        assert!(pre_tool(&readonly, &config).unwrap().is_none());
        let mut readonly_post = readonly;
        readonly_post["tool_response"] = json!({"stdout":"/tmp/example"});
        post_tool(&readonly_post, &config).unwrap();
        let control =
            crate::artifact_store::robot_gate_control("readonly-unbound-session").unwrap();
        assert!(!control.active);
        assert!(control.state.is_none());
        std::env::remove_var("VSC_RELAY_ARTIFACT_ROOT");
        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn absence_clearance_is_strictly_one_shot_per_lease() {
        assert_eq!(
            coverage_plan(CoverageStatus::AbsentFresh, None),
            CoveragePlan::ClearOnce
        );
        assert_eq!(
            coverage_plan(
                CoverageStatus::AbsentFresh,
                Some((GateState::ClearedAbsent, true))
            ),
            CoveragePlan::Evaluate(CoverageStatus::Stale)
        );
        assert_eq!(
            coverage_plan(
                CoverageStatus::AbsentFresh,
                Some((GateState::ClearedAbsent, false))
            ),
            CoveragePlan::ClearOnce
        );
    }

    #[test]
    fn robot_controller_resolves_absent_present_and_unknown_without_user_question() {
        let _guard = crate::artifact_store::ARTIFACT_ENV.lock().unwrap();
        let unique = format!(
            "vsc-relay-controller-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = std::env::temp_dir().join(unique);
        let repo = base.join("repo");
        let private_root = base.join("private-store");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::env::set_var("VSC_RELAY_ARTIFACT_ROOT", &private_root);

        crate::artifact_store::record_gate(
            &repo,
            "absent-session",
            "absent-action",
            "native-1",
            None,
            GateState::TargetProbe,
            GateDecision::AutonomousResolve,
            vec!["obl-1".to_string()],
        )
        .unwrap();
        register_robot_probe(
            "absent-session",
            &repo,
            Some("generated/new.rs"),
            "absent-action",
            "absent-session\0turn-1",
            false,
        );
        mark_target_probe_delivered("absent-session");
        advance_robot_probe("absent-session").unwrap();
        let absent =
            crate::artifact_store::gate_for_action(&repo, "absent-session", "absent-action")
                .unwrap()
                .unwrap();
        assert_eq!(absent.state, GateState::ClearedAbsent);
        assert!(!absent.clearance_consumed);
        assert!(crate::artifact_store::robot_gate_control("absent-session")
            .unwrap()
            .directive
            .is_none());

        std::fs::write(repo.join("already.rs"), b"present").unwrap();
        crate::artifact_store::record_gate(
            &repo,
            "present-session",
            "present-action",
            "native-2",
            None,
            GateState::TargetProbe,
            GateDecision::AutonomousResolve,
            vec!["obl-2".to_string()],
        )
        .unwrap();
        register_robot_probe(
            "present-session",
            &repo,
            Some("already.rs"),
            "present-action",
            "present-session\0turn-1",
            false,
        );
        mark_target_probe_delivered("present-session");
        advance_robot_probe("present-session").unwrap();
        let present =
            crate::artifact_store::gate_for_action(&repo, "present-session", "present-action")
                .unwrap()
                .unwrap();
        assert_eq!(present.state, GateState::RejectedPresent);
        assert_eq!(present.decision, GateDecision::Deny);

        crate::artifact_store::record_gate(
            &repo,
            "unknown-session",
            "unknown-action",
            "native-3",
            None,
            GateState::TargetProbe,
            GateDecision::AutonomousResolve,
            vec![],
        )
        .unwrap();
        register_robot_probe(
            "unknown-session",
            &repo,
            None,
            "unknown-action",
            "unknown-session\0turn-1",
            true,
        );
        mark_target_probe_delivered("unknown-session");
        advance_robot_probe("unknown-session").unwrap();
        let unknown = crate::artifact_store::robot_gate_control("unknown-session").unwrap();
        assert!(unknown.terminal);
        assert_eq!(unknown.state, Some(GateState::Quarantined));

        let config = AutomationConfig {
            default: Mode::Robot,
            smart: crate::automation::SmartConfig {
                enabled: true,
                gate: true,
                ..crate::automation::SmartConfig::default()
            },
            ..AutomationConfig::default()
        };
        let same_turn = serde_json::json!({
            "session_id":"unknown-session",
            "turn_id":"turn-1",
            "cwd":repo.to_string_lossy(),
            "tool_input":{"path":"another.rs", "content":"x"}
        });
        let terminal = pre_tool(&same_turn, &config).unwrap().unwrap();
        assert_eq!(terminal["gate_state"], "quarantined");
        assert_ne!(terminal["decision"], "ask_user");
        assert!(terminal["reason"].as_str().unwrap().contains("stop now"));

        let next_turn = serde_json::json!({
            "session_id":"unknown-session",
            "turn_id":"turn-2",
            "cwd":repo.to_string_lossy(),
            "tool_input":{"path":"another.rs", "content":"x"}
        });
        assert!(pre_tool(&next_turn, &config).unwrap().is_none());

        std::env::remove_var("VSC_RELAY_ARTIFACT_ROOT");
        let _ = std::fs::remove_dir_all(base);
    }
}
