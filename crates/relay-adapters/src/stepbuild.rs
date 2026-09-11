use relay_compass::{SemanticStep, StepRole};

pub(crate) fn looks_like_failure(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "error",
        "failed",
        "failure",
        "panic",
        "exception",
        "traceback",
        "assertion",
        "fatal",
        "no such",
        "not found",
    ]
    .iter()
    .any(|token| lower.contains(token))
}

pub(crate) fn append_assistant(pending: &mut Option<String>, text: &str) {
    match pending {
        Some(buf) => {
            buf.push('\n');
            buf.push_str(text);
        }
        None => *pending = Some(text.to_string()),
    }
}

pub(crate) fn flush_assistant(
    steps: &mut Vec<SemanticStep>,
    index: &mut u32,
    pending: &mut Option<String>,
) {
    if let Some(text) = pending.take() {
        steps.push(SemanticStep::new(*index, StepRole::Assistant, text));
        *index += 1;
    }
}
