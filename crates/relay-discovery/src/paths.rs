use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn discover() -> anyhow::Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
        Ok(Self { home })
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.home.join(".claude")
    }
    pub fn claude_ide_dir(&self) -> PathBuf {
        self.claude_dir().join("ide")
    }
    pub fn claude_sessions_dir(&self) -> PathBuf {
        self.claude_dir().join("sessions")
    }
    pub fn claude_projects_dir(&self) -> PathBuf {
        self.claude_dir().join("projects")
    }
    pub fn claude_project_dir(&self, cwd: &std::path::Path) -> PathBuf {
        self.claude_projects_dir().join(encode_cwd(cwd))
    }

    pub fn codex_dir(&self) -> PathBuf {
        self.home.join(".codex")
    }
    pub fn codex_state_db(&self) -> PathBuf {
        self.codex_dir().join("state_5.sqlite")
    }

    pub fn vscode_user_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/Code/User")
        } else if cfg!(target_os = "windows") {
            self.home.join("AppData/Roaming/Code/User")
        } else {
            self.home.join(".config/Code/User")
        }
    }
    pub fn vscode_storage_json(&self) -> PathBuf {
        self.vscode_user_dir().join("globalStorage/storage.json")
    }
}

pub fn encode_cwd(cwd: &std::path::Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '.' | '_' => '-',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn encodes_cwd_like_claude() {
        assert_eq!(
            encode_cwd(Path::new("/Users/itodev/devs/vsc_parser")),
            "-Users-itodev-devs-vsc-parser"
        );
    }
}
