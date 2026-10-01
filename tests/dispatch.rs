//! Integration tests for T009: dispatching workers into worktrees with
//! scoped tokens, driven entirely through `silent_critic::tools::Tools`'s public
//! `dispatch` tool -- the same containment boundary `tests/tools.rs`
//! exercises for every other tool.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::json;

use silent_critic::dispatch::{self, DispatchConfig, DispatchError, HarnessSpec, PromptVia};
use silent_critic::evaluate::automated::CheckTimeout;
use silent_critic::model::TaskId;
use silent_critic::store::{Store, StoreRoot};
use silent_critic::tools::{ToolOutcome, ToolSurface, Tools};
use silent_critic::worktree::GitEnv;

const SENTINEL: &str = "SENTINEL-7f3a9c";
const SENTINEL_PLAN: &str = include_str!("fixtures/2026-09-05-hidden-sentinel-plan.md");

/// Reading `PATH` here is test code finding the real `git` binary, not the
/// library reading its own environment (see `src/worktree.rs`'s module
/// docs).
#[allow(clippy::disallowed_methods)]
fn test_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Harden `command` against the ambient git-hook environment: `env_clear()`
/// plus an explicit, minimal environment. `env_remove("GIT_DIR")`/
/// `env_remove("GIT_WORK_TREE")` alone is not enough -- git also exports
/// `GIT_INDEX_FILE` into any process it spawns as a hook (this crate's own
/// pre-commit hook runs `cargo nextest`), and a test spawning `git` for its
/// own temporary repository without clearing the full environment then
/// operates against the outer repository's index/worktree instead of its
/// own, corrupting it.
fn harden_git_command(command: &mut Command, home: &Path) {
    command
        .env_clear()
        .env("PATH", test_path())
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
}

fn git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
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

fn init_repo(dir: &Path) -> Result<String, Box<dyn Error>> {
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
    )?;
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(["rev-parse", "HEAD"]);
    harden_git_command(&mut command, dir);
    let output = command.output()?;
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

struct Fixture {
    _repo_dir: tempfile::TempDir,
    _store_dir: tempfile::TempDir,
    store: Store,
    repo_start: PathBuf,
    plan_id: String,
    base_commit: String,
}

fn set_up() -> Result<Fixture, Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    let base_commit = init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-fixture-plan.md");
    std::fs::write(&plan_path, SENTINEL_PLAN)?;

    let store_dir = tempfile::tempdir()?;
    let store = Store::new(StoreRoot::new(store_dir.path().to_path_buf()));
    let plan_id = store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

    Ok(Fixture {
        repo_start: repo_dir.path().to_path_buf(),
        plan_id: plan_id.as_str().to_owned(),
        base_commit,
        store,
        _repo_dir: repo_dir,
        _store_dir: store_dir,
    })
}

/// A `GitEnv` good enough to actually run git: reading `PATH` here is test
/// code finding the real `git` binary, not the library reading its own
/// environment (see `src/worktree.rs`'s module docs).
fn test_git_env() -> GitEnv {
    GitEnv::new(
        "git".to_owned(),
        test_path(),
        std::env::temp_dir().to_string_lossy().into_owned(),
    )
}

/// A harness that dumps its own environment next to the brief (so the test
/// can inspect exactly what the child saw) and touches the brief file
/// without ever printing its content anywhere this test reads back.
fn scripted_harness() -> HarnessSpec {
    let script = "env > \"$SILENT_CRITIC_BRIEF.env\"; cat \"$SILENT_CRITIC_BRIEF\" > /dev/null";
    HarnessSpec {
        config: DispatchConfig {
            git: test_git_env(),
            silent_critic_mcp_path: PathBuf::from("silent-critic-mcp"),
        },
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_owned(), script.to_owned()],
        model: Some("test-model-1".to_owned()),
        model_flag: None,
        prompt_via: PromptVia::BriefPath,
        // Explicit, not inherited: this is the only `PATH` the child sees,
        // needed for `env`/`cat` to resolve.
        environment: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        shell: PathBuf::from("/bin/sh"),
        timeout: CheckTimeout::new(Duration::from_secs(10)),
    }
}

/// A harness that records the arguments `dispatch` appends after
/// `harness.args`, NUL-separated, next to the brief -- so a test can assert
/// on argument order and separator placement rather than only on the
/// evidence summary. `args` supplies a positional `command_name` (`"argv0"`,
/// POSIX `sh -c command_string command_name [argument...]`) so `$0` is set
/// explicitly and every argument `dispatch` appends afterward lands in
/// `"$@"`, which is what gets captured -- `command_name` itself is excluded
/// from `"$@"` by definition.
fn argv_recording_harness() -> HarnessSpec {
    // NUL-separated, not newline-separated: the brief argument itself
    // contains embedded newlines (it is the rendered Markdown), so a
    // newline separator would be ambiguous between "end of this argument"
    // and "a newline inside this argument".
    let script = "printf '%s\\0' \"$@\" > \"$SILENT_CRITIC_BRIEF.argv\"";
    HarnessSpec {
        config: DispatchConfig {
            git: test_git_env(),
            silent_critic_mcp_path: PathBuf::from("silent-critic-mcp"),
        },
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_owned(), script.to_owned(), "argv0".to_owned()],
        model: Some("test-model-1".to_owned()),
        model_flag: Some("--model".to_owned()),
        prompt_via: PromptVia::Argument,
        environment: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
        shell: PathBuf::from("/bin/sh"),
        timeout: CheckTimeout::new(Duration::from_secs(10)),
    }
}

fn dispatch_config() -> DispatchConfig {
    DispatchConfig {
        git: test_git_env(),
        silent_critic_mcp_path: PathBuf::from("silent-critic-mcp"),
    }
}

/// A dispatched task's worktree/scratch directory: `<store_root>/runs/<repo
/// identity>/<plan_id>/<task>/` (fix round 1: moved out from under the
/// plan directory, `REPO_INVARIANTS.md` HO-004).
fn run_dir(fixture: &Fixture, task: &str) -> Result<PathBuf, Box<dyn Error>> {
    let provenance = fixture
        .store
        .provenance(&fixture.repo_start, &fixture.plan_id)?;
    Ok(fixture
        .store
        .root_path()
        .join("runs")
        .join(provenance.repo.as_str())
        .join(&fixture.plan_id)
        .join(task))
}

/// The disposable token registry sidecar, still under the plan directory
/// (unaffected by the relocation above -- only the worktree and its
/// scratch files moved).
fn tokens_path(fixture: &Fixture) -> Result<PathBuf, Box<dyn Error>> {
    Ok(fixture
        .store
        .plan_directory(&fixture.repo_start, &fixture.plan_id)?
        .join("run")
        .join("tokens.toml"))
}

fn expected_branch(fixture: &Fixture, task: &str) -> String {
    format!("silent-critic/{}/{task}", fixture.plan_id)
}

fn branch_exists(repo_dir: &Path, branch: &str) -> Result<bool, Box<dyn Error>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo_dir).args([
        "rev-parse",
        "--verify",
        &format!("refs/heads/{branch}"),
    ]);
    harden_git_command(&mut command, repo_dir);
    let output = command.output()?;
    Ok(output.status.success())
}

// -- acceptance check 1: scoped token, no orchestrator credential --------

#[test]
fn dispatch_gives_the_child_a_task_scoped_token_and_nothing_inherited() -> Result<(), Box<dyn Error>>
{
    let fixture = set_up()?;
    let harness = scripted_harness();
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(harness),
    );

    let outcome = tools.call("dispatch", &json!({ "task_id": "T002" }));
    let ToolOutcome::Text(summary) = outcome else {
        return Err(format!("dispatch failed: {outcome:?}").into());
    };
    assert!(summary.contains("T002"));
    assert!(!summary.contains(SENTINEL));

    let run_dir = run_dir(&fixture, "T002")?;
    let brief_path = run_dir.join("brief.md");
    let env_dump = std::fs::read_to_string(format!("{}.env", brief_path.display()))?;

    // Find the minted worker token by reading the disposable token
    // registry sidecar directly -- the only durable place it is recorded
    // (fix round 1: it must no longer appear in the harness's own
    // environment at all).
    let tokens_body = std::fs::read_to_string(tokens_path(&fixture)?)?;
    // `TokenRegistry` serializes as TOML tables keyed by token, e.g.
    // `[tokens.silent_critic_worker_<uuid>.worker]` followed by `task = "T002"`; find
    // the token bound to T002 by that shape rather than re-parsing TOML
    // generically (no `toml` dev-dependency is needed for one lookup).
    let lines: Vec<&str> = tokens_body.lines().collect();
    let minted_token = lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            let token = line
                .strip_prefix("[tokens.")
                .and_then(|rest| rest.strip_suffix(".worker]"))?;
            let next = lines.get(index + 1)?.trim();
            if next == "task = \"T002\"" {
                Some(token.to_owned())
            } else {
                None
            }
        })
        .ok_or("could not find a minted worker token for T002 in the registry sidecar")?;

    // Fix round 1: the token must NOT reach the harness process's own
    // environment at all -- only `mcp.json`, for the `silent-critic-mcp` child a
    // harness's MCP client spawns, ever sees it.
    assert!(
        !env_dump.contains(&minted_token),
        "the minted token leaked into the harness's own environment:\n{env_dump}"
    );
    assert!(
        !env_dump
            .lines()
            .any(|line| line.starts_with("SILENT_CRITIC_TOKEN=")),
        "the harness environment must carry no SILENT_CRITIC_TOKEN at all:\n{env_dump}"
    );
    assert!(
        !env_dump
            .lines()
            .any(|line| line.starts_with("SILENT_CRITIC_STORE_ROOT=")),
        "the harness environment must carry no SILENT_CRITIC_STORE_ROOT:\n{env_dump}"
    );
    assert!(
        !env_dump
            .lines()
            .any(|line| line.starts_with("SILENT_CRITIC_PLAN_ID=")),
        "the harness environment must carry no SILENT_CRITIC_PLAN_ID:\n{env_dump}"
    );
    // What the harness *does* get: brief, repo (worktree), and where to
    // find mcp.json.
    assert!(
        env_dump
            .lines()
            .any(|line| line.starts_with("SILENT_CRITIC_BRIEF="))
    );
    assert!(
        env_dump
            .lines()
            .any(|line| line.starts_with("SILENT_CRITIC_REPO="))
    );
    assert!(
        env_dump
            .lines()
            .any(|line| line.starts_with("SILENT_CRITIC_MCP_CONFIG="))
    );

    // The token lives only in `mcp.json`, for the `silent-critic-mcp` child.
    let mcp_body = std::fs::read_to_string(run_dir.join("mcp.json"))?;
    assert!(
        mcp_body.contains(&minted_token),
        "mcp.json must carry the minted token for silent-critic-mcp:\n{mcp_body}"
    );
    // Fix round 2, finding #8: `mcp.json` also carries the task id, so
    // `silent-critic-mcp` can use it as `Tools::worker`'s caller-declared
    // `expected_task` instead of deriving it from the token itself.
    assert!(
        mcp_body.contains("SILENT_CRITIC_TASK_ID") && mcp_body.contains("T002"),
        "mcp.json must carry SILENT_CRITIC_TASK_ID for silent-critic-mcp:\n{mcp_body}"
    );

    // Nothing from the test process's own environment leaked through
    // (env_clear() plus only the explicit pairs this test and `dispatch`
    // supplied).
    assert!(
        !env_dump.lines().any(|line| line.starts_with("HOME=")),
        "HOME was never added to the harness environment and must not appear:\n{env_dump}"
    );
    assert!(
        !env_dump.lines().any(|line| line.starts_with("CARGO_")),
        "no CARGO_* variable from the test process should be inherited:\n{env_dump}"
    );

    silent_critic::dispatch::cleanup(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new("T002"),
        &dispatch_config(),
    )?;

    Ok(())
}

/// `mcp.json` carries a bearer token in the clear and must be restricted to
/// the owner (fix round 1).
#[test]
#[cfg(unix)]
fn mcp_config_is_written_with_owner_only_permissions() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = set_up()?;
    let harness = scripted_harness();
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(harness),
    );
    let outcome = tools.call("dispatch", &json!({ "task_id": "T002" }));
    assert!(matches!(outcome, ToolOutcome::Text(_)), "{outcome:?}");

    let mcp_path = run_dir(&fixture, "T002")?.join("mcp.json");
    let mode = std::fs::metadata(&mcp_path)?.permissions().mode() & 0o777;
    assert_eq!(0o600, mode, "mcp.json must be owner-read/write only");

    Ok(())
}

/// `tokens.toml` holds every worker token in cleartext and must be
/// restricted to the owner too (fix round 2, finding #7).
#[test]
#[cfg(unix)]
fn tokens_registry_is_written_with_owner_only_permissions() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = set_up()?;
    let harness = scripted_harness();
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(harness),
    );
    let outcome = tools.call("dispatch", &json!({ "task_id": "T002" }));
    assert!(matches!(outcome, ToolOutcome::Text(_)), "{outcome:?}");

    let tokens_path = fixture
        .store
        .plan_directory(&fixture.repo_start, &fixture.plan_id)?
        .join("run")
        .join("tokens.toml");
    let mode = std::fs::metadata(&tokens_path)?.permissions().mode() & 0o777;
    assert_eq!(0o600, mode, "tokens.toml must be owner-read/write only");

    Ok(())
}

// -- acceptance check 2: the rendered brief has no hidden criteria --------

#[test]
fn rendered_brief_file_on_disk_never_carries_the_sentinel() -> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let harness = scripted_harness();
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(harness),
    );

    let outcome = tools.call("dispatch", &json!({ "task_id": "T002" }));
    assert!(matches!(outcome, ToolOutcome::Text(_)), "{outcome:?}");

    let brief_path = run_dir(&fixture, "T002")?.join("brief.md");
    let brief_body = std::fs::read_to_string(&brief_path)?;
    assert!(
        !brief_body.contains(SENTINEL),
        "the rendered brief on disk leaked the sentinel"
    );
    assert!(!brief_body.is_empty());

    Ok(())
}

// -- acceptance check 3: recorded evidence -------------------------------

#[test]
fn recorded_evidence_carries_harness_model_arguments_exit_and_wall_time()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let harness = scripted_harness();
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(harness),
    );

    let outcome = tools.call("dispatch", &json!({ "task_id": "T002" }));
    assert!(matches!(outcome, ToolOutcome::Text(_)), "{outcome:?}");

    let state_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?
        .parent()
        .ok_or("plan path has no parent")?
        .join("run")
        .join("T002.toml");
    let state_body = std::fs::read_to_string(state_path)?;

    assert!(
        state_body.contains("/bin/sh"),
        "missing harness: {state_body}"
    );
    assert!(
        state_body.contains("test-model-1"),
        "missing model: {state_body}"
    );
    assert!(
        state_body.contains("arguments"),
        "missing arguments: {state_body}"
    );
    assert!(state_body.contains("exit"), "missing exit: {state_body}");
    assert!(
        state_body.contains("wall_time"),
        "missing wall_time: {state_body}"
    );
    assert!(
        !state_body.contains(SENTINEL),
        "evidence must never carry the sentinel"
    );

    // Critical fix (round 1): the raw `CheckResult` persisted alongside the
    // evidence must never carry the minted token in the clear either, even
    // though its `environment` field is a full copy of what the child
    // process ran with.
    let tokens_body = std::fs::read_to_string(tokens_path(&fixture)?)?;
    let lines: Vec<&str> = tokens_body.lines().collect();
    let minted_token = lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| {
            let token = line
                .strip_prefix("[tokens.")
                .and_then(|rest| rest.strip_suffix(".worker]"))?;
            let next = lines.get(index + 1)?.trim();
            if next == "task = \"T002\"" {
                Some(token.to_owned())
            } else {
                None
            }
        })
        .ok_or("could not find a minted worker token for T002 in the registry sidecar")?;
    assert!(
        !state_body.contains(&minted_token),
        "the minted token must never appear anywhere in the run-state sidecar:\n{state_body}"
    );

    Ok(())
}

// -- invariants: worktree shape, plan isolation, idempotent failure ------

#[test]
fn worktree_is_at_the_base_commit_on_the_expected_branch_outside_the_repository()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let harness = scripted_harness();
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(harness),
    );

    let outcome = tools.call("dispatch", &json!({ "task_id": "T002" }));
    assert!(matches!(outcome, ToolOutcome::Text(_)), "{outcome:?}");

    let worktree_path = run_dir(&fixture, "T002")?.join("worktree");
    assert!(worktree_path.is_dir());

    let canonical_worktree = worktree_path.canonicalize()?;
    let canonical_repo = fixture.repo_start.canonicalize()?;
    assert!(!canonical_worktree.starts_with(&canonical_repo));

    // No ancestor of the worktree is the plan directory (fix round 1,
    // `REPO_INVARIANTS.md` HO-004): the worktree now lives under the store
    // root's `runs/` tree, entirely separate from the plan's own directory.
    let plan_dir = fixture
        .store
        .plan_directory(&fixture.repo_start, &fixture.plan_id)?
        .canonicalize()?;
    assert!(
        !canonical_worktree.starts_with(&plan_dir),
        "the worktree must not be nested under the plan directory"
    );
    assert!(
        canonical_worktree
            .ancestors()
            .all(|ancestor| ancestor != plan_dir),
        "no ancestor of the worktree may be the plan directory"
    );

    let mut head_command = Command::new("git");
    head_command
        .arg("-C")
        .arg(&worktree_path)
        .args(["rev-parse", "HEAD"]);
    harden_git_command(&mut head_command, &worktree_path);
    let head = head_command.output()?;
    assert_eq!(fixture.base_commit, String::from_utf8(head.stdout)?.trim());

    let mut branch_command = Command::new("git");
    branch_command
        .arg("-C")
        .arg(&worktree_path)
        .args(["branch", "--show-current"]);
    harden_git_command(&mut branch_command, &worktree_path);
    let branch = branch_command.output()?;
    assert_eq!(
        expected_branch(&fixture, "T002"),
        String::from_utf8(branch.stdout)?.trim()
    );

    // The operator plan never reaches the worktree or the scratch dir.
    assert!(!worktree_path.join("2026-09-05-fixture-plan.md").exists());
    let scratch_dir = run_dir(&fixture, "T002")?;
    assert!(!scratch_dir.join("2026-09-05-fixture-plan.md").exists());
    assert!(!scratch_dir.join("plan.md").exists());

    Ok(())
}

#[test]
fn a_second_dispatch_of_the_same_task_fails_readably() -> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );

    let first = tools.call("dispatch", &json!({ "task_id": "T002" }));
    assert!(matches!(first, ToolOutcome::Text(_)), "{first:?}");

    let tools_again = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    let second = tools_again.call("dispatch", &json!({ "task_id": "T002" }));
    assert!(
        matches!(second, ToolOutcome::Failed(_)),
        "a second dispatch of the same task must fail readably, got: {second:?}"
    );

    // The first worktree must still be intact -- a failed repeat must not
    // corrupt it.
    let worktree_path = run_dir(&fixture, "T002")?.join("worktree");
    assert!(worktree_path.is_dir());
    assert!(worktree_path.join("README.md").is_file());

    Ok(())
}

#[test]
fn dispatch_fails_readably_with_no_harness_configured() -> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    );

    let outcome = tools.call("dispatch", &json!({ "task_id": "T002" }));
    assert!(matches!(outcome, ToolOutcome::Failed(_)));

    Ok(())
}

#[test]
fn dispatch_still_gates_on_readiness_before_touching_the_filesystem() -> Result<(), Box<dyn Error>>
{
    let fixture = set_up()?;
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );

    // T001 is already done; not a valid dispatch target.
    let outcome = tools.call("dispatch", &json!({ "task_id": "T001" }));
    assert!(matches!(outcome, ToolOutcome::Failed(_)));
    assert!(!run_dir(&fixture, "T001")?.exists());

    Ok(())
}

// -- direct `dispatch::dispatch`/`prepare` coverage -----------------------

#[test]
fn dispatch_supports_argument_prompt_via_and_a_model_flag() -> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let mut harness = scripted_harness();
    harness.prompt_via = PromptVia::Argument;
    harness.model_flag = Some("--model".to_owned());

    let report = dispatch::dispatch(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new("T002"),
        SENTINEL_PLAN,
        &harness,
    )?;

    assert!(report.evidence.summary().contains("test-model-1"));
    assert!(!report.evidence.summary().contains(SENTINEL));

    Ok(())
}

/// A brief whose front matter starts with `---` (every rendered brief does,
/// since the plan's YAML front matter is preserved by the worker
/// projection) must reach an argument-mode harness as a positional
/// argument, never as something a CLI-style option parser could read as a
/// flag. `--` after every configured option -- including the model flag --
/// is what forces that.
#[test]
fn dispatch_inserts_a_double_dash_before_the_brief_in_argument_mode() -> Result<(), Box<dyn Error>>
{
    let fixture = set_up()?;
    let harness = argv_recording_harness();

    let report = dispatch::dispatch(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new("T002"),
        SENTINEL_PLAN,
        &harness,
    )?;

    assert!(report.evidence.summary().contains("test-model-1"));

    let brief_path = fixture
        .store
        .root_path()
        .join("runs")
        .join(
            fixture
                .store
                .provenance(&fixture.repo_start, &fixture.plan_id)?
                .repo
                .as_str(),
        )
        .join(&fixture.plan_id)
        .join("T002")
        .join("brief.md");
    let recorded_argv = std::fs::read(format!("{}.argv", brief_path.display()))?;
    // Split on NUL, dropping the trailing empty element left by the final
    // separator.
    let mut fields: Vec<String> = recorded_argv
        .split(|byte| *byte == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect();
    if fields.last().is_some_and(String::is_empty) {
        fields.pop();
    }

    let dash_index = fields
        .iter()
        .position(|field| field == "--")
        .ok_or_else(|| format!("recorded argv must contain a literal `--`: {fields:?}"))?;
    assert_eq!(
        Some(&"--model".to_owned()),
        fields.get(dash_index.wrapping_sub(2)),
        "the model flag must come before `--`: {fields:?}"
    );
    assert_eq!(
        Some(&"test-model-1".to_owned()),
        fields.get(dash_index.wrapping_sub(1)),
        "the model value must come before `--`: {fields:?}"
    );
    let brief_arg = fields
        .get(dash_index + 1)
        .ok_or_else(|| format!("brief text must follow `--`: {fields:?}"))?;
    assert!(
        brief_arg.starts_with("---"),
        "the brief argument must be the rendered front matter, got: {brief_arg:?}"
    );
    assert_eq!(
        fields.len(),
        dash_index + 2,
        "the brief must be the final argument: {fields:?}"
    );

    Ok(())
}

#[test]
fn prepare_reuses_an_existing_token_registry_across_tasks_in_the_same_plan()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let config = dispatch_config();

    let _first = dispatch::prepare(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new("T002"),
        SENTINEL_PLAN,
        &config,
    )?;

    // A second, unrelated task id in the same plan: the token registry
    // sidecar already exists on disk and must be loaded, not recreated.
    let second = dispatch::prepare(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new("T900"),
        SENTINEL_PLAN,
        &config,
    )?;

    assert!(second.worktree_path.is_dir());
    assert!(second.token.as_str().starts_with("silent_critic_worker_"));

    Ok(())
}

#[test]
fn prepare_reports_a_corrupt_token_registry_and_rolls_back_the_worktree()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let config = dispatch_config();
    let plan_dir = fixture
        .store
        .plan_directory(&fixture.repo_start, &fixture.plan_id)?;
    std::fs::create_dir_all(plan_dir.join("run"))?;
    std::fs::write(
        plan_dir.join("run").join("tokens.toml"),
        "not = [valid toml",
    )?;

    let actual = dispatch::prepare(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new("T002"),
        SENTINEL_PLAN,
        &config,
    );

    assert!(matches!(actual, Err(DispatchError::Token(_))));

    // Fix round 1: a failure after the worktree was created and recorded
    // must roll it back rather than leaving a half-dispatched task behind.
    let worktree_path = run_dir(&fixture, "T002")?.join("worktree");
    assert!(!worktree_path.exists(), "the worktree must be rolled back");
    assert!(
        !branch_exists(&fixture.repo_start, &expected_branch(&fixture, "T002"))?,
        "the branch must be rolled back"
    );
    Ok(())
}

#[test]
fn prepare_reports_a_write_failure_for_the_mcp_config_and_rolls_back_the_worktree()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let config = dispatch_config();
    let run_dir = run_dir(&fixture, "T002")?;
    // Block `mcp.json` with a directory of the same name so the write
    // fails with a genuine I/O error.
    std::fs::create_dir_all(run_dir.join("mcp.json"))?;

    let actual = dispatch::prepare(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new("T002"),
        SENTINEL_PLAN,
        &config,
    );

    assert!(matches!(actual, Err(DispatchError::Io { .. })));

    // Fix round 1: same rollback guarantee for this failure mode. The
    // worktree directory's `mcp.json` blocker (a plain directory) is
    // itself inside the worktree's run directory, so a successful rollback
    // removes the worktree entirely -- the blocker included.
    let worktree_path = run_dir.join("worktree");
    assert!(!worktree_path.exists(), "the worktree must be rolled back");
    assert!(
        !branch_exists(&fixture.repo_start, &expected_branch(&fixture, "T002"))?,
        "the branch must be rolled back"
    );
    Ok(())
}

// -- path-safety: unsafe identifiers are rejected before touching disk ---

#[test]
fn prepare_rejects_a_task_id_that_would_escape_the_run_directory() -> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let config = dispatch_config();

    let actual = dispatch::prepare(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new("../x"),
        SENTINEL_PLAN,
        &config,
    );

    assert!(matches!(actual, Err(DispatchError::UnsafeIdentifier(_))));
    // Nothing was created: the check runs before any filesystem or git
    // side effect.
    assert!(!fixture.store.root_path().join("runs").exists());

    Ok(())
}

#[test]
fn cleanup_rejects_a_task_id_that_would_escape_the_run_directory() -> Result<(), Box<dyn Error>> {
    let fixture = set_up()?;
    let config = dispatch_config();

    let actual = dispatch::cleanup(
        &fixture.store,
        &fixture.repo_start,
        &fixture.plan_id,
        &TaskId::new(".."),
        &config,
    );

    assert!(matches!(actual, Err(DispatchError::UnsafeIdentifier(_))));

    Ok(())
}
