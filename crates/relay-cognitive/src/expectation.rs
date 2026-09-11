use crate::{EvidenceRef, EvidenceRelation};
use relay_compass::ledger::ProofLayer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedCondition {
    pub id: String,
    pub action_id: String,
    pub created_at_step: u32,
    pub expires_after_step: u32,
    pub obligation_id: String,
    pub required_layer: ProofLayer,
    pub statement: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    Unresolved,
    Satisfied,
    Violated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationRecord {
    expectation_id: String,
    status: VerificationStatus,
    evidence: Vec<EvidenceRef>,
}

impl VerificationRecord {
    pub fn unresolved(expectation: &ExpectedCondition) -> Self {
        Self {
            expectation_id: expectation.id.clone(),
            status: VerificationStatus::Unresolved,
            evidence: Vec::new(),
        }
    }

    pub fn from_evidence(
        expectation: &ExpectedCondition,
        status: VerificationStatus,
        evidence: Vec<EvidenceRef>,
    ) -> Option<Self> {
        if status == VerificationStatus::Unresolved
            || expectation.expires_after_step <= expectation.created_at_step
        {
            return None;
        }
        let evidence: Vec<EvidenceRef> = evidence
            .into_iter()
            .filter(|item| {
                let step = item.observation().step();
                let relation_matches = match status {
                    VerificationStatus::Satisfied => item.relation() == EvidenceRelation::Supports,
                    VerificationStatus::Violated => {
                        item.relation() == EvidenceRelation::Contradicts
                    }
                    VerificationStatus::Unresolved => false,
                };
                step > expectation.created_at_step
                    && step <= expectation.expires_after_step
                    && item.obligation_id() == expectation.obligation_id
                    && item.layer().covers(expectation.required_layer)
                    && relation_matches
            })
            .collect();
        if evidence.is_empty() {
            return None;
        }
        Some(Self {
            expectation_id: expectation.id.clone(),
            status,
            evidence,
        })
    }

    pub fn expectation_id(&self) -> &str {
        &self.expectation_id
    }

    pub fn status(&self) -> VerificationStatus {
        self.status
    }

    pub fn evidence(&self) -> &[EvidenceRef] {
        &self.evidence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObservationLog;
    use relay_compass::ledger::{EvidencePolarity, LedgerEvidence, ProofLayer};
    use relay_compass::{ContractLedger, SemanticStep, StepRole, ToolKind};

    fn expectation() -> ExpectedCondition {
        ExpectedCondition {
            id: "expected-1".to_string(),
            action_id: "action-1".to_string(),
            created_at_step: 2,
            expires_after_step: 5,
            obligation_id: "obl-1".to_string(),
            required_layer: ProofLayer::Unit,
            statement: "the declared check returns a typed result".to_string(),
        }
    }

    fn ledger_with_evidence(step: u32) -> ContractLedger {
        let mut ledger = ContractLedger::default();
        ledger.evidence.push(LedgerEvidence {
            id: "ev-1".to_string(),
            obligation_id: "obl-1".to_string(),
            step,
            source_kind: ToolKind::Execute,
            layer: ProofLayer::Unit,
            polarity: EvidencePolarity::Supports,
            strength: 1.0,
            fresh: true,
            inherited_from: None,
        });
        ledger
    }

    #[test]
    fn expectation_starts_unresolved_without_evidence() {
        let record = VerificationRecord::unresolved(&expectation());
        assert_eq!(record.status(), VerificationStatus::Unresolved);
        assert!(record.evidence().is_empty());
    }

    #[test]
    fn expectation_cannot_be_satisfied_without_observed_runtime_evidence() {
        assert!(VerificationRecord::from_evidence(
            &expectation(),
            VerificationStatus::Satisfied,
            Vec::new(),
        )
        .is_none());

        let assistant = SemanticStep::new(3, StepRole::Assistant, "it worked".to_string());
        let log = ObservationLog::from_steps(&[assistant]);
        assert!(log
            .admitted_evidence_refs(&[8; 32], &ledger_with_evidence(3))
            .is_empty());
    }

    #[test]
    fn observed_runtime_result_can_support_local_verification() {
        let mut result = SemanticStep::new(3, StepRole::ToolResult, "ok".to_string());
        result.tool_kind = ToolKind::Execute;
        let log = ObservationLog::from_steps(&[result]);
        let record = VerificationRecord::from_evidence(
            &expectation(),
            VerificationStatus::Satisfied,
            log.admitted_evidence_refs(&[9; 32], &ledger_with_evidence(3)),
        )
        .unwrap();
        assert_eq!(record.status(), VerificationStatus::Satisfied);
        assert_eq!(record.evidence().len(), 1);
    }

    #[test]
    fn old_or_expired_evidence_cannot_resolve_a_new_expectation() {
        for step in [2, 6] {
            let mut result = SemanticStep::new(step, StepRole::ToolResult, "ok".to_string());
            result.tool_kind = ToolKind::Execute;
            let log = ObservationLog::from_steps(&[result]);
            assert!(VerificationRecord::from_evidence(
                &expectation(),
                VerificationStatus::Satisfied,
                log.admitted_evidence_refs(&[9; 32], &ledger_with_evidence(step)),
            )
            .is_none());
        }
    }

    #[test]
    fn unrelated_or_weaker_evidence_cannot_resolve_an_expectation() {
        let mut result = SemanticStep::new(3, StepRole::ToolResult, "ok".to_string());
        result.tool_kind = ToolKind::Execute;
        let log = ObservationLog::from_steps(&[result]);
        let mut unrelated = ledger_with_evidence(3);
        unrelated.evidence[0].obligation_id = "obl-other".to_string();
        assert!(VerificationRecord::from_evidence(
            &expectation(),
            VerificationStatus::Satisfied,
            log.admitted_evidence_refs(&[9; 32], &unrelated),
        )
        .is_none());

        let mut weak = ledger_with_evidence(3);
        weak.evidence[0].layer = ProofLayer::Inspection;
        assert!(VerificationRecord::from_evidence(
            &expectation(),
            VerificationStatus::Satisfied,
            log.admitted_evidence_refs(&[9; 32], &weak),
        )
        .is_none());
    }

    #[test]
    fn evidence_polarity_must_match_verification_status() {
        let mut result = SemanticStep::new(3, StepRole::ToolResult, "failed".to_string());
        result.tool_kind = ToolKind::Execute;
        let log = ObservationLog::from_steps(&[result]);
        let mut ledger = ledger_with_evidence(3);
        ledger.evidence[0].polarity = EvidencePolarity::Contradicts;
        let evidence = log.admitted_evidence_refs(&[9; 32], &ledger);
        assert!(VerificationRecord::from_evidence(
            &expectation(),
            VerificationStatus::Satisfied,
            evidence.clone(),
        )
        .is_none());
        assert!(VerificationRecord::from_evidence(
            &expectation(),
            VerificationStatus::Violated,
            evidence,
        )
        .is_some());
    }
}
