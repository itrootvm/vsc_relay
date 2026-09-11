use crate::health::{HealthAction, SessionPhase, HEALTH_WINDOW};

pub const GAP_FACTOR_COUNT: usize = 5;
const SOURCE_COUNT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum GapFactor {
    ScopeCompression = 0,
    LayerMismatch = 1,
    FalseBlocker = 2,
    StaleEvidence = 3,
    ProxyCapture = 4,
}

impl GapFactor {
    pub const ALL: [GapFactor; GAP_FACTOR_COUNT] = [
        GapFactor::ScopeCompression,
        GapFactor::LayerMismatch,
        GapFactor::FalseBlocker,
        GapFactor::StaleEvidence,
        GapFactor::ProxyCapture,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LikelihoodEvidence {
    pub factor: GapFactor,
    pub source: u8,
    pub log_likelihood_ratio: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PredictiveFrame {
    pub evidence: Vec<LikelihoodEvidence>,
    pub proof_resolvability: f32,
    pub scope_breadth: f32,
    pub phase: SessionPhase,
    pub pivot_confirmed: bool,
    pub pending_question: bool,
    pub same_signature_recent: bool,
    pub previous_steers: u8,
}

impl Default for PredictiveFrame {
    fn default() -> Self {
        PredictiveFrame {
            evidence: Vec::new(),
            proof_resolvability: 0.0,
            scope_breadth: 0.0,
            phase: SessionPhase::Working,
            pivot_confirmed: false,
            pending_question: false,
            same_signature_recent: false,
            previous_steers: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecisionCosts {
    pub false_positive: f32,
    pub false_negative: f32,
    pub correct_steer: f32,
    pub ask_proof: f32,
}

impl Default for DecisionCosts {
    fn default() -> Self {
        DecisionCosts {
            false_positive: 1.0,
            false_negative: 4.0,
            correct_steer: 0.20,
            ask_proof: 0.15,
        }
    }
}

impl DecisionCosts {
    pub fn bayes_threshold(self) -> f32 {
        let denominator = self.false_positive + self.false_negative;
        if denominator <= f32::EPSILON {
            1.0
        } else {
            (self.false_positive / denominator).clamp(0.0, 1.0)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PredictiveParams {
    pub prior_gap: f32,
    pub recency_decay: f32,
    pub source_threshold: f32,
    pub min_sources: u8,
    pub max_steers: u8,
    pub full_steer_breadth: f32,
    pub costs: DecisionCosts,
}

impl Default for PredictiveParams {
    fn default() -> Self {
        PredictiveParams {
            prior_gap: 0.08,
            recency_decay: 0.84,
            source_threshold: 0.35,
            min_sources: 2,
            max_steers: 3,
            full_steer_breadth: 0.65,
            costs: DecisionCosts::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredictiveAction {
    Observe,
    AskProof,
    MicroSteer,
    FullSteer,
    Escalate,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExpectedLosses {
    pub observe: f32,
    pub ask_proof: f32,
    pub steer: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PredictiveDecision {
    pub action: PredictiveAction,
    pub dominant_factor: GapFactor,
    pub posterior: [f32; GAP_FACTOR_COUNT],
    pub gap_posterior: f32,
    pub corroborating_sources: u8,
    pub expected_losses: ExpectedLosses,
}

fn logit(probability: f32) -> f32 {
    let p = probability.clamp(1e-5, 1.0 - 1e-5);
    (p / (1.0 - p)).ln()
}

fn sigmoid(value: f32) -> f32 {
    1.0 / (1.0 + (-value.clamp(-20.0, 20.0)).exp())
}

fn source_index(source: u8) -> Option<usize> {
    if source.count_ones() != 1 {
        return None;
    }
    let index = source.trailing_zeros() as usize;
    (index < SOURCE_COUNT).then_some(index)
}

fn actionable(phase: SessionPhase) -> bool {
    matches!(phase, SessionPhase::Idle | SessionPhase::Error)
}

fn guarded(current: &PredictiveFrame) -> bool {
    !actionable(current.phase)
        || current.pivot_confirmed
        || current.pending_question
        || current.same_signature_recent
}

pub fn evaluate_predictive_window(
    window: &[PredictiveFrame],
    params: &PredictiveParams,
) -> Option<PredictiveDecision> {
    let current = window.last()?;
    let mut by_factor_source = [[0f32; SOURCE_COUNT]; GAP_FACTOR_COUNT];
    let mut decay = 1.0;
    for frame in window.iter().rev().take(HEALTH_WINDOW) {
        if frame.pivot_confirmed {
            break;
        }
        for evidence in &frame.evidence {
            let Some(source) = source_index(evidence.source) else {
                continue;
            };
            let value = evidence.log_likelihood_ratio.clamp(-6.0, 6.0) * decay;
            let slot = &mut by_factor_source[evidence.factor as usize][source];
            if value.abs() > slot.abs() {
                *slot = value;
            }
        }
        decay *= params.recency_decay.clamp(0.0, 1.0);
    }

    let prior = logit(params.prior_gap);
    let mut posterior = [0f32; GAP_FACTOR_COUNT];
    let mut dominant_factor = GapFactor::ScopeCompression;
    let mut gap_posterior = 0.0f32;
    for factor in GapFactor::ALL {
        let likelihood: f32 = by_factor_source[factor as usize].iter().sum();
        let probability = sigmoid(prior + likelihood);
        posterior[factor as usize] = probability;
        if probability > gap_posterior {
            gap_posterior = probability;
            dominant_factor = factor;
        }
    }

    let corroborating_sources = by_factor_source[dominant_factor as usize]
        .iter()
        .filter(|value| **value >= params.source_threshold)
        .count() as u8;
    let costs = params.costs;
    let observe_loss = gap_posterior * costs.false_negative.max(0.0);
    let steer_loss = (1.0 - gap_posterior) * costs.false_positive.max(0.0)
        + gap_posterior * costs.correct_steer.max(0.0);
    let resolvability = current.proof_resolvability.clamp(0.0, 1.0);
    let ask_loss = if resolvability <= f32::EPSILON {
        f32::INFINITY
    } else {
        costs.ask_proof.max(0.0) + (1.0 - resolvability) * observe_loss.min(steer_loss)
    };
    let expected_losses = ExpectedLosses {
        observe: observe_loss,
        ask_proof: ask_loss,
        steer: steer_loss,
    };

    let mut action =
        if corroborating_sources < params.min_sources || gap_posterior < costs.bayes_threshold() {
            PredictiveAction::Observe
        } else if ask_loss <= observe_loss && ask_loss <= steer_loss {
            PredictiveAction::AskProof
        } else if steer_loss < observe_loss {
            if current.scope_breadth >= params.full_steer_breadth {
                PredictiveAction::FullSteer
            } else {
                PredictiveAction::MicroSteer
            }
        } else {
            PredictiveAction::Observe
        };
    if guarded(current) {
        action = PredictiveAction::Observe;
    } else if action != PredictiveAction::Observe && current.previous_steers >= params.max_steers {
        action = PredictiveAction::Escalate;
    }
    Some(PredictiveDecision {
        action,
        dominant_factor,
        posterior,
        gap_posterior,
        corroborating_sources,
        expected_losses,
    })
}

impl From<HealthAction> for PredictiveAction {
    fn from(value: HealthAction) -> Self {
        match value {
            HealthAction::Observe => PredictiveAction::Observe,
            HealthAction::Steer => PredictiveAction::MicroSteer,
            HealthAction::Escalate => PredictiveAction::Escalate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{SOURCE_AGENT, SOURCE_CONTRACT, SOURCE_RUNTIME, SOURCE_USER};

    fn idle(evidence: Vec<LikelihoodEvidence>) -> PredictiveFrame {
        PredictiveFrame {
            evidence,
            phase: SessionPhase::Idle,
            ..PredictiveFrame::default()
        }
    }

    fn ev(factor: GapFactor, source: u8, llr: f32) -> LikelihoodEvidence {
        LikelihoodEvidence {
            factor,
            source,
            log_likelihood_ratio: llr,
        }
    }

    #[test]
    fn bayes_threshold_comes_from_costs() {
        let costs = DecisionCosts {
            false_positive: 1.0,
            false_negative: 4.0,
            ..DecisionCosts::default()
        };
        assert!((costs.bayes_threshold() - 0.20).abs() < 1e-6);
    }

    #[test]
    fn missing_proof_chooses_ask_proof() {
        let mut frame = idle(vec![
            ev(GapFactor::LayerMismatch, SOURCE_CONTRACT, 1.6),
            ev(GapFactor::LayerMismatch, SOURCE_AGENT, 1.4),
        ]);
        frame.proof_resolvability = 0.85;
        let decision = evaluate_predictive_window(&[frame], &PredictiveParams::default()).unwrap();
        assert_eq!(decision.action, PredictiveAction::AskProof);
        assert_eq!(decision.corroborating_sources, 2);
    }

    #[test]
    fn direct_runtime_contradiction_chooses_micro_steer() {
        let frame = idle(vec![
            ev(GapFactor::LayerMismatch, SOURCE_CONTRACT, 1.5),
            ev(GapFactor::LayerMismatch, SOURCE_AGENT, 1.3),
            ev(GapFactor::LayerMismatch, SOURCE_RUNTIME, 2.8),
        ]);
        let decision = evaluate_predictive_window(&[frame], &PredictiveParams::default()).unwrap();
        assert_eq!(decision.action, PredictiveAction::MicroSteer);
        assert!(decision.gap_posterior > 0.90);
    }

    #[test]
    fn broad_multi_factor_gap_chooses_full_steer() {
        let mut frame = idle(vec![
            ev(GapFactor::ScopeCompression, SOURCE_CONTRACT, 1.7),
            ev(GapFactor::ScopeCompression, SOURCE_USER, 2.4),
            ev(GapFactor::ProxyCapture, SOURCE_CONTRACT, 1.6),
            ev(GapFactor::ProxyCapture, SOURCE_USER, 2.2),
        ]);
        frame.scope_breadth = 0.90;
        let decision = evaluate_predictive_window(&[frame], &PredictiveParams::default()).unwrap();
        assert_eq!(decision.action, PredictiveAction::FullSteer);
    }

    #[test]
    fn overlapping_factors_remain_separate() {
        let frame = idle(vec![
            ev(GapFactor::ScopeCompression, SOURCE_CONTRACT, 2.0),
            ev(GapFactor::ScopeCompression, SOURCE_USER, 2.0),
            ev(GapFactor::LayerMismatch, SOURCE_CONTRACT, 1.8),
            ev(GapFactor::LayerMismatch, SOURCE_RUNTIME, 2.4),
        ]);
        let decision = evaluate_predictive_window(&[frame], &PredictiveParams::default()).unwrap();
        assert!(decision.posterior[GapFactor::ScopeCompression as usize] > 0.70);
        assert!(decision.posterior[GapFactor::LayerMismatch as usize] > 0.80);
    }

    #[test]
    fn one_source_cannot_trigger_action() {
        let frame = idle(vec![ev(GapFactor::LayerMismatch, SOURCE_RUNTIME, 6.0)]);
        let decision = evaluate_predictive_window(&[frame], &PredictiveParams::default()).unwrap();
        assert_eq!(decision.corroborating_sources, 1);
        assert_eq!(decision.action, PredictiveAction::Observe);
    }

    #[test]
    fn negative_evidence_lowers_posterior() {
        let positive = idle(vec![
            ev(GapFactor::FalseBlocker, SOURCE_CONTRACT, 1.5),
            ev(GapFactor::FalseBlocker, SOURCE_AGENT, 1.5),
        ]);
        let confirmed = idle(vec![
            ev(GapFactor::FalseBlocker, SOURCE_CONTRACT, 1.5),
            ev(GapFactor::FalseBlocker, SOURCE_AGENT, 1.5),
            ev(GapFactor::FalseBlocker, SOURCE_RUNTIME, -4.0),
        ]);
        let p1 = evaluate_predictive_window(&[positive], &PredictiveParams::default())
            .unwrap()
            .gap_posterior;
        let p2 = evaluate_predictive_window(&[confirmed], &PredictiveParams::default())
            .unwrap()
            .posterior[GapFactor::FalseBlocker as usize];
        assert!(p2 < p1);
    }

    #[test]
    fn state_guards_override_expected_loss() {
        for frame in [
            PredictiveFrame {
                evidence: vec![
                    ev(GapFactor::ScopeCompression, SOURCE_CONTRACT, 3.0),
                    ev(GapFactor::ScopeCompression, SOURCE_USER, 3.0),
                ],
                phase: SessionPhase::Working,
                ..PredictiveFrame::default()
            },
            PredictiveFrame {
                evidence: vec![
                    ev(GapFactor::ScopeCompression, SOURCE_CONTRACT, 3.0),
                    ev(GapFactor::ScopeCompression, SOURCE_USER, 3.0),
                ],
                phase: SessionPhase::Idle,
                pending_question: true,
                ..PredictiveFrame::default()
            },
        ] {
            let decision =
                evaluate_predictive_window(&[frame], &PredictiveParams::default()).unwrap();
            assert_eq!(decision.action, PredictiveAction::Observe);
        }
    }

    #[test]
    fn old_evidence_outside_window_is_ignored() {
        let risky = idle(vec![
            ev(GapFactor::ProxyCapture, SOURCE_CONTRACT, 4.0),
            ev(GapFactor::ProxyCapture, SOURCE_USER, 4.0),
        ]);
        let mut window = vec![risky];
        window.extend([idle(vec![]), idle(vec![]), idle(vec![]), idle(vec![])]);
        let decision = evaluate_predictive_window(&window, &PredictiveParams::default()).unwrap();
        assert_eq!(decision.action, PredictiveAction::Observe);
    }

    #[test]
    fn exhausted_budget_escalates() {
        let mut frame = idle(vec![
            ev(GapFactor::LayerMismatch, SOURCE_CONTRACT, 2.0),
            ev(GapFactor::LayerMismatch, SOURCE_RUNTIME, 3.0),
        ]);
        frame.previous_steers = 3;
        let decision = evaluate_predictive_window(&[frame], &PredictiveParams::default()).unwrap();
        assert_eq!(decision.action, PredictiveAction::Escalate);
    }
}
