# fzed Fossil Integration Plan

Date: 2026-05-12

## Fork Status

- GitHub fork: https://github.com/pekeler/fzed
- Upstream: https://github.com/zed-industries/zed
- Local checkout: `/Users/pekeler/Projects/FZed`
- Current base commit: `78c889c21d7ed84dd16c1678ae69867a85417fb4`
- Remotes:
  - `origin` -> `https://github.com/pekeler/fzed.git`
  - `upstream` -> `https://github.com/zed-industries/zed.git`

## Local Tooling Baseline

Installed or verified on 2026-05-12:

- Rust toolchain: `rustc 1.95.0`, matching `rust-toolchain.toml`
- Cargo: `cargo 1.95.0`
- Rust components: `rustfmt`, `clippy`, `rust-analyzer`, `rust-src`
- Rust targets: `aarch64-apple-darwin`, `wasm32-wasip2`, `wasm32-unknown-unknown`, `x86_64-unknown-linux-musl`
- Fossil: `2.27 [99675884a9] 2025-09-30`
- Xcode: `26.5`, selected at `/Applications/Xcode.app/Contents/Developer`
- Metal compiler: available via `xcrun --find metal`
- CMake: `4.3.2`, installed via Homebrew

Validation so far:

- `cargo metadata --format-version 1 --no-deps` succeeds for the workspace.
- `cargo check -p git` succeeds after populating Cargo's normal dependency cache; keep using it as the fast build check for SCM backend work.
- First Fossil backend slice compiles:
  - `cargo check -p git`
  - `cargo check -p fs -p worktree -p project`
  - `cargo test -p git fossil`
  - `cargo test -p worktree --features test-support fossil_repository_detection`
- Repository-kind/UI slice compiles:
  - `cargo check -p git -p project -p git_ui -p collab`
  - `cargo test -p proto split_repository_update`
  - `git diff --check`
- Phase 1 completion slice compiles:
  - `cargo check -p git_ui -p collab -p project -p git`
- Phase 2 file-selection/check-in slice compiles:
  - `cargo check -p git -p project -p git_ui -p proto`
  - `cargo test -p git fossil`
  - `cargo test -p project test_fossil_repository`
  - `cargo test -p fs fake_git_repo`
  - `cargo test -p worktree --features test-support fossil_repository_detection`
  - `cargo test -p proto split_repository_update`

## Goal

fzed should provide Fossil support comparable in quality to Zed's current Git experience, but optimized for Fossil's model rather than making Fossil pretend to be Git:

- repository detection and status updates
- Fossil-native changes/check-in panel
- project diff and file diff views
- editable diff views
- selected-file include/exclude workflow for the next check-in
- commit message editor and `fossil commit` action
- branch operations
- sync/autosync awareness
- multiple-checkout workflows
- stash, file history, blame, timeline, and conflict surfaces where Fossil supports them

## License Finding

Zed can be forked and modified, but fzed must respect the copyleft licenses already in the tree.

- Most Zed crates, including `crates/git`, `crates/git_ui`, `crates/project`, `crates/worktree`, and `crates/fs`, are `GPL-3.0-or-later`.
- `crates/collab` is `AGPL-3.0-or-later`.
- Several lower-level crates, including GPUI-related crates and some utility/client crates, are `Apache-2.0`.
- The root contains `LICENSE-GPL`, `LICENSE-AGPL`, and `LICENSE-APACHE`.

Practical consequence: a public or distributed fzed build is allowed, but corresponding source and license notices must remain available under the applicable GPL/AGPL terms. The AGPL matters especially if we run or distribute modified collaboration/network service components. This is not legal advice; before commercial distribution, have counsel review the exact build composition and distribution model.

## Current Zed SCM Architecture

Relevant code paths:

- `crates/git/src/repository.rs`
  - Defines `GitRepository`, the central backend trait.
  - `RealGitRepository` uses both `git2` and the `git` CLI.
  - Status, branches, worktrees, staging, unstaging, commits, remotes, stash, diff, blame, and graph methods are all Git-shaped.
- `crates/git/src/status.rs`
  - Defines `FileStatus`, `StageStatus`, `GitStatus`, `GitSummary`, diff stats, and parsers for Git porcelain output.
- `crates/project/src/git_store.rs`
  - Owns repository snapshots, status trees, buffer diff state, pending operations, job scheduling, and local/remote project propagation.
  - UI-facing code talks mostly to `GitStore` and `Repository`, not directly to `git2`.
- `crates/git_ui/src/*`
  - Implements the panel, project diff, text/file diff views, commit UI, branch picker, stash picker, blame UI, etc.
- `crates/worktree/src/worktree.rs`
  - Detects repositories by scanning for `.git`.
  - Emits `UpdatedGitRepository` entries to `GitStore`.
- `crates/fs/src/fs.rs`
  - `Fs::open_repo` returns `Arc<dyn GitRepository>`.

Important observation: the UI is reusable, but the domain model is named and shaped around Git. The strongest coupling is the Git index/staging model, not the diff renderer.

## Fossil Findings

Fossil has enough primitives for a high-quality integration:

- Repository checkout discovery is based on a checkout database named `.fslckout` or `_FOSSIL_`.
- `fossil changes|status` reports changed files and can classify changed, added, deleted, missing, conflict, and extra files.
- `fossil diff` emits unified diffs and supports `--numstat`.
- `fossil commit FILE...` can commit a subset of files.
- `fossil stash` supports save, list, show, apply, pop, drop, and diff.
- `fossil branch` supports current/list/new/close/reopen/hide/unhide.
- `fossil annotate|blame|praise` provides line attribution.

The major mismatch is staging. Fossil intentionally has no Git index. Plain Fossil commits all changes by default, or a listed subset of files. That supports file-level commit selection, but not Git-style hunk staging without an fzed-managed synthetic staging layer.

Additional UX implications from Fossil's own "Fossil Versus Git" comparison:

- Fossil is more than file versioning: tickets, wiki, docs, forum, notes, chat, web UI, and role-based access are part of the project model.
- Fossil is one self-contained executable, so setup and diagnostics should assume `fossil` as the primary interface.
- Fossil stores state in SQLite databases, making project history and descendant/timeline queries first-class product opportunities rather than bolt-ons.
- Fossil emphasizes the entire DAG and all branches, not a single currently-pushed branch.
- Fossil separates repository databases from working checkouts and encourages multiple checkouts where they fit the work.
- Fossil records what actually happened and avoids most history rewriting. UI should not center Git-like rebase, squash, force-push, or reset workflows.
- Fossil's default autosync means commit is closer to a public commitment; the editor should help users inspect and test before commit.
- Fossil merge/cherrypick/backout applies changes to the working checkout first and expects an explicit commit after testing.

## Recommendation

Do not replace Zed's Git backend with Fossil. Keep Git and add Fossil as a second backend.

Reasoning:

- Replacing Git would be faster only for a narrow private proof of concept, and it would make every upstream merge harder because almost all upstream SCM work will continue to assume Git exists.
- The existing diff and changes-list UI can probably be reused if we introduce an SCM backend boundary under `GitStore`, but the Fossil product surface should not be limited to Git-shaped commands.
- File-level "staging" can be a Fossil-native include/exclude step for `fossil commit FILE...`, not a fake Git index.
- Hunk staging should be treated as an explicit later product decision, because implementing it faithfully requires an fzed-managed overlay that Fossil itself does not have.
- Keeping Git makes fzed a credible long-lived fork rather than a one-off patch set.

Recommended strategy: adapter-first extension.

1. Add Fossil support behind the existing `GitStore`/`GitRepository` surface where this helps reuse Zed's diff and panel machinery.
2. Build Fossil-native UX decisions at the product layer: check-ins, autosync, whole-DAG visibility, timeline/history, and multiple checkouts.
3. Rename/generalize user-visible and internal concepts only after Fossil works end to end.
4. Avoid a repo-wide "Git -> SCM" rename as the first step; it creates churn before proving the backend.

## Fossil Backend Implementation Choice

Use the `fossil` binary for the initial backend. Do not start by embedding a Fossil repository library.

Research summary:

- `heroforge-core` is the only notable Rust crate found that claims broad Fossil read/write support. It is Apache-2.0 and describes a pure-Rust API for reading and writing Fossil repositories, but crates.io currently shows a single `0.2.2` release, 26 total downloads, and no established adoption signal.
- The related `heroforge` crate is yanked, so it is not a candidate.
- `fslutils` is small and useful-looking for checkout inspection only, but it is not a full SCM backend.
- `libfossil` is an unofficial C99 library by a long-time Fossil contributor. It is promising, but its own documentation calls out beta/alpha status, no API stability guarantee, no 100% feature-parity goal, missing or pending features such as stash/undo in the documented status, and continued reliance on the Fossil executable for network synchronization.
- Fossil's JSON API is useful for server/web/timeline-style integrations, but the docs describe it as JSON-over-HTTP rather than REST, explicitly not a goal to cover every Fossil feature, and not all API surfaces are final.

Decision:

- Build a `FossilBinary` wrapper first, analogous to Zed's current use of the `git` CLI in addition to `git2`.
- Prefer Fossil commands whose output is stable enough to parse or already structured: `fossil info`, `changes/status`, `diff`, `branch`, `stash`, `annotate`, `timeline`, `json` where appropriate.
- Keep all command invocation and parsing behind a narrow Rust trait so a future library-backed implementation can replace individual operations without changing UI code.
- Avoid direct SQLite repository reads in Phase 1. That would bypass Fossil's compatibility, safety, settings, autosync, and checkout semantics before we know where performance actually hurts.
- Re-evaluate libraries later for read-heavy history/timeline operations if shelling out proves too slow on large repositories.

## Implementation Plan

### Phase 0: Build and Test Baseline

- Build the fork unchanged.
- Run the existing Git/project tests relevant to `crates/git`, `crates/project`, `crates/worktree`, `crates/fs`, and `crates/git_ui`.
- Add a small local Fossil fixture repo for backend experiments.

### Phase 1: Fossil Detection and Read-Only Status

- Add Fossil checkout discovery to `worktree` by scanning ancestors for `.fslckout` and `_FOSSIL_`.
- Introduce a repository kind enum, initially internal: `Git` or `Fossil`.
- Add a Fossil backend that shells out to the `fossil` binary.
- Parse:
  - `fossil info` for checkout root, repository path, current check-in, and branch.
  - `fossil changes --classify --differ --no-merge --rel-paths` for file status.
  - `fossil diff --internal --unified` for diff text.
  - `fossil diff --numstat` for per-file diff stats.
- Surface Fossil status in the existing panel and project panel.

Expected result: Fossil repositories show changed files and open diffs, but commit/stage controls can be disabled or labeled experimental.

Initial implementation status:

- Added `.fslckout` and `_FOSSIL_` discovery to the existing worktree repository scanner.
- Added a `FossilRepository` backend inside the current `git` crate boundary so the first patch can reuse `GitStore` without a repo-wide rename.
- `RealFs::open_repo` now opens `FossilRepository` when the repository metadata path is Fossil checkout metadata.
- `LocalRepositoryState` resolves the `fossil` binary for Fossil repositories and `git` for Git repositories.
- Implemented read-only Fossil primitives for current check-in, branch list/current branch, status parsing, committed file text, worktree diff, and diff stat.
- Added a real temporary Fossil checkout smoke test for status, committed text, branch, head, and Fossil's native `diff --numstat` output format.
- Added worktree integration tests for `.fslckout` and `_FOSSIL_` discovery.
- Added `RepositoryKind` to the backend trait and repository snapshots, and propagated it through live `UpdateRepository` messages.
- The commit panel and commit modal now use "Check In" / "Check In Tracked" and `fossil commit` tooltip metadata when the active repository is Fossil.
- Disabled Git-style staging/check-in controls for Fossil repositories until the Phase 2 Fossil-native include/exclude workflow is implemented.
- Collab repository rows now store repository kind in the included DB table model and test schemas instead of defaulting persisted rows back to Git.
- Left Git-only operations as explicit unsupported errors until their Fossil-native UX is designed.
- Phase 1 is complete.

### Phase 2: Fossil-Native File Selection and Check-In

- Present the staging affordance as "include in next check-in" / "exclude from next check-in" for Fossil repositories.
- Store selected paths in fzed UI state, not in a fake persistent Fossil index.
- For edited tracked files, commit selected paths via `fossil commit -m MESSAGE FILE...`.
- For extra/missing files, call `fossil add`, `fossil rm`, or `fossil addremove` at the moment the file is included.
- Preserve Fossil's behavior for "commit all" when no subset is selected.
- Make autosync state visible enough that users understand when a check-in will also sync.
- Avoid Git-specific terms such as push target, upstream branch, force push, amend/reset, or detached HEAD in Fossil views.

Expected result: the existing panel/diff affordances are available at file granularity without pretending Fossil has a Git index.

Initial implementation status:

- Added an in-memory Fossil include list to `project::Repository`; selected paths render as staged in the existing panel, but no Fossil index is created.
- Stage/unstage-all maps to include/exclude-all for Fossil repositories.
- File checkboxes and directory/range selection now work for Fossil at file granularity.
- `GitRepository::commit_paths` lets backends commit an explicit subset of files without changing Git behavior.
- `FossilRepository` now implements `fossil commit FILE...`.
- Extra files selected for check-in are added just-in-time with `fossil add --force --dotfiles`.
- Missing tracked files selected for check-in are marked with `fossil rm --soft` before committing.
- If no subset is selected and tracked files changed, the existing "Check In Tracked" path includes tracked changes and excludes extras.
- Fossil conflicts are blocked until resolved instead of treating staged conflicts as committable.
- Project diff and the panel expose Fossil "Include All" / "Exclude All" / "Check In" labels where the action maps cleanly.
- Hunk staging remains disabled for Fossil.
- Remote/collab commit messages can now carry selected Fossil paths through the `Commit` proto.

Phase 2 is implemented for local file-level selection and check-in.

Phase 2.1 coverage status:

- Added project integration tests for Fossil repository include/exclude state, including single-file include, include-all, single-file exclude, and exclude-all.
- Added a project integration test proving a Fossil check-in only moves selected paths into the fake repository HEAD.
- Extended the fake repository backend so `.fslckout` and `_FOSSIL_` fixtures report `RepositoryKind::Fossil` and can simulate `commit_paths`.
- Re-ran focused Fossil, project, fs fake-git, and `git_ui` compile checks.

Known Phase 2 gaps:

- Autosync visibility is not implemented yet; `fossil commit` currently honors Fossil's configured autosync behavior without surfacing it in the UI.
- The include list is intentionally in-memory. It is not persisted across restarts and is not broadcast as shared state to other collaborators.
- Git-only split-menu actions such as amend/signoff are still visible in some Fossil commit surfaces, though the commit path strips them before calling Fossil.
- Hunk-level Fossil selection still needs a separate fzed-managed overlay design if we decide to support it.

## Open TODOs

- Verify whether a real collab database migration is needed for existing `project_repositories.repository_kind` rows beyond the updated table model/test schemas.
- Add Fossil autosync state and last-sync diagnostics to the check-in UI before encouraging daily use.
- Rename more user-facing "Git" surfaces only where they are visible in Fossil repositories; avoid a broad internal rename until the Fossil path is stable.
- Add direct `git_ui` panel tests for Fossil labels and toolbar button states once there is a small-enough panel-selection harness; current coverage is at the UI-facing repository state layer.
- Decide whether selected Fossil paths should be shared in collaborative sessions or remain local editor state.

### Phase 3: Fossil-First Branch, Sync, and Checkout UX

- Treat branch names as shared repository state, not local labels.
- Prefer Fossil's create-branch-at-commit workflow:
  - normal branch creation can be offered from the commit/check-in UI via `fossil commit --branch NAME`
  - `fossil branch new` can remain an advanced action
- Model sync as Fossil does:
  - `fossil sync` is all-project synchronization
  - `fossil pull` does not update the checkout by itself
  - `fossil update` is the normal "bring this checkout forward" operation
- Add explicit UI for multiple checkouts from one repository:
  - show repository database path separately from checkout path
  - support opening sibling checkouts
  - make branch switching via a different checkout feel normal, not exotic
- Show "test before check-in" affordances where useful, such as running configured tasks from the check-in panel before committing.

Expected result: fzed behaves like a Fossil client rather than a Git client using Fossil commands.

### Phase 4: History, Timeline, Stash, and Merge Surfaces

- Timeline/history:
  - expose whole-DAG timeline concepts rather than only branch-local log views
  - use Fossil's query-friendly history model for descendants, leaves, and file history
- Stash:
  - list/show/apply/pop/drop map directly to `fossil stash`
  - present stash as a working-checkout feature, because Fossil stores it in checkout state
- Blame:
  - parse `fossil annotate|blame|praise` output into Zed's blame model
- Merge/cherrypick/backout:
  - expose them as working-checkout changes followed by an explicit check-in
  - avoid implying that merge creates an immediate durable commit
- Conflict handling:
  - reuse Zed's conflict surface where marker formats and file states map cleanly

### Phase 5: Optional Hunk Selection

Hunk selection is desirable UX, but it is not Fossil's native commit mechanism. Treat this as a later design decision after file-level check-ins are solid.

If implemented, it needs an fzed-managed overlay, because Fossil has no index:

- Baseline text: loaded from the current Fossil check-in
- Worktree text: loaded from disk
- Selected text: generated from baseline plus selected hunks
- Unselected text: derived as worktree minus selected overlay

Commit algorithm candidate:

1. Resolve the Fossil repository path and current check-in with `fossil info`.
2. Create a temporary checkout of the same repository at the same check-in.
3. Write the staged overlay files into the temporary checkout.
4. Run `fossil add`/`rm` as needed in the temporary checkout.
5. Run `fossil commit -m MESSAGE FILE...` there.
6. Update the user's checkout to the new check-in, allowing Fossil to merge the still-uncommitted local edits.
7. Detect conflicts and surface them in the existing conflict UI.

This needs careful tests for overlapping hunks, binary files, renames, deleted files, and local edits that happen while the commit is running.

### Phase 6: Generalize Names and UI

Once Fossil works:

- Rename internal `GitRepository`/`GitStore` boundaries to SCM-neutral names in small patches.
- Keep user-facing commands compatible by adding Fossil-specific command aliases, not by deleting existing Git actions.
- Update settings names carefully:
  - preserve existing Git settings
  - add `fossil.*` settings
  - consider future `version_control.*` shared settings only where semantics are genuinely shared

## Main Risks

- Hunk staging is the main technical risk because Fossil has no staging area.
- Temporary-checkout commits must be robust against concurrent user edits.
- Fossil rename/copy metadata may not map cleanly to Git-oriented status codes.
- Upstream Zed may keep changing `crates/git` rapidly, so broad renames should wait.
- Shelling out is simplest and safest initially, but performance should be measured on large Fossil checkouts.

## Open TODOs

- Implement Fossil-native file include/exclude state for the next check-in.
- Wire `fossil commit` for selected paths and commit-all behavior.
- Replace remaining Git-only labels in Fossil repository views where the semantics diverge.
- Decide whether hunk-level selection is in scope after file-level check-ins are solid.

## Source Links

- Zed repo: https://github.com/zed-industries/zed
- Zed Git docs: https://zed.dev/docs/git
- Fossil Versus Git: https://fossil-scm.org/home/doc/trunk/www/fossil-v-git.wiki
- Fossil Git-user guide: https://www.fossil-scm.org/home/doc/trunk/www/gitusers.md
- Fossil technical overview: https://www.fossil-scm.org/home/doc/trunk/www/tech_overview.wiki
- Fossil `changes`: https://www3.fossil-scm.org/home/help/changes
- Fossil `diff`: https://www2.fossil-scm.org/home/help/diff
- Fossil `commit`: https://www2.fossil-scm.org/home/help?cmd=commit
- Fossil `stash`: https://fossil-scm.org/home/help/stash
- Fossil `branch`: https://www2.fossil-scm.org/home/help?cmd=branch
- Fossil JSON API index: https://fossil-scm.org/home/doc/trunk/www/json-api/index.md
- Fossil JSON API introduction: https://www.fossil-scm.org/home/doc/js-policy-doc/www/json-api/intro.md
- libfossil overview: https://fossil-scm.org/libfossil/home
- libfossil doxygen overview: https://fossil.wanderinghorse.net/doxygen/libfossil/
- `heroforge-core` crate metadata: https://crates.io/api/v1/crates/heroforge-core
- `heroforge-core` docs: https://docs.rs/heroforge-core/latest/heroforge_core/
- `fslutils` crate metadata: https://crates.io/api/v1/crates/fslutils
