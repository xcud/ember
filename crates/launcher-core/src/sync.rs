//! `ember sync` — make a `mods/` directory exactly match a `pack.lock`.
//!
//! Reads the lock, ensures every mod is present and hash-verified (pulling from
//! the content-addressed cache, downloading only what's missing), all in
//! parallel under a concurrency cap. Optionally prunes stray jars so the
//! directory is a faithful reproduction of the lock.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::download::{self, CacheOutcome};
use crate::manifest::Lock;

pub struct SyncOptions {
    pub concurrency: usize,
    pub cache_dir: PathBuf,
    /// Remove `.jar` files in the mods dir that aren't in the lock.
    pub prune: bool,
}

impl SyncOptions {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { concurrency: 8, cache_dir, prune: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModStatus {
    /// On disk and verified — nothing to do.
    UpToDate,
    /// Copied in from the cache (no network).
    Cached,
    /// Downloaded fresh, then installed.
    Downloaded,
}

#[derive(Debug, Clone)]
pub struct ModResult {
    pub slug: String,
    pub filename: String,
    pub status: ModStatus,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub results: Vec<ModResult>,
    pub failures: Vec<(String, String)>, // (slug, error)
    pub pruned: Vec<String>,
}

impl SyncReport {
    pub fn count(&self, status: ModStatus) -> usize {
        self.results.iter().filter(|r| r.status == status).count()
    }
}

async fn sync_one(
    http: reqwest::Client,
    cache_dir: PathBuf,
    mods_dir: PathBuf,
    slug: String,
    filename: String,
    url: String,
    sha1: String,
) -> Result<ModStatus, String> {
    let dest = mods_dir.join(&filename);

    // Already correct on disk? Skip entirely.
    if download::verify_file(&dest, &sha1) {
        return Ok(ModStatus::UpToDate);
    }
    if url.is_empty() {
        return Err(format!("{slug}: no download URL in lock"));
    }

    let (cached, outcome) = download::ensure_cached(&http, &cache_dir, &url, &sha1)
        .await
        .map_err(|e| format!("{slug}: {e}"))?;
    download::install(&cached, &dest).map_err(|e| format!("{slug}: install: {e}"))?;

    Ok(match outcome {
        CacheOutcome::Hit => ModStatus::Cached,
        CacheOutcome::Downloaded => ModStatus::Downloaded,
    })
}

/// Reproduce `lock` into `mods_dir`.
pub async fn sync(
    http: &reqwest::Client,
    lock: &Lock,
    mods_dir: &Path,
    opts: &SyncOptions,
) -> anyhow::Result<SyncReport> {
    std::fs::create_dir_all(mods_dir)?;
    let sem = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut tasks = Vec::new();

    for m in &lock.mods {
        let permit_sem = sem.clone();
        let http = http.clone();
        let cache_dir = opts.cache_dir.clone();
        let mods_dir = mods_dir.to_path_buf();
        let (slug, filename, url, sha1) =
            (m.slug.clone(), m.filename.clone(), m.url.clone(), m.sha1.clone());

        tasks.push(tokio::spawn(async move {
            let _permit = permit_sem.acquire_owned().await.unwrap();
            let status = sync_one(
                http,
                cache_dir,
                mods_dir,
                slug.clone(),
                filename.clone(),
                url,
                sha1,
            )
            .await;
            (slug, filename, status)
        }));
    }

    let mut report = SyncReport::default();
    for t in tasks {
        let (slug, filename, status) = t.await.map_err(|e| anyhow::anyhow!("task join: {e}"))?;
        match status {
            Ok(status) => report.results.push(ModResult { slug, filename, status }),
            Err(e) => report.failures.push((slug, e)),
        }
    }

    if opts.prune {
        let keep: HashSet<&str> = lock.mods.iter().map(|m| m.filename.as_str()).collect();
        for entry in std::fs::read_dir(mods_dir)? {
            let path = entry?.path();
            let is_jar = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("jar"))
                .unwrap_or(false);
            if !is_jar {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !keep.contains(name.as_str()) {
                std::fs::remove_file(&path)?;
                report.pruned.push(name);
            }
        }
        report.pruned.sort();
    }

    report.results.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(report)
}

/// Load a `pack.lock` from disk.
pub fn load_lock(path: &Path) -> anyhow::Result<Lock> {
    let text = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&text)?)
}
