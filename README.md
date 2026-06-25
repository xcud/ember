# ember

A fast, robust, terminal-first launcher for Minecraft: Java Edition, written in Rust.

> **ember is NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED
> WITH MOJANG OR MICROSOFT.**

`ember` is an open-source, unofficial launcher built around a clean, UI-agnostic
core (`launcher-core`) and a scriptable CLI. It manages mods reproducibly,
resolves compatibility-aware updates, and launches the player's own copy of
Minecraft: Java Edition — designed to be left open as a low-overhead companion
while you play, rather than a heavyweight window competing for resources.

## Why

Existing launchers are capable but can feel heavy and slow — much of that is the
embedded webview, not the work itself. `ember` does the work in async Rust and
presents it in the terminal: as fast as it can be, and cheap enough to leave
running.

The design goals, in order:

1. **Speed** — parallel, content-addressed downloads; verify-only fast paths.
2. **Robustness** — every artifact is SHA-1 verified; a corrupt download can
   never masquerade as valid because the cache is keyed by content hash.
3. **Reproducibility** — a declarative `pack.toml` plus a resolved `pack.lock`,
   modeled after Cargo, so a setup can be reproduced exactly anywhere.

## Architecture

```
launcher-core/         UI-agnostic library (the engine)
  manifest.rs          pack.toml (intent) + pack.lock (resolved, hashed)
  modrinth.rs          Modrinth API client (content-hash + version queries)
  import.rs            reverse-resolve an existing mods/ dir into a manifest
  download.rs          streaming, SHA-1-verified, content-addressed cache
  sync.rs              reproduce a lock into mods/ (parallel, prune, verify)
  update.rs            compatibility-aware re-resolution + diff (update/bump)
  launch.rs            version resolution + launch-command construction
  auth.rs              Microsoft / Xbox Live / Minecraft authentication
  bin/ember.rs         the CLI
```

Splitting the engine from the interface is deliberate: the same core can back
the CLI, a future TUI, or an AI agent driving the launcher conversationally.

## Commands

```
ember import <mods_dir>     # reverse-resolve an existing mods/ into pack.toml + pack.lock
ember sync                  # download + verify mods to match pack.lock
ember update                # re-resolve to latest compatible builds, show a diff
ember update --game 26.2    # bump to a new Minecraft version, flag mods with no build yet
ember launch <version_id>   # build and run the launch command (offline or signed-in)
ember login                 # sign in with a Microsoft account (device-code flow)
ember whoami                # show the signed-in account
```

### Example

```console
$ ember import ~/.minecraft/mods --name my-pack
Pack: my-pack  (1.21.11 on fabric)
  resolved:   13 mods

$ ember update --game 26.2
  [update] sodium    mc1.21.11-0.8.7-fabric  ->  mc26.2-0.9.1-beta.2-fabric
  [ !!   ] voxy      — no build for 26.2
  ...
  11 updated, 1 incompatible
```

## Authentication

`ember` uses the standard Microsoft OAuth 2.0 device-code flow to obtain Xbox
Live / Minecraft tokens for online play. It requests only the `XboxLive.signin`
scope, persists the refresh token locally (with `0600` permissions) so sign-in
is a one-time action, and stores nothing on any server. Single-player works in
offline mode without signing in.

## Status

Early but functional. The full lifecycle — import, sync, update/bump, launch,
and login — is implemented and working. A native TUI is planned.

## License

MIT — see [LICENSE](LICENSE).

## Trademark notice

ember is NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED WITH
MOJANG OR MICROSOFT.

"Minecraft" is a trademark of Mojang Synergies AB. It is used here only
descriptively, to indicate that this tool launches Minecraft: Java Edition; its
use does not imply any affiliation with, sponsorship by, or endorsement from
Mojang or Microsoft. ember uses no Minecraft logos, fonts, or other brand
assets, and is provided in accordance with the
[Minecraft Usage Guidelines](https://www.minecraft.net/en-us/usage-guidelines).
Players must own a valid copy of Minecraft: Java Edition to use this launcher.
