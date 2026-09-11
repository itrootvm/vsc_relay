use crate::noisy_or;

pub const HEALTH_WINDOW: usize = 4;
pub const SOURCE_CONTRACT: u8 = 1 << 0;
pub const SOURCE_AGENT: u8 = 1 << 1;
pub const SOURCE_USER: u8 = 1 << 2;
pub const SOURCE_TOOL: u8 = 1 << 3;
pub const SOURCE_RUNTIME: u8 = 1 << 4;

pub const SOURCE_DELEGATE: u8 = 1 << 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    Working,
    AwaitingUser,
    Idle,
    Error,
}

impl SessionPhase {
    fn actionable(self) -> bool {
        matches!(self, SessionPhase::Idle | SessionPhase::Error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HealthSignals {
    pub unresolved_contract: f32,
    pub completion_claim: f32,
    pub proof_deficit: f32,
    pub scope_loss: f32,
    pub observed_failure: f32,
    pub post_claim_recurrence: f32,
    pub unsupported_blocker: f32,
    pub source_mask: u8,
    pub phase: SessionPhase,
    pub pivot_confirmed: bool,
    pub pending_question: bool,
    pub same_signature_recent: bool,
    pub previous_steers: u8,
}

impl Default for HealthSignals {
    fn default() -> Self {
        HealthSignals {
            unresolved_contract: 0.0,
            completion_claim: 0.0,
            proof_deficit: 0.0,
            scope_loss: 0.0,
            observed_failure: 0.0,
            post_claim_recurrence: 0.0,
            unsupported_blocker: 0.0,
            source_mask: 0,
            phase: SessionPhase::Working,
            pivot_confirmed: false,
            pending_question: false,
            same_signature_recent: false,
            previous_steers: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GapChannels {
    pub contract_mismatch: f32,
    pub evidence_deficit: f32,
    pub scope_loss: f32,
    pub observed_failure: f32,
    pub recurrence: f32,
    pub unsupported_blocker: f32,
}

impl Default for GapChannels {
    fn default() -> Self {
        GapChannels {
            contract_mismatch: 0.0,
            evidence_deficit: 0.0,
            scope_loss: 0.0,
            observed_failure: 0.0,
            recurrence: 0.0,
            unsupported_blocker: 0.0,
        }
    }
}

impl GapChannels {
    fn probabilities(self) -> [f32; 6] {
        [
            self.contract_mismatch,
            self.evidence_deficit,
            self.scope_loss,
            self.observed_failure,
            self.recurrence,
            self.unsupported_blocker,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthAction {
    Observe,
    Steer,
    Escalate,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HealthDecision {
    pub action: HealthAction,
    pub risk: f32,
    pub active_families: u8,
    pub corroborating_sources: u8,
    pub channels: GapChannels,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HealthGateParams {
    pub action_threshold: f32,
    pub family_threshold: f32,
    pub min_families: u8,
    pub min_sources: u8,
    pub max_steers: u8,
    pub recency_decay: f32,
}

impl Default for HealthGateParams {
    fn default() -> Self {
        HealthGateParams {
            action_threshold: 0.82,
            family_threshold: 0.50,
            min_families: 2,
            min_sources: 2,
            max_steers: 3,
            recency_decay: 0.84,
        }
    }
}

fn unit(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

fn merge_max(target: &mut f32, value: f32) {
    *target = target.max(unit(value));
}

pub fn evaluate_health_window(
    window: &[HealthSignals],
    params: &HealthGateParams,
) -> HealthDecision {
    let Some(current) = window.last().copied() else {
        return HealthDecision {
            action: HealthAction::Observe,
            risk: 0.0,
            active_families: 0,
            corroborating_sources: 0,
            channels: GapChannels::default(),
        };
    };
    let mut channels = GapChannels::default();
    let mut source_mask = 0u8;
    let mut decay = 1.0;
    for signals in window.iter().rev().take(HEALTH_WINDOW) {
        if signals.pivot_confirmed {
            break;
        }
        source_mask |= signals.source_mask;
        let unresolved = unit(signals.unresolved_contract);
        let claim = unit(signals.completion_claim);
        merge_max(&mut channels.contract_mismatch, decay * claim * unresolved);
        merge_max(
            &mut channels.evidence_deficit,
            decay * claim * unit(signals.proof_deficit),
        );
        merge_max(
            &mut channels.scope_loss,
            decay * unresolved * unit(signals.scope_loss),
        );
        merge_max(
            &mut channels.observed_failure,
            decay * unit(signals.observed_failure),
        );
        merge_max(
            &mut channels.recurrence,
            decay * unit(signals.post_claim_recurrence),
        );
        merge_max(
            &mut channels.unsupported_blocker,
            decay * unit(signals.unsupported_blocker),
        );
        decay *= params.recency_decay.clamp(0.0, 1.0);
    }
    let probabilities = channels.probabilities();
    let weights = [0.85, 0.75, 0.65, 0.90, 0.75, 0.70];
    let weighted = probabilities
        .iter()
        .zip(weights)
        .map(|(probability, weight)| (*probability, weight))
        .collect::<Vec<_>>();
    let risk = noisy_or(&weighted);
    let active_families = probabilities
        .iter()
        .filter(|probability| **probability >= params.family_threshold)
        .count() as u8;
    let corroborating_sources = source_mask.count_ones() as u8;
    let warranted = risk >= params.action_threshold
        && active_families >= params.min_families
        && corroborating_sources >= params.min_sources
        && current.phase.actionable()
        && !current.pending_question
        && !current.same_signature_recent
        && !current.pivot_confirmed;
    let action = if warranted && current.previous_steers >= params.max_steers {
        HealthAction::Escalate
    } else if warranted {
        HealthAction::Steer
    } else {
        HealthAction::Observe
    };
    HealthDecision {
        action,
        risk,
        active_families,
        corroborating_sources,
        channels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> HealthSignals {
        HealthSignals {
            phase: SessionPhase::Idle,
            ..HealthSignals::default()
        }
    }

    #[test]
    fn premature_completion_with_missing_proof_steers() {
        let signals = HealthSignals {
            unresolved_contract: 0.95,
            completion_claim: 0.95,
            proof_deficit: 0.85,
            scope_loss: 0.80,
            source_mask: SOURCE_CONTRACT | SOURCE_AGENT,
            ..idle()
        };
        let decision = evaluate_health_window(&[signals], &HealthGateParams::default());
        assert_eq!(decision.action, HealthAction::Steer);
        assert!(decision.active_families >= 2);
    }

    #[test]
    fn correction_inside_four_step_window_steers() {
        let history = [
            HealthSignals {
                unresolved_contract: 0.90,
                completion_claim: 0.90,
                proof_deficit: 0.80,
                source_mask: SOURCE_CONTRACT | SOURCE_AGENT,
                ..idle()
            },
            HealthSignals::default(),
            HealthSignals {
                observed_failure: 0.95,
                post_claim_recurrence: 0.90,
                source_mask: SOURCE_USER,
                ..idle()
            },
        ];
        let decision = evaluate_health_window(&history, &HealthGateParams::default());
        assert_eq!(decision.action, HealthAction::Steer);
    }

    #[test]
    fn honest_work_in_progress_does_not_steer() {
        let signals = HealthSignals {
            unresolved_contract: 0.95,
            proof_deficit: 0.90,
            scope_loss: 0.85,
            source_mask: SOURCE_CONTRACT,
            phase: SessionPhase::Working,
            ..HealthSignals::default()
        };
        let decision = evaluate_health_window(&[signals], &HealthGateParams::default());
        assert_eq!(decision.action, HealthAction::Observe);
    }

    #[test]
    fn one_failure_family_does_not_steer() {
        let signals = HealthSignals {
            observed_failure: 1.0,
            source_mask: SOURCE_RUNTIME,
            ..idle()
        };
        let decision = evaluate_health_window(&[signals], &HealthGateParams::default());
        assert_eq!(decision.action, HealthAction::Observe);
        assert_eq!(decision.active_families, 1);
    }

    #[test]
    fn two_channels_from_one_source_do_not_steer() {
        let signals = HealthSignals {
            unresolved_contract: 1.0,
            completion_claim: 1.0,
            proof_deficit: 1.0,
            source_mask: SOURCE_AGENT,
            ..idle()
        };
        let decision = evaluate_health_window(&[signals], &HealthGateParams::default());
        assert!(decision.active_families >= 2);
        assert_eq!(decision.corroborating_sources, 1);
        assert_eq!(decision.action, HealthAction::Observe);
    }

    #[test]
    fn confirmed_pivot_resets_older_gap_evidence() {
        let history = [
            HealthSignals {
                unresolved_contract: 1.0,
                completion_claim: 1.0,
                proof_deficit: 1.0,
                source_mask: SOURCE_CONTRACT | SOURCE_AGENT,
                ..idle()
            },
            HealthSignals {
                pivot_confirmed: true,
                ..idle()
            },
        ];
        let decision = evaluate_health_window(&history, &HealthGateParams::default());
        assert_eq!(decision.action, HealthAction::Observe);
        assert_eq!(decision.risk, 0.0);
    }

    #[test]
    fn pending_question_and_cooldown_suppress_duplicate_steer() {
        for signals in [
            HealthSignals {
                unresolved_contract: 1.0,
                completion_claim: 1.0,
                proof_deficit: 1.0,
                pending_question: true,
                source_mask: SOURCE_CONTRACT | SOURCE_AGENT,
                ..idle()
            },
            HealthSignals {
                unresolved_contract: 1.0,
                completion_claim: 1.0,
                proof_deficit: 1.0,
                same_signature_recent: true,
                source_mask: SOURCE_CONTRACT | SOURCE_AGENT,
                ..idle()
            },
        ] {
            let decision = evaluate_health_window(&[signals], &HealthGateParams::default());
            assert_eq!(decision.action, HealthAction::Observe);
        }
    }

    #[test]
    fn fourth_prior_step_is_outside_window() {
        let risky = HealthSignals {
            unresolved_contract: 1.0,
            completion_claim: 1.0,
            proof_deficit: 1.0,
            source_mask: SOURCE_CONTRACT | SOURCE_AGENT,
            ..idle()
        };
        let mut history = vec![risky];
        history.extend([idle(), idle(), idle(), idle()]);
        let decision = evaluate_health_window(&history, &HealthGateParams::default());
        assert_eq!(decision.action, HealthAction::Observe);
        assert_eq!(decision.risk, 0.0);
    }

    #[test]
    fn repeated_warranted_gap_escalates_after_three_steers() {
        let signals = HealthSignals {
            unresolved_contract: 1.0,
            completion_claim: 1.0,
            proof_deficit: 1.0,
            previous_steers: 3,
            source_mask: SOURCE_CONTRACT | SOURCE_AGENT,
            ..idle()
        };
        let decision = evaluate_health_window(&[signals], &HealthGateParams::default());
        assert_eq!(decision.action, HealthAction::Escalate);
    }
}
