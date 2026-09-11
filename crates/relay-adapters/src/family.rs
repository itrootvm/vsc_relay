use relay_compass::SemanticStep;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Claude,
    Codex,
    Cursor,
    Antigravity,
}

impl Family {
    pub fn id(self) -> &'static str {
        match self {
            Family::Claude => "claude",
            Family::Codex => "codex",
            Family::Cursor => "cursor",
            Family::Antigravity => "antigravity",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Family::Claude => "Claude Code",
            Family::Codex => "Codex",
            Family::Cursor => "Cursor",
            Family::Antigravity => "Antigravity",
        }
    }

    pub fn parse(raw: &str) -> Option<Family> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => Some(Family::Claude),
            "codex" => Some(Family::Codex),
            "cursor" | "cursor-agent" => Some(Family::Cursor),
            "antigravity" | "agy" => Some(Family::Antigravity),
            _ => None,
        }
    }

    pub fn all() -> [Family; 4] {
        [
            Family::Claude,
            Family::Codex,
            Family::Cursor,
            Family::Antigravity,
        ]
    }

    pub fn compact_backend(self) -> &'static str {
        match self {
            Family::Claude => "claude-cli",
            Family::Codex => "codex-cli",
            Family::Cursor => "cursor-cli",
            Family::Antigravity => "antigravity",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SourceSession {
    pub family: Family,
    pub session_id: String,
    pub transcript: PathBuf,
    pub workspace: Option<PathBuf>,
}

pub fn locate_in(family: Family, session_id: &str) -> Option<SourceSession> {
    let transcript = match family {
        Family::Claude => claude_transcript(session_id),
        Family::Codex => None,
        Family::Cursor => crate::cursor::store_db(session_id),
        Family::Antigravity => crate::antigravity::transcript(session_id),
    }?;
    let workspace = session_cwd(family, &transcript);
    Some(SourceSession {
        family,
        session_id: session_id.to_string(),
        transcript,
        workspace,
    })
}

pub fn locate(session_id: &str) -> Option<SourceSession> {
    Family::all()
        .into_iter()
        .find_map(|family| locate_in(family, session_id))
}

fn claude_transcript(session_id: &str) -> Option<PathBuf> {
    let projects = dirs::home_dir()?.join(".claude").join("projects");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let path = entry.path().join(format!("{session_id}.jsonl"));
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

pub fn semantic_steps(family: Family, transcript: &Path) -> Vec<SemanticStep> {
    match family {
        Family::Claude => crate::claude::semantic_steps(transcript),
        Family::Codex => crate::codex::semantic_steps(transcript),
        Family::Cursor => crate::cursor::semantic_steps(transcript),
        Family::Antigravity => crate::antigravity::semantic_steps(transcript),
    }
}

pub fn tail_messages(family: Family, transcript: &Path, n: usize) -> Vec<(char, String)> {
    match family {
        Family::Claude => crate::claude::tail_messages(transcript, n),
        Family::Codex => crate::codex::tail_messages(transcript, n),
        Family::Cursor => crate::cursor::tail_messages(transcript, n),
        Family::Antigravity => crate::antigravity::tail_messages(transcript, n),
    }
}

pub fn latest_plan(family: Family, transcript: &Path) -> Option<String> {
    match family {
        Family::Claude => crate::claude::latest_plan(transcript),
        _ => None,
    }
}

pub fn session_cwd(family: Family, transcript: &Path) -> Option<PathBuf> {
    match family {
        Family::Claude => crate::claude::session_cwd(transcript),
        Family::Codex => None,
        Family::Cursor => crate::cursor::session_cwd(transcript),
        Family::Antigravity => crate::antigravity::session_cwd(transcript),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_round_trips_through_its_own_id() {
        for family in Family::all() {
            assert_eq!(Family::parse(family.id()), Some(family));
            assert!(!family.label().is_empty());
            assert!(!family.compact_backend().is_empty());
        }
    }

    #[test]
    fn the_names_a_user_or_a_cli_would_type_are_accepted() {
        assert_eq!(Family::parse("Claude"), Some(Family::Claude));
        assert_eq!(Family::parse(" cursor-agent "), Some(Family::Cursor));
        assert_eq!(Family::parse("agy"), Some(Family::Antigravity));
        assert_eq!(Family::parse("gemini"), None);
    }

    #[test]
    fn only_claude_carries_a_plan_and_the_others_say_so_rather_than_guessing() {
        let missing = Path::new("/nonexistent/transcript");
        for family in Family::all() {
            assert_eq!(latest_plan(family, missing), None);
            assert!(semantic_steps(family, missing).is_empty());
            assert!(tail_messages(family, missing, 4).is_empty());
        }
    }
}
