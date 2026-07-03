use crate::{
    Oid, RunHook,
    blame::Blame,
    repository::{
        Branch, BranchesScanResult, CommitData, CommitDataReader, CommitDetails, CommitDiff,
        CommitFile, CommitOptions, CommitSummary, CreateWorktreeTarget, DiffType, FetchOptions,
        FileHistoryChangedFileSets, FossilSyncState, GRAPH_CHUNK_SIZE, GitCommitTemplate,
        GitRepository, GitRepositoryCheckpoint, InitialGraphCommitData, LogOrder, LogSource,
        PushOptions, Remote, RemoteCommandOutput, RepoPath, RepositoryKind, ResetMode,
        SearchCommitArgs, Worktree,
    },
    stash::{GitStash, StashEntry},
    status::{
        DiffStat, DiffTreeType, FileStatus, GitDiffStat, GitStatus, StatusCode, StatusRename,
        TreeDiff, UnmergedStatus, UnmergedStatusCode,
    },
};
use anyhow::{Context as _, Result, anyhow};
use async_channel::Sender;
use collections::{HashMap, HashSet};
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{AsyncApp, BackgroundExecutor, SharedString, Task};
use parking_lot::Mutex;
use rope::Rope;
use smallvec::SmallVec;
use smol::lock::Mutex as AsyncMutex;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::ExitStatus,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};
use text::LineEnding;
use time::PrimitiveDateTime;
use util::{command::new_command, paths::PathStyle};

pub const FOSSIL_BINARY_NAME: &str = "fossil";
pub const FOSSIL_BINARY_NOT_FOUND_MESSAGE: &str = "Fossil executable not found. Install Fossil and make sure the `fossil` command is available on PATH.";

pub struct FossilRepository {
    checkout_db_path: PathBuf,
    work_directory: PathBuf,
    fossil_binary_path: PathBuf,
    executor: BackgroundExecutor,
    is_trusted: Arc<AtomicBool>,
    cached_info: Arc<Mutex<Option<FossilInfo>>>,
    cached_commit_data: Arc<Mutex<HashMap<Oid, CommitData>>>,
    command_lock: Arc<AsyncMutex<()>>,
    envs: Arc<HashMap<String, String>>,
}

impl FossilRepository {
    pub fn new(
        checkout_db_path: &Path,
        fossil_binary_path: Option<PathBuf>,
        executor: BackgroundExecutor,
    ) -> Result<Self> {
        let work_directory = checkout_db_path
            .parent()
            .context("Fossil checkout database has no parent directory")?
            .to_path_buf();
        let fossil_binary_path = match fossil_binary_path {
            Some(path) => path,
            None => resolve_fossil_binary(None, &work_directory)?,
        };
        log::info!(
            "opening Fossil repository at {checkout_db_path:?} using fossil binary {fossil_binary_path:?}"
        );
        Ok(Self {
            checkout_db_path: checkout_db_path.to_path_buf(),
            work_directory,
            fossil_binary_path,
            executor,
            is_trusted: Arc::new(AtomicBool::new(false)),
            cached_info: Arc::default(),
            cached_commit_data: Arc::default(),
            command_lock: Arc::default(),
            envs: Arc::default(),
        })
    }

    #[cfg(test)]
    fn new_for_test(
        checkout_db_path: &Path,
        fossil_binary_path: Option<PathBuf>,
        executor: BackgroundExecutor,
        envs: HashMap<String, String>,
    ) -> Result<Self> {
        let mut repository = Self::new(checkout_db_path, fossil_binary_path, executor)?;
        repository.envs = Arc::new(envs);
        Ok(repository)
    }

    fn fossil_binary(&self) -> FossilBinary {
        FossilBinary::new(
            self.fossil_binary_path.clone(),
            self.work_directory.clone(),
            self.command_lock.clone(),
            self.envs.clone(),
        )
    }

    async fn info(&self) -> Result<FossilInfo> {
        let output = self.fossil_binary().run(&["info"]).await?;
        let info = parse_fossil_info(&output);
        *self.cached_info.lock() = Some(info.clone());
        Ok(info)
    }

    fn unsupported<T: Send + 'static>(operation: &'static str) -> BoxFuture<'static, Result<T>> {
        async move { Err(anyhow!("Fossil backend does not support {operation} yet")) }.boxed()
    }
}

pub fn fossil_binary_not_found_error() -> anyhow::Error {
    anyhow!(FOSSIL_BINARY_NOT_FOUND_MESSAGE)
}

pub fn resolve_fossil_binary(
    search_paths: Option<&str>,
    working_directory: &Path,
) -> Result<PathBuf> {
    if let Some(path) = search_paths
        .filter(|paths| !paths.is_empty())
        .and_then(|search_paths| {
            which::which_in(FOSSIL_BINARY_NAME, Some(search_paths), working_directory).ok()
        })
    {
        return Ok(path);
    }

    if let Ok(path) = which::which(FOSSIL_BINARY_NAME) {
        return Ok(path);
    }

    #[cfg(target_os = "macos")]
    {
        for path in common_macos_fossil_paths() {
            if path.is_file() {
                return Ok(path);
            }
        }
    }

    Err(fossil_binary_not_found_error())
}

#[cfg(target_os = "macos")]
fn common_macos_fossil_paths() -> impl IntoIterator<Item = PathBuf> {
    [
        "/opt/homebrew/bin/fossil",
        "/usr/local/bin/fossil",
        "/opt/local/bin/fossil",
        "/sw/bin/fossil",
    ]
    .map(PathBuf::from)
}

impl GitRepository for FossilRepository {
    fn kind(&self) -> RepositoryKind {
        RepositoryKind::Fossil
    }

    fn load_index_text(&self, _path: RepoPath) -> BoxFuture<'_, Option<String>> {
        async move { None }.boxed()
    }

    fn load_committed_text(&self, path: RepoPath) -> BoxFuture<'_, Option<String>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                fossil
                    .run_raw(&[
                        OsString::from("cat"),
                        path.as_std_path().as_os_str().to_owned(),
                    ])
                    .await
                    .ok()
            })
            .boxed()
    }

    fn load_blob_content(&self, _oid: Oid) -> BoxFuture<'_, Result<String>> {
        Self::unsupported("loading blobs by Git object ID")
    }

    fn set_index_text(
        &self,
        _path: RepoPath,
        _content: Option<String>,
        _env: Arc<HashMap<String, String>>,
        _is_executable: bool,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("writing index entries")
    }

    fn remote_url(&self, name: &str) -> BoxFuture<'_, Option<String>> {
        let fossil = self.fossil_binary();
        let name = name.to_string();
        self.executor
            .spawn(async move {
                if name == "default" {
                    return Ok::<Option<String>, anyhow::Error>(parse_fossil_default_remote(
                        &fossil.run(&["remote"]).await?,
                    ));
                }

                let remotes = parse_fossil_remote_list(&fossil.run(&["remote", "list"]).await?);
                Ok::<Option<String>, anyhow::Error>(
                    remotes
                        .into_iter()
                        .find(|remote| remote.name == name)
                        .map(|remote| remote.url),
                )
            })
            .map(|result| result.ok().flatten())
            .boxed()
    }

    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let mut remote_urls = fossil
                    .run(&["remote", "list"])
                    .await
                    .map(|output| {
                        parse_fossil_remote_list(&output)
                            .into_iter()
                            .map(|remote| (remote.name, remote.url))
                            .collect::<HashMap<_, _>>()
                    })
                    .unwrap_or_default();

                if !remote_urls.contains_key("default")
                    && let Ok(output) = fossil.run(&["remote"]).await
                    && let Some(default_remote) = parse_fossil_default_remote(&output)
                {
                    remote_urls.insert("default".to_string(), default_remote);
                }

                remote_urls
            })
            .boxed()
    }

    fn fossil_sync_state(&self) -> BoxFuture<'_, Result<Option<FossilSyncState>>> {
        let this = self.clone_for_task();
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let info = this.info().await?;
                let autosync = fossil
                    .run(&["settings", "autosync", "--value"])
                    .await
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .map(SharedString::from);
                let default_remote = fossil
                    .run(&["remote"])
                    .await
                    .ok()
                    .and_then(|output| parse_fossil_default_remote(&output))
                    .map(SharedString::from);

                Ok(Some(FossilSyncState {
                    autosync,
                    default_remote,
                    repository: info
                        .repository
                        .map(|path| SharedString::from(path.to_string_lossy().into_owned())),
                }))
            })
            .boxed()
    }

    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        let this = self.clone_for_task();
        self.executor
            .spawn(async move {
                let info = this.info().await?;
                Ok(revs
                    .into_iter()
                    .map(|rev| match rev.as_str() {
                        "HEAD" | "checkout" | "current" => info.checkout.clone(),
                        _ => None,
                    })
                    .collect())
            })
            .boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        async move { None }.boxed()
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        let fossil = self.fossil_binary();
        let path_prefixes = path_prefixes.to_vec();
        self.executor.spawn(async move {
            let has_path_prefixes = path_prefixes.iter().any(|path| !path.is_empty());
            let output = fossil.run(&fossil_changes_args(path_prefixes)).await?;
            let mut changes = parse_fossil_changes_with_kind(&output);
            if has_path_prefixes
                && changes
                    .iter()
                    .any(|change| change.kind == FossilChangeKind::Extra)
            {
                let output = fossil.run(&fossil_changes_args(Vec::new())).await?;
                changes =
                    filter_scoped_fossil_extras(changes, parse_fossil_changes_with_kind(&output));
            }
            Ok(fossil_changes_to_status(changes))
        })
    }

    fn diff_tree(&self, _request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        async move {
            Ok(TreeDiff {
                entries: HashMap::default(),
            })
        }
        .boxed()
    }

    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let output = fossil.run_raw(&["stash", "list"]).await?;
                parse_fossil_stash_list(&output)
            })
            .boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let info = parse_fossil_info(&fossil.run(&["info"]).await?);
                let checkout_info = match info.checkout.as_deref() {
                    Some(checkout) => match fossil_commit_info(&fossil, checkout).await {
                        Ok(info) => Some(info),
                        Err(error) => {
                            log::warn!("failed to load Fossil check-in summary: {error:#}");
                            None
                        }
                    },
                    None => None,
                };
                let current = fossil
                    .run(&["branch", "current"])
                    .await
                    .ok()
                    .or_else(|| checkout_info.as_ref().and_then(|info| info.branch.clone()));
                let list = fossil
                    .run(&["branch", "list", "--all"])
                    .await
                    .unwrap_or_default();
                let current_commit = checkout_info.as_ref().map(fossil_commit_summary_from_info);
                let mut branches = Vec::new();
                for line in list.lines() {
                    if let Some(name) = parse_fossil_branch_list_line(line) {
                        let is_head = current.as_deref() == Some(name.as_str());
                        branches.push(Branch {
                            is_head,
                            ref_name: SharedString::from(format!("refs/heads/{name}")),
                            upstream: None,
                            most_recent_commit: is_head.then(|| current_commit.clone()).flatten(),
                        });
                    }
                }
                if branches.is_empty()
                    && let Some(current) = current
                {
                    branches.push(Branch {
                        is_head: true,
                        ref_name: SharedString::from(format!("refs/heads/{current}")),
                        upstream: None,
                        most_recent_commit: current_commit,
                    });
                }
                Ok(BranchesScanResult::from(branches))
            })
            .boxed()
    }

    fn change_branch(&self, name: String) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                fossil
                    .run(&[
                        OsString::from("update"),
                        OsString::from(fossil_branch_name(&name)),
                    ])
                    .await?;
                Ok(())
            })
            .boxed()
    }

    fn create_branch(
        &self,
        name: String,
        base_branch: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let name = fossil_branch_name(&name);
                let base_branch = base_branch
                    .as_deref()
                    .map(fossil_branch_name)
                    .unwrap_or_else(|| "current".to_string());
                fossil
                    .run(&[
                        OsString::from("branch"),
                        OsString::from("new"),
                        OsString::from(name.clone()),
                        OsString::from(base_branch),
                    ])
                    .await?;
                fossil
                    .run(&[OsString::from("update"), OsString::from(name)])
                    .await?;
                Ok(())
            })
            .boxed()
    }

    fn rename_branch(&self, _branch: String, _new_name: String) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("renaming branches")
    }

    fn delete_branch(
        &self,
        _is_remote: bool,
        _name: String,
        _force: bool,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("deleting branches")
    }

    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<Worktree>>> {
        let this = self.clone_for_task();
        self.executor
            .spawn(async move {
                let info = this.info().await?;
                let mut checkout_paths = if let Some(repository) = &info.repository {
                    parse_fossil_verbose_checkouts(
                        &this
                            .fossil_binary()
                            .run(&[
                                OsString::from("info"),
                                OsString::from("--verbose"),
                                repository.as_os_str().to_owned(),
                            ])
                            .await
                            .unwrap_or_default(),
                    )
                } else {
                    Vec::new()
                };

                if checkout_paths.is_empty() {
                    checkout_paths.push(this.work_directory.clone());
                }

                let current_work_directory = normalize_existing_path(this.work_directory.clone());
                let mut worktrees = Vec::new();
                for checkout_path in checkout_paths {
                    let checkout_path = normalize_existing_path(checkout_path);
                    let checkout_fossil = this
                        .fossil_binary()
                        .for_working_directory(checkout_path.clone());
                    let checkout_info = checkout_fossil
                        .run(&["info"])
                        .await
                        .ok()
                        .map(|output| parse_fossil_info(&output))
                        .unwrap_or_default();
                    let branch = checkout_fossil.run(&["branch", "current"]).await.ok();
                    let sha = checkout_info
                        .checkout
                        .or_else(|| info.checkout.clone())
                        .unwrap_or_default();

                    worktrees.push(Worktree {
                        path: checkout_path.clone(),
                        ref_name: branch
                            .map(|branch| SharedString::from(format!("refs/heads/{branch}"))),
                        sha: SharedString::from(sha),
                        is_main: checkout_path == current_work_directory,
                        is_bare: false,
                    });
                }

                worktrees.sort_by(|left, right| {
                    right
                        .is_main
                        .cmp(&left.is_main)
                        .then_with(|| left.path.cmp(&right.path))
                });
                worktrees.dedup_by(|left, right| left.path == right.path);
                Ok(worktrees)
            })
            .boxed()
    }

    fn worktree_created_at(
        &self,
        worktree_path: PathBuf,
    ) -> BoxFuture<'_, Result<Option<SystemTime>>> {
        self.executor
            .spawn(async move {
                let metadata = match std::fs::metadata(&worktree_path) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(None);
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to stat {}", worktree_path.display())
                        });
                    }
                };

                metadata.created().map(Some).with_context(|| {
                    format!("creation time unavailable for {}", worktree_path.display())
                })
            })
            .boxed()
    }

    fn create_worktree(
        &self,
        target: CreateWorktreeTarget,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        let this = self.clone_for_task();
        self.executor
            .spawn(async move {
                let info = this.info().await?;
                let repository = info
                    .repository
                    .context("Fossil repository path is unavailable")?;

                let version = match target {
                    CreateWorktreeTarget::ExistingBranch { branch_name } => {
                        Some(fossil_branch_name(&branch_name))
                    }
                    CreateWorktreeTarget::NewBranch {
                        branch_name,
                        base_sha,
                    } => {
                        let branch_name = fossil_branch_name(&branch_name);
                        let basis = base_sha.unwrap_or_else(|| "current".to_string());
                        this.fossil_binary()
                            .run(&[
                                OsString::from("branch"),
                                OsString::from("new"),
                                OsString::from(branch_name.clone()),
                                OsString::from(basis),
                            ])
                            .await?;
                        Some(branch_name)
                    }
                    CreateWorktreeTarget::Detached { base_sha } => base_sha,
                };

                let mut args = vec![OsString::from("open"), repository.as_os_str().to_owned()];
                if let Some(version) = version {
                    args.push(OsString::from(version));
                }
                args.push(OsString::from("--workdir"));
                args.push(path.as_os_str().to_owned());
                this.fossil_binary().run(&args).await?;
                Ok(())
            })
            .boxed()
    }

    fn checkout_branch_in_worktree(
        &self,
        branch_name: String,
        worktree_path: PathBuf,
        create: bool,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary().for_working_directory(worktree_path);
        self.executor
            .spawn(async move {
                let branch_name = fossil_branch_name(&branch_name);
                if create {
                    fossil
                        .run(&[
                            OsString::from("branch"),
                            OsString::from("new"),
                            OsString::from(branch_name.clone()),
                            OsString::from("current"),
                        ])
                        .await?;
                }
                fossil
                    .run(&[OsString::from("update"), OsString::from(branch_name)])
                    .await?;
                Ok(())
            })
            .boxed()
    }

    fn remove_worktree(&self, path: PathBuf, force: bool) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary().for_working_directory(path.clone());
        self.executor
            .spawn(async move {
                if !path.exists() && force {
                    return Ok(());
                }
                let mut args = vec![OsString::from("close")];
                if force {
                    args.push(OsString::from("--force"));
                }
                fossil.run(&args).await?;
                Ok(())
            })
            .boxed()
    }

    fn rename_worktree(&self, _old_path: PathBuf, _new_path: PathBuf) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("renaming worktrees")
    }

    fn reset(
        &self,
        _commit: String,
        _mode: ResetMode,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("reset")
    }

    fn checkout_files(
        &self,
        commit: String,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                if paths.is_empty() {
                    return Ok(());
                }

                let mut args = vec![OsString::from("revert")];
                if commit != "HEAD" {
                    args.push(OsString::from("--revision"));
                    args.push(OsString::from(commit));
                }
                args.extend(repo_paths_to_args(paths));
                fossil.run_with_env(&args, env).await?;
                Ok(())
            })
            .boxed()
    }

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        let fossil = self.fossil_binary();
        let cached_commit_data = self.cached_commit_data.clone();
        self.executor
            .spawn(async move {
                if let Some(stash_id) = fossil_stash_id_from_oid(&commit) {
                    let stash =
                        parse_fossil_stash_list(&fossil.run_raw(&["stash", "list"]).await?)?
                            .entries
                            .iter()
                            .find(|entry| entry.index == stash_id)
                            .cloned();

                    return Ok(CommitDetails {
                        sha: SharedString::from(commit),
                        message: SharedString::from(
                            stash
                                .as_ref()
                                .map(|entry| entry.message.clone())
                                .unwrap_or_else(|| format!("Fossil stash {stash_id}")),
                        ),
                        commit_timestamp: stash
                            .as_ref()
                            .map(|entry| entry.timestamp)
                            .unwrap_or_default(),
                        author_name: SharedString::from("Fossil stash"),
                        ..Default::default()
                    });
                }

                if let Some(commit_data) = cached_fossil_commit_data(&cached_commit_data, &commit) {
                    return Ok(CommitDetails {
                        sha: SharedString::from(commit_data.sha.to_string()),
                        message: commit_data.message,
                        commit_timestamp: commit_data.commit_timestamp,
                        author_name: commit_data.author_name,
                        author_email: commit_data.author_email,
                    });
                }

                let commit = if matches!(commit.as_str(), "HEAD" | "checkout" | "current") {
                    parse_fossil_info(&fossil.run(&["info"]).await?)
                        .checkout
                        .context("Fossil checkout hash is unavailable")?
                } else {
                    commit
                };
                let info = fossil_commit_info(&fossil, &commit).await?;
                Ok(CommitDetails {
                    sha: SharedString::from(info.hash),
                    message: SharedString::from(info.comment),
                    commit_timestamp: info.timestamp,
                    author_name: SharedString::from(info.user.unwrap_or_default()),
                    ..Default::default()
                })
            })
            .boxed()
    }

    fn load_commit(&self, commit: String, _cx: AsyncApp) -> BoxFuture<'_, Result<CommitDiff>> {
        let fossil = self.fossil_binary();
        let cached_commit_data = self.cached_commit_data.clone();
        self.executor
            .spawn(async move {
                let output = if let Some(stash_id) = fossil_stash_id_from_oid(&commit) {
                    fossil
                        .run_raw(&[
                            OsString::from("stash"),
                            OsString::from("show"),
                            OsString::from(stash_id.to_string()),
                            OsString::from("--verbose"),
                            OsString::from("--context"),
                            OsString::from("-1"),
                            OsString::from("--internal"),
                            OsString::from("--unified"),
                        ])
                        .await?
                } else {
                    let (parent, commit) = if let Some(commit_data) =
                        cached_fossil_commit_data(&cached_commit_data, &commit)
                    {
                        let Some(parent) = commit_data.parents.first().cloned() else {
                            return Ok(CommitDiff {
                                files: Vec::new(),
                                stats: Some((0, 0)),
                            });
                        };
                        (parent.to_string(), commit_data.sha.to_string())
                    } else {
                        let info = fossil_commit_info(&fossil, &commit).await?;
                        let Some(parent) = info.parents.first().cloned() else {
                            return Ok(CommitDiff {
                                files: Vec::new(),
                                stats: Some((0, 0)),
                            });
                        };
                        (parent, info.hash)
                    };
                    fossil
                        .run_raw(&[
                            OsString::from("diff"),
                            OsString::from("--from"),
                            OsString::from(parent),
                            OsString::from("--to"),
                            OsString::from(commit),
                            OsString::from("--verbose"),
                            OsString::from("--context"),
                            OsString::from("-1"),
                            OsString::from("--internal"),
                            OsString::from("--unified"),
                        ])
                        .await?
                };

                parse_fossil_unified_diff(&output)
            })
            .boxed()
    }

    fn blame(
        &self,
        path: RepoPath,
        _content: Rope,
        _line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<Blame>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let output = fossil
                    .run_raw(&[
                        OsString::from("blame"),
                        path.as_std_path().as_os_str().to_owned(),
                    ])
                    .await?;
                fossil_blame_from_output(&fossil, &path, &output).await
            })
            .boxed()
    }

    fn path(&self) -> PathBuf {
        self.checkout_db_path.clone()
    }

    fn main_repository_path(&self) -> PathBuf {
        self.checkout_db_path.clone()
    }

    fn stage_paths(
        &self,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("Git-style staging")
    }

    fn unstage_paths(
        &self,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("Git-style unstaging")
    }

    fn run_hook(
        &self,
        _hook: RunHook,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async move { Ok(()) }.boxed()
    }

    fn commit(
        &self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        askpass: askpass::AskPassDelegate,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        self.commit_paths(message, name_and_email, options, askpass, env, Vec::new())
    }

    fn commit_paths(
        &self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        _askpass: askpass::AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        paths: Vec<RepoPath>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                anyhow::ensure!(!options.amend, "Fossil check-ins do not support amend yet");
                anyhow::ensure!(
                    !options.signoff,
                    "Fossil check-ins do not support signoff yet"
                );

                let changes_output = fossil
                    .run_with_env(&fossil_changes_args(paths.clone()), env.clone())
                    .await?;
                let changes = parse_fossil_changes_with_kind(&changes_output);
                let selected_subset = !paths.is_empty();
                let mut extras = Vec::new();
                let mut missing = Vec::new();

                for change in changes {
                    match change.kind {
                        FossilChangeKind::Extra if selected_subset => {
                            extras.push(change.repo_path);
                        }
                        FossilChangeKind::Missing => {
                            missing.push(change.repo_path);
                        }
                        FossilChangeKind::Conflict => {
                            anyhow::bail!(
                                "Cannot check in unresolved Fossil conflict at {}",
                                change.repo_path.as_unix_str()
                            );
                        }
                        _ => {}
                    }
                }

                if !extras.is_empty() {
                    let mut args = vec![
                        OsString::from("add"),
                        OsString::from("--force"),
                        OsString::from("--dotfiles"),
                    ];
                    args.extend(repo_paths_to_args(extras));
                    fossil.run_with_env(&args, env.clone()).await?;
                }

                if !missing.is_empty() {
                    let mut args = vec![OsString::from("rm"), OsString::from("--soft")];
                    args.extend(repo_paths_to_args(missing));
                    fossil.run_with_env(&args, env.clone()).await?;
                }

                let mut args = vec![
                    OsString::from("commit"),
                    OsString::from("--comment"),
                    OsString::from(message.to_string()),
                    OsString::from("--no-prompt"),
                    OsString::from("--no-warnings"),
                    OsString::from("--no-verify-comment"),
                ];

                if options.allow_empty {
                    args.push(OsString::from("--allow-empty"));
                }

                if let Some((name, _email)) = name_and_email {
                    args.push(OsString::from("--user-override"));
                    args.push(OsString::from(name.to_string()));
                }

                args.extend(repo_paths_to_args(paths));
                run_fossil_commit_with_legacy_comment_verification_fallback(&fossil, &args, env)
                    .await?;
                Ok(())
            })
            .boxed()
    }

    fn record_fossil_rename(
        &self,
        old_path: RepoPath,
        new_path: RepoPath,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                fossil
                    .run_with_env(
                        &[
                            OsString::from("rename"),
                            old_path.as_std_path().as_os_str().to_owned(),
                            new_path.as_std_path().as_os_str().to_owned(),
                        ],
                        env,
                    )
                    .await?;
                Ok(())
            })
            .boxed()
    }

    fn undo_fossil_rename(
        &self,
        new_path: RepoPath,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let changes_output = fossil
                    .run_with_env(&fossil_changes_args(Vec::new()), env.clone())
                    .await?;
                let old_path = parse_fossil_changes_with_kind(&changes_output)
                    .into_iter()
                    .find_map(|change| {
                        (change.repo_path == new_path)
                            .then_some(change.rename_source)
                            .flatten()
                    })
                    .with_context(|| {
                        format!(
                            "No recorded Fossil rename found for {}",
                            new_path.as_unix_str()
                        )
                    })?;

                fossil
                    .run_with_env(
                        &[
                            OsString::from("rename"),
                            new_path.as_std_path().as_os_str().to_owned(),
                            old_path.as_std_path().as_os_str().to_owned(),
                        ],
                        env,
                    )
                    .await?;
                Ok(())
            })
            .boxed()
    }

    fn stash_paths(
        &self,
        paths: Vec<RepoPath>,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let message = message
                    .filter(|message| !message.trim().is_empty())
                    .unwrap_or_else(|| "fzed stash".to_string());
                let mut args = vec![
                    OsString::from("stash"),
                    OsString::from("save"),
                    OsString::from("--comment"),
                    OsString::from(message),
                ];
                args.extend(repo_paths_to_args(paths));
                fossil.run_with_env(&args, env).await?;
                Ok(())
            })
            .boxed()
    }

    fn stash_pop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                if let Some(index) = index {
                    fossil
                        .run_with_env(
                            &[
                                OsString::from("stash"),
                                OsString::from("apply"),
                                OsString::from(index.to_string()),
                            ],
                            env.clone(),
                        )
                        .await?;
                    fossil
                        .run_with_env(
                            &[
                                OsString::from("stash"),
                                OsString::from("drop"),
                                OsString::from(index.to_string()),
                            ],
                            env,
                        )
                        .await?;
                } else {
                    fossil
                        .run_with_env(&[OsString::from("stash"), OsString::from("pop")], env)
                        .await?;
                }
                Ok(())
            })
            .boxed()
    }

    fn stash_apply(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let mut args = vec![OsString::from("stash"), OsString::from("apply")];
                if let Some(index) = index {
                    args.push(OsString::from(index.to_string()));
                }
                fossil.run_with_env(&args, env).await?;
                Ok(())
            })
            .boxed()
    }

    fn stash_drop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let mut args = vec![OsString::from("stash"), OsString::from("drop")];
                if let Some(index) = index {
                    args.push(OsString::from(index.to_string()));
                }
                fossil.run_with_env(&args, env).await?;
                Ok(())
            })
            .boxed()
    }

    fn push(
        &self,
        _branch_name: String,
        _remote_branch_name: String,
        upstream_name: String,
        options: Option<PushOptions>,
        _askpass: askpass::AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                anyhow::ensure!(
                    !matches!(options, Some(PushOptions::Force)),
                    "Fossil sync does not support force-push"
                );

                let args = fossil_sync_args(&fossil, Some(upstream_name)).await?;
                fossil.run_output_with_env(&args, env).await
            })
            .boxed()
    }

    fn pull(
        &self,
        branch_name: Option<String>,
        _upstream_name: String,
        rebase: bool,
        _askpass: askpass::AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                anyhow::ensure!(!rebase, "Fossil update does not support rebase");
                let mut args = vec![OsString::from("update")];
                if let Some(branch_name) = branch_name {
                    args.push(OsString::from(fossil_branch_name(&branch_name)));
                }
                fossil.run_output_with_env(&args, env).await
            })
            .boxed()
    }

    fn fetch(
        &self,
        fetch_options: FetchOptions,
        _askpass: askpass::AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let remote_name = match fetch_options {
                    FetchOptions::All => None,
                    FetchOptions::Remote(remote) => Some(remote.name.to_string()),
                };
                let args = fossil_sync_args(&fossil, remote_name).await?;
                fossil.run_output_with_env(&args, env).await
            })
            .boxed()
    }

    fn get_push_remote(&self, _branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move { Ok(default_fossil_remote(&fossil).await?) })
            .boxed()
    }

    fn get_branch_remote(&self, _branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move { Ok(default_fossil_remote(&fossil).await?) })
            .boxed()
    }

    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<Remote>>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let mut remotes = parse_fossil_remote_list(&fossil.run(&["remote", "list"]).await?)
                    .into_iter()
                    .map(|remote| Remote {
                        name: SharedString::from(remote.name),
                    })
                    .collect::<Vec<_>>();

                if remotes.is_empty()
                    && parse_fossil_default_remote(&fossil.run(&["remote"]).await?).is_some()
                {
                    remotes.push(Remote {
                        name: SharedString::from("default"),
                    });
                }

                Ok(remotes)
            })
            .boxed()
    }

    fn remove_remote(&self, _name: String) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("removing remotes")
    }

    fn create_remote(&self, _name: String, _url: String) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("creating remotes")
    }

    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>> {
        async move { Ok(Vec::new()) }.boxed()
    }

    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                match diff {
                    DiffType::HeadToWorktree => {
                        fossil.run_raw(&["diff", "--internal", "--unified"]).await
                    }
                    DiffType::HeadToIndex => Ok(String::new()),
                    DiffType::MergeBase { .. } => Err(anyhow!(
                        "Fossil backend does not support merge-base diffs yet"
                    )),
                }
            })
            .boxed()
    }

    fn diff_stat(&self, path_prefixes: &[RepoPath]) -> BoxFuture<'static, Result<GitDiffStat>> {
        let fossil = self.fossil_binary();
        let path_prefixes = path_prefixes.to_vec();
        self.executor
            .spawn(async move {
                let mut args = vec![OsString::from("diff"), OsString::from("--numstat")];
                for prefix in path_prefixes {
                    if !prefix.is_empty() {
                        args.push(prefix.as_std_path().as_os_str().to_owned());
                    }
                }
                let output = fossil.run(&args).await?;
                Ok(parse_fossil_numstat(&output))
            })
            .boxed()
    }

    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>> {
        Self::unsupported("checkpoints")
    }

    fn restore_checkpoint(
        &self,
        _checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("restoring checkpoints")
    }

    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>> {
        Self::unsupported("archive checkpoints")
    }

    fn restore_archive_checkpoint(
        &self,
        _staged_sha: String,
        _unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("restoring archive checkpoints")
    }

    fn compare_checkpoints(
        &self,
        _left: GitRepositoryCheckpoint,
        _right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>> {
        Self::unsupported("comparing checkpoints")
    }

    fn diff_checkpoints(
        &self,
        _base_checkpoint: GitRepositoryCheckpoint,
        _target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>> {
        Self::unsupported("diffing checkpoints")
    }

    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>> {
        async move { Ok(None) }.boxed()
    }

    fn default_branch(
        &self,
        _include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        async move { Ok(Some(SharedString::from("trunk"))) }.boxed()
    }

    fn initial_graph_data(
        &self,
        log_source: LogSource,
        log_order: LogOrder,
        request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        let cached_commit_data = self.cached_commit_data.clone();
        self.executor
            .spawn(async move {
                let _ = log_order;
                let output = fossil
                    .run_raw(&fossil_timeline_args(&log_source, GRAPH_CHUNK_SIZE))
                    .await?;
                let mut commits = Vec::new();
                for entry in parse_fossil_timeline_entries(&output) {
                    let info = fossil_commit_info(&fossil, &entry.hash).await?;
                    let ref_names = info
                        .tags
                        .iter()
                        .map(|tag| {
                            if tag == "trunk" || Some(tag.as_str()) == info.branch.as_deref() {
                                SharedString::from(format!("refs/heads/{tag}"))
                            } else {
                                SharedString::from(format!("tag: {tag}"))
                            }
                        })
                        .collect();
                    let commit_data = match fossil_commit_data_from_info(&entry.hash, info) {
                        Ok(commit_data) => commit_data,
                        Err(error) => {
                            log::warn!("skipping Fossil timeline entry: {error:#}");
                            continue;
                        }
                    };
                    let sha = commit_data.sha;
                    let parents = commit_data.parents.clone();
                    cached_commit_data.lock().insert(sha, commit_data);

                    commits.push(Arc::new(InitialGraphCommitData {
                        sha,
                        parents,
                        ref_names,
                    }));
                }

                if !commits.is_empty() {
                    request_tx.send(commits).await.ok();
                }

                Ok(())
            })
            .boxed()
    }

    fn search_commits(
        &self,
        log_source: LogSource,
        search_args: SearchCommitArgs,
        request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let output = fossil
                    .run_raw(&fossil_timeline_args(&log_source, 0))
                    .await?;
                let query = if search_args.case_sensitive {
                    search_args.query.to_string()
                } else {
                    search_args.query.to_lowercase()
                };

                for entry in parse_fossil_timeline_entries(&output) {
                    let haystack = format!(
                        "{} {} {} {} {} {}",
                        entry.hash,
                        entry.date,
                        entry.author,
                        entry.comment,
                        entry.branch,
                        entry.tags
                    );
                    let matches = if search_args.case_sensitive {
                        haystack.contains(&query)
                    } else {
                        haystack.to_lowercase().contains(&query)
                    };
                    if matches
                        && let Some(oid) = fossil_oid_from_hash(&entry.hash)
                        && request_tx.send(oid).await.is_err()
                    {
                        break;
                    }
                }

                Ok(())
            })
            .boxed()
    }

    fn file_history_changed_files(
        &self,
        _paths: Vec<RepoPath>,
        _commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>> {
        Self::unsupported("file history changed files")
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        let fossil = self.fossil_binary();
        let cached_commit_data = self.cached_commit_data.clone();
        Ok(CommitDataReader::from_resolver(
            self.executor.clone(),
            move |sha| {
                let fossil = fossil.clone();
                let cached_commit_data = cached_commit_data.clone();
                async move {
                    if let Some(commit_data) = cached_commit_data.lock().get(&sha).cloned() {
                        return Ok(commit_data);
                    }

                    let commit_data = fossil_commit_data(&fossil, &sha.to_string()).await?;
                    cached_commit_data
                        .lock()
                        .insert(commit_data.sha, commit_data.clone());
                    Ok(commit_data)
                }
            },
        ))
    }

    fn update_ref(&self, _ref_name: String, _commit: String) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("updating refs")
    }

    fn delete_ref(&self, _ref_name: String) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("deleting refs")
    }

    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("repairing worktrees")
    }

    fn set_trusted(&self, trusted: bool) {
        self.is_trusted.store(trusted, Ordering::Release);
    }

    fn is_trusted(&self) -> bool {
        self.is_trusted.load(Ordering::Acquire)
    }
}

impl FossilRepository {
    fn clone_for_task(&self) -> Self {
        Self {
            checkout_db_path: self.checkout_db_path.clone(),
            work_directory: self.work_directory.clone(),
            fossil_binary_path: self.fossil_binary_path.clone(),
            executor: self.executor.clone(),
            is_trusted: self.is_trusted.clone(),
            cached_info: self.cached_info.clone(),
            cached_commit_data: self.cached_commit_data.clone(),
            command_lock: self.command_lock.clone(),
            envs: self.envs.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FossilInfo {
    checkout: Option<String>,
    repository: Option<PathBuf>,
    local_root: Option<PathBuf>,
}

fn parse_fossil_info(output: &str) -> FossilInfo {
    let mut info = FossilInfo::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "checkout" => {
                info.checkout = value.split_whitespace().next().map(str::to_owned);
            }
            "repository" => {
                let value = value.trim();
                if !value.is_empty() {
                    info.repository = Some(PathBuf::from(value));
                }
            }
            "local-root" => {
                let value = value.trim();
                if !value.is_empty() {
                    info.local_root = Some(PathBuf::from(
                        value.trim_end_matches(std::path::MAIN_SEPARATOR),
                    ));
                }
            }
            _ => {}
        }
    }
    info
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FossilCommitInfo {
    hash: String,
    parents: Vec<String>,
    tags: Vec<String>,
    branch: Option<String>,
    comment: String,
    user: Option<String>,
    timestamp: i64,
}

async fn fossil_commit_info(fossil: &FossilBinary, commit: &str) -> Result<FossilCommitInfo> {
    let output = fossil.run_raw(&["info", commit]).await?;
    parse_fossil_commit_info(&output).with_context(|| format!("parsing Fossil info for {commit}"))
}

fn fossil_commit_summary_from_info(info: &FossilCommitInfo) -> CommitSummary {
    CommitSummary {
        sha: SharedString::from(info.hash.clone()),
        subject: SharedString::from(info.comment.lines().next().unwrap_or_default().to_string()),
        commit_timestamp: info.timestamp,
        author_name: SharedString::from(info.user.clone().unwrap_or_default()),
        has_parent: !info.parents.is_empty(),
    }
}

async fn fossil_commit_data(fossil: &FossilBinary, commit: &str) -> Result<CommitData> {
    let info = fossil_commit_info(fossil, commit).await?;
    fossil_commit_data_from_info(commit, info)
}

fn fossil_commit_data_from_info(commit: &str, info: FossilCommitInfo) -> Result<CommitData> {
    let sha = fossil_oid_from_hash(&info.hash).with_context(|| {
        format!("Fossil check-in hash cannot be represented as an OID: {commit}")
    })?;
    let parents = info
        .parents
        .iter()
        .filter_map(|parent| fossil_oid_from_hash(parent))
        .collect::<SmallVec<[_; 1]>>();
    let subject = info.comment.lines().next().unwrap_or_default().to_string();

    Ok(CommitData {
        sha,
        parents,
        author_name: SharedString::from(info.user.unwrap_or_default()),
        author_email: SharedString::default(),
        commit_timestamp: info.timestamp,
        subject: SharedString::from(subject),
        message: SharedString::from(info.comment),
    })
}

fn cached_fossil_commit_data(
    cache: &Mutex<HashMap<Oid, CommitData>>,
    commit: &str,
) -> Option<CommitData> {
    let oid = fossil_oid_from_hash(commit)?;
    cache.lock().get(&oid).cloned()
}

fn parse_fossil_commit_info(output: &str) -> Result<FossilCommitInfo> {
    let mut info = FossilCommitInfo::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "hash" | "checkout" => {
                let mut parts = value.split_whitespace();
                if let Some(hash) = parts.next() {
                    info.hash = hash.to_string();
                }
                let date = parts.collect::<Vec<_>>().join(" ");
                if let Some(timestamp) = parse_fossil_timestamp(&date) {
                    info.timestamp = timestamp;
                }
            }
            "parent" => {
                if let Some(parent) = value.split_whitespace().next() {
                    info.parents.push(parent.to_string());
                }
            }
            "tags" => {
                info.tags = value
                    .split(',')
                    .flat_map(|part| part.split_whitespace())
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .map(str::to_string)
                    .collect();
                info.branch = info
                    .tags
                    .iter()
                    .find(|tag| tag.as_str() != "sym-trunk")
                    .cloned();
            }
            "comment" => {
                let (comment, user) = parse_fossil_comment_user(value);
                info.comment = comment.to_string();
                info.user = user.map(str::to_string);
            }
            "user" => {
                if !value.is_empty() {
                    info.user = Some(value.to_string());
                }
            }
            _ => {}
        }
    }

    anyhow::ensure!(!info.hash.is_empty(), "missing Fossil check-in hash");
    Ok(info)
}

fn parse_fossil_comment_user(value: &str) -> (&str, Option<&str>) {
    if let Some((comment, user)) = value.rsplit_once(" (user: ")
        && let Some(user) = user.strip_suffix(')')
    {
        return (comment, Some(user));
    }
    (value, None)
}

fn parse_fossil_timestamp(value: &str) -> Option<i64> {
    let value = value.trim().trim_end_matches(" UTC").trim();
    if value.is_empty() {
        return None;
    }
    let format = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    PrimitiveDateTime::parse(value, &format)
        .ok()
        .map(|date_time| date_time.assume_utc().unix_timestamp())
}

fn fossil_oid_from_hash(hash: &str) -> Option<Oid> {
    let hash = hash.trim();
    if hash.len() >= 40 {
        Oid::from_str(&hash[..40]).ok()
    } else {
        Oid::from_str(hash).ok()
    }
}

const FOSSIL_STASH_OID_PREFIX: &str = "f0551100";

fn fossil_stash_oid(stash_id: usize) -> Result<Oid> {
    Oid::from_str(&format!("{FOSSIL_STASH_OID_PREFIX}{:032x}", stash_id))
}

fn fossil_stash_id_from_oid(oid: &str) -> Option<usize> {
    if oid.len() != 40 {
        return None;
    }
    let suffix = oid.strip_prefix(FOSSIL_STASH_OID_PREFIX)?;
    usize::from_str_radix(suffix, 16).ok()
}

fn parse_fossil_stash_list(output: &str) -> Result<GitStash> {
    let mut entries = Vec::new();
    let mut current: Option<StashEntry> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "empty stash" {
            continue;
        }

        if let Some(entry) = parse_fossil_stash_header(trimmed)? {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(entry);
        } else if let Some(entry) = current.as_mut() {
            if !entry.message.is_empty() {
                entry.message.push('\n');
            }
            entry.message.push_str(trimmed);
        }
    }

    if let Some(entry) = current {
        entries.push(entry);
    }

    Ok(GitStash {
        entries: entries.into(),
    })
}

fn parse_fossil_stash_header(line: &str) -> Result<Option<StashEntry>> {
    let Some((id, rest)) = line.split_once(':') else {
        return Ok(None);
    };
    let Ok(index) = id.trim().parse::<usize>() else {
        return Ok(None);
    };
    let Some((_, after_hash)) = rest.split_once(']') else {
        return Ok(None);
    };
    let timestamp = after_hash
        .trim()
        .strip_prefix("on ")
        .and_then(parse_fossil_timestamp)
        .unwrap_or_default();

    Ok(Some(StashEntry {
        index,
        oid: fossil_stash_oid(index)?,
        message: String::new(),
        branch: None,
        timestamp,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FossilDiffFileKind {
    Added,
    Deleted,
    Modified,
}

#[derive(Default)]
struct FossilDiffFile {
    path: Option<RepoPath>,
    old_text: String,
    new_text: String,
    old_is_null: bool,
    new_is_null: bool,
    in_hunk: bool,
}

fn parse_fossil_unified_diff(output: &str) -> Result<CommitDiff> {
    let mut files = Vec::new();
    let mut current: Option<FossilDiffFile> = None;
    let mut added = 0;
    let mut removed = 0;

    for line in output.lines() {
        if let Some((kind, path)) = parse_fossil_diff_status_line(line) {
            push_fossil_diff_file(&mut files, current.take());
            current = Some(FossilDiffFile {
                path: Some(path),
                old_is_null: kind == FossilDiffFileKind::Added,
                new_is_null: kind == FossilDiffFileKind::Deleted,
                ..Default::default()
            });
            continue;
        }

        if let Some(path) = line.strip_prefix("Index: ") {
            let file = current.get_or_insert_with(FossilDiffFile::default);
            file.path = RepoPath::from_std_path(Path::new(path.trim()), PathStyle::local()).ok();
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if let Some(old_path) = line.strip_prefix("--- ") {
            file.old_is_null = old_path.trim() == "/dev/null";
            continue;
        }
        if let Some(new_path) = line.strip_prefix("+++ ") {
            file.new_is_null = new_path.trim() == "/dev/null";
            continue;
        }
        if line.starts_with("@@ ") {
            file.in_hunk = true;
            continue;
        }
        if !file.in_hunk || line.starts_with('\\') {
            continue;
        }

        if let Some(rest) = line.strip_prefix(' ') {
            file.old_text.push_str(rest);
            file.old_text.push('\n');
            file.new_text.push_str(rest);
            file.new_text.push('\n');
        } else if let Some(rest) = line.strip_prefix('-') {
            removed += 1;
            file.old_text.push_str(rest);
            file.old_text.push('\n');
        } else if let Some(rest) = line.strip_prefix('+') {
            added += 1;
            file.new_text.push_str(rest);
            file.new_text.push('\n');
        }
    }

    push_fossil_diff_file(&mut files, current);
    Ok(CommitDiff {
        files,
        stats: Some((added, removed)),
    })
}

fn push_fossil_diff_file(files: &mut Vec<CommitFile>, file: Option<FossilDiffFile>) {
    let Some(file) = file else {
        return;
    };
    let Some(path) = file.path else {
        return;
    };

    files.push(CommitFile {
        path,
        old_text: (!file.old_is_null).then_some(file.old_text),
        new_text: (!file.new_is_null).then_some(file.new_text),
        is_binary: false,
    });
}

fn parse_fossil_diff_status_line(line: &str) -> Option<(FossilDiffFileKind, RepoPath)> {
    let (kind, path) = line.trim().split_once(char::is_whitespace)?;
    let kind = match kind {
        "ADDED" => FossilDiffFileKind::Added,
        "DELETED" | "DELETE" => FossilDiffFileKind::Deleted,
        "CHANGED" | "EDITED" | "UPDATED_BY_MERGE" | "UPDATED_BY_INTEGRATE" => {
            FossilDiffFileKind::Modified
        }
        _ => return None,
    };
    let path = path.trim();
    let path = RepoPath::from_std_path(Path::new(path), PathStyle::local()).ok()?;
    Some((kind, path))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FossilTimelineEntry {
    hash: String,
    date: String,
    author: String,
    comment: String,
    branch: String,
    tags: String,
}

fn fossil_timeline_args(log_source: &LogSource, limit: usize) -> Vec<OsString> {
    let mut args = vec![OsString::from("timeline")];
    if let LogSource::Sha(sha) = log_source {
        args.push(OsString::from("ancestors"));
        args.push(OsString::from(sha.to_string()));
    }
    args.extend([
        OsString::from("--type"),
        OsString::from("ci"),
        OsString::from("--limit"),
        OsString::from(limit.to_string()),
        OsString::from("--format"),
        OsString::from("%H\t%d\t%a\t%c\t%b\t%t"),
    ]);
    match log_source {
        LogSource::Branch(branch) => {
            args.push(OsString::from("--branch"));
            args.push(OsString::from(fossil_branch_name(branch.as_ref())));
        }
        LogSource::Path(path) => {
            args.push(OsString::from("--path"));
            args.push(path.as_std_path().as_os_str().to_owned());
        }
        LogSource::All | LogSource::Sha(_) => {}
    }
    args
}

fn parse_fossil_timeline_entries(output: &str) -> Vec<FossilTimelineEntry> {
    output
        .lines()
        .filter_map(|line| {
            let parts = line.split('\t').collect::<Vec<_>>();
            if parts.len() != 6 {
                return None;
            }
            Some(FossilTimelineEntry {
                hash: parts[0].to_string(),
                date: parts[1].to_string(),
                author: parts[2].to_string(),
                comment: parts[3].to_string(),
                branch: parts[4].to_string(),
                tags: parts[5].to_string(),
            })
        })
        .collect()
}

async fn fossil_blame_from_output(
    fossil: &FossilBinary,
    path: &RepoPath,
    output: &str,
) -> Result<Blame> {
    let mut raw_lines = Vec::new();
    for (line_ix, line) in output.lines().enumerate() {
        if let Some(raw_line) = parse_fossil_blame_line(line_ix as u32, line) {
            raw_lines.push(raw_line);
        }
    }

    let mut info_by_prefix = HashMap::default();
    for raw_line in &raw_lines {
        if !info_by_prefix.contains_key(raw_line.hash_prefix.as_str()) {
            info_by_prefix.insert(
                raw_line.hash_prefix.clone(),
                fossil_commit_info(fossil, &raw_line.hash_prefix).await?,
            );
        }
    }

    let mut entries: Vec<crate::blame::BlameEntry> = Vec::new();
    let mut messages = HashMap::default();
    for raw_line in raw_lines {
        let Some(info) = info_by_prefix.get(raw_line.hash_prefix.as_str()) else {
            continue;
        };
        let Some(sha) = fossil_oid_from_hash(&info.hash) else {
            continue;
        };
        messages.insert(sha, info.comment.clone());

        if let Some(entry) = entries.last_mut()
            && entry.sha == sha
            && entry.range.end == raw_line.line_number
        {
            entry.range.end += 1;
            continue;
        }

        entries.push(crate::blame::BlameEntry {
            sha,
            range: raw_line.line_number..raw_line.line_number + 1,
            original_line_number: raw_line.line_number + 1,
            author: info.user.clone().or(Some(raw_line.author)),
            author_mail: None,
            author_time: Some(info.timestamp),
            author_tz: Some("+0000".to_string()),
            committer_name: info.user.clone(),
            committer_email: None,
            committer_time: Some(info.timestamp),
            committer_tz: Some("+0000".to_string()),
            summary: Some(info.comment.lines().next().unwrap_or_default().to_string()),
            previous: None,
            filename: path.as_unix_str().to_string(),
        });
    }

    Ok(Blame { entries, messages })
}

#[derive(Clone, Debug)]
struct FossilBlameLine {
    hash_prefix: String,
    author: String,
    line_number: u32,
}

fn parse_fossil_blame_line(line_number: u32, line: &str) -> Option<FossilBlameLine> {
    let (hash_prefix, rest) = take_token(line.trim_start())?;
    let (_date, rest) = take_token(rest.trim_start())?;
    let (author, _content) = rest.split_once(':')?;
    Some(FossilBlameLine {
        hash_prefix: hash_prefix.to_string(),
        author: author.trim().to_string(),
        line_number,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FossilChange {
    repo_path: RepoPath,
    kind: FossilChangeKind,
    rename_source: Option<RepoPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FossilChangeKind {
    Extra,
    Conflict,
    Added,
    Deleted,
    Missing,
    Renamed,
    Modified,
}

impl FossilChangeKind {
    fn status(self) -> FileStatus {
        match self {
            Self::Extra => FileStatus::Untracked,
            Self::Conflict => FileStatus::Unmerged(UnmergedStatus {
                first_head: UnmergedStatusCode::Updated,
                second_head: UnmergedStatusCode::Updated,
            }),
            Self::Added => StatusCode::Added.worktree(),
            Self::Deleted | Self::Missing => StatusCode::Deleted.worktree(),
            Self::Renamed => StatusCode::Renamed.worktree(),
            Self::Modified => StatusCode::Modified.worktree(),
        }
    }
}

fn parse_fossil_changes_with_kind(output: &str) -> Vec<FossilChange> {
    let mut entries = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line == "(none)" {
            continue;
        }
        let Some((change_type, path)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Some((rename_source, repo_path)) = parse_fossil_change_path(path.trim()) else {
            continue;
        };
        let kind = match change_type {
            "EXTRA" => FossilChangeKind::Extra,
            "CONFLICT" => FossilChangeKind::Conflict,
            "ADDED" => FossilChangeKind::Added,
            "DELETED" => FossilChangeKind::Deleted,
            "MISSING" => FossilChangeKind::Missing,
            "RENAMED" => FossilChangeKind::Renamed,
            "EXECUTABLE"
            | "META"
            | "EDITED"
            | "MERGED"
            | "UPDATED_BY_MERGE"
            | "UPDATED_BY_INTEGRATE" => FossilChangeKind::Modified,
            _ => continue,
        };
        entries.push(FossilChange {
            repo_path,
            kind,
            rename_source,
        });
    }
    entries.sort_unstable_by(|left, right| left.repo_path.cmp(&right.repo_path));
    entries.dedup_by(|left, right| left.repo_path == right.repo_path);
    entries
}

fn parse_fossil_change_path(path: &str) -> Option<(Option<RepoPath>, RepoPath)> {
    if path.is_empty() {
        return None;
    }

    let (rename_source, path) = if let Some((source, target)) = path.rsplit_once(" -> ") {
        let source = RepoPath::from_std_path(Path::new(source.trim()), PathStyle::local()).ok()?;
        (Some(source), target.trim())
    } else {
        (None, path)
    };

    let repo_path = RepoPath::from_std_path(Path::new(path), PathStyle::local()).ok()?;
    Some((rename_source, repo_path))
}

fn fossil_changes_to_status(changes: Vec<FossilChange>) -> GitStatus {
    let renames = changes
        .iter()
        .filter_map(|change| {
            change.rename_source.as_ref().map(|source| StatusRename {
                source: source.clone(),
                target: change.repo_path.clone(),
            })
        })
        .collect::<Vec<_>>();

    GitStatus {
        entries: changes
            .into_iter()
            .map(|change| (change.repo_path, change.kind.status()))
            .collect::<Vec<_>>()
            .into(),
        renames: renames.into(),
    }
}

fn filter_scoped_fossil_extras(
    scoped_changes: Vec<FossilChange>,
    unscoped_changes: Vec<FossilChange>,
) -> Vec<FossilChange> {
    let visible_extras = unscoped_changes
        .into_iter()
        .filter_map(|change| (change.kind == FossilChangeKind::Extra).then_some(change.repo_path))
        .collect::<HashSet<_>>();

    scoped_changes
        .into_iter()
        .filter(|change| {
            change.kind != FossilChangeKind::Extra || visible_extras.contains(&change.repo_path)
        })
        .collect()
}

fn fossil_changes_args(path_prefixes: Vec<RepoPath>) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("changes"),
        OsString::from("--classify"),
        OsString::from("--differ"),
        OsString::from("--no-merge"),
        OsString::from("--rel-paths"),
    ];
    args.extend(repo_paths_to_args(
        path_prefixes
            .into_iter()
            .filter(|path| !path.is_empty())
            .collect(),
    ));
    args
}

fn repo_paths_to_args(paths: Vec<RepoPath>) -> impl Iterator<Item = OsString> {
    paths
        .into_iter()
        .map(|path| path.as_std_path().as_os_str().to_owned())
}

fn parse_fossil_numstat(output: &str) -> GitDiffStat {
    let mut entries = Vec::new();
    for line in output.lines() {
        let Some((added_str, rest)) = take_token(line.trim_start()) else {
            continue;
        };
        let Some((deleted_str, path)) = take_token(rest.trim_start()) else {
            continue;
        };
        let Ok(added) = added_str.parse::<u32>() else {
            continue;
        };
        let Ok(deleted) = deleted_str.parse::<u32>() else {
            continue;
        };
        let path = path.trim_start();
        if path.is_empty() || path.starts_with("TOTAL ") {
            continue;
        }
        let Ok(path) = RepoPath::from_std_path(Path::new(path), PathStyle::local()) else {
            continue;
        };
        entries.push((path, DiffStat { added, deleted }));
    }
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    entries.dedup_by(|(left, _), (right, _)| left == right);

    GitDiffStat {
        entries: entries.into(),
    }
}

fn take_token(input: &str) -> Option<(&str, &str)> {
    let index = input.find(char::is_whitespace)?;
    Some((&input[..index], &input[index..]))
}

fn parse_fossil_branch_list_line(line: &str) -> Option<String> {
    let name = line
        .trim()
        .trim_start_matches('*')
        .trim_start_matches('#')
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FossilRemote {
    name: String,
    url: String,
}

fn parse_fossil_remote_list(output: &str) -> Vec<FossilRemote> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (name, url) = line.split_once(char::is_whitespace)?;
            let url = url.trim();
            (!url.is_empty()).then(|| FossilRemote {
                name: name.to_string(),
                url: url.to_string(),
            })
        })
        .collect()
}

fn parse_fossil_default_remote(output: &str) -> Option<String> {
    let remote = output.trim();
    (!remote.is_empty() && remote != "off").then(|| remote.to_string())
}

fn parse_fossil_verbose_checkouts(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.trim() != "check-out" {
                return None;
            }
            let path = parse_fossil_verbose_checkout_path(value)?;
            Some(PathBuf::from(
                path.trim_end_matches(std::path::MAIN_SEPARATOR),
            ))
        })
        .collect()
}

fn parse_fossil_verbose_checkout_path(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let path = match value.rfind(char::is_whitespace) {
        Some(index) => {
            let (path, suffix) = value.split_at(index);
            if is_fossil_checkout_date(suffix.trim()) {
                path.trim_end()
            } else {
                value
            }
        }
        None => value,
    };
    (!path.is_empty()).then_some(path)
}

fn is_fossil_checkout_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..].iter().all(u8::is_ascii_digit)
}

fn fossil_branch_name(name: &str) -> String {
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

fn normalize_existing_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

async fn run_fossil_commit_with_legacy_comment_verification_fallback(
    fossil: &FossilBinary,
    args: &[OsString],
    env: Arc<HashMap<String, String>>,
) -> Result<()> {
    match fossil.run_with_env(args, env.clone()).await {
        Ok(_) => Ok(()),
        Err(error) if fossil_error_is_unsupported_no_verify_comment(&error) => {
            log::warn!(
                "Fossil binary does not support --no-verify-comment; retrying commit without it"
            );
            let fallback_args = args
                .iter()
                .filter(|arg| arg.as_os_str() != OsStr::new("--no-verify-comment"))
                .cloned()
                .collect::<Vec<_>>();
            fossil.run_with_env(&fallback_args, env).await?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn fossil_error_is_unsupported_no_verify_comment(error: &anyhow::Error) -> bool {
    let Some(command_error) = error.downcast_ref::<FossilBinaryCommandError>() else {
        return false;
    };
    fossil_output_reports_unsupported_no_verify_comment(
        &command_error.stdout,
        &command_error.stderr,
    )
}

fn fossil_output_reports_unsupported_no_verify_comment(stdout: &str, stderr: &str) -> bool {
    let reports_unrecognized_option = stdout.contains("unrecognized command-line option")
        || stderr.contains("unrecognized command-line option");
    let mentions_no_verify_comment =
        stdout.contains("--no-verify-comment") || stderr.contains("--no-verify-comment");
    reports_unrecognized_option && mentions_no_verify_comment
}

#[derive(Clone)]
struct FossilBinary {
    fossil_binary_path: PathBuf,
    working_directory: PathBuf,
    command_lock: Arc<AsyncMutex<()>>,
    envs: Arc<HashMap<String, String>>,
}

impl FossilBinary {
    const EXECUTABLE_FILE_BUSY_RETRY_ATTEMPTS: usize = 3;

    fn new(
        fossil_binary_path: PathBuf,
        working_directory: PathBuf,
        command_lock: Arc<AsyncMutex<()>>,
        envs: Arc<HashMap<String, String>>,
    ) -> Self {
        Self {
            fossil_binary_path,
            working_directory,
            command_lock,
            envs,
        }
    }

    async fn run<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut stdout = self.run_raw(args).await?;
        if stdout.chars().last() == Some('\n') {
            stdout.pop();
        }
        Ok(stdout)
    }

    async fn run_with_env<S>(&self, args: &[S], env: Arc<HashMap<String, String>>) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut stdout = self.run_raw_with_env(args, Some(env)).await?;
        if stdout.chars().last() == Some('\n') {
            stdout.pop();
        }
        Ok(stdout)
    }

    async fn run_raw<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        self.run_raw_with_env(args, None).await
    }

    async fn run_raw_with_env<S>(
        &self,
        args: &[S],
        env: Option<Arc<HashMap<String, String>>>,
    ) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        Ok(self.run_output_raw_with_env(args, env).await?.stdout)
    }

    async fn run_output_with_env<S>(
        &self,
        args: &[S],
        env: Arc<HashMap<String, String>>,
    ) -> Result<RemoteCommandOutput>
    where
        S: AsRef<OsStr>,
    {
        self.run_output_raw_with_env(args, Some(env)).await
    }

    async fn run_output_raw_with_env<S>(
        &self,
        args: &[S],
        env: Option<Arc<HashMap<String, String>>>,
    ) -> Result<RemoteCommandOutput>
    where
        S: AsRef<OsStr>,
    {
        let _lock = self.command_lock.lock().await;
        let output = self.run_command_output_with_retries(args, env).await?;
        anyhow::ensure!(
            output.status.success(),
            FossilBinaryCommandError {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                status: output.status,
            }
        );
        Ok(RemoteCommandOutput {
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        })
    }

    async fn run_command_output_with_retries<S>(
        &self,
        args: &[S],
        env: Option<Arc<HashMap<String, String>>>,
    ) -> std::io::Result<std::process::Output>
    where
        S: AsRef<OsStr>,
    {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut command = self.build_command(args);
            if let Some(env) = env.as_ref() {
                command.envs(env.iter());
            }

            match command.output().await {
                Ok(output) => return Ok(output),
                Err(error)
                    if executable_file_is_busy(&error)
                        && attempts < Self::EXECUTABLE_FILE_BUSY_RETRY_ATTEMPTS =>
                {
                    smol::future::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn build_command<S>(&self, args: &[S]) -> util::command::Command
    where
        S: AsRef<OsStr>,
    {
        let mut command = new_command(&self.fossil_binary_path);
        command.current_dir(&self.working_directory);
        command.envs(self.envs.iter());
        command.args(args);
        command
    }

    fn for_working_directory(&self, working_directory: PathBuf) -> Self {
        Self {
            fossil_binary_path: self.fossil_binary_path.clone(),
            working_directory,
            command_lock: self.command_lock.clone(),
            envs: self.envs.clone(),
        }
    }
}

fn executable_file_is_busy(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(26)
    }

    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

async fn default_fossil_remote(fossil: &FossilBinary) -> Result<Option<Remote>> {
    if parse_fossil_default_remote(&fossil.run(&["remote"]).await?).is_some() {
        Ok(Some(Remote {
            name: SharedString::from("default"),
        }))
    } else {
        Ok(
            parse_fossil_remote_list(&fossil.run(&["remote", "list"]).await?)
                .into_iter()
                .next()
                .map(|remote| Remote {
                    name: SharedString::from(remote.name),
                }),
        )
    }
}

async fn fossil_sync_args(
    fossil: &FossilBinary,
    remote_name: Option<String>,
) -> Result<Vec<OsString>> {
    let remote = match remote_name.as_deref() {
        Some(remote_name) if remote_name != "default" => Some(remote_name.to_string()),
        _ => match parse_fossil_default_remote(&fossil.run(&["remote"]).await?) {
            Some(remote) => Some(remote),
            None => parse_fossil_remote_list(&fossil.run(&["remote", "list"]).await?)
                .into_iter()
                .next()
                .map(|remote| remote.name),
        },
    };
    let remote = remote.context("No Fossil remote is configured")?;
    Ok(vec![OsString::from("sync"), OsString::from(remote)])
}

#[derive(thiserror::Error, Debug)]
#[error("Fossil command failed:\n{stdout}{stderr}\n")]
struct FossilBinaryCommandError {
    stdout: String,
    stderr: String,
    status: ExitStatus,
}

#[cfg(test)]
mod tests {
    use super::{
        FossilBinary, FossilRepository, filter_scoped_fossil_extras, fossil_changes_to_status,
        fossil_oid_from_hash, fossil_output_reports_unsupported_no_verify_comment,
        fossil_stash_id_from_oid, fossil_sync_args, parse_fossil_blame_line,
        parse_fossil_branch_list_line, parse_fossil_changes_with_kind, parse_fossil_commit_info,
        parse_fossil_default_remote, parse_fossil_info, parse_fossil_numstat,
        parse_fossil_remote_list, parse_fossil_stash_list, parse_fossil_timeline_entries,
        parse_fossil_unified_diff, parse_fossil_verbose_checkouts, resolve_fossil_binary,
        run_fossil_commit_with_legacy_comment_verification_fallback,
    };
    use crate::{
        repository::{
            AskPassDelegate, CommitOptions, CreateWorktreeTarget, GitRepository, LogOrder,
            LogSource, RepoPath, SearchCommitArgs,
        },
        status::{FileStatus, StatusCode},
    };
    use collections::HashMap;
    use gpui::TestAppContext;
    use std::{
        path::{Path, PathBuf},
        process::{Command, Output},
        sync::Arc,
    };
    use text::{LineEnding, Rope};

    #[cfg(unix)]
    fn write_executable_script(path: &Path, contents: &str) -> std::io::Result<()> {
        use std::{io::Write as _, os::unix::fs::PermissionsExt};

        let temp_path = path.with_extension("tmp");
        {
            let mut script = std::fs::File::create(&temp_path)?;
            script.write_all(contents.as_bytes())?;
            script.sync_all()?;
        }

        let mut permissions = std::fs::metadata(&temp_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&temp_path, permissions)?;
        std::fs::rename(temp_path, path)
    }

    #[cfg(unix)]
    #[test]
    fn resolves_fossil_binary_from_search_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fossil_path = temp_dir.path().join("fossil");
        write_executable_script(&fossil_path, "#!/bin/sh\nexit 0\n").unwrap();

        assert_eq!(
            resolve_fossil_binary(temp_dir.path().to_str(), temp_dir.path()).unwrap(),
            fossil_path
        );
    }

    #[test]
    fn parses_fossil_info_checkout() {
        let info = parse_fossil_info(
            "project-name: demo\nrepository:   /tmp/repo.fossil\nlocal-root:   /tmp/checkout/\ncheckout:     abc123 2026-05-12 10:00:00 UTC\n",
        );
        assert_eq!(info.checkout.as_deref(), Some("abc123"));
        assert_eq!(
            info.repository.as_deref(),
            Some(Path::new("/tmp/repo.fossil"))
        );
        assert_eq!(info.local_root.as_deref(), Some(Path::new("/tmp/checkout")));
    }

    #[test]
    fn parses_fossil_changes() {
        let status = fossil_changes_to_status(parse_fossil_changes_with_kind(
            "EDITED src/main.rs\nADDED new file.txt\nDELETED old.rs\nEXTRA scratch.txt\nCONFLICT both.rs\n",
        ));

        let lookup = |path: &str| {
            status
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new(path).unwrap())
                .map(|(_, status)| *status)
        };

        assert_eq!(lookup("src/main.rs"), Some(StatusCode::Modified.worktree()));
        assert_eq!(lookup("new file.txt"), Some(StatusCode::Added.worktree()));
        assert_eq!(lookup("old.rs"), Some(StatusCode::Deleted.worktree()));
        assert_eq!(lookup("scratch.txt"), Some(FileStatus::Untracked));
        assert!(lookup("both.rs").unwrap().is_conflicted());
    }

    #[test]
    fn parses_fossil_renamed_changes_with_target_path() {
        let status = fossil_changes_to_status(parse_fossil_changes_with_kind(
            "RENAMED old.rs  ->  new.rs\nEDITED src/old name.rs  ->  src/new name.rs\n",
        ));

        let lookup = |path: &str| {
            status
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new(path).unwrap())
                .map(|(_, status)| *status)
        };

        assert_eq!(lookup("old.rs"), None);
        assert_eq!(lookup("new.rs"), Some(StatusCode::Renamed.worktree()));
        assert_eq!(
            lookup("src/new name.rs"),
            Some(StatusCode::Modified.worktree())
        );
        assert_eq!(
            status.renames.as_ref(),
            [
                crate::status::StatusRename {
                    source: RepoPath::new("old.rs").unwrap(),
                    target: RepoPath::new("new.rs").unwrap(),
                },
                crate::status::StatusRename {
                    source: RepoPath::new("src/old name.rs").unwrap(),
                    target: RepoPath::new("src/new name.rs").unwrap(),
                },
            ]
        );
    }

    #[test]
    fn filters_ignored_fossil_extras_from_scoped_status() {
        let scoped_changes = parse_fossil_changes_with_kind(
            "EXTRA ignored.txt\nEXTRA scratch.txt\nEDITED tracked.txt\n",
        );
        let unscoped_changes =
            parse_fossil_changes_with_kind("EXTRA scratch.txt\nEDITED tracked.txt\n");
        let status = fossil_changes_to_status(filter_scoped_fossil_extras(
            scoped_changes,
            unscoped_changes,
        ));

        let lookup = |path: &str| {
            status
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new(path).unwrap())
                .map(|(_, status)| *status)
        };

        assert_eq!(lookup("ignored.txt"), None);
        assert_eq!(lookup("scratch.txt"), Some(FileStatus::Untracked));
        assert_eq!(lookup("tracked.txt"), Some(StatusCode::Modified.worktree()));
    }

    #[test]
    fn parses_fossil_numstat() {
        let stats = parse_fossil_numstat(
            "  INSERTED    DELETED\n         1          2 tracked.txt\n         3          4 dir/file with spaces.txt\n         4          6 TOTAL over 2 changed files\n",
        );

        let lookup = |path: &str| {
            stats
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new(path).unwrap())
                .map(|(_, stat)| *stat)
        };

        assert_eq!(lookup("tracked.txt").unwrap().added, 1);
        assert_eq!(lookup("tracked.txt").unwrap().deleted, 2);
        assert_eq!(lookup("dir/file with spaces.txt").unwrap().added, 3);
        assert_eq!(lookup("dir/file with spaces.txt").unwrap().deleted, 4);
        assert_eq!(stats.entries.len(), 2);
    }

    #[test]
    fn parses_fossil_branch_lines() {
        assert_eq!(
            parse_fossil_branch_list_line("* trunk").as_deref(),
            Some("trunk")
        );
        assert_eq!(
            parse_fossil_branch_list_line("  # private").as_deref(),
            Some("private")
        );
        assert_eq!(parse_fossil_branch_list_line("   "), None);
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn fossil_branches_fall_back_to_checkout_tags(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let checkout = temp_dir.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(checkout.join(".fslckout"), "").unwrap();

        let script_path = temp_dir.path().join("fossil");
        write_executable_script(
            &script_path,
            "#!/bin/sh\n\
             checkout='1234567890abcdef1234567890abcdef12345678'\n\
             if [ \"$1\" = info ] && [ \"$#\" -eq 1 ]; then\n\
               printf 'checkout:     %s 2026-05-13 06:44:31 UTC\\n' \"$checkout\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = info ] && [ \"$2\" = \"$checkout\" ]; then\n\
               printf 'hash:         %s 2026-05-13 06:44:31 UTC\\n' \"$checkout\"\n\
               printf 'tags:         sym-trunk, feature\\n'\n\
               printf 'comment:      from tags (user: tester)\\n'\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = branch ] && [ \"$2\" = current ]; then\n\
               exit 1\n\
             fi\n\
             if [ \"$1\" = branch ] && [ \"$2\" = list ]; then\n\
               exit 0\n\
             fi\n\
             exit 1\n",
        )
        .unwrap();

        let repository = FossilRepository::new_for_test(
            &checkout.join(".fslckout"),
            Some(script_path),
            cx.executor(),
            HashMap::default(),
        )
        .unwrap();

        let branches = repository.branches().await.unwrap().branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].name(), "feature");
        assert!(branches[0].is_head);
        let commit = branches[0].most_recent_commit.as_ref().unwrap();
        assert_eq!(
            commit.sha.as_ref(),
            "1234567890abcdef1234567890abcdef12345678"
        );
        assert_eq!(commit.subject.as_ref(), "from tags");
        assert_eq!(commit.author_name.as_ref(), "tester");
    }

    #[test]
    fn detects_legacy_fossil_comment_verification_flag_error() {
        assert!(fossil_output_reports_unsupported_no_verify_comment(
            "",
            "unrecognized command-line option or missing argument: --no-verify-comment\n",
        ));
        assert!(!fossil_output_reports_unsupported_no_verify_comment(
            "",
            "unrecognized command-line option or missing argument: --allow-empty\n",
        ));
        assert!(!fossil_output_reports_unsupported_no_verify_comment(
            "",
            "check-in comment rejected by policy\n",
        ));
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn fossil_commit_retries_without_legacy_comment_verification_flag(
        cx: &mut TestAppContext,
    ) {
        use std::ffi::OsString;

        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let script_path = temp_dir.path().join("fossil");
        let captured_args_path = temp_dir.path().join("args");
        write_executable_script(
            &script_path,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"--no-verify-comment\" ]; then\n    echo 'unrecognized command-line option or missing argument: --no-verify-comment' >&2\n    exit 1\n  fi\ndone\nprintf '%s\\n' \"$@\" > \"$FZED_FOSSIL_TEST_ARGS\"\n",
        )
        .unwrap();

        let fossil = FossilBinary::new(
            script_path,
            temp_dir.path().to_path_buf(),
            Arc::default(),
            Arc::default(),
        );
        run_fossil_commit_with_legacy_comment_verification_fallback(
            &fossil,
            &[
                OsString::from("commit"),
                OsString::from("--comment"),
                OsString::from("message"),
                OsString::from("--no-verify-comment"),
            ],
            Arc::new(HashMap::from_iter([(
                "FZED_FOSSIL_TEST_ARGS".to_string(),
                captured_args_path.to_string_lossy().into_owned(),
            )])),
        )
        .await
        .unwrap();

        let captured_args = std::fs::read_to_string(captured_args_path).unwrap();
        assert!(captured_args.contains("commit\n"));
        assert!(!captured_args.contains("--no-verify-comment"));
    }

    #[test]
    fn parses_fossil_remote_and_checkout_metadata() {
        let remotes = parse_fossil_remote_list(
            "default            https://example.com/default\norigin             https://example.com/repo\n",
        );
        assert_eq!(remotes[0].name, "default");
        assert_eq!(remotes[0].url, "https://example.com/default");
        assert_eq!(remotes[1].name, "origin");
        assert_eq!(remotes[1].url, "https://example.com/repo");
        assert_eq!(
            parse_fossil_default_remote("https://example.com/default\n").as_deref(),
            Some("https://example.com/default")
        );
        assert_eq!(parse_fossil_default_remote("off\n"), None);

        let checkouts = parse_fossil_verbose_checkouts(
            "check-out:    /tmp/checkout1/           2026-05-13\ncheck-out:    /tmp/checkout2/           2026-05-13\n",
        );
        assert_eq!(
            checkouts,
            vec![
                PathBuf::from("/tmp/checkout1"),
                PathBuf::from("/tmp/checkout2")
            ]
        );
        let checkouts = parse_fossil_verbose_checkouts(
            "check-out:    /tmp/Card School/card.school/           2026-05-14\n",
        );
        assert_eq!(
            checkouts,
            vec![PathBuf::from("/tmp/Card School/card.school")]
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn fossil_sync_uses_explicit_remote(cx: &mut TestAppContext) {
        use std::ffi::OsString;

        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let script_path = temp_dir.path().join("fossil");
        write_executable_script(
            &script_path,
            "#!/bin/sh\n\
             if [ \"$1\" = remote ] && [ \"$#\" -eq 1 ]; then\n\
               printf '%s\\n' \"$FZED_FOSSIL_TEST_REMOTE\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = remote ] && [ \"$2\" = list ]; then\n\
               printf 'origin https://example.com/repo\\n'\n\
               exit 0\n\
             fi\n\
             exit 1\n",
        )
        .unwrap();

        let fossil = FossilBinary::new(
            script_path.clone(),
            temp_dir.path().to_path_buf(),
            Arc::default(),
            Arc::new(HashMap::from_iter([(
                "FZED_FOSSIL_TEST_REMOTE".to_string(),
                "https://example.com/default".to_string(),
            )])),
        );
        assert_eq!(
            fossil_sync_args(&fossil, None).await.unwrap(),
            vec![
                OsString::from("sync"),
                OsString::from("https://example.com/default")
            ]
        );

        let fossil = FossilBinary::new(
            script_path,
            temp_dir.path().to_path_buf(),
            Arc::default(),
            Arc::new(HashMap::from_iter([(
                "FZED_FOSSIL_TEST_REMOTE".to_string(),
                "off".to_string(),
            )])),
        );
        assert_eq!(
            fossil_sync_args(&fossil, None).await.unwrap(),
            vec![OsString::from("sync"), OsString::from("origin")]
        );
    }

    #[test]
    fn parses_fossil_history_stash_and_diff_metadata() {
        let info = parse_fossil_commit_info(
            "hash:         1234567890abcdef1234567890abcdef12345678 2026-05-13 06:44:31 UTC\nparent:       abcdef1234567890abcdef1234567890abcdef12 2026-05-13 06:44:01 UTC\ntags:         trunk, release\ncomment:      second commit (user: tester)\n",
        )
        .unwrap();
        assert_eq!(info.hash, "1234567890abcdef1234567890abcdef12345678");
        assert_eq!(
            info.parents,
            vec!["abcdef1234567890abcdef1234567890abcdef12"]
        );
        assert_eq!(info.comment, "second commit");
        assert_eq!(info.user.as_deref(), Some("tester"));
        assert!(info.timestamp > 0);

        let stash = parse_fossil_stash_list(
            "    2: [1234567890abcd] on 2026-05-13 06:44:06\n       work in progress\n       continued\n",
        )
        .unwrap();
        assert_eq!(stash.entries.len(), 1);
        assert_eq!(stash.entries[0].index, 2);
        assert_eq!(stash.entries[0].message, "work in progress\ncontinued");
        assert_eq!(
            fossil_stash_id_from_oid(&stash.entries[0].oid.to_string()),
            Some(2)
        );

        let diff = parse_fossil_unified_diff(
            "CHANGED a.txt\n--- a.txt\n+++ a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+three\nADDED b.txt\nIndex: b.txt\n==================================================================\n--- /dev/null\n+++ b.txt\n@@ -0,0 +1,1 @@\n+new\n",
        )
        .unwrap();
        assert_eq!(diff.stats, Some((2, 1)));
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files[0].path, RepoPath::new("a.txt").unwrap());
        assert_eq!(diff.files[0].old_text.as_deref(), Some("one\ntwo\n"));
        assert_eq!(diff.files[0].new_text.as_deref(), Some("one\nthree\n"));
        assert_eq!(diff.files[1].old_text, None);
        assert_eq!(diff.files[1].new_text.as_deref(), Some("new\n"));

        let timeline = parse_fossil_timeline_entries(
            "1234567890abcdef1234567890abcdef12345678\t2026-05-13 06:44:31\ttester\tsecond\ttrunk\ttrunk\n+++ no more data (1) +++\n",
        );
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].author, "tester");

        let blame_line =
            parse_fossil_blame_line(0, "1234567890 2026-05-13        tester: line text").unwrap();
        assert_eq!(blame_line.hash_prefix, "1234567890");
        assert_eq!(blame_line.author, "tester");
        assert_eq!(blame_line.line_number, 0);
    }

    #[gpui::test]
    async fn fossil_repository_reads_real_checkout(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        if Command::new("fossil").arg("version").output().is_err() {
            return;
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let fossil_home = temp_dir.path().join("home");
        let checkout = temp_dir.path().join("checkout with space");
        let repo_db = temp_dir.path().join("repo.fossil");
        std::fs::create_dir(&fossil_home).unwrap();
        std::fs::create_dir(&checkout).unwrap();

        run_fossil(
            &fossil_home,
            temp_dir.path(),
            &["init", repo_db.to_str().unwrap()],
        );
        run_fossil(
            &fossil_home,
            &checkout,
            &["open", repo_db.to_str().unwrap()],
        );

        std::fs::write(checkout.join("tracked.txt"), "initial\n").unwrap();
        std::fs::write(checkout.join("no-newline.txt"), "no newline").unwrap();
        run_fossil(&fossil_home, &checkout, &["add", "tracked.txt"]);
        run_fossil(&fossil_home, &checkout, &["add", "no-newline.txt"]);
        run_fossil(
            &fossil_home,
            &checkout,
            &[
                "commit",
                "--nosync",
                "--no-prompt",
                "--user-override",
                "tester",
                "-m",
                "initial",
            ],
        );

        std::fs::write(checkout.join("tracked.txt"), "modified\n").unwrap();
        std::fs::write(checkout.join("extra.txt"), "extra").unwrap();
        std::fs::create_dir(checkout.join(".fossil-settings")).unwrap();
        std::fs::write(
            checkout.join(".fossil-settings").join("ignore-glob"),
            "ignored.txt\nignored-dir\n",
        )
        .unwrap();
        std::fs::write(checkout.join("ignored.txt"), "ignored").unwrap();
        std::fs::create_dir(checkout.join("ignored-dir")).unwrap();
        std::fs::write(
            checkout.join("ignored-dir").join("generated.txt"),
            "ignored",
        )
        .unwrap();

        let repository = FossilRepository::new_for_test(
            &checkout.join(".fslckout"),
            Some("fossil".into()),
            cx.executor(),
            HashMap::from_iter([(
                "HOME".to_string(),
                fossil_home.to_string_lossy().into_owned(),
            )]),
        )
        .unwrap();

        let statuses = repository.status(&[]).await.unwrap();
        let lookup_status = |path: &str| {
            statuses
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new(path).unwrap())
                .map(|(_, status)| *status)
        };
        assert_eq!(
            lookup_status("tracked.txt"),
            Some(StatusCode::Modified.worktree())
        );
        assert_eq!(lookup_status("extra.txt"), Some(FileStatus::Untracked));
        assert_eq!(lookup_status("ignored.txt"), None);
        assert_eq!(lookup_status("ignored-dir/generated.txt"), None);

        let scoped_ignored_statuses = repository
            .status(&[
                RepoPath::new("ignored.txt").unwrap(),
                RepoPath::new("ignored-dir").unwrap(),
            ])
            .await
            .unwrap();
        assert!(scoped_ignored_statuses.entries.is_empty());

        let stats = repository.diff_stat(&[]).await.unwrap();
        let tracked_stat = stats
            .entries
            .iter()
            .find(|(repo_path, _)| repo_path == &RepoPath::new("tracked.txt").unwrap())
            .map(|(_, stat)| *stat)
            .unwrap();
        assert_eq!(tracked_stat.added, 1);
        assert_eq!(tracked_stat.deleted, 1);

        assert_eq!(
            repository
                .load_committed_text(RepoPath::new("tracked.txt").unwrap())
                .await,
            Some("initial\n".to_string())
        );
        assert_eq!(
            repository
                .load_committed_text(RepoPath::new("no-newline.txt").unwrap())
                .await,
            Some("no newline".to_string())
        );

        repository
            .checkout_files(
                "HEAD".to_string(),
                vec![RepoPath::new("tracked.txt").unwrap()],
                Arc::new(HashMap::default()),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "initial\n"
        );
        std::fs::write(checkout.join("tracked.txt"), "modified\n").unwrap();

        let head = repository.head_sha().await.unwrap();
        let head_details = repository.show("HEAD".to_string()).await.unwrap();
        assert_eq!(head_details.sha.as_ref(), head.as_str());
        assert_eq!(head_details.message.as_ref(), "initial");
        let branches = repository.branches().await.unwrap().branches;
        let head_branch = branches
            .iter()
            .find(|branch| branch.is_head && branch.name() == "trunk")
            .unwrap();
        let most_recent_commit = head_branch.most_recent_commit.as_ref().unwrap();
        assert_eq!(most_recent_commit.sha.as_ref(), head.as_str());
        assert_eq!(most_recent_commit.subject.as_ref(), "initial");
        assert_eq!(most_recent_commit.author_name.as_ref(), "tester");

        repository
            .create_branch("feature".to_string(), None)
            .await
            .unwrap();
        assert!(
            repository
                .branches()
                .await
                .unwrap()
                .branches
                .iter()
                .any(|branch| branch.is_head && branch.name() == "feature")
        );

        let sibling_checkout = temp_dir.path().join("feature-checkout");
        repository
            .create_worktree(
                CreateWorktreeTarget::ExistingBranch {
                    branch_name: "feature".to_string(),
                },
                sibling_checkout.clone(),
            )
            .await
            .unwrap();
        let worktrees = repository.worktrees().await.unwrap();
        let checkout = std::fs::canonicalize(&checkout).unwrap();
        let sibling_checkout = std::fs::canonicalize(&sibling_checkout).unwrap();
        assert!(worktrees.iter().any(|worktree| worktree.path == checkout));
        assert!(
            worktrees
                .iter()
                .any(|worktree| worktree.path == sibling_checkout)
        );

        repository
            .pull(
                Some("trunk".to_string()),
                "default".to_string(),
                false,
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                Arc::new(HashMap::default()),
                cx.to_async(),
            )
            .await
            .unwrap();
        assert!(
            repository
                .branches()
                .await
                .unwrap()
                .branches
                .iter()
                .any(|branch| branch.is_head && branch.name() == "trunk")
        );

        repository
            .commit_paths(
                "selected extra".into(),
                Some(("tester".into(), "tester@example.com".into())),
                CommitOptions {
                    amend: false,
                    signoff: false,
                    allow_empty: false,
                },
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                Arc::new(HashMap::default()),
                vec![RepoPath::new("extra.txt").unwrap()],
            )
            .await
            .unwrap();

        let statuses = repository.status(&[]).await.unwrap();
        let lookup_status = |path: &str| {
            statuses
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new(path).unwrap())
                .map(|(_, status)| *status)
        };
        assert_eq!(
            lookup_status("tracked.txt"),
            Some(StatusCode::Modified.worktree())
        );
        assert_eq!(lookup_status("extra.txt"), None);

        std::fs::remove_file(checkout.join("tracked.txt")).unwrap();
        repository
            .commit_paths(
                "delete tracked".into(),
                Some(("tester".into(), "tester@example.com".into())),
                CommitOptions {
                    amend: false,
                    signoff: false,
                    allow_empty: false,
                },
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                Arc::new(HashMap::default()),
                vec![RepoPath::new("tracked.txt").unwrap()],
            )
            .await
            .unwrap();

        let statuses = repository.status(&[]).await.unwrap();
        assert!(
            statuses
                .entries
                .iter()
                .all(|(repo_path, _)| repo_path != &RepoPath::new("tracked.txt").unwrap())
        );

        run_fossil(
            &fossil_home,
            &checkout,
            &["remote", "add", "origin", "https://example.com/repo"],
        );
        run_fossil(&fossil_home, &checkout, &["remote", "origin"]);
        run_fossil(&fossil_home, &checkout, &["settings", "autosync", "on"]);
        let sync_state = repository.fossil_sync_state().await.unwrap().unwrap();
        assert_eq!(sync_state.autosync.as_deref(), Some("on"));
        assert_eq!(
            sync_state.default_remote.as_deref(),
            Some("https://example.com/repo")
        );
        run_fossil(&fossil_home, &checkout, &["settings", "autosync", "off"]);

        std::fs::write(checkout.join("notes.txt"), "first\nsecond\n").unwrap();
        repository
            .commit_paths(
                "notes initial".into(),
                Some(("tester".into(), "tester@example.com".into())),
                CommitOptions {
                    amend: false,
                    signoff: false,
                    allow_empty: false,
                },
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                Arc::new(HashMap::default()),
                vec![RepoPath::new("notes.txt").unwrap()],
            )
            .await
            .unwrap();

        std::fs::write(checkout.join("notes.txt"), "first changed\nsecond\nthird\n").unwrap();
        repository
            .commit_paths(
                "notes update".into(),
                Some(("tester".into(), "tester@example.com".into())),
                CommitOptions {
                    amend: false,
                    signoff: false,
                    allow_empty: false,
                },
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                Arc::new(HashMap::default()),
                vec![RepoPath::new("notes.txt").unwrap()],
            )
            .await
            .unwrap();

        let head = repository.head_sha().await.unwrap();
        let head_oid = fossil_oid_from_hash(&head).unwrap();
        let details = repository.show(head.clone()).await.unwrap();
        assert_eq!(details.message, "notes update");
        assert_eq!(details.author_name, "tester");
        let commit_diff = repository.load_commit(head, cx.to_async()).await.unwrap();
        let notes_diff = commit_diff
            .files
            .iter()
            .find(|file| file.path == RepoPath::new("notes.txt").unwrap())
            .unwrap();
        assert_eq!(notes_diff.old_text.as_deref(), Some("first\nsecond\n"));
        assert_eq!(
            notes_diff.new_text.as_deref(),
            Some("first changed\nsecond\nthird\n")
        );

        let blame = repository
            .blame(
                RepoPath::new("notes.txt").unwrap(),
                Rope::from("first changed\nsecond\nthird\n"),
                LineEnding::Unix,
            )
            .await
            .unwrap();
        assert!(!blame.entries.is_empty());

        let (graph_tx, graph_rx) = async_channel::bounded(1);
        repository
            .initial_graph_data(LogSource::All, LogOrder::DateOrder, graph_tx)
            .await
            .unwrap();
        assert!(
            graph_rx
                .recv()
                .await
                .unwrap()
                .iter()
                .any(|commit| commit.sha == head_oid)
        );
        assert!(repository.cached_commit_data.lock().contains_key(&head_oid));

        let reader = repository.commit_data_reader().unwrap();
        let commit_data = reader.read(head_oid).await.unwrap();
        assert_eq!(commit_data.subject, "notes update");

        let (search_tx, search_rx) = async_channel::bounded(1);
        repository
            .search_commits(
                LogSource::All,
                SearchCommitArgs {
                    query: "notes update".into(),
                    case_sensitive: false,
                },
                search_tx,
            )
            .await
            .unwrap();
        assert_eq!(search_rx.recv().await.unwrap(), head_oid);

        std::fs::write(
            checkout.join("notes.txt"),
            "stashed change\nsecond\nthird\n",
        )
        .unwrap();
        repository
            .stash_paths(
                vec![RepoPath::new("notes.txt").unwrap()],
                Some("notes stash".to_string()),
                Arc::new(HashMap::default()),
            )
            .await
            .unwrap();
        let stash_entries = repository.stash_entries().await.unwrap();
        assert_eq!(stash_entries.entries.len(), 1);
        let stash_entry = stash_entries.entries[0].clone();
        assert_eq!(stash_entry.message, "notes stash");
        let stash_diff = repository
            .load_commit(stash_entry.oid.to_string(), cx.to_async())
            .await
            .unwrap();
        assert_eq!(stash_diff.files.len(), 1);

        repository
            .stash_apply(Some(stash_entry.index), Arc::new(HashMap::default()))
            .await
            .unwrap();
        assert_eq!(
            repository
                .status(&[])
                .await
                .unwrap()
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new("notes.txt").unwrap())
                .map(|(_, status)| *status),
            Some(StatusCode::Modified.worktree())
        );
        repository
            .stash_drop(Some(stash_entry.index), Arc::new(HashMap::default()))
            .await
            .unwrap();
        assert!(repository.stash_entries().await.unwrap().entries.is_empty());
    }

    #[gpui::test]
    async fn fossil_repository_records_and_undoes_rename(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        if Command::new("fossil").arg("version").output().is_err() {
            return;
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let fossil_home = temp_dir.path().join("home");
        let checkout = temp_dir.path().join("checkout");
        let repo_db = temp_dir.path().join("repo.fossil");
        std::fs::create_dir(&fossil_home).unwrap();
        std::fs::create_dir(&checkout).unwrap();

        run_fossil(
            &fossil_home,
            temp_dir.path(),
            &["init", repo_db.to_str().unwrap()],
        );
        run_fossil(
            &fossil_home,
            &checkout,
            &["open", repo_db.to_str().unwrap()],
        );

        std::fs::write(checkout.join("old.txt"), "contents\n").unwrap();
        run_fossil(&fossil_home, &checkout, &["add", "old.txt"]);
        run_fossil(
            &fossil_home,
            &checkout,
            &[
                "commit",
                "--nosync",
                "--no-prompt",
                "--user-override",
                "tester",
                "-m",
                "initial",
            ],
        );

        std::fs::rename(checkout.join("old.txt"), checkout.join("new.txt")).unwrap();
        let repository = FossilRepository::new_for_test(
            &checkout.join(".fslckout"),
            Some("fossil".into()),
            cx.executor(),
            HashMap::from_iter([(
                "HOME".to_string(),
                fossil_home.to_string_lossy().into_owned(),
            )]),
        )
        .unwrap();

        repository
            .record_fossil_rename(
                RepoPath::new("old.txt").unwrap(),
                RepoPath::new("new.txt").unwrap(),
                Arc::new(HashMap::default()),
            )
            .await
            .unwrap();

        let status = repository.status(&[]).await.unwrap();
        assert_eq!(
            status.renames.as_ref(),
            [crate::status::StatusRename {
                source: RepoPath::new("old.txt").unwrap(),
                target: RepoPath::new("new.txt").unwrap(),
            }]
        );
        assert!(
            status
                .entries
                .iter()
                .any(|(repo_path, _)| repo_path == &RepoPath::new("new.txt").unwrap())
        );

        repository
            .undo_fossil_rename(
                RepoPath::new("new.txt").unwrap(),
                Arc::new(HashMap::default()),
            )
            .await
            .unwrap();

        let status = repository.status(&[]).await.unwrap();
        assert!(status.renames.is_empty());
        assert_eq!(
            status
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new("old.txt").unwrap())
                .map(|(_, status)| *status),
            Some(StatusCode::Deleted.worktree())
        );
        assert_eq!(
            status
                .entries
                .iter()
                .find(|(repo_path, _)| repo_path == &RepoPath::new("new.txt").unwrap())
                .map(|(_, status)| *status),
            Some(FileStatus::Untracked)
        );
    }

    fn run_fossil(home: &Path, current_dir: &Path, args: &[&str]) -> Output {
        let output = Command::new("fossil")
            .env("HOME", home)
            .current_dir(current_dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fossil {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}
