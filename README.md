# fzed

fzed is a fork of [Zed](https://zed.dev) focused on adding first-class
[Fossil SCM](https://fossil-scm.org/) support while keeping the fork close
enough to upstream Zed to continue merging upstream changes.

## Fork-Specific Documentation

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

### Settings And Data

fzed uses separate global/user settings and application data from upstream Zed:

- user settings: `~/.config/fzed/settings.json`
- global settings: `~/.config/fzed/global_settings.json`
- keymap, tasks, and themes: under `~/.config/fzed/`
- data, database, extensions, languages, and debug adapters:
  `~/Library/Application Support/fzed/`
- logs: `~/Library/Logs/fzed/fzed.log`
- cache/temp data: `~/Library/Caches/fzed/`

Project settings are intentionally still shared with Zed for now:

- project settings: `.zed/settings.json`
- project tasks: `.zed/tasks.json`
- project debug config: `.zed/debug.json`

This keeps repository-local editor metadata compatible with upstream Zed and
with existing projects. A future `.fzed/` project settings layer can be added if
fzed-only project configuration becomes necessary.

The `--user-data-dir <DIR>` CLI option overrides the default fzed user data
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

- Web ([tracking issue](https://github.com/zed-industries/zed/issues/5396))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

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
