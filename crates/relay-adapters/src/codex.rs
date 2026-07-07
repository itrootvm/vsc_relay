use relay_core::model::CodexAgent;
use relay_core::state::{reduce_codex, CodexState};
use relay_discovery::registry::mtime_secs;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};

pub struct CodexThreadRow {
    pub id: String,
    pub rollout_path: PathBuf,
    pub approval_mode: String,
    pub model: Option<String>,
    pub title: String,
    pub recency_at_ms: i64,
}

pub fn tail_messages(rollout_path: &Path, n: usize) -> Vec<(char, String)> {
    let text = match std::fs::read_to_string(rollout_path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut msgs: Vec<(char, String)> = Vec::new();
    for line in text.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
            continue;
        }
        let p = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        match p.get("type").and_then(|t| t.as_str()) {
            Some("user_message") => {
                if let Some(m) = p.get("message").and_then(|x| x.as_str()) {
                    let m = m.trim();
                    if !m.is_empty() && !m.starts_with('<') {
                        msgs.push(('U', m.to_string()));
                    }
                }
            }
            Some("agent_message") => {
                if let Some(m) = p.get("message").and_then(|x| x.as_str()) {
                    if !m.trim().is_empty() {
                        msgs.push(('A', m.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    let start = msgs.len().saturating_sub(n);
    msgs.split_off(start)
}

pub fn open_ro(db_path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(2000))?;
    Ok(conn)
}

pub fn active_thread_for_cwd(
    conn: &Connection,
    cwd: &Path,
) -> anyhow::Result<Option<CodexThreadRow>> {
    let cwd_s = cwd.to_string_lossy().to_string();
    let row = conn
        .query_row(
            "SELECT id, rollout_path, approval_mode, model, title, recency_at_ms \
             FROM threads WHERE archived = 0 AND cwd = ?1 \
             ORDER BY recency_at_ms DESC LIMIT 1",
            [cwd_s],
            row_to_thread,
        )
        .optional()?;
    Ok(row)
}

pub fn recent_threads_for_cwd(
    conn: &Connection,
    cwd: &Path,
    limit: u32,
) -> anyhow::Result<Vec<CodexThreadRow>> {
    let cwd_s = cwd.to_string_lossy().to_string();
    let mut stmt = conn.prepare(
        "SELECT id, rollout_path, approval_mode, model, title, recency_at_ms \
         FROM threads WHERE archived = 0 AND cwd = ?1 \
         ORDER BY recency_at_ms DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![cwd_s, limit], row_to_thread)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn row_to_thread(r: &rusqlite::Row<'_>) -> rusqlite::Result<CodexThreadRow> {
    let id: String = r.get(0)?;
    let rollout_path = r
        .get::<_, Option<String>>(1)?
        .map(PathBuf::from)
        .unwrap_or_default();
    let approval_mode = r
        .get::<_, Option<String>>(2)?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let title = r
        .get::<_, Option<String>>(4)?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    Ok(CodexThreadRow {
        id,
        rollout_path,
        approval_mode,
        model: r.get(3)?,
        title,
        recency_at_ms: r.get(5)?,
    })
}

pub struct CodexAttach {
    pub agent: CodexAgent,
    pub last_message: Option<String>,
    pub last_duration_ms: Option<u64>,
    pub usage: Option<relay_core::state::TokenUsage>,
}

fn build_attach(row: CodexThreadRow) -> CodexAttach {
    let reduction = match std::fs::read_to_string(&row.rollout_path) {
        Ok(rollout) => reduce_codex(&rollout),
        Err(e) => relay_core::state::CodexReduction {
            state: CodexState::Error {
                message: format!("rollout read failed: {e}"),
            },
            last_agent_message: None,
            last_duration_ms: None,
            last_turn_tokens: None,
        },
    };
    let agent = CodexAgent {
        thread_id: row.id,
        rollout_path: row.rollout_path.clone(),
        approval_mode: row.approval_mode,
        model: row.model,
        title: row.title,
        state: reduction.state,
        recency_at_ms: row.recency_at_ms,
        last_activity_secs: mtime_secs(&row.rollout_path),
    };
    CodexAttach {
        agent,
        last_message: reduction.last_agent_message,
        last_duration_ms: reduction.last_duration_ms,
        usage: reduction.last_turn_tokens,
    }
}

pub fn attach(db_path: &Path, cwd: &Path, max_age_ms: i64, now_ms: i64) -> Option<CodexAttach> {
    if max_age_ms < 0 {
        return None;
    }
    let conn = open_ro(db_path).ok()?;
    let row = active_thread_for_cwd(&conn, cwd).ok().flatten()?;
    if now_ms.saturating_sub(row.recency_at_ms) > max_age_ms {
        return None;
    }
    Some(build_attach(row))
}

pub fn attach_all(
    db_path: &Path,
    cwd: &Path,
    max_age_ms: i64,
    now_ms: i64,
    limit: u32,
) -> Vec<CodexAttach> {
    if max_age_ms < 0 || limit == 0 {
        return Vec::new();
    }
    let conn = match open_ro(db_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let rows = recent_threads_for_cwd(&conn, cwd, limit).unwrap_or_default();
    rows.into_iter()
        .filter(|r| now_ms.saturating_sub(r.recency_at_ms) <= max_age_ms)
        .map(build_attach)
        .collect()
}
