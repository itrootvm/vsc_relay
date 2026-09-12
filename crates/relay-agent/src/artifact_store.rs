use anyhow::{Context, Result};
use relay_compass::{
    ArtifactState, BehaviorState, CoverageStatus, ExistenceState, GateDecision, GateState,
    PopulationState, ToolEffect, WiringState,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const STORE_SCHEMA: u32 = 1;
const ADAPTER_VERSION: &str = "artifact-fs-v1";

#[cfg(test)]
pub(crate) static ARTIFACT_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactRecord {
    locator_digest: String,
    locator_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    coverage_root_hash: Option<String>,
    state: ArtifactState,
    #[serde(default)]
    dimension_versions: DimensionVersions,
    adapter_version: String,
    observed_at: i64,
    evidence_ref: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DimensionVersions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    existence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    population: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wiring: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    behavior: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdapterGap {
    shape_digest: String,
    first_seen_at: i64,
    last_seen_at: i64,
    count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateRecord {
    pub session_digest: String,
    pub action_digest: String,
    pub locator_digest: Option<String>,
    pub state: GateState,
    pub decision: GateDecision,
    pub attempts: u8,
    pub probe_steps: u8,
    #[serde(default)]
    pub clearance_consumed: bool,
    #[serde(default)]
    probe_digests: Vec<String>,
    #[serde(default)]
    native_identity_digests: Vec<String>,
    #[serde(default)]
    controller_notified: bool,
    pub obligation_ids: Vec<String>,
    pub opened_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ArtifactStore {
    #[serde(default)]
    records: Vec<ArtifactRecord>,
    #[serde(default)]
    adapter_gaps: Vec<AdapterGap>,
    #[serde(default)]
    gates: Vec<GateRecord>,
}

#[derive(Debug, Clone)]
pub struct TargetSnapshot {
    pub locator_digest: String,
    pub version_hash: Option<String>,
    pub coverage: CoverageStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerProbeOutcome {
    Present,
    Absent,
    Unavailable,
}

fn digest_tag(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

fn trace_gate(stage: &'static str, gate: &GateRecord) {
    tracing::info!(
        target: "relay::gate",
        pipeline = "gate",
        stage,
        session = %digest_tag(&gate.session_digest),
        action = %digest_tag(&gate.action_digest),
        locator = %gate.locator_digest.as_deref().map(digest_tag).unwrap_or("unbound"),
        state = ?gate.state,
        decision = ?gate.decision,
        attempts = gate.attempts,
        probe_steps = gate.probe_steps,
        clearance_consumed = gate.clearance_consumed,
        obligations = gate.obligation_ids.len(),
        "[gate] transition"
    );
}

fn root() -> PathBuf {
    if let Some(path) = std::env::var_os("VSC_RELAY_ARTIFACT_ROOT") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("artifacts")
}

fn key_path() -> PathBuf {
    root().join(".key")
}

fn digest_key() -> Result<[u8; 32]> {
    if let Ok(bytes) = std::fs::read(key_path()) {
        if let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Ok(key);
        }
    }
    crate::fsutil::secure_dir(&root());
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key)
        .map_err(|error| anyhow::anyhow!("generate artifact key: {error}"))?;
    crate::fsutil::secure_write(&key_path(), &key)?;
    Ok(key)
}

fn private_digest(domain: &str, value: &[u8]) -> Result<String> {
    let key = digest_key()?;
    let mut hasher = blake3::Hasher::new_keyed(&key);
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(value);
    Ok(hasher.finalize().to_hex()[..32].to_string())
}

fn canonical_repo(cwd: &Path) -> Result<PathBuf> {
    let canonical = cwd
        .canonicalize()
        .with_context(|| format!("canonicalize {}", cwd.display()))?;
    let mut cursor = canonical.as_path();
    loop {
        if cursor.join(".git").exists() {
            return Ok(cursor.to_path_buf());
        }
        let Some(parent) = cursor.parent() else {
            return Ok(canonical);
        };
        cursor = parent;
    }
}

fn repo_digest(repo: &Path) -> Result<String> {
    private_digest("repo-v1", repo.to_string_lossy().as_bytes())
}

fn store_path(repo_digest: &str) -> PathBuf {
    root().join(format!("{repo_digest}.json"))
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

#[cfg(unix)]
fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("json.lock")
}

struct StoreLock {
    #[cfg(unix)]
    file: std::fs::File,
}

impl StoreLock {
    fn acquire(path: &Path) -> Result<Self> {
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
                .open(lock_path(path))?;

            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self { file })
        }
        #[cfg(windows)]
        {
            let _ = path;
            Ok(Self {})
        }
    }
}

#[cfg(unix)]
impl Drop for StoreLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn checksum(payload: &Value) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(payload)?)
        .to_hex()
        .to_string())
}

fn decode(path: &Path) -> Option<ArtifactStore> {
    let envelope: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if envelope.get("schema")?.as_u64()? != STORE_SCHEMA as u64 {
        return None;
    }
    let payload = envelope.get("payload")?.clone();
    if checksum(&payload).ok()?.as_str() != envelope.get("checksum")?.as_str()? {
        return None;
    }
    serde_json::from_value(payload).ok()
}

fn load(path: &Path) -> ArtifactStore {
    decode(path)
        .or_else(|| decode(&backup_path(path)))
        .unwrap_or_default()
}

fn save(path: &Path, store: &ArtifactStore) -> Result<()> {
    let payload = serde_json::to_value(store)?;
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "schema": STORE_SCHEMA,
        "checksum": checksum(&payload)?,
        "payload": payload,
    }))?;
    let backup = if decode(path).is_some() {
        std::fs::read(path).unwrap_or_else(|_| bytes.clone())
    } else {
        bytes.clone()
    };
    crate::fsutil::secure_write(&backup_path(path), &backup)?;
    crate::fsutil::secure_write(path, &bytes)?;
    Ok(())
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    let mut cursor = Some(path);
    while let Some(candidate) = cursor {
        if candidate.exists() {
            return Some(candidate);
        }
        cursor = candidate.parent();
    }
    None
}

fn normalize_relative(repo: &Path, cwd: &Path, target: &str) -> Option<(PathBuf, String)> {
    if target.trim().is_empty() || target.contains('\n') || target.contains('\0') {
        return None;
    }
    let raw = Path::new(target.trim());
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };

    let ancestor = nearest_existing_ancestor(&joined)?;
    let canonical_ancestor = ancestor.canonicalize().ok()?;
    let suffix = joined.strip_prefix(ancestor).ok()?;
    if suffix
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let clean = if suffix.as_os_str().is_empty() {
        canonical_ancestor
    } else {
        canonical_ancestor.join(suffix)
    };
    if !clean.starts_with(repo)
        || clean
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    let relative = clean.strip_prefix(repo).ok()?.to_string_lossy().to_string();
    Some((clean, relative))
}

fn content_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(blake3::hash(&bytes).to_hex().to_string())
}

fn directory_hash(path: &Path) -> Option<String> {
    let mut entries = std::fs::read_dir(path)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    entries.sort();
    Some(
        blake3::hash(entries.join("\0").as_bytes())
            .to_hex()
            .to_string(),
    )
}

fn absence_root_hash(path: &Path) -> Option<String> {
    let namespace = nearest_existing_ancestor(path.parent()?)?;
    namespace.is_dir().then_some(())?;
    directory_hash(namespace)
}

pub fn snapshot(cwd: &Path, target: &str) -> Result<Option<TargetSnapshot>> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let Some((path, relative)) = normalize_relative(&repo, cwd, target) else {
        return Ok(None);
    };
    let locator_digest = private_digest("locator-v1", relative.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    let existing = store
        .records
        .iter()
        .rev()
        .find(|record| record.locator_digest == locator_digest)
        .cloned();
    let now = crate::automation::now_secs();
    if path.is_file() || path.is_dir() {
        let (hash, locator_kind) = if path.is_file() {
            (
                content_hash(&path).context("hash artifact content")?,
                "file",
            )
        } else {
            (
                directory_hash(&path).context("hash artifact directory")?,
                "directory",
            )
        };
        let evidence_ref = private_digest(
            "artifact-evidence-v1",
            format!("{repo_digest}\0{locator_digest}\0{hash}").as_bytes(),
        )?;
        if existing.as_ref().is_none_or(|record| {
            record.state.version_hash.as_deref() != Some(hash.as_str())
                || record.state.existence != ExistenceState::Present
        }) {
            store
                .records
                .retain(|record| record.locator_digest != locator_digest);
            store.records.push(ArtifactRecord {
                locator_digest: locator_digest.clone(),
                locator_kind: locator_kind.to_string(),
                content_hash: (locator_kind == "file").then(|| hash.clone()),
                coverage_root_hash: (locator_kind == "directory").then(|| hash.clone()),
                state: ArtifactState {
                    existence: ExistenceState::Present,
                    version_hash: Some(hash.clone()),
                    ..ArtifactState::default()
                },
                dimension_versions: DimensionVersions {
                    existence: Some(hash.clone()),
                    ..DimensionVersions::default()
                },
                adapter_version: ADAPTER_VERSION.to_string(),
                observed_at: now,
                evidence_ref,
            });
            save(&store_file, &store)?;
        }
        return Ok(Some(TargetSnapshot {
            locator_digest,
            version_hash: Some(hash),
            coverage: CoverageStatus::PresentFresh,
        }));
    }
    let current_root = absence_root_hash(&path);
    let coverage = match existing {
        Some(record)
            if record.state.existence == ExistenceState::Absent
                && record.coverage_root_hash == current_root =>
        {
            CoverageStatus::AbsentFresh
        }
        Some(_) => CoverageStatus::Stale,
        None => CoverageStatus::Missing,
    };
    Ok(Some(TargetSnapshot {
        locator_digest,
        version_hash: current_root,
        coverage,
    }))
}

pub fn record_absence(cwd: &Path, target: &str, evidence_ref: &str) -> Result<bool> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let Some((path, relative)) = normalize_relative(&repo, cwd, target) else {
        return Ok(false);
    };
    if path.exists() {
        return Ok(false);
    }
    let Some(root_hash) = absence_root_hash(&path) else {
        return Ok(false);
    };
    let locator_digest = private_digest("locator-v1", relative.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    store
        .records
        .retain(|record| record.locator_digest != locator_digest);
    store.records.push(ArtifactRecord {
        locator_digest,
        locator_kind: "file".to_string(),
        content_hash: None,
        coverage_root_hash: Some(root_hash.clone()),
        state: ArtifactState {
            existence: ExistenceState::Absent,
            version_hash: Some(root_hash.clone()),
            ..ArtifactState::default()
        },
        dimension_versions: DimensionVersions {
            existence: Some(root_hash.clone()),
            ..DimensionVersions::default()
        },
        adapter_version: ADAPTER_VERSION.to_string(),
        observed_at: crate::automation::now_secs(),
        evidence_ref: private_digest("receipt-ref-v1", evidence_ref.as_bytes())?,
    });
    save(&store_file, &store)?;
    Ok(true)
}

pub fn controller_probe_exact(
    cwd: &Path,
    target: &str,
    evidence_ref: &str,
) -> Result<ControllerProbeOutcome> {
    let Some(snapshot) = snapshot(cwd, target)? else {
        return Ok(ControllerProbeOutcome::Unavailable);
    };
    if snapshot.coverage == CoverageStatus::PresentFresh {
        return Ok(ControllerProbeOutcome::Present);
    }
    if record_absence(cwd, target, evidence_ref)? {
        return Ok(ControllerProbeOutcome::Absent);
    }
    Ok(ControllerProbeOutcome::Unavailable)
}

pub fn rejection_reason(outcome: &TypedReceipt) -> &'static str {
    match outcome {
        TypedReceipt::Admitted => "admitted",
        TypedReceipt::Rejected(reason) => reason,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypedReceipt {
    Admitted,
    Rejected(&'static str),
}

impl TypedReceipt {
    pub fn admitted(&self) -> bool {
        matches!(self, TypedReceipt::Admitted)
    }
}

pub fn record_typed_receipt(
    cwd: &Path,
    target: &str,
    receipt: &Value,
    evidence_ref: &str,
) -> Result<TypedReceipt> {
    if receipt.get("schema").and_then(Value::as_str) != Some("artifact-state-v1") {
        return Ok(TypedReceipt::Rejected("not an artifact-state-v1 receipt"));
    }
    if receipt.get("target").and_then(Value::as_str) != Some(target) {
        return Ok(TypedReceipt::Rejected("receipt names a different target"));
    }
    let Some(receipt_version) = receipt.get("version_hash").and_then(Value::as_str) else {
        return Ok(TypedReceipt::Rejected("receipt carries no version_hash"));
    };
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let Some((_, relative)) = normalize_relative(&repo, cwd, target) else {
        return Ok(TypedReceipt::Rejected(
            "target is outside the admitted repo",
        ));
    };
    let locator_digest = private_digest("locator-v1", relative.as_bytes())?;
    let Some(snapshot) = snapshot(cwd, target)? else {
        return Ok(TypedReceipt::Rejected("target could not be snapshotted"));
    };
    if snapshot.version_hash.as_deref() != Some(receipt_version) {
        return Ok(TypedReceipt::Rejected(
            "receipt version is stale for this file",
        ));
    }
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    let Some(record) = store
        .records
        .iter_mut()
        .rev()
        .find(|record| record.locator_digest == locator_digest)
    else {
        return Ok(TypedReceipt::Rejected("no gate record for this locator"));
    };
    let mut admitted = false;
    let population = receipt
        .get("population")
        .and_then(Value::as_str)
        .map(|value| match value {
            "non_empty" => Some(PopulationState::NonEmpty),
            "empty" => Some(PopulationState::Empty),
            _ => None,
        })
        .or_else(|| {
            receipt.get("count").and_then(Value::as_u64).map(|count| {
                if count == 0 {
                    Some(PopulationState::Empty)
                } else {
                    Some(PopulationState::NonEmpty)
                }
            })
        })
        .flatten();
    if receipt.get("population").is_some() && population.is_none() {
        return Ok(TypedReceipt::Rejected(
            "population field is not a known state",
        ));
    }
    if let Some(population) = population {
        record.state.population = population;
        record.dimension_versions.population = Some(receipt_version.to_string());
        admitted = true;
    }
    let wiring = receipt
        .get("wiring")
        .and_then(Value::as_str)
        .map(|value| match value {
            "connected" => Some(WiringState::Connected),
            "disconnected" => Some(WiringState::Disconnected),
            "partial" => Some(WiringState::Partial),
            _ => None,
        })
        .or_else(|| {
            receipt
                .get("reference_count")
                .and_then(Value::as_u64)
                .map(|count| {
                    if count == 0 {
                        Some(WiringState::Disconnected)
                    } else {
                        Some(WiringState::Connected)
                    }
                })
        })
        .flatten();
    if receipt.get("wiring").is_some() && wiring.is_none() {
        return Ok(TypedReceipt::Rejected("wiring field is not a known state"));
    }
    if let Some(wiring) = wiring {
        record.state.wiring = wiring;
        record.dimension_versions.wiring = Some(receipt_version.to_string());
        admitted = true;
    }
    let behavior = receipt
        .get("behavior")
        .and_then(Value::as_str)
        .map(|value| match value {
            "working" => Some(BehaviorState::Working),
            "broken" => Some(BehaviorState::Broken),
            "partial" => Some(BehaviorState::Partial),
            _ => None,
        })
        .or_else(|| {
            receipt
                .get("test_passed")
                .or_else(|| receipt.get("live_passed"))
                .and_then(Value::as_bool)
                .map(|passed| {
                    if passed {
                        Some(BehaviorState::Working)
                    } else {
                        Some(BehaviorState::Broken)
                    }
                })
        })
        .flatten();
    if receipt.get("behavior").is_some() && behavior.is_none() {
        return Ok(TypedReceipt::Rejected(
            "behavior field is not a known state",
        ));
    }
    if let Some(behavior) = behavior {
        record.state.behavior = behavior;
        record.dimension_versions.behavior = Some(receipt_version.to_string());
        admitted = true;
    }
    if !admitted {
        return Ok(TypedReceipt::Rejected(
            "receipt named no dimension to admit",
        ));
    }
    record.observed_at = crate::automation::now_secs();
    record.evidence_ref = private_digest("receipt-ref-v1", evidence_ref.as_bytes())?;
    save(&store_file, &store)?;
    Ok(TypedReceipt::Admitted)
}

fn shape(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(_) => out.push_str("bool"),
        Value::Number(_) => out.push_str("number"),
        Value::String(_) => out.push_str("string"),
        Value::Array(values) => {
            out.push('[');
            if let Some(first) = values.first() {
                shape(first, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                out.push_str(key);
                out.push(':');
                shape(&map[key], out);
                out.push(';');
            }
            out.push('}');
        }
    }
}

pub fn record_unknown_shape(cwd: &Path, input: &Value) -> Result<String> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let mut signature = String::new();
    shape(input, &mut signature);
    let shape_digest = private_digest("payload-shape-v1", signature.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    let now = crate::automation::now_secs();
    let count = if let Some(gap) = store
        .adapter_gaps
        .iter_mut()
        .find(|gap| gap.shape_digest == shape_digest)
    {
        gap.last_seen_at = now;
        gap.count = gap.count.saturating_add(1);
        gap.count
    } else {
        store.adapter_gaps.push(AdapterGap {
            shape_digest: shape_digest.clone(),
            first_seen_at: now,
            last_seen_at: now,
            count: 1,
        });
        1
    };
    save(&store_file, &store)?;
    if first_sighting_this_process(&shape_digest) {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "adapter_gap",
            shape = %digest_tag(&shape_digest),
            count,
            "[gate] unknown payload shape recorded"
        );
    } else {
        tracing::debug!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "adapter_gap",
            shape = %digest_tag(&shape_digest),
            count,
            "[gate] unknown payload shape recorded"
        );
    }
    Ok(shape_digest)
}

fn first_sighting_this_process(shape_digest: &str) -> bool {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let mut seen = match SEEN.get_or_init(|| Mutex::new(HashSet::new())).lock() {
        Ok(seen) => seen,
        Err(poisoned) => poisoned.into_inner(),
    };
    if seen.contains(shape_digest) {
        return false;
    }
    if seen.len() >= 512 {
        seen.clear();
    }
    seen.insert(shape_digest.to_string());
    true
}

#[allow(clippy::too_many_arguments)]
pub fn record_gate(
    cwd: &Path,
    session_id: &str,
    action_material: &str,
    native_identity: &str,
    locator_digest: Option<String>,
    state: GateState,
    decision: GateDecision,
    obligation_ids: Vec<String>,
) -> Result<GateRecord> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    let action_digest = private_digest("action-v1", action_material.as_bytes())?;
    let native_identity_digest = private_digest("native-hook-v1", native_identity.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    let now = crate::automation::now_secs();
    let mut stage = "opened";
    let record = if let Some(record) = store.gates.iter_mut().find(|record| {
        record.session_digest == session_digest && record.action_digest == action_digest
    }) {
        stage = if state == GateState::ClearedAbsent {
            "clearance_consumed"
        } else {
            "retried"
        };
        record.state = state;
        record.decision = decision;
        if state == GateState::ClearedAbsent {
            record.clearance_consumed = true;
        }
        record.updated_at = now;
        if !record
            .native_identity_digests
            .contains(&native_identity_digest)
        {
            record.attempts = record.attempts.saturating_add(1);
            record.native_identity_digests.push(native_identity_digest);
        }
        record.obligation_ids = obligation_ids;
        record.clone()
    } else {
        let record = GateRecord {
            session_digest,
            action_digest,
            locator_digest,
            state,
            decision,
            attempts: 1,
            probe_steps: 0,
            clearance_consumed: state == GateState::ClearedAbsent,
            probe_digests: Vec::new(),
            native_identity_digests: vec![native_identity_digest],
            controller_notified: false,
            obligation_ids,
            opened_at: now,
            updated_at: now,
        };
        store.gates.push(record.clone());
        record
    };
    if store.gates.len() > 128 {
        store.gates.drain(..store.gates.len() - 128);
    }
    expire_stale_gates(&mut store, now);
    save(&store_file, &store)?;
    trace_gate(stage, &record);
    Ok(record)
}

const GATE_TTL_SECS: i64 = 30 * 60;

fn expire_stale_gates(store: &mut ArtifactStore, now: i64) {
    for gate in store.gates.iter_mut() {
        let waiting = matches!(
            gate.state,
            GateState::AwaitingProof | GateState::TargetProbe | GateState::ControllerProbe
        );
        if !waiting || now - gate.updated_at < GATE_TTL_SECS {
            continue;
        }
        gate.state = GateState::Stale;
        gate.updated_at = now;
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "expired",
            session = %digest_tag(&gate.session_digest),
            action = %digest_tag(&gate.action_digest),
            attempts = gate.attempts,
            probe_steps = gate.probe_steps,
            waited_secs = GATE_TTL_SECS,
            "[gate] proof never arrived; gate expired to base behavior"
        );
        crate::decision_log::record(
            "gate",
            "expired",
            serde_json::json!({"stage": "ttl", "attempts": gate.attempts,
                               "probe_steps": gate.probe_steps,
                               "reason": "gate waited for proof past its deadline"}),
        );
    }
}

pub fn note_post_tool(
    cwd: &Path,
    session_id: &str,
    effect: ToolEffect,
    probe_material: &str,
    target: Option<&str>,
) -> Result<()> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    let mutation_locator = target.and_then(|target| {
        normalize_relative(&repo, cwd, target)
            .and_then(|(_, relative)| private_digest("locator-v1", relative.as_bytes()).ok())
    });
    let Some(gate) = store.gates.iter_mut().rev().find(|gate| {
        gate.session_digest == session_digest
            && matches!(
                gate.state,
                GateState::AwaitingProof | GateState::TargetProbe | GateState::ControllerProbe
            )
    }) else {
        crate::decision_log::record(
            "gate",
            "result_unmatched",
            serde_json::json!({"stage": "post_tool",
                               "reason": "tool result found no waiting gate for this session"}),
        );
        return Ok(());
    };
    let before = (gate.state, gate.probe_steps);
    match effect {
        ToolEffect::ReadOnly => {
            let signature = private_digest("probe-v1", probe_material.as_bytes())?;
            if !gate.probe_digests.contains(&signature) {
                gate.probe_digests.push(signature);
                gate.probe_steps = gate.probe_steps.saturating_add(1).min(3);
                gate.state = GateState::TargetProbe;
                gate.controller_notified = false;
            }
        }
        ToolEffect::Mutation(_) if gate.locator_digest == mutation_locator => {
            gate.state = GateState::Stale;
            gate.controller_notified = false;
        }
        ToolEffect::Mutation(_) => {}
        ToolEffect::ExternalSideEffect | ToolEffect::Unknown => {}
    }
    gate.updated_at = crate::automation::now_secs();
    let snapshot = gate.clone();
    let stage = if before == (snapshot.state, snapshot.probe_steps) {
        "post_observed"
    } else {
        "post_transition"
    };
    save(&store_file, &store).inspect(|_| trace_gate(stage, &snapshot))
}

pub fn note_controller_probe(
    cwd: &Path,
    session_id: &str,
    action_material: &str,
    signature: &str,
) -> Result<()> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    let action_digest = private_digest("action-v1", action_material.as_bytes())?;
    let probe_digest = private_digest("probe-v1", signature.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    let Some(gate) = store
        .gates
        .iter_mut()
        .find(|gate| gate.session_digest == session_digest && gate.action_digest == action_digest)
    else {
        tracing::info!(
            target: "relay::gate",
            pipeline = "gate",
            stage = "probe_unmatched",
            session = %digest_tag(&session_digest),
            action = %digest_tag(&action_digest),
            "[gate] controller probe arrived for no open gate"
        );
        crate::decision_log::record(
            "gate",
            "probe_unmatched",
            serde_json::json!({"stage": "controller_probe",
                               "reason": "probe receipt found no open gate to advance"}),
        );
        return Ok(());
    };
    if !gate.probe_digests.contains(&probe_digest) {
        gate.probe_digests.push(probe_digest);
        gate.probe_steps = gate.probe_steps.saturating_add(1).min(3);
    }
    gate.state = GateState::ControllerProbe;
    gate.controller_notified = false;
    gate.updated_at = crate::automation::now_secs();
    let snapshot = gate.clone();
    save(&store_file, &store).inspect(|_| trace_gate("controller_probe", &snapshot))
}

#[derive(Debug, Clone)]
pub struct RobotGateControl {
    pub active: bool,
    pub terminal: bool,
    pub directive: Option<String>,
    pub probe_steps: u8,
    pub state: Option<GateState>,
}

pub fn robot_gate_control(session_id: &str) -> Result<RobotGateControl> {
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    crate::fsutil::secure_dir(&root());
    let mut files = std::fs::read_dir(root())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    files.sort();
    for path in files.into_iter().rev() {
        let _lock = StoreLock::acquire(&path)?;
        let mut store = load(&path);
        let Some(index) = store
            .gates
            .iter()
            .rposition(|gate| gate.session_digest == session_digest)
        else {
            continue;
        };
        let gate = &mut store.gates[index];
        if matches!(
            gate.state,
            GateState::AwaitingProof | GateState::TargetProbe | GateState::ControllerProbe
        ) && (gate.probe_steps >= 3 || gate.attempts > 3)
        {
            gate.state = GateState::Quarantined;
            gate.decision = GateDecision::QuarantineBranch;
            gate.controller_notified = false;
            trace_gate("probe_budget_exhausted", gate);
        }
        let (active, terminal, text) = match gate.state {
            GateState::Quarantined => (
                true,
                true,
                "Terminal SafeBlocked for this Robot lease: stop this turn now, invoke no further tools, do not ask the user, and emit only the deterministic dossier/report.",
            ),
            GateState::RejectedPresent => (
                true,
                false,
                "The artifact is structurally present. Replan autonomously to Modify, Wiring, Population, or Behavior; do not retry Create/Replace.",
            ),
            GateState::AwaitingProof | GateState::TargetProbe | GateState::ControllerProbe => (
                true,
                false,
                "Continue the bounded proof ladder autonomously with one new typed read-only probe. Do not repeat a probe signature and do not ask the user.",
            ),
            GateState::Advisory
            | GateState::Replanning
            | GateState::ClearedAbsent
            | GateState::Stale => (false, false, ""),
        };
        let directive = (active && !gate.controller_notified).then(|| text.to_string());
        let probe_steps = gate.probe_steps;
        let state = gate.state;
        if directive.is_some() {
            gate.controller_notified = true;
            gate.updated_at = crate::automation::now_secs();
            trace_gate("directive_issued", gate);
            save(&path, &store)?;
        }
        return Ok(RobotGateControl {
            active,
            terminal,
            directive,
            probe_steps,
            state: Some(state),
        });
    }
    Ok(RobotGateControl {
        active: false,
        terminal: false,
        directive: None,
        probe_steps: 0,
        state: None,
    })
}

pub fn gate_for_action(
    cwd: &Path,
    session_id: &str,
    action_material: &str,
) -> Result<Option<GateRecord>> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    let action_digest = private_digest("action-v1", action_material.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    Ok(load(&store_file)
        .gates
        .into_iter()
        .rev()
        .find(|gate| gate.session_digest == session_digest && gate.action_digest == action_digest))
}

pub fn set_gate_resolution(
    cwd: &Path,
    session_id: &str,
    action_material: &str,
    state: GateState,
    decision: GateDecision,
) -> Result<()> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    let action_digest = private_digest("action-v1", action_material.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    if let Some(gate) = store
        .gates
        .iter_mut()
        .find(|gate| gate.session_digest == session_digest && gate.action_digest == action_digest)
    {
        let changed = gate.state != state || gate.decision != decision;
        gate.state = state;
        gate.decision = decision;
        if changed {
            gate.controller_notified = false;
        }
        gate.updated_at = crate::automation::now_secs();
        let snapshot = gate.clone();
        save(&store_file, &store)?;
        trace_gate(
            if changed {
                "resolved"
            } else {
                "resolution_replayed"
            },
            &snapshot,
        );
    }
    Ok(())
}

pub fn mark_replan_delivered(session_id: &str) -> Result<()> {
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    crate::fsutil::secure_dir(&root());
    let mut files = std::fs::read_dir(root())
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    files.sort();
    for path in files.into_iter().rev() {
        let _lock = StoreLock::acquire(&path)?;
        let mut store = load(&path);
        let Some(gate) = store
            .gates
            .iter_mut()
            .rev()
            .find(|gate| gate.session_digest == session_digest)
        else {
            continue;
        };
        if gate.state == GateState::RejectedPresent {
            gate.state = GateState::Replanning;
            gate.decision = GateDecision::AutonomousResolve;
            gate.updated_at = crate::automation::now_secs();
            let snapshot = gate.clone();
            save(&path, &store)?;
            trace_gate("replan_delivered", &snapshot);
        }
        return Ok(());
    }
    Ok(())
}

pub fn admit_absence_to_latest_gate(cwd: &Path, session_id: &str) -> Result<()> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    if let Some(gate) = store.gates.iter_mut().rev().find(|gate| {
        gate.session_digest == session_digest
            && matches!(
                gate.state,
                GateState::AwaitingProof | GateState::TargetProbe | GateState::ControllerProbe
            )
    }) {
        gate.state = GateState::ClearedAbsent;
        gate.decision = GateDecision::DeferToBase;
        gate.clearance_consumed = false;
        gate.controller_notified = false;
        gate.updated_at = crate::automation::now_secs();
        let snapshot = gate.clone();
        save(&store_file, &store)?;
        trace_gate("absence_admitted", &snapshot);
    }
    Ok(())
}

pub fn admit_absence_to_gate(cwd: &Path, session_id: &str, action_material: &str) -> Result<()> {
    let repo = canonical_repo(cwd)?;
    let repo_digest = repo_digest(&repo)?;
    let session_digest = private_digest("session-v1", session_id.as_bytes())?;
    let action_digest = private_digest("action-v1", action_material.as_bytes())?;
    let store_file = store_path(&repo_digest);
    let _lock = StoreLock::acquire(&store_file)?;
    let mut store = load(&store_file);
    if let Some(gate) = store
        .gates
        .iter_mut()
        .find(|gate| gate.session_digest == session_digest && gate.action_digest == action_digest)
    {
        gate.state = GateState::ClearedAbsent;
        gate.decision = GateDecision::DeferToBase;
        gate.clearance_consumed = false;
        gate.controller_notified = false;
        gate.updated_at = crate::automation::now_secs();
        let snapshot = gate.clone();
        save(&store_file, &store)?;
        trace_gate("absence_admitted", &snapshot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_shape_never_contains_values() {
        let value = serde_json::json!({"secret":"do-not-store", "nested":{"path":"raw"}});
        let mut result = String::new();
        shape(&value, &mut result);
        assert!(!result.contains("do-not-store"));
        assert!(!result.contains("raw"));
        assert!(result.contains("secret:string"));
    }

    #[test]
    fn a_gate_that_never_gets_its_proof_expires_instead_of_waiting_forever() {
        let mut store = ArtifactStore::default();
        let now = 1_000_000i64;
        let gate = |state, updated_at| GateRecord {
            session_digest: String::new(),
            action_digest: String::new(),
            locator_digest: None,
            state,
            decision: GateDecision::AskProof,
            attempts: 1,
            probe_steps: 0,
            clearance_consumed: false,
            probe_digests: Vec::new(),
            native_identity_digests: Vec::new(),
            controller_notified: false,
            obligation_ids: Vec::new(),
            opened_at: updated_at,
            updated_at,
        };
        store.gates = vec![
            gate(GateState::AwaitingProof, now - GATE_TTL_SECS - 1),
            gate(GateState::AwaitingProof, now - 5),
            gate(GateState::ClearedAbsent, now - GATE_TTL_SECS - 1),
        ];

        expire_stale_gates(&mut store, now);

        assert_eq!(
            store.gates[0].state,
            GateState::Stale,
            "a gate past its deadline stops holding the action"
        );
        assert_eq!(
            store.gates[1].state,
            GateState::AwaitingProof,
            "a gate still inside the window keeps waiting"
        );
        assert_eq!(
            store.gates[2].state,
            GateState::ClearedAbsent,
            "a settled gate is not reopened by the sweep"
        );
    }

    #[test]
    fn every_refused_receipt_says_which_predicate_refused_it() {
        let _guard = ARTIFACT_ENV.lock().unwrap();
        let base = std::env::temp_dir().join(format!(
            "vsc-relay-receipt-reasons-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::env::set_var("VSC_RELAY_ARTIFACT_ROOT", base.join("private-store"));
        let target = "notes.txt";
        std::fs::write(repo.join(target), b"one").unwrap();

        let cases = vec![
            (
                serde_json::json!("prose"),
                "not an artifact-state-v1 receipt",
            ),
            (
                serde_json::json!({"schema":"artifact-state-v1","target":"other.txt"}),
                "receipt names a different target",
            ),
            (
                serde_json::json!({"schema":"artifact-state-v1","target":target}),
                "receipt carries no version_hash",
            ),
            (
                serde_json::json!({"schema":"artifact-state-v1","target":target,
                                   "version_hash":"stale","behavior":"working"}),
                "receipt version is stale for this file",
            ),
        ];
        for (receipt, expected) in cases {
            let outcome = record_typed_receipt(&repo, target, &receipt, "ref").unwrap();
            assert_eq!(
                rejection_reason(&outcome),
                expected,
                "a bare admitted=false cannot explain {receipt}"
            );
        }
        std::env::remove_var("VSC_RELAY_ARTIFACT_ROOT");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn private_versioned_store_recovers_backup_without_raw_text() {
        let _guard = ARTIFACT_ENV.lock().unwrap();
        let unique = format!(
            "vsc-relay-artifacts-{}-{}",
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
        std::fs::create_dir_all(repo.join("private")).unwrap();
        std::env::set_var("VSC_RELAY_ARTIFACT_ROOT", &private_root);

        let target = "private/user-secret-artifact.rs";
        std::fs::write(repo.join(target), b"version-one").unwrap();
        let first = snapshot(&repo, target).unwrap().unwrap();
        assert!(!TypedReceipt::admitted(
            &record_typed_receipt(
                &repo,
                target,
                &serde_json::json!("the artifact works and is fully wired"),
                "prose-is-not-evidence",
            )
            .unwrap()
        ));
        assert!(!TypedReceipt::admitted(
            &record_typed_receipt(
                &repo,
                target,
                &serde_json::json!({
                    "schema":"artifact-state-v1",
                    "target":target,
                    "version_hash":"stale-version",
                    "behavior":"working"
                }),
                "stale-receipt",
            )
            .unwrap()
        ));
        assert!(TypedReceipt::admitted(
            &record_typed_receipt(
                &repo,
                target,
                &serde_json::json!({
                    "schema":"artifact-state-v1",
                    "target":target,
                    "version_hash":first.version_hash.clone(),
                    "count":7,
                    "reference_count":1
                }),
                "typed-receipt-1",
            )
            .unwrap()
        ));
        let repo_id = repo_digest(&repo.canonicalize().unwrap()).unwrap();
        let path = store_path(&repo_id);
        let admitted = decode(&path).unwrap();
        let admitted = admitted.records.last().unwrap();
        assert_eq!(admitted.state.population, PopulationState::NonEmpty);
        assert_eq!(admitted.state.wiring, WiringState::Connected);
        assert_eq!(admitted.state.behavior, BehaviorState::Unknown);

        std::fs::write(repo.join(target), b"version-two").unwrap();
        let second = snapshot(&repo, target).unwrap().unwrap();
        assert_ne!(first.version_hash, second.version_hash);

        let nested_absent = "generated/deep/new-artifact.rs";
        assert_eq!(
            controller_probe_exact(&repo, nested_absent, "controller-nested-absence").unwrap(),
            ControllerProbeOutcome::Absent
        );
        assert_eq!(
            snapshot(&repo, nested_absent).unwrap().unwrap().coverage,
            CoverageStatus::AbsentFresh
        );

        std::fs::create_dir_all(repo.join("existing-directory")).unwrap();
        assert_eq!(
            snapshot(&repo, "existing-directory")
                .unwrap()
                .unwrap()
                .coverage,
            CoverageStatus::PresentFresh
        );

        record_gate(
            &repo,
            "robot-clearance-session",
            "lease-v1",
            "native-hold",
            Some(second.locator_digest.clone()),
            GateState::TargetProbe,
            GateDecision::AutonomousResolve,
            vec!["obl-0-1".to_string()],
        )
        .unwrap();
        admit_absence_to_latest_gate(&repo, "robot-clearance-session").unwrap();
        let ready = gate_for_action(&repo, "robot-clearance-session", "lease-v1")
            .unwrap()
            .unwrap();
        assert_eq!(ready.state, GateState::ClearedAbsent);
        assert!(!ready.clearance_consumed);
        assert!(
            !robot_gate_control("robot-clearance-session")
                .unwrap()
                .active
        );
        let consumed = record_gate(
            &repo,
            "robot-clearance-session",
            "lease-v1",
            "native-mutation",
            Some(second.locator_digest.clone()),
            GateState::ClearedAbsent,
            GateDecision::DeferToBase,
            Vec::new(),
        )
        .unwrap();
        assert!(consumed.clearance_consumed);

        record_gate(
            &repo,
            "robot-replan-session",
            "duplicate-lease",
            "native-duplicate",
            Some(second.locator_digest.clone()),
            GateState::RejectedPresent,
            GateDecision::Deny,
            vec!["obl-duplicate".to_string()],
        )
        .unwrap();
        let replan = robot_gate_control("robot-replan-session").unwrap();
        assert_eq!(replan.state, Some(GateState::RejectedPresent));
        assert!(replan.directive.is_some());
        mark_replan_delivered("robot-replan-session").unwrap();
        assert!(!robot_gate_control("robot-replan-session").unwrap().active);

        record_unknown_shape(
            &repo,
            &serde_json::json!({"path":"raw/private/path", "chat":"never persist me"}),
        )
        .unwrap();
        let serialized = std::fs::read_to_string(&path).unwrap();
        assert!(!serialized.contains("user-secret-artifact"));
        assert!(!serialized.contains("generated/deep"));
        assert!(!serialized.contains("existing-directory"));
        assert!(!serialized.contains("raw/private/path"));
        assert!(!serialized.contains("never persist me"));

        let store = decode(&path).expect("valid checksum envelope");
        let record = store.records.last().unwrap();
        assert_eq!(record.state.existence, ExistenceState::Present);
        assert_eq!(
            record.state.population,
            relay_compass::PopulationState::Unknown
        );
        assert_eq!(record.state.wiring, relay_compass::WiringState::Unknown);
        assert_eq!(record.state.behavior, relay_compass::BehaviorState::Unknown);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        record_unknown_shape(&repo, &serde_json::json!({"future":true})).unwrap();
        assert!(backup_path(&path).exists());
        std::fs::write(&path, b"corrupt").unwrap();
        record_unknown_shape(&repo, &serde_json::json!({"third":3})).unwrap();
        assert!(
            decode(&path).is_some(),
            "primary must be restored from .bak"
        );

        std::env::remove_var("VSC_RELAY_ARTIFACT_ROOT");
        let _ = std::fs::remove_dir_all(base);
    }
}
