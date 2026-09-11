use relay_adapters::claude::semantic_steps;
use relay_compass::health::SessionPhase;
use relay_compass::ledger::contract_input_from_steps;
use relay_compass::predictive::PredictiveParams;
use relay_compass::staged::{assemble_and_evaluate, StagedContext};
use relay_compass::{
    embed, literals, select_goal_index, OnlineParams, SemanticStep, StepRole, ToolKind, Tracker,
    UserOrigin,
};
use std::collections::BTreeSet;
use std::path::Path;

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "you", "your", "are", "can", "not", "but", "all",
    "from", "into", "was", "were", "has", "have", "will", "would", "should", "make", "made", "use",
    "using", "used", "get", "got", "let", "its", "our", "out", "now", "any", "add", "set", "run",
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

fn trunc(text: &str, n: usize) -> String {
    text.replace('\n', " ").chars().take(n).collect()
}

fn norm_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .take(48)
        .collect()
}

fn jstr(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: corpus_emit <transcript.jsonl>");
    let session_id = Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let steps = semantic_steps(Path::new(&path));
    let params = PredictiveParams::default();

    let hu_pos: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| is_human_user(s))
        .map(|(i, _)| i)
        .collect();
    let m = hu_pos.len();
    let user_texts: Vec<String> = hu_pos.iter().map(|&i| steps[i].text.clone()).collect();

    let input = contract_input_from_steps(&steps);
    let anchor_text = input
        .anchor_text
        .clone()
        .or_else(|| user_texts.first().cloned());

    let late_idx = select_goal_index(&user_texts);
    let auth_ordinal = input
        .anchor
        .as_ref()
        .and_then(|a| hu_pos.iter().position(|&i| steps[i].index == a.source_step));

    let frac = |idx: Option<usize>| -> f64 {
        match (idx, m) {
            (Some(i), n) if n > 0 => (n - i) as f64 / n as f64,
            _ => 0.0,
        }
    };

    let goal_kw = anchor_text.as_deref().map(keywords).unwrap_or_default();
    let goal_lit = anchor_text.as_deref().map(literals).unwrap_or_default();

    let mut tracker = anchor_text
        .as_deref()
        .and_then(embed)
        .and_then(|g| Tracker::new(g, OnlineParams::default()));

    let mut turn_rows: Vec<String> = Vec::new();
    let mut final_action = String::from("None");
    let mut final_pgap = 0.0f32;
    let mut final_scope = 0.0f32;
    let mut final_proof = 0.0f32;
    let mut final_det_feedback = false;

    for (t, &p) in hu_pos.iter().enumerate() {
        let text = &steps[p].text;
        let obs = tracker.as_mut().and_then(|tr| tr.observe(text));

        let end = if t + 1 < hu_pos.len() {
            hu_pos[t + 1]
        } else {
            steps.len()
        };
        let window = &steps[0..end];
        let episode = &steps[p..end];
        let errs_ep = episode
            .iter()
            .filter(|s| s.role == StepRole::ToolResult && s.is_error)
            .count();
        let errs_cum = window
            .iter()
            .filter(|s| s.role == StepRole::ToolResult && s.is_error)
            .count();
        let tools_ep = episode
            .iter()
            .filter(|s| s.role == StepRole::ToolUse)
            .count();
        let kind_count = |k: ToolKind| {
            episode
                .iter()
                .filter(|s| s.role == StepRole::ToolUse && s.tool_kind == k)
                .count()
        };
        let (k_ins, k_sea, k_exe, k_mod, k_del) = (
            kind_count(ToolKind::Inspect),
            kind_count(ToolKind::Search),
            kind_count(ToolKind::Execute),
            kind_count(ToolKind::Modify),
            kind_count(ToolKind::Delegate),
        );
        let ctx = StagedContext {
            contract_text: anchor_text.as_deref().unwrap_or(""),
            goal_keywords: &goal_kw,
            goal_literals: &goal_lit,
            observation: obs.as_ref(),
            deviation_mean: 0.45,
            deviation_sigma: 0.05,
            phase: SessionPhase::Idle,
            pending_question: false,
            previous_steers: 0,
            same_signature_recent: false,
            continuation_seeds: &[],
        };
        let out = assemble_and_evaluate(window, &[], &ctx, &params);

        let (run_mean, run_sigma) = tracker
            .as_ref()
            .map(|tr| {
                let st = tr.deviation_stats();
                if st.count >= 4 {
                    (st.mean, st.sigma().max(0.03))
                } else {
                    (0.45, 0.05)
                }
            })
            .unwrap_or((0.45, 0.05));
        let ctx_rs = StagedContext {
            contract_text: anchor_text.as_deref().unwrap_or(""),
            goal_keywords: &goal_kw,
            goal_literals: &goal_lit,
            observation: obs.as_ref(),
            deviation_mean: run_mean,
            deviation_sigma: run_sigma,
            phase: SessionPhase::Idle,
            pending_question: false,
            previous_steers: 0,
            same_signature_recent: false,
            continuation_seeds: &[],
        };
        let out_rs = assemble_and_evaluate(window, &[], &ctx_rs, &params);

        let (action, pgap, dominant, scope, proof, corrob) = match &out {
            Some(o) => (
                format!("{:?}", o.decision.action),
                o.decision.gap_posterior,
                format!("{:?}", o.decision.dominant_factor),
                o.scope_breadth,
                o.proof_resolvability,
                o.decision.corroborating_sources,
            ),
            None => ("None".into(), 0.0, "None".into(), 0.0, 0.0, 0),
        };
        let post = match &out {
            Some(o) => {
                let p = o.decision.posterior;
                format!(
                    "[{:.4},{:.4},{:.4},{:.4},{:.4}]",
                    p[0], p[1], p[2], p[3], p[4]
                )
            }
            None => "[0,0,0,0,0]".to_string(),
        };
        if let Some(o) = &out {
            final_action = format!("{:?}", o.decision.action);
            final_pgap = o.decision.gap_posterior;
            final_scope = o.scope_breadth;
            final_proof = o.proof_resolvability;
            final_det_feedback = o.deterministic_feedback;
        }

        let (d, dstate, z, cusum, coh, prog, stuck, riskg) = match &obs {
            Some(o) => (
                o.deviation,
                o.state_distance,
                o.deviation_z,
                o.cusum,
                o.coherence,
                o.progress,
                o.stuck,
                o.risk,
            ),
            None => (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false, 0.0),
        };

        let (sig, prov) = match &out {
            Some(o) => (o.ledger.signals, o.ledger.provenance),
            None => (Default::default(), Default::default()),
        };
        let (pgap_rs, action_rs, post_rs) = match &out_rs {
            Some(o) => {
                let pr = o.decision.posterior;
                (
                    o.decision.gap_posterior,
                    format!("{:?}", o.decision.action),
                    format!(
                        "[{:.4},{:.4},{:.4},{:.4},{:.4}]",
                        pr[0], pr[1], pr[2], pr[3], pr[4]
                    ),
                )
            }
            None => (0.0, "None".into(), "[0,0,0,0,0]".into()),
        };

        turn_rows.push(format!(
            "{{\"t\":{t},\"step\":{step},\"k\":{k},\"D\":{d:.4},\"d_state\":{dstate:.4},\"z\":{z:.4},\"cusum\":{cusum:.4},\"coherence\":{coh:.4},\"progress\":{prog:.4},\"stuck\":{stuck},\"risk_geo\":{riskg:.4},\"p_gap\":{pgap:.4},\"posterior\":{post},\"action\":{action},\"dominant\":{dominant},\"scope_breadth\":{scope:.4},\"proof_resolvability\":{proof:.4},\"corroborating_sources\":{corrob},\"errs_ep\":{errs_ep},\"errs_cum\":{errs_cum},\"tools_ep\":{tools_ep},\"k_inspect\":{k_ins},\"k_search\":{k_sea},\"k_execute\":{k_exe},\"k_modify\":{k_mod},\"k_delegate\":{k_del},\"sig_active\":{sa},\"sig_open\":{so},\"sig_claimed\":{sc},\"sig_verified\":{sv},\"sig_contradicted\":{scon},\"sig_disputed\":{sd},\"sig_stale\":{ss},\"sig_layer_mismatches\":{slm},\"sig_coverage\":{scov:.4},\"sig_scope_gap\":{ssg},\"sig_proof_deficit\":{spd},\"prov_tool_uses\":{ptu},\"prov_weak_anchors\":{pwa},\"prov_unmatched\":{pun},\"prov_weak_paired\":{pwp},\"prov_delegate_reports\":{pdr},\"run_mean\":{rm:.4},\"run_sigma\":{rs:.4},\"p_gap_rs\":{pgr:.4},\"posterior_rs\":{postrs},\"action_rs\":{actrs}}}",
            step = steps[p].index,
            k = jstr(&norm_key(&steps[p].text)),
            post = post,
            action = jstr(&action),
            dominant = jstr(&dominant),
            sa = sig.active,
            so = sig.open,
            sc = sig.claimed_unverified,
            sv = sig.verified,
            scon = sig.contradicted,
            sd = sig.disputed,
            ss = sig.stale,
            slm = sig.layer_mismatches,
            scov = sig.coverage,
            ssg = sig.completion_scope_gap,
            spd = sig.proof_deficit,
            ptu = prov.tool_uses,
            pwa = prov.weak_anchors,
            pun = prov.unmatched_results,
            pwp = prov.weakly_paired_results,
            pdr = prov.delegate_reports,
            rm = run_mean,
            rs = run_sigma,
            pgr = pgap_rs,
            postrs = post_rs,
            actrs = jstr(&action_rs),
        ));
    }

    let late_txt = late_idx
        .and_then(|i| user_texts.get(i))
        .map(|s| trunc(s, 160))
        .unwrap_or_default();
    let auth_txt = anchor_text
        .as_deref()
        .map(|s| trunc(s, 160))
        .unwrap_or_default();
    let agree = match (late_idx, auth_ordinal) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };

    println!(
        "{{\"session_id\":{sid},\"n_steps\":{ns},\"n_human_user_turns\":{m},\"has_anchor\":{ha},\"anchor\":{{\"late_index\":{li},\"late_retained_frac\":{lf:.4},\"late_text\":{lt},\"auth_ordinal\":{ao},\"auth_step\":{as_},\"auth_retained_frac\":{af:.4},\"auth_text\":{at},\"agree\":{agree}}},\"final\":{{\"action\":{fa},\"p_gap\":{fp:.4},\"scope_breadth\":{fs:.4},\"proof_resolvability\":{fpr:.4},\"deterministic_feedback\":{fdf}}},\"turns\":[{turns}]}}",
        sid = jstr(&session_id),
        ns = steps.len(),
        ha = input.anchor.is_some(),
        li = late_idx.map(|i| i as i64).unwrap_or(-1),
        lf = frac(late_idx),
        lt = jstr(&late_txt),
        ao = auth_ordinal.map(|i| i as i64).unwrap_or(-1),
        as_ = input.anchor.as_ref().map(|a| a.source_step as i64).unwrap_or(-1),
        af = frac(auth_ordinal),
        at = jstr(&auth_txt),
        fa = jstr(&final_action),
        fp = final_pgap,
        fs = final_scope,
        fpr = final_proof,
        fdf = final_det_feedback,
        turns = turn_rows.join(","),
    );
}
