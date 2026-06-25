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
                last_played: None,
            },
            dir,
        };
        inst.save()?;
        Ok(inst)
    }

    /// Synthesize a "main" instance pointing at an existing shared install,
    /// using its newest installed Fabric version. Used to bootstrap the UI when
    /// no instances have been created yet.
    pub fn detect_main() -> Option<Instance> {
        let mc_home = default_mc_home();
        let version_id = newest_fabric_version(&mc_home)?;
        Some(Instance {
            config: InstanceConfig {
                name: "main".to_string(),
                version_id,
                mc_home: mc_home.clone(),
                game_dir: mc_home.clone(),
                max_mb: 4096,
                last_played: None,
            },
            dir: mc_home,
        })
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
