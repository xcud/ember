//! Reverse-resolve an existing `mods/` directory into a `pack.toml` + `pack.lock`.
//!
//! Every `.jar` is hashed (SHA-1) and looked up against Modrinth's content
//! index, which is exact — no filename guessing. Jars Modrinth doesn't know
//! about are recorded as `unresolved` so nothing is silently lost.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use sha1::{Digest, Sha1};

use crate::manifest::{Loader, Lock, LockedMod, Pack, UnresolvedMod};
use crate::modrinth::{Client, Version};

struct ScannedJar {
    filename: String,
    sha1: String,
    size: u64,
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn scan_jars(mods_dir: &Path) -> anyhow::Result<Vec<ScannedJar>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(mods_dir).max_depth(1) {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_jar = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("jar"))
            .unwrap_or(false);
        if !is_jar {
            continue;
        }
        let bytes = std::fs::read(path)?;
        out.push(ScannedJar {
            filename: path.file_name().unwrap().to_string_lossy().into_owned(),
            sha1: sha1_hex(&bytes),
            size: bytes.len() as u64,
        });
    }
    out.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(out)
}

/// Pick the most common value, used to infer pack-wide game version / loader
/// from the set of resolved mods.
fn majority<'a>(items: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for it in items {
        *counts.entry(it).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(k, _)| k.to_string())
}

fn loader_from_str(s: &str) -> Option<Loader> {
    match s {
        "fabric" => Some(Loader::Fabric),
        "forge" => Some(Loader::Forge),
        "neoforge" => Some(Loader::NeoForge),
        "quilt" => Some(Loader::Quilt),
        _ => None,
    }
}

pub struct ImportResult {
    pub pack: Pack,
    pub lock: Lock,
    pub resolved: usize,
    pub unresolved: usize,
}

/// Import `mods_dir`, naming the pack `pack_name`.
///
/// `game_version` / `loader` may be supplied to pin the pack; if `None`, they
/// are inferred by majority vote across the resolved mods.
pub async fn import_mods_dir(
    client: &Client,
    mods_dir: &Path,
    pack_name: &str,
    game_version: Option<String>,
    loader: Option<Loader>,
) -> anyhow::Result<ImportResult> {
    let jars = scan_jars(mods_dir)?;
    let sha1s: Vec<String> = jars.iter().map(|j| j.sha1.clone()).collect();

    let by_hash = client.versions_by_sha1(&sha1s).await?;

    // Resolve project slugs in one batch.
    let project_ids: Vec<String> = by_hash
        .values()
        .map(|v| v.project_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let slug_by_id: HashMap<String, String> = client
        .projects(&project_ids)
        .await?
        .into_iter()
        .map(|p| (p.id, p.slug))
        .collect();

    // Infer pack-wide loader/game version from the resolved set when not given.
    let resolved_versions: Vec<&Version> = jars
        .iter()
        .filter_map(|j| by_hash.get(&j.sha1))
        .collect();

    let loader = loader
        .or_else(|| {
            majority(
                resolved_versions
                    .iter()
                    .flat_map(|v| v.loaders.iter())
                    .filter(|l| loader_from_str(l).is_some())
                    .map(|s| s.as_str()),
            )
            .and_then(|s| loader_from_str(&s))
        })
        .unwrap_or(Loader::Fabric);

    let game_version = game_version
        .or_else(|| {
            majority(
                resolved_versions
                    .iter()
                    .flat_map(|v| v.game_versions.iter())
                    // Prefer stable releases over snapshots when inferring.
                    .filter(|g| g.starts_with('1') && !g.contains('w'))
                    .map(|s| s.as_str()),
            )
        })
        .unwrap_or_else(|| "unknown".to_string());

    let mut mods_req: BTreeMap<String, String> = BTreeMap::new();
    let mut locked: Vec<LockedMod> = Vec::new();
    let mut unresolved: Vec<UnresolvedMod> = Vec::new();

    for jar in &jars {
        match by_hash.get(&jar.sha1) {
            Some(v) => {
                let slug = slug_by_id
                    .get(&v.project_id)
                    .cloned()
                    .unwrap_or_else(|| v.project_id.clone());
                // Prefer the file whose hash matches ours; fall back to primary.
                let file = v
                    .files
                    .iter()
                    .find(|f| f.hashes.sha1.as_deref() == Some(jar.sha1.as_str()))
                    .or_else(|| v.files.iter().find(|f| f.primary))
                    .or_else(|| v.files.first());
                let (url, filename, size) = match file {
                    Some(f) => (f.url.clone(), f.filename.clone(), f.size),
                    None => (String::new(), jar.filename.clone(), jar.size),
                };
                mods_req.insert(slug.clone(), "*".to_string());
                locked.push(LockedMod {
                    slug,
                    project_id: v.project_id.clone(),
                    version_id: v.id.clone(),
                    version_number: v.version_number.clone(),
                    filename,
                    sha1: jar.sha1.clone(),
                    url,
                    size,
                });
            }
            None => unresolved.push(UnresolvedMod {
                filename: jar.filename.clone(),
                sha1: jar.sha1.clone(),
                size: jar.size,
            }),
        }
    }

    locked.sort_by(|a, b| a.slug.cmp(&b.slug));

    let pack = Pack {
        name: pack_name.to_string(),
        game_version: game_version.clone(),
        loader,
        loader_version: None,
        mods: mods_req,
    };
    let lock = Lock {
        game_version,
        loader,
        mods: locked,
        unresolved,
    };

    Ok(ImportResult {
        resolved: lock.mods.len(),
        unresolved: lock.unresolved.len(),
        pack,
        lock,
    })
}
