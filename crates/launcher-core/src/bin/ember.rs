//! ember CLI (first slice).
//!
//! Usage:
//!   ember import <mods_dir> [--name NAME] [--game VERSION] [--loader LOADER]
//!                           [--out DIR]
//!
//! Hashes every jar in <mods_dir>, identifies it via Modrinth, and writes a
//! `pack.toml` + `pack.lock` into --out (default: current directory).

use std::path::PathBuf;

use launcher_core::import::import_mods_dir;
use launcher_core::manifest::Loader;
use launcher_core::modrinth::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    if cmd != "import" {
        eprintln!(
            "ember (first slice)\n\nUsage:\n  ember import <mods_dir> [--name NAME] [--game VERSION] [--loader LOADER] [--out DIR]"
        );
        std::process::exit(2);
    }

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
