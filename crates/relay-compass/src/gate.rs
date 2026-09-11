use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    Create,
    Replace,
    Migrate,
    Modify,
    Delete,
}

impl MutationKind {
    pub fn is_novelty(self) -> bool {
        matches!(self, Self::Create | Self::Replace | Self::Migrate)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    ReadOnly,
    Mutation(MutationKind),
    ExternalSideEffect,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExistenceState {
    Present,
    Absent,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PopulationState {
    NonEmpty,
    Empty,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WiringState {
    Connected,
    Disconnected,
    Partial,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BehaviorState {
    Working,
    Broken,
    Partial,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ArtifactState {
    #[serde(default)]
    pub existence: ExistenceState,
    #[serde(default)]
    pub population: PopulationState,
    #[serde(default)]
    pub wiring: WiringState,
    #[serde(default)]
    pub behavior: BehaviorState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    PresentFresh,
    AbsentFresh,
    Partial,
    Stale,
    #[default]
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateState {
    Advisory,
    AwaitingProof,
    TargetProbe,
    ControllerProbe,
    Replanning,
    ClearedAbsent,
    RejectedPresent,
    Quarantined,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Annotate,
    AskProof,
    Deny,
    AutonomousResolve,
    QuarantineBranch,
    DeferToBase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateProof {
    obligation_ids: Vec<String>,
    mutation: MutationKind,
    coverage: CoverageStatus,
}

impl GateProof {
    pub(crate) fn new(
        obligation_ids: Vec<String>,
        mutation: MutationKind,
        coverage: CoverageStatus,
    ) -> Self {
        Self {
            obligation_ids,
            mutation,
            coverage,
        }
    }

    pub fn obligation_ids(&self) -> &[String] {
        &self.obligation_ids
    }

    pub fn mutation(&self) -> MutationKind {
        self.mutation
    }

    pub fn coverage(&self) -> CoverageStatus {
        self.coverage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_create_replace_migrate_are_novelty_mutations() {
        assert!(MutationKind::Create.is_novelty());
        assert!(MutationKind::Replace.is_novelty());
        assert!(MutationKind::Migrate.is_novelty());
        assert!(!MutationKind::Modify.is_novelty());
        assert!(!MutationKind::Delete.is_novelty());
    }

    #[test]
    fn artifact_dimensions_do_not_imply_each_other() {
        let state = ArtifactState {
            existence: ExistenceState::Present,
            population: PopulationState::Empty,
            ..ArtifactState::default()
        };
        assert_eq!(state.wiring, WiringState::Unknown);
        assert_eq!(state.behavior, BehaviorState::Unknown);
    }
}
