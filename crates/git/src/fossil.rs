use crate::{
    Oid, RunHook,
    blame::Blame,
    repository::{
        Branch, CommitDataReader, CommitDetails, CommitDiff, CommitOptions, CreateWorktreeTarget,
        DiffType, FetchOptions, FossilSyncState, GitCommitTemplate, GitRepository,
        GitRepositoryCheckpoint, InitialGraphCommitData, LogOrder, LogSource, PushOptions, Remote,
        RemoteCommandOutput, RepoPath, RepositoryKind, ResetMode, SearchCommitArgs, Worktree,
    },
    stash::GitStash,
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
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use text::LineEnding;
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
        if let Some(info) = self.cached_info.lock().clone() {
            return Ok(info);
        }
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
        async move {
            Ok(GitStash {
                entries: Arc::default(),
            })
        }
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
        _commit: String,
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("checking out files")
    }

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        async move {
            Ok(CommitDetails {
                sha: SharedString::from(commit),
                ..Default::default()
            })
        }
        .boxed()
    }

    fn load_commit(&self, _commit: String, _cx: AsyncApp) -> BoxFuture<'_, Result<CommitDiff>> {
        Self::unsupported("loading commits")
    }

    fn blame(
        &self,
        _path: RepoPath,
        _content: Rope,
        _line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<Blame>> {
        Self::unsupported("blame")
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
        _paths: Vec<RepoPath>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("stashing paths")
    }

    fn stash_pop(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("stash pop")
    }

    fn stash_apply(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("stash apply")
    }

    fn stash_drop(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("stash drop")
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
        _log_source: LogSource,
        _log_order: LogOrder,
        _request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("initial graph data")
    }

    fn search_commits(
        &self,
        _log_source: LogSource,
        _search_args: SearchCommitArgs,
        _request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        Self::unsupported("searching commits")
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        Err(anyhow!(
            "Fossil backend does not support commit data streaming yet"
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
        FossilRepository, parse_fossil_branch_list_line, parse_fossil_changes,
        parse_fossil_default_remote, parse_fossil_info, parse_fossil_numstat,
        parse_fossil_remote_list, parse_fossil_verbose_checkouts,
    };
    use crate::{
        repository::{
            AskPassDelegate, CommitOptions, CreateWorktreeTarget, GitRepository, RepoPath,
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
