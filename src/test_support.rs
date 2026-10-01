//! A minimal fixture builder shared by `src/tools.rs`'s and `src/ledger.rs`'s
//! own inline `#[cfg(test)]` unit tests.
//!
//! Rust compiles this library twice: once with `--cfg test` (linked only
//! into the library's own unit-test binary, the artifact these inline
//! `mod tests` blocks run in) and once without (linked into every
//! integration test under `tests/` and into both binaries). A public
//! function or method exercised only from `tests/*.rs` accumulates an
//! execution count only on the *second* compiled copy; the first copy's own
//! entry for that function stays at zero. `cargo llvm-cov report`'s
//! function-level metric correctly discards an all-zero compiled copy in
//! favor of a sibling copy that does have executions, but its line-level
//! metric does not perform the same reconciliation on the LLVM release this
//! workspace is pinned to -- so those lines show as "missed" in the gate
//! even though every line is genuinely exercised, just only from the other
//! compiled copy.
//!
//! The fix is not a tool change (out of scope, and the pinned versions move
//! only via `mise run update`): it is executing the same public paths
//! `tests/*.rs` already covers *from inside this crate's own `--cfg test`
//! build too*, so both compiled copies accumulate real executions. This
//! module is the fixture that makes that cheap: a temporary git repository
//! with one commit and a temporary store root, with the sentinel plan
//! fixture already stored, duplicating a handful of lines from
//! `tests/tools.rs`'s own `set_up` deliberately -- the two are testing the
//! same containment boundary from the two different compiled copies it
//! actually ships as, not one one delegating to the other.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use crate::store::{Store, StoreRoot};

/// The sentinel-carrying plan fixture used across this crate's own test
/// suites (`tests/tools.rs`, `tests/dispatch.rs`, and this module),
/// identical to the copy those integration tests `include_str!` themselves.
pub const SENTINEL_PLAN: &str =
    include_str!("../tests/fixtures/2026-09-05-hidden-sentinel-plan.md");

/// A stored plan plus the repository and store it was bound to, kept alive
/// for the fixture's lifetime.
pub struct Fixture {
    _repo_dir: TempDir,
    _store_dir: TempDir,
    pub store: Store,
    pub repo_start: PathBuf,
    pub plan_id: String,
}

/// The `PATH` this process was started with, for a test harness to hand to
/// a spawned `git` subprocess explicitly. Reading it here is the
/// caller-side process edge a test harness stands in for, not the library
/// itself reading its own environment (`REPO_INVARIANTS.md` RS-008).
#[allow(
    clippy::disallowed_methods,
    reason = "test harness stands in for the caller-side process edge; the library under test never reads PATH itself"
)]
pub fn test_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Harden `command` (already given its program, arguments, and working
/// directory) against the ambient git-hook environment: `env_clear()` plus
/// an explicit, minimal environment -- `PATH`, `HOME` set to `home`, and
/// the same `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`/`GIT_TERMINAL_PROMPT`
/// pins production git invocations use (`src/git.rs`/`src/worktree.rs`).
///
/// `env_remove("GIT_DIR")`/`env_remove("GIT_WORK_TREE")` alone is not
/// enough: git also exports `GIT_INDEX_FILE` into any process it spawns as
/// a hook -- this crate's own pre-commit hook runs `cargo nextest` -- and a
/// test spawning `git` for its own temporary repository without clearing
/// the *full* environment then operates against the outer repository's
/// index and worktree instead of its own, corrupting it (the defect this
/// helper closes: seven `src/ledger.rs` tests reproduced it against a real
/// checkout before this fix). `env_clear()` removes that gap
/// unconditionally rather than trusting every call site to name the right
/// variables to remove.
pub fn harden_git_command<'a>(command: &'a mut Command, home: &Path) -> &'a mut Command {
    command
        .env_clear()
        .env("PATH", test_path())
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
}

fn run_git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    harden_git_command(&mut command, dir);
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("git {args:?} failed in {}", dir.display()).into())
    }
}

/// Build a fixture: a fresh one-commit git repository and a plan (`plan_body`)
/// stored for it under a fresh store root.
///
/// # Errors
///
/// Returns an error if `git` cannot be run, the repository cannot be
/// initialized or committed to, or the plan fails to validate and store.
pub fn set_up(plan_body: &str) -> Result<Fixture, Box<dyn Error>> {
    let repo_dir = TempDir::new()?;
    run_git(repo_dir.path(), &["init", "--quiet"])?;
    std::fs::write(repo_dir.path().join("README.md"), "hello\n")?;
    run_git(repo_dir.path(), &["add", "README.md"])?;
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
    run_git(repo_dir.path(), &commit_args)?;

    let plan_path = repo_dir.path().join("2026-09-05-fixture-plan.md");
    std::fs::write(&plan_path, plan_body)?;

    let store_dir = TempDir::new()?;
    let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
    let plan_id = store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

    Ok(Fixture {
        repo_start: repo_dir.path().to_path_buf(),
        plan_id: plan_id.as_str().to_owned(),
        store,
        _repo_dir: repo_dir,
        _store_dir: store_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::{SENTINEL_PLAN, run_git, set_up};

    /// `run_git`'s error branch (an unrecognized subcommand) is never
    /// exercised by `set_up` itself, since every real invocation there
    /// succeeds; without a direct test the error branch's own line stays at
    /// zero executions in this crate's `--cfg test` build.
    #[test]
    fn run_git_reports_a_failing_command() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::TempDir::new()?;
        let result = run_git(dir.path(), &["not-a-real-git-subcommand"]);
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn set_up_builds_a_usable_fixture() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = set_up(SENTINEL_PLAN)?;
        assert!(!fixture.plan_id.is_empty());
        Ok(())
    }
}
