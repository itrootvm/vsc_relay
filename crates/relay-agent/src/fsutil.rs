use std::io::Write;
use std::path::Path;

#[cfg(unix)]
pub fn secure_dir(path: &Path) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path);
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(windows)]
pub fn secure_dir(path: &Path) {
    let _ = std::fs::create_dir_all(path);
}

static WRITE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn secure_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        secure_dir(dir);
    }
    let sequence = WRITE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), sequence));
    write_private(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn secure_create_new(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    if let Some(dir) = path.parent() {
        secure_dir(dir);
    }
    let sequence = WRITE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("new.{}.{}", std::process::id(), sequence));
    write_private(&tmp, bytes)?;
    let claimed = match std::fs::hard_link(&tmp, path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
    };
    let _ = std::fs::remove_file(&tmp);
    Ok(claimed)
}

#[cfg(unix)]
fn write_private(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn write_private(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(tmp)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(unix)]
pub fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(windows)]
pub fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_never_publish_a_half_written_file() {
        let dir = std::env::temp_dir().join(format!("vsc-relay-fsutil-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("payload.json");
        let short = vec![b'a'; 4 * 1024];
        let long = vec![b'b'; 512 * 1024];

        let mut handles = Vec::new();
        for round in 0..8 {
            let path = path.clone();
            let bytes = if round % 2 == 0 {
                short.clone()
            } else {
                long.clone()
            };
            handles.push(std::thread::spawn(move || {
                for _ in 0..8 {
                    secure_write(&path, &bytes).expect("write");
                    let seen = std::fs::read(&path).expect("read");
                    assert!(
                        seen.len() == 4 * 1024 || seen.len() == 512 * 1024,
                        "a partial file of {} bytes was published",
                        seen.len()
                    );
                    assert!(
                        seen.iter().all(|byte| *byte == seen[0]),
                        "two writers were mixed into one file"
                    );
                }
            }));
        }
        for handle in handles {
            handle.join().expect("thread");
        }
        let leftovers = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(leftovers, 0, "temporary files were left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exactly_one_racing_writer_claims_a_new_secret() {
        let dir = std::env::temp_dir().join(format!(
            "vsc-relay-fsutil-claim-{}-{}",
            std::process::id(),
            WRITE_SEQUENCE.load(std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("secret.key");

        let mut handles = Vec::new();
        for round in 0..8u8 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                secure_create_new(&path, &[round; 32]).expect("claim")
            }));
        }
        let claims = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .filter(|claimed| *claimed)
            .count();

        assert_eq!(claims, 1, "more than one writer claimed the secret");
        assert_eq!(std::fs::read(&path).expect("read").len(), 32);
        let leftovers = std::fs::read_dir(&dir)
            .expect("dir")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".new."))
            .count();
        assert_eq!(leftovers, 0, "temporary files were left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
