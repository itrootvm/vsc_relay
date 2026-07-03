use crate::paths::Paths;
use crate::{git, ide_lock, sessions, vscode};
use chrono::Utc;
use relay_core::model::{workspace_alias, ClaudeAgent, WindowEntry};
use relay_core::state::ClaudeState;
use relay_core::WindowId;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn scan(paths: &Paths) -> Vec<WindowEntry> {
    let locks = ide_lock::read_all(&paths.claude_ide_dir());
    let live_locks: Vec<_> = locks
        .into_iter()
        .filter(|l| ide_lock::is_port_live(l.port))
        .collect();
    let all_sessions = sessions::read_all(&paths.claude_sessions_dir());
    let storage_folders = vscode::open_workspace_folders(&paths.vscode_storage_json());

    let mut lock_by_ws: BTreeMap<PathBuf, (&ide_lock::IdeLock, PathBuf)> = BTreeMap::new();
    for l in &live_locks {
        for folder in &l.workspace_folders {
            lock_by_ws
                .entry(folder.clone())
                .or_insert((l, folder.clone()));
        }
    }

    let mut workspaces: BTreeMap<PathBuf, ()> = BTreeMap::new();
    for l in &live_locks {
        for f in &l.workspace_folders {
            workspaces.insert(f.clone(), ());
        }
    }
    for f in &storage_folders {
        workspaces.insert(f.clone(), ());
    }

    let mut entries: Vec<WindowEntry> = Vec::new();
    let now = Utc::now();

    for ws in workspaces.keys() {
        let lock = lock_by_ws.get(ws);
        let (ide_port, auth_token) = match lock {
            Some((l, _)) => (Some(l.port), l.auth_token.clone()),
            None => (None, None),
        };

        let claude = bind_claude(paths, ws, &all_sessions);

        entries.push(WindowEntry {
            window_id: WindowId(workspace_alias(ws)),
            workspace: ws.clone(),
            workspace_folders: vec![ws.clone()],
            git_branch: git::branch(ws),
            ide_port,
            auth_token,
            remote: None,
            claude,
            codex: Vec::new(),
            last_seen: now,
        });
    }

    dedupe_window_ids(&mut entries);
    entries
}

fn bind_claude(paths: &Paths, ws: &Path, all: &[sessions::SessionInfo]) -> Vec<ClaudeAgent> {
    let mut agents: Vec<(ClaudeAgent, u64)> = Vec::new();
    for s in all {
        if !same_path(&s.cwd, ws) {
            continue;
        }
        if !sessions::pid_alive(s.pid) {
            continue;
        }
        let jsonl = paths
            .claude_project_dir(&s.cwd)
            .join(format!("{}.jsonl", s.session_id));
        let (state, last_activity) = if jsonl.exists() {
            (ClaudeState::Unknown, mtime_secs(&jsonl))
        } else {
            (ClaudeState::Idle, None)
        };
        agents.push((
            ClaudeAgent {
                session_id: s.session_id.clone(),
                jsonl_path: jsonl,
                pid: Some(s.pid),
                entrypoint: s.entrypoint.clone(),
                name: s.name.clone(),
                title: None,
                state,
                tip_uuid: None,
                last_activity_secs: last_activity,
            },
            last_activity.unwrap_or(0),
        ));
    }
    agents.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    agents.into_iter().map(|(a, _)| a).collect()
}

fn dedupe_window_ids(entries: &mut [WindowEntry]) {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for e in entries.iter() {
        *seen.entry(e.window_id.0.clone()).or_insert(0) += 1;
    }
    let mut used = BTreeSet::new();
    let mut suffixes: BTreeMap<String, usize> = BTreeMap::new();
    for e in entries.iter_mut() {
        let base = e.window_id.0.clone();
        if seen.get(&base).copied().unwrap_or(0) <= 1 && used.insert(base.clone()) {
            continue;
        }
        let mut candidate = e
            .ide_port
            .map(|p| format!("{base}:{p}"))
            .unwrap_or_else(|| next_window_id(&base, &mut suffixes));
        while !used.insert(candidate.clone()) {
            candidate = next_window_id(&base, &mut suffixes);
        }
        e.window_id = WindowId(candidate);
    }
}

fn next_window_id(base: &str, suffixes: &mut BTreeMap<String, usize>) -> String {
    let suffix = suffixes.entry(base.to_string()).or_insert(0);
    *suffix += 1;
    format!("{base}:{}", *suffix)
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn mtime_secs(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    mtime.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_without_ports() {
        let now = Utc::now();
        let mut entries = vec![
            WindowEntry {
                window_id: WindowId("repo".to_string()),
                workspace: PathBuf::from("/tmp/a/repo"),
                workspace_folders: Vec::new(),
                git_branch: None,
                ide_port: None,
                auth_token: None,
                remote: None,
                claude: Vec::new(),
                codex: Vec::new(),
                last_seen: now,
            },
            WindowEntry {
                window_id: WindowId("repo".to_string()),
                workspace: PathBuf::from("/tmp/b/repo"),
                workspace_folders: Vec::new(),
                git_branch: None,
                ide_port: None,
                auth_token: None,
                remote: None,
                claude: Vec::new(),
                codex: Vec::new(),
                last_seen: now,
            },
        ];
        dedupe_window_ids(&mut entries);
        assert_ne!(entries[0].window_id, entries[1].window_id);
    }

    #[test]
    fn dedupes_reused_ports() {
        let now = Utc::now();
        let mut entries = vec![
            WindowEntry {
                window_id: WindowId("repo".to_string()),
                workspace: PathBuf::from("/tmp/a/repo"),
                workspace_folders: Vec::new(),
                git_branch: None,
                ide_port: Some(1234),
                auth_token: None,
                remote: None,
                claude: Vec::new(),
                codex: Vec::new(),
                last_seen: now,
            },
            WindowEntry {
                window_id: WindowId("repo".to_string()),
                workspace: PathBuf::from("/tmp/b/repo"),
                workspace_folders: Vec::new(),
                git_branch: None,
                ide_port: Some(1234),
                auth_token: None,
                remote: None,
                claude: Vec::new(),
                codex: Vec::new(),
                last_seen: now,
            },
        ];
        dedupe_window_ids(&mut entries);
        assert_ne!(entries[0].window_id, entries[1].window_id);
        assert_eq!(entries[0].window_id.0, "repo:1234");
    }
}
