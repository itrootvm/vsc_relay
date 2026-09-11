use crate::supervisor::decision::{Action, Decision};
use anyhow::{anyhow, Result};
use portable_pty::{CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::info;

const IDLE_BEFORE_ASKING: Duration = Duration::from_millis(1500);
const READ_SLICE: Duration = Duration::from_millis(250);
const TAIL_CHARS: usize = 4000;
const MAX_ANSWERS: usize = 6;
const MAX_PROVIDER_FAILURES: usize = 2;
const MAX_RUNTIME: Duration = Duration::from_secs(30 * 60);
const DEAD_END_AFTER: usize = 2;
const ANSWER_SYSTEM: &str = "You are watching the terminal of a command line coding agent that \
     has stopped and is waiting for a keypress. You are given the last lines it printed. Decide \
     what a careful engineer would answer to get past this prompt without changing what the \
     agent was asked to do. Answer setup and preference questions with the safest default. Never \
     grant a permission, never confirm a destructive action, never change a model or a plan. If \
     the prompt is not a simple setup question, stop.";

#[derive(Debug, Clone)]
pub struct SupervisedRun {
    pub log: PathBuf,
    pub answered: usize,
    pub status: Option<i32>,
    pub stopped_by_guard: bool,
}

pub fn strip_ansi(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            if ch != '\r' {
                out.push(ch);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                for next in chars.by_ref() {
                    if next == '\u{7}' || next == '\u{1b}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

pub fn looks_like_prompt(tail: &str) -> bool {
    let lower = tail.trim_end().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    let last: String = lower.lines().rev().take(6).collect::<Vec<_>>().join(" ");
    [
        "?",
        "(y/n)",
        "[y/n]",
        "press enter",
        "choose",
        "select",
        "continue?",
        "do you want",
        "would you like",
        "enter to",
    ]
    .iter()
    .any(|marker| last.contains(marker))
        || numbered_menu(&lower)
}

fn numbered_menu(text: &str) -> bool {
    let hits = text
        .lines()
        .rev()
        .take(12)
        .filter(|line| {
            let t = line.trim_start_matches(['>', '❯', ' ', '\t']);
            t.starts_with("1.") || t.starts_with("2.") || t.starts_with("3.")
        })
        .count();
    hits >= 2
}

pub fn is_unsafe_prompt(tail: &str) -> bool {
    let lower = tail.to_lowercase();
    [
        "permission",
        "approve",
        "allow this",
        "trust",
        "delete",
        "rm -rf",
        "force push",
        "overwrite",
        "credential",
        "password",
        "token",
        "api key",
        "sign in",
        "login",
        "payment",
        "billing",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub fn keystrokes_for(decision: &Decision) -> Option<String> {
    match decision.action {
        Action::Stop | Action::Wait => None,
        _ => match (decision.option_index, decision.message.as_deref()) {
            (Some(index), _) => {
                let down = "\x1b[B".repeat(index);
                Some(format!("{down}\r"))
            }
            (None, Some(text)) if !text.trim().is_empty() => Some(format!("{}\r", text.trim())),
            _ => Some("\r".to_string()),
        },
    }
}

pub fn tail_window(buffer: &str) -> String {
    let count = buffer.chars().count();
    if count <= TAIL_CHARS {
        return buffer.to_string();
    }
    buffer.chars().skip(count - TAIL_CHARS).collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn run_supervised(
    bin: &Path,
    argv: &[String],
    workspace: &Path,
    prompt: &str,
    session_id: &str,
    providers: &crate::automation::Providers,
    log: PathBuf,
    now: i64,
) -> Result<SupervisedRun> {
    let pty = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| anyhow!("could not open a pseudo terminal: {err}"))?;

    let mut command = CommandBuilder::new(bin);
    for arg in argv {
        command.arg(arg);
    }
    command.arg(prompt);
    command.cwd(workspace);

    let mut child = pty
        .slave
        .spawn_command(command)
        .map_err(|err| anyhow!("could not start {}: {err}", bin.display()))?;
    drop(pty.slave);

    let mut reader = pty
        .master
        .try_clone_reader()
        .map_err(|err| anyhow!("could not read the agent terminal: {err}"))?;
    let mut writer = pty
        .master
        .take_writer()
        .map_err(|err| anyhow!("could not write to the agent terminal: {err}"))?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buffer) {
            if n == 0 || tx.send(buffer[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut transcript = String::new();
    let mut answered = 0usize;
    let mut failures = 0usize;
    let mut stopped_by_guard = false;
    let mut idle = Duration::ZERO;
    let mut ignored = 0usize;
    let note = |transcript: &mut String, line: &str| {
        transcript.push_str(&format!("\n[relay] {line}\n"));
    };
    let started = std::time::Instant::now();
    let discovered = crate::supervisor::discover::discover_all().await;

    loop {
        match tokio::time::timeout(READ_SLICE, rx.recv()).await {
            Ok(Some(chunk)) => {
                transcript.push_str(&String::from_utf8_lossy(&chunk));
                idle = Duration::ZERO;
                let _ = std::fs::write(&log, transcript.as_bytes());
                continue;
            }
            Ok(None) => break,
            Err(_) => idle += READ_SLICE,
        }

        if started.elapsed() > MAX_RUNTIME {
            let _ = child.kill();
            break;
        }
        if !matches!(child.try_wait(), Ok(None)) {
            break;
        }
        if idle < IDLE_BEFORE_ASKING || answered >= MAX_ANSWERS {
            continue;
        }
        idle = Duration::ZERO;

        let tail = strip_ansi(&tail_window(&transcript));
        if !looks_like_prompt(&tail) {
            continue;
        }
        if is_unsafe_prompt(&tail) {
            info!(
                target: "relay::trace",
                pipeline = "clirun", stage = "guard", kind = "prompt",
                "the agent asked something that must not be answered for you"
            );
            stopped_by_guard = true;
            let _ = child.kill();
            break;
        }

        let request = format!(
            "The agent is {}. It printed this and stopped:\n\n{tail}\n\nAnswer it.",
            bin.display()
        );
        let decided = crate::supervisor::pool::Pool::new()
            .ask(
                session_id,
                providers,
                &discovered,
                ANSWER_SYSTEM,
                &request,
                now,
            )
            .await;
        let Ok((backend, decision)) = decided else {
            failures += 1;
            note(
                &mut transcript,
                "no provider answered; see automation provider settings",
            );
            let _ = std::fs::write(&log, transcript.as_bytes());
            if failures >= MAX_PROVIDER_FAILURES {
                info!(
                    target: "relay::trace",
                    pipeline = "clirun", stage = "no_provider", kind = "prompt",
                    "no provider answered the waiting agent; leaving it alone"
                );
                let _ = child.kill();
                break;
            }
            continue;
        };
        let Some(keys) = keystrokes_for(&decision) else {
            note(
                &mut transcript,
                &format!(
                    "{} said to leave this alone: {}",
                    backend.id(),
                    decision.reason
                ),
            );
            let _ = std::fs::write(&log, transcript.as_bytes());
            let _ = child.kill();
            break;
        };
        let before = transcript.len();
        if writer.write_all(keys.as_bytes()).is_err() {
            break;
        }
        let _ = writer.flush();
        answered += 1;
        note(
            &mut transcript,
            &format!(
                "answered with {} because {} ({})",
                keys.escape_debug(),
                decision.reason,
                backend.id()
            ),
        );
        let _ = std::fs::write(&log, transcript.as_bytes());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        while let Ok(Some(chunk)) = tokio::time::timeout(READ_SLICE, rx.recv()).await {
            transcript.push_str(&String::from_utf8_lossy(&chunk));
        }
        if transcript.len() <= before + 64 {
            ignored += 1;
            if ignored >= DEAD_END_AFTER {
                note(
                    &mut transcript,
                    "the agent ignored what was pressed twice, so it is left to you",
                );
                let _ = std::fs::write(&log, transcript.as_bytes());
                let _ = child.kill();
                break;
            }
        } else {
            ignored = 0;
        }
        info!(
            target: "relay::trace",
            pipeline = "clirun", stage = "answered", kind = "prompt",
            backend = %backend.id(), reason = %decision.reason, answered,
            "a waiting agent prompt was answered for you"
        );
    }

    let status = child.wait().ok().map(|s| s.exit_code() as i32);
    let _ = std::fs::write(&log, transcript.as_bytes());

    Ok(SupervisedRun {
        log,
        answered,
        status,
        stopped_by_guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(action: Action, index: Option<usize>, message: Option<&str>) -> Decision {
        Decision {
            action,
            message: message.map(str::to_string),
            option_index: index,
            wait_seconds: None,
            reason: "test".to_string(),
            confidence: Some(0.9),
        }
    }

    #[test]
    fn a_waiting_agent_is_recognised_by_what_it_printed() {
        assert!(looks_like_prompt(
            "Choose the text style that looks best with your terminal\n 1. Auto\n 2. Dark mode\n 3. Light mode"
        ));
        assert!(looks_like_prompt("Do you want to continue? (y/n)"));
        assert!(looks_like_prompt("Press Enter to continue"));
        assert!(!looks_like_prompt(
            "Reading server.py\nWrote 4 files\nDone."
        ));
        assert!(!looks_like_prompt(""));
    }

    #[test]
    fn a_prompt_about_permission_or_secrets_is_never_answered_for_the_user() {
        assert!(is_unsafe_prompt("Claude needs your permission to use Bash"));
        assert!(is_unsafe_prompt("Do you trust the files in this folder?"));
        assert!(is_unsafe_prompt("Delete 400 rows? (y/n)"));
        assert!(is_unsafe_prompt("Paste your API key"));
        assert!(!is_unsafe_prompt(
            "Choose the text style that looks best with your terminal"
        ));
    }

    #[test]
    fn an_option_is_pressed_by_moving_down_and_a_plain_answer_is_typed() {
        assert_eq!(
            keystrokes_for(&decision(Action::Continue, Some(2), None)).as_deref(),
            Some("\x1b[B\x1b[B\r")
        );
        assert_eq!(
            keystrokes_for(&decision(Action::Continue, Some(0), None)).as_deref(),
            Some("\r")
        );
        assert_eq!(
            keystrokes_for(&decision(Action::Feedback, None, Some(" dark "))).as_deref(),
            Some("dark\r")
        );
        assert_eq!(
            keystrokes_for(&decision(Action::Continue, None, None)).as_deref(),
            Some("\r")
        );
        assert_eq!(keystrokes_for(&decision(Action::Stop, Some(1), None)), None);
        assert_eq!(keystrokes_for(&decision(Action::Wait, None, None)), None);
    }

    #[test]
    fn only_the_recent_output_is_shown_to_the_provider() {
        let long = "x".repeat(TAIL_CHARS * 2);
        let tail = tail_window(&long);
        assert_eq!(tail.chars().count(), TAIL_CHARS);
        assert_eq!(tail_window("short"), "short");
    }

    #[test]
    fn the_provider_sees_plain_text_and_not_terminal_escapes() {
        let raw = "\u{1b}[2GChoose\u{1b}[9Gthe\u{1b}[13Gstyle\r\n\u{1b}[4G1. Auto\r\n\u{1b}[4G2. Dark mode";
        let clean = strip_ansi(raw);
        assert!(clean.contains("Choose"));
        assert!(clean.contains("1. Auto"));
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\r'));
        assert!(looks_like_prompt(&clean));
    }
}
