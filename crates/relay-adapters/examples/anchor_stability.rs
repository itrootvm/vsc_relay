use relay_adapters::claude::semantic_steps;
use relay_compass::ledger::contract_input_from_steps;
use relay_compass::{select_goal_index, SemanticStep, StepRole, UserOrigin};
use std::path::Path;

fn is_human_user(step: &SemanticStep) -> bool {
    step.role == StepRole::User && step.user_origin == UserOrigin::Human
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: anchor_stability <transcript.jsonl>");
    let session_id = Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let steps = semantic_steps(Path::new(&path));
    let hu: Vec<(u32, String)> = steps
        .iter()
        .filter(|s| is_human_user(s))
        .map(|s| (s.index, s.text.clone()))
        .collect();
    let n = hu.len();

    let input = contract_input_from_steps(&steps);
    let auth_ordinal = input
        .anchor
        .as_ref()
        .and_then(|a| hu.iter().position(|(idx, _)| *idx == a.source_step));

    let texts: Vec<String> = hu.iter().map(|(_, t)| t.clone()).collect();

    let mut traj: Vec<i64> = Vec::new();
    for t in 1..=n {
        let idx = select_goal_index(&texts[0..t])
            .map(|i| i as i64)
            .unwrap_or(-1);
        traj.push(idx);
    }

    let reselections = traj.windows(2).filter(|w| w[0] != w[1]).count();
    let final_idx = traj.last().copied().unwrap_or(-1);
    let max_idx = traj.iter().copied().max().unwrap_or(-1);
    let forward = traj.windows(2).filter(|w| w[1] > w[0]).count();
    let backward = traj.windows(2).filter(|w| w[1] < w[0]).count();
    let tail_start = (n as f64 * 0.66) as usize;
    let settled = traj
        .get(tail_start..)
        .map(|s| s.windows(2).all(|w| w[0] == w[1]))
        .unwrap_or(true);

    let traj_str: Vec<String> = traj.iter().map(|i| i.to_string()).collect();
    println!(
        "{{\"session_id\":{sid},\"n_turns\":{n},\"auth_ordinal\":{ao},\"final_goal_idx\":{fi},\"max_goal_idx\":{mi},\"reselections\":{res},\"forward_jumps\":{fw},\"backward_jumps\":{bw},\"settled_last_third\":{settled},\"traj\":[{traj}]}}",
        sid = serde_json::to_string(&session_id).unwrap_or_else(|_| "\"\"".into()),
        ao = auth_ordinal.map(|i| i as i64).unwrap_or(-1),
        fi = final_idx,
        mi = max_idx,
        res = reselections,
        fw = forward,
        bw = backward,
        traj = traj_str.join(","),
    );
}
