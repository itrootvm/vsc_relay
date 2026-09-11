use relay_adapters::claude::semantic_steps;
use relay_compass::health::SessionPhase;
use relay_compass::ledger::contract_input_from_steps;
use relay_compass::predictive::PredictiveParams;
use relay_compass::staged::{assemble_and_evaluate, semantic_inputs, StagedContext};
use relay_compass::{
    embed, literals, OnlineParams, SemanticFacts, SemanticStep, StepRole, Tracker, UserOrigin,
};
use relay_semantic::classify;
use relay_semantic::config::SemanticConfig;
use std::collections::BTreeSet;
use std::path::Path;

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "you", "your", "are", "can", "not", "but", "all",
    "from", "into", "was", "were", "has", "have", "will", "would", "should", "make", "made", "use",
];

fn is_human_user(step: &SemanticStep) -> bool {
    step.role == StepRole::User && step.user_origin == UserOrigin::Human
}

fn keywords(text: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            token.push(ch.to_ascii_lowercase());
        } else {
            if token.len() >= 3 && !STOPWORDS.contains(&token.as_str()) {
                set.insert(token.clone());
            }
            token.clear();
        }
    }
    if token.len() >= 3 && !STOPWORDS.contains(&token.as_str()) {
        set.insert(token);
    }
    set
}

fn jstr(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

fn ctx_for<'a>(
    anchor: &'a str,
    kw: &'a BTreeSet<String>,
    lit: &'a BTreeSet<String>,
    obs: Option<&'a relay_compass::Observation>,
) -> StagedContext<'a> {
    StagedContext {
        contract_text: anchor,
        goal_keywords: kw,
        goal_literals: lit,
        observation: obs,
        deviation_mean: 0.45,
        deviation_sigma: 0.05,
        phase: SessionPhase::Idle,
        pending_question: false,
        previous_steers: 0,
        same_signature_recent: false,
        continuation_seeds: &[],
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: corpus_emit_semantic <transcript.jsonl> [cli] [model]");
    let cli = args.next().unwrap_or_else(|| "cursor".to_string());
    let model = args.next();
    let session_id = Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut config = SemanticConfig::default();
    config.apply_preset(&cli).expect("preset");
    config.cli_model = model.filter(|m| !m.is_empty());
    config.timeout_secs = 120;

    let steps = semantic_steps(Path::new(&path));
    let params = PredictiveParams::default();

    let input = contract_input_from_steps(&steps);
    let anchor_text = input
        .anchor_text
        .clone()
        .or_else(|| {
            steps
                .iter()
                .find(|s| is_human_user(s))
                .map(|s| s.text.clone())
        })
        .unwrap_or_default();
    let kw = keywords(&anchor_text);
    let lit = literals(&anchor_text);

    let mut tracker = embed(&anchor_text).and_then(|g| Tracker::new(g, OnlineParams::default()));
    let mut last_obs = None;
    for s in steps.iter().filter(|s| is_human_user(s)) {
        last_obs = tracker.as_mut().and_then(|tr| tr.observe(&s.text));
    }

    let ctx = ctx_for(&anchor_text, &kw, &lit, last_obs.as_ref());
    let det = assemble_and_evaluate(&steps, &[], &ctx, &params);

    let frames = semantic_inputs(&steps, &anchor_text);
    let n_frames = frames.len();
    let facts: Vec<SemanticFacts> = match classify(&config, frames, None).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("classify error ({session_id}): {e:#}");
            Vec::new()
        }
    };
    let classified = !facts.is_empty();
    let ctx2 = ctx_for(&anchor_text, &kw, &lit, last_obs.as_ref());
    let sem = assemble_and_evaluate(&steps, &facts, &ctx2, &params);

    let dec = |o: &Option<relay_compass::staged::StagedOutput>| -> (String, f32, f32, f32) {
        match o {
            Some(x) => (
                format!("{:?}", x.decision.action),
                x.decision.gap_posterior,
                x.scope_breadth,
                x.proof_resolvability,
            ),
            None => ("None".into(), 0.0, 0.0, 0.0),
        }
    };
    let (da, dp, dscope, dproof) = dec(&det);
    let (sa, sp, sscope, sproof) = dec(&sem);
    let facts_json = serde_json::to_string(&facts).unwrap_or_else(|_| "[]".to_string());

    println!(
        "{{\"session_id\":{sid},\"cli\":{cli},\"model\":{model},\"classified\":{classified},\"n_frames\":{nf},\"det\":{{\"action\":{da},\"p_gap\":{dp:.4},\"scope_breadth\":{dscope:.4},\"proof_resolvability\":{dproof:.4}}},\"sem\":{{\"action\":{sa},\"p_gap\":{sp:.4},\"scope_breadth\":{sscope:.4},\"proof_resolvability\":{sproof:.4}}},\"facts\":{facts}}}",
        sid = jstr(&session_id),
        cli = jstr(&cli),
        model = jstr(config.cli_model.as_deref().unwrap_or("account-default")),
        nf = n_frames,
        da = jstr(&da),
        sa = jstr(&sa),
        facts = facts_json,
    );
}
