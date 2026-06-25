//! ember launcher-core
//!
//! UI-agnostic core for the ember Minecraft launcher. This first slice covers
//! the manifest/lockfile model and a hash-based importer that reverse-resolves
//! an existing `mods/` directory back into a clean, reproducible `pack.toml` +
//! `pack.lock` via Modrinth's content-hash lookup.

pub mod manifest;
pub mod modrinth;
pub mod import;
pub mod download;
pub mod sync;
pub mod update;
pub mod launch;
pub mod auth;
pub mod instance;

pub use manifest::{Loader, Pack, Lock, LockedMod, UnresolvedMod};
