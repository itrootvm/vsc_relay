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
    let dirs = ext_native_dirs();
    let best = ext_native_dir().ok();
    EnvStatus {
        vscode: vscode_present(),
        extension: !dirs.is_empty(),
        shim_installed: dirs.iter().any(|d| is_installed(d)),
        version: best.as_deref().and_then(ext_version),
    }
}

const EXT_ROOT_DIRS: &[&str] = &[
    ".vscode",
    ".vscode-insiders",
    ".vscode-oss",
    ".vscodium",
    ".cursor",
    ".windsurf",
];

const EDITOR_BINS: &[&str] = &[
    "code",
    "code-insiders",
    "codium",
    "vscodium",
    "code-oss",
    "cursor",
];

fn vscode_present() -> bool {
    if Path::new("/Applications/Visual Studio Code.app").exists() {
        return true;
    }
    if !ext_roots().is_empty() {
        return true;
    }
    if EDITOR_BINS.iter().any(|b| which(b)) {
        return true;
    }
    let mut fixed: Vec<PathBuf> = [
        "/usr/share/code",
        "/usr/bin/code",
        "/opt/visual-studio-code",
        "/snap/bin/code",
        "/var/lib/flatpak/app/com.visualstudio.code",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = dirs::home_dir() {
        fixed.push(home.join(".local/share/flatpak/app/com.visualstudio.code"));
    }
    fixed.iter().any(|p| p.exists())
}

fn which(bin: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ext_roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for d in EXT_ROOT_DIRS {
        let p = home.join(d).join("extensions");
        if !p.exists() {
            continue;
        }
        let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.insert(key) {
            out.push(p);
        }
    }
    out
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
    let mut best: Option<((u32, u32, u32), PathBuf)> = None;
    for ext in ext_roots() {
        let Ok(rd) = std::fs::read_dir(&ext) else {
            continue;
        };
        for entry in rd.flatten() {
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
    }
    best.map(|(_, d)| d)
        .context("Claude Code extension native-binary directory not found")
}

fn ext_native_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for ext in ext_roots() {
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
    let dirs = ext_native_dirs();
    if dirs.is_empty() {
        bail!("Claude Code extension native-binary directory not found");
    }
    let mut restored = 0;
    for dir in &dirs {
        let claude = dir.join("claude");
        let real = dir.join("claude.real");
        if real.exists() {
            std::fs::rename(&real, &claude)
                .with_context(|| format!("restore real binary at {}", claude.display()))?;
            println!("restored real binary at {}", claude.display());
            restored += 1;
        }
    }
    if restored == 0 {
        println!("no claude.real found; nothing to restore");
    }
    Ok(())
}

fn status() -> Result<()> {
    let dirs = ext_native_dirs();
    if dirs.is_empty() {
        bail!("Claude Code extension native-binary directory not found");
    }
    for dir in &dirs {
        println!("dir: {}", dir.display());
        for name in ["claude", "claude.real"] {
            let p = dir.join(name);
            match std::fs::metadata(&p) {
                Ok(m) => println!("  {name}: {} bytes", m.len()),
                Err(_) => println!("  {name}: absent"),
            }
        }
        println!("  installed: {}", is_installed(dir));
    }
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
