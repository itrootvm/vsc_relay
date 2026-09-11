use crate::telegram::MediaKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const PREPROCESS_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone)]
pub struct StagedFile {
    pub path: PathBuf,
    pub kind: MediaKind,
    pub original_name: Option<String>,
    pub bytes: u64,
    pub sidecar: Option<PathBuf>,
    pub transcript: Option<String>,
}

pub fn media_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("VSC_RELAY_MEDIA_ROOT") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("media")
}

pub fn stage(
    chat_id: i64,
    message_id: i64,
    index: usize,
    kind: MediaKind,
    original_name: Option<&str>,
    bytes: &[u8],
) -> std::io::Result<StagedFile> {
    stage_in(
        &media_dir(),
        chat_id,
        message_id,
        index,
        kind,
        original_name,
        bytes,
    )
}

pub fn sweep(ttl_ms: i64, max_total_bytes: u64) {
    sweep_at(&media_dir(), SystemTime::now(), ttl_ms, max_total_bytes);
}

pub(crate) fn stage_in(
    root: &Path,
    chat_id: i64,
    message_id: i64,
    index: usize,
    kind: MediaKind,
    original_name: Option<&str>,
    bytes: &[u8],
) -> std::io::Result<StagedFile> {
    let dir = root.join(chat_id.to_string());
    let name = staged_name(message_id, index, kind, original_name);
    let path = dir.join(name);
    crate::fsutil::secure_write(&path, bytes)?;
    Ok(StagedFile {
        path,
        kind,
        original_name: original_name.map(|s| s.to_string()),
        bytes: bytes.len() as u64,
        sidecar: None,
        transcript: None,
    })
}

fn staged_name(message_id: i64, index: usize, kind: MediaKind, original: Option<&str>) -> String {
    let (stem, ext) = match original {
        Some(name) => split_name(name, kind),
        None => (
            kind.as_str().replace(' ', "-"),
            kind.default_ext().to_string(),
        ),
    };
    let mut stem = safe_component(&stem);
    if stem.is_empty() {
        stem = "file".to_string();
    }
    let ext = safe_component(&ext);
    let ext = if ext.is_empty() {
        kind.default_ext().to_string()
    } else {
        ext
    };
    format!("{message_id}-{index}-{stem}.{ext}")
}

fn split_name(name: &str, kind: MediaKind) -> (String, String) {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 8 => {
            (stem.to_string(), ext.to_string())
        }
        _ => (base.to_string(), kind.default_ext().to_string()),
    }
}

fn safe_component(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.len() > 64 {
        out.truncate(64);
    }
    out
}

fn sweep_at(root: &Path, now: SystemTime, ttl_ms: i64, max_total_bytes: u64) {
    let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    collect_files(root, &mut files);
    if ttl_ms > 0 {
        let ttl = Duration::from_millis(ttl_ms as u64);
        files.retain(|(path, modified, _)| {
            if let Ok(age) = now.duration_since(*modified) {
                if age > ttl {
                    let _ = std::fs::remove_file(path);
                    return false;
                }
            }
            true
        });
    }
    if max_total_bytes > 0 {
        let mut total: u64 = files.iter().map(|(_, _, size)| *size).sum();
        if total > max_total_bytes {
            files.sort_by_key(|(_, modified, _)| *modified);
            for (path, _, size) in &files {
                if total <= max_total_bytes {
                    break;
                }
                if std::fs::remove_file(path).is_ok() {
                    total = total.saturating_sub(*size);
                }
            }
        }
    }
    prune_empty_dirs(root);
}

fn collect_files(root: &Path, out: &mut Vec<(PathBuf, SystemTime, u64)>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if let Ok(meta) = entry.metadata() {
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((path, modified, meta.len()));
        }
    }
}

pub fn enrich(staged: &mut StagedFile) {
    if !preprocess_enabled() {
        return;
    }
    match staged.kind {
        MediaKind::Voice | MediaKind::Audio => transcribe(staged),
        MediaKind::Video | MediaKind::VideoNote => extract_frame(staged),
        _ => {}
    }
}

fn preprocess_enabled() -> bool {
    match std::env::var("RELAY_MEDIA_PREPROCESS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        ),
        Err(_) => true,
    }
}

fn transcribe(staged: &mut StagedFile) {
    let Some(bin) = tool_in_path("whisper").or_else(|| tool_in_path("whisper-cli")) else {
        return;
    };
    let Some(dir) = staged.path.parent() else {
        return;
    };
    let stem = staged
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let out_txt = dir.join(format!("{stem}.txt"));
    let is_cpp = bin
        .file_name()
        .map(|n| n.to_string_lossy().contains("whisper-cli"))
        .unwrap_or(false);
    let mut cmd = Command::new(&bin);
    if is_cpp {
        cmd.arg("-f")
            .arg(&staged.path)
            .arg("-otxt")
            .arg("-of")
            .arg(dir.join(&stem));
    } else {
        cmd.arg(&staged.path)
            .arg("--model")
            .arg("base")
            .arg("--output_format")
            .arg("txt")
            .arg("--output_dir")
            .arg(dir)
            .arg("--fp16")
            .arg("False");
    }
    if run_quiet(cmd, PREPROCESS_TIMEOUT).unwrap_or(false) {
        if let Ok(text) = std::fs::read_to_string(&out_txt) {
            let text = text.trim().to_string();
            if !text.is_empty() {
                staged.transcript = Some(text);
                staged.sidecar = Some(out_txt);
            }
        }
    }
}

fn extract_frame(staged: &mut StagedFile) {
    let Some(bin) = tool_in_path("ffmpeg") else {
        return;
    };
    let Some(dir) = staged.path.parent() else {
        return;
    };
    let stem = staged
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let out_jpg = dir.join(format!("{stem}-frame.jpg"));
    let mut seek = Command::new(&bin);
    seek.arg("-y")
        .arg("-ss")
        .arg("1")
        .arg("-i")
        .arg(&staged.path)
        .arg("-frames:v")
        .arg("1")
        .arg(&out_jpg);
    let mut ok = run_quiet(seek, PREPROCESS_TIMEOUT).unwrap_or(false) && out_jpg.exists();
    if !ok {
        let mut head = Command::new(&bin);
        head.arg("-y")
            .arg("-i")
            .arg(&staged.path)
            .arg("-frames:v")
            .arg("1")
            .arg(&out_jpg);
        ok = run_quiet(head, PREPROCESS_TIMEOUT).unwrap_or(false) && out_jpg.exists();
    }
    if ok {
        staged.sidecar = Some(out_jpg);
    }
}

fn tool_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

fn run_quiet(mut cmd: Command, timeout: Duration) -> std::io::Result<bool> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let deadline = SystemTime::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if SystemTime::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn prune_empty_dirs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let empty = std::fs::read_dir(&path)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false);
            if empty {
                let _ = std::fs::remove_dir(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "relay-media-test-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn stage_writes_named_file_under_chat_dir() {
        let root = scratch_root("stage");
        let staged = stage_in(
            &root,
            42,
            7,
            0,
            MediaKind::Document,
            Some("weird name!.pdf"),
            b"hello",
        )
        .expect("stage");
        assert!(staged.path.exists());
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"hello");
        assert_eq!(staged.bytes, 5);
        let name = staged
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(name, "7-0-weird_name_.pdf");
        assert!(staged.path.starts_with(root.join("42")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&staged.path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stage_synthesizes_name_when_absent() {
        let root = scratch_root("synth");
        let staged = stage_in(&root, 1, 3, 2, MediaKind::Voice, None, b"ogg-bytes").expect("stage");
        let name = staged
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(name, "3-2-voice-message.ogg");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_removes_files_past_ttl() {
        let root = scratch_root("ttl");
        let staged = stage_in(&root, 5, 1, 0, MediaKind::Photo, None, b"jpg").expect("stage");
        let modified = std::fs::metadata(&staged.path).unwrap().modified().unwrap();
        let future = modified + Duration::from_secs(10);
        sweep_at(&root, future, 1000, 0);
        assert!(!staged.path.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enrich_is_noop_for_document() {
        let staged = StagedFile {
            path: PathBuf::from("/tmp/does-not-matter.pdf"),
            kind: MediaKind::Document,
            original_name: Some("spec.pdf".to_string()),
            bytes: 3,
            sidecar: None,
            transcript: None,
        };
        let mut staged = staged;
        enrich(&mut staged);
        assert!(staged.sidecar.is_none());
        assert!(staged.transcript.is_none());
    }

    #[test]
    fn media_dir_defaults_under_private_relay_home() {
        if std::env::var_os("VSC_RELAY_MEDIA_ROOT").is_some() {
            return;
        }
        let dir = media_dir();
        assert!(dir.ends_with("media"));
        assert!(dir.to_string_lossy().contains(".vsc-relay"));
    }

    #[test]
    fn tool_in_path_probes_real_binaries() {
        #[cfg(unix)]
        assert!(tool_in_path("sh").is_some());
        assert!(tool_in_path("relay-nonexistent-binary-xyz-42").is_none());
    }

    #[test]
    fn sweep_enforces_total_size_cap() {
        let root = scratch_root("cap");
        for i in 0..4 {
            stage_in(
                &root,
                9,
                100 + i as i64,
                0,
                MediaKind::Document,
                None,
                &vec![0u8; 1000],
            )
            .expect("stage");
        }
        sweep_at(&root, SystemTime::now(), 0, 2500);
        let mut files = Vec::new();
        collect_files(&root, &mut files);
        let total: u64 = files.iter().map(|(_, _, size)| *size).sum();
        assert!(total <= 2500, "total after sweep = {total}");
        assert!(!files.is_empty(), "sweep should not remove everything");
        let _ = std::fs::remove_dir_all(&root);
    }
}
