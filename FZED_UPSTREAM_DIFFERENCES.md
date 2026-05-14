# FZed Upstream Differences

Use this file before merging an upstream Zed release into FZed. It records
intentional fork differences, why they exist, and what should be preserved when
resolving conflicts.

This is not an exhaustive list of every remaining `Zed`, `zed`, `Git`, or `git`
name in the tree. Many internal crate, type, setting, and action names remain
unchanged to reduce upstream merge churn. User-facing behavior and fork safety
matter more than mechanical renaming.

## Merge Rule

- Merge from upstream release tags, not from upstream `main`, unless a specific
  upstream fix is intentionally cherry-picked.
- Do not remove an intentional difference unless the reason is obsolete and the
  replacement preserves the same FZed behavior.
- If a merge changes any area listed here, update this file in the same change.
- Review `FZED_FOSSIL_PLAN.md` for the implementation status and historical
  verification notes.

## Merge Checklist

Before resolving or accepting upstream changes:

1. Read this file and `FZED_FOSSIL_PLAN.md`.
2. Confirm the target upstream release tag and update the documented baseline.
3. Resolve conflicts preserving the FZed behavior below.
4. Search for restored upstream-only update or distribution behavior:
   `cloud.zed.dev`, `zed-industries/zed`, `Zed.dmg`, `Zed.exe`,
   `zed.tar.gz`, and `zed-remote-server`.
5. Search for restored user-facing Git-only labels in shared SCM surfaces,
   especially panel names, diff actions, history views, and command palette
   entries.
6. Run the focused Fossil/update checks that match the touched area. Useful
   defaults:

```sh
cargo fmt --all -- --check
cargo check -p git -p project -p git_ui -p proto -p editor -p agent_ui
cargo test -p git fossil --lib
cargo test -p project fossil --lib
cargo test -p git_ui fossil --lib
cargo test -p worktree --features test-support fossil_repository_detection
cargo test -p proto split_repository_update
cargo test -p auto_update --lib
git diff --check
```

## Intentional Differences

### Upstream Release Cadence

Files: `README.md`, `FZED_FOSSIL_PLAN.md`

FZed tracks upstream Zed release tags instead of upstream `main`.

Reason: upstream `main` changes daily. Tracking release tags keeps integration
work bounded and makes the FZed delta easier to review after each upstream
release.

### Fossil Support Is Additive

Files: `crates/git`, `crates/project`, `crates/worktree`, `crates/fs`,
`crates/git_ui`, `crates/proto`

FZed adds Fossil support alongside Git support. It does not replace the Git
backend. Repository state carries a repository kind, Fossil checkouts are
detected through `.fslckout` and `_FOSSIL_`, and the Fossil backend shells out
to the `fossil` binary.

Reason: preserving Git support keeps the fork usable for normal Zed workflows
and makes upstream merges much easier. Calling the Fossil binary also follows
Fossil's stable CLI surface and avoids binding FZed to an incomplete library.

### Fossil-Native File Selection

Files: `crates/git`, `crates/project`, `crates/git_ui`, `crates/proto`

Fossil file inclusion for check-in is modeled as FZed/project state, not as a
fake Git index. A Fossil check-in commits the selected files with
`fossil commit FILE...`; extra and missing paths are added or removed just in
time when Fossil requires it.

Reason: Fossil does not have Git's index. The UI may look similar to Git's
staging UI where that is useful, but the backend should stay Fossil-native.
Hunk-level include/exclude is intentionally unsupported because Fossil has no
native hunk commit primitive.

Collaborative selected-path propagation mirrors Git staging state and is
intentional as long as it remains session/editor state rather than Fossil
repository state.

### Fossil Commands

Files: `crates/git/src/git.rs`, `crates/git_ui`

FZed exposes Fossil-specific command aliases for workflows that make sense for
Fossil users:

- check-in: `fossil::CheckIn`, `fossil::GenerateCheckInMessage`,
  `fossil::ViewCheckIn`
- file selection: `fossil::IncludeFile`, `fossil::ExcludeFile`,
  `fossil::ToggleIncluded`, `fossil::IncludeRange`, `fossil::IncludeAll`,
  `fossil::ExcludeAll`
- checkout maintenance: `fossil::RevertFile`,
  `fossil::RevertTrackedFiles`, `fossil::CleanExtras`
- repository setup: `fossil::Init`, `fossil::Clone`,
  `fossil::OpenRepository`
- sync/history: `fossil::Sync`, `fossil::Update`, `fossil::Timeline`,
  `fossil::FileTimeline`, `fossil::Annotate`, `fossil::Blame`
- navigation and repository state: `fossil::Branch`,
  `fossil::SwitchBranch`, `fossil::Checkouts`, `fossil::SelectRepo`,
  `fossil::OpenModifiedFiles`, `fossil::CopyBranchName`
- stash: `fossil::StashTracked`, `fossil::PopStash`,
  `fossil::ApplyStash`, `fossil::ViewStash`

Reason: users should be able to discover Fossil operations directly from the
command palette. Do not add one-for-one aliases for Git-only concepts such as
hunk staging, amend, signoff, rebase, force-push, GitHub pull requests,
`.gitignore`, or `fossil ui`.

### User-Facing SCM Names

Files: `crates/git_ui`, `crates/project`, `crates/zed`

Shared SCM UI should use generic source-control wording, or Fossil-specific
wording when a Fossil repository is active. Examples include "check-in",
"timeline", and "source control" instead of Git-only wording in Fossil
contexts.

Reason: FZed should not make Fossil look like Git with different plumbing.
Internal `GitRepository`, `GitStore`, `GitPanel`, crate names, and tests may
remain Git-named until a dedicated low-risk rename is worth the merge churn.

### Runtime Update And Release Safety

Files: `crates/release_channel/src/lib.rs`,
`crates/auto_update/src/auto_update.rs`,
`crates/auto_update_ui/src/auto_update_ui.rs`

FZed must not poll Zed's binary update channel, download Zed release artifacts,
or replace itself with upstream Zed. `ReleaseChannel::poll_for_updates()` is
disabled for all channels. Manual update checks explain that FZed binary
updates are not available yet and open the FZed GitHub releases page. Release
note links point to FZed GitHub pages. Remote-server release downloads are also
blocked until FZed has its own matching artifacts or another explicit
distribution path.

Reason: FZed does not publish official binaries yet. Any inherited Zed updater
behavior would be unsafe for this fork.

Future acceptable behavior: check GitHub for official FZed releases and open
the release page in a browser. Do not auto-download or self-install binaries
until FZed owns the full release pipeline.

### CI

Files: `.github/workflows/run_tests.yml`, `README.md`

FZed has a fork-owned focused CI workflow and README badge. It should not depend
on upstream Zed's private or self-hosted CI infrastructure.

Reason: upstream CI is larger than needed for the current fork and assumes
resources this repository does not own. The FZed workflow should focus on the
Fossil and fork-safety surface area.

When using GitHub CLI against this repository, pass the FZed repository
explicitly if needed so commands do not accidentally inspect upstream Zed.

### Branding, Binary Name, And User Data

Files: `README.md`, `crates/paths/src/paths.rs`, `crates/zed/src/main.rs`,
`crates/release_channel/src/lib.rs`

User-facing fork names should follow Zed's capitalization pattern: use `fzed`
where Zed would use lowercase `zed`, and `FZed` where Zed would use capitalized
`Zed`. The local development binary is `fzed`. Global/user settings, data,
logs, cache, and temp directories use `fzed` paths rather than upstream Zed
paths.

Reason: FZed should be installable and runnable alongside upstream Zed without
sharing global user data or presenting itself as upstream Zed.

Project settings intentionally remain `.zed/settings.json`, `.zed/tasks.json`,
and `.zed/debug.json` for now.

Reason: repository-local editor metadata should stay compatible with upstream
Zed and existing projects. Add a `.fzed/` layer only if FZed-specific project
configuration becomes necessary.

### Telemetry Defaults

File: `assets/settings/default.json`

FZed defaults anonymous usage metrics and diagnostic/crash telemetry to false.

Reason: this fork should not opt users into upstream Zed telemetry or crash
reporting by default.

## Known Inherited Surfaces

These are not necessarily intentional long-term differences, but they are
important during upstream merges:

- `script/install.sh` and `script/get-released-version` still target Zed Cloud.
  Treat them as inherited upstream tooling, not FZed release tooling, until they
  are explicitly rewritten for FZed.
- Platform app identifiers in `crates/release_channel/src/lib.rs` still use
  upstream-style IDs in some places. Do not change them casually; bundle IDs,
  application IDs, URL schemes, settings migration, and update behavior need to
  be migrated deliberately.
- Many source identifiers still say Git because the original Zed SCM layer is
  Git-shaped. User-facing Fossil behavior has priority over internal renaming.

## Explicitly Out Of Scope

- Fossil hunk staging or hunk revert emulation.
- Managed `fossil ui` process lifecycle or port allocation.
- Fossil wiki, ticket, forum, chat, or other non-SCM features unless upstream
  Zed gains comparable built-in GitHub issue/wiki functionality.
