use relay_adapters::claude;
use relay_compass::ledger::{build_contract_ledger, build_contract_ledger_from_input};
use relay_compass::{contract_input_from_steps, select_goal_index};
use std::io::Write;
use std::path::{Path, PathBuf};

fn fixture_path(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "relay-causal-{}-{}-{}.jsonl",
        tag,
        std::process::id(),
        nanos
    ))
}

fn write_causal_fixture(path: &Path) {
    let backend_root = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content":
            "Build a durable task-state layer for the workflow engine. Add the bpm.task_state table with forward and reverse migrations. Define the instance_task_binding schema and its foreign keys. Implement the state machine covering pending, running, done, and failed transitions, and persist every transition to the task-state table."
        }
    });
    let assistant_ack = serde_json::json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [
            {"type": "text", "text": "I will design the bpm.task_state table and its migrations for the workflow engine."}
        ]}
    });
    let review = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "text", "text": "<ide_opened_file>The user opened the file /tmp/review/notes.md in the IDE. This may or may not be related to the current task.</ide_opened_file>"},
            {"type": "text", "text": "https://example.test/app/editor Based on the screenshot, the main layout and interface problems that stand out:\n\n* Cropped right inspector panel: the right sidebar overflows the viewport and is cut off, so its fields are unusable.\n* Layer overlap z-index: the process minimap overlaps the inspector panel and hides its content.\n* Sticky canvas controls: the zoom and focus controls hug the left sidebar with no margin.\n* Left palette scroll: the top palette item is clipped and hard to reach.\n* Node title truncation: long node titles are cut mid-word across the canvas.\n* Left-panel alignment: the radio buttons in the task-creation block (lazy and eager) look clunky next to the other menu items, and their description text is small and faded."}
        ]}
    });
    let assistant_follow = serde_json::json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [
            {"type": "text", "text": "Continuing on the backend task-state table and the instance_task_binding migrations."}
        ]}
    });
    let mut file = std::fs::File::create(path).expect("create causal fixture");
    for line in [backend_root, assistant_ack, review, assistant_follow] {
        writeln!(file, "{}", serde_json::to_string(&line).unwrap()).unwrap();
    }
}

fn ui_review_leaked(texts: &[String]) -> bool {
    texts.iter().any(|text| {
        let lower = text.to_lowercase();
        lower.contains("radio button")
            || lower.contains("inspector")
            || lower.contains("faded")
            || lower.contains("minimap")
            || (lower.contains("lazy") && lower.contains("eager") && lower.contains("clunky"))
    })
}

fn run_pipeline_legacy(tag: &str) -> (String, Vec<String>) {
    let path = fixture_path(tag);
    write_causal_fixture(&path);
    let steps = claude::semantic_steps(&path);
    let turns = claude::user_messages(&path);
    let goal = select_goal_index(&turns).expect("a goal is selected");
    let goal_text = turns[goal].clone();
    let ledger = build_contract_ledger(&steps, &[], &goal_text);
    let texts = ledger.obligations.iter().map(|o| o.text.clone()).collect();
    let _ = std::fs::remove_file(&path);
    (goal_text, texts)
}

fn run_pipeline_authoritative(tag: &str) -> (Option<String>, usize, Vec<String>) {
    let path = fixture_path(tag);
    write_causal_fixture(&path);
    let steps = claude::semantic_steps(&path);
    let input = contract_input_from_steps(&steps);
    let anchor_text = input.anchor_text.clone();
    let candidate_count = input.candidates.len();
    let ledger = build_contract_ledger_from_input(&steps, &[], &input);
    let texts = ledger.obligations.iter().map(|o| o.text.clone()).collect();
    let _ = std::fs::remove_file(&path);
    (anchor_text, candidate_count, texts)
}

#[test]
fn legacy_goal_path_admits_the_late_review_as_root_contract() {
    let (goal_text, texts) = run_pipeline_legacy("legacy");
    assert!(
        goal_text.to_lowercase().contains("radio button"),
        "the legacy goal path selects the late UI review as the root contract; selected goal was: {goal_text}"
    );
    assert_eq!(
        texts.len(),
        6,
        "legacy root structural atomization mints six obligations from the six review bullets; got: {texts:?}"
    );
    assert!(
        ui_review_leaked(&texts),
        "the legacy path lets a review obligation through; got: {texts:?}"
    );
}

#[test]
fn authoritative_admission_keeps_the_late_review_out_of_the_contract() {
    let (anchor_text, candidate_count, texts) = run_pipeline_authoritative("authoritative");
    assert!(
        anchor_text
            .as_deref()
            .is_some_and(|text| text.to_lowercase().contains("task_state")),
        "the authoritative anchor is the backend root, not the UI review; anchor was: {anchor_text:?}"
    );
    assert_eq!(
        candidate_count, 1,
        "the late UI review is recorded as one provisional candidate"
    );
    assert!(
        !ui_review_leaked(&texts),
        "no UI-review bullet becomes a contract obligation; obligations: {texts:?}"
    );
}
