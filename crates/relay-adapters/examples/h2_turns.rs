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
        .expect("usage: h2_turns <transcript.jsonl>");
    let with_methods = std::env::args().nth(2).as_deref() == Some("--with-methods");
    let session_id = Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let steps = semantic_steps(Path::new(&path));
    let hu: Vec<&SemanticStep> = steps.iter().filter(|s| is_human_user(s)).collect();
    let texts: Vec<String> = hu.iter().map(|s| s.text.clone()).collect();

    let turns: Vec<String> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let clipped: String = t.chars().take(1500).collect();
            format!(
                "{{\"ord\":{i},\"text\":{}}}",
                serde_json::to_string(&clipped).unwrap_or_else(|_| "\"\"".into())
            )
        })
        .collect();

    let methods = if with_methods {
        let late = select_goal_index(&texts).map(|i| i as i64).unwrap_or(-1);
        let input = contract_input_from_steps(&steps);
        let auth = input
            .anchor
            .as_ref()
            .and_then(|a| hu.iter().position(|s| s.index == a.source_step))
            .map(|i| i as i64)
            .unwrap_or(-1);
        format!(",\"late_ord\":{late},\"auth_ord\":{auth}")
    } else {
        String::new()
    };

    println!(
        "{{\"session_id\":{sid},\"n_human_turns\":{n},\"turns\":[{turns}]{methods}}}",
        sid = serde_json::to_string(&session_id).unwrap_or_else(|_| "\"\"".into()),
        n = texts.len(),
        turns = turns.join(","),
    );
}
