use std::path::Path;
use std::path::PathBuf;

pub fn branch(workspace: &Path) -> Option<String> {
    let git = workspace.join(".git");
    let head_path = if git.is_dir() {
        git.join("HEAD")
    } else {
        gitdir_from_file(&git)?.join("HEAD")
    };
    let head = std::fs::read_to_string(head_path).ok()?;
    parse_head(&head)
}

fn gitdir_from_file(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let raw = text.trim().strip_prefix("gitdir:")?.trim();
    if raw.is_empty() {
        return None;
    }
    let dir = PathBuf::from(raw);
    if dir.is_absolute() {
        Some(dir)
    } else {
        path.parent().map(|parent| parent.join(dir))
    }
}

fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(rest) = head.strip_prefix("ref: refs/heads/") {
        if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }
    } else if head.len() >= 7 {
        Some(head[..7].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_head() {
        assert_eq!(
            parse_head("ref: refs/heads/main\n").as_deref(),
            Some("main")
        );
    }

    #[test]
    fn parses_detached_head() {
        assert_eq!(parse_head("1234567890abcdef\n").as_deref(), Some("1234567"));
    }

    #[test]
    fn rejects_short_detached_head() {
        assert_eq!(parse_head("123\n"), None);
    }
}
