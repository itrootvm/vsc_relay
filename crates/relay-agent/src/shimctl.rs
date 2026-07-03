use anyhow::{bail, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const MIN_REAL_BYTES: u64 = 50 * 1024 * 1024;

pub struct EnvStatus {
    pub vscode: bool,
    pub extension: bool,
    pub shim_installed: bool,
    pub version: Option<String>,
}

pub fn is_shim_command(arg: &str) -> bool {
    matches!(
        arg,
        "shim-install" | "shim-uninstall" | "shim-status" | "env-check"
    )
}

pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("shim-install") => install(),
        Some("shim-uninstall") => uninstall(),
        Some("shim-status") => status(),
        Some("env-check") => env_check(),
        _ => bail!("unknown shim command"),
    }
}

pub fn parse_semver(s: &str) -> (u32, u32, u32) {
    let head: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut it = head.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

pub fn env_status() -> EnvStatus {
    let home = dirs::home_dir();
    let vscode = Path::new("/Applications/Visual Studio Code.app").exists()
        || home
            .as_ref()
            .map(|h| h.join(".vscode").join("extensions").exists())
            .unwrap_or(false);
    let dir = ext_native_dir().ok();
    EnvStatus {
        vscode,
        extension: dir.is_some(),
        shim_installed: dir.as_deref().map(is_installed).unwrap_or(false),
        version: dir.as_deref().and_then(ext_version),
    }
}

fn install_one(dir: &Path) -> Result<String> {
    let claude = dir.join("claude");
    let real = dir.join("claude.real");
    let shim = shim_binary()?;

    if !real.exists() {
        let size = std::fs::metadata(&claude).map(|m| m.len()).unwrap_or(0);
        if size < MIN_REAL_BYTES {
            bail!(
                "refusing: {} is {} bytes (<50MB) and claude.real is missing; not the real binary",
                claude.display(),
                size
            );
        }
        std::fs::rename(&claude, &real).context("move real binary aside")?;
    }

    let tmp = dir.join(format!("claude.tmp.{}", std::process::id()));
    std::fs::copy(&shim, &tmp).context("copy shim into place")?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).context("chmod shim")?;
    std::fs::rename(&tmp, &claude).context("atomic swap shim")?;
    Ok(format!("installed shim at {}", claude.display()))
}

pub fn install_shim() -> Result<String> {
    let dirs = ext_native_dirs();
    if dirs.is_empty() {
        bail!("Claude Code extension native-binary directory not found");
    }
    let mut ok: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for dir in &dirs {
        match install_one(dir) {
            Ok(m) => ok.push(m),
            Err(e) => errs.push(format!("{}: {e}", dir.display())),
        }
    }
    if ok.is_empty() {
        bail!("shim install failed everywhere: {}", errs.join("; "));
    }
    let mut msg = ok.join("\n");
    if !errs.is_empty() {
        msg.push_str(&format!("\nskipped: {}", errs.join("; ")));
    }
    Ok(msg)
}

fn env_check() -> Result<()> {
    let s = env_status();
    println!("vscode={}", s.vscode);
    println!("extension={}", s.extension);
    println!("shim_installed={}", s.shim_installed);
    println!("version={}", s.version.unwrap_or_else(|| "-".to_string()));
    Ok(())
}

fn ext_version(native_dir: &Path) -> Option<String> {
    for anc in native_dir.ancestors() {
        if let Some(name) = anc.file_name().and_then(|s| s.to_str()) {
            if let Some(v) = name.strip_prefix("anthropic.claude-code-") {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn ext_native_dir() -> Result<PathBuf> {
    let ext = dirs::home_dir()
        .context("no home dir")?
        .join(".vscode")
        .join("extensions");
    let mut best: Option<((u32, u32, u32), PathBuf)> = None;
    for entry in std::fs::read_dir(&ext)
        .context("read extensions dir")?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(ver) = name.strip_prefix("anthropic.claude-code-") {
            let dir = entry.path().join("resources").join("native-binary");
            if dir.join("claude").exists() || dir.join("claude.real").exists() {
                let sv = parse_semver(ver);
                let take = match &best {
                    Some((v, _)) => sv > *v,
                    None => true,
                };
                if take {
                    best = Some((sv, dir));
                }
            }
        }
    }
    best.map(|(_, d)| d)
        .context("Claude Code extension native-binary directory not found")
}

fn ext_native_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let ext = home.join(".vscode").join("extensions");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&ext) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("anthropic.claude-code-") {
                let dir = entry.path().join("resources").join("native-binary");
                if dir.join("claude").exists() || dir.join("claude.real").exists() {
                    out.push(dir);
                }
            }
        }
    }
    out
}

fn shim_binary() -> Result<PathBuf> {
    let path = std::env::current_exe()
        .context("current_exe")?
        .with_file_name("vsc-claude-shim");
    if !path.exists() {
        bail!(
            "bundled shim binary not found next to the app: {}",
            path.display()
        );
    }
    Ok(path)
}

fn install() -> Result<()> {
    let msg = install_shim()?;
    println!("{msg}");
    println!("open a NEW Claude chat in VS Code to pick it up");
    Ok(())
}

fn uninstall() -> Result<()> {
    let dir = ext_native_dir()?;
    let claude = dir.join("claude");
    let real = dir.join("claude.real");
    if real.exists() {
        std::fs::rename(&real, &claude).context("restore real binary")?;
        println!("restored real binary at {}", claude.display());
    } else {
        println!("no claude.real found; nothing to restore");
    }
    Ok(())
}

fn status() -> Result<()> {
    let dir = ext_native_dir()?;
    println!("dir: {}", dir.display());
    for name in ["claude", "claude.real"] {
        let p = dir.join(name);
        match std::fs::metadata(&p) {
            Ok(m) => println!("{name}: {} bytes", m.len()),
            Err(_) => println!("{name}: absent"),
        }
    }
    println!("installed: {}", is_installed(&dir));
    Ok(())
}

fn is_installed(dir: &Path) -> bool {
    let claude = dir.join("claude");
    let real = dir.join("claude.real");
    if !real.exists() {
        return false;
    }
    std::fs::metadata(&claude)
        .map(|m| m.len() < MIN_REAL_BYTES)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::parse_semver;

    #[test]
    fn semver_orders_numerically() {
        assert!(parse_semver("2.1.100") > parse_semver("2.1.99"));
        assert!(parse_semver("2.1.199-darwin-arm64") > parse_semver("2.1.198-darwin-arm64"));
        assert!(parse_semver("2.2.0") > parse_semver("2.1.999"));
        assert_eq!(parse_semver("2.1.199-darwin-arm64"), (2, 1, 199));
    }
}
