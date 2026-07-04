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
        self.vscode_user_dirs()
            .into_iter()
            .next()
            .unwrap_or_else(|| self.home.join(".config/Code/User"))
    }

    pub fn vscode_user_dirs(&self) -> Vec<PathBuf> {
        let base = if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support")
        } else if cfg!(target_os = "windows") {
            self.home.join("AppData/Roaming")
        } else {
            self.home.join(".config")
        };
        VSCODE_APP_DIRS
            .iter()
            .map(|d| base.join(d).join("User"))
            .collect()
    }

    pub fn vscode_storage_json(&self) -> PathBuf {
        self.vscode_user_dir().join("globalStorage/storage.json")
    }

    pub fn vscode_storage_jsons(&self) -> Vec<PathBuf> {
        self.vscode_user_dirs()
            .into_iter()
            .map(|d| d.join("globalStorage").join("storage.json"))
            .filter(|p| p.exists())
            .collect()
    }
}

const VSCODE_APP_DIRS: &[&str] = &[
    "Code",
    "Code - Insiders",
    "VSCodium",
    "Cursor",
    "Code - OSS",
    "Windsurf",
];

pub fn encode_cwd(cwd: &std::path::Path) -> String {
    encode_str(&cwd.to_string_lossy())
}

#[cfg(not(windows))]
fn encode_str(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            '/' | '.' | '_' => '-',
            other => other,
        })
        .collect()
}

#[cfg(windows)]
fn encode_str(raw: &str) -> String {
    lowercase_drive(raw)
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '.' | '_' => '-',
            other => other,
        })
        .collect()
}

#[cfg(windows)]
fn lowercase_drive(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let mut out = String::with_capacity(s.len());
        out.push((b[0] as char).to_ascii_lowercase());
        out.push_str(&s[1..]);
        out
    } else {
        s.to_string()
    }
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

    #[cfg(windows)]
    #[test]
    fn encodes_windows_cwd_like_claude() {
        assert_eq!(
            encode_cwd(Path::new(r"c:\Users\admin\devs\vsx\vsc_relay")),
            "c--Users-admin-devs-vsx-vsc-relay"
        );
        assert_eq!(
            encode_cwd(Path::new(r"C:\Users\admin\devs\vsx\vsc_relay")),
            "c--Users-admin-devs-vsx-vsc-relay"
        );
    }
}
