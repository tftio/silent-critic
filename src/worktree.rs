//! Git worktree management for dispatched workers (T009).
//!
//! A dispatched task gets a git worktree of the supervised repository,
//! created outside that repository (under the store's plan directory, at
//! `run/<task_id>/worktree`) from the plan's recorded base commit, on a
//! fresh branch. This module owns only that mechanical step: it never reads
//! or writes plan content, and it never touches `binding.toml` itself (the
//! caller records the resulting path there through [`crate::store::Store`]).
//!
//! Every git subprocess this module spawns runs under an explicitly
//! constructed environment ([`GitEnv`]), mirroring `src/git.rs`'s
//! [`crate::git::GitInvocation`] discipline: the environment is cleared,
//! then `PATH` and `HOME` are set from caller-supplied values, and
//! `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, and `GIT_TERMINAL_PROMPT` are
//! pinned so worktree creation cannot be steered by the ambient process
//! environment or an operator's global git configuration. `GitInvocation`
//! itself is not reused: its fields are private to `src/git.rs` and it
//! exposes no generic "run this git command" entry point, only the
//! fact-capture calls T004 needed -- so this module mirrors the same
//! discipline with its own type instead. This module never reads
//! `std::env` itself (see `clippy.toml`'s `disallowed-methods` and
//! `REPO_INVARIANTS.md` RS-008); `git_binary`, `path`, and `home` reach it
//! only as fields on [`GitEnv`], supplied by the caller.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use thiserror::Error;

/// The explicit environment under which every git subprocess this module
/// spawns runs. See the module docs for why this exists instead of
/// inheriting the ambient process environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitEnv {
    git_binary: String,
    path: String,
    home: String,
}

impl GitEnv {
    /// Build a git invocation environment.
    ///
    /// `git_binary` is the path to (or bare name of) the git executable to
    /// run; `path` becomes the child process's `PATH`; `home` becomes its
    /// `HOME`.
    #[must_use]
    pub const fn new(git_binary: String, path: String, home: String) -> Self {
        Self {
            git_binary,
            path,
            home,
        }
    }
}

/// Failures creating or removing a worktree.
#[derive(Debug, Error)]
pub enum WorktreeError {
    /// A non-git filesystem operation (creating the worktree's parent
    /// directory) failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The git subprocess could not be spawned at all.
    #[error("failed to spawn `{command}`: {source}")]
    Spawn {
        /// The command that could not be spawned.
        command: String,
        /// The underlying spawn failure.
        source: std::io::Error,
    },
    /// The git subprocess ran and exited unsuccessfully.
    #[error("`{command}` failed: {stderr}")]
    CommandFailed {
        /// The command that failed.
        command: String,
        /// The command's captured standard error.
        stderr: String,
    },
}

/// Create a git worktree of the repository at `repo_start`, at
/// `worktree_path`, checked out at `base_commit` on a fresh `branch`.
///
/// `worktree_path` is expected to be outside `repo_start` (the caller —
/// `dispatch`, T009 — is responsible for that; this module does not verify
/// it). Its parent directory is created if it does not already exist; the
/// leaf itself must not exist (git creates it).
///
/// # Errors
///
/// Returns [`WorktreeError::Io`] when the worktree's parent directory
/// cannot be created, [`WorktreeError::Spawn`] when `git` cannot be
/// spawned, or [`WorktreeError::CommandFailed`] when `git worktree add`
/// exits unsuccessfully (for example, because `branch` already exists, or
/// `base_commit` does not resolve).
pub fn create(
    env: &GitEnv,
    repo_start: &Path,
    worktree_path: &Path,
    branch: &str,
    base_commit: &str,
) -> Result<(), WorktreeError> {
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| WorktreeError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let args: Vec<&std::ffi::OsStr> = vec![
        "worktree".as_ref(),
        "add".as_ref(),
        "--quiet".as_ref(),
        "-b".as_ref(),
        branch.as_ref(),
        worktree_path.as_ref(),
        base_commit.as_ref(),
    ];
    run_git(env, repo_start, &args)
}

/// Remove a worktree previously created by [`create`] and delete its
/// branch.
///
/// Never touches the operator plan or anything under the store; this is
/// purely a git-level cleanup, used by tests and the manual escape hatch.
///
/// # Errors
///
/// Returns [`WorktreeError::Spawn`] when `git` cannot be spawned, or
/// [`WorktreeError::CommandFailed`] when either `git worktree remove` or
/// `git branch -D` exits unsuccessfully.
pub fn remove(
    env: &GitEnv,
    repo_start: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<(), WorktreeError> {
    let remove_args: Vec<&std::ffi::OsStr> = vec![
        "worktree".as_ref(),
        "remove".as_ref(),
        "--force".as_ref(),
        worktree_path.as_ref(),
    ];
    run_git(env, repo_start, &remove_args)?;

    let branch_args: Vec<&std::ffi::OsStr> =
        vec!["branch".as_ref(), "-D".as_ref(), branch.as_ref()];
    run_git(env, repo_start, &branch_args)
}

fn run_git(env: &GitEnv, cwd: &Path, args: &[&std::ffi::OsStr]) -> Result<(), WorktreeError> {
    let command_desc = describe_command(env, args);
    let output = spawn_git(env, cwd, args).map_err(|source| WorktreeError::Spawn {
        command: command_desc.clone(),
        source,
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorktreeError::CommandFailed {
            command: command_desc,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn spawn_git(env: &GitEnv, cwd: &Path, args: &[&std::ffi::OsStr]) -> std::io::Result<Output> {
    Command::new(&env.git_binary)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", &env.path)
        .env("HOME", &env.home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
}

fn describe_command(env: &GitEnv, args: &[&std::ffi::OsStr]) -> String {
    let rendered_args = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} {rendered_args}", env.git_binary)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::Path;
    use std::process::Command;

    use super::{GitEnv, WorktreeError, create, remove};

    fn git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).args(args);
        crate::test_support::harden_git_command(&mut command, dir);
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("git {args:?} failed in {}", dir.display()).into())
        }
    }

    fn init_repo(dir: &Path) -> Result<String, Box<dyn Error>> {
        git(dir, &["init", "--quiet"])?;
        std::fs::write(dir.join("README.md"), "hello\n")?;
        git(dir, &["add", "README.md"])?;
        let commit_args = [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "initial commit",
        ];
        git(dir, &commit_args)?;
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).args(["rev-parse", "HEAD"]);
        crate::test_support::harden_git_command(&mut command, dir);
        let output = command.output()?;
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn test_env() -> GitEnv {
        // Reading `PATH` here (test code, not the library) is how a test
        // finds the real `git` binary; the library itself never does this
        // (see the module docs).
        #[allow(clippy::disallowed_methods)]
        let path = std::env::var("PATH").unwrap_or_default();
        GitEnv::new(
            "git".to_owned(),
            path,
            std::env::temp_dir().to_string_lossy().into_owned(),
        )
    }

    #[test]
    fn git_helper_reports_command_failures() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;
        let actual = git(tmp.path(), &["not-a-real-git-subcommand"]);
        assert!(actual.is_err());
        Ok(())
    }

    #[test]
    fn create_skips_parent_creation_when_the_worktree_path_has_none() -> Result<(), Box<dyn Error>>
    {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;
        let env = test_env();

        // `Path::new("")` has no parent at all, so `create` must skip
        // straight to invoking git (which then fails, since "" is not a
        // usable worktree path) rather than attempting to create a parent
        // directory for a path that has none.
        let actual = create(
            &env,
            repo_dir.path(),
            Path::new(""),
            "silent-critic/plan-1/T001",
            "HEAD",
        );

        assert!(matches!(actual, Err(WorktreeError::CommandFailed { .. })));
        Ok(())
    }

    #[test]
    fn create_checks_out_the_base_commit_on_a_fresh_branch() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        let base_commit = init_repo(repo_dir.path())?;
        let scratch = tempfile::tempdir()?;
        let worktree_path = scratch.path().join("run").join("T001").join("worktree");
        let env = test_env();

        let branch = "silent-critic/plan-1/T001";
        create(&env, repo_dir.path(), &worktree_path, branch, &base_commit)?;

        assert!(worktree_path.is_dir());
        assert!(worktree_path.join("README.md").is_file());
        let mut head_command = Command::new("git");
        head_command
            .arg("-C")
            .arg(&worktree_path)
            .args(["rev-parse", "HEAD"]);
        crate::test_support::harden_git_command(&mut head_command, &worktree_path);
        let head = head_command.output()?;
        assert_eq!(base_commit, String::from_utf8(head.stdout)?.trim());

        let mut branch_command = Command::new("git");
        branch_command
            .arg("-C")
            .arg(&worktree_path)
            .args(["branch", "--show-current"]);
        crate::test_support::harden_git_command(&mut branch_command, &worktree_path);
        let branch = branch_command.output()?;
        assert_eq!(
            "silent-critic/plan-1/T001",
            String::from_utf8(branch.stdout)?.trim()
        );

        remove(
            &env,
            repo_dir.path(),
            &worktree_path,
            "silent-critic/plan-1/T001",
        )?;
        assert!(!worktree_path.exists());
        Ok(())
    }

    #[test]
    fn create_reports_command_failures_for_an_unresolvable_commit() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;
        let scratch = tempfile::tempdir()?;
        let worktree_path = scratch.path().join("worktree");
        let env = test_env();

        let actual = create(
            &env,
            repo_dir.path(),
            &worktree_path,
            "silent-critic/plan-1/T001",
            "0000000000000000000000000000000000000000",
        );

        assert!(matches!(actual, Err(WorktreeError::CommandFailed { .. })));
        Ok(())
    }

    #[test]
    fn create_reports_spawn_failures() -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;
        let scratch = tempfile::tempdir()?;
        let worktree_path = scratch.path().join("worktree");
        let env = GitEnv::new(
            "silent-critic-test-nonexistent-git-binary".to_owned(),
            String::new(),
            String::new(),
        );

        let actual = create(
            &env,
            repo_dir.path(),
            &worktree_path,
            "silent-critic/plan-1/T001",
            "HEAD",
        );

        assert!(matches!(actual, Err(WorktreeError::Spawn { .. })));
        Ok(())
    }

    #[test]
    fn remove_reports_command_failures_for_a_worktree_that_does_not_exist()
    -> Result<(), Box<dyn Error>> {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;
        let env = test_env();

        let actual = remove(
            &env,
            repo_dir.path(),
            Path::new("/does/not/exist"),
            "no-such-branch",
        );

        assert!(matches!(actual, Err(WorktreeError::CommandFailed { .. })));
        Ok(())
    }

    #[test]
    fn create_reports_io_failures_when_the_parent_cannot_be_created() -> Result<(), Box<dyn Error>>
    {
        let repo_dir = tempfile::tempdir()?;
        init_repo(repo_dir.path())?;
        let scratch = tempfile::tempdir()?;
        // A plain file where the worktree's parent directory needs to go
        // blocks `create_dir_all`.
        let blocker = scratch.path().join("blocker");
        std::fs::write(&blocker, "not a directory")?;
        let worktree_path = blocker.join("worktree");
        let env = test_env();

        let actual = create(
            &env,
            repo_dir.path(),
            &worktree_path,
            "silent-critic/plan-1/T001",
            "HEAD",
        );

        assert!(matches!(actual, Err(WorktreeError::Io { .. })));
        Ok(())
    }
}
