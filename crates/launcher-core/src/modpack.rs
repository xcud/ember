//! Modrinth modpack (`.mrpack`) import.
//!
//! An `.mrpack` is a zip containing `modrinth.index.json` (game version, loader,
//! and a list of files with download URLs + hashes) plus optional `overrides/`
//! directories of loose config files. Importing one creates a managed instance,
//! downloads every file through the verified content-addressed cache, lays down
//! the overrides, and writes a `pack.lock` so the instance can be re-synced
//! later. "Embrace" the de-facto standard format; "extend" from there.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde::Deserialize;

use crate::download;
use crate::instance::Instance;
use crate::manifest::{Loader, Lock, LockedMod};

#[derive(Debug, Deserialize)]
struct MrIndex {
    #[serde(default)]
    name: String,
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    files: Vec<MrFile>,
}

#[derive(Debug, Deserialize)]
struct MrFile {
    path: String,
    #[serde(default)]
    downloads: Vec<String>,
    hashes: MrHashes,
    #[serde(default, rename = "fileSize")]
    file_size: u64,
    #[serde(default)]
    env: Option<MrEnv>,
}

#[derive(Debug, Deserialize)]
struct MrHashes {
    #[serde(default)]
    sha1: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MrEnv {
    #[serde(default)]
    client: Option<String>,
}

pub struct ImportReport {
    pub instance: Instance,
    pub pack_name: String,
    pub game_version: String,
    pub loader: Loader,
    pub version_id: String,
    pub installed: usize,
    pub skipped: usize,
    pub overrides: usize,
    /// Whether the required loader version is already installed in `mc_home`.
    /// If false, the instance imports fine but can't launch until installed.
    pub version_installed: bool,
}

fn detect_loader(deps: &BTreeMap<String, String>) -> (Loader, String) {
    for (key, loader) in [
        ("fabric-loader", Loader::Fabric),
        ("quilt-loader", Loader::Quilt),
        ("neoforge", Loader::NeoForge),
        ("forge", Loader::Forge),
    ] {
        if let Some(v) = deps.get(key) {
            return (loader, v.clone());
        }
    }
    (Loader::Vanilla, String::new())
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

fn stem(path: &str) -> String {
    let base = basename(path);
    base.strip_suffix(".jar").map(|s| s.to_string()).unwrap_or(base)
}

/// Import `mrpack` into a new instance named `name`.
pub async fn import_mrpack(
    http: &reqwest::Client,
    cache_dir: &Path,
    mrpack: &Path,
    name: &str,
    mc_home: std::path::PathBuf,
    max_mb: u32,
) -> anyhow::Result<ImportReport> {
    let file = std::fs::File::open(mrpack)
        .map_err(|e| anyhow::anyhow!("opening {}: {e}", mrpack.display()))?;
    let mut zip = zip::ZipArchive::new(file)?;

    let index: MrIndex = {
        let mut f = zip
            .by_name("modrinth.index.json")
            .map_err(|_| anyhow::anyhow!("not a valid .mrpack: no modrinth.index.json"))?;
        let mut s = String::new();
        f.read_to_string(&mut s)?;
        serde_json::from_str(&s)?
    };

    let game_version = index.dependencies.get("minecraft").cloned().unwrap_or_default();
    let (loader, loader_version) = detect_loader(&index.dependencies);
    let version_id = match loader {
        Loader::Fabric => format!("fabric-loader-{loader_version}-{game_version}"),
        Loader::Quilt => format!("quilt-loader-{loader_version}-{game_version}"),
        Loader::NeoForge => format!("neoforge-{loader_version}"),
        Loader::Forge => format!("{game_version}-forge-{loader_version}"),
        Loader::Vanilla => game_version.clone(),
    };

    let inst = Instance::create(name, &version_id, mc_home.clone(), max_mb)?;
    let game_dir = inst.game_dir().to_path_buf();

    // Download every client-relevant file into its declared path.
    let mut installed = 0usize;
    let mut skipped = 0usize;
    let mut locked: Vec<LockedMod> = Vec::new();
    for f in &index.files {
        if let Some(env) = &f.env {
            if env.client.as_deref() == Some("unsupported") {
                skipped += 1;
                continue;
            }
        }
        let (Some(url), Some(sha1)) = (f.downloads.first(), f.hashes.sha1.as_ref()) else {
            skipped += 1;
            continue;
        };
        let (cached, _) = download::ensure_cached(http, cache_dir, url, sha1).await?;
        download::install(&cached, &game_dir.join(&f.path))?;
        installed += 1;
        locked.push(LockedMod {
            slug: stem(&f.path),
            project_id: String::new(),
            version_id: String::new(),
            version_number: String::new(),
            filename: basename(&f.path),
            sha1: sha1.clone(),
            url: url.clone(),
            size: f.file_size,
        });
    }

    // Extract overrides/ and client-overrides/ over the game dir.
    let mut overrides = 0usize;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        let rel = name
            .strip_prefix("overrides/")
            .or_else(|| name.strip_prefix("client-overrides/"));
        let Some(rel) = rel else { continue };
        if rel.is_empty() || entry.is_dir() {
            continue;
        }
        let dest = game_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        std::fs::write(&dest, buf)?;
        overrides += 1;
    }

    // Write a pack.lock so the instance can be re-synced/verified later.
    locked.sort_by(|a, b| a.filename.cmp(&b.filename));
    let lock = Lock { game_version: game_version.clone(), loader, mods: locked, unresolved: Vec::new() };
    lock.write(inst.lock_path())?;

    let version_installed = mc_home
        .join("versions")
        .join(&version_id)
        .join(format!("{version_id}.json"))
        .exists();

    Ok(ImportReport {
        instance: inst,
        pack_name: index.name,
        game_version,
        loader,
        version_id,
        installed,
        skipped,
        overrides,
        version_installed,
    })
}
