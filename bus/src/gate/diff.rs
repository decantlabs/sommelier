//! The parsed form of a submission. `bundle` produces it, the gates read it.
//!
//! Shared type file of the spine. Its owner is the gate module. Other modules use it as it is.
//!
//! The `files` list is the diff of the two end trees. The commits between base and head are NOT diffed
//! one by one. A submission lands as one squashed commit made from the gated end tree, so a middle
//! commit never reaches main. The paths of every commit are listed in [`CommitInfo::paths`] so that a
//! middle commit cannot touch what the end diff may not.

use serde::{Deserialize, Serialize};

/// What happened to a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// The kind of tree entry after the change (before it, for a deleted path).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    Regular,
    Executable,
    Symlink,
    Submodule,
}

/// One line of a text file with its line number (new numbering for added, old for removed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub no: u32,
    pub text: String,
}

/// One changed path of a submission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// Path after the change, with forward slashes, relative to the repository root.
    pub path: String,
    /// Path before the change when it was renamed.
    pub old_path: Option<String>,
    pub change: ChangeKind,
    pub mode: FileMode,
    /// True when git treats the content as binary. Then `added` and `removed` are empty.
    pub binary: bool,
    /// Size of the new blob in bytes, 0 for a deleted path.
    pub size_after: u64,
    pub added: Vec<Line>,
    pub removed: Vec<Line>,
    /// The kind of the tree entry before the change, `None` for a new path.
    #[serde(default)]
    pub old_mode: Option<FileMode>,
    /// For a renamed text file: every line of the new content. A pure rename has no added lines, so
    /// the capability gate reads this list instead. Empty for every other change.
    #[serde(default)]
    pub content: Vec<Line>,
}

/// One commit of a submission, oldest first in [`ParsedDiff::commits`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub message: String,
    /// Committer fields are as hostile as author fields. Empty when a record predates them.
    #[serde(default)]
    pub committer_name: String,
    #[serde(default)]
    pub committer_email: String,
    /// Every path this commit changes against its parent, as git prints it. The end diff of a
    /// submission cannot show what a middle commit added and a later one removed, so the gates for
    /// paths, scope, hold paths and entry kinds read this list too.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// A submission after safe unpacking: the commits from `base` (exclusive) to `head` and their total diff.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedDiff {
    /// The commit the lease was based on.
    pub base: String,
    pub head: String,
    pub commits: Vec<CommitInfo>,
    /// The diff of `base` against `head`, one entry per path, sorted by path.
    pub files: Vec<FileChange>,
    /// Lower case hex SHA 256 of the bundle file.
    pub bundle_sha256: String,
    pub bundle_bytes: u64,
    /// Every file path of the base tree. The case collision gate checks new paths against it.
    #[serde(default)]
    pub base_paths: Vec<String>,
}
