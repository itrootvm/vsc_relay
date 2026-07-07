use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const MIN_REAL_BYTES: u64 = 50 * 1024 * 1024;

fn claude_name() -> String {
    format!("claude{}", std::env::consts::EXE_SUFFIX)
}

fn claude_real_name() -> String {
    format!("claude.real{}", std::env::consts::EXE_SUFFIX)
}

fn shim_name() -> String {
    format!("vsc-claude-shim{}", std::env::consts::EXE_SUFFIX)
}

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
        shim_installed: best.as_deref().map(is_installed).unwrap_or(false),
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
    fixed_editor_paths().iter().any(|p| p.exists())
}

#[cfg(windows)]
fn fixed_editor_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(local) = dirs::data_local_dir() {
        out.push(local.join(r"Programs\Microsoft VS Code\Code.exe"));
        out.push(local.join(r"Programs\Microsoft VS Code Insiders\Code - Insiders.exe"));
        out.push(local.join(r"Programs\cursor\Cursor.exe"));
    }
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            out.push(PathBuf::from(pf).join(r"Microsoft VS Code\Code.exe"));
        }
    }
    out
}

#[cfg(not(windows))]
fn fixed_editor_paths() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [
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
        out.push(home.join(".local/share/flatpak/app/com.visualstudio.code"));
    }
    out
}

fn which(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&paths) {
        for ext in path_exts() {
            if dir.join(format!("{bin}{ext}")).is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(windows)]
fn path_exts() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
    let mut v: Vec<String> = raw
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    v.push(String::new());
    v
}

#[cfg(not(windows))]
fn path_exts() -> Vec<String> {
    vec![String::new()]
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
    let claude = dir.join(claude_name());
    let real = dir.join(claude_real_name());
    let shim = shim_binary()?;

    if is_installed(dir) && same_bytes(&claude, &shim) {
        return Ok(format!("shim already current at {}", claude.display()));
    }

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
    crate::fsutil::set_executable(&tmp).context("chmod shim")?;
    if let Err(e) = std::fs::rename(&tmp, &claude) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| {
            format!(
                "atomic swap shim at {} (close every open Claude Code chat first; a running chat locks the file)",
                claude.display()
            )
        });
    }
    Ok(format!("installed shim at {}", claude.display()))
}

fn same_bytes(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() != mb.len() => return false,
        (Ok(_), Ok(_)) => {}
        _ => return false,
    }
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn sweep_shim_temp(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().starts_with("claude.tmp.") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

pub fn install_shim() -> Result<String> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dirs = ext_native_dirs();
    if dirs.is_empty() {
        bail!("Claude Code extension native-binary directory not found");
    }
    let mut ok: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    for dir in &dirs {
        sweep_shim_temp(dir);
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
                if dir.join(claude_name()).exists() || dir.join(claude_real_name()).exists() {
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
                    if dir.join(claude_name()).exists() || dir.join(claude_real_name()).exists() {
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
        .with_file_name(shim_name());
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
        sweep_shim_temp(dir);
        let claude = dir.join(claude_name());
        let real = dir.join(claude_real_name());
        if real.exists() {
            std::fs::rename(&real, &claude).with_context(|| {
                format!(
                    "restore real binary at {} (close every open Claude Code chat first; a running chat locks the file)",
                    claude.display()
                )
            })?;
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
        for name in [claude_name(), claude_real_name()] {
            let p = dir.join(&name);
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
    let claude = dir.join(claude_name());
    let real = dir.join(claude_real_name());
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
