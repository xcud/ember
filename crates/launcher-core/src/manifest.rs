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

/// A kind of installable content. Each lives in its own game-dir folder and is
/// a distinct Modrinth project type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Mod,
    ResourcePack,
    Shader,
}

impl ContentType {
    pub const ALL: [ContentType; 3] =
        [ContentType::Mod, ContentType::ResourcePack, ContentType::Shader];

    /// Modrinth `project_type` facet value.
    pub fn project_type(self) -> &'static str {
        match self {
            ContentType::Mod => "mod",
            ContentType::ResourcePack => "resourcepack",
            ContentType::Shader => "shader",
        }
    }

    /// Game-directory subfolder this content installs into.
    pub fn dir_name(self) -> &'static str {
        match self {
            ContentType::Mod => "mods",
            ContentType::ResourcePack => "resourcepacks",
            ContentType::Shader => "shaderpacks",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ContentType::Mod => "Mods",
            ContentType::ResourcePack => "Resource Packs",
            ContentType::Shader => "Shaders",
        }
    }

    /// Loader values to filter versions by. Resource packs use "minecraft";
    /// shaders use the shader loaders; mods use the instance's mod loader.
    pub fn version_loaders(self, mod_loader: Loader) -> Vec<String> {
        match self {
            ContentType::Mod => vec![mod_loader.modrinth_id().to_string()],
            ContentType::ResourcePack => vec!["minecraft".to_string()],
            ContentType::Shader => {
                vec!["iris".to_string(), "optifine".to_string(), "canvas".to_string()]
            }
        }
    }

    /// Extra `categories:` facets for search (only mods filter by loader).
    pub fn search_categories(self, mod_loader: Loader) -> Vec<String> {
        match self {
            ContentType::Mod => vec![mod_loader.modrinth_id().to_string()],
            _ => Vec::new(),
        }
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
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
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
    /// Human-friendly metadata, captured at resolve time (may be empty for
    /// older locks or mods not found on Modrinth).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
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
