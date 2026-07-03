use crate::ids::{AgentKind, MachineId, WindowId};
use crate::state::{ClaudeState, CodexState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineSnapshot {
    pub machine_id: MachineId,
    pub hostname: String,
    pub generated_at: DateTime<Utc>,
    pub windows: Vec<WindowEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowEntry {
    pub window_id: WindowId,
    pub workspace: PathBuf,
    pub workspace_folders: Vec<PathBuf>,
    pub git_branch: Option<String>,
    pub ide_port: Option<u16>,
    pub auth_token: Option<String>,
    pub remote: Option<String>,
    pub claude: Vec<ClaudeAgent>,
    pub codex: Vec<CodexAgent>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeAgent {
    pub session_id: String,
    pub jsonl_path: PathBuf,
    pub pid: Option<u32>,
    pub entrypoint: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub state: ClaudeState,
    pub tip_uuid: Option<String>,
    pub last_activity_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAgent {
    pub thread_id: String,
    pub rollout_path: PathBuf,
    pub approval_mode: String,
    pub model: Option<String>,
    pub title: String,
    pub state: CodexState,
    pub recency_at_ms: i64,
    pub last_activity_secs: Option<u64>,
}

pub fn workspace_alias(workspace: &std::path::Path) -> String {
    workspace
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| workspace.to_string_lossy().to_string())
}

impl WindowEntry {
    pub fn agent_kinds(&self) -> Vec<AgentKind> {
        let mut v = Vec::new();
        if !self.claude.is_empty() {
            v.push(AgentKind::ClaudeCode);
        }
        if !self.codex.is_empty() {
            v.push(AgentKind::Codex);
        }
        v
    }

    pub fn chat_count(&self) -> usize {
        self.claude.len() + self.codex.len()
    }
}
