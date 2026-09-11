use relay_adapters::family::{self, Family};
use relay_compass::StepRole;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let family = args
        .next()
        .and_then(|raw| Family::parse(&raw))
        .expect("usage: foreign_session <cursor|antigravity|claude|codex> <session-id|path>");
    let target = args.next().expect("session id or transcript path");

    let transcript = if PathBuf::from(&target).exists() {
        PathBuf::from(&target)
    } else {
        family::locate_in(family, &target)
            .map(|found| found.transcript)
            .expect("session not found")
    };

    let steps = family::semantic_steps(family, &transcript);
    let cwd = family::session_cwd(family, &transcript);
    println!("family     {}", family.label());
    println!("transcript {}", transcript.display());
    println!("workspace  {:?}", cwd);
    println!("steps      {}", steps.len());

    let users = steps.iter().filter(|s| s.role == StepRole::User).count();
    let tools = steps.iter().filter(|s| s.role == StepRole::ToolUse).count();
    let results = steps
        .iter()
        .filter(|s| s.role == StepRole::ToolResult)
        .count();
    let failures = steps.iter().filter(|s| s.is_error).count();
    println!("user {users} · tool {tools} · result {results} · failed {failures}");

    if let Some(first) = steps.iter().find(|s| s.role == StepRole::User) {
        println!("\ncontract:\n{}", truncate(&first.text, 400));
    }
    println!("\nlast steps:");
    for step in steps.iter().rev().take(6).rev() {
        println!(
            "  {:>4} {:?}{} {}",
            step.index,
            step.role,
            if step.is_error { " ERR" } else { "" },
            truncate(step.text.trim(), 110)
        );
    }
}

fn truncate(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        return flat;
    }
    flat.chars().take(max).collect::<String>() + "…"
}
