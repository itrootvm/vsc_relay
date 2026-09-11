use relay_compass::ledger::{EvidencePolarity, ProofLayer};
use relay_compass::{ContractLedger, SemanticStep, StepRole, ToolKind};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    ContractInput,
    AgentClaim,
    CommandRequest,
    RuntimeResult,
    DelegateClaim,
}

impl ObservationKind {
    fn label(self) -> &'static str {
        match self {
            Self::ContractInput => "contract_input",
            Self::AgentClaim => "agent_claim",
            Self::CommandRequest => "command_request",
            Self::RuntimeResult => "runtime_result",
            Self::DelegateClaim => "delegate_claim",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservationRef {
    step: u32,
    kind: ObservationKind,
    digest: String,
}

impl ObservationRef {
    pub fn step(&self) -> u32 {
        self.step
    }

    pub fn kind(&self) -> ObservationKind {
        self.kind
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvidenceRef {
    ledger_evidence_id: String,
    obligation_id: String,
    observation: ObservationRef,
    layer: ProofLayer,
    relation: EvidenceRelation,
    runtime_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRelation {
    Supports,
    Contradicts,
}

impl EvidenceRef {
    pub fn ledger_evidence_id(&self) -> &str {
        &self.ledger_evidence_id
    }

    pub fn obligation_id(&self) -> &str {
        &self.obligation_id
    }

    pub fn observation(&self) -> &ObservationRef {
        &self.observation
    }

    pub fn layer(&self) -> ProofLayer {
        self.layer
    }

    pub fn relation(&self) -> EvidenceRelation {
        self.relation
    }

    pub fn runtime_error(&self) -> bool {
        self.runtime_error
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedEvent {
    step: u32,
    kind: ObservationKind,
    text: String,
    tool_name: Option<String>,
    correlation_id: Option<String>,
    tool_target: Option<String>,
    runtime_error: bool,
}

impl ObservedEvent {
    pub fn step(&self) -> u32 {
        self.step
    }

    pub fn kind(&self) -> ObservationKind {
        self.kind
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn private_ref(&self, key: &[u8; 32]) -> ObservationRef {
        let mut hasher = blake3::Hasher::new_keyed(key);
        hasher.update(b"vsc-relay-observed-event-v1");
        hasher.update(&self.step.to_le_bytes());
        hasher.update(self.kind.label().as_bytes());
        update_optional(&mut hasher, self.tool_name.as_deref());
        update_optional(&mut hasher, self.correlation_id.as_deref());
        update_optional(&mut hasher, self.tool_target.as_deref());
        hasher.update(&[u8::from(self.runtime_error)]);
        hasher.update(self.text.as_bytes());
        ObservationRef {
            step: self.step,
            kind: self.kind,
            digest: hasher.finalize().to_hex().as_str()[..32].to_string(),
        }
    }
}

fn update_optional(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(&(value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObservationSummary {
    pub schema: u32,
    pub events: usize,
    pub contract_inputs: usize,
    pub agent_claims: usize,
    pub command_requests: usize,
    pub runtime_results: usize,
    pub runtime_errors: usize,
    pub delegate_claims: usize,
    pub evidence_candidates: usize,
    pub last_step: Option<u32>,
    pub chain_digest: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservationLog {
    events: Vec<ObservedEvent>,
}

impl ObservationLog {
    pub fn from_steps(steps: &[SemanticStep]) -> Self {
        let events = steps.iter().map(observed_event).collect();
        Self { events }
    }

    pub fn events(&self) -> &[ObservedEvent] {
        &self.events
    }

    pub fn admitted_evidence_refs(
        &self,
        key: &[u8; 32],
        ledger: &ContractLedger,
    ) -> Vec<EvidenceRef> {
        ledger
            .evidence
            .iter()
            .filter(|admitted| admitted.fresh)
            .filter_map(|admitted| {
                self.events
                    .iter()
                    .find(|event| {
                        event.step == admitted.step && event.kind == ObservationKind::RuntimeResult
                    })
                    .map(|event| EvidenceRef {
                        ledger_evidence_id: admitted.id.clone(),
                        obligation_id: admitted.obligation_id.clone(),
                        observation: event.private_ref(key),
                        layer: admitted.layer,
                        relation: match admitted.polarity {
                            EvidencePolarity::Supports => EvidenceRelation::Supports,
                            EvidencePolarity::Contradicts => EvidenceRelation::Contradicts,
                        },
                        runtime_error: event.runtime_error,
                    })
            })
            .collect()
    }

    pub fn verifying_evidence_refs(
        &self,
        key: &[u8; 32],
        ledger: &ContractLedger,
    ) -> Vec<EvidenceRef> {
        self.admitted_evidence_refs(key, ledger)
            .into_iter()
            .filter(|reference| {
                reference.relation == EvidenceRelation::Supports
                    && reference.layer != ProofLayer::Unknown
                    && ledger
                        .obligations
                        .iter()
                        .find(|obligation| obligation.id == reference.obligation_id)
                        .is_some_and(|obligation| reference.layer.covers(obligation.required_layer))
            })
            .collect()
    }

    pub fn summary(&self, key: &[u8; 32]) -> ObservationSummary {
        let mut contract_inputs = 0;
        let mut agent_claims = 0;
        let mut command_requests = 0;
        let mut runtime_results = 0;
        let mut runtime_errors = 0;
        let mut delegate_claims = 0;
        let mut chain = blake3::Hasher::new_keyed(key);
        chain.update(b"vsc-relay-observation-log-v1");
        for event in &self.events {
            match event.kind {
                ObservationKind::ContractInput => contract_inputs += 1,
                ObservationKind::AgentClaim => agent_claims += 1,
                ObservationKind::CommandRequest => command_requests += 1,
                ObservationKind::RuntimeResult => {
                    runtime_results += 1;
                    runtime_errors += usize::from(event.runtime_error);
                }
                ObservationKind::DelegateClaim => delegate_claims += 1,
            }
            let reference = event.private_ref(key);
            chain.update(&(reference.digest.len() as u64).to_le_bytes());
            chain.update(reference.digest.as_bytes());
        }
        ObservationSummary {
            schema: 1,
            events: self.events.len(),
            contract_inputs,
            agent_claims,
            command_requests,
            runtime_results,
            runtime_errors,
            delegate_claims,
            evidence_candidates: runtime_results,
            last_step: self.events.last().map(ObservedEvent::step),
            chain_digest: chain.finalize().to_hex().as_str()[..32].to_string(),
        }
    }
}

fn observed_event(step: &SemanticStep) -> ObservedEvent {
    let kind = match step.role {
        StepRole::User => ObservationKind::ContractInput,
        StepRole::Assistant => ObservationKind::AgentClaim,
        StepRole::ToolUse => ObservationKind::CommandRequest,
        StepRole::ToolResult if step.tool_kind == ToolKind::Delegate => {
            ObservationKind::DelegateClaim
        }
        StepRole::ToolResult => ObservationKind::RuntimeResult,
        StepRole::DelegateResult => ObservationKind::DelegateClaim,
    };
    ObservedEvent {
        step: step.index,
        kind,
        text: step.text.clone(),
        tool_name: step.tool_name.clone(),
        correlation_id: step.correlation_id.clone(),
        tool_target: step.tool_target.clone(),
        runtime_error: step.is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_compass::ledger::LedgerEvidence;

    fn step(index: u32, role: StepRole, text: &str) -> SemanticStep {
        SemanticStep::new(index, role, text.to_string())
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
    fn preserves_chat_roles_without_promoting_claims_to_evidence() {
        let mut command = step(2, StepRole::ToolUse, "run test");
        command.tool_kind = ToolKind::Execute;
        let mut result = step(3, StepRole::ToolResult, "ok");
        result.tool_kind = ToolKind::Execute;
        let steps = vec![
            step(0, StepRole::User, "fix it"),
            step(1, StepRole::Assistant, "done"),
            command,
            result,
            step(4, StepRole::DelegateResult, "verified by child"),
        ];
        let log = ObservationLog::from_steps(&steps);
        let kinds: Vec<ObservationKind> = log.events().iter().map(ObservedEvent::kind).collect();
        assert_eq!(
            kinds,
            vec![
                ObservationKind::ContractInput,
                ObservationKind::AgentClaim,
                ObservationKind::CommandRequest,
                ObservationKind::RuntimeResult,
                ObservationKind::DelegateClaim,
            ]
        );
        assert_eq!(
            log.admitted_evidence_refs(&[7; 32], &ledger_with_evidence(3))
                .len(),
            1
        );
    }

    #[test]
    fn legacy_delegate_tool_result_stays_a_claim() {
        let mut result = step(0, StepRole::ToolResult, "child says complete");
        result.tool_kind = ToolKind::Delegate;
        let log = ObservationLog::from_steps(&[result]);
        assert_eq!(log.events()[0].kind(), ObservationKind::DelegateClaim);
        assert!(log
            .admitted_evidence_refs(&[3; 32], &ledger_with_evidence(0))
            .is_empty());
    }

    #[test]
    fn runtime_result_requires_ledger_admission() {
        let mut result = step(0, StepRole::ToolResult, "permission denied");
        result.tool_kind = ToolKind::Execute;
        result.is_error = true;
        let log = ObservationLog::from_steps(&[result]);
        assert!(log
            .admitted_evidence_refs(&[4; 32], &ContractLedger::default())
            .is_empty());
        let evidence = log.admitted_evidence_refs(&[4; 32], &ledger_with_evidence(0));
        assert_eq!(evidence.len(), 1);
        assert!(evidence[0].runtime_error());
        assert_eq!(evidence[0].ledger_evidence_id(), "ev-1");
    }

    #[test]
    fn stale_ledger_evidence_is_not_admitted() {
        let mut result = step(0, StepRole::ToolResult, "old result");
        result.tool_kind = ToolKind::Execute;
        let log = ObservationLog::from_steps(&[result]);
        let mut ledger = ledger_with_evidence(0);
        ledger.evidence[0].fresh = false;
        assert!(log.admitted_evidence_refs(&[5; 32], &ledger).is_empty());
    }

    #[test]
    fn verifying_refs_reject_read_only_and_accept_execution() {
        use relay_compass::build_contract_ledger;
        let goal = "Implement and verify parser.rs end to end";

        let mut exec_tool = step(1, StepRole::ToolUse, "test parser.rs");
        exec_tool.tool_kind = ToolKind::Execute;
        exec_tool.tool_target = Some("test parser.rs".to_string());
        exec_tool.correlation_id = Some("call-7".to_string());
        let mut exec_result = step(2, StepRole::ToolResult, "ok");
        exec_result.tool_kind = ToolKind::Execute;
        exec_result.correlation_id = Some("call-7".to_string());
        let steps = vec![step(0, StepRole::User, goal), exec_tool, exec_result];
        let ledger = build_contract_ledger(&steps, &[], goal);
        let log = ObservationLog::from_steps(&steps);
        assert!(!log.verifying_evidence_refs(&[9; 32], &ledger).is_empty());

        let mut probe_tool = step(1, StepRole::ToolUse, "read parser.rs");
        probe_tool.tool_kind = ToolKind::Search;
        probe_tool.tool_target = Some("read parser.rs".to_string());
        probe_tool.correlation_id = Some("probe-7".to_string());
        let mut probe_result = step(2, StepRole::ToolResult, "contents");
        probe_result.tool_kind = ToolKind::Search;
        probe_result.correlation_id = Some("probe-7".to_string());
        let steps = vec![step(0, StepRole::User, goal), probe_tool, probe_result];
        let ledger = build_contract_ledger(&steps, &[], goal);
        let log = ObservationLog::from_steps(&steps);
        assert!(log.verifying_evidence_refs(&[9; 32], &ledger).is_empty());
    }

    #[test]
    fn summary_is_private_stable_and_order_sensitive() {
        let key = [11; 32];
        let left = ObservationLog::from_steps(&[
            step(0, StepRole::User, "private requirement"),
            step(1, StepRole::Assistant, "private claim"),
        ]);
        let same = ObservationLog::from_steps(&[
            step(0, StepRole::User, "private requirement"),
            step(1, StepRole::Assistant, "private claim"),
        ]);
        let reversed = ObservationLog::from_steps(&[
            step(0, StepRole::Assistant, "private claim"),
            step(1, StepRole::User, "private requirement"),
        ]);
        let summary = left.summary(&key);
        assert_eq!(summary, same.summary(&key));
        assert_ne!(summary.chain_digest, reversed.summary(&key).chain_digest);
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("private requirement"));
        assert!(!json.contains("private claim"));
    }
}
