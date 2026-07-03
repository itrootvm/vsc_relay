use crate::vscode::uri_to_path;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct IdeLock {
    pub port: u16,
    pub workspace_folders: Vec<PathBuf>,
    pub auth_token: Option<String>,
    pub ide_name: Option<String>,
}

pub fn read_all(ide_dir: &Path) -> Vec<IdeLock> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(ide_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lock") {
            continue;
        }
        let port: u16 = match path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok())
        {
            Some(p) => p,
            None => continue,
        };
        if port == 0 {
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
        let workspace_folders = v
            .get("workspaceFolders")
            .and_then(|w| w.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .filter_map(workspace_folder_path)
                    .fold(Vec::new(), |mut out, path| {
                        if !out.contains(&path) {
                            out.push(path);
                        }
                        out
                    })
            })
            .unwrap_or_default();
        let auth_token = v
            .get("authToken")
            .and_then(|x| x.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let ide_name = v
            .get("ideName")
            .and_then(|x| x.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        out.push(IdeLock {
            port,
            workspace_folders,
            auth_token,
            ide_name,
        });
    }
    out
}

pub fn is_port_live(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(120)).is_ok()
}

fn workspace_folder_path(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("file://") {
        uri_to_path(trimmed)
    } else {
        Some(PathBuf::from(trimmed))
    }
}
