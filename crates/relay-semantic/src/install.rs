use crate::config::{builtin_model_dir, BUILTIN_MODEL_ID};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

const BASE: &str =
    "https://huggingface.co/onnx-community/multilingual-MiniLMv2-L6-mnli-xnli-ONNX/resolve/main";

struct Artifact {
    remote: &'static str,
    local: &'static str,
    sha256: &'static str,
}

const ARTIFACTS: &[Artifact] = &[
    Artifact {
        remote: "onnx/model_fp16.onnx",
        local: "model.onnx",
        sha256: "911561370b93632d5c6f5c1541454382ebc263ee7915e695a8565fcc8830175e",
    },
    Artifact {
        remote: "tokenizer.json",
        local: "tokenizer.json",
        sha256: "d0091a328b3441d754e481db5a390d7f3b8dabc6016869fd13ba350d23ddc4cd",
    },
    Artifact {
        remote: "config.json",
        local: "config.json",
        sha256: "3ebbbdf3c3803649b49715fd169207c0f42c4250dfea5dd304b1d8b8f745852a",
    },
];

#[derive(Serialize)]
struct Manifest<'a> {
    schema: u32,
    kind: &'a str,
    model_id: &'a str,
    model_file: &'a str,
    tokenizer_file: &'a str,
    max_length: usize,
    entailment_index: usize,
    neutral_index: usize,
    contradiction_index: usize,
    calibrated: bool,
    source: &'a str,
}

async fn verified_download(
    client: &reqwest::Client,
    artifact: &Artifact,
    dir: &Path,
) -> Result<()> {
    let target = dir.join(artifact.local);
    if target.is_file() {
        let bytes = tokio::fs::read(&target).await?;
        if format!("{:x}", Sha256::digest(&bytes)) == artifact.sha256 {
            return Ok(());
        }
    }
    let temp = target.with_extension("download");
    let response = client
        .get(format!("{BASE}/{}", artifact.remote))
        .send()
        .await
        .context("download local semantic model")?
        .error_for_status()
        .context("local semantic model HTTP status")?;
    let mut response = response;
    let mut file = tokio::fs::File::create(&temp).await?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = response.chunk().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    let actual = format!("{:x}", hasher.finalize());
    if actual != artifact.sha256 {
        let _ = tokio::fs::remove_file(&temp).await;
        bail!(
            "checksum mismatch for {}: expected {}, got {}",
            artifact.local,
            artifact.sha256,
            actual
        );
    }
    tokio::fs::rename(&temp, &target).await?;
    Ok(())
}

pub async fn install_builtin() -> Result<PathBuf> {
    let dir = builtin_model_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let client = reqwest::Client::builder().build()?;
    for artifact in ARTIFACTS {
        verified_download(&client, artifact, &dir).await?;
    }
    let manifest = Manifest {
        schema: 1,
        kind: "nli",
        model_id: BUILTIN_MODEL_ID,
        model_file: "model.onnx",
        tokenizer_file: "tokenizer.json",
        max_length: 384,
        entailment_index: 0,
        neutral_index: 1,
        contradiction_index: 2,
        calibrated: false,
        source: "onnx-community/multilingual-MiniLMv2-L6-mnli-xnli-ONNX",
    };
    tokio::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )
    .await?;
    Ok(dir)
}
