use std::collections::{BTreeSet, HashMap, VecDeque};

pub mod feedback;
pub mod gate;
pub mod handoff;
pub mod health;
pub mod information;
pub mod ledger;
pub mod predictive;
pub mod semantic;
pub mod staged;
pub mod steps;

pub use feedback::{
    health_steer_id, parse_health_result, session_health_protocol_context, ConflictAnchor,
    ConflictSource, ConflictStance, ConflictVersion, ContractRelation, HealthResult,
    HealthResultError, HealthResultStatus, StateConflict, StateConflictClaim, TopicRelation,
    HEALTH_RESULT_CLOSE, HEALTH_RESULT_OPEN, SESSION_HEALTH_STEER_ID,
};
pub use gate::{
    ArtifactState, BehaviorState, CoverageStatus, ExistenceState, GateDecision, GateProof,
    GateState, MutationKind, PopulationState, ToolEffect, WiringState,
};
pub use handoff::{
    build_handoff, compact_request, extract_receipt, handoff_prompt, missing_anchors,
    receipt_request, render_handoff_markdown, HandoffBrief, HandoffCommand, HandoffCompact,
    HandoffContext, HandoffLimits, HandoffObligation, HandoffTurn, RECEIPT_CLOSE, RECEIPT_HEADING,
    RECEIPT_OPEN,
};
pub use information::InformationProfile;
pub use ledger::{
    build_contract_ledger, build_contract_ledger_from_input,
    build_contract_ledger_from_input_with_seeds, build_contract_ledger_with_seeds,
    contract_input_from_steps, contract_input_from_steps_promoted, document_shaped,
    AdmissionReason, AgentFeedbackMetrics, ArtifactEvidenceEdge, AtomizationMetrics,
    AtomizationMode, ContinuationMetrics, ContractAnchor, ContractCandidate, ContractInput,
    ContractLedger, DelegatedReport, LedgerSeed, ObligationOrigin, ObligationState, ProofLayer,
    ProvenanceMetrics, TopicIntersection, TopicIntersectionMetrics,
};
pub use semantic::{
    ClassScore, ContractAtomHint, ContractAtomKind, ContractSource, ObligationLinkHint, Relation,
    RelationScore, SemanticFacts, SemanticInputFrame, Truth,
};
pub use steps::{ContextFlags, SemanticStep, SourceTurnId, StepRole, ToolKind, UserOrigin};

pub const SIM_BITS: usize = 1024;
pub const SIM_WORDS: usize = SIM_BITS / 64;
const EPS_NORM: f32 = 1e-6;
const UNPROFILED_KEY: [u8; 32] = [0; 32];

#[derive(Debug, Clone, PartialEq)]
pub enum Vector {
    Sim([u64; SIM_WORDS]),
    Dense(Vec<f32>),
}

fn features(text: &str) -> Vec<(String, f32)> {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut tf: HashMap<String, u32> = HashMap::new();
    for n in [3usize, 4, 5] {
        if chars.len() >= n {
            for w in chars.windows(n) {
                if w.iter().all(|c| c.is_whitespace()) {
                    continue;
                }
                let g: String = w.iter().collect();
                *tf.entry(format!("c{n}\u{1f}{g}")).or_insert(0) += 1;
            }
        }
    }
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    for n in [1usize, 2] {
        if words.len() >= n {
            for w in words.windows(n) {
                let g = w.join(" ");
                *tf.entry(format!("w{n}\u{1f}{g}")).or_insert(0) += 1;
            }
        }
    }
    let mut feats: Vec<(String, f32)> = tf
        .into_iter()
        .map(|(f, c)| (f, 1.0 + (c as f32).ln()))
        .collect();
    feats.sort_by(|a, b| a.0.cmp(&b.0));
    feats
}

fn simhash(feats: &[(String, f32)]) -> [u64; SIM_WORDS] {
    let mut acc = [0f32; SIM_BITS];
    for (f, w) in feats {
        let mut hasher = blake3::Hasher::new_derive_key("vsc-relay-compass-simhash-v2");
        hasher.update(f.as_bytes());
        let mut bytes = [0u8; SIM_BITS / 8];
        hasher.finalize_xof().fill(&mut bytes);
        for (i, a) in acc.iter_mut().enumerate() {
            let bit = (bytes[i / 8] >> (i % 8)) & 1;
            *a += if bit == 1 { *w } else { -*w };
        }
    }
    let mut sig = [0u64; SIM_WORDS];
    for (i, a) in acc.iter().enumerate() {
        if *a > 0.0 {
            sig[i / 64] |= 1u64 << (i % 64);
        }
    }
    sig
}

pub fn embed(text: &str) -> Option<Vector> {
    let feats = features(text);
    if feats.is_empty() {
        return None;
    }
    Some(Vector::Sim(simhash(&feats)))
}

pub fn feature_count(text: &str) -> usize {
    features(text).len()
}

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < EPS_NORM {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

pub fn distance(a: &Vector, b: &Vector) -> Option<f32> {
    match (a, b) {
        (Vector::Sim(x), Vector::Sim(y)) => {
            let h: u32 = x.iter().zip(y).map(|(p, q)| (p ^ q).count_ones()).sum();
            Some(h as f32 / SIM_BITS as f32)
        }
        (Vector::Dense(x), Vector::Dense(y)) if x.len() == y.len() && !x.is_empty() => {
            let (xn, yn) = (normalize(x)?, normalize(y)?);
            let dot: f32 = xn.iter().zip(&yn).map(|(p, q)| p * q).sum();
            let diff: f32 = xn
                .iter()
                .zip(&yn)
                .map(|(p, q)| (p - q) * (p - q))
                .sum::<f32>()
                .sqrt();
            let d = if dot >= 0.0 {
                2.0 * (diff / 2.0).min(1.0).asin() / std::f32::consts::PI
            } else {
                dot.clamp(-1.0, 1.0).acos() / std::f32::consts::PI
            };
            Some(d)
        }
        _ => None,
    }
}

pub fn alignment(a: &Vector, b: &Vector) -> Option<f32> {
    distance(a, b).map(|d| (d * std::f32::consts::PI).cos())
}

pub fn is_context_noise(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_lowercase();
    [
        "this session is being continued",
        "caveat:",
        "[system notification",
        "[system reminder",
        "[vsc_relay_health_steer",
        "your questions have been answered:",
        "todos have been modified successfully",
        "async agent launched successfully",
        "tool_use_error",
        "stdout",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

pub fn is_transcript_noise(text: &str) -> bool {
    text.trim().starts_with('<') || is_context_noise(text)
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn signal_terms(text: &str) -> BTreeSet<String> {
    words(text).into_iter().collect()
}

pub fn is_substantive_goal(text: &str) -> bool {
    !is_transcript_noise(text) && !words(text).is_empty()
}

pub fn select_goal_index(turns: &[String]) -> Option<usize> {
    select_goal_index_profiled(turns, &InformationProfile::default(), &UNPROFILED_KEY)
}

pub fn select_goal_index_authoritative(turns: &[String]) -> Option<usize> {
    let segment_start = turns
        .iter()
        .rposition(|text| {
            text.trim()
                .to_lowercase()
                .starts_with("this session is being continued")
        })
        .map(|index| index + 1)
        .unwrap_or(0);
    turns
        .iter()
        .enumerate()
        .skip(segment_start)
        .find(|(_, text)| is_substantive_goal(text))
        .or_else(|| {
            turns
                .iter()
                .enumerate()
                .find(|(_, text)| is_substantive_goal(text))
        })
        .map(|(index, _)| index)
}

pub fn select_goal_index_profiled(
    turns: &[String],
    profile: &InformationProfile,
    key: &[u8; 32],
) -> Option<usize> {
    let segment_start = turns
        .iter()
        .rposition(|text| {
            text.trim()
                .to_lowercase()
                .starts_with("this session is being continued")
        })
        .map(|index| index + 1)
        .unwrap_or(0);
    let mut search_start = segment_start;
    let mut candidates = turns
        .iter()
        .enumerate()
        .skip(search_start)
        .filter(|(_, text)| is_substantive_goal(text))
        .map(|(index, text)| (index, profile.information_mass(text, key)))
        .collect::<Vec<_>>();
    if candidates.is_empty() && search_start > 0 {
        search_start = 0;
        candidates = turns
            .iter()
            .enumerate()
            .filter(|(_, text)| is_substantive_goal(text))
            .map(|(index, text)| (index, profile.information_mass(text, key)))
            .collect();
    }
    if candidates.len() == 1 {
        return candidates.first().map(|(index, _)| *index);
    }
    if !candidates.is_empty() {
        let mut low = candidates
            .iter()
            .map(|(_, mass)| *mass)
            .fold(f32::INFINITY, f32::min);
        let mut high = candidates
            .iter()
            .map(|(_, mass)| *mass)
            .fold(f32::NEG_INFINITY, f32::max);
        if (high - low).abs() <= f32::EPSILON {
            return candidates.first().map(|(index, _)| *index);
        }
        for _ in 0..16 {
            let boundary = (low + high) / 2.0;
            let (mut low_sum, mut low_count) = (0.0, 0usize);
            let (mut high_sum, mut high_count) = (0.0, 0usize);
            for (_, mass) in &candidates {
                if *mass <= boundary {
                    low_sum += *mass;
                    low_count += 1;
                } else {
                    high_sum += *mass;
                    high_count += 1;
                }
            }
            if low_count == 0 || high_count == 0 {
                break;
            }
            let next_low = low_sum / low_count as f32;
            let next_high = high_sum / high_count as f32;
            if (next_low - low).abs() + (next_high - high).abs() <= f32::EPSILON {
                low = next_low;
                high = next_high;
                break;
            }
            low = next_low;
            high = next_high;
        }
        let boundary = (low + high) / 2.0;
        if let Some((index, _)) = candidates.iter().rev().find(|(_, mass)| *mass > boundary) {
            return Some(*index);
        }
    }
    turns
        .iter()
        .enumerate()
        .skip(search_start)
        .find(|(_, text)| !is_transcript_noise(text) && embed(text).is_some())
        .map(|(index, _)| index)
}

#[derive(Debug, Clone)]
pub struct DriftParams {
    pub eps_state: f32,
    pub tau_fid: f32,
    pub tau_fid_high: f32,
}

impl Default for DriftParams {
    fn default() -> Self {
        DriftParams {
            eps_state: 0.05,
            tau_fid: 0.45,
            tau_fid_high: 0.75,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub accept: bool,
    pub reason: &'static str,
    pub goal_align_orig: Option<f32>,
    pub goal_align_rewrite: Option<f32>,
    pub fidelity: Option<f32>,
}

fn is_flag(t: &str) -> bool {
    let b = t.as_bytes();
    (t.starts_with("--") && b.len() > 2 && (b[2].is_ascii_alphabetic()))
        || (t.starts_with('-') && b.len() >= 2 && b[1].is_ascii_alphabetic())
}

fn is_number(t: &str) -> bool {
    !t.is_empty()
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
}

fn is_pathish(t: &str) -> bool {
    if t.contains('/') {
        return true;
    }
    let parts: Vec<&str> = t.split('.').filter(|s| !s.is_empty()).collect();
    parts.len() >= 2
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && t.chars().any(|c| c.is_ascii_alphabetic())
}

fn extract_quoted(text: &str, q: char, out: &mut std::collections::BTreeSet<String>) {
    let mut open: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c == q {
            match open {
                None => open = Some(i + c.len_utf8()),
                Some(start) => {
                    let inner = &text[start..i];
                    if !inner.trim().is_empty() {
                        out.insert(inner.to_string());
                    }
                    open = None;
                }
            }
        }
    }
}

pub fn literals(text: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    extract_quoted(text, '"', &mut set);
    extract_quoted(text, '\'', &mut set);
    extract_quoted(text, '`', &mut set);
    for raw in text.split_whitespace() {
        let t = raw.trim_matches(|c: char| ",;:!?()[]{}\"'`".contains(c));
        if t.is_empty() {
            continue;
        }
        if is_flag(t) || is_number(t) || is_pathish(t) {
            set.insert(t.to_string());
        }
    }
    set
}

pub fn preserves_literals(orig: &str, rewrite: &str) -> bool {
    let want = literals(orig);
    if want.is_empty() {
        return true;
    }
    let have = literals(rewrite);
    want.iter().all(|l| have.contains(l))
}

fn calibrated_fidelity(
    orig: &str,
    rewrite: &str,
    cosine: f32,
    profile: &InformationProfile,
    key: &[u8; 32],
) -> (f32, f32) {
    let count = words(orig).len();
    if count <= 8 {
        if let Some(recall) = profile.weighted_recall(orig, rewrite, key) {
            let cosine_unit = (cosine + 1.0) / 2.0;
            let score = 0.75 * recall + 0.25 * cosine_unit;
            let floor = if count <= 4 { 0.55 } else { 0.50 };
            return (score, floor);
        }
    }
    (cosine, DriftParams::default().tau_fid)
}

fn reject(reason: &'static str) -> Verdict {
    Verdict {
        accept: false,
        reason,
        goal_align_orig: None,
        goal_align_rewrite: None,
        fidelity: None,
    }
}

pub fn accept_rewrite(
    goal: Option<&Vector>,
    orig: &str,
    rewrite: &str,
    p: &DriftParams,
) -> Verdict {
    accept_rewrite_profiled(
        goal,
        orig,
        rewrite,
        p,
        &InformationProfile::default(),
        &UNPROFILED_KEY,
    )
}

pub fn accept_rewrite_profiled(
    goal: Option<&Vector>,
    orig: &str,
    rewrite: &str,
    p: &DriftParams,
    profile: &InformationProfile,
    key: &[u8; 32],
) -> Verdict {
    let (ov, rv) = match (embed(orig), embed(rewrite)) {
        (Some(o), Some(r)) => (o, r),
        _ => return reject("degenerate embedding"),
    };
    if !preserves_literals(orig, rewrite) {
        return reject("literal dropped");
    }
    let Some(raw_fidelity) = alignment(&rv, &ov) else {
        return reject("unscorable fidelity");
    };
    let (f, calibrated_floor) = calibrated_fidelity(orig, rewrite, raw_fidelity, profile, key);
    let fidelity_floor = calibrated_floor.max(p.tau_fid);
    match goal {
        Some(g) => {
            let (Some(a_o), Some(a_r)) = (alignment(&ov, g), alignment(&rv, g)) else {
                return reject("unscorable goal alignment");
            };
            let on_goal = a_r >= a_o - p.eps_state;
            let faithful = f >= fidelity_floor;
            Verdict {
                accept: on_goal && faithful,
                reason: if !faithful {
                    "low fidelity"
                } else if !on_goal {
                    "drifts off goal"
                } else {
                    "accept"
                },
                goal_align_orig: Some(a_o),
                goal_align_rewrite: Some(a_r),
                fidelity: Some(f),
            }
        }
        None => Verdict {
            accept: f >= p.tau_fid_high,
            reason: if f >= p.tau_fid_high {
                "accept (no goal, paraphrase-only)"
            } else {
                "no goal, fidelity below strict floor"
            },
            goal_align_orig: None,
            goal_align_rewrite: None,
            fidelity: Some(f),
        },
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunningStats {
    pub count: u64,
    pub mean: f32,
    m2: f32,
}

impl RunningStats {
    pub fn push(&mut self, value: f32) {
        self.count += 1;
        let delta = value - self.mean;
        self.mean += delta / self.count as f32;
        let delta2 = value - self.mean;
        self.m2 += delta * delta2;
    }

    pub fn sigma(&self) -> f32 {
        if self.count < 2 {
            0.0
        } else {
            (self.m2 / (self.count - 1) as f32).max(0.0).sqrt()
        }
    }

    pub fn z_before_push(&self, value: f32, floor: f32) -> f32 {
        if self.count < 4 {
            0.0
        } else {
            (value - self.mean) / self.sigma().max(floor)
        }
    }
}

#[derive(Debug, Clone)]
pub struct OnlineParams {
    pub half_life: f32,
    pub sigma_floor: f32,
    pub cusum_reference: f32,
    pub cusum_threshold: f32,
    pub sign_window: usize,
}

impl Default for OnlineParams {
    fn default() -> Self {
        OnlineParams {
            half_life: 2.5,
            sigma_floor: 0.03,
            cusum_reference: 0.10,
            cusum_threshold: 5.0,
            sign_window: 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub turn: u64,
    pub deviation: f32,
    pub deviation_z: f32,
    pub state_distance: f32,
    pub coherence: f32,
    pub progress: f32,
    pub cusum: f32,
    pub stuck: bool,
    pub risk: f32,
}

#[derive(Debug, Clone)]
pub struct Tracker {
    goal: Vector,
    acc: [f32; SIM_BITS],
    state: Vector,
    prev_state_distance: f32,
    deviation: RunningStats,
    cusum: f32,
    signs: VecDeque<bool>,
    turns: u64,
    params: OnlineParams,
}

fn sim_vote(sig: &[u64; SIM_WORDS], index: usize) -> f32 {
    if (sig[index / 64] >> (index % 64)) & 1 == 1 {
        1.0
    } else {
        -1.0
    }
}

fn threshold_acc(acc: &[f32; SIM_BITS]) -> [u64; SIM_WORDS] {
    let mut sig = [0u64; SIM_WORDS];
    for (i, value) in acc.iter().enumerate() {
        if *value > 0.0 {
            sig[i / 64] |= 1u64 << (i % 64);
        }
    }
    sig
}

fn sigmoid(value: f32) -> f32 {
    1.0 / (1.0 + (-value.clamp(-20.0, 20.0)).exp())
}

pub fn noisy_or(channels: &[(f32, f32)]) -> f32 {
    1.0 - channels.iter().fold(1.0, |product, (probability, weight)| {
        product * (1.0 - probability.clamp(0.0, 1.0) * weight.clamp(0.0, 1.0))
    })
}

impl Tracker {
    pub fn new(goal: Vector, params: OnlineParams) -> Option<Self> {
        let Vector::Sim(sig) = &goal else {
            return None;
        };
        let mut acc = [0f32; SIM_BITS];
        for (i, value) in acc.iter_mut().enumerate() {
            *value = sim_vote(sig, i);
        }
        Some(Tracker {
            state: goal.clone(),
            goal,
            acc,
            prev_state_distance: 0.0,
            deviation: RunningStats::default(),
            cusum: 0.0,
            signs: VecDeque::new(),
            turns: 0,
            params,
        })
    }

    pub fn observe(&mut self, text: &str) -> Option<Observation> {
        let current = embed(text)?;
        let Vector::Sim(sig) = &current else {
            return None;
        };
        let deviation = distance(&current, &self.goal)?;
        let deviation_z = self
            .deviation
            .z_before_push(deviation, self.params.sigma_floor)
            .clamp(-8.0, 8.0);
        self.deviation.push(deviation);

        let alpha = 1.0 - 2f32.powf(-1.0 / self.params.half_life.max(0.1));
        for (i, value) in self.acc.iter_mut().enumerate() {
            *value = alpha * sim_vote(sig, i) + (1.0 - alpha) * *value;
        }
        self.state = Vector::Sim(threshold_acc(&self.acc));
        let state_distance = distance(&self.state, &self.goal)?;
        let coherence = self.acc.iter().map(|value| value.abs()).sum::<f32>() / SIM_BITS as f32;
        let scale = self.deviation.sigma().max(self.params.sigma_floor);
        let progress = ((self.prev_state_distance - state_distance) / scale).clamp(-1.0, 1.0);
        let reliability = (0.5 + 0.5 * coherence).clamp(0.0, 1.0);
        self.cusum = (self.cusum + reliability * (self.params.cusum_reference - progress)).max(0.0);
        if progress.abs() >= 0.05 {
            self.signs.push_back(progress > 0.0);
            while self.signs.len() > self.params.sign_window {
                self.signs.pop_front();
            }
        }
        self.turns += 1;
        let positives = self.signs.iter().filter(|positive| **positive).count();
        let sign_agrees = self.signs.len() < self.params.sign_window || positives <= 1;
        let stuck = self.turns >= 6 && self.cusum >= self.params.cusum_threshold && sign_agrees;
        let deviation_probability = if self.deviation.count >= 6 {
            sigmoid(1.5 * (deviation_z - 1.5))
        } else {
            0.0
        };
        let stuck_probability = if stuck {
            1.0
        } else {
            1.0 - (-self.cusum / self.params.cusum_threshold.max(0.1)).exp()
        };
        let risk = noisy_or(&[(deviation_probability, 0.55), (stuck_probability, 0.80)]);
        self.prev_state_distance = state_distance;
        Some(Observation {
            turn: self.turns,
            deviation,
            deviation_z,
            state_distance,
            coherence,
            progress,
            cusum: self.cusum,
            stuck,
            risk,
        })
    }

    pub fn state(&self) -> &Vector {
        &self.state
    }

    pub fn deviation_stats(&self) -> &RunningStats {
        &self.deviation
    }
}

#[derive(Debug, Clone)]
pub struct SessionAnalysis {
    pub goal_index: usize,
    pub observed_turns: u64,
    pub deviation_mean: f32,
    pub deviation_sigma: f32,
    pub last: Option<Observation>,
    pub state: Vector,
}

pub fn analyze_turns(turns: &[String]) -> Option<SessionAnalysis> {
    let goal_index = select_goal_index(turns)?;
    analyze_turns_from_goal_index(turns, goal_index)
}

pub fn analyze_turns_profiled(
    turns: &[String],
    profile: &InformationProfile,
    key: &[u8; 32],
) -> Option<SessionAnalysis> {
    let goal_index = select_goal_index_profiled(turns, profile, key)?;
    analyze_turns_from_goal_index(turns, goal_index)
}

pub fn analyze_turns_from_goal_index(
    turns: &[String],
    goal_index: usize,
) -> Option<SessionAnalysis> {
    if goal_index >= turns.len() || is_transcript_noise(&turns[goal_index]) {
        return None;
    }
    let goal = embed(&turns[goal_index])?;
    let mut tracker = Tracker::new(goal, OnlineParams::default())?;
    let mut last = None;
    let mut previous: Option<&str> = None;
    for turn in turns.iter().skip(goal_index + 1) {
        if is_transcript_noise(turn) || previous == Some(turn.trim()) {
            continue;
        }
        previous = Some(turn.trim());
        if let Some(observation) = tracker.observe(turn) {
            last = Some(observation);
        }
    }
    Some(SessionAnalysis {
        goal_index,
        observed_turns: tracker.turns + 1,
        deviation_mean: tracker.deviation_stats().mean,
        deviation_sigma: tracker.deviation_stats().sigma(),
        last,
        state: tracker.state().clone(),
    })
}

pub fn combine_risk(base: f32, problem_similarity: f32) -> f32 {
    noisy_or(&[(base, 1.0), (problem_similarity, 0.65)])
}

pub fn choose_rewrite<'a>(
    enabled: bool,
    goal: Option<&Vector>,
    original: &'a str,
    candidate: &'a str,
    params: &DriftParams,
    risk: f32,
) -> (&'a str, Option<Verdict>) {
    choose_rewrite_profiled(
        enabled,
        goal,
        original,
        candidate,
        params,
        risk,
        (&InformationProfile::default(), &UNPROFILED_KEY),
    )
}

pub fn choose_rewrite_profiled<'a>(
    enabled: bool,
    goal: Option<&Vector>,
    original: &'a str,
    candidate: &'a str,
    params: &DriftParams,
    risk: f32,
    information: (&InformationProfile, &[u8; 32]),
) -> (&'a str, Option<Verdict>) {
    let Some(goal) = goal.filter(|_| enabled) else {
        return (candidate, None);
    };
    let mut adjusted = params.clone();
    adjusted.tau_fid = (adjusted.tau_fid + 0.20 * risk.clamp(0.0, 1.0)).min(0.72);
    let verdict = accept_rewrite_profiled(
        Some(goal),
        original,
        candidate,
        &adjusted,
        information.0,
        information.1,
    );
    let selected = if verdict.accept { candidate } else { original };
    (selected, Some(verdict))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim(v: &Vector) -> [u64; SIM_WORDS] {
        match v {
            Vector::Sim(s) => *s,
            _ => panic!("not sim"),
        }
    }

    #[test]
    fn embed_is_deterministic_and_zero_self_distance() {
        let a = embed("fix the login bug in the auth flow").unwrap();
        let b = embed("fix the login bug in the auth flow").unwrap();
        assert_eq!(sim(&a), sim(&b));
        assert_eq!(distance(&a, &b), Some(0.0));
        assert!((alignment(&a, &b).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn empty_or_tiny_text_is_none() {
        assert!(embed("").is_none());
        assert!(embed("  \n ").is_none());
    }

    #[test]
    fn distance_is_symmetric_and_in_unit_range() {
        let a = embed("add a flowchart export to png").unwrap();
        let b = embed("write unit tests for the parser").unwrap();
        let d1 = distance(&a, &b).unwrap();
        let d2 = distance(&b, &a).unwrap();
        assert!((d1 - d2).abs() < 1e-9);
        assert!((0.0..=1.0).contains(&d1));
    }

    #[test]
    fn similar_text_is_closer_than_unrelated() {
        let goal = embed("implement a flowchart diagram editor with drag and drop nodes").unwrap();
        let near = embed("add drag and drop nodes to the flowchart editor").unwrap();
        let far = embed("upgrade the postgres database migration scripts").unwrap();
        let d_near = distance(&goal, &near).unwrap();
        let d_far = distance(&goal, &far).unwrap();
        assert!(d_near < d_far, "near {d_near} should be < far {d_far}");
    }

    #[test]
    fn character_shingles_work_without_word_boundaries() {
        let goal = embed("修复导出报告中的数据库类型错误").unwrap();
        let near = embed("修复导出报告的类型错误并验证结果").unwrap();
        let far = embed("重新设计地图上的地形渲染和相机控制").unwrap();
        assert!(distance(&goal, &near).unwrap() < distance(&goal, &far).unwrap());
    }

    #[test]
    fn dense_backend_shares_the_angular_scale() {
        let same = Vector::Dense(vec![1.0, 0.0, 0.0]);
        let same2 = Vector::Dense(vec![2.0, 0.0, 0.0]);
        let orth = Vector::Dense(vec![0.0, 1.0, 0.0]);
        let opp = Vector::Dense(vec![-1.0, 0.0, 0.0]);
        assert!(distance(&same, &same2).unwrap() < 1e-4);
        assert!((distance(&same, &orth).unwrap() - 0.5).abs() < 1e-4);
        assert!((distance(&same, &opp).unwrap() - 1.0).abs() < 1e-4);
        assert!((alignment(&same, &orth).unwrap()).abs() < 1e-4);
    }

    #[test]
    fn dense_arcsin_and_arccos_forms_agree_near_one() {
        for k in 0..90 {
            let theta = (k as f32) * std::f32::consts::PI / 180.0;
            let a = Vector::Dense(vec![1.0, 0.0]);
            let b = Vector::Dense(vec![theta.cos(), theta.sin()]);
            let d = distance(&a, &b).unwrap();
            let expect = theta / std::f32::consts::PI;
            assert!((d - expect).abs() < 1e-4, "theta {theta}: {d} vs {expect}");
        }
    }

    #[test]
    fn degenerate_dense_is_none_never_nan() {
        let zero = Vector::Dense(vec![0.0, 0.0, 0.0]);
        let ok = Vector::Dense(vec![1.0, 0.0, 0.0]);
        assert!(distance(&zero, &ok).is_none());
        assert!(alignment(&zero, &ok).is_none());
    }

    #[test]
    fn mixed_backends_refuse_comparison() {
        let s = embed("hello world this is a test").unwrap();
        let d = Vector::Dense(vec![1.0, 0.0, 0.0]);
        assert!(distance(&s, &d).is_none());
    }

    #[test]
    fn literals_extraction() {
        let l = literals("run tests in src/parser.rs with --release at line 42 and \"exact msg\"");
        assert!(l.contains("src/parser.rs"));
        assert!(l.contains("--release"));
        assert!(l.contains("42"));
        assert!(l.contains("exact msg"));
        assert!(!l.contains("run"));
    }

    #[test]
    fn preserves_literals_detects_dropped_specifics() {
        assert!(preserves_literals(
            "run the tests in src/parser.rs with --release",
            "carefully run the unit tests in src/parser.rs with the --release flag"
        ));
        assert!(!preserves_literals(
            "run the tests in src/parser.rs with --release",
            "run the tests in release mode"
        ));
        assert!(preserves_literals(
            "no specifics here",
            "a totally reworded prompt"
        ));
    }

    #[test]
    fn drift_guard_accepts_on_goal_reword() {
        let goal =
            embed("implement a flowchart diagram editor with drag and drop nodes and svg export")
                .unwrap();
        let orig = "make the nodes draggable";
        let rewrite = "make the flowchart nodes draggable with drag and drop in the editor";
        let v = accept_rewrite(Some(&goal), orig, rewrite, &DriftParams::default());
        assert!(v.accept, "{v:?}");
    }

    #[test]
    fn drift_guard_rejects_off_goal_rewrite() {
        let goal =
            embed("implement a flowchart diagram editor with drag and drop nodes and svg export")
                .unwrap();
        let orig = "make the nodes draggable";
        let rewrite = "delete the postgres database and drop all the tables now";
        let v = accept_rewrite(Some(&goal), orig, rewrite, &DriftParams::default());
        assert!(!v.accept, "{v:?}");
    }

    #[test]
    fn drift_guard_rejects_dropped_literal() {
        let goal = embed("run the test suite and fix failures in the parser").unwrap();
        let orig = "run the tests in src/parser.rs with --release";
        let rewrite = "run the tests in release mode to check the parser thoroughly";
        let v = accept_rewrite(Some(&goal), orig, rewrite, &DriftParams::default());
        assert!(!v.accept);
        assert_eq!(v.reason, "literal dropped");
    }

    #[test]
    fn drift_guard_no_goal_allows_paraphrase_blocks_drift() {
        let orig = "please fix the login bug it doesnt work when i sign in";
        let para = "please fix the login bug, it does not work when i sign in";
        let drift =
            "please investigate the entire authentication subsystem and refactor everything";
        let p = DriftParams::default();
        assert!(accept_rewrite(None, orig, para, &p).accept);
        assert!(!accept_rewrite(None, orig, drift, &p).accept);
    }

    #[test]
    fn drift_guard_rejects_degenerate() {
        let goal = embed("some clear goal about the parser").unwrap();
        let v = accept_rewrite(Some(&goal), "", "anything", &DriftParams::default());
        assert!(!v.accept);
        assert_eq!(v.reason, "degenerate embedding");
    }

    #[test]
    fn simhash_hamming_tracks_angle_unbiased() {
        let base = embed("build a small flowchart tool that exports diagrams to svg and png files")
            .unwrap();
        let mut total = 0.0f32;
        let n = 40;
        for i in 0..n {
            let other = embed(&format!(
                "build a small flowchart tool that exports diagrams to svg and png files edit {i}"
            ))
            .unwrap();
            total += distance(&base, &other).unwrap();
        }
        let avg = total / n as f32;
        assert!(avg < 0.25, "near-duplicate avg distance too high: {avg}");
    }

    #[test]
    fn fallback_uses_1024_bits() {
        assert_eq!(SIM_BITS, 1024);
        assert_eq!(
            sim(&embed("a substantive prompt for the parser").unwrap()).len(),
            16
        );
    }

    #[test]
    fn goal_selection_skips_compaction_and_tool_noise() {
        let turns = vec![
            "This session is being continued from a previous conversation".to_string(),
            "<task-notification>finished</task-notification>".to_string(),
            "/Users/example/project".to_string(),
            "Build a flowchart editor with draggable nodes and SVG export".to_string(),
        ];
        assert_eq!(select_goal_index(&turns), Some(3));
    }

    #[test]
    fn health_steer_is_not_selected_as_a_user_goal() {
        let turns = vec![
            "Build the report export and prove it against the production database".to_string(),
            "[VSC_RELAY_HEALTH_STEER v1 id=abc] verify missing production evidence".to_string(),
        ];
        assert!(is_transcript_noise(&turns[1]));
        assert_eq!(select_goal_index(&turns), Some(0));
    }

    #[test]
    fn goal_selection_uses_latest_sustained_pivot() {
        let turns = vec![
            "This session is being continued from a previous conversation".to_string(),
            "Build the automation gearbox with providers, retries, interfaces, model selection, and robust local verification across every supported operating system".to_string(),
            "continue".to_string(),
            "Design the complete smart dossier with frozen goals, accumulated facts, vector progress, risk signals, persistence, and strict optionality for the base relay".to_string(),
            "write the detailed report".to_string(),
        ];
        assert_eq!(select_goal_index(&turns), Some(3));
    }

    #[test]
    fn authoritative_goal_holds_first_substantive_turn_not_late_drift() {
        let turns = vec![
            "This session is being continued from a previous conversation".to_string(),
            "Build the automation gearbox with providers, retries, interfaces, model selection, and robust local verification across every supported operating system".to_string(),
            "continue".to_string(),
            "Design the complete smart dossier with frozen goals, accumulated facts, vector progress, risk signals, persistence, and strict optionality for the base relay".to_string(),
            "write the detailed report".to_string(),
        ];
        assert_eq!(select_goal_index(&turns), Some(3));
        assert_eq!(select_goal_index_authoritative(&turns), Some(1));
    }

    #[test]
    fn goal_selection_has_no_minimum_prompt_length_or_language_list() {
        for turns in [
            vec!["fix parser".to_string(), "go".to_string()],
            vec!["чинить parser".to_string(), "да".to_string()],
            vec!["parser түзет".to_string(), "иә".to_string()],
            vec!["修复解析器".to_string(), "继续".to_string()],
        ] {
            assert_eq!(select_goal_index(&turns), Some(0), "{turns:?}");
        }
        assert_eq!(select_goal_index(&["fix".to_string()]), Some(0));
    }

    #[test]
    fn short_prompt_expansion_uses_weighted_signal_fidelity() {
        let goal = embed("build and test the deployment process for the local service").unwrap();
        let verdict = accept_rewrite(
            Some(&goal),
            "build and run pm2",
            "build the service and run it through pm2 with local verification",
            &DriftParams::default(),
        );
        assert!(verdict.accept, "{verdict:?}");
    }

    #[test]
    fn optionality_and_double_off_are_byte_identical() {
        let original = "do the task exactly";
        let candidate = "do the task exactly and verify the result";
        let params = DriftParams::default();
        let (disabled, verdict) = choose_rewrite(false, None, original, candidate, &params, 1.0);
        assert_eq!(disabled.as_bytes(), candidate.as_bytes());
        assert!(verdict.is_none());
        let (degraded, verdict) = choose_rewrite(true, None, original, candidate, &params, 1.0);
        assert_eq!(degraded.as_bytes(), candidate.as_bytes());
        assert!(verdict.is_none());
    }

    #[test]
    fn running_stats_standardize_against_prior_session_data() {
        let mut stats = RunningStats::default();
        for value in [0.40, 0.42, 0.38, 0.41, 0.39, 0.40] {
            stats.push(value);
        }
        assert!((stats.mean - 0.40).abs() < 0.01);
        assert!(stats.z_before_push(0.55, 0.01) > 5.0);
    }

    #[test]
    fn tracker_produces_finite_relative_signals() {
        let goal = embed("implement a flowchart editor with nodes edges and svg export").unwrap();
        let mut tracker = Tracker::new(goal, OnlineParams::default()).unwrap();
        let prompts = [
            "add draggable nodes to the flowchart editor",
            "connect the nodes with directed edges",
            "write svg export for the completed diagram",
            "verify the flowchart parser with unit tests",
            "show validation errors beside invalid nodes",
            "save the current diagram to a local file",
        ];
        let mut last = None;
        for prompt in prompts {
            last = tracker.observe(prompt);
        }
        let observation = last.unwrap();
        assert!(observation.deviation_z.is_finite());
        assert!(observation.progress.is_finite());
        assert!((0.0..=1.0).contains(&observation.risk));
        assert_eq!(tracker.deviation_stats().count, 6);
    }

    #[test]
    fn noisy_or_is_bounded_and_monotone() {
        let low = noisy_or(&[(0.2, 0.5), (0.1, 0.8)]);
        let high = noisy_or(&[(0.8, 0.5), (0.1, 0.8)]);
        assert!((0.0..=1.0).contains(&low));
        assert!((0.0..=1.0).contains(&high));
        assert!(high > low);
    }

    #[test]
    fn analyze_turns_uses_frozen_substantive_goal() {
        let turns = vec![
            "This session is being continued from another context".to_string(),
            "Create a parser that converts text into a tested flowchart model".to_string(),
            "add node validation".to_string(),
            "add edge validation".to_string(),
        ];
        let analysis = analyze_turns(&turns).unwrap();
        assert_eq!(analysis.goal_index, 1);
        assert_eq!(analysis.observed_turns, 3);
        assert_eq!(analysis.last.unwrap().turn, 2);
    }

    #[test]
    fn explicit_goal_index_cannot_be_replaced_by_a_later_long_correction() {
        let turns = vec![
            "Audit every report column against the source XML and real filled reports, remove hard-coded thresholds, and verify the tonnage workflow end to end".to_string(),
            "continue".to_string(),
            "You missed the central requirement: search every mounted data source, compare each XML field with every report column, recover every formula, expose every threshold through managed settings, and validate the existing tonnage import tool".to_string(),
        ];
        assert_eq!(select_goal_index(&turns), Some(2));

        let analysis = analyze_turns_from_goal_index(&turns, 0).unwrap();
        assert_eq!(analysis.goal_index, 0);
        assert_eq!(analysis.observed_turns, 3);
    }

    #[test]
    fn explicit_goal_index_preserves_a_short_valid_contract() {
        let turns = vec!["fix parser.rs".to_string(), "continue".to_string()];
        let analysis = analyze_turns_from_goal_index(&turns, 0).unwrap();
        assert_eq!(analysis.goal_index, 0);
        assert_eq!(analysis.observed_turns, 2);
    }
}
