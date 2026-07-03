use std::path::{Path, PathBuf};

pub fn open_workspace_folders(storage_json: &Path) -> Vec<PathBuf> {
    let text = match std::fs::read_to_string(storage_json) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    if let Some(folders) = v
        .pointer("/backupWorkspaces/folders")
        .and_then(|f| f.as_array())
    {
        for f in folders {
            if let Some(uri) = f.get("folderUri").and_then(|x| x.as_str()) {
                if let Some(p) = uri_to_path(uri) {
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest.to_string()
    } else if let Some(local) = rest.strip_prefix("localhost/") {
        format!("/{local}")
    } else if rest == "localhost" {
        "/".to_string()
    } else if cfg!(windows) {
        rest.to_string()
    } else {
        return None;
    };
    let decoded = percent_decode(&path);
    if decoded.is_empty() {
        return None;
    }
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn decodes_file_uri() {
        assert_eq!(
            uri_to_path("file:///Users/a%20b/project"),
            Some(PathBuf::from("/Users/a b/project"))
        );
    }

    #[test]
    fn decodes_localhost_file_uri() {
        assert_eq!(
            uri_to_path("file://localhost/Users/a/project"),
            Some(PathBuf::from("/Users/a/project"))
        );
    }

    #[test]
    fn rejects_remote_file_uri_on_unix() {
        if cfg!(unix) {
            assert_eq!(uri_to_path("file://server/share/project"), None);
        }
    }
}
