//! ember CLI (first slice).
//!
//! Usage:
//!   ember import <mods_dir> [--name NAME] [--game VERSION] [--loader LOADER]
//!                           [--out DIR]
//!
//! Hashes every jar in <mods_dir>, identifies it via Modrinth, and writes a
//! `pack.toml` + `pack.lock` into --out (default: current directory).

use std::path::{Path, PathBuf};

use launcher_core::import::import_mods_dir;
use launcher_core::manifest::Loader;
use launcher_core::modrinth::Client;
use launcher_core::auth::{self, Account};
use launcher_core::install;
use launcher_core::instance::Instance;
use launcher_core::launch::{self, AuthSession, Host, LaunchOptions};
use launcher_core::modpack;
use launcher_core::manifest::Pack;
use launcher_core::sync::{self, ModStatus, SyncOptions};
use launcher_core::update::{self, Change};

const USAGE: &str = "ember (first slice)

Usage:
  ember import <mods_dir> [--name NAME] [--game VERSION] [--loader LOADER] [--out DIR]
  ember sync   [--lock pack.lock] [--mods DIR] [--cache DIR] [--concurrency N] [--prune]
  ember update [--pack pack.toml] [--lock pack.lock] [--game VERSION] [--apply]
  ember launch <version_id> [--mc DIR] [--name NAME] [--java PATH] [--max-mb N] [--run] [--offline]
       (default prints the command; --run starts the game; uses your account unless --offline)
  ember login    (sign in with your Microsoft account via device code)
  ember whoami   (show the signed-in account)
  ember instance list
  ember instance new <name> --version <id> [--mc DIR] [--max-mb N]
  ember instance clone <name> <new_name>
  ember instance delete <name>
  ember modpack import <file.mrpack> [--name NAME] [--mc DIR] [--max-mb N]
  ember install <mc_version> [--fabric <loader_version>] [--mc DIR]
       (download a vanilla version, and optionally a Fabric loader, into the shared install)";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    match cmd.as_str() {
        "import" => cmd_import(args).await,
        "sync" => cmd_sync(args).await,
        "update" => cmd_update(args).await,
        "launch" => cmd_launch(args).await,
        "login" => cmd_login().await,
        "whoami" => cmd_whoami().await,
        "instance" => cmd_instance(args).await,
        "modpack" => cmd_modpack(args).await,
        "install" => cmd_install(args).await,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn default_cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("ember");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache").join("ember");
    }
    PathBuf::from(".ember-cache")
}

async fn cmd_sync(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut lock_path = PathBuf::from("pack.lock");
    let mut mods_dir = PathBuf::from("mods");
    let mut cache_dir = default_cache_dir();
    let mut concurrency = 8usize;
    let mut prune = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--lock" => lock_path = PathBuf::from(args.next().unwrap_or_default()),
            "--mods" => mods_dir = PathBuf::from(args.next().unwrap_or_default()),
            "--cache" => cache_dir = PathBuf::from(args.next().unwrap_or_default()),
            "--concurrency" => {
                concurrency = args.next().and_then(|s| s.parse().ok()).unwrap_or(8)
            }
            "--prune" => prune = true,
            other => {
                eprintln!("unexpected argument: {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let lock = sync::load_lock(&lock_path)?;
    let client = Client::new()?;
    let opts = SyncOptions { concurrency, cache_dir: cache_dir.clone(), prune };

    eprintln!(
        "Syncing {} mods -> {}  (cache: {})",
        lock.mods.len(),
        mods_dir.display(),
        cache_dir.display()
    );
    let report = sync::sync(client.http(), &lock, &mods_dir, &opts).await?;

    for r in &report.results {
        let tag = match r.status {
            ModStatus::UpToDate => "ok  ",
            ModStatus::Cached => "cache",
            ModStatus::Downloaded => "get ",
        };
        println!("  [{tag}] {}  ({})", r.slug, r.filename);
    }
    for name in &report.pruned {
        println!("  [prune] {name}");
    }
    for (slug, err) in &report.failures {
        println!("  [FAIL] {slug}: {err}");
    }

    println!(
        "\n{} up-to-date, {} from cache, {} downloaded{}{}",
        report.count(ModStatus::UpToDate),
        report.count(ModStatus::Cached),
        report.count(ModStatus::Downloaded),
        if report.pruned.is_empty() {
            String::new()
        } else {
            format!(", {} pruned", report.pruned.len())
        },
        if report.failures.is_empty() {
            String::new()
        } else {
            format!(", {} FAILED", report.failures.len())
        },
    );
    if !report.failures.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

fn default_mc_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".minecraft")
}

fn find_java(mc_dir: &Path, component: &str, host: &Host) -> PathBuf {
    // Prefer Mojang's bundled runtime for this version's java component.
    let candidate = mc_dir
        .join("runtime")
        .join(component)
        .join(host.os_name)
        .join(component)
        .join("bin")
        .join("java");
    if candidate.exists() {
        return candidate;
    }
    PathBuf::from("java") // fall back to PATH
}

async fn cmd_install(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut mc_version: Option<String> = None;
    let mut fabric: Option<String> = None;
    let mut mc = default_mc_dir();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--fabric" => fabric = args.next(),
            "--mc" => mc = PathBuf::from(args.next().unwrap_or_default()),
            other if mc_version.is_none() => mc_version = Some(other.to_string()),
            _ => {}
        }
    }
    let mc_version = mc_version.unwrap_or_else(|| {
        eprintln!("usage: ember install <mc_version> [--fabric <loader_version>] [--mc DIR]");
        std::process::exit(2);
    });

    let host = Host::current();
    let cache_dir = default_cache_dir();
    // Throttled progress line per phase.
    let progress: std::sync::Arc<dyn Fn(&str, usize, usize) + Send + Sync> =
        std::sync::Arc::new(|phase: &str, n: usize, total: usize| {
            if n == total || n % 25 == 0 {
                eprint!("\r  {phase}: {n}/{total}    ");
                use std::io::Write;
                let _ = std::io::stderr().flush();
                if n == total {
                    eprintln!();
                }
            }
        });

    let report = match &fabric {
        Some(lv) => {
            eprintln!("Installing Fabric {lv} for Minecraft {mc_version} into {} ...", mc.display());
            install::install_fabric(&mc, &cache_dir, &mc_version, lv, &host, 16, progress).await?
        }
        None => {
            eprintln!("Installing Minecraft {mc_version} into {} ...", mc.display());
            install::install_vanilla(&mc, &cache_dir, &mc_version, &host, 16, progress).await?
        }
    };

    println!(
        "\nInstalled {}: {} downloaded, {} already present{}",
        report.version_id,
        report.downloaded,
        report.skipped,
        if report.failures.is_empty() {
            String::new()
        } else {
            format!(", {} FAILED", report.failures.len())
        }
    );
    for f in report.failures.iter().take(10) {
        println!("  FAIL: {f}");
    }
    if report.failures.is_empty() {
        println!("Ready to launch: ember launch {} --mc {}", report.version_id, mc.display());
    }
    Ok(())
}

async fn cmd_instance(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let sub = args.next().unwrap_or_default();
    match sub.as_str() {
        "list" => {
            let instances = Instance::all();
            if instances.is_empty() {
                println!("No instances. Create one with `ember instance new` or `ember modpack import`.");
            }
            for i in &instances {
                let managed = if i.is_managed() { "" } else { "  (shared)" };
                println!("{:<20} {}{}", i.config.name, i.config.version_id, managed);
            }
        }
        "new" => {
            let name = args.next().unwrap_or_default();
            let mut version: Option<String> = None;
            let mut mc = default_mc_dir();
            let mut max_mb = 4096u32;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--version" => version = args.next(),
                    "--mc" => mc = PathBuf::from(args.next().unwrap_or_default()),
                    "--max-mb" => max_mb = args.next().and_then(|s| s.parse().ok()).unwrap_or(max_mb),
                    _ => {}
                }
            }
            if name.is_empty() || version.is_none() {
                eprintln!("usage: ember instance new <name> --version <id> [--mc DIR] [--max-mb N]");
                std::process::exit(2);
            }
            let inst = Instance::create(&name, &version.unwrap(), mc, max_mb)?;
            println!("Created instance '{}' at {}", inst.config.name, inst.dir.display());
        }
        "clone" => {
            let name = args.next().unwrap_or_default();
            let new_name = args.next().unwrap_or_default();
            let src = Instance::find(&name)
                .or_else(|| Instance::detect_main().filter(|m| m.config.name == name))
                .ok_or_else(|| anyhow::anyhow!("no instance named '{name}'"))?;
            let inst = src.clone_to(&new_name)?;
            println!("Cloned '{name}' -> '{}' ({})", inst.config.name, inst.dir.display());
        }
        "delete" => {
            let name = args.next().unwrap_or_default();
            let inst = Instance::find(&name)
                .ok_or_else(|| anyhow::anyhow!("no managed instance named '{name}'"))?;
            inst.delete()?;
            println!("Deleted instance '{name}'");
        }
        other => {
            eprintln!("unknown instance subcommand: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    Ok(())
}

async fn cmd_modpack(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let sub = args.next().unwrap_or_default();
    if sub != "import" {
        eprintln!("usage: ember modpack import <file.mrpack> [--name NAME] [--mc DIR] [--max-mb N]");
        std::process::exit(2);
    }
    let mut mrpack: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut mc = default_mc_dir();
    let mut max_mb = 4096u32;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.next(),
            "--mc" => mc = PathBuf::from(args.next().unwrap_or_default()),
            "--max-mb" => max_mb = args.next().and_then(|s| s.parse().ok()).unwrap_or(max_mb),
            other if mrpack.is_none() => mrpack = Some(PathBuf::from(other)),
            _ => {}
        }
    }
    let mrpack = mrpack.unwrap_or_else(|| {
        eprintln!("usage: ember modpack import <file.mrpack> [--name NAME]");
        std::process::exit(2);
    });
    // Default the instance name to the mrpack filename stem.
    let name = name.unwrap_or_else(|| {
        mrpack
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "modpack".into())
    });

    let client = Client::new()?;
    let cache_dir = default_cache_dir();
    eprintln!("Importing {} ...", mrpack.display());
    let report =
        modpack::import_mrpack(client.http(), &cache_dir, &mrpack, &name, mc, max_mb).await?;

    println!(
        "\nImported '{}' as instance '{}'",
        report.pack_name, report.instance.config.name
    );
    println!(
        "  {} on {} ({})",
        report.game_version, report.loader, report.version_id
    );
    println!("  {} files installed, {} overrides, {} skipped", report.installed, report.overrides, report.skipped);
    if !report.version_installed {
        println!(
            "  ⚠ loader version '{}' is not installed in {} — install it before launching.",
            report.version_id,
            default_mc_dir().display()
        );
    }
    println!("\nLaunch with: ember launch {} --mc <shared install>", report.version_id);
    Ok(())
}

async fn cmd_login() -> anyhow::Result<()> {
    eprintln!("Signing in with Microsoft (Azure app {})...", auth::client_id());
    let account = auth::login_interactive(|dc| {
        let url = &dc.verification_uri;
        println!("\n  To sign in, open:  {url}");
        println!("  and enter code:    {}\n", dc.user_code);
        if let Some(msg) = &dc.message {
            eprintln!("  ({msg})");
        }
        eprintln!("  Waiting for approval...");
    })
    .await?;
    println!(
        "\n✓ Signed in as {} ({})",
        account.name, account.uuid
    );
    println!("  Saved to {}", auth::account_path().display());
    Ok(())
}

async fn cmd_whoami() -> anyhow::Result<()> {
    match Account::load() {
        Some(a) => {
            println!("{} ({})", a.name, a.uuid);
            println!("  account file: {}", auth::account_path().display());
        }
        None => {
            println!("Not signed in. Run `ember login`.");
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn cmd_launch(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut version_id: Option<String> = None;
    let mut mc_dir = default_mc_dir();
    let mut name = "Player".to_string();
    let mut java: Option<PathBuf> = None;
    let mut max_mb = 4096u32;
    let mut run = false;
    let mut offline = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--mc" => mc_dir = PathBuf::from(args.next().unwrap_or_default()),
            "--name" => name = args.next().unwrap_or(name),
            "--java" => java = args.next().map(PathBuf::from),
            "--max-mb" => max_mb = args.next().and_then(|s| s.parse().ok()).unwrap_or(max_mb),
            "--run" => run = true,
            "--offline" => offline = true,
            other if version_id.is_none() => version_id = Some(other.to_string()),
            other => {
                eprintln!("unexpected argument: {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    let version_id = version_id.unwrap_or_else(|| {
        eprintln!("error: <version_id> is required (e.g. fabric-loader-0.19.3-1.21.11)\n\n{USAGE}");
        std::process::exit(2);
    });

    let host = Host::current();
    let versions_dir = mc_dir.join("versions");
    let resolved = launch::resolve(&versions_dir, &version_id, &host)?;

    let java_path = java.unwrap_or_else(|| find_java(&mc_dir, &resolved.java_component, &host));
    let natives_dir = mc_dir.join("bin").join(format!("ember-natives-{}", resolved.root_id));

    eprintln!(
        "Resolved {} (root {}, main {})",
        resolved.id, resolved.root_id, resolved.main_class
    );
    eprintln!("  java:        {}", java_path.display());
    eprintln!("  assetIndex:  {}", resolved.asset_index_id);

    let cp = resolved.classpath(&mc_dir);
    let missing = resolved.missing_classpath(&mc_dir);
    eprintln!("  classpath:   {} entries, {} missing on disk", cp.len(), missing.len());
    for m in &missing {
        eprintln!("      MISSING: {}", m.display());
    }

    // Prefer the signed-in account; fall back to offline.
    let auth = if offline {
        AuthSession::offline(&name)
    } else if let Some(account) = Account::load() {
        match auth::ensure_session(account).await {
            Ok(session) => {
                eprintln!("  account:     {} (online)", session.player_name);
                session
            }
            Err(e) => {
                eprintln!("  account:     refresh failed ({e}) — falling back to offline");
                AuthSession::offline(&name)
            }
        }
    } else {
        eprintln!("  account:     none — offline mode (run `ember login` for multiplayer)");
        AuthSession::offline(&name)
    };
    let opts = LaunchOptions {
        game_dir: mc_dir.clone(),
        java_path: java_path.clone(),
        min_mb: 512,
        max_mb,
        natives_dir: natives_dir.clone(),
    };
    let argv = resolved.build_command(&mc_dir, &host, &auth, &opts);

    if !run {
        println!("\n# Launch command (offline / dry run):");
        println!("{} \\", java_path.display());
        for a in &argv {
            // Lightly quote args that contain spaces for readability.
            if a.contains(' ') {
                println!("  '{a}' \\");
            } else {
                println!("  {a} \\");
            }
        }
        println!("\n({} total JVM+game args)", argv.len());
        if !missing.is_empty() {
            println!("\n⚠ {} classpath entries missing — run sync/install first.", missing.len());
            std::process::exit(1);
        }
        println!("\nDry run. Re-run with --run to start the game (offline mode).");
        return Ok(());
    }

    if !missing.is_empty() {
        eprintln!("Refusing to launch: {} classpath entries missing.", missing.len());
        std::process::exit(1);
    }
    std::fs::create_dir_all(&natives_dir)?;
    eprintln!("\nLaunching (offline)...");
    let status = std::process::Command::new(&java_path)
        .args(&argv)
        .current_dir(&mc_dir)
        .status()?;
    eprintln!("Game exited with {status}");
    Ok(())
}

async fn cmd_update(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut pack_path = PathBuf::from("pack.toml");
    let mut lock_path = PathBuf::from("pack.lock");
    let mut target_game: Option<String> = None;
    let mut apply = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--pack" => pack_path = PathBuf::from(args.next().unwrap_or_default()),
            "--lock" => lock_path = PathBuf::from(args.next().unwrap_or_default()),
            "--game" => target_game = args.next(),
            "--apply" => apply = true,
            other => {
                eprintln!("unexpected argument: {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let pack = Pack::load(&pack_path)?;
    let old_lock = sync::load_lock(&lock_path).ok();
    let client = Client::new()?;

    match &target_game {
        Some(g) => eprintln!("Bumping {} -> Minecraft {} ({})", pack.name, g, pack.loader),
        None => eprintln!(
            "Updating {} (Minecraft {} on {})",
            pack.name, pack.game_version, pack.loader
        ),
    }

    let plan = update::plan(&client, &pack, old_lock.as_ref(), target_game.clone(), 8).await?;

    let mut incompatible = 0usize;
    for u in &plan.updates {
        match &u.change {
            Change::Added => {
                let v = u.locked.as_ref().unwrap();
                println!("  [+ add ] {}  -> {}", u.slug, v.version_number);
            }
            Change::Updated { from } => {
                let v = u.locked.as_ref().unwrap();
                println!("  [update] {}  {}  ->  {}", u.slug, from, v.version_number);
            }
            Change::Unchanged => {
                let v = u.locked.as_ref().unwrap();
                println!("  [  ok  ] {}  {}", u.slug, v.version_number);
            }
            Change::Incompatible => {
                incompatible += 1;
                println!("  [ !!   ] {}  — no build for {}", u.slug, plan.game_version);
            }
        }
    }
    for slug in &plan.removed {
        println!("  [remove] {slug}  — no longer in pack.toml");
    }

    let updated = plan
        .updates
        .iter()
        .filter(|u| matches!(u.change, Change::Updated { .. }))
        .count();
    let added = plan
        .updates
        .iter()
        .filter(|u| matches!(u.change, Change::Added))
        .count();
    println!(
        "\n{added} added, {updated} updated, {incompatible} incompatible, {} removed",
        plan.removed.len()
    );

    if incompatible > 0 {
        println!(
            "  ⚠ {incompatible} mod(s) have no {} build yet — excluded from the new lock.",
            plan.game_version
        );
    }

    if !apply {
        if plan.changed() {
            println!("\nDry run. Re-run with --apply to write {}.", lock_path.display());
        } else {
            println!("\nAlready up to date. Nothing to write.");
        }
        return Ok(());
    }

    // Apply: write the new lock, and on a bump, update pack.toml's game version.
    plan.new_lock().write(&lock_path)?;
    println!("\nWrote {}", lock_path.display());
    if let Some(g) = &target_game {
        let mut pack = pack;
        pack.game_version = g.clone();
        pack.write(&pack_path)?;
        println!("Updated {} to game_version = \"{g}\"", pack_path.display());
    }
    println!("Run `ember sync` to realize the new lock.");
    Ok(())
}

async fn cmd_import(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut mods_dir: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut game: Option<String> = None;
    let mut loader: Option<Loader> = None;
    let mut out = PathBuf::from(".");

    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => name = args.next(),
            "--game" => game = args.next(),
            "--out" => out = PathBuf::from(args.next().unwrap_or_else(|| ".".into())),
            "--loader" => {
                loader = match args.next().as_deref() {
                    Some("fabric") => Some(Loader::Fabric),
                    Some("forge") => Some(Loader::Forge),
                    Some("neoforge") => Some(Loader::NeoForge),
                    Some("quilt") => Some(Loader::Quilt),
                    other => {
                        eprintln!("unknown loader: {other:?}");
                        std::process::exit(2);
                    }
                }
            }
            other if mods_dir.is_none() => mods_dir = Some(PathBuf::from(other)),
            other => {
                eprintln!("unexpected argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let mods_dir = mods_dir.unwrap_or_else(|| {
        eprintln!("error: <mods_dir> is required");
        std::process::exit(2);
    });
    let name = name.unwrap_or_else(|| "imported-pack".to_string());

    eprintln!("Scanning {} ...", mods_dir.display());
    let client = Client::new()?;
    let result = import_mods_dir(&client, &mods_dir, &name, game, loader).await?;

    std::fs::create_dir_all(&out)?;
    let pack_path = out.join("pack.toml");
    let lock_path = out.join("pack.lock");
    result.pack.write(&pack_path)?;
    result.lock.write(&lock_path)?;

    println!(
        "\nPack: {}  ({} on {})",
        result.pack.name, result.pack.game_version, result.pack.loader
    );
    println!("  resolved:   {} mods", result.resolved);
    if result.unresolved > 0 {
        println!("  unresolved: {} jar(s) (not on Modrinth):", result.unresolved);
        for u in &result.lock.unresolved {
            println!("      - {}", u.filename);
        }
    }
    println!("\nWrote {}", pack_path.display());
    println!("Wrote {}", lock_path.display());
    Ok(())
}
