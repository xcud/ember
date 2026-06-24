//! Streaming, hash-verified downloads backed by a content-addressed cache.
//!
//! Files are stored under `<cache>/objects/<sha1>`. Because the key *is* the
//! content hash, the same library shared across packs/versions is fetched once,
//! and a corrupt or truncated download can never masquerade as a cache hit —
//! it simply fails verification and is discarded.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;

/// Where a verified object lives in the cache.
pub fn object_path(cache_dir: &Path, sha1: &str) -> PathBuf {
    cache_dir.join("objects").join(sha1)
}

fn file_sha1(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha1::new();
    h.update(&bytes);
    Ok(hex::encode(h.finalize()))
}

/// True if `path` exists and its SHA-1 matches `expected`.
pub fn verify_file(path: &Path, expected: &str) -> bool {
    path.is_file() && file_sha1(path).map(|h| h == expected).unwrap_or(false)
}

/// Outcome of ensuring one object is present in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// Already present and verified — no network.
    Hit,
    /// Downloaded fresh.
    Downloaded,
}

/// Ensure the object with `expected_sha1` is present in the cache, downloading
/// from `url` if needed. Returns its cache path and how we got it.
///
/// The download streams to a temp file while hashing on the fly, verifies, then
/// atomically renames into the cache — so a partial download is never visible.
pub async fn ensure_cached(
    http: &reqwest::Client,
    cache_dir: &Path,
    url: &str,
    expected_sha1: &str,
) -> anyhow::Result<(PathBuf, CacheOutcome)> {
    let dest = object_path(cache_dir, expected_sha1);
    if verify_file(&dest, expected_sha1) {
        return Ok((dest, CacheOutcome::Hit));
    }
    std::fs::create_dir_all(dest.parent().unwrap())?;

    let tmp = dest.with_extension(format!("tmp-{expected_sha1}"));
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut hasher = Sha1::new();

    let resp = http.get(url).send().await?.error_for_status()?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    let got = hex::encode(hasher.finalize());
    if got != expected_sha1 {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!("hash mismatch for {url}: expected {expected_sha1}, got {got}");
    }
    std::fs::rename(&tmp, &dest)?;
    Ok((dest, CacheOutcome::Downloaded))
}

/// Place a cached object at `dest` (the live mods dir). Tries a hard link first
/// to save disk, falling back to a copy across filesystems.
pub fn install(cached: &Path, dest: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    match std::fs::hard_link(cached, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(cached, dest)?;
            Ok(())
        }
    }
}
