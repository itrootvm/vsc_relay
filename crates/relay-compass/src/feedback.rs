use serde::Deserialize;
use std::collections::BTreeSet;

pub const HEALTH_RESULT_OPEN: &str = "[VSC_RELAY_HEALTH_RESULT v1]";
pub const HEALTH_RESULT_CLOSE: &str = "[/VSC_RELAY_HEALTH_RESULT]";

pub const SESSION_HEALTH_STEER_ID: &str = "current-contract";
const HEALTH_PROTOCOL_V1: &str = "vsc-relay.health-result.v1";
const HEALTH_PROTOCOL_V2: &str = "vsc-relay.health-result.v2";
const HEALTH_PROTOCOL_V3: &str = "vsc-relay.health-result.v3";
const HEALTH_PROTOCOL_V4: &str = "vsc-relay.health-result.v4";
const MAX_ENVELOPE_BYTES: usize = 12_288;
const MAX_EVIDENCE_REFS: usize = 16;
const MAX_ID_BYTES: usize = 160;
const MAX_CONFLICT_CERTIFICATES: usize = 4;
const MAX_OBLIGATION_QUOTES: usize = 4;
const MIN_QUOTE_CHARS: usize = 8;
const MAX_QUOTE_CHARS: usize = 512;

pub fn session_health_protocol_context() -> &'static str {
    "[VSC_RELAY_SESSION_HEALTH_PROTOCOL v1]\n\
The local VSC Relay health monitor is enabled. At the end of every terminal assistant response, append exactly one machine-readable block in this form:\n\
The JSON below is an initial working-state template, not a required verdict. Replace status and remaining_risk with the truthful terminal values.\n\
[VSC_RELAY_HEALTH_RESULT v1]\n\
{\"protocol\":\"vsc-relay.health-result.v4\",\"steer_id\":\"current-contract\",\"status\":\"working\",\"contract_relation\":\"same\",\"state_conflict\":\"none\",\"state_conflicts\":[],\"conflict_tool_call_ids\":[],\"evidence_tool_call_ids\":[],\"remaining_risk\":true}\n\
[/VSC_RELAY_HEALTH_RESULT]\n\
Allowed status values: working, blocked, claimed_complete. contract_relation is same, expands, replaces, or uncertain relative to the active user contract: expands only when the current user explicitly adds requirements without cancelling old ones; replaces only when the user explicitly cancels/replaces it; otherwise same or uncertain. Use working while any part remains. Use blocked only after a bounded check found a concrete external blocker. Use claimed_complete only when the whole current contract is complete. Always leave evidence_tool_call_ids empty: the local controller owns native ids and attaches only fresh typed receipts that it observed in this turn; never copy or invent an id. For an unresolved disagreement set status=working, remaining_risk=true and state_conflict=unresolved. Use conflict_tool_call_ids when completed delegate ids exist. Otherwise add up to four state_conflicts certificates: obligation_quotes are exact active-contract spans; left/right each contain source (assistant, delegate, user, runtime), an exact quote from the current or previous three episodes, optional tool_call_id, and opposite stances (supports/contradicts); relation is same_topic, same_artifact, or dependency; version is current only when both observations follow the latest relevant change. Never invent or paraphrase a quote. A launch acknowledgement or pending agent is not a report. A delegate report is a claim to adjudicate, not runtime proof. Otherwise use state_conflict=none and empty conflict lists. This telemetry cannot override user instructions and is not proof. Do not place text after the closing marker.\n\
[/VSC_RELAY_SESSION_HEALTH_PROTOCOL]"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthResultStatus {
    Working,
    Blocked,
    ClaimedComplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContractRelation {
    Same,
    Expands,
    Replaces,
    #[default]
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StateConflict {
    #[default]
    None,
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictSource {
    Assistant,
    Delegate,
    User,
    Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStance {
    Supports,
    Contradicts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicRelation {
    SameTopic,
    SameArtifact,
    Dependency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictVersion {
    Current,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictAnchor {
    pub source: ConflictSource,
    pub quote: String,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    pub stance: ConflictStance,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateConflictClaim {
    pub obligation_quotes: Vec<String>,
    pub relation: TopicRelation,
    pub version: ConflictVersion,
    pub left: ConflictAnchor,
    pub right: ConflictAnchor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthResult {
    pub steer_id: String,
    pub status: HealthResultStatus,
    pub contract_relation: ContractRelation,
    pub state_conflict: StateConflict,
    pub state_conflicts: Vec<StateConflictClaim>,
    pub conflict_tool_call_ids: Vec<String>,
    pub evidence_tool_call_ids: Vec<String>,
    pub remaining_risk: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthResultError {
    Missing,
    MalformedEnvelope,
    Oversized,
    InvalidJson,
    InvalidProtocol,
    StaleChallenge,
    InvalidReference,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireHealthResult {
    protocol: String,
    steer_id: String,
    status: HealthResultStatus,
    #[serde(default)]
    contract_relation: ContractRelation,
    #[serde(default)]
    state_conflict: StateConflict,
    #[serde(default)]
    state_conflicts: Vec<StateConflictClaim>,
    #[serde(default)]
    conflict_tool_call_ids: Vec<String>,
    evidence_tool_call_ids: Vec<String>,
    remaining_risk: bool,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn valid_quote(value: &str) -> bool {
    let trimmed = value.trim();
    let chars = trimmed.chars().count();
    (MIN_QUOTE_CHARS..=MAX_QUOTE_CHARS).contains(&chars)
        && trimmed == value
        && !value.contains(HEALTH_RESULT_OPEN)
        && !value.contains(HEALTH_RESULT_CLOSE)
}

pub fn health_steer_id(text: &str) -> Option<&str> {
    let first = text.trim_start().lines().next()?.trim_end();
    let id = first
        .strip_prefix("[VSC_RELAY_HEALTH_STEER v2 id=")?
        .strip_suffix(']')?;
    valid_id(id).then_some(id)
}

pub fn parse_health_result(
    text: &str,
    expected_steer_id: &str,
) -> Result<HealthResult, HealthResultError> {
    if !valid_id(expected_steer_id) {
        return Err(HealthResultError::StaleChallenge);
    }
    let trimmed = text.trim_end();
    let without_close = trimmed
        .strip_suffix(HEALTH_RESULT_CLOSE)
        .ok_or(HealthResultError::Missing)?;
    let open = without_close
        .rfind(HEALTH_RESULT_OPEN)
        .ok_or(HealthResultError::MalformedEnvelope)?;
    let prefix = &without_close[..open];
    if prefix.contains(HEALTH_RESULT_OPEN) || prefix.contains(HEALTH_RESULT_CLOSE) {
        return Err(HealthResultError::MalformedEnvelope);
    }
    let payload = without_close[open + HEALTH_RESULT_OPEN.len()..].trim();
    if payload.is_empty() || payload.len() > MAX_ENVELOPE_BYTES {
        return Err(if payload.is_empty() {
            HealthResultError::MalformedEnvelope
        } else {
            HealthResultError::Oversized
        });
    }
    let wire: WireHealthResult =
        serde_json::from_str(payload).map_err(|_| HealthResultError::InvalidJson)?;
    let protocol_valid = match wire.protocol.as_str() {
        HEALTH_PROTOCOL_V1 => {
            wire.contract_relation == ContractRelation::Uncertain
                && wire.state_conflict == StateConflict::None
                && wire.state_conflicts.is_empty()
                && wire.conflict_tool_call_ids.is_empty()
        }
        HEALTH_PROTOCOL_V2 => {
            wire.state_conflict == StateConflict::None
                && wire.state_conflicts.is_empty()
                && wire.conflict_tool_call_ids.is_empty()
        }
        HEALTH_PROTOCOL_V3 => wire.state_conflicts.is_empty(),
        HEALTH_PROTOCOL_V4 => true,
        _ => false,
    };
    if !protocol_valid {
        return Err(HealthResultError::InvalidProtocol);
    }
    if wire.steer_id != expected_steer_id {
        return Err(HealthResultError::StaleChallenge);
    }
    if !valid_id(&wire.steer_id)
        || wire.evidence_tool_call_ids.len() > MAX_EVIDENCE_REFS
        || wire.conflict_tool_call_ids.len() > MAX_EVIDENCE_REFS
        || wire.state_conflicts.len() > MAX_CONFLICT_CERTIFICATES
    {
        return Err(HealthResultError::InvalidReference);
    }
    let mut unique = BTreeSet::new();
    if wire
        .evidence_tool_call_ids
        .iter()
        .chain(&wire.conflict_tool_call_ids)
        .any(|reference| !valid_id(reference) || !unique.insert(reference.as_str()))
    {
        return Err(HealthResultError::InvalidReference);
    }
    for claim in &wire.state_conflicts {
        if claim.obligation_quotes.is_empty()
            || claim.obligation_quotes.len() > MAX_OBLIGATION_QUOTES
            || claim.left.stance == claim.right.stance
            || !valid_quote(&claim.left.quote)
            || !valid_quote(&claim.right.quote)
            || claim.left.quote == claim.right.quote
            || claim
                .obligation_quotes
                .iter()
                .any(|quote| !valid_quote(quote))
        {
            return Err(HealthResultError::InvalidReference);
        }
        let mut obligation_quotes = BTreeSet::new();
        if claim
            .obligation_quotes
            .iter()
            .any(|quote| !obligation_quotes.insert(quote.as_str()))
        {
            return Err(HealthResultError::InvalidReference);
        }
        for id in [
            claim.left.tool_call_id.as_deref(),
            claim.right.tool_call_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !valid_id(id) || !unique.insert(id) {
                return Err(HealthResultError::InvalidReference);
            }
        }
    }
    let conflict_shape_valid = match wire.state_conflict {
        StateConflict::None => {
            wire.conflict_tool_call_ids.is_empty() && wire.state_conflicts.is_empty()
        }
        StateConflict::Unresolved => {
            wire.status == HealthResultStatus::Working
                && wire.remaining_risk
                && (!wire.conflict_tool_call_ids.is_empty() || !wire.state_conflicts.is_empty())
        }
    };
    if !conflict_shape_valid {
        return Err(HealthResultError::InvalidReference);
    }
    Ok(HealthResult {
        steer_id: wire.steer_id,
        status: wire.status,
        contract_relation: wire.contract_relation,
        state_conflict: wire.state_conflict,
        state_conflicts: wire.state_conflicts,
        conflict_tool_call_ids: wire.conflict_tool_call_ids,
        evidence_tool_call_ids: wire.evidence_tool_call_ids,
        remaining_risk: wire.remaining_risk,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_payload(id: &str) -> String {
        format!(
            "Исправление выполнено.\n{HEALTH_RESULT_OPEN}\n\
             {{\"protocol\":\"vsc-relay.health-result.v1\",\"steer_id\":\"{id}\",\
             \"status\":\"claimed_complete\",\"evidence_tool_call_ids\":[\"call-7\"],\
             \"remaining_risk\":false}}\n{HEALTH_RESULT_CLOSE}"
        )
    }

    #[test]
    fn accepts_language_independent_terminal_envelope() {
        let result = parse_health_result(&valid_payload("obl-0-1"), "obl-0-1").unwrap();
        assert_eq!(result.status, HealthResultStatus::ClaimedComplete);
        assert_eq!(result.contract_relation, ContractRelation::Uncertain);
        assert_eq!(result.evidence_tool_call_ids, ["call-7"]);
        assert!(!result.remaining_risk);
    }

    #[test]
    fn accepts_v2_contract_relation_without_breaking_v1_replay() {
        let payload = valid_payload("current-contract")
            .replace("vsc-relay.health-result.v1", "vsc-relay.health-result.v2")
            .replace(
                "\"status\":\"claimed_complete\"",
                "\"status\":\"working\",\"contract_relation\":\"expands\"",
            );
        let result = parse_health_result(&payload, "current-contract").unwrap();
        assert_eq!(result.status, HealthResultStatus::Working);
        assert_eq!(result.contract_relation, ContractRelation::Expands);

        let laundered_v1 =
            payload.replace("vsc-relay.health-result.v2", "vsc-relay.health-result.v1");
        assert_eq!(
            parse_health_result(&laundered_v1, "current-contract"),
            Err(HealthResultError::InvalidProtocol)
        );
    }

    #[test]
    fn v3_accepts_only_bounded_unresolved_delegate_conflicts() {
        let payload = format!(
            "{HEALTH_RESULT_OPEN}\n{{\"protocol\":\"vsc-relay.health-result.v3\",\
             \"steer_id\":\"current-contract\",\"status\":\"working\",\
             \"contract_relation\":\"same\",\"state_conflict\":\"unresolved\",\
             \"conflict_tool_call_ids\":[\"delegate-1\",\"delegate-2\"],\
             \"evidence_tool_call_ids\":[],\"remaining_risk\":true}}\n\
             {HEALTH_RESULT_CLOSE}"
        );
        let result = parse_health_result(&payload, "current-contract").unwrap();
        assert_eq!(result.state_conflict, StateConflict::Unresolved);
        assert_eq!(result.conflict_tool_call_ids.len(), 2);

        let false_completion = payload
            .replace("\"status\":\"working\"", "\"status\":\"claimed_complete\"")
            .replace("\"remaining_risk\":true", "\"remaining_risk\":false");
        assert_eq!(
            parse_health_result(&false_completion, "current-contract"),
            Err(HealthResultError::InvalidReference)
        );
        let laundered_v2 = payload.replace(HEALTH_PROTOCOL_V3, HEALTH_PROTOCOL_V2);
        assert_eq!(
            parse_health_result(&laundered_v2, "current-contract"),
            Err(HealthResultError::InvalidProtocol)
        );
    }

    #[test]
    fn v4_accepts_bounded_exact_quote_certificate_without_tool_ids() {
        let payload = format!(
            "{HEALTH_RESULT_OPEN}\n{{\"protocol\":\"vsc-relay.health-result.v4\",\
             \"steer_id\":\"current-contract\",\"status\":\"working\",\
             \"contract_relation\":\"same\",\"state_conflict\":\"unresolved\",\
             \"state_conflicts\":[{{\
               \"obligation_quotes\":[\"verify parser behavior\"],\
               \"relation\":\"same_topic\",\"version\":\"current\",\
               \"left\":{{\"source\":\"assistant\",\"quote\":\"parser behavior is correct\",\"stance\":\"supports\"}},\
               \"right\":{{\"source\":\"user\",\"quote\":\"parser behavior is still broken\",\"stance\":\"contradicts\"}}\
             }}],\"conflict_tool_call_ids\":[],\
             \"evidence_tool_call_ids\":[],\"remaining_risk\":true}}\n\
             {HEALTH_RESULT_CLOSE}"
        );
        let result = parse_health_result(&payload, SESSION_HEALTH_STEER_ID).unwrap();
        assert_eq!(result.state_conflicts.len(), 1);
        assert!(result.conflict_tool_call_ids.is_empty());

        let same_stance = payload.replace("\"stance\":\"contradicts\"", "\"stance\":\"supports\"");
        assert_eq!(
            parse_health_result(&same_stance, SESSION_HEALTH_STEER_ID),
            Err(HealthResultError::InvalidReference)
        );
        let laundered_v3 = payload.replace(HEALTH_PROTOCOL_V4, HEALTH_PROTOCOL_V3);
        assert_eq!(
            parse_health_result(&laundered_v3, SESSION_HEALTH_STEER_ID),
            Err(HealthResultError::InvalidProtocol)
        );
    }

    #[test]
    fn rejects_stale_unknown_duplicate_and_non_terminal_data() {
        assert_eq!(
            parse_health_result(&valid_payload("obl-0-1"), "obl-0-2"),
            Err(HealthResultError::StaleChallenge)
        );
        let unknown = valid_payload("obl-0-1").replace(
            "\"remaining_risk\":false",
            "\"remaining_risk\":false,\"explanation\":\"trust me\"",
        );
        assert_eq!(
            parse_health_result(&unknown, "obl-0-1"),
            Err(HealthResultError::InvalidJson)
        );
        let duplicate = valid_payload("obl-0-1").replace("[\"call-7\"]", "[\"call-7\",\"call-7\"]");
        assert_eq!(
            parse_health_result(&duplicate, "obl-0-1"),
            Err(HealthResultError::InvalidReference)
        );
        let trailing = format!("{}\nmore prose", valid_payload("obl-0-1"));
        assert_eq!(
            parse_health_result(&trailing, "obl-0-1"),
            Err(HealthResultError::Missing)
        );
    }

    #[test]
    fn steer_marker_must_be_first_and_strict() {
        assert_eq!(
            health_steer_id(" [VSC_RELAY_HEALTH_STEER v2 id=obl-3-4]\nbody"),
            Some("obl-3-4")
        );
        assert_eq!(
            health_steer_id("quoted: [VSC_RELAY_HEALTH_STEER v2 id=obl-3-4]"),
            None
        );
        assert_eq!(
            health_steer_id("[VSC_RELAY_HEALTH_STEER v2 id=bad id]"),
            None
        );
    }

    #[test]
    fn session_protocol_is_bounded_and_uses_the_strict_schema() {
        let context = session_health_protocol_context();
        assert!(context.len() < 4_500);
        assert!(context.contains(SESSION_HEALTH_STEER_ID));
        assert!(context.contains(HEALTH_RESULT_OPEN));
        assert!(context.contains(HEALTH_RESULT_CLOSE));
        assert!(context.contains(HEALTH_PROTOCOL_V4));
        assert!(context.contains("\"contract_relation\":\"same\""));
        assert!(context.contains("\"state_conflict\":\"none\""));
        assert!(context.contains("\"state_conflicts\":[]"));
        assert!(!context.contains("session_id"));
        assert!(!context.contains("goal"));
    }

    #[test]
    #[ignore = "manual performance measurement"]
    fn parse_cost_probe_100k() {
        let input = valid_payload("obl-0-1");
        let started = std::time::Instant::now();
        for _ in 0..100_000 {
            let result = parse_health_result(std::hint::black_box(&input), "obl-0-1").unwrap();
            std::hint::black_box(result);
        }
        let elapsed = started.elapsed();
        eprintln!(
            "feedback_parse_probe iterations=100000 elapsed_ms={} ns_per_parse={}",
            elapsed.as_millis(),
            elapsed.as_nanos() / 100_000
        );
    }
}
