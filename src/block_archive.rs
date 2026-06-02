use std::path::PathBuf;

use anyhow::Context;
use chrono::Utc;
use serde_json::json;
use tokio::fs;
use tracing::warn;

pub async fn maybe_archive_candidate_block(
    dir: Option<PathBuf>,
    height: u64,
    hash: &str,
    block_hex: &str,
    coinbase_hex: &str,
    template_key: &str,
) -> Option<String> {
    let dir = dir?;
    match archive_candidate_block(dir, height, hash, block_hex, coinbase_hex, template_key).await {
        Ok(path) => Some(path),
        Err(err) => {
            warn!(
                "failed to archive candidate block height={} hash={}: {err:?}",
                height, hash
            );
            None
        }
    }
}

async fn archive_candidate_block(
    dir: PathBuf,
    height: u64,
    hash: &str,
    block_hex: &str,
    coinbase_hex: &str,
    template_key: &str,
) -> anyhow::Result<String> {
    fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("create {}", dir.display()))?;
    let filename = format!("{}-{}.json", height, hash);
    let full_path = dir.join(filename);
    let body = json!({
        "height": height,
        "hash": hash,
        "template_key": template_key,
        "block_hex": block_hex,
        "coinbase_hex": coinbase_hex,
        "saved_at": Utc::now().to_rfc3339(),
    });
    fs::write(&full_path, serde_json::to_vec_pretty(&body)?)
        .await
        .with_context(|| format!("write {}", full_path.display()))?;
    Ok(full_path.display().to_string())
}
