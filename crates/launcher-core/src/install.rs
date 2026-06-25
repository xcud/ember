//! Version installation — making ember self-sufficient.
//!
//! Fetches everything needed to launch a version without relying on the
//! official launcher having downloaded it first:
//!   - vanilla: the version JSON, client jar, rule-filtered libraries, and the
//!     asset index + objects (from Mojang's `version_manifest_v2`);
//!   - Fabric: the loader profile + its libraries (from `meta.fabricmc.net`).
//!
//! Every file goes through the same verified path as `sync`: if it already
//! exists on disk with the right SHA-1 it's skipped (so installing a version
//! that shares libraries/assets with an existing install is nearly free),
//! otherwise it's streamed through the content-addressed cache and verified.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Semaphore;

use crate::download::{self, verify_file};
use crate::launch::{self, Host};

const VERSION_MANIFEST: &str =
    "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const RESOURCES: &str = "https://resources.download.minecraft.net";

fn fabric_profile_url(mc: &str, loader: &str) -> String {
    format!("https://meta.fabricmc.net/v2/versions/loader/{mc}/{loader}/profile/json")
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent("ember/0.1.0").build()?)
}

async fn fetch_json(http: &reqwest::Client, url: &str) -> anyhow::Result<Value> {
    Ok(http.get(url).send().await?.error_for_status()?.json().await?)
}

/// One file to place on disk: download `url`, verify `sha1`, write to `dest`.
struct FileSpec {
    url: String,
    sha1: String,
    dest: PathBuf,
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub version_id: String,
    pub downloaded: usize,
    pub skipped: usize,
    pub failures: Vec<String>,
}

/// Download a batch of files in parallel, skipping those already valid on disk.
async fn install_files(
    http: &reqwest::Client,
    cache_dir: &Path,
    specs: Vec<FileSpec>,
    concurrency: usize,
    progress: Arc<dyn Fn(usize, usize) + Send + Sync>,
) -> (usize, usize, Vec<String>) {
    let total = specs.len();
    let done = Arc::new(AtomicUsize::new(0));
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut tasks = Vec::new();

    for spec in specs {
        let sem = sem.clone();
        let http = http.clone();
        let cache_dir = cache_dir.to_path_buf();
        let done = done.clone();
        let progress = progress.clone();
        tasks.push(tokio::spawn(async move {
            let _p = sem.acquire_owned().await.unwrap();
            let result: Result<bool, String> = async {
                if verify_file(&spec.dest, &spec.sha1) {
                    return Ok(false); // skipped
                }
                let (cached, _) = download::ensure_cached(&http, &cache_dir, &spec.url, &spec.sha1)
                    .await
                    .map_err(|e| format!("{}: {e}", spec.url))?;
                download::install(&cached, &spec.dest).map_err(|e| format!("{}: {e}", spec.dest.display()))?;
                Ok(true) // downloaded
            }
            .await;
            let n = done.fetch_add(1, Ordering::SeqCst) + 1;
            progress(n, total);
            result
        }));
    }

    let (mut downloaded, mut skipped, mut failures) = (0usize, 0usize, Vec::new());
    for t in tasks {
        match t.await {
            Ok(Ok(true)) => downloaded += 1,
            Ok(Ok(false)) => skipped += 1,
            Ok(Err(e)) => failures.push(e),
            Err(e) => failures.push(format!("task: {e}")),
        }
    }
    (downloaded, skipped, failures)
}

/// Collect the rule-filtered library FileSpecs from a version JSON.
fn library_specs(vjson: &Value, mc_home: &Path, host: &Host) -> Vec<FileSpec> {
    let lib_dir = mc_home.join("libraries");
    let mut specs = Vec::new();
    if let Some(libs) = vjson.get("libraries").and_then(|l| l.as_array()) {
        for lib in libs {
            if let Some(rules) = lib.get("rules") {
                if !launch::rules_allow(rules, host) {
                    continue;
                }
            }
            // Modern format: downloads.artifact has path/url/sha1.
            if let Some(art) = lib.pointer("/downloads/artifact") {
                let (Some(path), Some(url), Some(sha1)) = (
                    art.get("path").and_then(|p| p.as_str()),
                    art.get("url").and_then(|u| u.as_str()),
                    art.get("sha1").and_then(|s| s.as_str()),
                ) else {
                    continue;
                };
                specs.push(FileSpec {
                    url: url.to_string(),
                    sha1: sha1.to_string(),
                    dest: lib_dir.join(path),
                });
            }
        }
    }
    specs
}

/// Install a vanilla Minecraft version (JSON, client jar, libraries, assets).
pub async fn install_vanilla(
    mc_home: &Path,
    cache_dir: &Path,
    mc_version: &str,
    host: &Host,
    concurrency: usize,
    progress: Arc<dyn Fn(&str, usize, usize) + Send + Sync>,
) -> anyhow::Result<InstallReport> {
    let http = http_client()?;

    // Locate the version's JSON via the manifest.
    let manifest = fetch_json(&http, VERSION_MANIFEST).await?;
    let url = manifest["versions"]
        .as_array()
        .and_then(|vs| vs.iter().find(|v| v["id"].as_str() == Some(mc_version)))
        .and_then(|v| v["url"].as_str())
        .ok_or_else(|| anyhow::anyhow!("unknown Minecraft version '{mc_version}'"))?
        .to_string();
    let vjson = fetch_json(&http, &url).await?;

    // version JSON to disk.
    let vdir = mc_home.join("versions").join(mc_version);
    std::fs::create_dir_all(&vdir)?;
    std::fs::write(vdir.join(format!("{mc_version}.json")), serde_json::to_vec_pretty(&vjson)?)?;

    let mut report = InstallReport { version_id: mc_version.to_string(), ..Default::default() };

    // Client jar + libraries.
    let mut core_specs = Vec::new();
    if let Some(client) = vjson.pointer("/downloads/client") {
        if let (Some(u), Some(s)) = (client["url"].as_str(), client["sha1"].as_str()) {
            core_specs.push(FileSpec {
                url: u.to_string(),
                sha1: s.to_string(),
                dest: vdir.join(format!("{mc_version}.jar")),
            });
        }
    }
    core_specs.extend(library_specs(&vjson, mc_home, host));
    {
        let p = progress.clone();
        let cb: Arc<dyn Fn(usize, usize) + Send + Sync> =
            Arc::new(move |n, t| p("libraries", n, t));
        let (d, s, f) = install_files(&http, cache_dir, core_specs, concurrency, cb).await;
        report.downloaded += d;
        report.skipped += s;
        report.failures.extend(f);
    }

    // Assets: index + objects.
    if let Some(ai) = vjson.get("assetIndex") {
        let (ai_id, ai_url) = (
            ai["id"].as_str().unwrap_or("legacy").to_string(),
            ai["url"].as_str().unwrap_or_default().to_string(),
        );
        let index = fetch_json(&http, &ai_url).await?;
        let idx_dir = mc_home.join("assets").join("indexes");
        std::fs::create_dir_all(&idx_dir)?;
        std::fs::write(idx_dir.join(format!("{ai_id}.json")), serde_json::to_vec(&index)?)?;

        let objects_dir = mc_home.join("assets").join("objects");
        let mut asset_specs = Vec::new();
        if let Some(objs) = index.get("objects").and_then(|o| o.as_object()) {
            for obj in objs.values() {
                if let Some(hash) = obj["hash"].as_str() {
                    let sub = &hash[0..2];
                    asset_specs.push(FileSpec {
                        url: format!("{RESOURCES}/{sub}/{hash}"),
                        sha1: hash.to_string(),
                        dest: objects_dir.join(sub).join(hash),
                    });
                }
            }
        }
        let p = progress.clone();
        let cb: Arc<dyn Fn(usize, usize) + Send + Sync> =
            Arc::new(move |n, t| p("assets", n, t));
        let (d, s, f) = install_files(&http, cache_dir, asset_specs, concurrency, cb).await;
        report.downloaded += d;
        report.skipped += s;
        report.failures.extend(f);
    }

    Ok(report)
}

/// Install a Fabric loader for `mc_version` (installing the vanilla base too).
/// Returns the launchable version id, e.g. `fabric-loader-0.16.9-1.21.1`.
pub async fn install_fabric(
    mc_home: &Path,
    cache_dir: &Path,
    mc_version: &str,
    loader_version: &str,
    host: &Host,
    concurrency: usize,
    progress: Arc<dyn Fn(&str, usize, usize) + Send + Sync>,
) -> anyhow::Result<InstallReport> {
    // The vanilla base must exist first (Fabric inheritsFrom it).
    let mut report = install_vanilla(mc_home, cache_dir, mc_version, host, concurrency, progress.clone()).await?;

    let http = http_client()?;
    let profile = fetch_json(&http, &fabric_profile_url(mc_version, loader_version)).await?;
    let id = profile["id"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("fabric-loader-{loader_version}-{mc_version}"));

    let vdir = mc_home.join("versions").join(&id);
    std::fs::create_dir_all(&vdir)?;
    std::fs::write(vdir.join(format!("{id}.json")), serde_json::to_vec_pretty(&profile)?)?;

    // Fabric libraries: name + maven `url` base + sha1 (no downloads block).
    let lib_dir = mc_home.join("libraries");
    let mut specs = Vec::new();
    if let Some(libs) = profile.get("libraries").and_then(|l| l.as_array()) {
        for lib in libs {
            let (Some(name), Some(base)) = (
                lib.get("name").and_then(|n| n.as_str()),
                lib.get("url").and_then(|u| u.as_str()),
            ) else {
                continue;
            };
            let Some(path) = launch::maven_to_path(name) else { continue };
            let sha1 = lib.get("sha1").and_then(|s| s.as_str()).unwrap_or_default();
            let base = base.trim_end_matches('/');
            specs.push(FileSpec {
                url: format!("{base}/{path}"),
                sha1: sha1.to_string(),
                dest: lib_dir.join(&path),
            });
        }
    }

    // Some Fabric libs ship without a sha1; download those without verification.
    let (verified, unverified): (Vec<_>, Vec<_>) =
        specs.into_iter().partition(|s| !s.sha1.is_empty());

    {
        let p = progress.clone();
        let cb: Arc<dyn Fn(usize, usize) + Send + Sync> = Arc::new(move |n, t| p("fabric", n, t));
        let (d, s, f) = install_files(&http, cache_dir, verified, concurrency, cb).await;
        report.downloaded += d;
        report.skipped += s;
        report.failures.extend(f);
    }
    for spec in unverified {
        if spec.dest.exists() {
            report.skipped += 1;
            continue;
        }
        match http.get(&spec.url).send().await.and_then(|r| r.error_for_status()) {
            Ok(resp) => {
                let bytes = resp.bytes().await.unwrap_or_default();
                if let Some(parent) = spec.dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::write(&spec.dest, &bytes).is_ok() {
                    report.downloaded += 1;
                } else {
                    report.failures.push(format!("write {}", spec.dest.display()));
                }
            }
            Err(e) => report.failures.push(format!("{}: {e}", spec.url)),
        }
    }

    report.version_id = id;
    Ok(report)
}
