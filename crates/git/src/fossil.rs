use crate::{
    Oid, RunHook,
    blame::Blame,
    repository::{
        Branch, CommitData, CommitDataReader, CommitDetails, CommitDiff, CommitFile, CommitOptions,
        CreateWorktreeTarget, DiffType, FetchOptions, FossilSyncState, GRAPH_CHUNK_SIZE,
        GitCommitTemplate, GitRepository, GitRepositoryCheckpoint, InitialGraphCommitData,
        LogOrder, LogSource, PushOptions, Remote, RemoteCommandOutput, RepoPath, RepositoryKind,
        ResetMode, SearchCommitArgs, Worktree,
    },
    stash::{GitStash, StashEntry},
    status::{
        DiffStat, DiffTreeType, FileStatus, GitDiffStat, GitStatus, StatusCode, TreeDiff,
        UnmergedStatus, UnmergedStatusCode,
    },
};
use anyhow::{Context as _, Result, anyhow};
use async_channel::Sender;
use collections::HashMap;
use futures::{FutureExt as _, future::BoxFuture};
use gpui::{AsyncApp, BackgroundExecutor, SharedString, Task};
use parking_lot::Mutex;
use rope::Rope;
use smallvec::SmallVec;
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::ExitStatus,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use text::LineEnding;
use time::PrimitiveDateTime;
use util::{command::new_command, paths::PathStyle};

pub struct FossilRepository {
    checkout_db_path: PathBuf,
    work_directory: PathBuf,
    fossil_binary_path: PathBuf,
    executor: BackgroundExecutor,
    is_trusted: Arc<AtomicBool>,
    cached_info: Arc<Mutex<Option<FossilInfo>>>,
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
        let fossil_binary_path = fossil_binary_path.unwrap_or_else(|| PathBuf::from("fossil"));
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

impl GitRepository for FossilRepository {
    fn kind(&self) -> RepositoryKind {
        RepositoryKind::Fossil
    }

    fn reload_index(&self) {
        *self.cached_info.lock() = None;
    }

    fn load_index_text(&self, _path: RepoPath) -> BoxFuture<'_, Option<String>> {
        async move { None }.boxed()
    }

    fn load_committed_text(&self, path: RepoPath) -> BoxFuture<'_, Option<String>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                fossil
                    .run(&[
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
            let output = fossil.run(&fossil_changes_args(path_prefixes)).await?;
            parse_fossil_changes(&output)
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

    fn stash_entries(&self) -> BoxFuture<'_, Result<GitStash>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let output = fossil.run_raw(&["stash", "list"]).await?;
                parse_fossil_stash_list(&output)
            })
            .boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<Vec<Branch>>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let current = fossil.run(&["branch", "current"]).await.ok();
                let list = fossil
                    .run(&["branch", "list", "--all"])
                    .await
                    .unwrap_or_default();
                let mut branches = Vec::new();
                for line in list.lines() {
                    if let Some(name) = parse_fossil_branch_list_line(line) {
                        let is_head = current.as_deref() == Some(name.as_str());
                        branches.push(Branch {
                            is_head,
                            ref_name: SharedString::from(format!("refs/heads/{name}")),
                            upstream: None,
                            most_recent_commit: None,
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
                        most_recent_commit: None,
                    });
                }
                Ok(branches)
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
                    let info = fossil_commit_info(&fossil, &commit).await?;
                    let Some(parent) = info.parents.first().cloned() else {
                        return Ok(CommitDiff { files: Vec::new() });
                    };
                    fossil
                        .run_raw(&[
                            OsString::from("diff"),
                            OsString::from("--from"),
                            OsString::from(parent),
                            OsString::from("--to"),
                            OsString::from(info.hash),
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
                fossil.run_with_env(&args, env).await?;
                Ok(())
            })
            .boxed()
    }

    fn stash_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let fossil = self.fossil_binary();
        self.executor
            .spawn(async move {
                let mut args = vec![
                    OsString::from("stash"),
                    OsString::from("save"),
                    OsString::from("--comment"),
                    OsString::from("fzed stash"),
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

                let mut args = vec![OsString::from("sync")];
                if upstream_name != "default" {
                    args.push(OsString::from(upstream_name));
                }
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
                let mut args = vec![OsString::from("sync")];
                if let FetchOptions::Remote(remote) = fetch_options
                    && remote.name != "default"
                {
                    args.push(OsString::from(remote.name.to_string()));
                }
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

    fn diff_stat(&self, path_prefixes: &[RepoPath]) -> BoxFuture<'_, Result<GitDiffStat>> {
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
        self.executor
            .spawn(async move {
                let _ = log_order;
                let output = fossil
                    .run_raw(&fossil_timeline_args(&log_source, GRAPH_CHUNK_SIZE))
                    .await?;
                let mut commits = Vec::new();
                for entry in parse_fossil_timeline_entries(&output) {
                    let info = fossil_commit_info(&fossil, &entry.hash).await?;
                    let Some(sha) = fossil_oid_from_hash(&info.hash) else {
                        continue;
                    };
                    let parents = info
                        .parents
                        .iter()
                        .filter_map(|parent| fossil_oid_from_hash(parent))
                        .collect::<SmallVec<[_; 1]>>();
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

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        let fossil = self.fossil_binary();
        Ok(CommitDataReader::from_resolver(
            self.executor.clone(),
            move |sha| {
                let fossil = fossil.clone();
                async move { fossil_commit_data(&fossil, &sha.to_string()).await }
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

async fn fossil_commit_data(fossil: &FossilBinary, commit: &str) -> Result<CommitData> {
    let info = fossil_commit_info(fossil, commit).await?;
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
            file.old_text.push_str(rest);
            file.old_text.push('\n');
        } else if let Some(rest) = line.strip_prefix('+') {
            file.new_text.push_str(rest);
            file.new_text.push('\n');
        }
    }

    push_fossil_diff_file(&mut files, current);
    Ok(CommitDiff { files })
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

fn parse_fossil_changes(output: &str) -> Result<GitStatus> {
    Ok(fossil_changes_to_status(parse_fossil_changes_with_kind(
        output,
    )))
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
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        let Ok(repo_path) = RepoPath::from_std_path(Path::new(path), PathStyle::local()) else {
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
        entries.push(FossilChange { repo_path, kind });
    }
    entries.sort_unstable_by(|left, right| left.repo_path.cmp(&right.repo_path));
    entries.dedup_by(|left, right| left.repo_path == right.repo_path);
    entries
}

fn fossil_changes_to_status(changes: Vec<FossilChange>) -> GitStatus {
    GitStatus {
        entries: changes
            .into_iter()
            .map(|change| (change.repo_path, change.kind.status()))
            .collect::<Vec<_>>()
            .into(),
    }
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
            let path = value.split_whitespace().next()?;
            (!path.is_empty()).then(|| PathBuf::from(path.trim_end_matches('/')))
        })
        .collect()
}

fn fossil_branch_name(name: &str) -> String {
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

fn normalize_existing_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

#[derive(Clone)]
struct FossilBinary {
    fossil_binary_path: PathBuf,
    working_directory: PathBuf,
    envs: Arc<HashMap<String, String>>,
}

impl FossilBinary {
    fn new(
        fossil_binary_path: PathBuf,
        working_directory: PathBuf,
        envs: Arc<HashMap<String, String>>,
    ) -> Self {
        Self {
            fossil_binary_path,
            working_directory,
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
        let mut command = self.build_command(args);
        if let Some(env) = env {
            command.envs(env.iter());
        }
        let output = command.output().await?;
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
            envs: self.envs.clone(),
        }
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
        FossilRepository, fossil_oid_from_hash, fossil_stash_id_from_oid, parse_fossil_blame_line,
        parse_fossil_branch_list_line, parse_fossil_changes, parse_fossil_commit_info,
        parse_fossil_default_remote, parse_fossil_info, parse_fossil_numstat,
        parse_fossil_remote_list, parse_fossil_stash_list, parse_fossil_timeline_entries,
        parse_fossil_unified_diff, parse_fossil_verbose_checkouts,
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
        let status = parse_fossil_changes(
            "EDITED src/main.rs\nADDED new file.txt\nDELETED old.rs\nEXTRA scratch.txt\nCONFLICT both.rs\n",
        )
        .unwrap();

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

        std::fs::write(checkout.join("tracked.txt"), "initial").unwrap();
        run_fossil(&fossil_home, &checkout, &["add", "tracked.txt"]);
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

        std::fs::write(checkout.join("tracked.txt"), "modified").unwrap();
        std::fs::write(checkout.join("extra.txt"), "extra").unwrap();

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
            Some("initial".to_string())
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
            "initial"
        );
        std::fs::write(checkout.join("tracked.txt"), "modified").unwrap();

        assert!(repository.head_sha().await.is_some());
        assert!(
            repository
                .branches()
                .await
                .unwrap()
                .iter()
                .any(|branch| branch.is_head && branch.name() == "trunk")
        );

        repository
            .create_branch("feature".to_string(), None)
            .await
            .unwrap();
        assert!(
            repository
                .branches()
                .await
                .unwrap()
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
                Arc::new(HashMap::default()),
            )
            .await
            .unwrap();
        let stash_entries = repository.stash_entries().await.unwrap();
        assert_eq!(stash_entries.entries.len(), 1);
        let stash_entry = stash_entries.entries[0].clone();
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
