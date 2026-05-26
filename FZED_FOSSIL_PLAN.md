# fzed Fossil Integration Plan

Date: 2026-05-12

## Fork Status

- GitHub fork: https://github.com/pekeler/fzed
- Upstream: https://github.com/zed-industries/zed
- Local checkout: `/Users/pekeler/Projects/FZed`
- Upstream tracking policy: follow upstream release tags, not upstream `main`
- Current upstream baseline: `v1.3.7`
- Remotes:
  - `origin` -> `https://github.com/pekeler/fzed.git`
  - `upstream` -> `https://github.com/zed-industries/zed.git`

## Fork Runtime Safety

- FZed must not use Zed's binary auto-update channel or replace itself with upstream Zed.
- Until FZed publishes official binary releases, runtime binary update polling is disabled for every release channel.
- Manual "Check for Updates" should explain that FZed binary updates are not available yet and can open `https://github.com/pekeler/fzed/releases`.
- Release-note links should point at FZed GitHub pages, not Zed Cloud or `zed-industries/zed`.
- Remote-server binary downloads from Zed Cloud are disabled until FZed publishes matching remote server artifacts or implements a FZed-specific remote-server distribution path.
- Future optional task: replace the disabled binary updater with a FZed GitHub release check that never downloads or installs binaries. If it detects a newer official FZed release, the action should open the GitHub release page in the browser.

## Local Tooling Baseline

Installed or verified on 2026-05-12:

- Rust toolchain: `rustc 1.95.0`, matching `rust-toolchain.toml`
- Cargo: `cargo 1.95.0`
- Rust components: `rustfmt`, `clippy`, `rust-analyzer`, `rust-src`
- Rust targets: `aarch64-apple-darwin`, `wasm32-wasip2`, `wasm32-unknown-unknown`, `x86_64-unknown-linux-musl`
- Fossil: `2.27 [99675884a9] 2025-09-30`
- Xcode: `26.5`, selected at `/Applications/Xcode.app/Contents/Developer`
- Metal compiler: working. On macOS 26/Xcode 26.5, `xcrun -sdk macosx metal` initially failed even after downloading the Metal Toolchain. Running Xcode first-launch setup, exporting the Metal Toolchain, deleting the stale installed component, and importing the exported bundle repaired Xcode's component registration.
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
- Phase 3 branch/sync/checkout slice compiles:
  - `cargo check -p git -p fs -p project -p git_ui -p proto`
  - `cargo check -p collab`
  - `cargo test -p git fossil`
  - `cargo test -p project test_fossil_repository`
  - `cargo test -p project fossil_included_paths`
  - `cargo test -p fs fake_git_repo`
  - `cargo test -p worktree --features test-support fossil_repository_detection`
  - `cargo test -p proto split_repository_update`
  - `git diff --check`
- Phase 4 history/stash/blame slice compiles:
  - `cargo check -p git -p project -p git_ui -p proto`
  - `cargo check -p collab`
  - `cargo test -p git fossil`
  - `cargo test -p project test_fossil_repository`
  - `cargo test -p fs fake_git_repo`
  - `cargo test -p worktree --features test-support fossil_repository_detection`
  - `cargo test -p proto split_repository_update`
  - `git diff --check`
- Phase 6 command alias/scope slice compiles:
  - `cargo test -p git fossil --lib`
  - `cargo test -p project fossil --lib`
  - `cargo test -p git_ui fossil --lib`
  - `cargo test -p worktree --features test-support fossil_repository_detection`
  - `cargo test -p proto split_repository_update`
  - `cargo build -p zed`
  - Debug binary: `/Users/pekeler/Projects/FZed/target/debug/fzed`

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
- Fossil is commonly used as a purely local repository, without a central host. fzed must not assume that sync targets or GitHub-style hosting exist.
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
- Fossil include/exclude state is shared through repository updates in collaborative sessions, and remote participant actions are forwarded to the host through the existing stage/unstage request path. This matches Git's shared staging behavior without creating a Fossil index.
- Fossil check-ins clear the include/exclude state after a successful check-in.
- Git-only amend/signoff menu entries are hidden for Fossil repositories.

Phase 2 is implemented for file-level selection and check-in.

Phase 2.1 coverage status:

- Added project integration tests for Fossil repository include/exclude state, including single-file include, include-all, single-file exclude, and exclude-all.
- Added a project integration test proving a Fossil check-in only moves selected paths into the fake repository HEAD.
- Extended the fake repository backend so `.fslckout` and `_FOSSIL_` fixtures report `RepositoryKind::Fossil` and can simulate `commit_paths`.
- Re-ran focused Fossil, project, fs fake-git, and `git_ui` compile checks.

Known Phase 2 deferrals:

- Autosync visibility belongs in Phase 3 with Fossil sync/update UX. The Phase 3 slice now surfaces autosync/default remote metadata in check-in command tooltips.
- The include list remains in-memory and intentionally non-persistent across restarts, like a transient check-in selection.
- Direct `git_ui` panel tests for Fossil labels and toolbar button states are deferred to Phase 5; current coverage is at the UI-facing repository state layer.
- Hunk-level selection is out of scope for fzed's Fossil integration because Fossil has no native hunk commit mechanism.

## Resolved Or Deferred TODOs

- Real collab database migration: this checkout has schema snapshots, not a forward migration chain; `repository_kind` is already in the table model and both schema snapshots.
- Autosync state/default remote visibility: implemented in Phase 3. Richer last-sync diagnostics remain a later polish item.
- Remaining visible Git naming in Fossil views: Phase 5, then broader internal naming in Phase 6.
- Direct `git_ui` Fossil panel tests: Phase 5.
- Shared selected Fossil paths: implemented. This matches Git's shared staging behavior and does not conflict with Fossil, because it is editor session state that becomes Fossil-native selected file arguments at check-in time.
- Fossil tickets/wiki/forum/notes/chat are out of scope for the SCM integration unless a comparable built-in Zed surface appears for GitHub issues/wiki-style workflows.
- `fossil ui` command: out of scope. Fossil's web UI remains available from a terminal, but fzed will not manage hidden `fossil ui` server processes.

### Phase 3: Fossil-First Branch, Sync, and Checkout UX

- Treat branch names as shared repository state, not local labels.
- Prefer Fossil's create-branch-at-commit workflow:
  - normal branch creation can be offered from the commit/check-in UI via `fossil commit --branch NAME`
  - `fossil branch new` can remain an advanced action
- Model sync as Fossil does:
  - `fossil sync` is all-project synchronization
  - `fossil pull` does not update the checkout by itself
  - `fossil update` is the normal "bring this checkout forward" operation
- Surface autosync state, sync target, and last-sync/check-in diagnostics in the check-in UI.
- Add explicit UI for multiple checkouts from one repository:
  - show repository database path separately from checkout path
  - support opening sibling checkouts
  - make branch switching via a different checkout feel normal, not exotic
- Show "test before check-in" affordances where useful, such as running configured tasks from the check-in panel before committing.

Expected result: fzed behaves like a Fossil client rather than a Git client using Fossil commands.

Initial implementation status:

- `fossil sync` is wired through the existing remote-operation path used by the panel, with Fossil-specific success/error text and a "Sync" button in Fossil repositories.
- `fossil update` is wired as the checkout-forward action, separate from sync, with a Fossil "Update" button.
- Fossil branch switching uses `fossil update BRANCH`, so changing branches keeps Fossil's working-checkout semantics.
- Advanced branch creation uses `fossil branch new NAME BASIS` followed by `fossil update NAME`. This keeps the existing branch picker usable while the preferred create-branch-at-check-in workflow remains a later UI refinement.
- Fossil sync metadata is captured in repository snapshots and shared through collaboration updates:
  - autosync setting value
  - default remote URL
  - repository database path
- Check-in tooltips include autosync/default-remote metadata when available, so users can see whether check-in may also sync.
- Fossil sibling checkouts are listed via `fossil info --verbose REPOSITORY`; creating another checkout uses `fossil open REPOSITORY VERSION --workdir PATH`.
- Repository snapshots are refreshed after Fossil sync/update/branch/checkout commands so branch, head, checkout list, and sync metadata stay current.

Phase 3 is implemented for backend behavior and panel-level sync/update UX.

Known Phase 3 deferrals:

- The preferred `fossil commit --branch NAME` create-branch-at-check-in UI is deferred to Phase 5 polish. The advanced `fossil branch new` path works now.
- The existing worktree picker can create/open Fossil sibling checkouts through the backend, but its copy still says "worktree" in several places. Fossil-specific checkout wording belongs in Phase 5.
- "Test before check-in" task affordances need task-runner UX work and are deferred until the check-in panel polish pass.

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

Initial implementation status:

- Fossil stash list/show/apply/pop/drop are wired through the existing stash surfaces.
- Fossil stash entries keep Fossil's checkout-local stash IDs; the commit/stash diff view now matches entries by ID instead of assuming IDs are vector offsets.
- Fossil stash diffs and check-in diffs are parsed from full-context Fossil unified diffs into Zed's existing commit diff model.
- Fossil commit details are loaded from `fossil info`, including user, timestamp, comment, parents, and tags where available.
- Fossil blame is parsed from `fossil blame` output and mapped into Zed's existing blame model.
- Fossil timeline/search support is backed by `fossil timeline --type ci` with branch/path/source filtering where Fossil maps cleanly.
- Fossil commit graph data and commit data streaming are implemented without requiring Git object storage.
- Fossil checkout info is refreshed on each read so head/branch metadata does not go stale after check-ins, updates, or stash operations.
- Conflict states from Phase 1 continue to reuse Zed's existing conflict marker surface for files reported by Fossil as conflicted.

Phase 4 is implemented for existing Zed history, stash, commit-diff, search, and blame entry points.

Known Phase 4 deferrals:

- Fossil-managed tickets, wiki, forum, notes, chat, and non-SCM timeline items are intentionally out of scope unless fzed later grows a comparable hosted-project surface.
- Merge/cherrypick/backout do not have dedicated Fossil UI yet. They should be exposed later as working-checkout changes followed by explicit check-in.
- Fossil stash does not include unmanaged extra files. fzed follows Fossil here instead of adding extras behind the user's back.
- Blame currently reflects Fossil's checked-out file state; unsaved editor-buffer blame would require a temporary checkout or another Fossil-specific overlay.
- Timeline ordering is Fossil-native reverse chronological for now; custom Git graph ordering modes are not fully emulated.

### Phase 5: Fossil UI Polish And Coverage

- Replace remaining user-facing Git labels in Fossil-specific views where the semantics diverge.
- Add direct `git_ui` panel tests for Fossil labels, include/exclude button states, check-in enablement, and project diff toolbar behavior.
- Verify collaborative include/exclude updates in a remote project fixture.
- Keep hunk controls hidden/disabled for Fossil, with labels that do not imply the feature is coming soon.

Expected result: Fossil users should not have to mentally translate Git terms for the file-level check-in workflow.

Phase 5 is implemented for the current file-level Fossil workflow:

- File context menus now say "Include File" / "Exclude File" for Fossil selections and no longer show `.gitignore` actions in Fossil repositories.
- Panel overflow actions now expose Fossil stash as "Stash Tracked", matching Fossil's behavior of not stashing unmanaged extra files.
- Fossil repositories can discard tracked checkout edits through native `fossil revert`, and the fake repository backend now supports checkout/revert operations for UI tests.
- Project diff hunk stage/unstage buttons are hidden for Fossil instead of showing disabled Git hunk-staging copy.
- Direct `git_ui` tests now cover Fossil include/exclude labels, stash policy, check-in button state, and project diff toolbar behavior.
- Project tests now cover Fossil selected-path propagation into remote repository updates, in addition to the existing proto update coverage.

### Phase 6: Generalize Names and UI

Once Fossil works:

- Rename internal `GitRepository`/`GitStore` boundaries to SCM-neutral names in small patches.
- Keep user-facing commands compatible by adding Fossil-specific command aliases, not by deleting existing Git actions.
- Update settings names carefully:
  - preserve existing Git settings
  - add `fossil.*` settings
  - consider future `version_control.*` shared settings only where semantics are genuinely shared

Phase 6 first slice is in progress:

- Fossil-visible history surfaces now use timeline/check-in wording in the panel tab, loading state, entry tooltip, file context menu, graph/timeline button, commit/check-in view toolbar, blame/commit tooltip hash copy actions, and "View Commit" modal.
- Commit message generation copy now switches to "check-in message" for Fossil repositories.
- Added first-class Fossil command aliases that route to existing Fossil-aware behavior while leaving Git actions intact:
  - check-in: `fossil::CheckIn`, `fossil::ToggleFillCheckInEditor`, `fossil::GenerateCheckInMessage`, `fossil::ViewCheckIn`
  - file selection: `fossil::IncludeFile`, `fossil::ExcludeFile`, `fossil::ToggleIncluded`, `fossil::IncludeRange`, `fossil::IncludeAll`, `fossil::ExcludeAll`
  - checkout maintenance: `fossil::RevertFile`, `fossil::RevertTrackedFiles`, `fossil::CleanExtras`
  - sync/history: `fossil::Sync`, `fossil::Update`, `fossil::Timeline`, `fossil::FileTimeline`, `fossil::Annotate`, `fossil::Blame`
  - navigation and repository state: `fossil::Branch`, `fossil::SwitchBranch`, `fossil::Checkouts`, `fossil::SelectRepo`, `fossil::OpenModifiedFiles`, `fossil::CopyBranchName`
  - stash: `fossil::StashTracked`, `fossil::PopStash`, `fossil::ApplyStash`, `fossil::ViewStash`
- Intentionally did not add Fossil aliases for Git hunk staging, amend, signoff, rebase, force-push, GitHub pull requests, `.gitignore`, or `fossil ui`. Those either conflict with Fossil's model or are out of scope for fzed's Fossil integration.
- Added focused unit tests for the Fossil/Git label split.

Remaining Phase 6 work should be split into small patches:

- Internal `GitRepository`/`GitStore` boundary renames are still deferred until the Fossil behavior stabilizes across more UI paths.
- Settings names still need an explicit audit before introducing `fossil.*` settings or any shared `version_control.*` settings.

### Explicitly Out Of Scope

- Hunk-level include/exclude or hunk-level check-in emulation. Fossil commits named files or all eligible changes; fzed should preserve that model rather than adding a synthetic index.
- Managed `fossil ui` process control. Fossil's web UI can still be launched outside fzed, but fzed will not own server lifetimes or port allocation.

## Main Risks

- Fossil rename/copy metadata may not map cleanly to Git-oriented status codes.
- Upstream Zed may keep changing `crates/git` rapidly, so broad renames should wait.
- Shelling out is simplest and safest initially, but performance should be measured on large Fossil checkouts.

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
