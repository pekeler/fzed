# FZed

[![CI](https://github.com/pekeler/fzed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/pekeler/fzed/actions/workflows/run_tests.yml)

FZed is a fork of [Zed](https://zed.dev) focused on adding first-class
[Fossil SCM](https://fossil-scm.org/) support while keeping the fork close
enough to upstream Zed to continue merging upstream changes.

<img width="200" alt="cloning a repo" src="https://github.com/user-attachments/assets/a30a433b-7ede-4c29-88c4-2043319286ad" />
<img width="200" alt="entering a repo's URL" src="https://github.com/user-attachments/assets/e357f3df-5467-4bd3-9f95-2a441d209bb1" />
<img width="200" alt="the source control panel with some changes" src="https://github.com/user-attachments/assets/97fea045-a971-4ab6-980d-f14a4bf512f0" />
<img width="200" alt="viewing a file" src="https://github.com/user-attachments/assets/46489c5b-0b61-4067-bc53-076595aa42e7" />
<img width="200" alt="viewing a file's history" src="https://github.com/user-attachments/assets/c4c9a93b-32a6-438c-aa8f-ef0643267df6" />
<img width="200" alt="the repo's timeline" src="https://github.com/user-attachments/assets/b30743a5-b34c-498c-9756-1ffd08cb0225" />
<img width="200" alt="the context menu" src="https://github.com/user-attachments/assets/7fddcd84-1a8b-49b3-a9a3-86cb2f97cb02" />
<img width="200" alt="the command palette" src="https://github.com/user-attachments/assets/137266dd-b610-460d-88ed-8817ec053169" />

## Fork-Specific Documentation

### Upstream Tracking Policy

FZed tracks upstream Zed release tags, not upstream `main`.

The fork should be rebased or merged forward when Zed publishes a new release
tag, then FZed-specific commits should be replayed and tested on top of that
release. Between upstream releases, avoid pulling daily upstream `main` commits
unless a specific fix is needed and is intentionally cherry-picked.

Before each upstream merge, review
[FZed Upstream Differences](./FZED_UPSTREAM_DIFFERENCES.md). It lists the
intentional fork differences that should be preserved while resolving conflicts.

Current upstream baseline: `v1.6.3`.

### Versioning

FZed versions are derived from the upstream Zed release tag they are based on:

- If the upstream tag is `vX.Y.Z`, the first FZed release based on that tag is
  `X.Y.Z-fzed.0`.
- If FZed publishes fork-only follow-up releases without changing the upstream
  base tag, increment the suffix: `X.Y.Z-fzed.1`, `X.Y.Z-fzed.2`, and so on.
- When FZed moves to a newer upstream Zed release tag, reset the suffix to
  `fzed.0` for that upstream version.

Use the prerelease suffix, not build metadata, so fork-only follow-ups sort in
release order. FZed update checks must compare only against FZed releases, not
against upstream Zed releases.

### Fossil Executable

FZed does not bundle Fossil. Install Fossil separately and make sure the
`fossil` executable is available.

When opening an existing Fossil checkout, FZed looks for `fossil` in the
project shell environment's `PATH`, then in the app process `PATH`. On macOS,
it also checks common package-manager locations:

- `/opt/homebrew/bin/fossil`
- `/usr/local/bin/fossil`
- `/opt/local/bin/fossil`
- `/sw/bin/fossil`

FZed shows a clear error if Fossil cannot be found.

On macOS, apps launched from Finder, Dock, or Spotlight may not inherit the
same `PATH` as Terminal shells. If Fossil is installed in a custom location,
either start FZed from a shell that already has the right `PATH`, or update the
per-user launchd `PATH`.

For the current login session only:

```sh
launchctl setenv PATH "/opt/homebrew/bin:/usr/local/bin:/opt/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
```

For a persistent setting:

```sh
launchctl config user path "/opt/homebrew/bin:/usr/local/bin:/opt/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
```

The persistent setting requires a reboot before newly launched GUI apps see the
new `PATH`.

### Current Development Build

The Cargo package is still named `zed` to reduce upstream merge churn, but the
fork's local development binary is named `fzed`.

Build it with:

```sh
cargo build -p zed
```

Run the latest local debug build with:

```sh
target/debug/fzed
```

Build an optimized local binary with:

```sh
CARGO_INCREMENTAL=0 cargo build -p zed --release
```

Run the optimized binary with:

```sh
target/release/fzed
```

On macOS, build a distributable `.app` bundle and `.dmg` with:

```sh
script/bundle-mac
```

The bundled app/DMG is written under `target/<target-triple>/release/`. Local
builds are ad-hoc signed unless the Apple signing and notarization environment
variables expected by `script/bundle-mac` are configured.

### Settings And Data

FZed uses separate global/user settings and application data from upstream Zed:

- user settings: `~/.config/fzed/settings.json`
- global settings: `~/.config/fzed/global_settings.json`
- keymap, tasks, and themes: under `~/.config/fzed/`
- data, database, extensions, languages, and debug adapters:
  `~/Library/Application Support/fzed/`
- logs: `~/Library/Logs/fzed/fzed.log`
- cache/temp data: `~/Library/Caches/fzed/`
- debug-build credentials: `~/.config/fzed/development_credentials`
- production keychain credentials: namespaced as FZed entries, not shared with
  upstream Zed

Project settings are intentionally still shared with Zed for now:

- project settings: `.zed/settings.json`
- project tasks: `.zed/tasks.json`
- project debug config: `.zed/debug.json`

This keeps repository-local editor metadata compatible with upstream Zed and
with existing projects. A future `.fzed/` project settings layer can be added if
FZed-only project configuration becomes necessary.

The `--user-data-dir <DIR>` CLI option overrides the default FZed user data
locations for a single run.

### Expected First-Run Debug Logs

Debug builds are more verbose than packaged release builds. These messages are
expected during local development:

- `sqlez::migrations ...`: local database setup or schema checks
- `Debug assertions enabled, skipping hang monitoring`: normal debug build
- `Minidump endpoint not set`: no crash-upload endpoint is configured
- `Couldn't find any enabled panel for the Left dock`: harmless layout state
- extension installation/index rebuild messages: normal first-run extension setup

---

# Original Zed README

# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
