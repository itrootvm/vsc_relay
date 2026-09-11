use crate::health::{
    evaluate_health_window, HealthDecision, HealthGateParams, HealthSignals, SessionPhase,
    HEALTH_WINDOW, SOURCE_AGENT, SOURCE_CONTRACT, SOURCE_DELEGATE, SOURCE_RUNTIME, SOURCE_TOOL,
    SOURCE_USER,
};
use crate::ledger::{
    build_contract_ledger_from_input_with_seeds, contract_input_from_steps, ContractLedger,
    LedgerSeed,
};
use crate::predictive::{
    evaluate_predictive_window, GapFactor, LikelihoodEvidence, PredictiveDecision, PredictiveFrame,
    PredictiveParams, GAP_FACTOR_COUNT,
};
use crate::semantic::{Relation, SemanticFacts, SemanticInputFrame};
use crate::steps::{SemanticStep, StepRole, ToolKind};
use crate::{noisy_or, Observation};
use std::collections::BTreeSet;
use std::ops::Range;

pub struct StagedContext<'a> {
    pub contract_text: &'a str,
    pub goal_keywords: &'a BTreeSet<String>,
    pub goal_literals: &'a BTreeSet<String>,
    pub observation: Option<&'a Observation>,
    pub deviation_mean: f32,
    pub deviation_sigma: f32,
    pub phase: SessionPhase,
    pub pending_question: bool,
    pub previous_steers: u8,
    pub same_signature_recent: bool,
    pub continuation_seeds: &'a [LedgerSeed],
}

pub struct StagedOutput {
    pub decision: PredictiveDecision,
    pub scope_breadth: f32,
    pub proof_resolvability: f32,
    pub dominant_source_mask: u8,
    pub pivot_confirmed: bool,
    pub semantic_calibrated: bool,
    pub semantic_backend: String,
    pub semantic_model: String,

    pub deterministic_feedback: bool,
    pub ledger: ContractLedger,
    pub health: HealthDecision,
}

fn shadow_health_signals(
    ledger: &ContractLedger,
    frame: &PredictiveFrame,
    source_mask: u8,
) -> HealthSignals {
    let signals = &ledger.signals;
    HealthSignals {
        unresolved_contract: (1.0 - signals.coverage).clamp(0.0, 1.0),
        completion_claim: if signals.claimed_unverified > 0 {
            1.0
        } else {
            0.0
        },
        proof_deficit: if signals.proof_deficit { 1.0 } else { 0.0 },
        scope_loss: if signals.completion_scope_gap {
            1.0
        } else {
            0.0
        },
        observed_failure: if signals.contradicted > 0 { 1.0 } else { 0.0 },
        post_claim_recurrence: if signals.stale > 0 { 1.0 } else { 0.0 },
        unsupported_blocker: if signals.disputed > 0 { 1.0 } else { 0.0 },
        source_mask,
        phase: frame.phase,
        pivot_confirmed: frame.pivot_confirmed,
        pending_question: frame.pending_question,
        same_signature_recent: frame.same_signature_recent,
        previous_steers: frame.previous_steers,
    }
}

#[derive(Clone)]
struct Episode {
    ordinal: u32,
    range: Range<usize>,
    contract_turn: Option<usize>,
}

fn episodes(steps: &[SemanticStep]) -> Vec<Episode> {
    let mut result = Vec::new();
    let mut i = 0;
    let mut last_user: Option<usize> = None;
    while i < steps.len() {
        if steps[i].role == StepRole::User {
            last_user = Some(i);
            i += 1;
            continue;
        }
        let start = i;
        while i < steps.len() && steps[i].role != StepRole::User {
            i += 1;
        }
        result.push(Episode {
            ordinal: result.len() as u32,
            range: start..i,
            contract_turn: last_user,
        });
    }
    result
}

fn bounded(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

pub fn semantic_inputs(steps: &[SemanticStep], goal: &str) -> Vec<SemanticInputFrame> {
    let all = episodes(steps);
    let start = all.len().saturating_sub(HEALTH_WINDOW);
    all[start..]
        .iter()
        .map(|ep| {
            let mut assistant = Vec::new();
            let mut tools = Vec::new();
            let mut runtime = Vec::new();
            let mut runtime_error = false;
            for step in &steps[ep.range.clone()] {
                match step.role {
                    StepRole::Assistant => assistant.push(bounded(&step.text, 4000)),
                    StepRole::ToolUse => tools.push(format!(
                        "call_id={}; kind={:?}; target={}",
                        bounded(step.correlation_id.as_deref().unwrap_or("legacy-none"), 200),
                        step.tool_kind,
                        bounded(step.tool_target.as_deref().unwrap_or(""), 600)
                    )),
                    StepRole::ToolResult => {
                        runtime_error |= step.is_error;
                        runtime.push(format!(
                            "call_id={}; error={}; output={}",
                            bounded(step.correlation_id.as_deref().unwrap_or("legacy-none"), 200),
                            step.is_error,
                            bounded(&step.text, 1800)
                        ));
                    }
                    StepRole::DelegateResult => tools.push(format!(
                        "delegate_report_id={}; error={}; claim={}",
                        bounded(
                            step.correlation_id.as_deref().unwrap_or("no-native-id"),
                            200
                        ),
                        step.is_error,
                        bounded(&step.text, 1800)
                    )),
                    StepRole::User => {}
                }
            }
            SemanticInputFrame {
                episode: ep.ordinal,
                goal: bounded(goal, 4000),
                user_contract: ep
                    .contract_turn
                    .map(|i| bounded(&steps[i].text, 3000))
                    .unwrap_or_default(),
                assistant: assistant.join("\n"),
                tools,
                runtime,
                runtime_error,
            }
        })
        .collect()
}

struct EpisodeView {
    tool_targets: Vec<String>,
    tool_kinds: Vec<ToolKind>,
    result_texts: Vec<(String, bool)>,
}

fn view(steps: &[SemanticStep], ep: &Episode) -> EpisodeView {
    let mut tool_targets = Vec::new();
    let mut tool_kinds = Vec::new();
    let mut result_texts = Vec::new();
    for step in &steps[ep.range.clone()] {
        match step.role {
            StepRole::ToolUse => {
                tool_kinds.push(step.tool_kind);
                if let Some(target) = &step.tool_target {
                    tool_targets.push(target.to_lowercase());
                }
            }
            StepRole::ToolResult => result_texts.push((step.text.clone(), step.is_error)),
            StepRole::Assistant | StepRole::User | StepRole::DelegateResult => {}
        }
    }
    EpisodeView {
        tool_targets,
        tool_kinds,
        result_texts,
    }
}

fn last_index<F: Fn(&SemanticStep) -> bool>(
    steps: &[SemanticStep],
    end: usize,
    f: F,
) -> Option<usize> {
    (0..end.min(steps.len())).rev().find(|&i| f(&steps[i]))
}

fn uncovered_literal(ctx: &StagedContext<'_>, view: &EpisodeView) -> bool {
    !ctx.goal_literals.is_empty()
        && ctx.goal_literals.iter().any(|literal| {
            let literal = literal.to_lowercase();
            !view
                .tool_targets
                .iter()
                .any(|target| target.contains(&literal))
        })
}

fn bound_probe_covers_open_obligations(
    steps: &[SemanticStep],
    ep: &Episode,
    ledger: &ContractLedger,
) -> bool {
    let current_results = steps[ep.range.clone()]
        .iter()
        .filter(|step| step.role == StepRole::ToolResult)
        .map(|step| step.index)
        .collect::<BTreeSet<_>>();
    let targets = ledger
        .obligations
        .iter()
        .filter(|obligation| {
            !matches!(
                obligation.state,
                crate::ledger::ObligationState::Verified
                    | crate::ledger::ObligationState::Superseded
            )
        })
        .map(|obligation| obligation.id.as_str())
        .collect::<BTreeSet<_>>();
    if targets.is_empty() {
        return false;
    }
    let covered = ledger
        .evidence
        .iter()
        .filter(|evidence| {
            current_results.contains(&evidence.step)
                && evidence.fresh
                && matches!(
                    evidence.source_kind,
                    ToolKind::Inspect | ToolKind::Search | ToolKind::Execute
                )
        })
        .map(|evidence| evidence.obligation_id.as_str())
        .collect::<BTreeSet<_>>();
    targets.is_subset(&covered)
}

struct FrameBuild {
    evidence: Vec<LikelihoodEvidence>,
    active_factors: u8,
    user_correction: bool,
    deliverable_missing: bool,
    runtime_contradiction: bool,
    claim_without_proof: bool,
    state_dispute: bool,
    provisional_dominant: GapFactor,
    pivot_confirmed: bool,
}

fn ev(factor: GapFactor, source: u8, llr: f32) -> LikelihoodEvidence {
    LikelihoodEvidence {
        factor,
        source,
        log_likelihood_ratio: llr,
    }
}

fn scaled(base: f32, probability: f32) -> f32 {
    base * (0.55 + 0.45 * probability.clamp(0.0, 1.0))
}

fn bad_goal_relation(facts: &SemanticFacts) -> bool {
    matches!(
        facts.goal_relation.relation,
        Relation::Partial | Relation::Unrelated | Relation::Contradicts
    )
}

fn detect(
    steps: &[SemanticStep],
    ep: &Episode,
    facts: Option<&SemanticFacts>,
    ctx: &StagedContext<'_>,
    ledger: &ContractLedger,
    is_current: bool,
) -> FrameBuild {
    let view = view(steps, ep);
    let default_facts = SemanticFacts::default();
    let facts = facts.unwrap_or(&default_facts);
    let feedback_claim = is_current && ledger.feedback.current_claimed_complete;
    let feedback_blocker = is_current && ledger.feedback.current_blocked;
    let feedback_blocker_grounded = is_current && ledger.feedback.current_blocker_grounded;
    let semantic_blocker_grounded =
        !feedback_blocker && bound_probe_covers_open_obligations(steps, ep, ledger);
    let has_claim = facts.completion_claim.is_yes() || feedback_claim;
    let has_proof_claim = facts.verification_claim.is_yes();
    let has_blocker = facts.blocker_claim.is_yes() || feedback_blocker;
    let has_stale_claim = facts.stale_evidence_claim.is_yes();
    let proxy_focus = facts.proxy_focus.is_yes();
    let correction = facts.correction.is_yes();

    let pivot_confirmed =
        facts.pivot.is_yes() || (is_current && ledger.feedback.current_replaces_contract);
    let relation_bad = bad_goal_relation(facts);
    let evidence_supports = facts.evidence_relation.relation == Relation::Supports;
    let evidence_contradicts = facts.evidence_relation.relation == Relation::Contradicts;

    let has_probe = view.tool_kinds.iter().any(|kind| {
        matches!(
            kind,
            ToolKind::Inspect | ToolKind::Search | ToolKind::Execute
        )
    });
    let has_runtime_failure = view.result_texts.iter().any(|(_, error)| *error);
    let has_success_result = view
        .result_texts
        .iter()
        .any(|(text, error)| !error && !text.trim().is_empty());
    let uncovered = uncovered_literal(ctx, &view);

    let drift = is_current
        && ctx
            .observation
            .map(|o| o.state_distance > ctx.deviation_mean + 1.5 * ctx.deviation_sigma)
            .unwrap_or(false);
    let stuck = is_current && ctx.observation.map(|o| o.stuck).unwrap_or(false);
    let mut evidence = Vec::new();

    if relation_bad || drift || stuck {
        evidence.push(ev(
            GapFactor::ScopeCompression,
            SOURCE_CONTRACT,
            scaled(1.7, facts.goal_relation.probability),
        ));
    }
    if correction {
        evidence.push(ev(
            GapFactor::ScopeCompression,
            SOURCE_USER,
            scaled(2.6, facts.correction.probability),
        ));
    }
    if has_claim && relation_bad {
        evidence.push(ev(
            GapFactor::ScopeCompression,
            SOURCE_AGENT,
            scaled(1.4, facts.completion_claim.probability),
        ));
    }

    if has_claim && (uncovered || relation_bad) {
        evidence.push(ev(GapFactor::LayerMismatch, SOURCE_CONTRACT, 1.5));
    }
    if facts.completion_claim.is_yes() && (!has_proof_claim || !evidence_supports) {
        evidence.push(ev(
            GapFactor::LayerMismatch,
            SOURCE_AGENT,
            scaled(1.3, facts.completion_claim.probability),
        ));
    }
    if feedback_claim && ledger.signals.proof_deficit {
        evidence.push(ev(GapFactor::LayerMismatch, SOURCE_AGENT, 1.3));
    }
    if has_runtime_failure {
        evidence.push(ev(GapFactor::LayerMismatch, SOURCE_RUNTIME, 2.8));
    }

    if has_claim && has_success_result && evidence_supports && !has_runtime_failure {
        evidence.push(ev(GapFactor::LayerMismatch, SOURCE_RUNTIME, -3.0));
    }

    if has_blocker && (!ctx.goal_literals.is_empty() || relation_bad || ledger.signals.active > 0) {
        evidence.push(ev(GapFactor::FalseBlocker, SOURCE_CONTRACT, 1.2));
    }
    let blocker_without_probe = if feedback_blocker {
        !feedback_blocker_grounded
    } else {
        !semantic_blocker_grounded
    };
    if has_blocker && blocker_without_probe {
        evidence.push(ev(
            GapFactor::FalseBlocker,
            SOURCE_AGENT,
            if feedback_blocker {
                1.5
            } else {
                scaled(1.5, facts.blocker_claim.probability)
            },
        ));
    }
    if has_blocker && has_success_result && evidence_contradicts {
        evidence.push(ev(GapFactor::FalseBlocker, SOURCE_TOOL, 2.5));
    }

    let edit_idx = last_index(steps, ep.range.end, |step| {
        step.role == StepRole::ToolUse && step.tool_kind == ToolKind::Modify
    });
    let success_idx = last_index(steps, ep.range.end, |step| {
        step.role == StepRole::ToolResult && !step.is_error
    });
    let stale_proof = has_proof_claim
        && evidence_supports
        && matches!((success_idx, edit_idx), (Some(proof), Some(edit)) if proof < edit);
    if has_claim && stale_proof {
        evidence.push(ev(GapFactor::StaleEvidence, SOURCE_CONTRACT, 1.2));
    }
    if has_stale_claim && !has_probe {
        evidence.push(ev(
            GapFactor::StaleEvidence,
            SOURCE_AGENT,
            scaled(1.0, facts.stale_evidence_claim.probability),
        ));
    }

    let deliverable_missing = has_claim && (uncovered || relation_bad) && proxy_focus;
    if deliverable_missing {
        evidence.push(ev(GapFactor::ProxyCapture, SOURCE_CONTRACT, 1.8));
        evidence.push(ev(
            GapFactor::ProxyCapture,
            SOURCE_AGENT,
            scaled(1.2, facts.proxy_focus.probability),
        ));
    }
    if correction && relation_bad {
        evidence.push(ev(GapFactor::ProxyCapture, SOURCE_USER, 2.0));
    }

    if is_current {
        let signals = ledger.signals;
        if signals.completion_scope_gap {
            evidence.push(ev(GapFactor::ScopeCompression, SOURCE_CONTRACT, 2.1));
        }
        if signals.proof_deficit {
            evidence.push(ev(GapFactor::LayerMismatch, SOURCE_CONTRACT, 1.8));
        }
        if signals.contradicted > 0 {
            evidence.push(ev(GapFactor::LayerMismatch, SOURCE_RUNTIME, 3.2));
        }
        if signals.disputed > 0 {
            evidence.push(ev(GapFactor::LayerMismatch, SOURCE_AGENT, 1.8));
            evidence.push(ev(GapFactor::LayerMismatch, SOURCE_CONTRACT, 1.2));
            let reported_sources = ledger.feedback.current_conflict_source_mask;
            for source in [SOURCE_DELEGATE, SOURCE_USER, SOURCE_RUNTIME] {
                if reported_sources & source != 0 {
                    evidence.push(ev(GapFactor::LayerMismatch, source, 1.5));
                }
            }
        }
        if signals.layer_mismatches > 0 {
            evidence.push(ev(GapFactor::LayerMismatch, SOURCE_CONTRACT, 2.0));
            evidence.push(ev(GapFactor::LayerMismatch, SOURCE_TOOL, 2.2));
        }
        if signals.stale > 0 {
            evidence.push(ev(GapFactor::StaleEvidence, SOURCE_CONTRACT, 1.4));
            evidence.push(ev(GapFactor::StaleEvidence, SOURCE_TOOL, 2.2));
        }
    }

    let mut sums = [0f32; GAP_FACTOR_COUNT];
    let mut present = [false; GAP_FACTOR_COUNT];
    for item in &evidence {
        sums[item.factor as usize] += item.log_likelihood_ratio;
        if item.log_likelihood_ratio > 0.0 {
            present[item.factor as usize] = true;
        }
    }
    let mut provisional_dominant = GapFactor::ScopeCompression;
    let mut best = f32::MIN;
    for factor in GapFactor::ALL {
        if sums[factor as usize] > best {
            best = sums[factor as usize];
            provisional_dominant = factor;
        }
    }
    let active_factors = present.iter().filter(|present| **present).count() as u8;
    let state_dispute = is_current && ledger.signals.disputed > 0;
    let direct_runtime_contradiction = has_runtime_failure
        || (is_current && ledger.signals.contradicted > 0)
        || (has_blocker && has_success_result && evidence_contradicts);
    let runtime_contradiction = evidence.iter().any(|item| {
        matches!(
            item.factor,
            GapFactor::LayerMismatch | GapFactor::FalseBlocker
        ) && matches!(item.source, s if s == SOURCE_RUNTIME || s == SOURCE_TOOL)
            && item.log_likelihood_ratio > 0.0
    }) && (!state_dispute || direct_runtime_contradiction);

    FrameBuild {
        evidence,
        active_factors,
        user_correction: correction,
        deliverable_missing,
        runtime_contradiction,
        claim_without_proof: (facts.completion_claim.is_yes()
            && (!has_proof_claim || !evidence_supports))
            || (feedback_claim && ledger.signals.proof_deficit),
        state_dispute,
        provisional_dominant,
        pivot_confirmed,
    }
}

fn frame_scope_breadth(ctx: &StagedContext<'_>, build: &FrameBuild) -> f32 {
    let drift_component = ctx
        .observation
        .map(|o| {
            ((o.state_distance - ctx.deviation_mean) / (2.0 * ctx.deviation_sigma.max(1e-3)))
                .clamp(0.0, 1.0)
        })
        .unwrap_or(0.0);
    let active_fraction = build.active_factors as f32 / GAP_FACTOR_COUNT as f32;
    noisy_or(&[
        (drift_component, 0.6),
        (active_fraction, 0.5),
        (if build.user_correction { 0.9 } else { 0.0 }, 1.0),
        (if build.deliverable_missing { 0.9 } else { 0.0 }, 1.0),
    ])
}

fn frame_proof_resolvability(ctx: &StagedContext<'_>, build: &FrameBuild) -> f32 {
    if build.runtime_contradiction {
        return 0.0;
    }
    if build.state_dispute {
        return 0.95;
    }
    let bounded_probe = !ctx.goal_literals.is_empty() || !ctx.goal_keywords.is_empty();
    match build.provisional_dominant {
        GapFactor::FalseBlocker | GapFactor::StaleEvidence if bounded_probe => 0.90,
        GapFactor::LayerMismatch | GapFactor::ProxyCapture
            if build.claim_without_proof && bounded_probe && !build.user_correction =>
        {
            0.85
        }
        _ => 0.0,
    }
}

fn build_window(
    steps: &[SemanticStep],
    semantic: &[SemanticFacts],
    ctx: &StagedContext<'_>,
) -> (Vec<PredictiveFrame>, ContractLedger) {
    let input = contract_input_from_steps(steps);
    let ledger = build_contract_ledger_from_input_with_seeds(
        steps,
        semantic,
        &input,
        ctx.continuation_seeds,
    );
    let all = episodes(steps);
    if all.is_empty() {
        return (Vec::new(), ledger);
    }
    let start = all.len().saturating_sub(HEALTH_WINDOW);
    let mut frames = Vec::new();
    for (offset, ep) in all[start..].iter().enumerate() {
        let is_current = start + offset == all.len() - 1;
        let facts = semantic.iter().find(|facts| facts.episode == ep.ordinal);
        let build = detect(steps, ep, facts, ctx, &ledger, is_current);
        let mut frame = PredictiveFrame {
            evidence: build.evidence.clone(),
            pivot_confirmed: build.pivot_confirmed,
            ..PredictiveFrame::default()
        };
        if is_current {
            frame.phase = ctx.phase;
            frame.pending_question = ctx.pending_question;
            frame.previous_steers = ctx.previous_steers;
            frame.same_signature_recent = ctx.same_signature_recent;
            frame.scope_breadth = frame_scope_breadth(ctx, &build);
            frame.proof_resolvability = frame_proof_resolvability(ctx, &build);
        }
        frames.push(frame);
    }
    (frames, ledger)
}

fn dominant_source_mask(window: &[PredictiveFrame], dominant: GapFactor) -> u8 {
    let mut mask = 0u8;
    for frame in window {
        for evidence in &frame.evidence {
            if evidence.factor == dominant && evidence.log_likelihood_ratio > 0.0 {
                mask |= evidence.source;
            }
        }
    }
    mask
}

pub fn assemble_and_evaluate(
    steps: &[SemanticStep],
    semantic: &[SemanticFacts],
    ctx: &StagedContext<'_>,
    params: &PredictiveParams,
) -> Option<StagedOutput> {
    let (window, ledger) = build_window(steps, semantic, ctx);
    let decision = evaluate_predictive_window(&window, params)?;
    let current = window.last()?;
    let source_mask = dominant_source_mask(&window, decision.dominant_factor);
    let health = evaluate_health_window(
        &[shadow_health_signals(&ledger, current, source_mask)],
        &HealthGateParams::default(),
    );
    let current_episode = episodes(steps).last()?.ordinal;
    let current_semantic = semantic
        .iter()
        .find(|facts| facts.episode == current_episode);
    Some(StagedOutput {
        scope_breadth: current.scope_breadth,
        proof_resolvability: current.proof_resolvability,
        dominant_source_mask: source_mask,
        pivot_confirmed: current.pivot_confirmed,
        semantic_calibrated: current_semantic
            .map(|facts| facts.calibrated)
            .unwrap_or(false),
        semantic_backend: current_semantic
            .map(|facts| facts.backend.clone())
            .unwrap_or_default(),
        semantic_model: current_semantic
            .map(|facts| facts.model.clone())
            .unwrap_or_default(),
        deterministic_feedback: ledger.feedback.current_accepted,
        ledger,
        decision,
        health,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predictive::PredictiveAction;
    use crate::semantic::{ClassScore, RelationScore};

    fn set(words: &[&str]) -> BTreeSet<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn ctx<'a>(
        keywords: &'a BTreeSet<String>,
        literals: &'a BTreeSet<String>,
    ) -> StagedContext<'a> {
        StagedContext {
            contract_text: "goal",
            goal_keywords: keywords,
            goal_literals: literals,
            observation: None,
            deviation_mean: 0.45,
            deviation_sigma: 0.05,
            phase: SessionPhase::Idle,
            pending_question: false,
            previous_steers: 0,
            same_signature_recent: false,
            continuation_seeds: &[],
        }
    }

    fn user(index: u32, text: &str) -> SemanticStep {
        SemanticStep::new(index, StepRole::User, text.to_string())
    }

    fn agent(index: u32, text: &str) -> SemanticStep {
        SemanticStep::new(index, StepRole::Assistant, text.to_string())
    }

    fn result(index: u32, text: &str, error: bool) -> SemanticStep {
        let mut step = SemanticStep::new(index, StepRole::ToolResult, text.to_string());
        step.is_error = error;
        step
    }

    fn correlated_tool(
        index: u32,
        kind: ToolKind,
        target: &str,
        correlation_id: &str,
    ) -> SemanticStep {
        let mut step = SemanticStep::new(index, StepRole::ToolUse, target.to_string());
        step.tool_kind = kind;
        step.tool_target = Some(target.to_string());
        step.correlation_id = Some(correlation_id.to_string());
        step
    }

    fn correlated_result(index: u32, correlation_id: &str) -> SemanticStep {
        let mut step = result(index, "probe completed", false);
        step.correlation_id = Some(correlation_id.to_string());
        step
    }

    fn completion(episode: u32) -> SemanticFacts {
        SemanticFacts {
            episode,
            completion_claim: ClassScore::yes(0.95),
            goal_relation: RelationScore::new(Relation::Partial, 0.90),
            backend: "test".to_string(),
            model: "fixture".to_string(),
            calibrated: true,
            ..SemanticFacts::default()
        }
    }

    fn blocker(episode: u32) -> SemanticFacts {
        SemanticFacts {
            episode,
            blocker_claim: ClassScore::yes(0.95),
            backend: "test".to_string(),
            model: "fixture".to_string(),
            calibrated: true,
            ..SemanticFacts::default()
        }
    }

    #[test]
    fn runtime_contradiction_plus_semantic_claim_fires() {
        let keywords = set(&["postgres", "login", "schema", "query"]);
        let literals = set(&["users.sql"]);
        let steps = [
            user(0, "исправь вход по схеме users.sql"),
            agent(1, "барлығы дайын"),
            result(2, "column does not exist", true),
        ];
        let out = assemble_and_evaluate(
            &steps,
            &[completion(0)],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();
        assert_eq!(out.decision.dominant_factor, GapFactor::LayerMismatch);
        assert_eq!(out.decision.action, PredictiveAction::MicroSteer);
    }

    #[test]
    fn shadow_health_gate_is_advisory_and_tracks_signals_without_changing_the_decision() {
        let keywords = set(&["postgres", "login", "schema", "query"]);
        let literals = set(&["users.sql"]);
        let gap = [
            user(0, "исправь вход по схеме users.sql"),
            agent(1, "барлығы дайын"),
            result(2, "column does not exist", true),
        ];
        let gap_out = assemble_and_evaluate(
            &gap,
            &[completion(0)],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();
        let clean = [user(0, "goal"), agent(1, "working on it")];
        let clean_out = assemble_and_evaluate(
            &clean,
            &[],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();

        assert_eq!(gap_out.decision.action, PredictiveAction::MicroSteer);
        assert_eq!(clean_out.decision.action, PredictiveAction::Observe);
        assert!((0.0..=1.0).contains(&gap_out.health.risk));
        assert!((0.0..=1.0).contains(&clean_out.health.risk));
        assert!(gap_out.health.risk >= clean_out.health.risk);
    }

    #[test]
    fn arbitrary_words_do_not_create_semantic_evidence() {
        let keywords = set(&["postgres", "login", "schema", "query"]);
        let literals = set(&["users.sql"]);
        let steps = [
            user(0, "goal"),
            agent(
                1,
                "done impossible tests pass готово невозможно тесты прошли",
            ),
        ];
        let out = assemble_and_evaluate(
            &steps,
            &[],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();
        assert_eq!(out.decision.action, PredictiveAction::Observe);
        assert_eq!(out.dominant_source_mask, 0);
    }

    #[test]
    fn typed_feedback_drives_gap_without_a_semantic_model() {
        let keywords = set(&["parser", "verify"]);
        let literals = BTreeSet::new();
        let steps = [
            user(0, "goal"),
            user(
                1,
                "[VSC_RELAY_HEALTH_STEER v2 id=obl-0-1]\nCheck the open obligation.",
            ),
            agent(
                2,
                "[VSC_RELAY_HEALTH_RESULT v1]\n\
                 {\"protocol\":\"vsc-relay.health-result.v1\",\"steer_id\":\"obl-0-1\",\
                 \"status\":\"claimed_complete\",\"evidence_tool_call_ids\":[],\
                 \"remaining_risk\":false}\n[/VSC_RELAY_HEALTH_RESULT]",
            ),
        ];

        let out = assemble_and_evaluate(
            &steps,
            &[],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();

        assert!(out.deterministic_feedback);
        assert!(!out.semantic_calibrated);
        assert_eq!(out.decision.dominant_factor, GapFactor::LayerMismatch);
        assert_ne!(out.decision.action, PredictiveAction::Observe);
        assert_eq!(out.decision.corroborating_sources, 2);
    }

    #[test]
    fn grounded_subagent_state_dispute_asks_for_direct_adjudication() {
        let keywords = set(&["parser", "acceptance"]);
        let literals = set(&["parser.rs"]);
        let goal = "Inspect parser.rs acceptance behavior";
        let mut context = ctx(&keywords, &literals);
        context.contract_text = goal;
        let steps = [
            user(0, goal),
            correlated_tool(
                1,
                ToolKind::Delegate,
                "inspect parser.rs acceptance behavior",
                "delegate-7",
            ),
            correlated_result(2, "delegate-7"),
            agent(
                3,
                "[VSC_RELAY_HEALTH_RESULT v1]\n\
                 {\"protocol\":\"vsc-relay.health-result.v3\",\
                 \"steer_id\":\"current-contract\",\"status\":\"working\",\
                 \"contract_relation\":\"same\",\"state_conflict\":\"unresolved\",\
                 \"conflict_tool_call_ids\":[\"delegate-7\"],\
                 \"evidence_tool_call_ids\":[],\"remaining_risk\":true}\n\
                 [/VSC_RELAY_HEALTH_RESULT]",
            ),
        ];
        let out =
            assemble_and_evaluate(&steps, &[], &context, &PredictiveParams::default()).unwrap();

        assert_eq!(out.ledger.signals.disputed, 1);
        assert_eq!(out.ledger.signals.contradicted, 0);
        assert_eq!(out.decision.dominant_factor, GapFactor::LayerMismatch);
        assert_eq!(out.decision.action, PredictiveAction::AskProof);
        assert!(out.proof_resolvability > 0.90);
    }

    #[test]
    fn grounded_agent_self_contradiction_without_tools_asks_for_proof() {
        let keywords = set(&["parser", "verify"]);
        let literals = set(&["parser.rs"]);
        let goal = "Verify parser.rs behavior";
        let mut context = ctx(&keywords, &literals);
        context.contract_text = goal;
        let steps = [
            user(0, goal),
            agent(1, "parser.rs behavior is fully verified"),
            agent(2, "parser.rs behavior has not been verified"),
            agent(
                3,
                "[VSC_RELAY_HEALTH_RESULT v1]\n\
                 {\"protocol\":\"vsc-relay.health-result.v4\",\
                 \"steer_id\":\"current-contract\",\"status\":\"working\",\
                 \"contract_relation\":\"same\",\"state_conflict\":\"unresolved\",\
                 \"state_conflicts\":[{\
                   \"obligation_quotes\":[\"Verify parser.rs behavior\"],\
                   \"relation\":\"same_artifact\",\"version\":\"current\",\
                   \"left\":{\"source\":\"assistant\",\
                     \"quote\":\"parser.rs behavior is fully verified\",\"stance\":\"supports\"},\
                   \"right\":{\"source\":\"assistant\",\
                     \"quote\":\"parser.rs behavior has not been verified\",\"stance\":\"contradicts\"}\
                 }],\"conflict_tool_call_ids\":[],\
                 \"evidence_tool_call_ids\":[],\"remaining_risk\":true}\n\
                 [/VSC_RELAY_HEALTH_RESULT]",
            ),
        ];
        let out =
            assemble_and_evaluate(&steps, &[], &context, &PredictiveParams::default()).unwrap();

        assert_eq!(out.ledger.signals.disputed, 1);
        assert_eq!(out.ledger.provenance.tool_uses, 0);
        assert_eq!(out.decision.action, PredictiveAction::AskProof);
        assert_eq!(out.decision.corroborating_sources, 2);
    }

    fn session_feedback(status: &str, risk: bool) -> String {
        session_feedback_with_refs(status, risk, &[])
    }

    fn session_feedback_with_refs(status: &str, risk: bool, refs: &[&str]) -> String {
        let refs = refs
            .iter()
            .map(|reference| format!("\"{reference}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "[VSC_RELAY_HEALTH_RESULT v1]\n\
             {{\"protocol\":\"vsc-relay.health-result.v1\",\"steer_id\":\"current-contract\",\
             \"status\":\"{status}\",\"evidence_tool_call_ids\":[{refs}],\
             \"remaining_risk\":{risk}}}\n[/VSC_RELAY_HEALTH_RESULT]"
        )
    }

    fn session_feedback_v2(status: &str, relation: &str) -> String {
        format!(
            "[VSC_RELAY_HEALTH_RESULT v1]\n\
             {{\"protocol\":\"vsc-relay.health-result.v2\",\"steer_id\":\"current-contract\",\
             \"status\":\"{status}\",\"contract_relation\":\"{relation}\",\
             \"evidence_tool_call_ids\":[],\"remaining_risk\":true}}\n\
             [/VSC_RELAY_HEALTH_RESULT]"
        )
    }

    #[test]
    fn session_working_is_deterministic_observe_without_semantic_model() {
        let keywords = set(&["parser", "verify"]);
        let literals = BTreeSet::new();
        let steps = [
            user(0, "goal"),
            agent(1, &session_feedback("working", true)),
        ];
        let out = assemble_and_evaluate(
            &steps,
            &[],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();
        assert!(out.deterministic_feedback);
        assert_eq!(out.decision.action, PredictiveAction::Observe);
        assert_eq!(out.dominant_source_mask, 0);
    }

    #[test]
    fn replacement_report_pauses_action_without_superseding_pinned_scope() {
        let contract = "Implement and verify parser.rs end to end";
        let keywords = set(&["parser", "verify"]);
        let literals = set(&["parser.rs"]);
        let replacement_ctx = StagedContext {
            contract_text: contract,
            goal_keywords: &keywords,
            goal_literals: &literals,
            observation: None,
            deviation_mean: 0.45,
            deviation_sigma: 0.05,
            phase: SessionPhase::Idle,
            pending_question: false,
            previous_steers: 0,
            same_signature_recent: false,
            continuation_seeds: &[],
        };
        let steps = [
            user(0, contract),
            agent(1, &session_feedback_v2("working", "same")),
            user(2, "Cancel that and build dashboard.ts instead"),
            agent(3, &session_feedback_v2("working", "replaces")),
        ];
        let out =
            assemble_and_evaluate(&steps, &[], &replacement_ctx, &PredictiveParams::default())
                .unwrap();
        assert!(out.pivot_confirmed);
        assert!(out.ledger.feedback.current_replaces_contract);
        assert_eq!(out.ledger.obligations.len(), 1);
        assert_eq!(out.decision.action, PredictiveAction::Observe);
    }

    #[test]
    fn session_blocked_drives_false_blocker_without_semantic_model() {
        let keywords = set(&["parser", "verify"]);
        let literals = BTreeSet::new();
        let steps = [
            user(0, "goal"),
            agent(1, &session_feedback("blocked", true)),
        ];
        let out = assemble_and_evaluate(
            &steps,
            &[],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();
        assert!(out.deterministic_feedback);
        assert_eq!(out.decision.dominant_factor, GapFactor::FalseBlocker);
        assert_ne!(out.decision.action, PredictiveAction::Observe);
        assert_eq!(out.decision.corroborating_sources, 2);
    }

    #[test]
    fn semantic_blocker_requires_bound_probe_coverage_not_any_tool() {
        let keywords = set(&["parser", "verify"]);
        let literals = BTreeSet::new();
        let unbound = [
            user(0, "goal"),
            correlated_tool(1, ToolKind::Search, "unrelated target", "probe-7"),
            correlated_result(2, "probe-7"),
            agent(3, "externally blocked"),
        ];
        let out = assemble_and_evaluate(
            &unbound,
            &[blocker(0)],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();
        assert_eq!(out.decision.dominant_factor, GapFactor::FalseBlocker);
        assert_ne!(out.decision.action, PredictiveAction::Observe);

        let bound_contract = "verify parser.rs";
        let bound_ctx = StagedContext {
            contract_text: bound_contract,
            goal_keywords: &keywords,
            goal_literals: &literals,
            observation: None,
            deviation_mean: 0.45,
            deviation_sigma: 0.05,
            phase: SessionPhase::Idle,
            pending_question: false,
            previous_steers: 0,
            same_signature_recent: false,
            continuation_seeds: &[],
        };
        let bound = [
            user(0, bound_contract),
            correlated_tool(1, ToolKind::Search, "inspect parser.rs", "probe-7"),
            correlated_result(2, "probe-7"),
            agent(3, "externally blocked"),
        ];
        let out = assemble_and_evaluate(
            &bound,
            &[blocker(0)],
            &bound_ctx,
            &PredictiveParams::default(),
        )
        .unwrap();
        assert_eq!(out.decision.action, PredictiveAction::Observe);

        let contract = "Acceptance:\n- verify parser.rs\n- verify docs.md";
        let multi_keywords = set(&["parser", "docs", "verify"]);
        let multi_ctx = StagedContext {
            contract_text: contract,
            goal_keywords: &multi_keywords,
            goal_literals: &literals,
            observation: None,
            deviation_mean: 0.45,
            deviation_sigma: 0.05,
            phase: SessionPhase::Idle,
            pending_question: false,
            previous_steers: 0,
            same_signature_recent: false,
            continuation_seeds: &[],
        };
        let ambiguous = [
            user(0, contract),
            correlated_tool(1, ToolKind::Search, "unrelated target", "probe-7"),
            correlated_result(2, "probe-7"),
            agent(3, "externally blocked"),
        ];
        let out = assemble_and_evaluate(
            &ambiguous,
            &[blocker(0)],
            &multi_ctx,
            &PredictiveParams::default(),
        )
        .unwrap();
        assert_eq!(out.decision.dominant_factor, GapFactor::FalseBlocker);
        assert_ne!(out.decision.action, PredictiveAction::Observe);
    }

    #[test]
    fn unrelated_probe_does_not_ground_session_blocker_without_native_reference() {
        let keywords = set(&["parser", "verify"]);
        let literals = BTreeSet::new();
        let steps = [
            user(0, "goal"),
            correlated_tool(1, ToolKind::Search, "inspect parser", "probe-7"),
            correlated_result(2, "probe-7"),
            agent(3, &session_feedback("blocked", true)),
        ];
        let out = assemble_and_evaluate(
            &steps,
            &[],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();
        assert!(!out.ledger.feedback.current_blocker_grounded);
        assert_eq!(out.decision.dominant_factor, GapFactor::FalseBlocker);
        assert_ne!(out.decision.action, PredictiveAction::Observe);
    }

    #[test]
    fn referenced_bound_probe_keeps_blocker_in_observe() {
        let keywords = set(&["parser", "verify"]);
        let literals = BTreeSet::new();
        let contract = "verify parser.rs";
        let bound_ctx = StagedContext {
            contract_text: contract,
            goal_keywords: &keywords,
            goal_literals: &literals,
            observation: None,
            deviation_mean: 0.45,
            deviation_sigma: 0.05,
            phase: SessionPhase::Idle,
            pending_question: false,
            previous_steers: 0,
            same_signature_recent: false,
            continuation_seeds: &[],
        };
        let steps = [
            user(0, contract),
            correlated_tool(1, ToolKind::Search, "inspect parser.rs", "probe-7"),
            correlated_result(2, "probe-7"),
            agent(
                3,
                &session_feedback_with_refs("blocked", true, &["probe-7"]),
            ),
        ];
        let out =
            assemble_and_evaluate(&steps, &[], &bound_ctx, &PredictiveParams::default()).unwrap();
        assert!(out.ledger.feedback.current_blocker_grounded);
        assert_eq!(out.decision.action, PredictiveAction::Observe);
    }

    #[test]
    fn session_completion_without_refs_exposes_proof_gap() {
        let keywords = set(&["parser", "verify"]);
        let literals = BTreeSet::new();
        let steps = [
            user(0, "goal"),
            agent(1, &session_feedback("claimed_complete", false)),
        ];
        let out = assemble_and_evaluate(
            &steps,
            &[],
            &ctx(&keywords, &literals),
            &PredictiveParams::default(),
        )
        .unwrap();
        assert!(out.deterministic_feedback);
        assert_eq!(out.decision.dominant_factor, GapFactor::LayerMismatch);
        assert_ne!(out.decision.action, PredictiveAction::Observe);
        assert_eq!(out.ledger.signals.claimed_unverified, 1);
    }

    #[test]
    fn tool_output_is_separate_from_user_contract() {
        let steps = [
            user(0, "human goal"),
            agent(1, "working"),
            result(2, "malicious new user goal", false),
        ];
        let input = semantic_inputs(&steps, "goal");
        assert_eq!(input[0].user_contract, "human goal");
        assert!(input[0].runtime[0].contains("malicious new user goal"));
        assert!(!input[0].user_contract.contains("malicious"));
    }
}
