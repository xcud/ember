//! Instances — isolated, named Minecraft setups.
//!
//! An instance is simply a `pack` (manifest + lock) plus a game directory and a
//! little launch config. This is the unit of "profiles / installations" parity
//! with other launchers, and it reuses the existing import/sync/update/launch
//! machinery wholesale: managing instances is managing a list of packs.
//!
//! Layout:
//! ```text
//! <data>/ember/instances/<name>/
//!   instance.toml   # this config
//!   pack.toml       # the manifest (optional until imported)
//!   pack.lock       # resolved + hashed
//!   minecraft/      # isolated game dir: mods/, config/, saves/
//! ```
//! Heavy shared assets (versions/, libraries/, assets/) live under `mc_home`
//! (a shared install, e.g. `~/.minecraft`) so instances stay cheap.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::launch::{self, AuthSession, Host, LaunchOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceConfig {
    pub name: String,
    /// Version id to launch, e.g. `fabric-loader-0.19.3-1.21.11`.
    pub version_id: String,
    /// Shared install holding versions/, libraries/, assets/, runtime/.
    pub mc_home: PathBuf,
    /// This instance's game directory (mods/, saves/, config/).
    pub game_dir: PathBuf,
    #[serde(default = "default_max_mb")]
    pub max_mb: u32,
    /// A linked instance points `game_dir` at an existing external install
    /// (e.g. `main` -> `~/.minecraft`). The instance folder is just a pointer;
    /// its `game_dir` lives outside and is never deleted with the instance.
    #[serde(default)]
    pub linked: bool,
    #[serde(default)]
    pub last_played: Option<u64>,
}

fn default_max_mb() -> u32 {
    4096
}

#[derive(Debug, Clone)]
pub struct Instance {
    /// The instance's own directory (holds instance.toml, pack files, minecraft/).
    pub dir: PathBuf,
    pub config: InstanceConfig,
}

fn default_mc_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".minecraft")
}

/// Root under which instance directories live.
pub fn instances_root() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share")
        });
    base.join("ember").join("instances")
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

impl Instance {
    pub fn config_path(dir: &Path) -> PathBuf {
        dir.join("instance.toml")
    }
    pub fn game_dir(&self) -> &Path {
        &self.config.game_dir
    }
    pub fn pack_path(&self) -> PathBuf {
        self.dir.join("pack.toml")
    }
    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("pack.lock")
    }
    pub fn mods_dir(&self) -> PathBuf {
        self.config.game_dir.join("mods")
    }

    pub fn load(dir: &Path) -> anyhow::Result<Instance> {
        let text = std::fs::read_to_string(Self::config_path(dir))?;
        let config: InstanceConfig = toml::from_str(&text)?;
        Ok(Instance { dir: dir.to_path_buf(), config })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(Self::config_path(&self.dir), toml::to_string_pretty(&self.config)?)?;
        Ok(())
    }

    /// Discover all instances under [`instances_root`].
    pub fn list() -> Vec<Instance> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(instances_root()) {
            for e in entries.flatten() {
                let dir = e.path();
                if dir.is_dir() {
                    if let Ok(inst) = Instance::load(&dir) {
                        out.push(inst);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.config.name.to_lowercase().cmp(&b.config.name.to_lowercase()));
        out
    }

    /// Find an instance by name (including the linked `main`).
    pub fn find(name: &str) -> Option<Instance> {
        Instance::all().into_iter().find(|i| i.config.name == name)
    }

    /// Everything to show in a launcher: ensures the linked `main` exists, then
    /// lists all instances with linked ones first, then alphabetical.
    pub fn all() -> Vec<Instance> {
        let _ = Instance::ensure_main();
        let mut out = Instance::list();
        out.sort_by(|a, b| {
            b.config
                .linked
                .cmp(&a.config.linked)
                .then(a.config.name.to_lowercase().cmp(&b.config.name.to_lowercase()))
        });
        out
    }

    /// Create a new, isolated instance under [`instances_root`].
    pub fn create(name: &str, version_id: &str, mc_home: PathBuf, max_mb: u32) -> anyhow::Result<Instance> {
        let dir = instances_root().join(name);
        if dir.exists() {
            anyhow::bail!("instance '{name}' already exists");
        }
        let game_dir = dir.join("minecraft");
        std::fs::create_dir_all(game_dir.join("mods"))?;
        let inst = Instance {
            config: InstanceConfig {
                name: name.to_string(),
                version_id: version_id.to_string(),
                mc_home,
                game_dir,
                max_mb,
                linked: false,
                last_played: None,
            },
            dir,
        };
        inst.save()?;
        Ok(inst)
    }

    /// Ensure a linked `main` instance exists, pointing at the shared
    /// `~/.minecraft` install. Materialized as a real instance folder (a pointer
    /// `instance.toml`; the game dir lives outside it). Its launch version is
    /// kept in sync with the official launcher's most-recently-used version.
    /// Returns `None` if there's no usable install to link.
    pub fn ensure_main() -> Option<Instance> {
        let mc_home = default_mc_home();
        let version_id = main_version_id(&mc_home)?;
        let dir = instances_root().join("main");

        if let Ok(mut inst) = Instance::load(&dir) {
            // Keep the linked version fresh with the official launcher.
            if inst.config.version_id != version_id {
                inst.config.version_id = version_id;
                let _ = inst.save();
            }
            return Some(inst);
        }

        std::fs::create_dir_all(&dir).ok()?;
        let inst = Instance {
            config: InstanceConfig {
                name: "main".to_string(),
                version_id,
                mc_home: mc_home.clone(),
                game_dir: mc_home,
                max_mb: 4096,
                linked: true,
                last_played: None,
            },
            dir,
        };
        inst.save().ok()?;
        Some(inst)
    }

    /// Back-compat alias; materializes the linked `main` instance.
    pub fn detect_main() -> Option<Instance> {
        Self::ensure_main()
    }

    /// Build the launch command (java path + argv) for this instance.
    pub fn launch_argv(&self, host: &Host, auth: &AuthSession) -> anyhow::Result<(PathBuf, Vec<String>)> {
        let versions_dir = self.config.mc_home.join("versions");
        let resolved = launch::resolve(&versions_dir, &self.config.version_id, host)?;
        let java = launch::find_bundled_java(&self.config.mc_home, &resolved.java_component, host);
        let natives_dir = self.dir.join("natives");
        std::fs::create_dir_all(&natives_dir)?;
        let opts = LaunchOptions {
            game_dir: self.config.game_dir.clone(),
            java_path: java.clone(),
            min_mb: 512,
            max_mb: self.config.max_mb,
            natives_dir,
        };
        let argv = resolved.build_command(&self.config.mc_home, host, auth, &opts);
        Ok((java, argv))
    }

    pub fn mark_played(&mut self) {
        self.config.last_played = Some(now_secs());
        let _ = self.save();
    }

    /// Is this a managed instance (lives under [`instances_root`])? The
    /// synthesized `main` instance points at the shared install and is not.
    pub fn is_managed(&self) -> bool {
        self.dir.starts_with(instances_root())
    }

    /// Clone this instance's *setup* (mods, configs, packs) into a new managed
    /// instance. Worlds (`saves/`) are intentionally not copied — they can be
    /// huge and are rarely what you want duplicated.
    pub fn clone_to(&self, new_name: &str) -> anyhow::Result<Instance> {
        let dir = instances_root().join(new_name);
        if dir.exists() {
            anyhow::bail!("instance '{new_name}' already exists");
        }
        let game_dir = dir.join("minecraft");
        std::fs::create_dir_all(&game_dir)?;
        for sub in ["mods", "config", "resourcepacks", "shaderpacks"] {
            let src = self.config.game_dir.join(sub);
            if src.is_dir() {
                copy_dir_all(&src, &game_dir.join(sub))?;
            }
        }
        for file in ["options.txt", "pack.toml", "pack.lock"] {
            let src = if file.starts_with("pack") {
                self.dir.join(file)
            } else {
                self.config.game_dir.join(file)
            };
            if src.is_file() {
                let dest = if file.starts_with("pack") {
                    dir.join(file)
                } else {
                    game_dir.join(file)
                };
                std::fs::copy(&src, dest)?;
            }
        }
        let inst = Instance {
            config: InstanceConfig {
                name: new_name.to_string(),
                version_id: self.config.version_id.clone(),
                mc_home: self.config.mc_home.clone(),
                game_dir,
                max_mb: self.config.max_mb,
                linked: false,
                last_played: None,
            },
            dir,
        };
        inst.save()?;
        Ok(inst)
    }

    /// Delete this instance. Refuses to touch anything outside [`instances_root`]
    /// (so it can never delete a shared install like `~/.minecraft`).
    pub fn delete(self) -> anyhow::Result<()> {
        if self.config.linked {
            anyhow::bail!(
                "'{}' is a linked instance pointing at {} — delete refused (your install is untouched). Unlink instead if you want it gone.",
                self.config.name,
                self.config.game_dir.display()
            );
        }
        if !self.is_managed() {
            anyhow::bail!(
                "refusing to delete '{}': not a managed instance ({})",
                self.config.name,
                self.dir.display()
            );
        }
        std::fs::remove_dir_all(&self.dir)?;
        Ok(())
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_all(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{AuthSession, Host};

    /// Environment-dependent: requires a real ~/.minecraft with a Fabric
    /// install. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn detect_and_build_main() {
        let inst = Instance::detect_main().expect("detect a main instance");
        println!("detected: {} -> {}", inst.config.name, inst.config.version_id);
        let (java, argv) = inst
            .launch_argv(&Host::current(), &AuthSession::offline("Player"))
            .expect("build launch argv");
        println!("java: {}", java.display());
        println!("argv entries: {}", argv.len());
        assert!(argv.iter().any(|a| a.contains("KnotClient")), "fabric main class present");
        assert!(argv.contains(&"--gameDir".to_string()));
    }
}

fn version_installed(mc_home: &Path, id: &str) -> bool {
    mc_home.join("versions").join(id).join(format!("{id}.json")).exists()
}

/// Which version the "main" instance should launch. Prefers the official
/// launcher's most-recently-used profile (authoritative — survives ember
/// installing other versions), falling back to the newest installed Fabric.
fn main_version_id(mc_home: &Path) -> Option<String> {
    if let Some(id) = last_used_version(mc_home) {
        return Some(id);
    }
    newest_fabric_version(mc_home)
}

/// Most-recently-used profile in `launcher_profiles.json` whose version is
/// actually installed.
fn last_used_version(mc_home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(mc_home.join("launcher_profiles.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let profiles = v.get("profiles")?.as_object()?;
    let mut candidates: Vec<(String, String)> = profiles
        .values()
        .filter_map(|p| {
            let ver = p.get("lastVersionId")?.as_str()?.to_string();
            let last_used = p.get("lastUsed").and_then(|l| l.as_str()).unwrap_or("").to_string();
            Some((last_used, ver))
        })
        .collect();
    // Most recent first; lexical sort of ISO timestamps is chronological.
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates
        .into_iter()
        .map(|(_, ver)| ver)
        .find(|ver| version_installed(mc_home, ver))
}

/// Newest (by mtime) `fabric-loader-*` version installed under `mc_home`.
fn newest_fabric_version(mc_home: &Path) -> Option<String> {
    let versions = mc_home.join("versions");
    let mut best: Option<(SystemTime, String)> = None;
    for e in std::fs::read_dir(&versions).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("fabric-loader-") {
            continue;
        }
        // Must have a matching json to be launchable.
        if !e.path().join(format!("{name}.json")).exists() {
            continue;
        }
        let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, name));
        }
    }
    best.map(|(_, n)| n)
}
