# Privacy Statement

**Effective date:** June 25, 2026

This Privacy Statement explains how **ember**, an open-source, unofficial launcher
for Minecraft: Java Edition ("the Software"), handles information. The Software is
maintained by the ember project and distributed at
<https://github.com/xcud/ember>.

## Summary

ember runs entirely on your own computer. **It has no servers, collects no
analytics, and contains no telemetry or tracking.** It does not transmit any
information about you to the ember project — we never receive your data, because
there is nowhere for it to go. The only data the Software handles is what it needs
to sign you in to your own Microsoft account and to download the game files, mods,
and content you ask for, and all of that stays on your machine.

## Information the Software handles

**Microsoft / Xbox Live / Minecraft credentials.** To enable online play, ember
uses the standard Microsoft OAuth 2.0 device-code flow to obtain Xbox Live and
Minecraft tokens. It requests only the `XboxLive.signin` scope. The resulting
refresh and access tokens, and basic account details returned by the sign-in flow
(such as your Minecraft profile name and UUID), are stored **locally on your
device only**, in your user configuration directory, with restrictive file
permissions (`0600` on Unix-like systems). These tokens are sent only to
Microsoft, Xbox Live, and Minecraft services to authenticate you — never to the
ember project or any other party.

**Game, mod, and instance data.** Your instances, manifests (`pack.toml` /
`pack.lock`), downloaded game files, libraries, assets, mods, resource packs, and
shaders are stored locally on your device. ember does not upload or share them.

## Network connections the Software makes

The Software connects directly from your device to the following third-party
services, only as needed to perform the action you requested:

- **Microsoft and Xbox Live** — to sign you in and obtain play tokens.
- **Mojang / Minecraft services** — to authenticate and to download official
  game versions, libraries, and assets.
- **Modrinth** (`api.modrinth.com`) — to search for and download mods, modpacks,
  resource packs, and shaders, and to resolve content by hash.
- **FabricMC** (`meta.fabricmc.net`) — to download the Fabric mod loader.

Each of these services has its own privacy policy that governs the information it
receives when your device contacts it. We do not control these services and do
not receive copies of those communications.

## What the Software does **not** do

- It does not run any ember-operated server or backend.
- It does not collect analytics, usage statistics, or telemetry.
- It does not track you or build any profile of you.
- It does not sell, rent, or share personal information — we never collect it in
  the first place.
- It contains no advertising.

## Data retention and deletion

Because all data is stored locally, you are in full control of it. You can revoke
ember's access at any time from your Microsoft account security settings
(<https://account.microsoft.com>), and you can delete all locally stored tokens
and data by signing out (`ember logout`, where available) or by removing the
Software's configuration and data directories from your device.

## Children's privacy

The Software is a launcher used with a Microsoft account. Use of Microsoft and
Minecraft accounts, including by children, is governed by Microsoft's and
Mojang's terms and privacy policies.

## Changes to this statement

We may update this Privacy Statement from time to time. Changes take effect when
the updated version is published in the project repository.

## Contact

Questions about this Privacy Statement can be raised as an issue at
<https://github.com/xcud/ember/issues>.
