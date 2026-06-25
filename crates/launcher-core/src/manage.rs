//! Instance mod management: resolve a real pack, add (with dependency
//! expansion), remove, and update mods for an instance.
//!
//! Modpack-imported / linked instances may have a `pack.lock` with synthetic
//! slugs (or none at all). [`ensure_pack`] hash-resolves the instance's actual
//! jars into a real `pack.toml` + `pack.lock` (Modrinth content lookup), giving
//! genuine slugs so update/add operations are accurate.

use std::collections::HashSet;
use std::path::Path;

use crate::import;
use crate::instance::Instance;
use crate::manifest::{Loader, Lock, LockedMod, Pack};
use crate::modrinth::Client;
use crate::sync::{self, SyncOptions};
use crate::update;
use crate::{download, modrinth};

/// Load the instance's pack, hash-resolving its current mods into a real
/// `pack.toml` + `pack.lock` the first time (so slugs are genuine).
pub async fn ensure_pack(client: &Client, instance: &Instance) -> anyhow::Result<(Pack, Lock)> {
    let pack_path = instance.pack_path();
    let lock_path = instance.lock_path();

    if pack_path.exists() {
        let pack = Pack::load(&pack_path)?;
        let lock = if lock_path.exists() {
            sync::load_lock(&lock_path)?
        } else {
            Lock { game_version: pack.game_version.clone(), loader: pack.loader, mods: Vec::new(), unresolved: Vec::new() }
        };
        // A real pack from `ember import` has Modrinth slugs; one synthesized by
        // modpack import has filename-stem slugs with empty project_ids. If the
        // lock looks synthetic, re-resolve by hash for accuracy.
        let synthetic = lock.mods.iter().any(|m| m.project_id.is_empty());
        if !synthetic {
            return Ok((pack, lock));
        }
    }

    let result = import::import_mods_dir(client, &instance.mods_dir(), &instance.config.name, None, None).await?;
    result.pack.write(&pack_path)?;
    result.lock.write(&lock_path)?;
    Ok((result.pack, result.lock))
}

pub struct AddReport {
    pub installed: Vec<String>,    // "slug version_number"
    pub already: Vec<String>,      // slugs already present
    pub incompatible: Vec<String>, // slugs/ids with no compatible build
}

/// The loader + game version for an instance, derived cheaply from its version
/// id (no hash resolve needed — used for search before any pack exists).
fn instance_loader_gv(instance: &Instance) -> (Loader, String) {
    let vid = &instance.config.version_id;
    let loader = if vid.starts_with("fabric") {
        Loader::Fabric
    } else if vid.starts_with("quilt") {
        Loader::Quilt
    } else if vid.starts_with("neoforge") {
        Loader::NeoForge
    } else if vid.contains("forge") {
        Loader::Forge
    } else {
        Loader::Fabric
    };
    let host = crate::launch::Host::current();
    let gv = crate::launch::resolve(&instance.config.mc_home.join("versions"), vid, &host)
        .map(|r| r.root_id)
        .unwrap_or_default();
    (loader, gv)
}

/// Search Modrinth for mods compatible with this instance.
pub async fn search(
    client: &Client,
    instance: &Instance,
    query: &str,
) -> anyhow::Result<Vec<SearchHit>> {
    let (loader, gv) = instance_loader_gv(instance);
    client.search(query, loader.modrinth_id(), &gv).await
}

/// Install `start_ident` (slug or project id) and its required dependencies,
/// mutating `pack`/`lock`/`report`.
async fn install_tree(
    client: &Client,
    cache_dir: &Path,
    instance: &Instance,
    pack: &mut Pack,
    lock: &mut Lock,
    loader: Loader,
    gv: &str,
    start_ident: &str,
    report: &mut AddReport,
) -> anyhow::Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![start_ident.to_string()];
    while let Some(ident) = queue.pop() {
        if !seen.insert(ident.clone()) {
            continue;
        }
        let versions = client.project_versions(&ident, loader.modrinth_id(), gv).await?;
        let Some(best) = versions.into_iter().next() else {
            report.incompatible.push(ident);
            continue;
        };
        let slug = client
            .projects(&[best.project_id.clone()])
            .await
            .ok()
            .and_then(|ps| ps.into_iter().next())
            .map(|p| p.slug)
            .unwrap_or_else(|| ident.clone());

        if pack.mods.contains_key(&slug) {
            report.already.push(slug);
        } else if let Some(file) = best.primary_file() {
            let (cached, _) = download::ensure_cached(
                client.http(),
                cache_dir,
                &file.url,
                file.hashes.sha1.as_deref().unwrap_or_default(),
            )
            .await?;
            download::install(&cached, &instance.mods_dir().join(&file.filename))?;
            pack.mods.insert(slug.clone(), "*".to_string());
            lock.mods.retain(|m| m.slug != slug);
            lock.mods.push(LockedMod {
                slug: slug.clone(),
                project_id: best.project_id.clone(),
                version_id: best.id.clone(),
                version_number: best.version_number.clone(),
                filename: file.filename.clone(),
                sha1: file.hashes.sha1.clone().unwrap_or_default(),
                url: file.url.clone(),
                size: file.size,
            });
            report.installed.push(format!("{slug} {}", best.version_number));
        }

        for dep in &best.dependencies {
            if dep.dependency_type == "required" {
                if let Some(pid) = &dep.project_id {
                    queue.push(pid.clone());
                }
            }
        }
    }
    Ok(())
}

/// Add a specific Modrinth project (by slug or id) and its required deps.
pub async fn add_project(
    client: &Client,
    cache_dir: &Path,
    instance: &Instance,
    ident: &str,
) -> anyhow::Result<AddReport> {
    let (mut pack, mut lock) = ensure_pack(client, instance).await?;
    let (loader, fallback_gv) = instance_loader_gv(instance);
    let gv = if pack.game_version != "unknown" && !pack.game_version.is_empty() {
        pack.game_version.clone()
    } else {
        fallback_gv
    };
    let mut report = AddReport { installed: Vec::new(), already: Vec::new(), incompatible: Vec::new() };
    install_tree(client, cache_dir, instance, &mut pack, &mut lock, loader, &gv, ident, &mut report).await?;
    lock.mods.sort_by(|a, b| a.slug.cmp(&b.slug));
    pack.write(&instance.pack_path())?;
    lock.write(&instance.lock_path())?;
    Ok(report)
}

/// Search and add the top hit (CLI convenience).
pub async fn add_mod(
    client: &Client,
    cache_dir: &Path,
    instance: &Instance,
    query: &str,
) -> anyhow::Result<AddReport> {
    let hits = search(client, instance, query).await?;
    let start = hits
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no results for '{query}'"))?;
    add_project(client, cache_dir, instance, &start.slug).await
}

/// Remove a mod jar from an instance and drop it from the pack/lock.
pub fn remove_mod(instance: &Instance, filename: &str) -> anyhow::Result<()> {
    let path = instance.mods_dir().join(filename);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let lock_path = instance.lock_path();
    if lock_path.exists() {
        let mut lock = sync::load_lock(&lock_path)?;
        let removed_slug = lock.mods.iter().find(|m| m.filename == filename).map(|m| m.slug.clone());
        lock.mods.retain(|m| m.filename != filename);
        lock.write(&lock_path)?;
        if let (Some(slug), true) = (removed_slug, instance.pack_path().exists()) {
            if let Ok(mut pack) = Pack::load(instance.pack_path()) {
                pack.mods.remove(&slug);
                let _ = pack.write(&instance.pack_path());
            }
        }
    }
    Ok(())
}

pub struct UpdateSummary {
    pub updated: usize,
    pub added: usize,
    pub incompatible: usize,
    pub downloaded: usize,
}

/// Re-resolve an instance's mods to the latest compatible builds and apply.
/// Only superseded versions of managed mods are removed (never unknown jars).
pub async fn update_instance(
    client: &Client,
    cache_dir: &Path,
    instance: &Instance,
) -> anyhow::Result<UpdateSummary> {
    let (pack, old_lock) = ensure_pack(client, instance).await?;
    let plan = update::plan(client, &pack, Some(&old_lock), None, 8).await?;
    let new_lock = plan.new_lock();

    // Remove old versions of mods whose filenames changed.
    let new_files: HashSet<&str> = new_lock.mods.iter().map(|m| m.filename.as_str()).collect();
    for m in &old_lock.mods {
        if !new_files.contains(m.filename.as_str()) {
            let p = instance.mods_dir().join(&m.filename);
            if p.exists() {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    new_lock.write(&instance.lock_path())?;
    let opts = SyncOptions { concurrency: 8, cache_dir: cache_dir.to_path_buf(), prune: false };
    let report = sync::sync(client.http(), &new_lock, &instance.mods_dir(), &opts).await?;

    let updated = plan.updates.iter().filter(|u| matches!(u.change, update::Change::Updated { .. })).count();
    let added = plan.updates.iter().filter(|u| matches!(u.change, update::Change::Added)).count();
    let incompatible = plan.updates.iter().filter(|u| matches!(u.change, update::Change::Incompatible)).count();
    Ok(UpdateSummary {
        updated,
        added,
        incompatible,
        downloaded: report.count(sync::ModStatus::Downloaded),
    })
}

// Re-export for callers that pattern-match on hits.
pub use modrinth::SearchHit;
