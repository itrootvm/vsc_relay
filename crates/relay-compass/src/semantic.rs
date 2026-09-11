use serde::{Deserialize, Serialize};

use crate::ledger::ProofLayer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContractSource {
    #[default]
    Goal,
    UserContract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContractAtomKind {
    Deliverable,
    Constraint,
    Acceptance,
    Evidence,
    #[default]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ContractAtomHint {
    #[serde(default)]
    pub source: ContractSource,
    #[serde(default)]
    pub quote: String,
    #[serde(default)]
    pub kind: ContractAtomKind,
    #[serde(default)]
    pub required_evidence_layer: ProofLayer,
    #[serde(default, alias = "confidence")]
    pub probability: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ObligationLinkHint {
    #[serde(default)]
    pub tool_call_id: String,
    #[serde(default)]
    pub obligation_quote: String,
    #[serde(default)]
    pub observed_evidence_layer: ProofLayer,
    #[serde(default, alias = "confidence")]
    pub probability: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Truth {
    Yes,
    No,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClassScore {
    #[serde(default)]
    pub state: Truth,
    #[serde(default, alias = "confidence")]
    pub probability: f32,
}

impl Default for ClassScore {
    fn default() -> Self {
        Self {
            state: Truth::Unknown,
            probability: 0.0,
        }
    }
}

impl ClassScore {
    pub fn yes(probability: f32) -> Self {
        Self {
            state: Truth::Yes,
            probability: probability.clamp(0.0, 1.0),
        }
    }

    pub fn no(probability: f32) -> Self {
        Self {
            state: Truth::No,
            probability: probability.clamp(0.0, 1.0),
        }
    }

    pub fn is_yes(self) -> bool {
        self.state == Truth::Yes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    Supports,
    Contradicts,
    Partial,
    Unrelated,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RelationScore {
    #[serde(default)]
    pub relation: Relation,
    #[serde(default, alias = "confidence")]
    pub probability: f32,
}

impl Default for RelationScore {
    fn default() -> Self {
        Self {
            relation: Relation::Unknown,
            probability: 0.0,
        }
    }
}

impl RelationScore {
    pub fn new(relation: Relation, probability: f32) -> Self {
        Self {
            relation,
            probability: probability.clamp(0.0, 1.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SemanticFacts {
    pub episode: u32,
    #[serde(default)]
    pub completion_claim: ClassScore,
    #[serde(default)]
    pub blocker_claim: ClassScore,
    #[serde(default)]
    pub verification_claim: ClassScore,
    #[serde(default)]
    pub correction: ClassScore,
    #[serde(default)]
    pub pivot: ClassScore,
    #[serde(default)]
    pub continuation_intent: ClassScore,
    #[serde(default)]
    pub stale_evidence_claim: ClassScore,
    #[serde(default)]
    pub proxy_focus: ClassScore,
    #[serde(default)]
    pub goal_relation: RelationScore,
    #[serde(default)]
    pub evidence_relation: RelationScore,

    #[serde(default)]
    pub required_evidence_layer: ProofLayer,

    #[serde(default)]
    pub observed_evidence_layer: ProofLayer,

    #[serde(default)]
    pub contract_atoms: Vec<ContractAtomHint>,

    #[serde(default)]
    pub obligation_links: Vec<ObligationLinkHint>,
    #[serde(default)]
    pub calibrated: bool,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticInputFrame {
    pub episode: u32,
    pub goal: String,
    pub user_contract: String,
    pub assistant: String,
    pub tools: Vec<String>,
    pub runtime: Vec<String>,
    pub runtime_error: bool,
}
