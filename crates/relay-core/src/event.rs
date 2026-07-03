use crate::ids::{AgentKind, MachineId};
use crate::state::AskUserQuestion;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Hook,
    Tail,
    Sqlite,
    ControlResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted {
        entrypoint: String,
    },
    StateChanged {
        from: String,
        to: String,
    },
    TurnComplete {
        last_message_excerpt: String,
        duration_ms: Option<u64>,
    },
    QuestionAsked {
        question: AskUserQuestion,
    },
    AwaitingPermission {
        tool: String,
        target: Option<String>,
        suspected: bool,
    },
    Error {
        message: String,
    },
    SubagentActivity {
        count: u16,
    },
    SessionEnded,
}

impl EventKind {
    pub fn tag(&self) -> &'static str {
        match self {
            EventKind::SessionStarted { .. } => "session_started",
            EventKind::StateChanged { .. } => "state_changed",
            EventKind::TurnComplete { .. } => "turn_complete",
            EventKind::QuestionAsked { .. } => "question_asked",
            EventKind::AwaitingPermission { .. } => "awaiting_permission",
            EventKind::Error { .. } => "error",
            EventKind::SubagentActivity { .. } => "subagent_activity",
            EventKind::SessionEnded => "session_ended",
        }
    }

    pub fn actionable(&self) -> bool {
        matches!(
            self,
            EventKind::QuestionAsked { .. }
                | EventKind::AwaitingPermission { .. }
                | EventKind::Error { .. }
        )
    }

    fn normalized_text(&self) -> String {
        match self {
            EventKind::QuestionAsked { question } => question
                .questions
                .iter()
                .map(|q| {
                    let opts = q
                        .options
                        .iter()
                        .map(|o| o.label.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    format!(
                        "{}|{}",
                        q.question.split_whitespace().collect::<Vec<_>>().join(" "),
                        opts
                    )
                })
                .collect::<Vec<_>>()
                .join("||"),
            EventKind::AwaitingPermission { tool, target, .. } => {
                format!("{}|{}", tool, target.as_deref().unwrap_or(""))
            }
            EventKind::TurnComplete {
                last_message_excerpt,
                ..
            } => last_message_excerpt.clone(),
            EventKind::StateChanged { from, to } => format!("{from}->{to}"),
            EventKind::Error { message } => message.clone(),
            EventKind::SubagentActivity { count } => count.to_string(),
            EventKind::SessionStarted { entrypoint } => entrypoint.clone(),
            EventKind::SessionEnded => String::new(),
        }
    }

    fn visible_actions(&self) -> String {
        match self {
            EventKind::AwaitingPermission { .. } => "approve|deny".into(),
            EventKind::QuestionAsked { question } => (0..question
                .questions
                .iter()
                .map(|q| q.options.len())
                .sum::<usize>())
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join("|"),
            EventKind::Error { .. } => "retry".into(),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayEvent {
    pub machine_id: MachineId,
    pub workspace: PathBuf,
    pub git_branch: Option<String>,
    pub agent: AgentKind,
    pub session_ref: String,
    pub title: Option<String>,
    pub at: DateTime<Utc>,
    pub kind: EventKind,
    pub source: EventSource,
    pub fingerprint: String,
    pub actionable: bool,
}

impl RelayEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        machine_id: MachineId,
        workspace: PathBuf,
        git_branch: Option<String>,
        agent: AgentKind,
        session_ref: String,
        title: Option<String>,
        at: DateTime<Utc>,
        kind: EventKind,
        source: EventSource,
    ) -> Self {
        let fingerprint = fingerprint(&machine_id, &workspace, agent, &session_ref, &kind);
        let actionable = kind.actionable();
        Self {
            machine_id,
            workspace,
            git_branch,
            agent,
            session_ref,
            title,
            at,
            kind,
            source,
            fingerprint,
            actionable,
        }
    }
}

pub fn fingerprint(
    machine: &MachineId,
    workspace: &std::path::Path,
    agent: AgentKind,
    session_ref: &str,
    kind: &EventKind,
) -> String {
    let mut h = blake3::Hasher::new();
    let sep = [0x01u8];
    h.update(machine.0.as_bytes());
    h.update(&sep);
    h.update(workspace.to_string_lossy().as_bytes());
    h.update(&sep);
    h.update(agent.as_str().as_bytes());
    h.update(&sep);
    h.update(session_ref.as_bytes());
    h.update(&sep);
    h.update(kind.tag().as_bytes());
    h.update(&sep);
    h.update(kind.normalized_text().as_bytes());
    h.update(&sep);
    h.update(kind.visible_actions().as_bytes());
    h.finalize().to_hex().to_string()
}
