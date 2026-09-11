use crate::config::SemanticConfig;
use anyhow::{bail, Context, Result};
use ort::{
    session::{builder::GraphOptimizationLevel, Session},
    value::Tensor,
};
use relay_compass::{
    ClassScore, Relation, RelationScore, SemanticFacts, SemanticInputFrame, Truth,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: u32,
    kind: String,
    model_id: String,
    model_file: String,
    tokenizer_file: String,
    max_length: usize,
    entailment_index: usize,
    neutral_index: usize,
    contradiction_index: usize,
    #[serde(default = "unit_temperature")]
    temperature: f32,
    #[serde(default)]
    calibrated: bool,
    #[serde(default)]
    calibration: Option<CalibrationManifest>,
}

fn unit_temperature() -> f32 {
    1.0
}

#[derive(Debug, Deserialize)]
struct CalibrationManifest {
    dataset_sha256: String,
    held_out_sessions: usize,
    working_pending_negatives: usize,
    false_steers: usize,
    precision_critical: f32,
    recall_critical: f32,
    ece: f32,
    brier: f32,
}

struct LocalNli {
    dir: PathBuf,
    manifest: Manifest,
    tokenizer: Tokenizer,
    session: Session,
    cache: VecDeque<(String, SemanticFacts)>,
}

static MODEL: OnceLock<Mutex<Option<LocalNli>>> = OnceLock::new();

fn model_slot() -> &'static Mutex<Option<LocalNli>> {
    MODEL.get_or_init(|| Mutex::new(None))
}

fn read_manifest(dir: &Path) -> Result<Manifest> {
    let text = std::fs::read_to_string(dir.join("manifest.json"))
        .with_context(|| format!("read {}/manifest.json", dir.display()))?;
    let manifest: Manifest = serde_json::from_str(&text)?;
    if manifest.schema != 1 || manifest.kind != "nli" {
        bail!("unsupported local semantic manifest schema/kind");
    }
    if manifest.entailment_index > 2
        || manifest.neutral_index > 2
        || manifest.contradiction_index > 2
    {
        bail!("NLI output indices must be in 0..=2");
    }
    if !manifest.temperature.is_finite() || !(0.1..=10.0).contains(&manifest.temperature) {
        bail!("NLI temperature must be finite and in 0.1..=10");
    }
    if manifest.calibrated {
        let profile = manifest
            .calibration
            .as_ref()
            .context("calibrated bundle is missing calibration provenance")?;
        let digest_ok = profile.dataset_sha256.len() == 64
            && profile
                .dataset_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit());
        if !digest_ok {
            bail!("calibration dataset_sha256 must be 64 hexadecimal characters");
        }
        if profile.held_out_sessions < 50
            || profile.working_pending_negatives < 600
            || profile.false_steers != 0
            || !profile.precision_critical.is_finite()
            || profile.precision_critical < 0.92
            || !profile.recall_critical.is_finite()
            || profile.recall_critical < 0.82
            || !profile.ece.is_finite()
            || profile.ece > 0.05
            || !profile.brier.is_finite()
            || !(0.0..=1.0).contains(&profile.brier)
        {
            bail!("calibration profile does not meet the auto-steer acceptance gate");
        }
    }
    Ok(manifest)
}

impl LocalNli {
    fn load(dir: &Path) -> Result<Self> {
        let manifest = read_manifest(dir)?;
        let mut tokenizer = Tokenizer::from_file(dir.join(&manifest.tokenizer_file))
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: manifest.max_length.clamp(32, 512),
                ..TruncationParams::default()
            }))
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..PaddingParams::default()
        }));
        let session = Session::builder()?
            .with_intra_threads(2)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Disable)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .commit_from_file(dir.join(&manifest.model_file))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
            tokenizer,
            session,
            cache: VecDeque::new(),
        })
    }

    fn probabilities(&mut self, pairs: &[(String, String)]) -> Result<Vec<[f32; 3]>> {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(pairs.to_vec(), true)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let batch = encodings.len();
        let width = encodings
            .first()
            .map(|encoding| encoding.get_ids().len())
            .unwrap_or(0);
        if width == 0 {
            bail!("tokenizer returned an empty batch");
        }
        let mut ids = Vec::with_capacity(batch * width);
        let mut masks = Vec::with_capacity(batch * width);
        for encoding in &encodings {
            ids.extend(encoding.get_ids().iter().map(|value| i64::from(*value)));
            masks.extend(
                encoding
                    .get_attention_mask()
                    .iter()
                    .map(|value| i64::from(*value)),
            );
        }
        let outputs = self.session.run(ort::inputs! {
            "input_ids" => Tensor::from_array(([batch, width], ids))?,
            "attention_mask" => Tensor::from_array(([batch, width], masks))?
        })?;
        let (_, logits) = outputs[0].try_extract_tensor::<f32>()?;
        if logits.len() != batch * 3 {
            bail!(
                "NLI model returned {} logits for batch {batch}",
                logits.len()
            );
        }
        Ok(logits
            .as_chunks::<3>()
            .0
            .iter()
            .map(|row| {
                let temperature = self.manifest.temperature;
                let max = row
                    .iter()
                    .map(|value| *value / temperature)
                    .fold(f32::NEG_INFINITY, f32::max);
                let mut exp = [0.0; 3];
                let mut sum = 0.0;
                for (index, value) in row.iter().enumerate() {
                    exp[index] = (*value / temperature - max).exp();
                    sum += exp[index];
                }
                [exp[0] / sum, exp[1] / sum, exp[2] / sum]
            })
            .collect())
    }
}

fn frame_key(frame: &SemanticInputFrame) -> String {
    let bytes = serde_json::to_vec(frame).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

fn binary_score(probabilities: [f32; 3], manifest: &Manifest, threshold: f32) -> ClassScore {
    let entailment = probabilities[manifest.entailment_index];
    let neutral = probabilities[manifest.neutral_index];
    let contradiction = probabilities[manifest.contradiction_index];
    let threshold = threshold.clamp(0.55, 0.98);
    let (state, probability) = [
        (Truth::Yes, entailment),
        (Truth::Unknown, neutral),
        (Truth::No, contradiction),
    ]
    .into_iter()
    .max_by(|left, right| left.1.total_cmp(&right.1))
    .expect("three NLI classes");
    if state == Truth::Unknown || probability < threshold {
        ClassScore {
            state: Truth::Unknown,
            probability,
        }
    } else {
        ClassScore { state, probability }
    }
}

fn relation_score(
    probabilities: &[[f32; 3]],
    labels: &[Relation],
    manifest: &Manifest,
    threshold: f32,
) -> RelationScore {
    let entailments: Vec<f32> = probabilities
        .iter()
        .map(|row| row[manifest.entailment_index])
        .collect();
    let total = entailments.iter().sum::<f32>().max(1e-6);
    let mut ranked: Vec<(usize, f32)> = entailments
        .iter()
        .enumerate()
        .map(|(index, value)| (index, *value / total))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (best_index, best) = ranked[0];
    let margin = best - ranked.get(1).map(|item| item.1).unwrap_or(0.0);
    if best < threshold.clamp(0.45, 0.95) || margin < 0.10 {
        RelationScore::new(Relation::Unknown, best)
    } else {
        RelationScore::new(labels[best_index], best)
    }
}

fn assistant_context(frame: &SemanticInputFrame) -> String {
    format!(
        "ASSISTANT RESPONSE:\n{}\n\nCURRENT USER CONTRACT:\n{}\n\nUSER GOAL:\n{}",
        frame.assistant, frame.user_contract, frame.goal
    )
}

fn evidence_context(frame: &SemanticInputFrame) -> String {
    format!(
        "ASSISTANT RESPONSE:\n{}\n\nTOOLS:\n{}\n\nRUNTIME OUTPUT:\n{}",
        frame.assistant,
        frame.tools.join("\n"),
        frame.runtime.join("\n")
    )
}

fn classify_frame(
    model: &mut LocalNli,
    frame: &SemanticInputFrame,
    threshold: f32,
) -> Result<SemanticFacts> {
    let context = assistant_context(frame);
    let speech = [
        "The requested work is complete.",
        "Work cannot continue because of a blocker.",
        "The result was verified with concrete evidence.",
        "The previous assistant response is wrong or incomplete.",
        "The previous goal is cancelled or replaced with a new goal.",
        "Work will continue.",
        "The evidence is older than the latest change.",
        "A proxy check is treated as the requested final deliverable.",
    ]
    .map(str::to_string);
    let evidence = evidence_context(frame);
    let goal_hypotheses = [
        "The assistant response directly supports and covers the user's requested goal.",
        "The assistant response covers only part of the user's requested goal.",
        "The assistant response contradicts the user's requested goal.",
        "The assistant response is unrelated to the user's requested goal.",
    ]
    .map(str::to_string);
    let evidence_hypotheses = [
        "The runtime and tool evidence supports the assistant's claim.",
        "The runtime and tool evidence only partially supports the assistant's claim.",
        "The runtime and tool evidence contradicts the assistant's claim.",
        "The runtime and tool evidence is unrelated to the assistant's claim.",
    ]
    .map(str::to_string);
    let assistant = frame.assistant.clone();
    let user = frame.user_contract.clone();
    let speech_premises = [
        assistant.clone(),
        assistant.clone(),
        assistant.clone(),
        user.clone(),
        user,
        assistant.clone(),
        context.clone(),
        context.clone(),
    ];
    let speech_pairs: Vec<(String, String)> = speech_premises.into_iter().zip(speech).collect();
    let speech_probabilities = model.probabilities(&speech_pairs)?;
    let binary: Vec<ClassScore> = speech_probabilities
        .iter()
        .map(|probabilities| binary_score(*probabilities, &model.manifest, threshold))
        .collect();

    let goal_pairs: Vec<(String, String)> = goal_hypotheses
        .iter()
        .map(|hypothesis| (context.clone(), hypothesis.clone()))
        .collect();
    let goal_probabilities = model.probabilities(&goal_pairs)?;
    let goal_relation = relation_score(
        &goal_probabilities,
        &[
            Relation::Supports,
            Relation::Partial,
            Relation::Contradicts,
            Relation::Unrelated,
        ],
        &model.manifest,
        threshold - 0.15,
    );

    let evidence_pairs: Vec<(String, String)> = evidence_hypotheses
        .iter()
        .map(|hypothesis| (evidence.clone(), hypothesis.clone()))
        .collect();
    let evidence_probabilities = model.probabilities(&evidence_pairs)?;
    let evidence_relation = relation_score(
        &evidence_probabilities,
        &[
            Relation::Supports,
            Relation::Partial,
            Relation::Contradicts,
            Relation::Unrelated,
        ],
        &model.manifest,
        threshold - 0.15,
    );

    Ok(SemanticFacts {
        episode: frame.episode,
        completion_claim: binary[0],
        blocker_claim: binary[1],
        verification_claim: binary[2],
        correction: binary[3],
        pivot: binary[4],
        continuation_intent: binary[5],
        stale_evidence_claim: binary[6],
        proxy_focus: binary[7],
        goal_relation,
        evidence_relation,
        required_evidence_layer: relay_compass::ProofLayer::Unknown,
        observed_evidence_layer: relay_compass::ProofLayer::Unknown,
        contract_atoms: Vec::new(),
        obligation_links: Vec::new(),
        calibrated: model.manifest.calibrated,
        backend: "local_nli".to_string(),
        model: model.manifest.model_id.clone(),
    })
}

pub fn classify(
    config: &SemanticConfig,
    frames: &[SemanticInputFrame],
) -> Result<Vec<SemanticFacts>> {
    let dir = config.model_dir();
    let mut slot = model_slot()
        .lock()
        .map_err(|_| anyhow::anyhow!("local model lock poisoned"))?;
    if slot.as_ref().map(|model| model.dir.as_path()) != Some(dir.as_path()) {
        *slot = Some(LocalNli::load(&dir)?);
    }
    let model = slot.as_mut().expect("model loaded");
    let mut result = Vec::with_capacity(frames.len());
    for frame in frames {
        let key = frame_key(frame);
        if let Some((_, facts)) = model.cache.iter().find(|(cached, _)| cached == &key) {
            result.push(facts.clone());
            continue;
        }
        let facts = classify_frame(model, frame, config.min_confidence)?;
        model.cache.push_back((key, facts.clone()));
        while model.cache.len() > 128 {
            model.cache.pop_front();
        }
        result.push(facts);
    }
    Ok(result)
}

pub fn status(config: &SemanticConfig) -> Result<String> {
    let dir = config.model_dir();
    let manifest = read_manifest(&dir)?;
    let model = dir.join(&manifest.model_file);
    let tokenizer = dir.join(&manifest.tokenizer_file);
    if !model.is_file() || !tokenizer.is_file() {
        bail!("local semantic bundle is incomplete at {}", dir.display());
    }
    let calibration = manifest
        .calibration
        .as_ref()
        .filter(|_| manifest.calibrated)
        .map(|profile| {
            format!(
                ", held_out_sessions={}, negatives={}, precision={:.3}, recall={:.3}, ece={:.3}, brier={:.3}",
                profile.held_out_sessions,
                profile.working_pending_negatives,
                profile.precision_critical,
                profile.recall_critical,
                profile.ece,
                profile.brier
            )
        })
        .unwrap_or_default();
    Ok(format!(
        "ready: {} at {} (calibrated={}{})",
        manifest.model_id,
        dir.display(),
        manifest.calibrated,
        calibration
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(calibrated: bool, calibration: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "kind": "nli",
            "model_id": "test",
            "model_file": "model.onnx",
            "tokenizer_file": "tokenizer.json",
            "max_length": 128,
            "entailment_index": 0,
            "neutral_index": 1,
            "contradiction_index": 2,
            "calibrated": calibrated,
            "calibration": calibration
        })
    }

    fn write_manifest(value: serde_json::Value) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "relay-semantic-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn calibrated_flag_without_provenance_is_rejected() {
        let dir = write_manifest(manifest(true, serde_json::Value::Null));
        assert!(read_manifest(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn neutral_nli_mass_abstains_instead_of_becoming_yes_or_no() {
        let manifest: Manifest =
            serde_json::from_value(manifest(false, serde_json::Value::Null)).unwrap();
        let score = binary_score([0.06, 0.90, 0.04], &manifest, 0.78);
        assert_eq!(score.state, Truth::Unknown);
        assert!((score.probability - 0.90).abs() < f32::EPSILON);
    }

    #[test]
    fn acceptance_grade_calibration_profile_is_allowed() {
        let profile = serde_json::json!({
            "dataset_sha256": "a".repeat(64),
            "held_out_sessions": 50,
            "working_pending_negatives": 600,
            "false_steers": 0,
            "precision_critical": 0.92,
            "recall_critical": 0.82,
            "ece": 0.05,
            "brier": 0.20
        });
        let dir = write_manifest(manifest(true, profile));
        assert!(read_manifest(&dir).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
