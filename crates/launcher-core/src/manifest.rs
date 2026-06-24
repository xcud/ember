//! The declarative manifest (`pack.toml`) and resolved lockfile (`pack.lock`).
//!
//! `pack.toml` is what the human edits: "I want these mods, for this game
//! version, on this loader." `pack.lock` is what the resolver produces: exact
//! builds, hashes, and URLs so a sync is reproducible and verifiable.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Loader {
    Fabric,
    Forge,
    NeoForge,
    Quilt,
    Vanilla,
}

impl Loader {
    /// The identifier Modrinth uses for this loader.
    pub fn modrinth_id(self) -> &'static str {
        match self {
            Loader::Fabric => "fabric",
            Loader::Forge => "forge",
            Loader::NeoForge => "neoforge",
            Loader::Quilt => "quilt",
            Loader::Vanilla => "vanilla",
        }
    }
}

impl fmt::Display for Loader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.modrinth_id())
    }
}

/// `pack.toml` — the human-authored manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pack {
    pub name: String,
    pub game_version: String,
    pub loader: Loader,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    /// slug -> version requirement. `"*"` means "latest compatible build".
    #[serde(default)]
    pub mods: BTreeMap<String, String>,
}

impl Pack {
    pub fn to_toml(&self) -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }
    pub fn write(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        std::fs::write(path, self.to_toml()?)?;
        Ok(())
    }
}

/// One resolved mod in `pack.lock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedMod {
    pub slug: String,
    pub project_id: String,
    pub version_id: String,
    pub version_number: String,
    pub filename: String,
    pub sha1: String,
    pub url: String,
    pub size: u64,
}

/// A jar we found on disk but could not identify on Modrinth (e.g. OptiFine,
/// CurseForge-only mods, or a private build). We surface these explicitly
/// rather than silently dropping them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedMod {
    pub filename: String,
    pub sha1: String,
    pub size: u64,
}

/// `pack.lock` — the resolver's reproducible output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lock {
    pub game_version: String,
    pub loader: Loader,
    #[serde(rename = "mod", default)]
    pub mods: Vec<LockedMod>,
    #[serde(rename = "unresolved", default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<UnresolvedMod>,
}

impl Lock {
    pub fn to_toml(&self) -> anyhow::Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }
    pub fn write(&self, path: impl AsRef<Path>) -> anyhow::Result<()> {
        std::fs::write(path, self.to_toml()?)?;
        Ok(())
    }
}
