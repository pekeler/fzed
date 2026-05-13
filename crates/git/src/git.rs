pub mod blame;
pub mod commit;
pub mod fossil;
mod hosting_provider;
mod remote;
pub mod repository;
pub mod stash;
pub mod status;

pub use crate::hosting_provider::*;
pub use crate::remote::*;
use anyhow::{Context as _, Result};
pub use git2 as libgit;
use gpui::{Action, actions};
pub use repository::RemoteCommandOutput;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

pub const DOT_GIT: &str = ".git";
pub const DOT_FOSSIL: &str = ".fslckout";
pub const FOSSIL_CHECKOUT: &str = "_FOSSIL_";
pub const GITIGNORE: &str = ".gitignore";
pub const FSMONITOR_DAEMON: &str = "fsmonitor--daemon";
pub const LFS_DIR: &str = "lfs";
pub const COMMIT_MESSAGE: &str = "COMMIT_EDITMSG";
pub const INDEX_LOCK: &str = "index.lock";
pub const REPO_EXCLUDE: &str = "info/exclude";

actions!(
    git,
    [
        // per-hunk
        /// Toggles the staged state of the hunk or status entry at cursor.
        ToggleStaged,
        /// Stage status entries between an anchor entry and the cursor.
        StageRange,
        /// Stages the current hunk and moves to the next one.
        StageAndNext,
        /// Unstages the current hunk and moves to the next one.
        UnstageAndNext,
        /// Restores the selected hunks to their original state.
        #[action(deprecated_aliases = ["editor::RevertSelectedHunks"])]
        Restore,
        /// Restores the selected hunks to their original state and moves to the
        /// next one.
        RestoreAndNext,
        // per-file
        /// Shows git blame information for the current file.
        #[action(deprecated_aliases = ["editor::ToggleGitBlame"])]
        Blame,
        /// Shows the git history for the selected file, folder, or project.
        FileHistory,
        /// Stages the current file.
        StageFile,
        /// Unstages the current file.
        UnstageFile,
        // repo-wide
        /// Stages all changes in the repository.
        StageAll,
        /// Unstages all changes in the repository.
        UnstageAll,
        /// Stashes all changes in the repository, including untracked files.
        StashAll,
        /// Pops the most recent stash.
        StashPop,
        /// Apply the most recent stash.
        StashApply,
        /// Restores all tracked files to their last committed state.
        RestoreTrackedFiles,
        /// Moves all untracked files to trash.
        TrashUntrackedFiles,
        /// Undoes the last commit, keeping changes in the working directory.
        Uncommit,
        /// Pushes commits to the remote repository.
        Push,
        /// Pushes commits to a specific remote branch.
        PushTo,
        /// Force pushes commits to the remote repository.
        ForcePush,
        /// Pulls changes from the remote repository.
        Pull,
        /// Pulls changes from the remote repository with rebase.
        PullRebase,
        /// Fetches changes from the remote repository.
        Fetch,
        /// Fetches changes from a specific remote.
        FetchFrom,
        /// Creates a new commit with staged changes.
        Commit,
        /// Amends the last commit with staged changes.
        Amend,
        /// Enable the --signoff option.
        Signoff,
        /// Cancels the current git operation.
        Cancel,
        /// Expands the commit message editor.
        ExpandCommitEditor,
        /// Toggles whether the commit message editor fills all the available
        /// vertical space within the git panel.
        ToggleFillCommitEditor,
        /// Generates a commit message using AI.
        GenerateCommitMessage,
        /// Initializes a new git repository.
        Init,
        /// Opens all modified files in the editor.
        OpenModifiedFiles,
        /// Clones a repository.
        Clone,
        ViewCommit,
        /// Adds a file to .gitignore.
        AddToGitignore,
        /// Copies the current branch name to the clipboard.
        CopyBranchName,
    ]
);

pub mod fossil_actions {
    use gpui::actions;

    actions!(
        fossil,
        [
            /// Initializes a new Fossil repository and opens a checkout.
            Init,
            /// Clones a remote Fossil repository and opens a checkout.
            Clone,
            /// Opens an existing Fossil repository database into a checkout.
            OpenRepository,
            /// Opens the Fossil check-in modal for the active checkout.
            CheckIn,
            /// Generates a Fossil check-in message using AI.
            GenerateCheckInMessage,
            /// Includes the selected path in the next Fossil check-in.
            IncludeFile,
            /// Excludes the selected path from the next Fossil check-in.
            ExcludeFile,
            /// Toggles whether the selected path is included in the next Fossil check-in.
            ToggleIncluded,
            /// Includes changed paths between the anchor path and the selected path.
            IncludeRange,
            /// Includes all changed paths in the next Fossil check-in.
            IncludeAll,
            /// Excludes all selected paths from the next Fossil check-in.
            ExcludeAll,
            /// Stashes tracked changes in the active Fossil checkout.
            StashTracked,
            /// Pops the most recent Fossil stash.
            PopStash,
            /// Applies the most recent Fossil stash.
            ApplyStash,
            /// Opens the Fossil stash selector.
            ViewStash,
            /// Reverts the selected path in the active Fossil checkout.
            RevertFile,
            /// Reverts tracked checkout changes in the active Fossil checkout.
            RevertTrackedFiles,
            /// Moves Fossil extra files to trash.
            CleanExtras,
            /// Synchronizes the active Fossil checkout with its remote.
            Sync,
            /// Updates the active Fossil checkout.
            Update,
            /// Opens the Fossil timeline for the active checkout.
            Timeline,
            /// Opens the Fossil timeline for the selected path.
            FileTimeline,
            /// Shows Fossil annotation information for the current file.
            Annotate,
            /// Shows Fossil blame information for the current file.
            Blame,
            /// Opens the Fossil branch selector.
            Branch,
            /// Switches the active Fossil checkout to a different branch.
            SwitchBranch,
            /// Opens the Fossil checkout selector.
            Checkouts,
            /// Selects a different repository.
            SelectRepo,
            /// Opens all modified files in the active Fossil checkout.
            OpenModifiedFiles,
            /// Copies the current branch name to the clipboard.
            CopyBranchName,
            /// Opens a Fossil check-in by hash.
            ViewCheckIn,
        ]
    );
}

/// Renames a git branch.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git)]
#[serde(deny_unknown_fields)]
pub struct RenameBranch {
    /// The branch to rename.
    ///
    /// Default: the current branch.
    #[serde(default)]
    pub branch: Option<String>,
}

/// Restores a file to its last committed state, discarding local changes.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, Action)]
#[action(namespace = git, deprecated_aliases = ["editor::RevertFile"])]
#[serde(deny_unknown_fields)]
pub struct RestoreFile {
    #[serde(default)]
    pub skip_prompt: bool,
}

/// The length of a Git short SHA.
pub const SHORT_SHA_LENGTH: usize = 7;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct Oid(libgit::Oid);

impl Oid {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let oid = libgit::Oid::from_bytes(bytes).context("failed to parse bytes into git oid")?;
        Ok(Self(oid))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn random(rng: &mut impl rand::Rng) -> Self {
        let mut bytes = [0; 20];
        rng.fill(&mut bytes);
        Self::from_bytes(&bytes).unwrap()
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.0.is_zero()
    }

    /// Returns this [`Oid`] as a short SHA.
    pub fn display_short(&self) -> String {
        self.to_string().chars().take(SHORT_SHA_LENGTH).collect()
    }
}

impl TryFrom<&str> for Oid {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> std::prelude::v1::Result<Self, Self::Error> {
        Oid::from_str(value)
    }
}

impl FromStr for Oid {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::prelude::v1::Result<Self, Self::Err> {
        libgit::Oid::from_str(s)
            .context("parsing git oid")
            .map(Self)
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Serialize for Oid {
    fn serialize<S>(&self, serializer: S) -> std::prelude::v1::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Oid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<Oid>().map_err(serde::de::Error::custom)
    }
}

impl Default for Oid {
    fn default() -> Self {
        Self(libgit::Oid::zero())
    }
}

impl From<Oid> for u32 {
    fn from(oid: Oid) -> Self {
        let bytes = oid.0.as_bytes();
        debug_assert!(bytes.len() > 4);

        let mut u32_bytes: [u8; 4] = [0; 4];
        u32_bytes.copy_from_slice(&bytes[..4]);

        u32::from_ne_bytes(u32_bytes)
    }
}

impl From<Oid> for usize {
    fn from(oid: Oid) -> Self {
        let bytes = oid.0.as_bytes();
        debug_assert!(bytes.len() > 8);

        let mut u64_bytes: [u8; 8] = [0; 8];
        u64_bytes.copy_from_slice(&bytes[..8]);

        u64::from_ne_bytes(u64_bytes) as usize
    }
}

#[repr(i32)]
#[derive(Copy, Clone, Debug)]
pub enum RunHook {
    PreCommit,
}

impl RunHook {
    pub fn as_str(&self) -> &str {
        match self {
            Self::PreCommit => "pre-commit",
        }
    }

    pub fn to_proto(&self) -> i32 {
        *self as i32
    }

    pub fn from_proto(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::PreCommit),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Action;

    #[test]
    fn fossil_action_names_are_first_class() {
        assert_eq!(fossil_actions::CheckIn::name_for_type(), "fossil::CheckIn");
        assert_eq!(
            fossil_actions::GenerateCheckInMessage::name_for_type(),
            "fossil::GenerateCheckInMessage"
        );
        assert_eq!(
            fossil_actions::IncludeFile::name_for_type(),
            "fossil::IncludeFile"
        );
        assert_eq!(
            fossil_actions::ExcludeFile::name_for_type(),
            "fossil::ExcludeFile"
        );
        assert_eq!(
            fossil_actions::ToggleIncluded::name_for_type(),
            "fossil::ToggleIncluded"
        );
        assert_eq!(
            fossil_actions::IncludeRange::name_for_type(),
            "fossil::IncludeRange"
        );
        assert_eq!(
            fossil_actions::IncludeAll::name_for_type(),
            "fossil::IncludeAll"
        );
        assert_eq!(
            fossil_actions::ExcludeAll::name_for_type(),
            "fossil::ExcludeAll"
        );
        assert_eq!(
            fossil_actions::StashTracked::name_for_type(),
            "fossil::StashTracked"
        );
        assert_eq!(
            fossil_actions::PopStash::name_for_type(),
            "fossil::PopStash"
        );
        assert_eq!(
            fossil_actions::ApplyStash::name_for_type(),
            "fossil::ApplyStash"
        );
        assert_eq!(
            fossil_actions::ViewStash::name_for_type(),
            "fossil::ViewStash"
        );
        assert_eq!(
            fossil_actions::RevertFile::name_for_type(),
            "fossil::RevertFile"
        );
        assert_eq!(
            fossil_actions::RevertTrackedFiles::name_for_type(),
            "fossil::RevertTrackedFiles"
        );
        assert_eq!(
            fossil_actions::CleanExtras::name_for_type(),
            "fossil::CleanExtras"
        );
        assert_eq!(fossil_actions::Init::name_for_type(), "fossil::Init");
        assert_eq!(fossil_actions::Clone::name_for_type(), "fossil::Clone");
        assert_eq!(
            fossil_actions::OpenRepository::name_for_type(),
            "fossil::OpenRepository"
        );
        assert_eq!(fossil_actions::Sync::name_for_type(), "fossil::Sync");
        assert_eq!(fossil_actions::Update::name_for_type(), "fossil::Update");
        assert_eq!(
            fossil_actions::Timeline::name_for_type(),
            "fossil::Timeline"
        );
        assert_eq!(
            fossil_actions::FileTimeline::name_for_type(),
            "fossil::FileTimeline"
        );
        assert_eq!(
            fossil_actions::Annotate::name_for_type(),
            "fossil::Annotate"
        );
        assert_eq!(fossil_actions::Blame::name_for_type(), "fossil::Blame");
        assert_eq!(fossil_actions::Branch::name_for_type(), "fossil::Branch");
        assert_eq!(
            fossil_actions::SwitchBranch::name_for_type(),
            "fossil::SwitchBranch"
        );
        assert_eq!(
            fossil_actions::Checkouts::name_for_type(),
            "fossil::Checkouts"
        );
        assert_eq!(
            fossil_actions::SelectRepo::name_for_type(),
            "fossil::SelectRepo"
        );
        assert_eq!(
            fossil_actions::OpenModifiedFiles::name_for_type(),
            "fossil::OpenModifiedFiles"
        );
        assert_eq!(
            fossil_actions::CopyBranchName::name_for_type(),
            "fossil::CopyBranchName"
        );
        assert_eq!(
            fossil_actions::ViewCheckIn::name_for_type(),
            "fossil::ViewCheckIn"
        );
    }
}
