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

pub fn secure_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        secure_dir(dir);
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    write_private(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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
        f.flush()?;
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
    f.flush()
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
