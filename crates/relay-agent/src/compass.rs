use anyhow::{bail, Context, Result};
use relay_cognitive::ObservationLog;
use relay_compass::health::{
    SessionPhase, SOURCE_AGENT, SOURCE_CONTRACT, SOURCE_DELEGATE, SOURCE_RUNTIME, SOURCE_TOOL,
    SOURCE_USER,
};
use relay_compass::predictive::{GapFactor, PredictiveAction, PredictiveParams};
use relay_compass::staged::{assemble_and_evaluate, semantic_inputs, StagedContext};
use relay_compass::{
    AtomizationMode, ContractAtomKind, ContractLedger, InformationProfile, LedgerSeed,
    ObligationOrigin, ObligationState, ProofLayer, SemanticFacts, SemanticStep, SessionAnalysis,
    StepRole, TopicRelation, Vector, SIM_WORDS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const DOSSIER_SCHEMA: u32 = 2;
const PROBLEM_RADIUS: f32 = 0.38;
const COOLDOWN_TURNS: u64 = 4;

pub struct Analysis {
    pub goal: String,
    pub core: SessionAnalysis,
    pub problem_risk: f32,
    pub risk: f32,
    pub goal_terms: Vec<String>,
    pub specifics: Vec<String>,
    information_profile: InformationProfile,
    information_key: [u8; 32],
}

pub struct GuardResult {
    pub selected: String,
    pub accepted: bool,
    pub reason: String,
    pub risk: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompletionSnapshot {
    pub active: usize,
    pub open: usize,
    pub claimed_unverified: usize,
    pub verified: usize,
    pub contradicted: usize,
    pub disputed: usize,
    pub stale: usize,
    pub layer_mismatches: usize,
    pub coverage: f32,
    pub completion_scope_gap: bool,
    pub proof_deficit: bool,
    pub current_claimed_complete: bool,
}

impl CompletionSnapshot {
    pub(crate) fn from_ledger(ledger: &ContractLedger) -> Self {
        Self {
            active: ledger.signals.active,
            open: ledger.signals.open,
            claimed_unverified: ledger.signals.claimed_unverified,
            verified: ledger.signals.verified,
            contradicted: ledger.signals.contradicted,
            disputed: ledger.signals.disputed,
            stale: ledger.signals.stale,
            layer_mismatches: ledger.signals.layer_mismatches,
            coverage: ledger.signals.coverage,
            completion_scope_gap: ledger.signals.completion_scope_gap,
            proof_deficit: ledger.signals.proof_deficit,
            current_claimed_complete: ledger.feedback.current_claimed_complete,
        }
    }

    pub fn unresolved(self) -> usize {
        self.open + self.claimed_unverified + self.contradicted + self.disputed + self.stale
    }

    pub fn is_verified_final(self) -> bool {
        self.active > 0
            && self.verified == self.active
            && self.unresolved() == 0
            && self.layer_mismatches == 0
            && !self.completion_scope_gap
            && !self.proof_deficit
            && self.coverage >= 0.999
    }

    pub fn status_label(self) -> &'static str {
        if self.is_verified_final() {
            "verified_final"
        } else if self.active > 0 {
            "needs_proof"
        } else {
            "unproven"
        }
    }
}

pub struct StagedAssessment {
    pub action: PredictiveAction,
    pub dominant_factor: GapFactor,
    pub gap_posterior: f32,
    pub corroborating_sources: u8,
    pub proof_request: Option<String>,
    pub directive: Option<String>,
    pub gap_signature: String,
    pub explanation: String,
    pub semantic_calibrated: bool,
    pub semantic_backend: String,
    pub semantic_model: String,
    pub deterministic_feedback: bool,
    pub obligation_id: Option<String>,
    pub contract_coverage: f32,
    pub open_obligations: usize,
    pub completion: CompletionSnapshot,
}

pub struct Notice {
    pub risk: f32,
    pub stuck: bool,
    pub risk_alert: bool,
    pub staged: Option<StagedAssessment>,
}

struct StagedComputed {
    assessment: StagedAssessment,
    block: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PrototypeStore {
    #[serde(default)]
    prototypes: Vec<Prototype>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Prototype {
    id: String,
    bad: u32,
    good: u32,
    votes: Vec<i32>,
    updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ContinuationStore {
    #[serde(default)]
    links: BTreeMap<String, String>,
}

fn root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("compass")
}

fn safe_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn dossier_path(session_id: &str) -> PathBuf {
    root().join(format!("{}.json", safe_id(session_id)))
}

fn problems_path() -> PathBuf {
    root().join("problems.json")
}

fn continuations_path() -> PathBuf {
    root().join("continuations.json")
}

fn digest_key_path() -> PathBuf {
    root().join("dossier.key")
}

fn information_profile_path() -> PathBuf {
    root().join("information-profile-v2.bin")
}

#[cfg(unix)]
fn information_profile_lock_path() -> PathBuf {
    root().join("information-profile-v2.lock")
}

struct InformationProfileLock {
    #[cfg(unix)]
    file: std::fs::File,
}

impl InformationProfileLock {
    fn acquire() -> Result<Self> {
        crate::fsutil::secure_dir(&root());
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;

            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .mode(0o600)
                .open(information_profile_lock_path())?;

            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self { file })
        }
        #[cfg(windows)]
        {
            Ok(Self {})
        }
    }
}

#[cfg(unix)]
impl Drop for InformationProfileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_digest_key(path: &std::path::Path) -> Option<[u8; 32]> {
    <[u8; 32]>::try_from(std::fs::read(path).ok()?.as_slice()).ok()
}

fn digest_key() -> Result<[u8; 32]> {
    let path = digest_key_path();
    if let Some(key) = read_digest_key(&path) {
        return Ok(key);
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key)
        .map_err(|error| anyhow::anyhow!("generate dossier digest key: {error}"))?;
    if crate::fsutil::secure_create_new(&path, &key)? {
        return Ok(key);
    }
    if let Some(existing) = read_digest_key(&path) {
        return Ok(existing);
    }
    crate::fsutil::secure_write(&path, &key)?;
    Ok(key)
}

fn private_digest(text: &str) -> Result<String> {
    let digest = blake3::keyed_hash(&digest_key()?, text.as_bytes());
    Ok(digest.to_hex().as_str()[..32].to_string())
}

fn information_profile_binding(key: &[u8; 32]) -> [u8; 16] {
    let digest = blake3::keyed_hash(key, b"vsc-relay-information-profile-binding-v2");
    digest.as_bytes()[..16].try_into().unwrap_or([0; 16])
}

fn load_information_profile(key: &[u8; 32]) -> InformationProfile {
    let Ok(bytes) = std::fs::read(information_profile_path()) else {
        return InformationProfile::default();
    };
    let Some((binding, payload)) = bytes.split_at_checked(16) else {
        return InformationProfile::default();
    };
    if binding != information_profile_binding(key) {
        return InformationProfile::default();
    }
    InformationProfile::decode(payload).unwrap_or_default()
}

fn bound_information_profile(profile: &InformationProfile, key: &[u8; 32]) -> Vec<u8> {
    let payload = profile.encode();
    let mut bytes = Vec::with_capacity(16 + payload.len());
    bytes.extend_from_slice(&information_profile_binding(key));
    bytes.extend_from_slice(&payload);
    bytes
}

fn observe_user_documents(
    source_id: &str,
    turns: &[String],
) -> Result<(InformationProfile, [u8; 32])> {
    let _lock = InformationProfileLock::acquire()?;
    let key = digest_key()?;

    let profile = load_information_profile(&key);
    let mut updated = profile.clone();
    let mut changed = false;
    for (index, turn) in turns.iter().enumerate() {
        if relay_compass::is_transcript_noise(turn) {
            continue;
        }
        let document_id = format!("{source_id}\u{1f}{index}");
        changed |= updated.observe_document(&document_id, turn, &key);
    }
    if changed {
        crate::fsutil::secure_write(
            &information_profile_path(),
            &bound_information_profile(&updated, &key),
        )?;
    }
    Ok((profile, key))
}

pub fn information_profile_status() -> Result<String> {
    let _lock = InformationProfileLock::acquire()?;
    let key = digest_key()?;
    let profile = load_information_profile(&key);
    Ok(format!(
        "information profile: documents={} memory_bytes={} raw_text=0",
        profile.documents(),
        profile.memory_bytes()
    ))
}

pub fn bootstrap_information_profile() -> Result<String> {
    let _lock = InformationProfileLock::acquire()?;
    let key = digest_key()?;
    let mut profile = load_information_profile(&key);
    let before = profile.documents();
    let mut files = 0u64;
    let projects = dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("projects");
    if let Ok(projects) = std::fs::read_dir(projects) {
        for project in projects.flatten().filter(|entry| entry.path().is_dir()) {
            let Ok(entries) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                    continue;
                }
                files += 1;
                let source = path.as_os_str().as_encoded_bytes();
                let source = blake3::keyed_hash(&key, source).to_hex().to_string();
                for step in relay_adapters::claude::semantic_steps(&path)
                    .into_iter()
                    .filter(|step| step.role == StepRole::User)
                {
                    if relay_compass::is_transcript_noise(&step.text) {
                        continue;
                    }
                    profile.observe_document(
                        &format!("claude-bootstrap:{source}:{}", step.index),
                        &step.text,
                        &key,
                    );
                }
            }
        }
    }
    if profile.documents() != before {
        crate::fsutil::secure_write(
            &information_profile_path(),
            &bound_information_profile(&profile, &key),
        )?;
    }
    Ok(format!(
        "information profile bootstrap: principal_claude_files={} added_documents={} total_documents={} memory_bytes={} raw_text=0",
        files,
        profile.documents().saturating_sub(before),
        profile.documents(),
        profile.memory_bytes()
    ))
}

fn obligation_vector_mask() -> Result<[u64; SIM_WORDS]> {
    let mut bytes = [0u8; relay_compass::SIM_BITS / 8];
    let mut hasher = blake3::Hasher::new_keyed(&digest_key()?);
    hasher.update(b"vsc-relay-obligation-vector-mask-v1");
    hasher.finalize_xof().fill(&mut bytes);
    let mut words = [0u64; SIM_WORDS];
    for (index, chunk) in bytes.as_chunks::<8>().0.iter().enumerate() {
        words[index] = u64::from_le_bytes(*chunk);
    }
    Ok(words)
}

fn private_vector_signature(text: &str) -> Option<String> {
    let Vector::Sim(mut words) = relay_compass::embed(text)? else {
        return None;
    };
    let mask = obligation_vector_mask().ok()?;
    for (word, mask) in words.iter_mut().zip(mask) {
        *word ^= mask;
    }
    vector_hex(&Vector::Sim(words))
}

fn private_signature_vector(signature: &str) -> Option<Vector> {
    if signature.len() != SIM_WORDS * 16 {
        return None;
    }
    let mask = obligation_vector_mask().ok()?;
    let mut words = [0u64; SIM_WORDS];
    for (index, word) in words.iter_mut().enumerate() {
        let start = index * 16;
        *word = u64::from_str_radix(&signature[start..start + 16], 16).ok()? ^ mask[index];
    }
    Some(Vector::Sim(words))
}

fn backup_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    path.with_extension(format!("{extension}.bak"))
}

fn payload_checksum(payload: &Value) -> Option<String> {
    let text = serde_json::to_string(payload).ok()?;
    Some(blake3::hash(text.as_bytes()).to_hex().to_string())
}

fn read_payload_file(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let envelope: Value = serde_json::from_str(&text).ok()?;
    if envelope.get("schema")?.as_u64()? != DOSSIER_SCHEMA as u64 {
        return None;
    }
    let payload = envelope.get("payload")?.clone();
    let expected = envelope.get("checksum")?.as_str()?;
    (payload_checksum(&payload)?.as_str() == expected).then_some(payload)
}

fn read_payload(path: &Path) -> Option<Value> {
    read_payload_file(path).or_else(|| read_payload_file(&backup_path(path)))
}

fn payload_bytes(payload: &Value) -> Result<Vec<u8>> {
    let checksum = payload_checksum(payload).context("serialize compass payload")?;
    let envelope = json!({
        "schema": DOSSIER_SCHEMA,
        "checksum": checksum,
        "payload": payload,
    });
    Ok(serde_json::to_vec_pretty(&envelope)?)
}

fn write_payload_fresh(path: &Path, payload: &Value) -> Result<()> {
    let bytes = payload_bytes(payload)?;
    crate::fsutil::secure_write(&backup_path(path), &bytes)?;
    crate::fsutil::secure_write(path, &bytes)?;
    Ok(())
}

fn write_payload(path: &Path, payload: &Value) -> Result<()> {
    let bytes = payload_bytes(payload)?;

    let backup = if read_payload_file(path).is_some() {
        std::fs::read(path).unwrap_or_else(|_| bytes.clone())
    } else {
        bytes.clone()
    };
    crate::fsutil::secure_write(&backup_path(path), &backup)?;
    crate::fsutil::secure_write(path, &bytes)?;
    Ok(())
}

fn redact_legacy_payload(mut payload: Value) -> Result<Value> {
    let Some(object) = payload.as_object_mut() else {
        return Ok(json!({"migration": "legacy_non_object"}));
    };
    if let Some(goal) = object
        .remove("goal")
        .and_then(|value| value.as_str().map(str::to_string))
    {
        object.insert("goal_digest".to_string(), json!(private_digest(&goal)?));
    }
    if let Some(terms) = object.remove("goal_terms") {
        object.insert(
            "goal_term_count".to_string(),
            json!(terms.as_array().map(Vec::len).unwrap_or_default()),
        );
    }
    if let Some(specifics) = object.remove("specifics") {
        object.insert(
            "specific_count".to_string(),
            json!(specifics.as_array().map(Vec::len).unwrap_or_default()),
        );
    }
    Ok(payload)
}

fn envelope_payload_any(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let envelope: Value = serde_json::from_str(&text).ok()?;
    envelope.get("payload").cloned()
}

fn contains_legacy_text_fields(payload: &Value) -> bool {
    payload.as_object().is_some_and(|object| {
        object.contains_key("goal")
            || object.contains_key("goal_terms")
            || object.contains_key("specifics")
    })
}

fn migrate_dossier_file(path: &Path) -> Result<bool> {
    let backup = backup_path(path);
    if let Some(payload) = read_payload_file(path) {
        let backup_payload = read_payload_file(&backup);
        if contains_legacy_text_fields(&payload)
            || backup_payload
                .as_ref()
                .is_some_and(contains_legacy_text_fields)
        {
            write_payload_fresh(path, &redact_legacy_payload(payload)?)?;
            return Ok(true);
        }
        if backup_payload.is_none() {
            write_payload_fresh(path, &payload)?;
            return Ok(true);
        }
        return Ok(false);
    }
    if let Some(payload) = envelope_payload_any(path).or_else(|| envelope_payload_any(&backup)) {
        write_payload_fresh(path, &redact_legacy_payload(payload)?)?;
        return Ok(true);
    }

    write_payload_fresh(path, &json!({"migration": "unreadable_dossier_redacted"}))?;
    Ok(true)
}

pub fn migrate_dossiers() -> Result<usize> {
    let dir = root();
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut migrated = 0usize;
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if path.is_file() && name.ends_with(".json") {
            migrated += usize::from(migrate_dossier_file(&path)?);
        }
    }

    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let Some(main_name) = name.strip_suffix(".json.bak") else {
            continue;
        };
        let main = dir.join(format!("{main_name}.json"));
        if !main.exists() {
            migrated += usize::from(migrate_dossier_file(&main)?);
        }
    }
    Ok(migrated)
}

pub fn find_claude_jsonl(session_id: &str) -> Option<PathBuf> {
    let projects = dirs::home_dir()?.join(".claude").join("projects");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let path = entry.path().join(format!("{session_id}.jsonl"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn session_turns(session_id: &str) -> Result<Vec<String>> {
    let path = find_claude_jsonl(session_id).context("session transcript not found")?;
    let turns = relay_adapters::claude::user_messages(&path);
    if turns.is_empty() {
        bail!("session has no user turns");
    }
    Ok(turns)
}

fn vector_hex(vector: &Vector) -> Option<String> {
    let Vector::Sim(words) = vector else {
        return None;
    };
    Some(words.iter().map(|word| format!("{word:016x}")).collect())
}

fn vector_votes(vector: &Vector) -> Option<Vec<i32>> {
    let Vector::Sim(words) = vector else {
        return None;
    };
    Some(
        (0..relay_compass::SIM_BITS)
            .map(|index| {
                if (words[index / 64] >> (index % 64)) & 1 == 1 {
                    1
                } else {
                    -1
                }
            })
            .collect(),
    )
}

fn votes_vector(votes: &[i32]) -> Option<Vector> {
    if votes.len() != relay_compass::SIM_BITS {
        return None;
    }
    let mut words = [0u64; SIM_WORDS];
    for (index, vote) in votes.iter().enumerate() {
        if *vote > 0 {
            words[index / 64] |= 1u64 << (index % 64);
        }
    }
    Some(Vector::Sim(words))
}

fn load_prototypes() -> PrototypeStore {
    read_payload(&problems_path())
        .and_then(|payload| serde_json::from_value(payload).ok())
        .unwrap_or_default()
}

fn save_prototypes(store: &PrototypeStore) -> Result<()> {
    write_payload(&problems_path(), &serde_json::to_value(store)?)
}

fn load_continuations() -> ContinuationStore {
    read_payload(&continuations_path())
        .and_then(|payload| serde_json::from_value(payload).ok())
        .unwrap_or_default()
}

fn save_continuations(store: &ContinuationStore) -> Result<()> {
    write_payload(&continuations_path(), &serde_json::to_value(store)?)
}

fn parse_obligation_state(value: &str) -> Option<ObligationState> {
    match value {
        "open" => Some(ObligationState::Open),
        "claimed" => Some(ObligationState::Claimed),
        "verified" => Some(ObligationState::Verified),
        "contradicted" => Some(ObligationState::Contradicted),
        "superseded" => Some(ObligationState::Superseded),
        _ => None,
    }
}

fn parse_proof_layer(value: &str) -> Option<ProofLayer> {
    match value {
        "unknown" => Some(ProofLayer::Unknown),
        "inspection" => Some(ProofLayer::Inspection),
        "unit" => Some(ProofLayer::Unit),
        "integration" => Some(ProofLayer::Integration),
        "live" => Some(ProofLayer::Live),
        "acceptance" => Some(ProofLayer::Acceptance),
        _ => None,
    }
}

fn continuation_seeds(session_id: &str) -> Vec<LedgerSeed> {
    let store = load_continuations();
    let Some(parent) = store.links.get(session_id) else {
        return Vec::new();
    };
    let Some(ledger) = read_payload(&dossier_path(parent))
        .and_then(|payload| payload.get("contract_ledger").cloned())
    else {
        return Vec::new();
    };
    let source_ref = private_digest(parent)
        .map(|digest| digest.chars().take(16).collect::<String>())
        .unwrap_or_else(|_| "parent".to_string());
    ledger
        .get("obligations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(32)
        .filter_map(|obligation| {
            let signature = obligation.get("text_signature")?.as_str()?;
            let state = parse_obligation_state(obligation.get("state")?.as_str()?)?;
            let required_layer = parse_proof_layer(obligation.get("required_layer")?.as_str()?)?;
            let observed_layer = parse_proof_layer(obligation.get("observed_layer")?.as_str()?)?;
            Some(LedgerSeed {
                fingerprint: private_signature_vector(signature)?,
                state,
                required_layer,
                observed_layer,
                source_ref: source_ref.clone(),
            })
        })
        .collect()
}

pub fn link_continuation(child: &str, parent: &str) -> Result<String> {
    let child = safe_id(child.trim());
    let parent = safe_id(parent.trim());
    if child.is_empty() || parent.is_empty() || child == parent {
        bail!("usage: automation compass-link <child_session_id> <parent_session_id>");
    }
    let parent_payload = read_payload(&dossier_path(&parent))
        .context("parent dossier is missing; assess/inspect the parent first")?;
    if parent_payload.get("contract_ledger").is_none() {
        bail!("parent dossier has no contract ledger; run compass-assess first");
    }
    let mut store = load_continuations();
    let mut cursor = parent.as_str();
    for _ in 0..32 {
        if cursor == child {
            bail!("continuation link would create a cycle");
        }
        let Some(next) = store.links.get(cursor) else {
            break;
        };
        cursor = next;
    }
    store.links.insert(child.clone(), parent.clone());
    save_continuations(&store)?;
    Ok(format!("continuation linked: {child} <- {parent}"))
}

pub fn unlink_continuation(child: &str) -> Result<String> {
    let child = safe_id(child.trim());
    if child.is_empty() {
        bail!("usage: automation compass-unlink <child_session_id>");
    }
    let mut store = load_continuations();
    let removed = store.links.remove(&child).is_some();
    save_continuations(&store)?;
    Ok(format!(
        "continuation {}: {child}",
        if removed { "unlinked" } else { "not linked" }
    ))
}

fn similarity_probability(distance: f32) -> f32 {
    1.0 / (1.0 + (12.0 * (distance - PROBLEM_RADIUS)).exp())
}

fn problem_risk(state: &Vector) -> f32 {
    load_prototypes()
        .prototypes
        .iter()
        .filter(|prototype| prototype.bad >= 2 && prototype.bad > prototype.good)
        .filter_map(|prototype| {
            let centroid = votes_vector(&prototype.votes)?;
            let distance = relay_compass::distance(state, &centroid)?;
            let posterior =
                (prototype.bad as f32 + 1.0) / (prototype.bad as f32 + prototype.good as f32 + 2.0);
            Some(similarity_probability(distance) * posterior)
        })
        .fold(0.0, f32::max)
}

enum PinnedGoalIndex {
    Unpinned,
    Found(usize),
    Missing,
}

fn pinned_goal_index(dossier_key: &str, turns: &[String]) -> PinnedGoalIndex {
    let pinned = read_payload(&dossier_path(dossier_key)).and_then(|payload| {
        payload
            .get("goal_digest")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    if let Some(pinned) = pinned {
        return turns
            .iter()
            .position(|turn| {
                !relay_compass::is_transcript_noise(turn)
                    && private_digest(turn.trim()).is_ok_and(|digest| digest == pinned)
            })
            .map(PinnedGoalIndex::Found)
            .unwrap_or(PinnedGoalIndex::Missing);
    }
    PinnedGoalIndex::Unpinned
}

fn goal_index_profiled(
    dossier_key: &str,
    turns: &[String],
    _profile: &InformationProfile,
    _key: &[u8; 32],
) -> Option<usize> {
    match pinned_goal_index(dossier_key, turns) {
        PinnedGoalIndex::Found(index) => Some(index),
        PinnedGoalIndex::Missing => None,
        PinnedGoalIndex::Unpinned => relay_compass::select_goal_index_authoritative(turns),
    }
}

#[cfg(test)]
fn select_pinned_goal_index(dossier_key: &str, turns: &[String]) -> Option<usize> {
    let profile = InformationProfile::default();
    goal_index_profiled(dossier_key, turns, &profile, &[0; 32])
}

#[cfg(test)]
fn analyze_turns_pinned(dossier_key: &str, turns: &[String]) -> Option<SessionAnalysis> {
    let profile = InformationProfile::default();
    analyze_turns_pinned_profiled(dossier_key, turns, &profile, &[0; 32])
}

fn analyze_turns_pinned_profiled(
    dossier_key: &str,
    turns: &[String],
    profile: &InformationProfile,
    key: &[u8; 32],
) -> Option<SessionAnalysis> {
    let goal_index = goal_index_profiled(dossier_key, turns, profile, key)?;
    relay_compass::analyze_turns_from_goal_index(turns, goal_index)
}

pub fn analyze_session(session_id: &str) -> Result<Analysis> {
    let turns = session_turns(session_id)?;
    let (information_profile, information_key) =
        observe_user_documents(&format!("claude:{session_id}"), &turns)?;
    let core =
        analyze_turns_pinned_profiled(session_id, &turns, &information_profile, &information_key)
            .context("no substantive goal")?;
    let goal = turns[core.goal_index].trim().to_string();
    let problem_risk = problem_risk(&core.state);
    let base_risk = core.last.as_ref().map(|value| value.risk).unwrap_or(0.0);
    let risk = relay_compass::combine_risk(base_risk, problem_risk);
    let goal_terms = information_profile
        .ranked_terms(&goal, &information_key)
        .into_iter()
        .take(32)
        .collect();
    let mut specifics = BTreeSet::new();
    for turn in turns.iter().rev().take(64) {
        for literal in relay_compass::literals(turn) {
            if literal.chars().count() <= 200 {
                specifics.insert(literal);
            }
        }
    }
    Ok(Analysis {
        goal,
        core,
        problem_risk,
        risk,
        goal_terms,
        specifics: specifics.into_iter().take(48).collect(),
        information_profile,
        information_key,
    })
}

fn phase_of_state(state: &relay_core::state::ClaudeState) -> (SessionPhase, bool) {
    use relay_core::state::ClaudeState;
    match state {
        ClaudeState::Idle => (SessionPhase::Idle, false),
        ClaudeState::Error { .. } => (SessionPhase::Error, false),
        ClaudeState::PendingQuestion(_) | ClaudeState::AwaitingPermission { .. } => {
            (SessionPhase::AwaitingUser, true)
        }
        _ => (SessionPhase::Working, false),
    }
}

#[cfg(test)]
fn phase_of(text: &str) -> (SessionPhase, bool) {
    phase_of_state(&relay_core::state::reduce_claude(text).state)
}

fn factor_label(factor: GapFactor) -> &'static str {
    match factor {
        GapFactor::ScopeCompression => "scope_compression",
        GapFactor::LayerMismatch => "layer_mismatch",
        GapFactor::FalseBlocker => "false_blocker",
        GapFactor::StaleEvidence => "stale_evidence",
        GapFactor::ProxyCapture => "proxy_capture",
    }
}

fn action_label(action: PredictiveAction) -> &'static str {
    match action {
        PredictiveAction::Observe => "observe",
        PredictiveAction::AskProof => "ask_proof",
        PredictiveAction::MicroSteer => "micro_steer",
        PredictiveAction::FullSteer => "full_steer",
        PredictiveAction::Escalate => "escalate",
    }
}

fn source_names(mask: u8) -> Vec<&'static str> {
    let mut names = Vec::new();
    if mask & SOURCE_CONTRACT != 0 {
        names.push("contract");
    }
    if mask & SOURCE_AGENT != 0 {
        names.push("agent");
    }
    if mask & SOURCE_USER != 0 {
        names.push("user");
    }
    if mask & SOURCE_TOOL != 0 {
        names.push("tool");
    }
    if mask & SOURCE_RUNTIME != 0 {
        names.push("runtime");
    }
    if mask & SOURCE_DELEGATE != 0 {
        names.push("delegate");
    }
    names
}

fn gap_signature(factor: GapFactor, mask: u8, goal_hash: &str, obligation_id: &str) -> String {
    short_hash(&format!(
        "{}|{}|{}|{}",
        factor_label(factor),
        mask,
        goal_hash,
        obligation_id
    ))
}

fn is_cooldown(prior_sig: Option<&str>, sig: &str, observed_turns: u64, prior_turn: u64) -> bool {
    prior_sig == Some(sig) && observed_turns.saturating_sub(prior_turn) <= COOLDOWN_TURNS
}

fn next_previous_steers(prior_steers: u8, pivot: bool, is_steer: bool) -> u8 {
    if pivot {
        0
    } else if is_steer {
        prior_steers.saturating_add(1)
    } else {
        prior_steers
    }
}

pub fn record_steer(session_id: &str, gap_signature: &str) {
    let path = dossier_path(session_id);
    let Some(mut payload) = read_payload(&path) else {
        return;
    };
    let prior = payload
        .get("previous_steers")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u8;
    let observed = payload
        .get("observed_turns")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "previous_steers".to_string(),
            json!(next_previous_steers(prior, false, true)),
        );
        object.insert("gap_signature".to_string(), json!(gap_signature));
        object.insert("gap_signature_turn".to_string(), json!(observed));
    }
    let _ = write_payload(&path, &payload);
}

fn staged_target(literals: &BTreeSet<String>, keywords: &[String]) -> String {
    literals
        .iter()
        .next()
        .cloned()
        .or_else(|| keywords.first().cloned())
        .unwrap_or_else(|| "the requested goal".to_string())
}

fn layer_label(layer: ProofLayer) -> &'static str {
    match layer {
        ProofLayer::Unknown => "unknown",
        ProofLayer::Inspection => "inspection",
        ProofLayer::Unit => "unit",
        ProofLayer::Integration => "integration",
        ProofLayer::Live => "live",
        ProofLayer::Acceptance => "acceptance",
    }
}

fn obligation_state_label(state: ObligationState) -> &'static str {
    match state {
        ObligationState::Open => "open",
        ObligationState::Claimed => "claimed",
        ObligationState::Verified => "verified",
        ObligationState::Contradicted => "contradicted",
        ObligationState::Superseded => "superseded",
    }
}

fn obligation_kind_label(kind: ContractAtomKind) -> &'static str {
    match kind {
        ContractAtomKind::Deliverable => "deliverable",
        ContractAtomKind::Constraint => "constraint",
        ContractAtomKind::Acceptance => "acceptance",
        ContractAtomKind::Evidence => "evidence",
        ContractAtomKind::Other => "other",
    }
}

fn obligation_origin_label(origin: ObligationOrigin) -> &'static str {
    match origin {
        ObligationOrigin::WholeContract => "whole_contract",
        ObligationOrigin::Structural => "structural",
        ObligationOrigin::Extractive => "extractive",
    }
}

fn atomization_mode_label(mode: AtomizationMode) -> &'static str {
    match mode {
        AtomizationMode::Whole => "whole",
        AtomizationMode::Structural => "structural",
        AtomizationMode::Extractive => "extractive",
    }
}

fn topic_relation_label(relation: TopicRelation) -> &'static str {
    match relation {
        TopicRelation::SameTopic => "same_topic",
        TopicRelation::SameArtifact => "same_artifact",
        TopicRelation::Dependency => "dependency",
    }
}

fn redacted_ledger(ledger: &ContractLedger) -> Value {
    json!({
        "schema": 1,
        "epoch": ledger.epoch,
        "coverage": ledger.signals.coverage,
        "atomization": {
            "mode": atomization_mode_label(ledger.atomization.mode),
            "proposed": ledger.atomization.proposed,
            "accepted": ledger.atomization.accepted,
            "signal_coverage": ledger.atomization.signal_coverage,
            "literals_preserved": ledger.atomization.literals_preserved,
            "clause_boundaries_preserved": ledger.atomization.clause_boundaries_preserved,
            "source_conserved": ledger.atomization.source_conserved,
        },
        "counts": {
            "active": ledger.signals.active,
            "open": ledger.signals.open,
            "claimed_unverified": ledger.signals.claimed_unverified,
            "verified": ledger.signals.verified,
            "contradicted": ledger.signals.contradicted,
            "disputed": ledger.signals.disputed,
            "stale": ledger.signals.stale,
            "layer_mismatches": ledger.signals.layer_mismatches,
        },
        "agent_feedback": {
            "challenged": ledger.feedback.challenged,
            "accepted": ledger.feedback.accepted,
            "rejected": ledger.feedback.rejected,
            "evidence_refs": ledger.feedback.evidence_refs,
            "matched_evidence_refs": ledger.feedback.matched_evidence_refs,
            "controller_evidence_receipts": ledger.feedback.controller_evidence_receipts,
            "matched_blocker_probe_refs": ledger.feedback.matched_blocker_probe_refs,
            "conflict_refs": ledger.feedback.conflict_refs,
            "matched_conflict_refs": ledger.feedback.matched_conflict_refs,
            "reported_expansions": ledger.feedback.reported_expansions,
            "reported_replacements": ledger.feedback.reported_replacements,
            "current_accepted": ledger.feedback.current_accepted,
            "current_working": ledger.feedback.current_working,
            "current_blocked": ledger.feedback.current_blocked,
            "current_blocker_grounded": ledger.feedback.current_blocker_grounded,
            "current_state_disputed": ledger.feedback.current_state_disputed,
            "current_conflict_source_mask": ledger.feedback.current_conflict_source_mask,
            "current_expands_contract": ledger.feedback.current_expands_contract,
            "current_replaces_contract": ledger.feedback.current_replaces_contract,
            "current_claimed_complete": ledger.feedback.current_claimed_complete,
        },
        "provenance": {
            "tool_uses": ledger.provenance.tool_uses,
            "native_ids": ledger.provenance.native_ids,
            "weak_anchors": ledger.provenance.weak_anchors,
            "unknown_capabilities": ledger.provenance.unknown_capabilities,
            "unmatched_results": ledger.provenance.unmatched_results,
            "weakly_paired_results": ledger.provenance.weakly_paired_results,
            "delegate_reports": ledger.provenance.delegate_reports,
        },
        "topic_intersections": {
            "certificates": ledger.topic_intersections.certificates,
            "grounded": ledger.topic_intersections.grounded,
            "rejected_unknown_version": ledger.topic_intersections.rejected_unknown_version,
            "same_obligation": ledger.topic_intersections.same_obligation,
            "multi_obligation": ledger.topic_intersections.multi_obligation,
            "literal_intersections": ledger.topic_intersections.literal_intersections,
            "vector_candidates": ledger.topic_intersections.vector_candidates,
            "declared_cross_topic": ledger.topic_intersections.declared_cross_topic,
            "source_mask": ledger.topic_intersections.source_mask,
        },
        "topic_edges": ledger.topic_edges.iter().map(|edge| json!({
            "id": edge.id,
            "obligation_ids": edge.obligation_ids,
            "relation": topic_relation_label(edge.relation),
            "source_mask": edge.source_mask,
            "left_step": edge.left_step,
            "right_step": edge.right_step,
            "version_floor": edge.version_floor,
            "literal_intersection": edge.literal_intersection,
            "vector_candidate": edge.vector_candidate,
            "declared_only": edge.declared_only,
        })).collect::<Vec<_>>(),
        "focus_id": ledger.focus_obligation().map(|obligation| obligation.id.as_str()),
        "obligations": ledger.obligations.iter().map(|obligation| json!({
            "id": obligation.id,
            "epoch": obligation.epoch,
            "source_step": obligation.source_step,
            "priority": obligation.priority,
            "state": obligation_state_label(obligation.state),
            "kind": obligation_kind_label(obligation.kind),
            "origin": obligation_origin_label(obligation.origin),
            "text_digest": private_digest(&obligation.text).ok(),
            "text_signature": private_vector_signature(&obligation.text),
            "required_layer": layer_label(obligation.required_layer),
            "observed_layer": layer_label(obligation.observed_layer),
            "last_claim_step": obligation.last_claim_step,
            "last_change_step": obligation.last_change_step,
            "last_evidence_step": obligation.last_evidence_step,
            "evidence_count": obligation.evidence_ids.len(),
        })).collect::<Vec<_>>(),
        "evidence": ledger.evidence.iter().map(|evidence| json!({
            "id": evidence.id,
            "obligation_id": evidence.obligation_id,
            "step": evidence.step,
            "layer": layer_label(evidence.layer),
            "polarity": match evidence.polarity {
                relay_compass::ledger::EvidencePolarity::Supports => "supports",
                relay_compass::ledger::EvidencePolarity::Contradicts => "contradicts",
            },
            "strength": evidence.strength,
            "fresh": evidence.fresh,
            "inherited_from": evidence.inherited_from,
        })).collect::<Vec<_>>(),
        "delegated_reports": ledger.delegated_reports.iter().map(|report| json!({
            "id": report.id,
            "obligation_id": report.obligation_id,
            "step": report.step,
            "correlation_id": report.correlation_id,
            "fresh": report.fresh,
            "failed": report.failed,
        })).collect::<Vec<_>>(),
        "continuation": {
            "seeds": ledger.continuation.seeds,
            "exact_inherited": ledger.continuation.exact_inherited,
            "approximate_advisory": ledger.continuation.approximate_advisory,
            "ambiguous_rejected": ledger.continuation.ambiguous_rejected,
        },
    })
}

fn redacted_observation_boundary(steps: &[SemanticStep]) -> Result<Value> {
    let summary = ObservationLog::from_steps(steps).summary(&digest_key()?);
    Ok(serde_json::to_value(summary)?)
}

fn health_prompt_text(
    factor: GapFactor,
    obligation_id: &str,
    target: &str,
    required: ProofLayer,
    observed: ProofLayer,
    ask_only: bool,
) -> String {
    let mismatch = match factor {
        GapFactor::ScopeCompression => {
            "the active contract still contains an uncovered obligation while scope has narrowed"
        }
        GapFactor::LayerMismatch => {
            "completion or verification is not backed by sufficient fresh evidence"
        }
        GapFactor::FalseBlocker => {
            "a blocker was asserted without a bounded check of the current external state"
        }
        GapFactor::StaleEvidence => "the cited proof predates a later change to the obligation",
        GapFactor::ProxyCapture => "a proxy check is being treated as the requested deliverable",
    };
    let hypothesis = match factor {
        GapFactor::ScopeCompression => "the acceptance ledger was compressed to a local subtask",
        GapFactor::LayerMismatch => "the available check exercises a weaker or unrelated layer",
        GapFactor::FalseBlocker => "the environment model is stale or was not probed",
        GapFactor::StaleEvidence => "the last proof refers to an older artifact version",
        GapFactor::ProxyCapture => "the easiest measurable proxy displaced the product outcome",
    };
    let first_step = if ask_only {
        "Run the smallest bounded check that can resolve the mismatch on the required layer."
    } else {
        "Re-open the cited obligation and inspect the missing or contradictory evidence link."
    };
    format!(
        "[VSC_RELAY_HEALTH_STEER v2 id={obligation_id}]\n\
This is a session-health correction, not a new product scope.\n\
Contract still at risk ({obligation_id}): {target}\n\
Observed mismatch: {mismatch}. Required layer: {}; observed layer: {}.\n\
Likely cause to verify (hypothesis): {hypothesis}.\n\
Recovery, maximum three steps:\n\
1. {first_step}\n\
2. Fix the root cause without weakening or replacing the original acceptance criterion.\n\
3. Re-run fresh verification on the required layer; the local controller records its native receipt.\n\
Do not claim completion until {obligation_id} has fresh sufficient evidence.\n\
Finish the response with exactly one machine-readable envelope (no text after it):\n\
The JSON below is an initial working-state template, not a required verdict. If fresh checks cover the obligation, replace status with claimed_complete and remaining_risk with false.\n\
[VSC_RELAY_HEALTH_RESULT v1]\n\
{{\"protocol\":\"vsc-relay.health-result.v4\",\"steer_id\":\"{obligation_id}\",\"status\":\"working\",\"contract_relation\":\"same\",\"state_conflict\":\"none\",\"state_conflicts\":[],\"conflict_tool_call_ids\":[],\"evidence_tool_call_ids\":[],\"remaining_risk\":true}}\n\
[/VSC_RELAY_HEALTH_RESULT]\n\
Allowed status values are working, blocked, and claimed_complete. Keep contract_relation=same because this correction does not change product scope. Use claimed_complete only after the correction. Always leave evidence_tool_call_ids empty: the local controller owns native ids and attaches fresh typed receipts that occurred inside this bounded challenge; never copy or invent an id. If states disagree, use state_conflict=unresolved and a state_conflicts certificate with exact obligation and source quotes from the current or previous three episodes; never paraphrase. Keep remaining_risk=true and run one direct adjudication probe. A delegate report is not runtime proof. Set remaining_risk=false only when the checks you actually ran cover the required layer.",
        layer_label(required),
        layer_label(observed),
    )
}

fn factor_note(factor: GapFactor) -> &'static str {
    match factor {
        GapFactor::ScopeCompression => "the global goal is being displaced by local fixes",
        GapFactor::LayerMismatch => "proof is claimed at the wrong layer",
        GapFactor::FalseBlocker => "an external blocker is asserted without verification",
        GapFactor::StaleEvidence => "the cited proof predates the latest change",
        GapFactor::ProxyCapture => "a proxy metric stands in for the missing deliverable",
    }
}

fn explanation_text(
    action: PredictiveAction,
    factor: GapFactor,
    posterior: f32,
    mask: u8,
) -> String {
    format!(
        "Shadow staged assessment: {} on {} (posterior {:.2}, sources: {}). {}.",
        action_label(action),
        factor_label(factor),
        posterior,
        source_names(mask).join("+"),
        factor_note(factor)
    )
}

fn guard_reason(
    phase: SessionPhase,
    pending: bool,
    cooldown: bool,
    previous_steers: u8,
) -> Option<String> {
    if !matches!(phase, SessionPhase::Idle | SessionPhase::Error) {
        Some("phase".to_string())
    } else if pending {
        Some("pending_question".to_string())
    } else if cooldown {
        Some("cooldown".to_string())
    } else if previous_steers >= 3 {
        Some("budget".to_string())
    } else {
        None
    }
}

struct StagedComputeInput<'a> {
    dossier_key: &'a str,
    steps: &'a [SemanticStep],
    goal: &'a str,
    goal_terms: &'a [String],
    core: &'a SessionAnalysis,
    phase: SessionPhase,
    pending: bool,
    semantic: &'a [SemanticFacts],
}

fn staged_compute(input: StagedComputeInput<'_>) -> Option<StagedComputed> {
    let StagedComputeInput {
        dossier_key,
        steps,
        goal,
        goal_terms,
        core,
        phase,
        pending,
        semantic,
    } = input;
    if steps.len() < 2 {
        return None;
    }
    let goal_keywords: BTreeSet<String> = goal_terms.iter().cloned().collect();
    let goal_literals = relay_compass::literals(goal);
    let goal_hash = private_digest(goal).ok()?;
    let observed_turns = core.observed_turns;
    let inherited = continuation_seeds(dossier_key);

    let prior = read_payload(&dossier_path(dossier_key));
    let prior_steers = prior
        .as_ref()
        .and_then(|p| p.get("previous_steers"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u8;
    let prior_sig = prior
        .as_ref()
        .and_then(|p| p.get("gap_signature"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let prior_sig_turn = prior
        .as_ref()
        .and_then(|p| p.get("gap_signature_turn"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let params = PredictiveParams::default();
    let ctx1 = StagedContext {
        contract_text: goal,
        goal_keywords: &goal_keywords,
        goal_literals: &goal_literals,
        observation: core.last.as_ref(),
        deviation_mean: core.deviation_mean,
        deviation_sigma: core.deviation_sigma,
        phase,
        pending_question: pending,
        previous_steers: prior_steers,
        same_signature_recent: false,
        continuation_seeds: &inherited,
    };
    let pass1 = assemble_and_evaluate(steps, semantic, &ctx1, &params)?;
    let obligation_id = pass1
        .ledger
        .focus_obligation()
        .map(|obligation| obligation.id.clone())
        .unwrap_or_default();
    let sig1 = gap_signature(
        pass1.decision.dominant_factor,
        pass1.dominant_source_mask,
        &goal_hash,
        &obligation_id,
    );
    let cooldown = is_cooldown(prior_sig.as_deref(), &sig1, observed_turns, prior_sig_turn);

    let ctx2 = StagedContext {
        contract_text: goal,
        goal_keywords: &goal_keywords,
        goal_literals: &goal_literals,
        observation: core.last.as_ref(),
        deviation_mean: core.deviation_mean,
        deviation_sigma: core.deviation_sigma,
        phase,
        pending_question: pending,
        previous_steers: prior_steers,
        same_signature_recent: cooldown,
        continuation_seeds: &inherited,
    };
    let out = assemble_and_evaluate(steps, semantic, &ctx2, &params)?;
    let action = out.decision.action;
    let dominant = out.decision.dominant_factor;

    let new_steers = next_previous_steers(prior_steers, out.pivot_confirmed, false);
    let sig_store = prior_sig.clone().unwrap_or_default();
    let turn_store = prior_sig_turn;

    let reason = guard_reason(phase, pending, cooldown, prior_steers);
    let target = out
        .ledger
        .focus_obligation()
        .map(|obligation| obligation.text.chars().take(240).collect())
        .unwrap_or_else(|| staged_target(&goal_literals, goal_terms));
    let obligation_id = out
        .ledger
        .focus_obligation()
        .map(|obligation| obligation.id.clone());
    let required_layer = out
        .ledger
        .focus_obligation()
        .map(|obligation| obligation.required_layer)
        .unwrap_or(ProofLayer::Unknown);
    let observed_layer = out
        .ledger
        .focus_obligation()
        .map(|obligation| obligation.observed_layer)
        .unwrap_or(ProofLayer::Unknown);
    let prompt_id = obligation_id.as_deref().unwrap_or("obl-unknown");
    let final_signature = gap_signature(
        dominant,
        out.dominant_source_mask,
        &goal_hash,
        obligation_id.as_deref().unwrap_or(""),
    );
    let completion = CompletionSnapshot::from_ledger(&out.ledger);

    let proof_request = (action == PredictiveAction::AskProof
        || (completion.active > 0 && completion.unresolved() > 0))
        .then(|| {
            health_prompt_text(
                dominant,
                prompt_id,
                &target,
                required_layer,
                observed_layer,
                true,
            )
        });
    let directive = match action {
        PredictiveAction::AskProof => proof_request.clone(),
        PredictiveAction::MicroSteer | PredictiveAction::FullSteer => Some(health_prompt_text(
            dominant,
            prompt_id,
            &target,
            required_layer,
            observed_layer,
            false,
        )),
        _ => None,
    };
    let explanation = if completion.is_verified_final() {
        format!(
            "Contract Ledger verified {}/{} active obligations with fresh sufficient evidence (coverage {:.3}).",
            completion.verified, completion.active, completion.coverage
        )
    } else {
        explanation_text(
            action,
            dominant,
            out.decision.gap_posterior,
            out.dominant_source_mask,
        )
    };

    let entry = json!({
        "at": now_secs(),
        "action": action_label(action),
        "factor": factor_label(dominant),
        "gap_posterior": out.decision.gap_posterior,
        "posterior": out.decision.posterior,
        "corroborating_sources": out.decision.corroborating_sources,
        "expected_losses": {
            "observe": out.decision.expected_losses.observe,
            "ask_proof": out.decision.expected_losses.ask_proof,
            "steer": out.decision.expected_losses.steer,
        },
        "proof_resolvability": out.proof_resolvability,
        "scope_breadth": out.scope_breadth,
        "guard_reason": reason,
        "obligation_id": obligation_id.clone(),
        "contract_coverage": out.ledger.signals.coverage,
    });
    let mut history = prior
        .as_ref()
        .and_then(|p| p.get("staged_history"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if action != PredictiveAction::Observe {
        history.push(entry);
        if history.len() > 32 {
            history.drain(..history.len() - 32);
        }
    }

    let block = json!({
        "staged": {
            "action": action_label(action),
            "factor": factor_label(dominant),
            "gap_posterior": out.decision.gap_posterior,
            "posterior": out.decision.posterior,
            "corroborating_sources": out.decision.corroborating_sources,
            "scope_breadth": out.scope_breadth,
            "proof_resolvability": out.proof_resolvability,
            "guard_reason": reason,
            "sources": source_names(out.dominant_source_mask),
            "obligation_id": obligation_id.clone(),
            "contract_coverage": out.ledger.signals.coverage,
            "health_shadow": {
                "advisory": true,
                "risk": out.health.risk,
                "action": format!("{:?}", out.health.action),
                "active_families": out.health.active_families,
                "corroborating_sources": out.health.corroborating_sources,
                "note": "noisy-OR R_t over the same ledger signals; uncalibrated, never gates; the predictive action above is authoritative",
            },
            "semantic": {
                "backend": out.semantic_backend,
                "model": out.semantic_model,
                "calibrated": out.semantic_calibrated,
            },
            "deterministic_feedback": out.deterministic_feedback,
            "completion": {
                "status": completion.status_label(),
                "active": completion.active,
                "open": completion.open,
                "claimed_unverified": completion.claimed_unverified,
                "verified": completion.verified,
                "contradicted": completion.contradicted,
                "disputed": completion.disputed,
                "stale": completion.stale,
                "layer_mismatches": completion.layer_mismatches,
                "coverage": completion.coverage,
                "completion_scope_gap": completion.completion_scope_gap,
                "proof_deficit": completion.proof_deficit,
            },
        },
        "observation_boundary": redacted_observation_boundary(steps).ok()?,
        "contract_ledger": redacted_ledger(&out.ledger),
        "previous_steers": new_steers,
        "gap_signature": sig_store,
        "gap_signature_turn": turn_store,
        "staged_history": history,
    });

    Some(StagedComputed {
        assessment: StagedAssessment {
            action,
            dominant_factor: dominant,
            gap_posterior: out.decision.gap_posterior,
            corroborating_sources: out.decision.corroborating_sources,
            proof_request,
            directive,
            gap_signature: final_signature,
            obligation_id,
            contract_coverage: out.ledger.signals.coverage,
            open_obligations: out.ledger.signals.open
                + out.ledger.signals.claimed_unverified
                + out.ledger.signals.contradicted,
            completion,
            explanation,
            semantic_calibrated: out.semantic_calibrated,
            semantic_backend: out.semantic_backend,
            semantic_model: out.semantic_model,
            deterministic_feedback: out.deterministic_feedback,
        },
        block,
    })
}

const TRANSCRIPT_HEAD_BYTES: u64 = 1024 * 1024;
const TRANSCRIPT_TAIL_BYTES: u64 = 8 * 1024 * 1024;

fn bounded_steps(path: &std::path::Path) -> Vec<relay_compass::SemanticStep> {
    relay_adapters::claude::semantic_steps_window(
        path,
        TRANSCRIPT_HEAD_BYTES,
        TRANSCRIPT_TAIL_BYTES,
    )
}

fn staged_of(session_id: &str, analysis: &Analysis) -> Option<StagedComputed> {
    let path = find_claude_jsonl(session_id)?;
    let steps = bounded_steps(&path);
    let state = relay_adapters::claude::read_state(&path, None).ok()?.state;
    let (phase, pending) = phase_of_state(&state);
    staged_compute(StagedComputeInput {
        dossier_key: session_id,
        steps: &steps,
        goal: &analysis.goal,
        goal_terms: &analysis.goal_terms,
        core: &analysis.core,
        phase,
        pending,
        semantic: &[],
    })
}

fn codex_phase(state: &relay_core::state::CodexState) -> (SessionPhase, bool) {
    use relay_core::state::CodexState;
    match state {
        CodexState::Idle | CodexState::NeedsReplyMaybe { .. } => (SessionPhase::Idle, false),
        CodexState::Error { .. } => (SessionPhase::Error, false),
        _ => (SessionPhase::Working, false),
    }
}

pub(crate) fn semantic_key(config: &relay_semantic::config::SemanticConfig) -> Option<String> {
    crate::supervisor::keys::key_for("semantic").or_else(|| {
        let endpoint = config.normalized_endpoint()?.to_ascii_lowercase();
        if endpoint.contains("openrouter.ai") {
            crate::supervisor::keys::key_for("openrouter")
        } else if endpoint.contains("api.nvidia.com") {
            std::env::var("NVIDIA_API_KEY").ok()
        } else {
            std::env::var("OPENAI_API_KEY").ok()
        }
    })
}

pub async fn assess_smart(
    session_id: &str,
    config: &relay_semantic::config::SemanticConfig,
) -> Result<Option<StagedAssessment>> {
    let analysis = analyze_session(session_id)?;
    let Some(path) = find_claude_jsonl(session_id) else {
        return Ok(None);
    };
    let steps = bounded_steps(&path);
    if steps.len() < 2 {
        return Ok(None);
    }
    let state = relay_adapters::claude::read_state(&path, None)?.state;
    let (phase, pending) = phase_of_state(&state);

    let deterministic_ledger =
        relay_compass::ledger::build_contract_ledger(&steps, &[], &analysis.goal);
    let local_feedback = deterministic_ledger.feedback.current_accepted;
    let terminal_contract = matches!(phase, SessionPhase::Idle | SessionPhase::Error)
        && deterministic_ledger.signals.active > 0;
    let facts = if local_feedback || terminal_contract {
        Vec::new()
    } else {
        let frames = semantic_inputs(&steps, &analysis.goal);
        relay_semantic::classify(config, frames, semantic_key(config)).await?
    };
    let Some(StagedComputed { assessment, block }) = staged_compute(StagedComputeInput {
        dossier_key: session_id,
        steps: &steps,
        goal: &analysis.goal,
        goal_terms: &analysis.goal_terms,
        core: &analysis.core,
        phase,
        pending,
        semantic: &facts,
    }) else {
        return Ok(None);
    };
    let alert_active = read_payload(&dossier_path(session_id))
        .and_then(|payload| payload.get("alert_active").and_then(Value::as_bool))
        .unwrap_or(false);
    persist_analysis(session_id, &analysis, None, alert_active, Some(block))?;
    Ok(Some(assessment))
}

pub async fn assess_codex_smart(
    thread_id: &str,
    rollout_path: &Path,
    state: &relay_core::state::CodexState,
    config: &relay_semantic::config::SemanticConfig,
) -> Result<Option<StagedAssessment>> {
    let steps = relay_adapters::codex::semantic_steps(rollout_path);
    let user_turns: Vec<String> = steps
        .iter()
        .filter(|step| step.role == StepRole::User)
        .map(|step| step.text.clone())
        .collect();
    if user_turns.is_empty() {
        return Ok(None);
    }
    let (information_profile, information_key) =
        observe_user_documents(&format!("codex:{thread_id}"), &user_turns)?;
    let Some(core) = analyze_turns_pinned_profiled(
        thread_id,
        &user_turns,
        &information_profile,
        &information_key,
    ) else {
        return Ok(None);
    };
    let goal = user_turns[core.goal_index].trim().to_string();
    let goal_terms: Vec<String> = information_profile
        .ranked_terms(&goal, &information_key)
        .into_iter()
        .take(32)
        .collect();
    let (phase, pending) = codex_phase(state);
    let deterministic_ledger = relay_compass::ledger::build_contract_ledger(&steps, &[], &goal);
    let local_feedback = deterministic_ledger.feedback.current_accepted;
    let terminal_contract = matches!(phase, SessionPhase::Idle | SessionPhase::Error)
        && deterministic_ledger.signals.active > 0;
    let facts = if local_feedback || terminal_contract {
        Vec::new()
    } else {
        relay_semantic::classify(config, semantic_inputs(&steps, &goal), semantic_key(config))
            .await?
    };
    let Some(StagedComputed { assessment, block }) = staged_compute(StagedComputeInput {
        dossier_key: thread_id,
        steps: &steps,
        goal: &goal,
        goal_terms: &goal_terms,
        core: &core,
        phase,
        pending,
        semantic: &facts,
    }) else {
        return Ok(None);
    };
    let base_risk = core.last.as_ref().map(|obs| obs.risk).unwrap_or(0.0);
    let analysis = Analysis {
        goal,
        core,
        problem_risk: 0.0,
        risk: base_risk,
        goal_terms,
        specifics: Vec::new(),
        information_profile,
        information_key,
    };
    let _ = persist_analysis(thread_id, &analysis, None, false, Some(block));
    Ok(Some(assessment))
}

#[cfg(test)]
fn has_current_deterministic_feedback(steps: &[SemanticStep], goal: &str) -> bool {
    relay_compass::ledger::build_contract_ledger(steps, &[], goal)
        .feedback
        .current_accepted
}

pub fn render_assessment(staged: &StagedAssessment) -> String {
    let mut lines = vec![
        format!("action: {}", action_label(staged.action)),
        format!("factor: {}", factor_label(staged.dominant_factor)),
        format!("gap_posterior: {:.3}", staged.gap_posterior),
        format!("corroborating_sources: {}", staged.corroborating_sources),
        format!("contract_coverage: {:.3}", staged.contract_coverage),
        format!("open_obligations: {}", staged.open_obligations),
        format!("completion_status: {}", staged.completion.status_label()),
        format!(
            "completion: verified={}/{} claimed_unverified={} stale={} layer_mismatches={}",
            staged.completion.verified,
            staged.completion.active,
            staged.completion.claimed_unverified,
            staged.completion.stale,
            staged.completion.layer_mismatches,
        ),
        format!("explanation: {}", staged.explanation),
    ];
    if let Some(obligation_id) = &staged.obligation_id {
        lines.push(format!("obligation_id: {obligation_id}"));
    }
    if let Some(directive) = &staged.directive {
        lines.push(format!("directive: {directive}"));
    }
    if let Some(request) = &staged.proof_request {
        lines.push(format!("proof_request: {request}"));
    }
    lines.push(format!(
        "semantic: backend={} model={} calibrated={}",
        staged.semantic_backend, staged.semantic_model, staged.semantic_calibrated
    ));
    lines.join("\n")
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn short_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().as_str()[..16].to_string()
}

const STAGED_KEYS: [&str; 8] = [
    "staged",
    "observation_boundary",
    "contract_ledger",
    "previous_steers",
    "gap_signature",
    "gap_signature_turn",
    "staged_history",
    "gate_pins",
];

pub fn record_gate_pin(
    session_id: &str,
    target: &str,
    obligation_id: &str,
    epoch: u32,
    controller: bool,
) -> Result<String> {
    if session_id.trim().is_empty() || target.trim().is_empty() || obligation_id.trim().is_empty() {
        bail!("session, target, and obligation are required");
    }
    let source = if controller { "controller" } else { "user" };
    let path = dossier_path(session_id);
    let mut payload = read_payload(&path).unwrap_or_else(|| json!({}));
    let object = payload
        .as_object_mut()
        .context("dossier payload is not an object")?;
    let pins = object.entry("gate_pins").or_insert_with(|| json!([]));
    if !pins.is_array() {
        *pins = json!([]);
    }
    let pins = pins.as_array_mut().expect("normalized gate pins");
    pins.retain(|pin| {
        !(pin.get("artifact_locator").and_then(Value::as_str) == Some(target)
            && pin.get("contract_epoch").and_then(Value::as_u64) == Some(epoch as u64))
    });
    pins.push(json!({
        "source": source,
        "artifact_locator": target,
        "obligation_id": obligation_id,
        "contract_epoch": epoch,
        "pinned_at": now_secs(),
    }));
    if pins.len() > 64 {
        let drop = pins.len() - 64;
        pins.drain(..drop);
    }
    write_payload(&path, &payload)?;
    Ok(format!(
        "pinned {obligation_id} to {target} at epoch {epoch} ({source})"
    ))
}

pub fn gate_pins(session_id: &str) -> Vec<Value> {
    read_payload(&dossier_path(session_id))
        .and_then(|payload| payload.get("gate_pins").cloned())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

pub fn clear_gate_pins(session_id: &str, target: Option<&str>) -> Result<String> {
    let path = dossier_path(session_id);
    let Some(mut payload) = read_payload(&path) else {
        return Ok("no dossier for session".to_string());
    };
    let Some(object) = payload.as_object_mut() else {
        return Ok("no gate pins".to_string());
    };
    let before = object
        .get("gate_pins")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    match target {
        Some(target) => {
            if let Some(pins) = object.get_mut("gate_pins").and_then(Value::as_array_mut) {
                pins.retain(|pin| {
                    pin.get("artifact_locator").and_then(Value::as_str) != Some(target)
                });
            }
        }
        None => {
            object.remove("gate_pins");
        }
    }
    let after = object
        .get("gate_pins")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    write_payload(&path, &payload)?;
    Ok(format!("cleared {} pin(s)", before.saturating_sub(after)))
}

pub fn list_gate_pins(session_id: &str) -> Result<String> {
    let pins = gate_pins(session_id);
    if pins.is_empty() {
        return Ok("no gate pins".to_string());
    }
    let mut out = String::new();
    for pin in &pins {
        let source = pin.get("source").and_then(Value::as_str).unwrap_or("?");
        let target = pin
            .get("artifact_locator")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let obligation = pin
            .get("obligation_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let epoch = pin
            .get("contract_epoch")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        out.push_str(&format!(
            "{source} epoch={epoch} {obligation} -> {target}\n"
        ));
    }
    Ok(out.trim_end().to_string())
}

fn usage_path(session_id: &str) -> PathBuf {
    root().join(format!("usage-{}.json", safe_id(session_id)))
}

pub fn record_usage(
    session_id: &str,
    backend: &str,
    model: &str,
    tokens: Option<u64>,
    est_usd: f64,
    latency_ms: u64,
    ok: bool,
) -> Result<()> {
    let path = usage_path(session_id);
    let mut payload = read_payload(&path).unwrap_or_else(|| json!({}));
    let object = payload
        .as_object_mut()
        .context("usage payload is not an object")?;
    let total = object
        .get("total_usd")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        + est_usd;
    object.insert("total_usd".to_string(), json!(total));
    let calls_total = object
        .get("total_calls")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;
    object.insert("total_calls".to_string(), json!(calls_total));
    let per = object
        .entry("per_provider_usd")
        .or_insert_with(|| json!({}));
    if let Some(map) = per.as_object_mut() {
        let prev = map.get(backend).and_then(Value::as_f64).unwrap_or(0.0);
        map.insert(backend.to_string(), json!(prev + est_usd));
    }
    let calls = object.entry("calls").or_insert_with(|| json!([]));
    if !calls.is_array() {
        *calls = json!([]);
    }
    let calls = calls.as_array_mut().expect("normalized usage calls");
    calls.push(json!({
        "at": now_secs(),
        "backend": backend,
        "model": model,
        "tokens": tokens,
        "est_usd": est_usd,
        "latency_ms": latency_ms,
        "ok": ok,
    }));
    if calls.len() > 256 {
        let drop = calls.len() - 256;
        calls.drain(..drop);
    }
    write_payload(&path, &payload)?;
    Ok(())
}

pub fn usage_totals(session_id: &str) -> (f64, BTreeMap<String, f64>, u64) {
    let Some(payload) = read_payload(&usage_path(session_id)) else {
        return (0.0, BTreeMap::new(), 0);
    };
    let total = payload
        .get("total_usd")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let calls = payload
        .get("total_calls")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut per_provider = BTreeMap::new();
    if let Some(map) = payload.get("per_provider_usd").and_then(Value::as_object) {
        for (backend, value) in map {
            per_provider.insert(backend.clone(), value.as_f64().unwrap_or(0.0));
        }
    }
    (total, per_provider, calls)
}

pub fn usage_report(session_id: &str) -> Result<String> {
    let (total, per_provider, calls) = usage_totals(session_id);
    if calls == 0 {
        return Ok("no supervisor provider calls recorded".to_string());
    }
    let mut out = format!("calls={calls} est_usd={total:.4}\n");
    for (backend, usd) in &per_provider {
        out.push_str(&format!("  {backend}: est_usd={usd:.4}\n"));
    }
    Ok(out.trim_end().to_string())
}

fn analysis_payload(
    session_id: &str,
    analysis: &Analysis,
    decisions: Vec<Value>,
    alert_active: bool,
    staged: Option<&Value>,
) -> Result<Value> {
    let state_signature = vector_hex(&analysis.core.state).context("unsupported state vector")?;
    let observation = analysis.core.last.as_ref();
    let mut payload = json!({
        "session_id": session_id,
        "updated_at": now_secs(),
        "goal_digest": private_digest(&analysis.goal)?,
        "goal_term_count": analysis.goal_terms.len(),
        "information_profile_documents": analysis.information_profile.documents(),
        "information_profile_bytes": analysis.information_profile.memory_bytes(),
        "specific_count": analysis.specifics.len(),
        "observed_turns": analysis.core.observed_turns,
        "deviation_mean": analysis.core.deviation_mean,
        "deviation_sigma": analysis.core.deviation_sigma,
        "deviation_z": observation.map(|value| value.deviation_z),
        "state_distance": observation.map(|value| value.state_distance),
        "coherence": observation.map(|value| value.coherence),
        "progress": observation.map(|value| value.progress),
        "cusum": observation.map(|value| value.cusum),
        "stuck": observation.map(|value| value.stuck).unwrap_or(false),
        "base_risk": observation.map(|value| value.risk).unwrap_or(0.0),
        "problem_risk": analysis.problem_risk,
        "risk": analysis.risk,
        "state_signature": state_signature,
        "alert_active": alert_active,
        "decisions": decisions,
    });
    if let (Some(staged), Some(object)) = (staged, payload.as_object_mut()) {
        for key in STAGED_KEYS {
            if let Some(value) = staged.get(key) {
                object.insert(key.to_string(), value.clone());
            }
        }
    }
    Ok(payload)
}

fn carry_staged(previous: Option<&Value>) -> Option<Value> {
    let previous = previous?;
    let mut carried = serde_json::Map::new();
    for key in STAGED_KEYS {
        if let Some(value) = previous.get(key) {
            carried.insert(key.to_string(), value.clone());
        }
    }
    (!carried.is_empty()).then_some(Value::Object(carried))
}

fn persist_analysis(
    session_id: &str,
    analysis: &Analysis,
    decision: Option<Value>,
    alert_active: bool,
    staged: Option<Value>,
) -> Result<()> {
    let path = dossier_path(session_id);
    let previous = read_payload(&path);
    let mut decisions = previous
        .as_ref()
        .and_then(|payload| payload.get("decisions"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(decision) = decision {
        decisions.push(decision);
        if decisions.len() > 32 {
            decisions.drain(..decisions.len() - 32);
        }
    }
    let staged = staged.or_else(|| carry_staged(previous.as_ref()));
    let payload = analysis_payload(
        session_id,
        analysis,
        decisions,
        alert_active,
        staged.as_ref(),
    )?;
    let unchanged = previous.as_ref().is_some_and(|old| {
        old.get("goal_digest") == payload.get("goal_digest")
            && old.get("observed_turns") == payload.get("observed_turns")
            && old.get("state_signature") == payload.get("state_signature")
            && old.get("risk") == payload.get("risk")
            && old.get("alert_active") == payload.get("alert_active")
            && old.get("decisions") == payload.get("decisions")
            && old.get("staged") == payload.get("staged")
            && old.get("observation_boundary") == payload.get("observation_boundary")
            && old.get("contract_ledger") == payload.get("contract_ledger")
    });
    if !unchanged {
        write_payload(&path, &payload)?;
    }
    Ok(())
}

pub fn guard_rewrite(session_id: &str, original: &str, candidate: &str) -> GuardResult {
    let Ok(analysis) = analyze_session(session_id) else {
        return GuardResult {
            selected: candidate.to_string(),
            accepted: true,
            reason: "no dossier".to_string(),
            risk: 0.0,
        };
    };
    let goal = relay_compass::embed(&analysis.goal);
    let (selected, verdict) = relay_compass::choose_rewrite_profiled(
        true,
        goal.as_ref(),
        original,
        candidate,
        &relay_compass::DriftParams::default(),
        analysis.risk,
        (&analysis.information_profile, &analysis.information_key),
    );
    let accepted = selected.as_bytes() == candidate.as_bytes();
    let reason = verdict
        .as_ref()
        .map(|value| value.reason.to_string())
        .unwrap_or_else(|| "no goal".to_string());
    let prior_active = read_payload(&dossier_path(session_id))
        .and_then(|payload| payload.get("alert_active").and_then(Value::as_bool))
        .unwrap_or(false);
    let decision = json!({
        "at": now_secs(),
        "kind": "rewrite",
        "accepted": accepted,
        "reason": reason,
        "risk": analysis.risk,
        "original_hash": short_hash(original),
        "candidate_hash": short_hash(candidate),
    });
    let _ = persist_analysis(session_id, &analysis, Some(decision), prior_active, None);
    GuardResult {
        selected: selected.to_string(),
        accepted,
        reason,
        risk: analysis.risk,
    }
}

pub fn observe_session(session_id: &str) -> Option<Notice> {
    let analysis = analyze_session(session_id).ok()?;
    let previous_active = read_payload(&dossier_path(session_id))
        .and_then(|payload| payload.get("alert_active").and_then(Value::as_bool))
        .unwrap_or(false);
    let stuck = analysis
        .core
        .last
        .as_ref()
        .map(|value| value.stuck)
        .unwrap_or(false);
    let active = if previous_active {
        stuck || analysis.risk >= 0.55
    } else {
        stuck || analysis.risk >= 0.75
    };
    let computed = staged_of(session_id, &analysis);
    let block = computed.as_ref().map(|c| c.block.clone());
    persist_analysis(session_id, &analysis, None, active, block).ok()?;
    let risk_alert = !previous_active && active;
    let staged = computed
        .map(|c| c.assessment)
        .filter(|a| a.action != PredictiveAction::Observe);
    (risk_alert || staged.is_some()).then_some(Notice {
        risk: analysis.risk,
        stuck,
        risk_alert,
        staged,
    })
}

pub fn inspect(session_id: &str) -> Result<String> {
    let analysis = analyze_session(session_id)?;
    let path = dossier_path(session_id);
    let active = read_payload(&path)
        .and_then(|payload| payload.get("alert_active").and_then(Value::as_bool))
        .unwrap_or(false);
    let block = staged_of(session_id, &analysis).map(|c| c.block);
    persist_analysis(session_id, &analysis, None, active, block)?;
    let payload = read_payload(&path).context("dossier write failed")?;
    Ok(serde_json::to_string_pretty(&payload)?)
}

pub fn mark_problem(session_id: &str, label: &str) -> Result<String> {
    if !matches!(label, "bad" | "good") {
        bail!("usage: automation compass-mark <session_id> <bad|good>");
    }
    let analysis = analyze_session(session_id)?;
    let state_votes = vector_votes(&analysis.core.state).context("unsupported state vector")?;
    let mut store = load_prototypes();
    let nearest = store
        .prototypes
        .iter()
        .enumerate()
        .filter_map(|(index, prototype)| {
            let centroid = votes_vector(&prototype.votes)?;
            let distance = relay_compass::distance(&analysis.core.state, &centroid)?;
            Some((index, distance))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1));
    let index = match nearest.filter(|(_, distance)| *distance <= PROBLEM_RADIUS) {
        Some((index, _)) => index,
        None => {
            let state_hex = vector_hex(&analysis.core.state).context("unsupported state vector")?;
            store.prototypes.push(Prototype {
                id: short_hash(&state_hex),
                bad: 0,
                good: 0,
                votes: vec![0; relay_compass::SIM_BITS],
                updated_at: now_secs(),
            });
            store.prototypes.len() - 1
        }
    };
    let prototype = &mut store.prototypes[index];
    if label == "bad" {
        prototype.bad += 1;
        for (vote, sample) in prototype.votes.iter_mut().zip(state_votes) {
            *vote += sample;
        }
    } else {
        prototype.good += 1;
    }
    prototype.updated_at = now_secs();
    let id = prototype.id.clone();
    let bad = prototype.bad;
    let good = prototype.good;
    store
        .prototypes
        .sort_by_key(|prototype| std::cmp::Reverse(prototype.updated_at));
    store.prototypes.truncate(32);
    save_prototypes(&store)?;
    Ok(format!("prototype {id}: bad={bad} good={good}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_checksum_detects_mutation() {
        let payload = json!({"turns": 12, "risk": 0.4});
        let checksum = payload_checksum(&payload).unwrap();
        let changed = json!({"turns": 13, "risk": 0.4});
        assert_ne!(checksum, payload_checksum(&changed).unwrap());
    }

    #[test]
    fn information_profile_is_bound_to_the_private_key() {
        let key = [3; 32];
        let other = [4; 32];
        let mut profile = InformationProfile::default();
        profile.observe_document("s:0", "verify parser.rs", &key);
        let bytes = bound_information_profile(&profile, &key);
        let (binding, payload) = bytes.split_at(16);
        assert_eq!(binding, information_profile_binding(&key));
        assert_ne!(binding, information_profile_binding(&other));
        assert_eq!(InformationProfile::decode(payload), Some(profile));
    }

    #[test]
    fn votes_roundtrip_to_signature() {
        let vector = relay_compass::embed("build the tested flowchart parser").unwrap();
        let votes = vector_votes(&vector).unwrap();
        assert_eq!(votes_vector(&votes), Some(vector));
    }

    #[test]
    fn private_obligation_signature_roundtrips_for_local_matching() {
        let text = "verify parser.rs in the live runtime";
        let signature = private_vector_signature(text).unwrap();
        assert_eq!(
            private_signature_vector(&signature),
            relay_compass::embed(text)
        );
        assert!(!signature.contains("parser"));
    }

    #[test]
    fn dossier_digest_pins_contract_across_a_longer_correction() {
        let session_id = format!("scope-pin-{}-{}", std::process::id(), now_secs());
        let path = dossier_path(&session_id);
        let contract = "Audit each report column against source XML and real filled reports, remove hard-coded thresholds, and verify the tonnage workflow";
        let turns = vec![
            contract.to_string(),
            "continue".to_string(),
            "You missed the main requirement: search all mounted storage, compare every source field with every output column, recover all formulas, move all thresholds into managed settings, and test every existing data-entry tool against real reports".to_string(),
        ];
        write_payload(
            &path,
            &json!({"goal_digest": private_digest(contract).unwrap()}),
        )
        .unwrap();

        assert_eq!(relay_compass::select_goal_index(&turns), Some(2));
        assert_eq!(select_pinned_goal_index(&session_id, &turns), Some(0));
        assert_eq!(
            analyze_turns_pinned(&session_id, &turns)
                .unwrap()
                .goal_index,
            0
        );

        write_payload(
            &path,
            &json!({"goal_digest": private_digest("a different unavailable contract").unwrap()}),
        )
        .unwrap();
        assert_eq!(select_pinned_goal_index(&session_id, &turns), None);
        assert!(analyze_turns_pinned(&session_id, &turns).is_none());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(backup_path(&path));
    }

    #[test]
    fn safe_session_id_cannot_escape_store() {
        assert_eq!(safe_id("../bad/session"), ".._bad_session");
    }

    #[test]
    fn usage_ledger_accumulates_totals_across_capped_history() {
        let sid = format!("usage-{}-{}", std::process::id(), now_secs());
        let path = usage_path(&sid);
        record_usage(&sid, "openrouter", "x/y", Some(1000), 0.02, 12, true).unwrap();
        record_usage(&sid, "openrouter", "x/y", Some(500), 0.01, 8, true).unwrap();
        record_usage(&sid, "ollama", "llama", None, 0.0, 5, true).unwrap();

        let (total, per_provider, calls) = usage_totals(&sid);
        assert_eq!(calls, 3);
        assert!((total - 0.03).abs() < 1e-9);
        assert!((per_provider.get("openrouter").copied().unwrap_or(0.0) - 0.03).abs() < 1e-9);
        assert_eq!(per_provider.get("ollama").copied(), Some(0.0));
        assert!(usage_report(&sid).unwrap().contains("calls=3"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(backup_path(&path));
    }

    #[test]
    fn gate_pin_round_trips_and_survives_a_persist_cycle() {
        let session_id = format!("gate-pin-{}-{}", std::process::id(), now_secs());
        let path = dossier_path(&session_id);
        record_gate_pin(&session_id, "src/lib.rs", "obl-2", 4, false).unwrap();

        let pins = gate_pins(&session_id);
        assert_eq!(pins.len(), 1);
        assert_eq!(
            pins[0].get("obligation_id").and_then(Value::as_str),
            Some("obl-2")
        );
        assert_eq!(pins[0].get("source").and_then(Value::as_str), Some("user"));

        let carried = carry_staged(read_payload(&path).as_ref()).unwrap();
        assert_eq!(
            carried
                .get("gate_pins")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        record_gate_pin(&session_id, "src/lib.rs", "obl-3", 4, true).unwrap();
        assert_eq!(gate_pins(&session_id).len(), 1);
        assert_eq!(
            gate_pins(&session_id)[0]
                .get("obligation_id")
                .and_then(Value::as_str),
            Some("obl-3")
        );

        assert_eq!(
            clear_gate_pins(&session_id, None).unwrap(),
            "cleared 1 pin(s)"
        );
        assert!(gate_pins(&session_id).is_empty());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(backup_path(&path));
    }

    #[test]
    fn corrupt_current_payload_recovers_from_backup() {
        let dir = std::env::temp_dir().join(format!(
            "vsc-relay-compass-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let path = dir.join("dossier.json");
        let first = json!({"turns": 1});
        let second = json!({"turns": 2});
        write_payload(&path, &first).unwrap();
        write_payload(&path, &second).unwrap();
        std::fs::write(&path, b"corrupt").unwrap();
        assert_eq!(read_payload(&path), Some(first));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_dossier_migration_removes_raw_goal_from_main_and_backup() {
        let dir = std::env::temp_dir().join(format!(
            "vsc-relay-compass-legacy-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dossier.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "schema": 1,
                "payload": {
                    "goal": "private raw goal",
                    "goal_terms": ["private", "goal"],
                    "specifics": ["secret.txt"],
                    "risk": 0.4
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(migrate_dossier_file(&path).unwrap());
        let payload = read_payload(&path).unwrap();
        assert!(payload.get("goal").is_none());
        assert!(payload.get("goal_terms").is_none());
        assert!(payload.get("specifics").is_none());
        assert_eq!(payload.get("goal_term_count"), Some(&json!(2)));
        assert_eq!(payload.get("specific_count"), Some(&json!(1)));
        for stored in [&path, &backup_path(&path)] {
            let text = std::fs::read_to_string(stored).unwrap();
            assert!(!text.contains("private raw goal"));
            assert!(!text.contains("secret.txt"));
            assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["schema"], 2);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gap_signature_is_deterministic_and_factor_sensitive() {
        let a = gap_signature(
            GapFactor::LayerMismatch,
            SOURCE_CONTRACT | SOURCE_AGENT,
            "goalx",
            "obl-0-1",
        );
        let b = gap_signature(
            GapFactor::LayerMismatch,
            SOURCE_CONTRACT | SOURCE_AGENT,
            "goalx",
            "obl-0-1",
        );
        let c = gap_signature(
            GapFactor::ProxyCapture,
            SOURCE_CONTRACT | SOURCE_AGENT,
            "goalx",
            "obl-0-1",
        );
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn persisted_contract_ledger_contains_no_raw_obligation_text() {
        let goal = "Implement private_feature.rs and verify secret acceptance output";
        let steps = vec![
            SemanticStep::new(0, StepRole::User, goal.to_string()),
            SemanticStep::new(1, StepRole::Assistant, "working".to_string()),
        ];
        let ledger = relay_compass::ledger::build_contract_ledger(&steps, &[], goal);
        let stored = serde_json::to_string(&redacted_ledger(&ledger)).unwrap();
        assert!(!stored.contains("private_feature"));
        assert!(!stored.contains("secret acceptance"));
        assert!(stored.contains("obl-0-1"));
        assert!(stored.contains("\"clause_boundaries_preserved\":true"));
        assert!(stored.contains("\"source_conserved\":true"));
    }

    #[test]
    fn persisted_observation_boundary_preserves_roles_without_raw_chat() {
        let mut command = SemanticStep::new(
            2,
            StepRole::ToolUse,
            "execute private acceptance check".to_string(),
        );
        command.tool_kind = relay_compass::ToolKind::Execute;
        let mut result = SemanticStep::new(
            3,
            StepRole::ToolResult,
            "private acceptance output".to_string(),
        );
        result.tool_kind = relay_compass::ToolKind::Execute;
        let steps = vec![
            SemanticStep::new(0, StepRole::User, "private requirement".to_string()),
            SemanticStep::new(1, StepRole::Assistant, "private claim".to_string()),
            command,
            result,
            SemanticStep::new(
                4,
                StepRole::DelegateResult,
                "private delegate report".to_string(),
            ),
        ];
        let boundary = redacted_observation_boundary(&steps).unwrap();
        assert_eq!(boundary["contract_inputs"], 1);
        assert_eq!(boundary["agent_claims"], 1);
        assert_eq!(boundary["command_requests"], 1);
        assert_eq!(boundary["runtime_results"], 1);
        assert_eq!(boundary["delegate_claims"], 1);
        assert_eq!(boundary["evidence_candidates"], 1);
        let stored = serde_json::to_string(&boundary).unwrap();
        assert!(!stored.contains("private requirement"));
        assert!(!stored.contains("private claim"));
        assert!(!stored.contains("private acceptance"));
        assert!(!stored.contains("private delegate"));
    }

    #[test]
    fn persisted_topic_edges_contain_no_raw_certificate_quotes() {
        let goal = "Verify private parser acceptance behavior";
        let steps = vec![
            SemanticStep::new(0, StepRole::User, goal.to_string()),
            SemanticStep::new(
                1,
                StepRole::Assistant,
                "private parser is definitely correct".to_string(),
            ),
            SemanticStep::new(
                2,
                StepRole::Assistant,
                "private parser is definitely broken".to_string(),
            ),
            SemanticStep::new(
                3,
                StepRole::Assistant,
                "[VSC_RELAY_HEALTH_RESULT v1]\n\
                 {\"protocol\":\"vsc-relay.health-result.v4\",\
                 \"steer_id\":\"current-contract\",\"status\":\"working\",\
                 \"contract_relation\":\"same\",\"state_conflict\":\"unresolved\",\
                 \"state_conflicts\":[{\
                   \"obligation_quotes\":[\"Verify private parser acceptance behavior\"],\
                   \"relation\":\"same_artifact\",\"version\":\"current\",\
                   \"left\":{\"source\":\"assistant\",\
                     \"quote\":\"private parser is definitely correct\",\"stance\":\"supports\"},\
                   \"right\":{\"source\":\"assistant\",\
                     \"quote\":\"private parser is definitely broken\",\"stance\":\"contradicts\"}\
                 }],\"conflict_tool_call_ids\":[],\
                 \"evidence_tool_call_ids\":[],\"remaining_risk\":true}\n\
                 [/VSC_RELAY_HEALTH_RESULT]"
                    .to_string(),
            ),
        ];
        let ledger = relay_compass::ledger::build_contract_ledger(&steps, &[], goal);
        assert_eq!(ledger.topic_edges.len(), 1);

        let stored = serde_json::to_string(&redacted_ledger(&ledger)).unwrap();
        assert!(stored.contains("\"topic_edges\""));
        assert!(!stored.contains("private parser"));
        assert!(!stored.contains("acceptance behavior"));
    }

    #[test]
    fn health_prompt_is_obligation_specific_and_bounded_to_three_steps() {
        let prompt = health_prompt_text(
            GapFactor::LayerMismatch,
            "obl-2-7",
            "live parser acceptance",
            ProofLayer::Live,
            ProofLayer::Unit,
            false,
        );
        assert!(prompt.starts_with("[VSC_RELAY_HEALTH_STEER v2 id=obl-2-7]"));
        assert!(prompt.contains("Required layer: live; observed layer: unit"));
        assert!(prompt.contains("1. "));
        assert!(prompt.contains("2. "));
        assert!(prompt.contains("3. "));
        assert!(!prompt.contains("4. "));
        assert!(prompt.contains("[VSC_RELAY_HEALTH_RESULT v1]"));
        assert!(prompt.contains("vsc-relay.health-result.v4"));
        assert!(prompt.contains("\"state_conflicts\":[]"));
        assert!(prompt.contains("\"contract_relation\":\"same\""));
        assert!(prompt.contains("\"state_conflict\":\"none\""));
        assert!(prompt.contains("\"steer_id\":\"obl-2-7\""));
        assert!(prompt.contains("the local controller owns native ids"));
        assert!(prompt.contains("\"evidence_tool_call_ids\":[]"));
        assert!(prompt.ends_with("required layer."));
    }

    #[test]
    fn idle_open_ledger_always_exports_a_bounded_proof_request() {
        let dossier_key = format!("completion-gap-{}-{}", std::process::id(), now_secs());
        let goal = "Implement and verify the flowchart parser end to end";
        let turns = vec![goal.to_string(), "Done, everything looks good".to_string()];
        let core = relay_compass::analyze_turns_from_goal_index(&turns, 0).unwrap();
        let steps = vec![
            SemanticStep::new(0, StepRole::User, turns[0].clone()),
            SemanticStep::new(1, StepRole::Assistant, turns[1].clone()),
        ];
        let terms = relay_compass::literals(goal)
            .into_iter()
            .collect::<Vec<_>>();

        let computed = staged_compute(StagedComputeInput {
            dossier_key: &dossier_key,
            steps: &steps,
            goal,
            goal_terms: &terms,
            core: &core,
            phase: SessionPhase::Idle,
            pending: false,
            semantic: &[],
        })
        .unwrap();

        assert_eq!(computed.assessment.completion.active, 1);
        assert_eq!(computed.assessment.completion.verified, 0);
        assert!(computed.assessment.completion.unresolved() > 0);
        let proof = computed.assessment.proof_request.unwrap();
        assert!(proof.contains("maximum three steps"));
        assert!(proof.contains("Do not claim completion"));
        assert!(proof.contains("the local controller owns native ids"));
        assert!(proof.contains("\"evidence_tool_call_ids\":[]"));

        let path = dossier_path(&dossier_key);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(backup_path(&path));
    }

    #[test]
    fn session_feedback_bypasses_semantic_provider_on_the_first_turn() {
        let goal = "Implement and verify parser.rs end to end";
        let steps = vec![
            SemanticStep::new(0, StepRole::User, goal.to_string()),
            SemanticStep::new(
                1,
                StepRole::Assistant,
                "[VSC_RELAY_HEALTH_RESULT v1]\n\
                 {\"protocol\":\"vsc-relay.health-result.v1\",\"steer_id\":\"current-contract\",\
                 \"status\":\"working\",\"evidence_tool_call_ids\":[],\
                 \"remaining_risk\":true}\n[/VSC_RELAY_HEALTH_RESULT]"
                    .to_string(),
            ),
        ];
        assert!(has_current_deterministic_feedback(&steps, goal));

        let v2 = vec![
            SemanticStep::new(0, StepRole::User, goal.to_string()),
            SemanticStep::new(
                1,
                StepRole::Assistant,
                "[VSC_RELAY_HEALTH_RESULT v1]\n\
                 {\"protocol\":\"vsc-relay.health-result.v2\",\"steer_id\":\"current-contract\",\
                 \"status\":\"working\",\"contract_relation\":\"expands\",\
                 \"evidence_tool_call_ids\":[],\"remaining_risk\":true}\n\
                 [/VSC_RELAY_HEALTH_RESULT]"
                    .to_string(),
            ),
        ];
        assert!(has_current_deterministic_feedback(&v2, goal));

        let plain = vec![
            SemanticStep::new(0, StepRole::User, goal.to_string()),
            SemanticStep::new(1, StepRole::Assistant, "still working".to_string()),
        ];
        assert!(!has_current_deterministic_feedback(&plain, goal));
    }

    #[test]
    fn cooldown_requires_matching_signature_in_window() {
        assert!(is_cooldown(Some("sig"), "sig", 10, 8));
        assert!(!is_cooldown(Some("sig"), "sig", 20, 8));
        assert!(!is_cooldown(Some("other"), "sig", 10, 8));
        assert!(!is_cooldown(None, "sig", 10, 8));
    }

    #[test]
    fn previous_steers_accumulate_and_reset_on_pivot() {
        assert_eq!(next_previous_steers(0, false, true), 1);
        assert_eq!(next_previous_steers(2, false, true), 3);
        assert_eq!(next_previous_steers(2, false, false), 2);
        assert_eq!(next_previous_steers(3, true, true), 0);
    }

    #[test]
    fn phase_mapping_from_transcript() {
        let idle = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"#;
        assert_eq!(phase_of(idle).0, SessionPhase::Idle);
        let error = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"content":[{"type":"text","text":"API Error: Connection closed mid-response."}]}}"#;
        assert_eq!(phase_of(error).0, SessionPhase::Error);
        let question = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"q1","name":"AskUserQuestion","input":{"questions":[{"header":"S","question":"q?","multiSelect":false,"options":[{"label":"a","description":"x"},{"label":"b","description":"y"}]}]}}]}}"#;
        let (phase, pending) = phase_of(question);
        assert_eq!(phase, SessionPhase::AwaitingUser);
        assert!(pending);
    }

    #[test]
    fn guard_reason_precedence() {
        assert_eq!(
            guard_reason(SessionPhase::Working, false, false, 0).as_deref(),
            Some("phase")
        );
        assert_eq!(
            guard_reason(SessionPhase::Idle, true, false, 0).as_deref(),
            Some("pending_question")
        );
        assert_eq!(
            guard_reason(SessionPhase::Idle, false, true, 0).as_deref(),
            Some("cooldown")
        );
        assert_eq!(
            guard_reason(SessionPhase::Idle, false, false, 3).as_deref(),
            Some("budget")
        );
        assert_eq!(guard_reason(SessionPhase::Idle, false, false, 0), None);
    }

    #[test]
    fn codex_phase_mapping() {
        use relay_core::state::CodexState;
        assert_eq!(codex_phase(&CodexState::Idle).0, SessionPhase::Idle);
        assert_eq!(
            codex_phase(&CodexState::NeedsReplyMaybe {
                last_msg: "x".into()
            })
            .0,
            SessionPhase::Idle
        );
        assert_eq!(
            codex_phase(&CodexState::Error {
                message: "x".into()
            })
            .0,
            SessionPhase::Error
        );
        assert_eq!(
            codex_phase(&CodexState::Working {
                turn_id: "t".into()
            })
            .0,
            SessionPhase::Working
        );
    }

    #[test]
    fn carry_staged_preserves_staged_keys() {
        let previous = json!({
            "risk": 0.4,
            "staged": {"action": "micro_steer"},
            "previous_steers": 2,
            "gap_signature": "abc",
            "gap_signature_turn": 5,
            "staged_history": [{"action": "micro_steer"}],
        });
        let carried = carry_staged(Some(&previous)).unwrap();
        assert_eq!(
            carried.get("previous_steers").and_then(Value::as_u64),
            Some(2)
        );
        assert_eq!(
            carried.get("gap_signature").and_then(Value::as_str),
            Some("abc")
        );
        assert!(carried.get("risk").is_none());
        assert!(carry_staged(Some(&json!({"risk": 0.1}))).is_none());
    }
}
