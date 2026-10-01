//! Tool-side git fact capture (T004).
//!
//! Everything in [`GitFacts`] is read from a git worktree by this module,
//! never accepted as an argument describing what changed. An orchestrator
//! that narrates its own diff is an orchestrator whose report is worth
//! exactly as much as its incentive to be accurate; this module exists so
//! the record carries evidence instead.
//!
//! Every git subprocess this module spawns runs under an explicitly
//! constructed environment ([`GitInvocation`]): the environment is cleared,
//! then `PATH` and `HOME` are set from caller-supplied values, and
//! `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, and `GIT_TERMINAL_PROMPT` are
//! pinned so the capture cannot be steered by the ambient process
//! environment or an operator's global git configuration. This module never
//! reads `std::env` itself (see `clippy.toml`'s `disallowed-methods` and
//! `REPO_INVARIANTS.md` RS-008); `PATH` and `HOME` reach it only as fields
//! on [`GitInvocation`], supplied by the caller.

use crate::model::{BaseRef, ChangedPath, Residual, TaskId};
use std::path::Path;
use std::process::{Command, ExitStatus, Output};
use thiserror::Error;

// ---------------------------------------------------------------------
// Invocation environment
// ---------------------------------------------------------------------

/// The explicit environment under which every git subprocess this module
/// spawns runs.
///
/// This module never reads the process environment itself; `git_binary`,
/// `path`, and `home` are the only inputs a caller can use to steer how git
/// is invoked, and none of them describe *what changed* -- that is read
/// from the repository, not supplied here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInvocation {
    git_binary: String,
    path: String,
    home: String,
}

impl GitInvocation {
    /// Build a git invocation environment.
    ///
    /// `git_binary` is the path to (or bare name of) the git executable to
    /// run. `path` becomes the child process's `PATH`; `home` becomes its
    /// `HOME` (a caller typically passes the worktree itself, so git never
    /// consults a real user's global configuration).
    ///
    /// Takes owned `String`s rather than `impl Into<String>` so this
    /// constructor is monomorphized exactly once in the library, instead of
    /// once per call-site type -- including inside binaries that never
    /// execute it.
    #[must_use]
    pub const fn new(git_binary: String, path: String, home: String) -> Self {
        Self {
            git_binary,
            path,
            home,
        }
    }
}

// ---------------------------------------------------------------------
// Facts
// ---------------------------------------------------------------------

/// The git facts captured for a run, read entirely from the repository.
///
/// Path lists are sorted lexicographically so the same worktree and base
/// ref produce identical facts on every capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    base_ref: BaseRef,
    base_commit: String,
    head_commit: String,
    changed_paths: Vec<ChangedPath>,
    deleted_paths: Vec<ChangedPath>,
    untracked_paths: Vec<ChangedPath>,
    diffstat: String,
    diff: String,
}

impl GitFacts {
    /// The base ref as given by the caller.
    #[must_use]
    pub const fn base_ref(&self) -> &BaseRef {
        &self.base_ref
    }

    /// The commit the base ref resolved to.
    #[must_use]
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    /// The commit `HEAD` resolved to.
    #[must_use]
    pub fn head_commit(&self) -> &str {
        &self.head_commit
    }

    /// Tracked paths added, modified, or renamed relative to the base
    /// commit, sorted lexicographically. A renamed path appears once, under
    /// its new name.
    #[must_use]
    pub fn changed_paths(&self) -> &[ChangedPath] {
        &self.changed_paths
    }

    /// Tracked paths deleted relative to the base commit, sorted
    /// lexicographically.
    #[must_use]
    pub fn deleted_paths(&self) -> &[ChangedPath] {
        &self.deleted_paths
    }

    /// Untracked paths present in the worktree, sorted lexicographically.
    #[must_use]
    pub fn untracked_paths(&self) -> &[ChangedPath] {
        &self.untracked_paths
    }

    /// The diffstat for tracked changes relative to the base commit
    /// (`git diff --stat <base>`). Untracked files do not contribute to
    /// this summary; see [`GitFacts::diff`] for how they appear in the
    /// judge-facing diff text.
    #[must_use]
    pub fn diffstat(&self) -> &str {
        &self.diffstat
    }

    /// The full diff text the judge reads: `git diff <base>` for tracked
    /// changes, followed by each untracked file's content presented as a
    /// pure addition (`git diff --no-index -- /dev/null <path>`), one per
    /// untracked path in lexicographic order.
    #[must_use]
    pub fn diff(&self) -> &str {
        &self.diff
    }

    /// The union of [`GitFacts::changed_paths`], [`GitFacts::deleted_paths`],
    /// and [`GitFacts::untracked_paths`], deduplicated and sorted
    /// lexicographically -- every path this task touched, by any means.
    #[must_use]
    pub fn all_changed_paths(&self) -> Vec<ChangedPath> {
        let mut all: Vec<ChangedPath> = self
            .changed_paths
            .iter()
            .chain(self.deleted_paths.iter())
            .chain(self.untracked_paths.iter())
            .cloned()
            .collect();
        sort_and_dedup_changed_paths(&mut all);
        all
    }
}

/// Lexicographic ordering on [`ChangedPath`] by its raw string, shared by
/// every sort site in this module so the comparator is defined once.
fn cmp_changed_path(a: &ChangedPath, b: &ChangedPath) -> std::cmp::Ordering {
    a.as_str().cmp(b.as_str())
}

/// Equality on [`ChangedPath`] by its raw string, shared by every dedup site
/// in this module so the comparator is defined once. Takes `&mut` per
/// [`Vec::dedup_by`]'s signature, though it only reads through the
/// references.
fn eq_changed_path(a: &mut ChangedPath, b: &mut ChangedPath) -> bool {
    a.as_str() == b.as_str()
}

/// Sort `paths` lexicographically and remove adjacent duplicates, in place.
fn sort_and_dedup_changed_paths(paths: &mut Vec<ChangedPath>) {
    paths.sort_by(cmp_changed_path);
    paths.dedup_by(eq_changed_path);
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

/// A failure capturing git facts.
///
/// Every variant is typed: a git failure never produces an empty
/// [`GitFacts`], which would read as "nothing changed".
#[derive(Debug, Error)]
pub enum GitError {
    /// The git subprocess could not be spawned at all.
    #[error("failed to spawn `{command}`: {message}")]
    Spawn {
        /// The command that could not be spawned.
        command: String,
        /// The underlying spawn failure, rendered as text.
        message: String,
    },

    /// The git subprocess ran and exited with a non-zero (or otherwise
    /// unsuccessful) status.
    #[error("`{command}` failed ({status}): {stderr}")]
    CommandFailed {
        /// The command that failed.
        command: String,
        /// The exit status git reported.
        status: ExitStatus,
        /// The command's captured standard error.
        stderr: String,
    },

    /// The git subprocess produced output that was not valid UTF-8.
    #[error("`{command}` produced output that was not valid UTF-8")]
    InvalidUtf8 {
        /// The command whose output could not be decoded.
        command: String,
    },
}

// ---------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------

/// Capture the git facts for `worktree` relative to `base_ref`.
///
/// This is a pure function of `(worktree, base_ref, invocation)`: it reads
/// only the repository at `worktree` through the git binary named by
/// `invocation`, and returns the same facts for the same inputs and
/// repository state. No fact in the result is supplied by the caller.
///
/// # Errors
///
/// Returns [`GitError`] when `base_ref` does not resolve to a commit,
/// `worktree` is not a git repository, or any underlying git invocation
/// fails or produces output that is not valid UTF-8. Capture never returns
/// an empty or default [`GitFacts`] in place of an error.
pub fn capture(
    worktree: &Path,
    base_ref: &BaseRef,
    invocation: &GitInvocation,
) -> Result<GitFacts, GitError> {
    let base_commit = resolve_commit(worktree, invocation, base_ref.as_str())?;
    let head_commit = resolve_commit(worktree, invocation, "HEAD")?;

    let name_status_args = ["diff", "--name-status", "-z", base_commit.as_str()];
    let name_status = run_git(invocation, worktree, &name_status_args)?;
    let name_status_text = to_utf8(name_status, "git diff --name-status")?;
    let (changed_paths, deleted_paths) = parse_name_status(&name_status_text);

    let status_args = ["status", "--porcelain=v1", "-z", "--untracked-files=all"];
    let status = run_git(invocation, worktree, &status_args)?;
    let status_text = to_utf8(status, "git status")?;
    let untracked_paths = parse_untracked_paths(&status_text);

    let diffstat_bytes = run_git(invocation, worktree, &["diff", "--stat", &base_commit])?;
    let diffstat = to_utf8(diffstat_bytes, "git diff --stat")?;

    let diff_bytes = run_git(invocation, worktree, &["diff", &base_commit])?;
    let mut diff = to_utf8(diff_bytes, "git diff")?;

    for path in &untracked_paths {
        let addition = diff_untracked_as_addition(worktree, invocation, path.as_str())?;
        diff.push_str(&addition);
    }

    Ok(GitFacts {
        base_ref: base_ref.clone(),
        base_commit,
        head_commit,
        changed_paths,
        deleted_paths,
        untracked_paths,
        diffstat,
        diff,
    })
}

/// Compute the uncovered-changed-scope residual for a task.
///
/// Takes the whole [`GitFacts`] rather than a caller-chosen path list: the
/// set a task "actually changed" is [`GitFacts::all_changed_paths`] -- the
/// union of changed, deleted, and untracked paths -- not any single one of
/// those lists. A caller passing only [`GitFacts::changed_paths`] would
/// silently miss a task that deleted or left behind files outside its
/// declared scope; taking `&GitFacts` here removes that choice instead of
/// documenting it as a caveat.
///
/// A declared entry in `declared_likely_modify` covers exactly the path it
/// names -- an exact-path match, not a directory prefix.
///
/// `declared_likely_modify` is `Option<&[String]>`, not `&[String]`,
/// because "the task declares no file list at all" (`files` absent from
/// the plan -- every `mode: single` plan, per decision 4) and "the task
/// declares an empty file list" are different claims: the former means
/// scope was never stated, the latter means the task claims it will touch
/// nothing. Only the latter should flag every changed path as uncovered;
/// conflating them treated "nothing declared" as "declared nothing" and
/// flagged every changed path on every single-mode plan, which is the
/// defect decision 4 fixes. When `declared_likely_modify` is `None` this
/// returns `None` unconditionally -- no residual at all, deliberately, not
/// a distinct informational residual (documented in `REPO_INVARIANTS.md`-
/// adjacent module docs and the plan's Decision section): an
/// undeclared-scope task carries no signal here for
/// `Ledger::unresolved_items`/`measure` to even see, so it cannot
/// contribute to `operator_attention_required` by itself.
///
/// Returns `None` when every changed path is covered; otherwise returns
/// [`Residual::UncoveredChangedScope`] carrying the uncovered paths, sorted
/// lexicographically.
#[must_use]
pub fn uncovered_changed_scope(
    task_id: TaskId,
    facts: &GitFacts,
    declared_likely_modify: Option<&[String]>,
) -> Option<Residual> {
    let declared_likely_modify = declared_likely_modify?;
    uncovered_changed_scope_over(task_id, &facts.all_changed_paths(), declared_likely_modify)
}

/// Pure set-difference behind [`uncovered_changed_scope`], over an explicit
/// path list rather than a [`GitFacts`] -- kept separate so the comparator
/// behavior (sorting, deduplication) can be exercised directly against
/// hand-built path lists without building a real git repository.
fn uncovered_changed_scope_over(
    task_id: TaskId,
    changed_paths: &[ChangedPath],
    declared_likely_modify: &[String],
) -> Option<Residual> {
    let declared: std::collections::BTreeSet<&str> =
        declared_likely_modify.iter().map(String::as_str).collect();

    let mut uncovered: Vec<ChangedPath> = changed_paths
        .iter()
        .filter(|path| !declared.contains(path.as_str()))
        .cloned()
        .collect();

    if uncovered.is_empty() {
        return None;
    }

    sort_and_dedup_changed_paths(&mut uncovered);

    Some(Residual::UncoveredChangedScope {
        task_id,
        paths: uncovered,
    })
}

// ---------------------------------------------------------------------
// git subprocess plumbing
// ---------------------------------------------------------------------

fn spawn_git(
    invocation: &GitInvocation,
    worktree: &Path,
    args: &[&str],
) -> Result<Output, GitError> {
    Command::new(&invocation.git_binary)
        .args(args)
        .current_dir(worktree)
        .env_clear()
        .env("PATH", &invocation.path)
        .env("HOME", &invocation.home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|source| GitError::Spawn {
            command: describe_command(invocation, args),
            message: source.to_string(),
        })
}

/// Run a git command, treating any non-zero exit as a failure.
fn run_git(
    invocation: &GitInvocation,
    worktree: &Path,
    args: &[&str],
) -> Result<Vec<u8>, GitError> {
    let output = spawn_git(invocation, worktree, args)?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::CommandFailed {
            command: describe_command(invocation, args),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Resolve `ref_name` to the commit it names.
fn resolve_commit(
    worktree: &Path,
    invocation: &GitInvocation,
    ref_name: &str,
) -> Result<String, GitError> {
    let commit_ish = format!("{ref_name}^{{commit}}");
    let output = run_git(
        invocation,
        worktree,
        &["rev-parse", "--verify", &commit_ish],
    )?;
    let text = to_utf8(output, "git rev-parse --verify")?;
    Ok(text.trim().to_string())
}

/// Diff a single untracked file against `/dev/null` so its content appears
/// as a pure addition. `git diff --no-index` exits `0` when the two sides
/// are identical (an empty untracked file) and `1` when they differ (the
/// normal case for a real untracked file); neither is a failure. Any other
/// exit status is a genuine git failure.
fn diff_untracked_as_addition(
    worktree: &Path,
    invocation: &GitInvocation,
    path: &str,
) -> Result<String, GitError> {
    let args = ["diff", "--no-index", "--", "/dev/null", path];
    let output = spawn_git(invocation, worktree, &args)?;
    classify_no_index_output(output, describe_command(invocation, &args))
}

/// Interpret the result of `git diff --no-index`, which -- unlike every
/// other invocation this module makes -- treats exit status `1` (the two
/// sides differ) as success rather than failure. Split out as a pure
/// function of an already-captured [`Output`] so the exit-status
/// classification can be exercised without spawning a process.
fn classify_no_index_output(output: Output, command: String) -> Result<String, GitError> {
    match output.status.code() {
        Some(0 | 1) => to_utf8(output.stdout, "git diff --no-index"),
        _ => Err(GitError::CommandFailed {
            command,
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
    }
}

fn describe_command(invocation: &GitInvocation, args: &[&str]) -> String {
    let mut command = invocation.git_binary.clone();
    for arg in args {
        command.push(' ');
        command.push_str(arg);
    }
    command
}

fn to_utf8(bytes: Vec<u8>, command: &str) -> Result<String, GitError> {
    String::from_utf8(bytes).map_err(|_source| GitError::InvalidUtf8 {
        command: command.to_string(),
    })
}

// ---------------------------------------------------------------------
// Parsing `git diff --name-status -z` and `git status --porcelain=v1 -z`
// ---------------------------------------------------------------------

/// Parse `git diff --name-status -z <base>` output into (changed, deleted).
///
/// Records are NUL-separated: `<status>\0<path>\0` for an ordinary change,
/// or `<status>\0<old-path>\0<new-path>\0` for a rename or copy (`status`
/// starting with `R` or `C`). A rename or copy contributes only its new
/// path, to `changed`.
fn parse_name_status(text: &str) -> (Vec<ChangedPath>, Vec<ChangedPath>) {
    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    let mut tokens = text.split('\0').filter(|token| !token.is_empty());

    while let Some(status) = tokens.next() {
        if status.starts_with('R') || status.starts_with('C') {
            let _old_path = tokens.next();
            if let Some(new_path) = tokens.next() {
                changed.push(ChangedPath::new(new_path));
            }
        } else if let Some(path) = tokens.next() {
            if status.starts_with('D') {
                deleted.push(ChangedPath::new(path));
            } else {
                changed.push(ChangedPath::new(path));
            }
        }
    }

    changed.sort_by(cmp_changed_path);
    deleted.sort_by(cmp_changed_path);
    (changed, deleted)
}

/// Parse `git status --porcelain=v1 -z --untracked-files=all` output,
/// returning only the untracked (`??`) paths.
///
/// Records are NUL-separated: `<XY> <path>\0`, with an extra
/// `\0<orig-path>` when `X` or `Y` is `R` or `C` (renamed or copied,
/// consumed here to keep subsequent records aligned but otherwise unused --
/// tracked changes are read from `git diff --name-status` instead).
fn parse_untracked_paths(text: &str) -> Vec<ChangedPath> {
    let mut untracked = Vec::new();
    let mut tokens = text.split('\0').filter(|token| !token.is_empty());

    while let Some(entry) = tokens.next() {
        let mut chars = entry.chars();
        let index_status = chars.next();
        let worktree_status = chars.next();
        let rest = chars.as_str();
        let path = rest.strip_prefix(' ').unwrap_or(rest);

        if index_status == Some('?') && worktree_status == Some('?') {
            untracked.push(ChangedPath::new(path));
        } else if index_status == Some('R')
            || index_status == Some('C')
            || worktree_status == Some('R')
            || worktree_status == Some('C')
        {
            let _orig_path = tokens.next();
        }
    }

    untracked.sort_by(cmp_changed_path);
    untracked
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        GitError, GitFacts, GitInvocation, capture, classify_no_index_output, parse_name_status,
        parse_untracked_paths, to_utf8, uncovered_changed_scope, uncovered_changed_scope_over,
    };
    use crate::model::{BaseRef, ChangedPath, Residual, TaskId};
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus, Output};
    use tempfile::TempDir;

    /// Build a [`GitInvocation`] for tests. Reading `PATH` here stands in
    /// for the process edge that will supply it in production (see
    /// `REPO_INVARIANTS.md` RS-008): this is test harness code acting as
    /// the caller of the library, not the library itself.
    #[allow(
        clippy::disallowed_methods,
        reason = "test harness stands in for the caller-side process edge; the library under test never reads PATH itself"
    )]
    fn test_invocation(home: &Path) -> GitInvocation {
        // `unwrap_or_default` rather than `unwrap_or_else` with a fallback
        // closure: PATH is always set in practice, so a fallback closure
        // would be dead code no test can reach without faking process
        // environment absence, which is exactly what this module must not
        // do outside this test-harness function.
        let path = std::env::var("PATH").unwrap_or_default();
        GitInvocation::new("git".to_string(), path, home.to_string_lossy().into_owned())
    }

    /// Run a `git` command against `repo` with a clean, deterministic
    /// operator identity, for building test fixtures (not the code under
    /// test, which goes through [`super::spawn_git`]).
    fn git(repo: &Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(repo)
            .env_clear()
            .env("PATH", test_invocation(repo).path)
            .env("HOME", repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()?;
        assert!(status.success(), "git {args:?} failed with {status}");
        Ok(())
    }

    fn init_repo() -> Result<(TempDir, PathBuf), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let repo = dir.path().to_path_buf();
        git(&repo, &["-c", "init.defaultBranch=main", "init", "-q"])?;
        Ok((dir, repo))
    }

    fn write(
        repo: &Path,
        relative: &str,
        contents: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let full = repo.join(relative);
        let parent = full.parent().unwrap_or(repo).to_path_buf();
        std::fs::create_dir_all(parent)?;
        std::fs::write(&full, contents)?;
        Ok(())
    }

    fn commit_all(repo: &Path, message: &str) -> Result<String, Box<dyn std::error::Error>> {
        git(repo, &["add", "-A"])?;
        git(repo, &["commit", "-q", "-m", message])?;
        rev_parse(repo, "HEAD")
    }

    fn rev_parse(repo: &Path, rev: &str) -> Result<String, Box<dyn std::error::Error>> {
        let mut command = Command::new("git");
        command.args(["rev-parse", rev]).current_dir(repo);
        crate::test_support::harden_git_command(&mut command, repo);
        let output = command.output()?;
        assert!(output.status.success(), "git rev-parse {rev} failed");
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    #[test]
    fn captures_changed_paths_and_diffstat_against_base() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, repo) = init_repo()?;
        write(&repo, "src/a.txt", "line1\nline2\n")?;
        let base = commit_all(&repo, "base")?;

        write(&repo, "src/a.txt", "line1\nline2 changed\n")?;
        git(&repo, &["add", "-A"])?;
        git(&repo, &["commit", "-q", "-m", "modify a"])?;

        let invocation = test_invocation(&repo);
        let facts = capture(&repo, &BaseRef::new(base.clone()), &invocation)?;

        assert_eq!(base, facts.base_commit());
        assert_eq!(
            vec![ChangedPath::new("src/a.txt")],
            facts.changed_paths().to_vec()
        );
        assert!(facts.deleted_paths().is_empty());
        assert!(facts.untracked_paths().is_empty());
        assert_eq!(
            " src/a.txt | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)\n",
            facts.diffstat()
        );
        assert!(facts.diff().contains("line2 changed"));
        Ok(())
    }

    #[test]
    fn captures_renamed_deleted_untracked_and_space_paths() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, repo) = init_repo()?;
        write(&repo, "keep.txt", &"x\n".repeat(20))?;
        write(&repo, "old_name.txt", &"y\n".repeat(20))?;
        write(&repo, "gone.txt", "will be deleted\n")?;
        let base = commit_all(&repo, "base")?;

        git(&repo, &["mv", "old_name.txt", "new_name.txt"])?;
        std::fs::remove_file(repo.join("gone.txt"))?;
        write(&repo, "with space/file name.txt", "untracked content\n")?;

        let invocation = test_invocation(&repo);
        let facts = capture(&repo, &BaseRef::new(base), &invocation)?;

        // A rename should appear once, under its new name.
        assert_eq!(
            vec![ChangedPath::new("new_name.txt")],
            facts.changed_paths().to_vec()
        );
        assert_eq!(
            vec![ChangedPath::new("gone.txt")],
            facts.deleted_paths().to_vec()
        );
        assert_eq!(
            vec![ChangedPath::new("with space/file name.txt")],
            facts.untracked_paths().to_vec()
        );
        // Untracked file content should appear in the diff as an addition.
        assert!(facts.diff().contains("untracked content"));
        Ok(())
    }

    #[test]
    fn all_changed_paths_is_the_sorted_union() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, repo) = init_repo()?;
        write(&repo, "b.txt", "b\n")?;
        write(&repo, "d.txt", "d\n")?;
        let base = commit_all(&repo, "base")?;

        write(&repo, "b.txt", "b changed\n")?;
        std::fs::remove_file(repo.join("d.txt"))?;
        write(&repo, "a-untracked.txt", "untracked\n")?;

        let invocation = test_invocation(&repo);
        let facts = capture(&repo, &BaseRef::new(base), &invocation)?;

        assert_eq!(
            vec![
                ChangedPath::new("a-untracked.txt"),
                ChangedPath::new("b.txt"),
                ChangedPath::new("d.txt"),
            ],
            facts.all_changed_paths()
        );
        Ok(())
    }

    /// Decision 4: `None` means the task declared no `files` block at all
    /// (every `mode: single` plan), which is a different claim than
    /// "declared an empty scope" and must never flag every changed path as
    /// uncovered.
    #[test]
    fn uncovered_changed_scope_is_none_when_scope_is_undeclared()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, repo) = init_repo()?;
        write(&repo, "src/anything.rs", "anything\n")?;
        let base = commit_all(&repo, "base")?;
        write(&repo, "src/anything.rs", "changed\n")?;

        let invocation = test_invocation(&repo);
        let facts = capture(&repo, &BaseRef::new(base), &invocation)?;
        assert!(!facts.changed_paths().is_empty());

        assert_eq!(
            None,
            uncovered_changed_scope(TaskId::new("task-1"), &facts, None)
        );
        Ok(())
    }

    #[test]
    fn uncovered_changed_scope_over_flags_paths_outside_declared_scope() {
        // Two uncovered paths, given out of order, so this also exercises the
        // sort/dedup comparator closures with a real two-element comparison.
        let changed = vec![
            ChangedPath::new("src/declared.rs"),
            ChangedPath::new("src/zzz_surprise.rs"),
            ChangedPath::new("src/aaa_surprise.rs"),
        ];
        let declared = vec!["src/declared.rs".to_string()];

        let residual = uncovered_changed_scope_over(TaskId::new("task-1"), &changed, &declared);

        assert_eq!(
            Some(Residual::UncoveredChangedScope {
                task_id: TaskId::new("task-1"),
                paths: vec![
                    ChangedPath::new("src/aaa_surprise.rs"),
                    ChangedPath::new("src/zzz_surprise.rs"),
                ],
            }),
            residual
        );
    }

    #[test]
    fn uncovered_changed_scope_over_is_none_when_everything_is_declared() {
        let changed = vec![ChangedPath::new("src/declared.rs")];
        let declared = vec!["src/declared.rs".to_string(), "src/unused.rs".to_string()];

        assert_eq!(
            None,
            uncovered_changed_scope_over(TaskId::new("task-1"), &changed, &declared)
        );
    }

    #[test]
    fn uncovered_changed_scope_over_treats_declared_directories_as_non_matching() {
        // Declared entries are exact paths, not directory prefixes: a
        // declared "src/" does not cover "src/file.rs".
        let changed = vec![ChangedPath::new("src/file.rs")];
        let declared = vec!["src/".to_string()];

        let residual = uncovered_changed_scope_over(TaskId::new("task-1"), &changed, &declared);

        assert_eq!(
            Some(Residual::UncoveredChangedScope {
                task_id: TaskId::new("task-1"),
                paths: vec![ChangedPath::new("src/file.rs")],
            }),
            residual
        );
    }

    #[test]
    fn uncovered_changed_scope_flags_deleted_only_drift() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, repo) = init_repo()?;
        write(&repo, "src/declared.rs", "declared\n")?;
        write(&repo, "src/gone.rs", "will be deleted\n")?;
        let base = commit_all(&repo, "base")?;

        // The declared file is untouched; only an undeclared file is
        // deleted. `changed_paths()` alone would see nothing here.
        std::fs::remove_file(repo.join("src/gone.rs"))?;

        let invocation = test_invocation(&repo);
        let facts = capture(&repo, &BaseRef::new(base), &invocation)?;
        assert!(facts.changed_paths().is_empty());
        let declared = vec!["src/declared.rs".to_string()];

        let residual = uncovered_changed_scope(TaskId::new("task-1"), &facts, Some(&declared));

        assert_eq!(
            Some(Residual::UncoveredChangedScope {
                task_id: TaskId::new("task-1"),
                paths: vec![ChangedPath::new("src/gone.rs")],
            }),
            residual
        );
        Ok(())
    }

    #[test]
    fn uncovered_changed_scope_flags_untracked_only_drift() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, repo) = init_repo()?;
        write(&repo, "src/declared.rs", "declared\n")?;
        let base = commit_all(&repo, "base")?;

        // The declared file is untouched; only an undeclared file is left
        // behind, untracked. `changed_paths()` alone would see nothing here.
        write(&repo, "src/surprise.rs", "not declared\n")?;

        let invocation = test_invocation(&repo);
        let facts = capture(&repo, &BaseRef::new(base), &invocation)?;
        assert!(facts.changed_paths().is_empty());
        let declared = vec!["src/declared.rs".to_string()];

        let residual = uncovered_changed_scope(TaskId::new("task-1"), &facts, Some(&declared));

        assert_eq!(
            Some(Residual::UncoveredChangedScope {
                task_id: TaskId::new("task-1"),
                paths: vec![ChangedPath::new("src/surprise.rs")],
            }),
            residual
        );
        Ok(())
    }

    #[test]
    fn uncovered_changed_scope_covers_a_declared_path_that_was_deleted()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, repo) = init_repo()?;
        write(&repo, "src/declared.rs", "declared\n")?;
        let base = commit_all(&repo, "base")?;

        // The task deleted exactly the path it declared -- deletion is a
        // legitimate way to touch a declared path, not drift.
        std::fs::remove_file(repo.join("src/declared.rs"))?;

        let invocation = test_invocation(&repo);
        let facts = capture(&repo, &BaseRef::new(base), &invocation)?;
        assert_eq!(
            vec![ChangedPath::new("src/declared.rs")],
            facts.deleted_paths().to_vec()
        );
        let declared = vec!["src/declared.rs".to_string()];

        assert_eq!(
            None,
            uncovered_changed_scope(TaskId::new("task-1"), &facts, Some(&declared))
        );
        Ok(())
    }

    #[test]
    fn capture_returns_typed_error_when_base_ref_does_not_resolve()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, repo) = init_repo()?;
        write(&repo, "a.txt", "a\n")?;
        commit_all(&repo, "base")?;

        let invocation = test_invocation(&repo);
        let result = capture(&repo, &BaseRef::new("does-not-exist"), &invocation);

        assert!(matches!(
            &result,
            Err(GitError::CommandFailed { command, .. }) if command.contains("rev-parse")
        ));
        Ok(())
    }

    #[test]
    fn capture_returns_typed_error_for_a_non_repository_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = TempDir::new()?;
        let invocation = test_invocation(dir.path());

        let result = capture(dir.path(), &BaseRef::new("HEAD"), &invocation);

        assert!(matches!(result, Err(GitError::CommandFailed { .. })));
        Ok(())
    }

    #[test]
    fn capture_returns_typed_error_when_git_binary_cannot_be_spawned()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, repo) = init_repo()?;
        write(&repo, "a.txt", "a\n")?;
        commit_all(&repo, "base")?;

        let invocation = GitInvocation::new(
            "definitely-not-a-real-git-binary".to_string(),
            "/usr/bin:/bin".to_string(),
            repo.to_string_lossy().into_owned(),
        );

        let result = capture(&repo, &BaseRef::new("HEAD"), &invocation);

        assert!(matches!(result, Err(GitError::Spawn { .. })));
        Ok(())
    }

    #[test]
    fn capture_is_deterministic_across_repeated_captures() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, repo) = init_repo()?;
        write(&repo, "z.txt", "z\n")?;
        write(&repo, "a.txt", "a\n")?;
        let base = commit_all(&repo, "base")?;

        write(&repo, "z.txt", "z changed\n")?;
        write(&repo, "untracked.txt", "untracked\n")?;

        let invocation = test_invocation(&repo);
        let first = capture(&repo, &BaseRef::new(base.clone()), &invocation)?;
        let second = capture(&repo, &BaseRef::new(base), &invocation)?;

        assert_eq!(first, second);
        Ok(())
    }

    #[test]
    fn parse_name_status_handles_add_modify_delete_and_rename() {
        // Two deleted entries, given out of order, so this also exercises the
        // `deleted` sort comparator with a real two-element comparison.
        let text = "A\0added.txt\0M\0modified.txt\0D\0z_deleted.txt\0D\0a_deleted.txt\0R100\0old.txt\0new.txt\0";

        let (changed, deleted) = parse_name_status(text);

        assert_eq!(
            vec![
                ChangedPath::new("added.txt"),
                ChangedPath::new("modified.txt"),
                ChangedPath::new("new.txt"),
            ],
            changed
        );
        assert_eq!(
            vec![
                ChangedPath::new("a_deleted.txt"),
                ChangedPath::new("z_deleted.txt"),
            ],
            deleted
        );
    }

    #[test]
    fn parse_name_status_handles_empty_input() {
        let (changed, deleted) = parse_name_status("");
        assert!(changed.is_empty());
        assert!(deleted.is_empty());
    }

    #[test]
    fn parse_name_status_ignores_a_trailing_status_with_no_path() {
        let (changed, deleted) = parse_name_status("M");
        assert!(changed.is_empty());
        assert!(deleted.is_empty());
    }

    #[test]
    fn parse_untracked_paths_ignores_tracked_entries_including_renames() {
        // Two untracked entries, given out of order, so this also exercises
        // the `untracked` sort comparator with a real two-element comparison.
        let text = "R  new.txt\0old.txt\0?? z_untracked.txt\0?? a_untracked.txt\0 M tracked.txt\0";

        let untracked = parse_untracked_paths(text);

        assert_eq!(
            vec![
                ChangedPath::new("a_untracked.txt"),
                ChangedPath::new("z_untracked.txt"),
            ],
            untracked
        );
    }

    #[test]
    fn to_utf8_rejects_invalid_utf8() {
        let result = to_utf8(vec![0xFF, 0xFE], "some command");
        assert!(matches!(result, Err(GitError::InvalidUtf8 { .. })));
    }

    #[test]
    fn every_git_error_variant_displays_a_message() {
        let spawn = GitError::Spawn {
            command: "git rev-parse".to_string(),
            message: "no such file".to_string(),
        };
        assert!(spawn.to_string().contains("git rev-parse"));

        let command_failed = GitError::CommandFailed {
            command: "git diff".to_string(),
            status: ExitStatus::from_raw(1 << 8),
            stderr: "fatal: boom".to_string(),
        };
        assert!(command_failed.to_string().contains("fatal: boom"));

        let invalid_utf8 = GitError::InvalidUtf8 {
            command: "git status".to_string(),
        };
        assert!(invalid_utf8.to_string().contains("git status"));
    }

    #[test]
    fn to_utf8_accepts_valid_utf8() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!("hello", to_utf8(b"hello".to_vec(), "some command")?);
        Ok(())
    }

    #[test]
    fn classify_no_index_output_accepts_exit_zero_and_one() -> Result<(), Box<dyn std::error::Error>>
    {
        for code in [0, 1] {
            let output = Output {
                status: ExitStatus::from_raw(code << 8),
                stdout: b"diff text".to_vec(),
                stderr: Vec::new(),
            };
            assert_eq!(
                "diff text",
                classify_no_index_output(output, "git diff --no-index".to_string())?
            );
        }
        Ok(())
    }

    #[test]
    fn classify_no_index_output_rejects_other_exit_codes() {
        let output = Output {
            status: ExitStatus::from_raw(128 << 8),
            stdout: Vec::new(),
            stderr: b"fatal: cannot hash".to_vec(),
        };

        let result = classify_no_index_output(output, "git diff --no-index".to_string());

        assert!(matches!(result, Err(GitError::CommandFailed { .. })));
    }

    #[test]
    fn git_facts_expose_base_ref_and_head_commit() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, repo) = init_repo()?;
        write(&repo, "a.txt", "a\n")?;
        let base = commit_all(&repo, "base")?;
        let head = rev_parse(&repo, "HEAD")?;

        let invocation = test_invocation(&repo);
        let facts: GitFacts = capture(&repo, &BaseRef::new(base.clone()), &invocation)?;

        assert_eq!(&BaseRef::new(base), facts.base_ref());
        assert_eq!(head, facts.head_commit());
        Ok(())
    }
}
