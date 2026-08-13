//! Unix Git worktree allocation and merge preflight for the Workspace CLI.
//!
//! This adapter owns Git subprocesses.  The core Workspace module remains
//! limited to local identity, leases, read sets, and guarded file writes.

use std::error::Error;
use std::fmt;
#[cfg(unix)]
use std::fs;
use std::io;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Command, Output};

use greentyper_core::workspace::WorkspaceFacts;
#[cfg(unix)]
use greentyper_core::workspace::{WorkspaceAccess, WorkspaceRoot};
use serde::Serialize;

#[cfg(unix)]
const MAX_GIT_OUTPUT_BYTES: usize = 1024 * 1024;
#[cfg(unix)]
const MAX_REF_BYTES: usize = 128;
#[cfg(unix)]
const MAX_CONFLICT_PATH_BYTES: usize = 512;

#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeAllocationStatus {
    Created,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeAllocation {
    pub status: WorktreeAllocationStatus,
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    pub repository: WorkspaceFacts,
    pub worktree: WorkspaceFacts,
}

#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeCheckStatus {
    Mergeable,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MergeCheck {
    pub status: MergeCheckStatus,
    pub target_commit: String,
    pub source_commit: String,
    pub merge_tree: String,
    pub conflict_paths: Vec<String>,
}

pub fn allocate_worktree(
    root_path: impl AsRef<Path>,
    worktree_path: impl AsRef<Path>,
    branch: &str,
    base: &str,
) -> Result<WorktreeAllocation, WorkspaceGitError> {
    #[cfg(not(unix))]
    {
        let _ = (root_path, worktree_path, branch, base);
        Err(WorkspaceGitError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    {
        validate_ref(branch, true)?;
        validate_ref(base, false)?;

        let root = WorkspaceRoot::open(root_path).map_err(WorkspaceGitError::Workspace)?;
        verify_repository(&root)?;
        let _lease = root
            .acquire_lease(WorkspaceAccess::ReadWrite)
            .map_err(WorkspaceGitError::Workspace)?;
        let target = validate_new_worktree_path(root.path(), worktree_path.as_ref())?;
        let base_commit = resolve_commit(root.path(), base)?;

        let branch_probe = git(
            root.path(),
            [
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )?;
        match branch_probe.status.code() {
            Some(1) => {}
            Some(0) => return Err(WorkspaceGitError::BranchExists),
            _ => return Err(WorkspaceGitError::CommandFailed("branch lookup")),
        }

        let target_arg = target
            .to_str()
            .ok_or(WorkspaceGitError::InvalidWorktreePath)?;
        let allocation = git(
            root.path(),
            ["worktree", "add", "-b", branch, target_arg, &base_commit],
        )?;
        if !allocation.status.success() {
            return Err(WorkspaceGitError::CommandFailed("worktree allocation"));
        }

        let verified = (|| {
            let worktree = WorkspaceRoot::open(&target).map_err(WorkspaceGitError::Workspace)?;
            let head_commit = resolve_commit(worktree.path(), "HEAD")?;
            if head_commit != base_commit {
                return Err(WorkspaceGitError::UnexpectedOutput);
            }
            let actual_branch = git_text(worktree.path(), ["branch", "--show-current"])?;
            if actual_branch != branch {
                return Err(WorkspaceGitError::UnexpectedOutput);
            }
            Ok(WorktreeAllocation {
                status: WorktreeAllocationStatus::Created,
                branch: branch.to_owned(),
                base_commit,
                head_commit,
                repository: root.facts(),
                worktree: worktree.facts(),
            })
        })();

        if verified.is_err() {
            rollback_allocation(root.path(), &target, branch);
        }
        verified
    }
}

pub fn check_merge(
    root_path: impl AsRef<Path>,
    target: &str,
    source: &str,
) -> Result<MergeCheck, WorkspaceGitError> {
    #[cfg(not(unix))]
    {
        let _ = (root_path, target, source);
        Err(WorkspaceGitError::UnsupportedPlatform)
    }

    #[cfg(unix)]
    {
        validate_ref(target, false)?;
        validate_ref(source, false)?;
        let root = WorkspaceRoot::open(root_path).map_err(WorkspaceGitError::Workspace)?;
        verify_repository(&root)?;
        let _lease = root
            .acquire_lease(WorkspaceAccess::ReadWrite)
            .map_err(WorkspaceGitError::Workspace)?;
        let target_commit = resolve_commit(root.path(), target)?;
        let source_commit = resolve_commit(root.path(), source)?;

        let output = git(
            root.path(),
            [
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                "--no-messages",
                &target_commit,
                &source_commit,
            ],
        )?;
        let status = match output.status.code() {
            Some(0) => MergeCheckStatus::Mergeable,
            Some(1) => MergeCheckStatus::Conflict,
            _ => return Err(WorkspaceGitError::CommandFailed("merge check")),
        };
        let (merge_tree, conflict_paths) = parse_merge_tree_output(&output.stdout)?;

        if resolve_commit(root.path(), target)? != target_commit
            || resolve_commit(root.path(), source)? != source_commit
        {
            return Err(WorkspaceGitError::StaleReference);
        }
        if status == MergeCheckStatus::Mergeable && !conflict_paths.is_empty() {
            return Err(WorkspaceGitError::UnexpectedOutput);
        }

        Ok(MergeCheck {
            status,
            target_commit,
            source_commit,
            merge_tree,
            conflict_paths,
        })
    }
}

#[cfg(unix)]
fn verify_repository(root: &WorkspaceRoot) -> Result<(), WorkspaceGitError> {
    let top_level = git_text(root.path(), ["rev-parse", "--show-toplevel"])?;
    let top_level = fs::canonicalize(top_level).map_err(WorkspaceGitError::Io)?;
    if top_level != root.path() {
        return Err(WorkspaceGitError::InvalidRepository);
    }
    let bare = git_text(root.path(), ["rev-parse", "--is-bare-repository"])?;
    if bare != "false" {
        return Err(WorkspaceGitError::InvalidRepository);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_new_worktree_path(root: &Path, target: &Path) -> Result<PathBuf, WorkspaceGitError> {
    if !target.is_absolute() || target.exists() {
        return Err(if target.exists() {
            WorkspaceGitError::WorktreeExists
        } else {
            WorkspaceGitError::InvalidWorktreePath
        });
    }
    let parent = target
        .parent()
        .ok_or(WorkspaceGitError::InvalidWorktreePath)?;
    let canonical_parent = fs::canonicalize(parent).map_err(WorkspaceGitError::Io)?;
    if canonical_parent.starts_with(root) || target.file_name().is_none() {
        return Err(WorkspaceGitError::InvalidWorktreePath);
    }
    Ok(canonical_parent.join(
        target
            .file_name()
            .ok_or(WorkspaceGitError::InvalidWorktreePath)?,
    ))
}

#[cfg(unix)]
fn validate_ref(value: &str, branch: bool) -> Result<(), WorkspaceGitError> {
    if value.is_empty()
        || value.len() > MAX_REF_BYTES
        || value.starts_with('-')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(if branch {
            WorkspaceGitError::InvalidBranch
        } else {
            WorkspaceGitError::InvalidReference
        });
    }
    if branch {
        let output = Command::new("git")
            .args(["check-ref-format", "--branch", value])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .output()
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    WorkspaceGitError::GitUnavailable
                } else {
                    WorkspaceGitError::Io(error)
                }
            })?;
        if !output.status.success() {
            return Err(WorkspaceGitError::InvalidBranch);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn resolve_commit(root: &Path, reference: &str) -> Result<String, WorkspaceGitError> {
    let revision = format!("{reference}^{{commit}}");
    let output = git(root, ["rev-parse", "--verify", &revision])?;
    if !output.status.success() {
        return Err(WorkspaceGitError::InvalidReference);
    }
    let commit = text(&output.stdout)?;
    validate_object_id(&commit)?;
    Ok(commit)
}

#[cfg(unix)]
fn parse_merge_tree_output(bytes: &[u8]) -> Result<(String, Vec<String>), WorkspaceGitError> {
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let tree = fields
        .next()
        .ok_or(WorkspaceGitError::UnexpectedOutput)
        .and_then(text)?;
    validate_object_id(&tree)?;
    let mut conflicts = Vec::new();
    for field in fields {
        let path = text(field)?;
        if path.is_empty()
            || path.len() > MAX_CONFLICT_PATH_BYTES
            || path.starts_with('/')
            || path.bytes().any(|byte| byte.is_ascii_control())
            || path.split('/').any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(WorkspaceGitError::UnexpectedOutput);
        }
        conflicts.push(path);
    }
    conflicts.sort();
    conflicts.dedup();
    Ok((tree, conflicts))
}

#[cfg(unix)]
fn validate_object_id(value: &str) -> Result<(), WorkspaceGitError> {
    if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(WorkspaceGitError::UnexpectedOutput)
    }
}

#[cfg(unix)]
fn git_text<const N: usize>(root: &Path, args: [&str; N]) -> Result<String, WorkspaceGitError> {
    let output = git(root, args)?;
    if !output.status.success() {
        return Err(WorkspaceGitError::CommandFailed("repository inspection"));
    }
    text(&output.stdout)
}

#[cfg(unix)]
fn text(bytes: &[u8]) -> Result<String, WorkspaceGitError> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| WorkspaceGitError::UnexpectedOutput)?
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    if value.is_empty() {
        Err(WorkspaceGitError::UnexpectedOutput)
    } else {
        Ok(value)
    }
}

#[cfg(unix)]
fn git<const N: usize>(root: &Path, args: [&str; N]) -> Result<Output, WorkspaceGitError> {
    let output = Command::new("git")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                WorkspaceGitError::GitUnavailable
            } else {
                WorkspaceGitError::Io(error)
            }
        })?;
    if output.stdout.len() > MAX_GIT_OUTPUT_BYTES || output.stderr.len() > MAX_GIT_OUTPUT_BYTES {
        return Err(WorkspaceGitError::OutputTooLarge);
    }
    Ok(output)
}

#[cfg(unix)]
fn rollback_allocation(root: &Path, target: &Path, branch: &str) {
    let Some(target) = target.to_str() else {
        return;
    };
    let _ = git(root, ["worktree", "remove", "--force", target]);
    let _ = git(root, ["branch", "-D", branch]);
}

#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug)]
pub enum WorkspaceGitError {
    #[cfg(not(unix))]
    UnsupportedPlatform,
    GitUnavailable,
    InvalidRepository,
    InvalidReference,
    InvalidBranch,
    InvalidWorktreePath,
    WorktreeExists,
    BranchExists,
    StaleReference,
    UnexpectedOutput,
    OutputTooLarge,
    CommandFailed(&'static str),
    Workspace(greentyper_core::workspace::WorkspaceError),
    Io(io::Error),
}

impl fmt::Display for WorkspaceGitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(not(unix))]
            Self::UnsupportedPlatform => {
                write!(formatter, "Git worktrees are unavailable on this platform")
            }
            Self::GitUnavailable => write!(formatter, "Git executable is unavailable"),
            Self::InvalidRepository => {
                write!(formatter, "workspace root is not a Git repository root")
            }
            Self::InvalidReference => write!(formatter, "Git reference is invalid or missing"),
            Self::InvalidBranch => write!(formatter, "Git worktree branch is invalid"),
            Self::InvalidWorktreePath => write!(formatter, "Git worktree path is invalid"),
            Self::WorktreeExists => write!(formatter, "Git worktree path already exists"),
            Self::BranchExists => write!(formatter, "Git worktree branch already exists"),
            Self::StaleReference => write!(formatter, "Git reference changed during merge check"),
            Self::UnexpectedOutput => write!(formatter, "Git returned an invalid worktree result"),
            Self::OutputTooLarge => write!(formatter, "Git worktree result is too large"),
            Self::CommandFailed(operation) => write!(formatter, "Git {operation} failed"),
            Self::Workspace(source) => write!(formatter, "{source}"),
            Self::Io(_) => write!(formatter, "Git worktree I/O failed"),
        }
    }
}

impl Error for WorkspaceGitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Workspace(source) => Some(source),
            Self::Io(source) => Some(source),
            _ => None,
        }
    }
}
