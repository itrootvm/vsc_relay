use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::Duration;

const REPO: &str = "itrootvm/vsc_relay";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const BINARIES: &[&str] = &["vsc-relay-agent", "vsc-claude-shim", "vsc-relay-gui"];

struct Asset {
    name: String,
    url: String,
}

struct Release {
    tag: String,
    assets: Vec<Asset>,
}

pub fn is_self_update_command(arg: &str) -> bool {
    arg == "self-update"
}

pub async fn run(args: &[String]) -> Result<()> {
    let check_only = args.iter().any(|a| a == "--check");
    let release = fetch_latest().await?;
    let latest = release.tag.trim_start_matches('v').to_string();

    if crate::shimctl::parse_semver(&latest) <= crate::shimctl::parse_semver(CURRENT) {
        println!("current=v{CURRENT} latest=v{latest} update=false");
        return Ok(());
    }
    println!("current=v{CURRENT} latest=v{latest} update=true");
    if check_only {
        return Ok(());
    }

    let arch = std::env::consts::ARCH;
    #[cfg(windows)]
    let (os_tag, ext) = ("windows", ".zip");
    #[cfg(not(windows))]
    let (os_tag, ext) = ("linux", ".tar.gz");
    let tag = format!("{os_tag}-{arch}");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name.contains(&tag) && a.name.ends_with(ext))
        .with_context(|| format!("no {tag} archive in release {}", release.tag))?;
    let sum = release
        .assets
        .iter()
        .find(|a| a.name == format!("{}.sha256", asset.name))
        .or_else(|| release.assets.iter().find(|a| a.name == "SHA256SUMS"));

    let work = std::env::temp_dir().join(format!("vsc-relay-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).context("create temp dir")?;

    println!("downloading {}", asset.name);
    let tarball = work.join(&asset.name);
    download(&asset.url, &tarball).await?;

    match sum {
        Some(s) => {
            let text = fetch_text(&s.url).await?;
            verify_sha256(&tarball, &text, &asset.name)?;
            println!("checksum ok");
        }
        None => println!("warning: no checksum published; skipping verification"),
    }

    extract(&tarball, &work)?;
    let staged_bin = work
        .join(format!("vsc-relay-{latest}-{os_tag}-{arch}"))
        .join("bin");
    let dest = std::env::current_exe()
        .context("current exe")?
        .parent()
        .context("exe dir")?
        .to_path_buf();

    let mut replaced = Vec::new();
    for base in BINARIES {
        let name = format!("{base}{}", std::env::consts::EXE_SUFFIX);
        let src = staged_bin.join(&name);
        if src.exists() {
            install_atomic(&src, &dest.join(&name)).with_context(|| format!("install {name}"))?;
            replaced.push(name);
        }
    }
    if replaced.is_empty() {
        bail!("no binaries found in the downloaded tarball");
    }
    println!("replaced: {}", replaced.join(", "));

    match crate::shimctl::install_shim() {
        Ok(_) => println!("shim reinstalled"),
        Err(e) => println!("shim reinstall skipped: {e}"),
    }

    let _ = std::fs::remove_dir_all(&work);
    println!("updated to v{latest}; restart the app or service to run it");
    Ok(())
}

async fn fetch_latest() -> Result<Release> {
    let http = client()?;
    let resp = http
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("github request")?;
    if !resp.status().is_success() {
        bail!("github returned {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await.context("parse github json")?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .context("no tag_name")?
        .to_string();
    let assets = v
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let name = a.get("name")?.as_str()?.to_string();
                    let url = a.get("browser_download_url")?.as_str()?.to_string();
                    Some(Asset { name, url })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release { tag, assets })
}

async fn fetch_text(url: &str) -> Result<String> {
    let http = client()?;
    let resp = http.get(url).send().await.context("download checksum")?;
    if !resp.status().is_success() {
        bail!("checksum download returned {}", resp.status());
    }
    resp.text().await.context("read checksum")
}

async fn download(url: &str, dest: &Path) -> Result<()> {
    let http = client()?;
    let resp = http.get(url).send().await.context("download request")?;
    if !resp.status().is_success() {
        bail!("download returned {}", resp.status());
    }
    let bytes = resp.bytes().await.context("read download")?;
    std::fs::write(dest, &bytes).context("write download")?;
    Ok(())
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("vsc-relay/{CURRENT}"))
        .timeout(Duration::from_secs(120))
        .build()
        .context("http client")
}

fn verify_sha256(file: &Path, sums: &str, asset_name: &str) -> Result<()> {
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut it = line.split_whitespace();
            let hash = it.next()?;
            let named = line.trim_end().ends_with(asset_name) || it.next().is_none();
            if named {
                Some(hash.to_ascii_lowercase())
            } else {
                None
            }
        })
        .context("asset not found in checksum file")?;
    let got = file_sha256(file)?;
    if got != expected {
        bail!("checksum mismatch (expected {expected}, got {got})");
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).context("open for hashing")?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).context("hash file")?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(not(windows))]
fn extract(archive: &Path, into: &Path) -> Result<()> {
    use std::process::Command;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .status()
        .context("run tar")?;
    if !status.success() {
        bail!("tar extraction failed");
    }
    Ok(())
}

#[cfg(windows)]
fn extract(archive: &Path, into: &Path) -> Result<()> {
    let file = std::fs::File::open(archive).context("open archive")?;
    let mut zip = zip::ZipArchive::new(file).context("read zip")?;
    zip.extract(into).context("extract zip")?;
    Ok(())
}

#[cfg(not(windows))]
fn install_atomic(src: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().context("dest dir")?;
    let file_name = dest.file_name().context("dest name")?.to_string_lossy();
    let tmp = dir.join(format!(".{file_name}.new"));
    std::fs::copy(src, &tmp).context("stage new binary")?;
    crate::fsutil::set_executable(&tmp).context("chmod")?;
    std::fs::rename(&tmp, dest).context("atomic rename")?;
    Ok(())
}

#[cfg(windows)]
fn install_atomic(src: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().context("dest dir")?;
    let file_name = dest.file_name().context("dest name")?.to_string_lossy();
    let tmp = dir.join(format!(".{file_name}.new"));
    std::fs::copy(src, &tmp).context("stage new binary")?;
    if dest.exists() {
        let old = dir.join(format!("{file_name}.old"));
        let _ = std::fs::remove_file(&old);
        std::fs::rename(dest, &old).context("move running exe aside")?;
    }
    std::fs::rename(&tmp, dest).context("install new binary")?;
    Ok(())
}

#[cfg(windows)]
pub fn sweep_old() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("old") {
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(not(windows))]
pub fn sweep_old() {}
