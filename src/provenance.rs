//! Provenance binding for a stored plan.
//!
//! A plan stored outside every worktree ([`crate::store`]) carries no
//! filesystem-implied binding to a repository or commit. This module derives
//! and records that binding explicitly, at the boundary, before a plan is
//! ever written to the store: the repository identity it belongs to and the
//! base ref the operator gave (resolved to a commit id). A dispatched task's
//! worktree path is *not* part of this binding (fix round 2, finding #1): it
//! is deterministic from `dispatch::run_dir`, so callers recompute it per
//! task rather than reading a single plan-level path that would otherwise
//! be overwritten every time a second task dispatches.
//!
//! Repository discovery and ref resolution shell out to the `git` binary
//! rather than depending on a git library: the process boundary is already
//! wrapped in this module's small API, so nothing outside it depends on how
//! that boundary is implemented.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tftio_lib::project::Slug;
use thiserror::Error;

/// Content-derived identity of a repository.
///
/// Derived from the repository's *common git directory*
/// (`git rev-parse --git-common-dir`), not its working-tree root: every
/// linked worktree of one repository shares the same common git directory
/// (for an ordinary clone it is `<root>/.git`; for a linked worktree it
/// resolves through to the main checkout's `.git`; for a bare repository
/// with linked worktrees it is the bare directory itself), so binding from
/// any one of them converges on the same identity. This mirrors
/// `silent-critic`'s `project::find_repo_root`, which walks git's
/// `commondir` for the same reason, one level up from
/// `project::compute_repo_hash`'s canonicalize-then-hash step, which this
/// still reuses directly. Canonicalizing before hashing matters on macOS,
/// where a symlinked temporary directory (`/tmp` -> `/private/tmp`) would
/// otherwise make a path and its resolved form hash differently even though
/// they name the same repository.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoIdentity(String);

impl RepoIdentity {
    /// Derive the identity of the repository containing `start`.
    ///
    /// # Errors
    ///
    /// Returns [`ProvenanceError::SpawnGit`] when `git` cannot be run,
    /// [`ProvenanceError::NotARepository`] when `start` is not inside a git
    /// repository, or [`ProvenanceError::Canonicalize`] when the discovered
    /// common git directory cannot be canonicalized.
    pub fn derive(start: &Path) -> Result<Self, ProvenanceError> {
        let common_git_dir = discover_common_git_dir(start)?;
        Self::from_common_git_dir(&common_git_dir)
    }

    fn from_common_git_dir(common_git_dir: &Path) -> Result<Self, ProvenanceError> {
        let canonical = match common_git_dir.canonicalize() {
            Ok(canonical) => canonical,
            Err(source) => {
                return Err(ProvenanceError::Canonicalize {
                    path: common_git_dir.to_path_buf(),
                    source,
                });
            }
        };
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        Ok(Self(hex_encode(&hasher.finalize())))
    }

    /// Return the identity as its stable string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A base ref: the name the operator gave, resolved to a commit id.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct BaseRef {
    /// The ref name the operator gave (e.g. `HEAD`, `main`, a tag).
    pub name: String,
    /// The commit id the name resolved to at binding time.
    pub commit: String,
}

/// The full provenance binding a stored plan carries.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Identity of the repository the plan is bound to.
    pub repo: RepoIdentity,
    /// The base ref the plan is bound to.
    pub base_ref: BaseRef,
    /// The plan's fleet project slug, when the stored plan is a v2 document
    /// that declares one.
    ///
    /// Populated once, in [`crate::store::Store::add_plan`], from the
    /// validated plan's own `PlanMetadata.project` -- never derived from
    /// `repo_start` or any other filesystem signal -- so the binding and
    /// the plan can never disagree about which project a stored plan
    /// belongs to. `#[serde(default)]` (and `skip_serializing_if`, so a
    /// slugless binding's TOML looks exactly as it did before this field
    /// existed) means every `binding.toml` written before this field
    /// existed still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Slug>,
}

/// Bind provenance for a plan: discover the repository at `start` and
/// resolve `base_ref_name` to a commit within it.
///
/// # Errors
///
/// Returns [`ProvenanceError::SpawnGit`] when `git` cannot be run,
/// [`ProvenanceError::NotARepository`] when `start` is not inside a git
/// repository, [`ProvenanceError::Canonicalize`] when the discovered common
/// git directory cannot be canonicalized, or
/// [`ProvenanceError::UnresolvedBaseRef`] when `base_ref_name` does not
/// resolve to a commit.
pub fn bind(start: &Path, base_ref_name: &str) -> Result<Provenance, ProvenanceError> {
    let common_git_dir = discover_common_git_dir(start)?;
    let repo = RepoIdentity::from_common_git_dir(&common_git_dir)?;
    // Base-ref resolution needs any path git recognizes as inside the
    // repository, not specifically its working-tree root, so `start` itself
    // (whatever the operator gave, or the current directory) is used
    // directly rather than discovering a further path.
    let commit = resolve_commit(start, base_ref_name)?;
    Ok(Provenance {
        repo,
        base_ref: BaseRef {
            name: base_ref_name.to_string(),
            commit,
        },
        // `bind` derives repository and base-ref identity only; the caller
        // ([`crate::store::Store::add_plan`]) populates `project` from the
        // plan's own validated metadata, never from anything `bind` can see.
        project: None,
    })
}

/// Failures while binding a plan's provenance.
#[derive(Debug, Error)]
pub enum ProvenanceError {
    /// The `git` binary could not be spawned.
    #[error("spawning git in {path}: {source}")]
    SpawnGit {
        /// The directory `git` was invoked from.
        path: PathBuf,
        /// The underlying process-spawn error.
        source: std::io::Error,
    },
    /// `start` is not inside a git repository.
    #[error("{path} is not inside a git repository: {stderr}")]
    NotARepository {
        /// The path discovery started from.
        path: PathBuf,
        /// `git`'s diagnostic output.
        stderr: String,
    },
    /// The discovered repository root could not be canonicalized.
    #[error("canonicalizing repository path {path}: {source}")]
    Canonicalize {
        /// The path that could not be canonicalized.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The given base ref does not resolve to a commit.
    #[error("base ref '{name}' does not resolve to a commit in {path}: {stderr}")]
    UnresolvedBaseRef {
        /// The ref name the operator gave.
        name: String,
        /// The repository the ref was resolved against.
        path: PathBuf,
        /// `git`'s diagnostic output.
        stderr: String,
    },
}

// The `git` binary name is threaded through as a parameter (rather than
// hardcoded in `run_git`) so tests can force `GitFailure::Spawn` with a
// binary that cannot be found, exercising that branch deterministically:
// forcing an actual `git`-not-installed condition is not something a
// portable test can do.
const GIT_BINARY: &str = "git";

/// Discover the repository's common git directory containing `start`:
/// `<root>/.git` for an ordinary clone, the main checkout's `.git` for a
/// linked worktree, or the bare directory itself for a bare repository —
/// the same path for every worktree of one repository (see
/// [`RepoIdentity`]).
fn discover_common_git_dir(start: &Path) -> Result<PathBuf, ProvenanceError> {
    discover_common_git_dir_with(GIT_BINARY, start)
}

fn discover_common_git_dir_with(
    git_binary: &str,
    start: &Path,
) -> Result<PathBuf, ProvenanceError> {
    match run_git(git_binary, start, &["rev-parse", "--git-common-dir"]) {
        Ok(stdout) => {
            let git_dir = PathBuf::from(stdout);
            if git_dir.is_absolute() {
                Ok(git_dir)
            } else {
                // `git`'s output is relative to the directory it was
                // invoked from (`start`, via `-C`); resolve `start` itself
                // to an absolute path first (lexically, against the
                // process's current directory — no filesystem access, no
                // symlink resolution) so joining the two always yields an
                // absolute path, regardless of whether the caller passed a
                // relative `start`. Canonicalization (resolving symlinks)
                // happens once, afterward, in `RepoIdentity::from_common_git_dir`.
                // `std::path::absolute` only fails if the current directory
                // cannot be determined (e.g. it was deleted after this
                // process started) — not deterministically forceable in a
                // portable test — so the fallback is eager
                // (`Result::unwrap_or`, a `core`-only call with no
                // project-local branch of its own to cover) rather than a
                // closure passed to `unwrap_or_else`.
                #[allow(clippy::or_fun_call)] // deliberately eager; see comment above
                let absolute_start = std::path::absolute(start).unwrap_or(start.to_path_buf());
                Ok(absolute_start.join(git_dir))
            }
        }
        Err(GitFailure::Spawn(source)) => Err(ProvenanceError::SpawnGit {
            path: start.to_path_buf(),
            source,
        }),
        Err(GitFailure::Command(stderr)) => Err(ProvenanceError::NotARepository {
            path: start.to_path_buf(),
            stderr,
        }),
    }
}

fn resolve_commit(repo_root: &Path, base_ref_name: &str) -> Result<String, ProvenanceError> {
    resolve_commit_with(GIT_BINARY, repo_root, base_ref_name)
}

fn resolve_commit_with(
    git_binary: &str,
    repo_root: &Path,
    base_ref_name: &str,
) -> Result<String, ProvenanceError> {
    let revspec = format!("{base_ref_name}^{{commit}}");
    match run_git(git_binary, repo_root, &["rev-parse", "--verify", &revspec]) {
        Ok(commit) => Ok(commit),
        Err(GitFailure::Spawn(source)) => Err(ProvenanceError::SpawnGit {
            path: repo_root.to_path_buf(),
            source,
        }),
        Err(GitFailure::Command(stderr)) => Err(ProvenanceError::UnresolvedBaseRef {
            name: base_ref_name.to_string(),
            path: repo_root.to_path_buf(),
            stderr,
        }),
    }
}

enum GitFailure {
    Spawn(std::io::Error),
    Command(String),
}

fn run_git(binary: &str, cwd: &Path, args: &[&str]) -> Result<String, GitFailure> {
    let output = Command::new(binary)
        .arg("-C")
        .arg(cwd)
        // Ignore any ambient GIT_DIR/GIT_WORK_TREE so `-C` reliably picks the
        // repository at `cwd` rather than one the environment points at.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(args)
        .output()
        .map_err(GitFailure::Spawn)?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(GitFailure::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

/// Hex-encode `bytes`, avoiding a dependency on the `hex` crate for one
/// small helper (mirrors `silent-critic`'s `project::hex`).
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::Path;
    use std::process::Command;

    use super::{ProvenanceError, RepoIdentity, bind};

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

    fn init_repo(dir: &Path) -> Result<(), Box<dyn Error>> {
        git(dir, &["init", "--quiet"])?;
        std::fs::write(dir.join("README.md"), "hello\n")?;
        git(dir, &["add", "README.md"])?;
        git(
            dir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "initial commit",
            ],
        )
    }

    #[test]
    fn bind_resolves_head_to_a_commit() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;
        init_repo(tmp.path())?;

        let provenance = bind(tmp.path(), "HEAD")?;

        assert_eq!("HEAD", provenance.base_ref.name);
        assert_eq!(40, provenance.base_ref.commit.len());
        assert!(
            provenance
                .base_ref
                .commit
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
        Ok(())
    }

    #[test]
    fn bind_rejects_a_non_repository() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;

        let actual = bind(tmp.path(), "HEAD");

        assert!(matches!(
            actual,
            Err(ProvenanceError::NotARepository { .. })
        ));
        Ok(())
    }

    #[test]
    fn bind_rejects_an_unresolvable_base_ref() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;
        init_repo(tmp.path())?;

        let actual = bind(tmp.path(), "does-not-exist");

        assert!(matches!(
            actual,
            Err(ProvenanceError::UnresolvedBaseRef { .. })
        ));
        Ok(())
    }

    #[test]
    fn identity_is_stable_and_content_derived() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;
        init_repo(tmp.path())?;

        let first = RepoIdentity::derive(tmp.path())?;
        let second = RepoIdentity::derive(tmp.path())?;

        assert_eq!(first, second);
        assert_eq!(64, first.as_str().len());
        Ok(())
    }

    #[test]
    fn identity_derivation_reports_canonicalize_failures() {
        // Calling the private, path-only half of derivation directly with a
        // path `git` never produced lets us exercise the canonicalize
        // failure without depending on a filesystem race.
        let actual = super::RepoIdentity::from_common_git_dir(Path::new(
            "/definitely/does/not/exist/silent-critic-test-path",
        ));

        assert!(matches!(actual, Err(ProvenanceError::Canonicalize { .. })));
    }

    #[test]
    fn discover_common_git_dir_reports_spawn_failures() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;

        let actual = super::discover_common_git_dir_with(
            "silent-critic-test-nonexistent-git-binary",
            tmp.path(),
        );

        assert!(matches!(actual, Err(ProvenanceError::SpawnGit { .. })));
        Ok(())
    }

    #[test]
    fn resolve_commit_reports_spawn_failures() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;

        let actual = super::resolve_commit_with(
            "silent-critic-test-nonexistent-git-binary",
            tmp.path(),
            "HEAD",
        );

        assert!(matches!(actual, Err(ProvenanceError::SpawnGit { .. })));
        Ok(())
    }

    #[test]
    fn git_helper_reports_command_failures() -> Result<(), Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;

        let actual = git(tmp.path(), &["not-a-real-git-subcommand"]);

        assert!(actual.is_err());
        Ok(())
    }

    #[test]
    fn identity_differs_across_repositories() -> Result<(), Box<dyn Error>> {
        let tmp_a = tempfile::tempdir()?;
        let tmp_b = tempfile::tempdir()?;
        init_repo(tmp_a.path())?;
        init_repo(tmp_b.path())?;

        let identity_a = RepoIdentity::derive(tmp_a.path())?;
        let identity_b = RepoIdentity::derive(tmp_b.path())?;

        assert_ne!(identity_a, identity_b);
        Ok(())
    }

    fn worktree_add(
        repo_dir: &Path,
        worktree_path: &Path,
        branch: &str,
    ) -> Result<(), Box<dyn Error>> {
        let worktree_path = worktree_path
            .to_str()
            .ok_or("worktree path must be valid UTF-8 for this test")?;
        git(
            repo_dir,
            &["worktree", "add", "--quiet", "-b", branch, worktree_path],
        )
    }

    #[test]
    fn identity_converges_across_a_linked_worktree_of_an_ordinary_clone()
    -> Result<(), Box<dyn Error>> {
        let main_checkout = tempfile::tempdir()?;
        init_repo(main_checkout.path())?;

        let worktree_parent = tempfile::tempdir()?;
        let worktree_path = worktree_parent.path().join("linked-worktree");
        worktree_add(main_checkout.path(), &worktree_path, "linked")?;

        let identity_from_main = RepoIdentity::derive(main_checkout.path())?;
        let identity_from_worktree = RepoIdentity::derive(&worktree_path)?;

        assert_eq!(identity_from_main, identity_from_worktree);
        Ok(())
    }

    #[test]
    fn identity_converges_across_linked_worktrees_of_a_bare_repository()
    -> Result<(), Box<dyn Error>> {
        let bare_dir = tempfile::tempdir()?;
        git(bare_dir.path(), &["init", "--quiet", "--bare"])?;

        let worktree_parent = tempfile::tempdir()?;
        let worktree_a = worktree_parent.path().join("worktree-a");
        let worktree_b = worktree_parent.path().join("worktree-b");
        worktree_add(bare_dir.path(), &worktree_a, "branch-a")?;
        worktree_add(bare_dir.path(), &worktree_b, "branch-b")?;

        let identity_a = RepoIdentity::derive(&worktree_a)?;
        let identity_b = RepoIdentity::derive(&worktree_b)?;

        assert_eq!(identity_a, identity_b);
        Ok(())
    }
}
