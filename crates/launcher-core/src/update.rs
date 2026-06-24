//! `ember update` / `bump` — re-resolve a `pack.toml` to the latest compatible
//! builds and diff against the current lock.
//!
//! For each requested mod we ask Modrinth for its versions filtered by the
//! pack's loader and target game version, take the newest, and compare to
//! what's locked. Mods with no compatible build are reported as `incompatible`
//! (the exact pain of a version bump) rather than silently dropped.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tokio::sync::Semaphore;

use crate::manifest::{Loader, Lock, LockedMod, Pack};
use crate::modrinth::Client;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Newly added to the lock.
    Added,
    /// Version changed: (old_version_number -> new).
    Updated { from: String },
    /// Already at the latest compatible build.
    Unchanged,
    /// No build exists for the target loader + game version.
    Incompatible,
}

#[derive(Debug, Clone)]
pub struct ModUpdate {
    pub slug: String,
    pub change: Change,
    /// The freshly resolved lock entry (absent when incompatible).
    pub locked: Option<LockedMod>,
}

pub struct UpdatePlan {
    pub game_version: String,
    pub loader: Loader,
    pub updates: Vec<ModUpdate>,
    /// Slugs present in the old lock but no longer requested by the pack.
    pub removed: Vec<String>,
}

impl UpdatePlan {
    pub fn changed(&self) -> bool {
        !self.removed.is_empty()
            || self
                .updates
                .iter()
                .any(|u| !matches!(u.change, Change::Unchanged))
    }

    /// Build the new lock from the resolved (compatible) mods.
    pub fn new_lock(&self) -> Lock {
        let mut mods: Vec<LockedMod> = self
            .updates
            .iter()
            .filter_map(|u| u.locked.clone())
            .collect();
        mods.sort_by(|a, b| a.slug.cmp(&b.slug));
        Lock { game_version: self.game_version.clone(), loader: self.loader, mods, unresolved: Vec::new() }
    }
}

async fn resolve_one(
    client: Client,
    slug: String,
    loader: Loader,
    game_version: String,
) -> (String, Option<LockedMod>) {
    let versions = match client
        .project_versions(&slug, loader.modrinth_id(), &game_version)
        .await
    {
        Ok(v) => v,
        Err(_) => return (slug, None),
    };
    let Some(best) = versions.into_iter().next() else {
        return (slug, None);
    };
    let Some(file) = best.primary_file() else {
        return (slug, None);
    };
    let locked = LockedMod {
        slug: slug.clone(),
        project_id: best.project_id.clone(),
        version_id: best.id.clone(),
        version_number: best.version_number.clone(),
        filename: file.filename.clone(),
        sha1: file.hashes.sha1.clone().unwrap_or_default(),
        url: file.url.clone(),
        size: file.size,
    };
    (slug, Some(locked))
}

/// Compute an update plan. `target_game` overrides the pack's game version
/// (that's what `bump` does); pass `None` for a plain in-place `update`.
pub async fn plan(
    client: &Client,
    pack: &Pack,
    old_lock: Option<&Lock>,
    target_game: Option<String>,
    concurrency: usize,
) -> anyhow::Result<UpdatePlan> {
    let game_version = target_game.unwrap_or_else(|| pack.game_version.clone());
    let loader = pack.loader;

    let old_by_slug: HashMap<&str, &LockedMod> = old_lock
        .map(|l| l.mods.iter().map(|m| (m.slug.as_str(), m)).collect())
        .unwrap_or_default();

    // Resolve every requested mod in parallel under a concurrency cap.
    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut tasks = Vec::new();
    for slug in pack.mods.keys() {
        let sem = sem.clone();
        let client = client.clone();
        let slug = slug.clone();
        let game = game_version.clone();
        tasks.push(tokio::spawn(async move {
            let _p = sem.acquire_owned().await.unwrap();
            resolve_one(client, slug, loader, game).await
        }));
    }

    let mut resolved: BTreeMap<String, Option<LockedMod>> = BTreeMap::new();
    for t in tasks {
        let (slug, locked) = t.await.map_err(|e| anyhow::anyhow!("task join: {e}"))?;
        resolved.insert(slug, locked);
    }

    let mut updates = Vec::new();
    for (slug, locked) in resolved {
        let change = match &locked {
            None => Change::Incompatible,
            Some(new) => match old_by_slug.get(slug.as_str()) {
                None => Change::Added,
                Some(old) if old.version_number != new.version_number => {
                    Change::Updated { from: old.version_number.clone() }
                }
                Some(_) => Change::Unchanged,
            },
        };
        updates.push(ModUpdate { slug, change, locked });
    }

    // Anything in the old lock no longer requested by the pack.
    let requested: std::collections::HashSet<&str> =
        pack.mods.keys().map(|s| s.as_str()).collect();
    let mut removed: Vec<String> = old_by_slug
        .keys()
        .filter(|s| !requested.contains(*s))
        .map(|s| s.to_string())
        .collect();
    removed.sort();

    Ok(UpdatePlan { game_version, loader, updates, removed })
}
