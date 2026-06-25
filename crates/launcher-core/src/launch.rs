//! Build the exact launch command for an installed (modded) Minecraft version.
//!
//! This reads the on-disk version JSON(s) — resolving Fabric's `inheritsFrom`
//! chain onto the vanilla parent — evaluates the OS rule guards, assembles the
//! classpath, and performs the JVM/game argument placeholder substitution that
//! turns a version manifest into an `argv`. It is intentionally auth-agnostic:
//! you hand it an `AuthSession` (real or offline) and it produces the command.
//!
//! Targets the modern (1.13+) `arguments.{jvm,game}` format. Natives in modern
//! versions ship as `:natives-<os>` classifier libraries on the classpath, so
//! no legacy native extraction is needed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// The host we're launching on. Used for rule evaluation and native selection.
pub struct Host {
    pub os_name: &'static str, // "linux" | "osx" | "windows"
    pub arch: &'static str,    // "x86_64" | "x86" | "arm64"
}

impl Host {
    pub fn current() -> Self {
        let os_name = if cfg!(target_os = "macos") {
            "osx"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "linux"
        };
        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86"
        };
        Host { os_name, arch }
    }
}

/// Identity passed into the game. For offline/dry runs, use [`AuthSession::offline`].
#[derive(Debug, Clone)]
pub struct AuthSession {
    pub player_name: String,
    pub uuid: String,
    pub access_token: String,
    pub xuid: String,
    pub client_id: String,
    pub user_type: String, // "msa" for real accounts
}

impl AuthSession {
    /// A placeholder session for dry-runs / offline single-player.
    pub fn offline(name: &str) -> Self {
        AuthSession {
            player_name: name.to_string(),
            uuid: "00000000-0000-0000-0000-000000000000".to_string(),
            access_token: "0".to_string(),
            xuid: "0".to_string(),
            client_id: "0".to_string(),
            user_type: "legacy".to_string(),
        }
    }
}

pub struct LaunchOptions {
    pub game_dir: PathBuf,
    pub java_path: PathBuf,
    pub min_mb: u32,
    pub max_mb: u32,
    /// Where LWJGL extracts native libs; created if missing.
    pub natives_dir: PathBuf,
}

/// Locate Mojang's bundled Java runtime for `component` under `mc_home`,
/// falling back to `java` on PATH.
pub fn find_bundled_java(mc_home: &Path, component: &str, host: &Host) -> PathBuf {
    let candidate = mc_home
        .join("runtime")
        .join(component)
        .join(host.os_name)
        .join(component)
        .join("bin")
        .join("java");
    if candidate.exists() {
        candidate
    } else {
        PathBuf::from("java")
    }
}

/// The merged, rule-filtered result, ready to turn into an argv.
pub struct Resolved {
    pub id: String,
    pub root_id: String,
    pub main_class: String,
    pub asset_index_id: String,
    pub java_component: String,
    libraries: Vec<Value>,
    jvm_args: Vec<Value>,
    game_args: Vec<Value>,
}

fn read_version_json(versions_dir: &Path, id: &str) -> anyhow::Result<Value> {
    let path = versions_dir.join(id).join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

/// Does this rule set allow inclusion on `host`? Rules with a `features` guard
/// (demo mode, custom resolution, …) are treated as not-matching, so their
/// optional args are dropped.
fn rules_allow(rules: &Value, host: &Host) -> bool {
    let Some(rules) = rules.as_array() else {
        return true;
    };
    if rules.is_empty() {
        return true;
    }
    let mut allowed = false;
    for rule in rules {
        let action_allow = rule.get("action").and_then(|a| a.as_str()) == Some("allow");
        let mut matches = true;
        if rule.get("features").is_some() {
            matches = false; // we don't opt into any feature flags
        }
        if let Some(os) = rule.get("os") {
            if let Some(name) = os.get("name").and_then(|n| n.as_str()) {
                if name != host.os_name {
                    matches = false;
                }
            }
            if let Some(arch) = os.get("arch").and_then(|a| a.as_str()) {
                // Mojang uses "x86" to gate 32-bit only.
                if arch != host.arch {
                    matches = false;
                }
            }
        }
        if matches {
            allowed = action_allow;
        }
    }
    allowed
}

/// `group:artifact:version[:classifier]` -> relative jar path under `libraries/`.
fn maven_to_path(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let group = parts[0].replace('.', "/");
    let artifact = parts[1];
    let version = parts[2];
    let classifier = parts.get(3);
    let file = match classifier {
        Some(c) => format!("{artifact}-{version}-{c}.jar"),
        None => format!("{artifact}-{version}.jar"),
    };
    Some(format!("{group}/{artifact}/{version}/{file}"))
}

/// Dedup key: `group:artifact[:classifier]`, ignoring version, so the child
/// (Fabric) override of a shared library wins and we don't double-list.
fn lib_key(name: &str) -> String {
    let parts: Vec<&str> = name.split(':').collect();
    match parts.as_slice() {
        [g, a, _v, c, ..] => format!("{g}:{a}:{c}"),
        [g, a, ..] => format!("{g}:{a}"),
        _ => name.to_string(),
    }
}

fn lib_relative_path(lib: &Value) -> Option<String> {
    // Prefer an explicit artifact path; fall back to deriving from the name.
    if let Some(p) = lib
        .pointer("/downloads/artifact/path")
        .and_then(|p| p.as_str())
    {
        return Some(p.to_string());
    }
    lib.get("name").and_then(|n| n.as_str()).and_then(maven_to_path)
}

/// Resolve `version_id`, following `inheritsFrom` to the vanilla root and
/// merging child over parent. Rule-filters libraries for `host`.
pub fn resolve(versions_dir: &Path, version_id: &str, host: &Host) -> anyhow::Result<Resolved> {
    // Walk the inheritance chain, root (vanilla) last.
    let mut chain: Vec<Value> = Vec::new();
    let mut current = version_id.to_string();
    loop {
        let json = read_version_json(versions_dir, &current)?;
        let parent = json
            .get("inheritsFrom")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string());
        chain.push(json);
        match parent {
            Some(p) => current = p,
            None => break,
        }
    }
    // chain[0] = child (fabric), chain.last() = root (vanilla).
    let root = chain.last().unwrap();
    let child = &chain[0];

    let root_id = root
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or(&current)
        .to_string();
    let asset_index_id = root
        .pointer("/assetIndex/id")
        .and_then(|i| i.as_str())
        .unwrap_or("legacy")
        .to_string();
    let java_component = root
        .pointer("/javaVersion/component")
        .and_then(|c| c.as_str())
        .unwrap_or("jre-legacy")
        .to_string();
    // Child mainClass wins (Fabric's KnotClient).
    let main_class = child
        .get("mainClass")
        .and_then(|m| m.as_str())
        .or_else(|| root.get("mainClass").and_then(|m| m.as_str()))
        .unwrap_or("net.minecraft.client.main.Main")
        .to_string();

    // Merge libraries root-first, child overrides shared keys; preserve order.
    let mut order: Vec<String> = Vec::new();
    let mut by_key: BTreeMap<String, Value> = BTreeMap::new();
    for json in chain.iter().rev() {
        // root .. child
        if let Some(libs) = json.get("libraries").and_then(|l| l.as_array()) {
            for lib in libs {
                let name = lib.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if let Some(rules) = lib.get("rules") {
                    if !rules_allow(rules, host) {
                        continue;
                    }
                }
                let key = lib_key(name);
                if !by_key.contains_key(&key) {
                    order.push(key.clone());
                }
                by_key.insert(key, lib.clone());
            }
        }
    }
    let libraries: Vec<Value> = order.into_iter().filter_map(|k| by_key.remove(&k)).collect();

    // Merge arguments: root first, then child appends.
    let mut jvm_args = Vec::new();
    let mut game_args = Vec::new();
    for json in chain.iter().rev() {
        if let Some(jvm) = json.pointer("/arguments/jvm").and_then(|a| a.as_array()) {
            jvm_args.extend(jvm.iter().cloned());
        }
        if let Some(game) = json.pointer("/arguments/game").and_then(|a| a.as_array()) {
            game_args.extend(game.iter().cloned());
        }
    }

    Ok(Resolved {
        id: version_id.to_string(),
        root_id,
        main_class,
        asset_index_id,
        java_component,
        libraries,
        jvm_args,
        game_args,
    })
}

impl Resolved {
    /// Absolute classpath entries: every library + the root client jar.
    pub fn classpath(&self, mc_dir: &Path) -> Vec<PathBuf> {
        let lib_dir = mc_dir.join("libraries");
        let mut cp: Vec<PathBuf> = self
            .libraries
            .iter()
            .filter_map(lib_relative_path)
            .map(|p| lib_dir.join(p))
            .collect();
        cp.push(
            mc_dir
                .join("versions")
                .join(&self.root_id)
                .join(format!("{}.jar", self.root_id)),
        );
        cp
    }

    /// Classpath entries that don't exist on disk (would need downloading).
    pub fn missing_classpath(&self, mc_dir: &Path) -> Vec<PathBuf> {
        self.classpath(mc_dir)
            .into_iter()
            .filter(|p| !p.exists())
            .collect()
    }

    fn substitute(&self, raw: &str, vars: &BTreeMap<&str, String>) -> String {
        let mut out = raw.to_string();
        for (k, v) in vars {
            out = out.replace(&format!("${{{k}}}"), v);
        }
        out
    }

    /// Flatten a rule-gated argument array into concrete strings for `host`.
    fn flatten_args(&self, args: &[Value], host: &Host, vars: &BTreeMap<&str, String>) -> Vec<String> {
        let mut out = Vec::new();
        for arg in args {
            match arg {
                Value::String(s) => out.push(self.substitute(s, vars)),
                Value::Object(obj) => {
                    let rules = obj.get("rules").cloned().unwrap_or(Value::Null);
                    if !rules_allow(&rules, host) {
                        continue;
                    }
                    match obj.get("value") {
                        Some(Value::String(s)) => out.push(self.substitute(s, vars)),
                        Some(Value::Array(vs)) => {
                            for v in vs {
                                if let Some(s) = v.as_str() {
                                    out.push(self.substitute(s, vars));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Build the full launch command: (java_path, argv-after-java).
    pub fn build_command(
        &self,
        mc_dir: &Path,
        host: &Host,
        auth: &AuthSession,
        opts: &LaunchOptions,
    ) -> Vec<String> {
        let sep = if host.os_name == "windows" { ";" } else { ":" };
        let classpath = self
            .classpath(mc_dir)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(sep);

        let assets_root = mc_dir.join("assets").to_string_lossy().into_owned();
        let natives = opts.natives_dir.to_string_lossy().into_owned();

        let mut vars: BTreeMap<&str, String> = BTreeMap::new();
        vars.insert("classpath", classpath);
        vars.insert("classpath_separator", sep.to_string());
        vars.insert("natives_directory", natives);
        vars.insert("library_directory", mc_dir.join("libraries").to_string_lossy().into_owned());
        vars.insert("launcher_name", "ember".to_string());
        vars.insert("launcher_version", "0.1.0".to_string());
        vars.insert("version_name", self.id.clone());
        vars.insert("version_type", "release".to_string());
        vars.insert("game_directory", opts.game_dir.to_string_lossy().into_owned());
        vars.insert("assets_root", assets_root.clone());
        vars.insert("game_assets", assets_root);
        vars.insert("assets_index_name", self.asset_index_id.clone());
        vars.insert("auth_player_name", auth.player_name.clone());
        vars.insert("auth_uuid", auth.uuid.clone());
        vars.insert("auth_access_token", auth.access_token.clone());
        vars.insert("auth_xuid", auth.xuid.clone());
        vars.insert("clientid", auth.client_id.clone());
        vars.insert("auth_session", format!("token:{}", auth.access_token));
        vars.insert("user_type", auth.user_type.clone());
        vars.insert("user_properties", "{}".to_string());

        let mut argv: Vec<String> = Vec::new();
        argv.push(format!("-Xms{}m", opts.min_mb));
        argv.push(format!("-Xmx{}m", opts.max_mb));
        argv.extend(self.flatten_args(&self.jvm_args, host, &vars));
        argv.push(self.main_class.clone());
        argv.extend(self.flatten_args(&self.game_args, host, &vars));
        argv
    }
}
