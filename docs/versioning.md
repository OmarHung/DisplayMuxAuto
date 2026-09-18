# Versioning

The project is on `0.MINOR.PATCH` and will stay there until the agent protocol and the
settings format are ones worth promising to keep compatible.

## Which number moves

Raise **MINOR** (`0.N.0`) when any of these is true:

1. **The upgrade asks something of the user.** Pairing again, reconfiguring, reinstalling —
   anything that does not work until they act.
2. **A host on the previous version loses a capability.** Not a feature it never had: a thing
   it could do before and cannot do with an updated peer.
3. **Where the app is, or how it is reached, changes.** Leaving the Dock for the menu bar,
   changing what launches it, moving where its window lives.

Raise **PATCH** (`0.x.N`) for everything else, including new features that an older peer
simply ignores.

Reach `1.0.0` when the agent protocol and the settings format are stable enough to promise
compatibility going forward.

## Why the third rule exists

It was added for `0.3.0`, which moved the macOS app into the menu bar and out of the Dock.
Nothing broke, nothing needed configuring, and no peer lost anything — so the first two rules
called it a patch. But a macOS user who updates goes looking in the Dock and does not find
the app.

A version number's job is to signal before anyone reads the notes. When it takes a patch
number to a change that leaves someone hunting for their app, it has stopped doing that job.

## What this is not

This is not the agent protocol version. `AGENT_PROTOCOL_VERSION` in
`crates/muxsu-core/src/network.rs` decides whether two hosts can understand and authenticate
each other. It moves on its own schedule and is bumped whenever authentication or request
interpretation changes incompatibly.

The complete Agent response, including `protocol_version`, is signed. A client accepts response
data only after verifying that signature and an exact protocol-version match.

## The version lives in six places

All must agree, or the release workflow builds one version and names the artifacts another:

```
package.json
src-tauri/tauri.conf.json
src-tauri/Cargo.toml
crates/muxsu-core/Cargo.toml
crates/muxsu-cli/Cargo.toml
Cargo.lock                        # via `cargo update -w --offline`
```

A version containing `-` is published as a pre-release and does not become the updater's
`latest`, so existing installations are never offered it. That check is one line in
`.github/workflows/release.yml`:

```bash
node -p '"prerelease=" + require("./src-tauri/tauri.conf.json").version.includes("-")'
```

Release notes go in `.github/release-notes/v<version>.{zh-TW,en}.md`, and stable releases are
added to `releaseHistoryFallback` in `src/main.ts` so the in-app history still lists them when
GitHub cannot be reached.
