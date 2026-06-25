# ember

A fast, robust, terminal-first launcher for Minecraft: Java Edition, written in Rust.

> **ember is NOT AN OFFICIAL MINECRAFT PRODUCT. NOT APPROVED BY OR ASSOCIATED
> WITH MOJANG OR MICROSOFT.**

`ember` is an open-source, unofficial launcher built around a clean, UI-agnostic
core (`launcher-core`), a terminal UI (`ember-tui`), and a scriptable CLI. It
manages instances, mods, resource packs, shaders, and Modrinth modpacks;
resolves compatibility-aware updates; installs Minecraft and mod loaders itself;
and launches the player's own copy of Minecraft: Java Edition — designed to be
left open as a low-overhead companion while you play, rather than a heavyweight
window competing for resources.

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

A workspace of three crates, splitting the engine from the interfaces so the
same core backs the CLI, the TUI, and (eventually) an AI agent driving the
launcher conversationally.

```
crates/
  launcher-core/       UI-agnostic library (the engine)
    manifest.rs        pack.toml (intent) + pack.lock (resolved, hashed); content types
    modrinth.rs        Modrinth API client (content-hash, version, search queries)
    import.rs          reverse-resolve an existing mods/ dir into a manifest
    download.rs        streaming, SHA-1-verified, content-addressed cache
    sync.rs            reproduce a lock into mods/ (parallel, prune, verify)
    update.rs          compatibility-aware re-resolution + diff (update/bump)
    install.rs         install vanilla + Fabric (manifests, libraries, assets)
    launch.rs          version resolution + launch-command construction
    auth.rs            Microsoft / Xbox Live / Minecraft authentication
    instance.rs        instances = pack + game dir + launch config
    modpack.rs         Modrinth .mrpack import
    manage.rs          per-instance mod/content add, remove, update (with deps)
    bin/ember.rs       the CLI
  ember-term/          PTY virtual-terminal sessions (portable-pty + vt100)
  ember-tui/           the terminal UI (ratatui)
```

`ember` is self-sufficient: it downloads versions, loaders, libraries, and
assets itself (verifying everything by hash and reusing what's already on disk),
so it doesn't depend on the official launcher having fetched them.

## The TUI

`ember-tui` is the primary interface: a sidebar of instances and a tabbed detail
pane.

```
┌─ instances ─────┬─ 1 Properties │ 2 Content │ 3 Console ─────────┐
│ ▸ main (linked) │  ▸ Mods │ Resource Packs │ Shaders             │
│   just-a-few    │  Sodium                 v0.6.3                 │
│   26.2-test     │  Iris Shaders           v1.10.7               │
│                 │  …                                            │
│                 │ ┌─ details ───────────────────────────────┐  │
│                 │ │ Sodium  v0.6.3                            │  │
│                 │ │ A high-performance rendering engine …     │  │
│                 │ └───────────────────────────────────────────┘  │
└─────────────────┴───────────────────────────────────────────────┘
```

- **Properties** — version, linked target, game dir, RAM, mod count, last played.
- **Content** — Mods / Resource Packs / Shaders, each with rich metadata
  (title, version, description) and a Modrinth search-and-install picker.
- **Console** — the running game's log, streamed through a real PTY, scrollable.

Navigation is fully spatial — arrow keys move between zones (sidebar → tabs →
type strip → body), `↓`/`Enter` go in, `↑`/`Esc` step out, `Tab` hops to the
tabs. Letters only ever *act*: `p` play, `a` add, `r` remove, `u` update,
`n`/`c`/`d`/`i` manage instances.

Instance mod metadata is enriched in the background, so an instance shows rich
names and descriptions shortly after you select it, with no manual step.

## CLI

The same operations are scriptable from `ember`:

```
ember import <mods_dir>             # reverse-resolve an existing mods/ into pack.toml + pack.lock
ember sync                          # download + verify mods to match pack.lock
ember update [--game <version>]     # re-resolve / bump to a new Minecraft version
ember install <version> [--fabric <loader>]   # install vanilla + Fabric
ember instance list|new|clone|delete
ember modpack import <file.mrpack>  # import a Modrinth modpack as an instance
ember mod add|remove|update <instance> [--type mod|resourcepack|shader]
ember launch <version_id>           # build and run the launch command (offline or signed-in)
ember login | whoami                # Microsoft account
```

### Example

```console
$ ember modpack import "Just a Few.mrpack"
Imported 'Just a Few' as instance 'just-a-few'
  1.21.1 on fabric (fabric-loader-0.16.9-1.21.1)
  15 files installed, 20 overrides

$ ember install 1.21.1 --fabric 0.16.9
Installed fabric-loader-0.16.9-1.21.1: 9 downloaded, 3966 already present

$ ember mod add just-a-few waystones
  [+ add ] waystones 21.1.34+fabric-1.21.1
  [+ add ] balm 21.0.59+fabric-1.21.1      # required dependency, pulled automatically
```

## Authentication

`ember` uses the standard Microsoft OAuth 2.0 device-code flow to obtain Xbox
Live / Minecraft tokens for online play. It requests only the `XboxLive.signin`
scope, persists the refresh token locally (with `0600` permissions) so sign-in
is a one-time action, and stores nothing on any server. Single-player works in
offline mode without signing in.

## Status

Functional and self-contained. Working today: instance management, Modrinth
modpack import, self-sufficient version/loader installation, content management
(mods with dependency expansion, resource packs, shaders) with a search picker,
compatibility-aware update/bump, launching (offline; online via Microsoft auth),
and the terminal UI with background metadata enrichment.

Microsoft online auth requires the Azure app to be approved for the Minecraft
API; offline single-player works without it. Resource packs and shaders are
currently folder-managed (not yet part of `pack.lock`); full lock-tracking for
them is planned, along with quick-play world launching and a multiplexed
companion shell.

## License

MIT — see [LICENSE](LICENSE).

## Legal

- [Terms of Service](docs/TERMS.md)
- [Privacy Statement](docs/PRIVACY.md)

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
