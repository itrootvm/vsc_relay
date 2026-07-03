use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub pid: u32,
    pub session_id: String,
    pub cwd: PathBuf,
    pub entrypoint: Option<String>,
    pub name: Option<String>,
}

pub fn read_all(sessions_dir: &Path) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let pid = match v
            .get("pid")
            .and_then(|x| x.as_u64())
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| *p > 0)
        {
            Some(p) => p,
            None => continue,
        };
        let session_id = match v.get("sessionId").and_then(|x| x.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let cwd = match v.get("cwd").and_then(|x| x.as_str()) {
            Some(c) => PathBuf::from(c),
            None => continue,
        };
        out.push(SessionInfo {
            pid,
            session_id,
            cwd,
            entrypoint: v
                .get("entrypoint")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
            name: v
                .get("name")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        });
    }
    out
}

pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    pid_alive_impl(pid)
}

#[cfg(unix)]
fn pid_alive_impl(pid: u32) -> bool {
    unsafe {
        let r = libc::kill(pid as libc::pid_t, 0);
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
fn pid_alive_impl(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_zero_is_not_alive() {
        assert!(!pid_alive(0));
    }
}
