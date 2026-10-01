//! Integration tests for `silent_critic::tools`, driven entirely through the
//! crate's public API.
//!
//! This lives under `tests/` (not only inline in `src/tools.rs`) so the
//! containment boundary is exercised through the same compiled artifact a
//! consuming binary would use, not only the crate's own `--cfg test` build.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{Value, json};

use silent_critic::dispatch::{DispatchConfig, HarnessSpec, PromptVia as HarnessPromptVia};
use silent_critic::evaluate::automated::{CheckEnvironment, CheckTimeout};
use silent_critic::git::GitInvocation;
use silent_critic::ledger::Ledger;
use silent_critic::model::{CriterionId, Judgment, NonEmptyString, PlanId, TaskId, Verdict};
use silent_critic::provider::{
    CommandEnvironment, CommandProviderSpec, PromptVia as ProviderPromptVia,
};
use silent_critic::store::{Store, StoreRoot};
use silent_critic::token::{Role, Token, TokenRegistry};
use silent_critic::tools::{JudgeConfig, JudgeProviders, ToolOutcome, ToolSurface, Tools};
use silent_critic::worktree::GitEnv;

const SENTINEL: &str = "SENTINEL-7f3a9c";
const SENTINEL_PLAN: &str = include_str!("fixtures/2026-09-05-hidden-sentinel-plan.md");
const VALID_PLAN: &str = include_str!("fixtures/2026-09-05-store-plan.md");
/// Planning Document Format v2: no `mode`, no authored `blocks`, and no
/// `files` on any task (planner 0.4 never populates either of the latter
/// two from a v2 document). T001 `depends_on` `[]` and is `done`; T002
/// `depends_on` `[T001]` and is `not_started`.
const V2_SENTINEL_PLAN: &str = include_str!("fixtures/2026-09-14-v2-sentinel-plan.md");

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

/// Unwrap a successful [`ToolOutcome::Text`], or surface a
/// [`ToolOutcome::Failed`] reason as a test error.
fn expect_text(outcome: ToolOutcome) -> Result<String, String> {
    match outcome {
        ToolOutcome::Text(text) => Ok(text),
        ToolOutcome::Failed(reason) => Err(reason),
    }
}

#[test]
fn expect_text_surfaces_a_failed_outcome() {
    assert_eq!(
        Err("nope".to_owned()),
        expect_text(ToolOutcome::Failed("nope".to_owned()))
    );
}

#[test]
fn git_helper_reports_command_failures() -> Result<(), Box<dyn Error>> {
    let tmp = tempfile::tempdir()?;
    let actual = git(tmp.path(), &["not-a-real-git-subcommand"]);
    assert!(actual.is_err());
    Ok(())
}

struct Fixture {
    _repo_dir: tempfile::TempDir,
    _store_dir: tempfile::TempDir,
    store: Store,
    repo_start: PathBuf,
    plan_id: String,
}

fn set_up(plan_body: &str) -> Result<Fixture, Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-fixture-plan.md");
    std::fs::write(&plan_path, plan_body)?;

    let store_dir = tempfile::tempdir()?;
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

fn orchestrator(fixture: &Fixture) -> Tools<'_> {
    Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
}

// -- sentinel leak test -----------------------------------------------

#[test]
fn no_orchestrator_response_leaks_the_sentinel() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let tools = orchestrator(&fixture);

    let calls: Vec<(&str, Value)> = vec![
        ("plan_status", json!({})),
        ("next_ready", json!({})),
        ("dispatch", json!({ "task_id": "T002" })),
        ("dispatch", json!({ "task_id": "T001" })),
        ("dispatch", json!({ "task_id": "does-not-exist" })),
        ("judge", json!({ "task_id": "T002" })),
        ("judge", json!({ "task_id": "does-not-exist" })),
        (
            "record_decision",
            json!({ "title": "A decision", "body": "Some decision body." }),
        ),
        (
            "request_guidance",
            json!({ "question": "A question.", "affected_tasks": ["T002"] }),
        ),
        ("no_such_tool", json!({})),
    ];

    for (name, arguments) in calls {
        let outcome = tools.call(name, &arguments);
        let text = match &outcome {
            ToolOutcome::Text(text) | ToolOutcome::Failed(text) => text,
        };
        assert!(
            !text.contains(SENTINEL),
            "tool {name} leaked the sentinel: {text}"
        );
    }

    for spec in tools.specs() {
        assert!(!spec.description.contains(SENTINEL));
        assert!(!spec.schema.to_string().contains(SENTINEL));
    }

    Ok(())
}

/// Fix round 2, finding #4: `ToolSurface::instructions` must never tell a
/// caller (of any scope) that the plan carries hidden acceptance criteria
/// at all -- `REPO_INVARIANTS.md` HO-001 covers even that fact's
/// existence, not only hidden criteria's text or count.
#[test]
fn no_scopes_instructions_mention_hidden_criteria_or_the_sentinel() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;

    let orchestrator = orchestrator(&fixture);
    let orchestrator_instructions = orchestrator.instructions();
    assert!(!orchestrator_instructions.contains(SENTINEL));
    assert!(
        !orchestrator_instructions.to_lowercase().contains("hidden"),
        "orchestrator instructions must not mention hidden criteria: \
         {orchestrator_instructions}"
    );

    let plan_id = PlanId::new(fixture.plan_id.clone());
    let mut registry = TokenRegistry::new(plan_id.clone());
    let task = TaskId::new("T002");
    let token = registry.mint(Role::Worker { task: task.clone() }, &plan_id)?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &token,
    );
    let worker_instructions = worker.instructions();
    assert!(!worker_instructions.contains(SENTINEL));
    assert!(
        !worker_instructions.to_lowercase().contains("hidden"),
        "worker instructions must not mention hidden criteria: {worker_instructions}"
    );

    // An unauthenticated session (a token that never resolves) gets the
    // same worker-safe text, never anything more revealing.
    let unauthenticated = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &Token::new("silent_critic_worker_does-not-exist"),
    );
    let unauthenticated_instructions = unauthenticated.instructions();
    assert!(!unauthenticated_instructions.contains(SENTINEL));
    assert!(
        !unauthenticated_instructions
            .to_lowercase()
            .contains("hidden")
    );

    Ok(())
}

/// A `GitEnv`/`GitInvocation` pair good enough to actually run git: reading
/// `PATH` here is test code finding the real `git` binary, not the library
/// reading its own environment (see `src/worktree.rs`/`src/git.rs`'s module
/// docs).
fn test_path() -> String {
    #[allow(clippy::disallowed_methods)]
    std::env::var("PATH").unwrap_or_default()
}

fn test_git_env() -> GitEnv {
    GitEnv::new(
        "git".to_owned(),
        test_path(),
        std::env::temp_dir().to_string_lossy().into_owned(),
    )
}

fn test_git_invocation() -> GitInvocation {
    GitInvocation::new(
        "git".to_owned(),
        test_path(),
        std::env::temp_dir().to_string_lossy().into_owned(),
    )
}

/// A harness that writes one new file into the worktree, so the dispatched
/// task has a real diff for `judge` to capture.
fn scripted_harness() -> HarnessSpec {
    HarnessSpec {
        config: DispatchConfig {
            git: test_git_env(),
            silent_critic_mcp_path: PathBuf::from("silent-critic-mcp"),
        },
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".to_owned(),
            "echo did-the-sensitive-thing > sensitive.txt".to_owned(),
        ],
        model: None,
        model_flag: None,
        prompt_via: HarnessPromptVia::BriefPath,
        environment: Vec::new(),
        shell: PathBuf::from("/bin/sh"),
        timeout: CheckTimeout::new(Duration::from_secs(10)),
    }
}

/// A `CommandProviderSpec` that drains and ignores its prompt, then prints a fixed JSON
/// judge response: a shell-scripted provider, mirroring how
/// `scripted_harness` above stands in for a real harness, since
/// [`JudgeConfig`] (like [`HarnessSpec`]) is a subprocess specification, not
/// a `Provider` trait object.
fn scripted_judge_provider(response_json: &str) -> CommandProviderSpec {
    CommandProviderSpec {
        program: "/bin/sh".into(),
        // `cat >/dev/null` drains the prompt first: a child that exits 0 without
        // reading stdin races the provider's writer thread, and a lost race is a
        // broken pipe the provider reports as an error.
        args: vec![
            "-c".to_owned(),
            format!("cat >/dev/null; printf '%s' '{response_json}'"),
        ],
        model_flag: None,
        model: None,
        prompt_via: ProviderPromptVia::Stdin,
        environment: CommandEnvironment::default(),
        timeout: Duration::from_secs(10),
    }
}

/// A [`JudgeConfig`] whose canned response accepts both `visible-0` and
/// `hidden-0` with a fixed criterion-level rationale, but a caller-chosen
/// *run-level* `rationale` -- the field
/// `record_run_rationale_as_hidden_evidence` records as hidden-criterion
/// evidence, distinct from any one judgment's own rationale.
fn judge_config_with_rationale(rationale: &str) -> JudgeConfig {
    let response_json = format!(
        r#"{{"judgments":[{{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"}},{{"criterion_id":"hidden-0","judgment":"pass","rationale":"criterion-level rationale"}}],"disposition":"accept","rationale":"{rationale}"}}"#
    );
    JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(&response_json)),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    }
}

#[test]
fn judge_end_to_end_writes_the_sentinel_to_the_plan_but_never_to_the_response()
-> Result<(), Box<dyn Error>> {
    // The stock sentinel fixture's hidden criteria are `human_judgment` and
    // `automated`; `judge` only renders judgments for `agent_evaluated`
    // criteria, so this test flips the `human_judgment` one to
    // `agent_evaluated` (it already carries an `ask`, which
    // `agent_evaluated` also accepts).
    let plan_body = SENTINEL_PLAN.replacen(
        "        evaluator: human_judgment\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        "        evaluator: agent_evaluated\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );

    let fixture = set_up(&plan_body)?;

    // Dispatch T002 so it has a real worktree with a real diff.
    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    // The judge's canned response: a `pass` on the one visible criterion, a
    // `fail` on the (now agent-evaluated) hidden criterion, with the
    // sentinel embedded in that hidden criterion's rationale -- exactly the
    // shape HO-001 forbids from ever reaching the tool's own response.
    let response_json = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"the visible check looks satisfied"},{"criterion_id":"hidden-0","judgment":"fail","rationale":"SENTINEL-7f3a9c the worker did in fact weaken a check"}],"disposition":"needs_operator","rationale":"one hidden criterion failed"}"#;
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(response_json)),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let response = expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;
    assert!(
        !response.contains(SENTINEL),
        "judge's tool response leaked the sentinel: {response}"
    );
    let payload: Value = serde_json::from_str(&response)?;
    assert_eq!(
        Some("needs_operator"),
        payload.get("disposition").and_then(Value::as_str)
    );
    assert_eq!(
        Some(true),
        payload
            .get("operator_attention_required")
            .and_then(Value::as_bool)
    );
    let judgments = payload
        .get("judgments")
        .and_then(Value::as_array)
        .ok_or("expected a judgments array")?;
    assert_eq!(
        1,
        judgments.len(),
        "only the visible criterion's judgment should be returned"
    );
    assert_eq!(
        Some("pass"),
        judgments
            .first()
            .and_then(|j| j.get("judgment"))
            .and_then(Value::as_str)
    );

    // The sentinel-carrying hidden verdict is present in the stored plan.
    let stored = std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?;
    assert!(
        stored.contains(SENTINEL),
        "expected the stored plan to carry the sentinel in the hidden criterion's rationale"
    );
    assert!(tftio_planner::validate_markdown(&stored)?.is_valid());

    Ok(())
}

/// The judge run's own *disposition rationale* (not any single criterion's)
/// is informed by every criterion it considered, hidden ones included: it
/// must never reach `completion_evidence` (worker-visible) or the tool's own
/// response, only judge-provenance evidence on the hidden criteria the run
/// actually judged.
#[test]
fn judge_run_level_rationale_never_reaches_completion_evidence_or_the_worker_projection()
-> Result<(), Box<dyn Error>> {
    const RUN_RATIONALE_SENTINEL: &str = "SENTINEL-RUN-RATIONALE-2b6e";

    let plan_body = SENTINEL_PLAN.replacen(
        "        evaluator: human_judgment\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        "        evaluator: agent_evaluated\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );

    let fixture = set_up(&plan_body)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    // The run-level `rationale` (not any per-criterion one) carries the
    // sentinel this time.
    let response_json = format!(
        r#"{{"judgments":[{{"criterion_id":"visible-0","judgment":"pass","rationale":"the visible check looks satisfied"}},{{"criterion_id":"hidden-0","judgment":"pass","rationale":"looks fine"}}],"disposition":"accept","rationale":"{RUN_RATIONALE_SENTINEL} across every criterion this run considered"}}"#
    );
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(&response_json)),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let response = expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;
    assert!(
        !response.contains(RUN_RATIONALE_SENTINEL),
        "judge's tool response leaked the run-level rationale: {response}"
    );

    let stored = std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?;
    assert!(
        stored.contains(RUN_RATIONALE_SENTINEL),
        "expected the operator plan to carry the run-level rationale as hidden-criterion evidence"
    );
    assert!(tftio_planner::validate_markdown(&stored)?.is_valid());

    // Never in the task's own completion evidence (worker-visible).
    let plan = tftio_planner::parse_markdown(&stored)?;
    let task = plan
        .tasks
        .iter()
        .find(|task| task.id.as_str() == "T002")
        .ok_or("plan has no T002")?;
    assert!(
        !task
            .completion_evidence
            .as_deref()
            .unwrap_or_default()
            .contains(RUN_RATIONALE_SENTINEL),
        "run-level rationale leaked into completion_evidence: {:?}",
        task.completion_evidence
    );

    // Never in the worker-safe projection, by either route.
    let worker_markdown = tftio_planner::project_worker_markdown(&stored)?;
    assert!(!worker_markdown.contains(RUN_RATIONALE_SENTINEL));

    Ok(())
}

#[test]
fn judge_requires_configuration_and_a_prior_dispatch() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;

    // No judge configured at all.
    let unconfigured = orchestrator(&fixture);
    let outcome = unconfigured.call("judge", &json!({ "task_id": "T002" }));
    let ToolOutcome::Failed(reason) = outcome else {
        return Err("expected judge to fail without a JudgeConfig".into());
    };
    assert!(reason.contains("no judge is configured"));

    // Configured, but the requested task does not exist in the plan.
    let unknown_task_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(
            r#"{"judgments":[],"disposition":"accept","rationale":"nothing to judge"}"#,
        )),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let unknown_task = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(unknown_task_config);
    let outcome = unknown_task.call("judge", &json!({ "task_id": "does-not-exist" }));
    let ToolOutcome::Failed(reason) = outcome else {
        return Err("expected judge to fail for an unknown task".into());
    };
    assert!(reason.contains("no such task"));

    // Configured, but the task was never dispatched (no worktree).
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(
            r#"{"judgments":[],"disposition":"accept","rationale":"nothing to judge"}"#,
        )),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let not_dispatched = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);
    let outcome = not_dispatched.call("judge", &json!({ "task_id": "T002" }));
    let ToolOutcome::Failed(reason) = outcome else {
        return Err("expected judge to fail before any dispatch".into());
    };
    assert!(reason.contains("has not been dispatched"));

    Ok(())
}

/// Two independent, ready tasks, each carrying one visible acceptance
/// check and one `automated` hidden criterion whose check looks for a
/// marker file unique to that task's own dispatch.
const TWO_INDEPENDENT_TASKS_PLAN: &str = r"---
plan_format_version: 1
plan_id: PLAN-20260905-two-independent-tasks
title: SilentCritic two-independent-tasks fixture
status: approved
mode: single
created_at: 2026-09-05
updated_at: 2026-09-05
owner: test
source:
  type: manual
  url: null
  external_id: null
  imported_at: null
bug:
  summary: Exercise judge against two independently dispatched worktrees.
  severity: low
  affected_area: scaffold
  user_impact: Judging the wrong task's worktree files verdicts against the wrong task.
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: ready
---

# ADR: SilentCritic two-independent-tasks fixture

## Problem Statement

`judge` must resolve the worktree of the task it was asked to judge, not
whichever task dispatched most recently.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

This fixture exists only to exercise `silent_critic::tools::judge` across two
independently dispatched tasks from the crate's own test suite.

## Context

Fix round 2, finding #1.

## Constraints

Use LF line endings.

## Non-Goals

Exercising hidden-criteria concealment; see the sentinel fixture for that.

## Decision

Carry two ready, independent tasks (T001, T002), each with one visible
acceptance check and one `automated` hidden criterion whose check looks
for a marker file unique to that task's own worktree.

## Alternatives Considered

None.

## Consequences

`silent-critic` fails its tests if `judge` ever inspects the wrong task's
worktree.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: First independent task
    status: not_started
    depends_on: []
    description: A task judged against its own worktree.
    invariants:
      - Task one's marker stays present.
    acceptance_checks:
      - Task one's marker file exists.
    hidden_criteria:
      - claim: Task one's marker file is present in task one's worktree.
        criticality: must
        evaluator: automated
        check: test -f task1-marker.txt
        why_hidden: An automated check does not need to be visible to the worker.
        counterfactual: Visible, the worker would special-case this check.
  - id: T002
    title: Second independent task
    status: not_started
    depends_on: []
    description: A task judged against its own worktree.
    invariants:
      - Task two's marker stays present.
    acceptance_checks:
      - Task two's marker file exists.
    hidden_criteria:
      - claim: Task two's marker file is present in task two's worktree.
        criticality: must
        evaluator: automated
        check: test -f task2-marker.txt
        why_hidden: An automated check does not need to be visible to the worker.
        counterfactual: Visible, the worker would special-case this check.
```
<!-- TASK_GRAPH:END -->

# Task Details

## T001 — First independent task

Status: `not_started`

Depends on: none

### Description

A task judged against its own worktree.

### Invariants

- Task one's marker stays present.

### Acceptance Checks

- Task one's marker file exists.

### Completion Evidence

Pending.

## T002 — Second independent task

Status: `not_started`

Depends on: none

### Description

A task judged against its own worktree.

### Invariants

- Task two's marker stays present.

### Acceptance Checks

- Task two's marker file exists.

### Completion Evidence

Pending.
";

/// A harness that writes one marker file (named `marker_name`) into the
/// worktree, distinguishing which task's worktree a dispatch actually ran
/// in.
fn marker_harness(marker_name: &str) -> HarnessSpec {
    HarnessSpec {
        config: DispatchConfig {
            git: test_git_env(),
            silent_critic_mcp_path: PathBuf::from("silent-critic-mcp"),
        },
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_owned(), format!("touch {marker_name}")],
        model: None,
        model_flag: None,
        prompt_via: HarnessPromptVia::BriefPath,
        environment: Vec::new(),
        shell: PathBuf::from("/bin/sh"),
        timeout: CheckTimeout::new(Duration::from_secs(10)),
    }
}

/// Regression test for fix round 2, finding #1: with two tasks dispatched,
/// `judge` must recompute the worktree from the *task being judged*
/// (`dispatch::run_dir(..., task_id).join("worktree")`) rather than reading
/// a single plan-level `Provenance.worktree` field, which the second
/// dispatch would have overwritten.
///
/// T002 is dispatched *after* T001 (so a plan-level binding would name
/// T002's worktree), and only T001 is judged. T001's automated hidden
/// check looks for a marker file only T001's own harness run created; if
/// `judge` inspected T002's worktree instead, the check would fail (no
/// `task1-marker.txt` there) and the recorded verdict would be `fail`
/// rather than `pass`.
#[test]
fn judge_uses_the_judged_tasks_own_worktree_not_the_most_recently_dispatched_one()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up(TWO_INDEPENDENT_TASKS_PLAN)?;

    let dispatch_t001 = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(marker_harness("task1-marker.txt")),
    );
    expect_text(dispatch_t001.call("dispatch", &json!({ "task_id": "T001" })))?;

    // Dispatched second, so a plan-level (rather than task-scoped) binding
    // would now point at T002's worktree.
    let dispatch_t002 = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(marker_harness("task2-marker.txt")),
    );
    expect_text(dispatch_t002.call("dispatch", &json!({ "task_id": "T002" })))?;

    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(
            r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"}],"disposition":"accept","rationale":"all good"}"#,
        )),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let response = expect_text(judging.call("judge", &json!({ "task_id": "T001" })))?;
    let payload: Value = serde_json::from_str(&response)?;
    assert_eq!(
        Some("accept"),
        payload.get("disposition").and_then(Value::as_str)
    );

    let stored = std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?;
    // Find T001's task block specifically so the assertion cannot pass
    // by matching T002's (unjudged) hidden criterion instead.
    let t001_start = stored
        .find("id: T001")
        .ok_or("expected T001 in the stored plan")?;
    let t002_start = stored
        .find("id: T002")
        .ok_or("expected T002 in the stored plan")?;
    let t001_block = &stored[t001_start..t002_start];
    assert!(
        t001_block.contains("verdict: pass"),
        "T001's hidden check must pass against T001's own worktree, where \
         task1-marker.txt exists: {t001_block}"
    );
    assert!(
        !t001_block.contains("verdict: fail"),
        "a fail verdict here means judge inspected the wrong worktree: {t001_block}"
    );

    Ok(())
}

/// Fix round 2, finding #3: `route_verdict` (`src/tools.rs`) calls
/// `Ledger::record_hidden_verdict` with an empty evidence slice on every
/// judge run -- correct, since re-judging must be able to update a hidden
/// criterion's verdict without re-supplying evidence -- but before this
/// fix, `tftio_planner::Mutation::RecordHiddenVerdict` always *replaced*
/// the evidence list wholesale, even with an empty one, so a second judge
/// run silently erased the first run's own judge-provenance evidence
/// (recorded by `record_run_rationale_as_hidden_evidence`).
///
/// Judging T002 twice, with a different run-level rationale each time,
/// must leave both runs' rationale evidence present in the stored plan.
#[test]
fn judging_a_task_twice_never_erases_the_first_runs_hidden_evidence() -> Result<(), Box<dyn Error>>
{
    let plan_body = SENTINEL_PLAN.replacen(
        "        evaluator: human_judgment\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        "        evaluator: agent_evaluated\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );

    let fixture = set_up(&plan_body)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    let first_judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config_with_rationale("RUN-ONE-RATIONALE"));
    expect_text(first_judging.call("judge", &json!({ "task_id": "T002" })))?;

    let second_judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config_with_rationale("RUN-TWO-RATIONALE"));
    expect_text(second_judging.call("judge", &json!({ "task_id": "T002" })))?;

    let stored = std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?;
    assert!(
        stored.contains("RUN-ONE-RATIONALE"),
        "the first run's evidence must survive a second judge run:\n{stored}"
    );
    assert!(
        stored.contains("RUN-TWO-RATIONALE"),
        "the second run's evidence must also be recorded:\n{stored}"
    );

    Ok(())
}

#[test]
fn judge_surfaces_a_malformed_response_from_either_panel_member() -> Result<(), Box<dyn Error>> {
    let plan_body = SENTINEL_PLAN.replacen(
        "        evaluator: human_judgment\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        "        evaluator: agent_evaluated\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );
    let fixture = set_up(&plan_body)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    let good_response = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"fine"},{"criterion_id":"hidden-0","judgment":"pass","rationale":"fine"}],"disposition":"accept","rationale":"fine"}"#;

    // A single misconfigured provider: not even valid JSON, so every retry
    // attempt fails the same way and the retry budget is exhausted.
    let broken_single = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider("not json at all")),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::new(0),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let outcome = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(broken_single)
    .call("judge", &json!({ "task_id": "T002" }));
    assert!(matches!(outcome, ToolOutcome::Failed(_)));

    // A dual panel where the second provider is the one that is broken:
    // exercises the `judge_twice` error path specifically.
    let broken_dual = JudgeConfig {
        providers: JudgeProviders::Dual(
            scripted_judge_provider(good_response),
            scripted_judge_provider("not json at all"),
        ),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::new(0),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let outcome = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(broken_dual)
    .call("judge", &json!({ "task_id": "T002" }));
    assert!(matches!(outcome, ToolOutcome::Failed(_)));

    Ok(())
}

#[test]
fn judge_does_not_flag_scope_the_task_declared_it_would_touch() -> Result<(), Box<dyn Error>> {
    let plan_body = SENTINEL_PLAN.replacen(
        "    description: A task carrying one hidden criterion for the sentinel test.",
        "    description: A task carrying one hidden criterion for the sentinel test.\n    files:\n      likely_read: []\n      likely_modify:\n        - sensitive.txt",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );
    let fixture = set_up(&plan_body)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    let response_json = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"}],"disposition":"accept","rationale":"all clear"}"#;
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(response_json)),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);
    expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;

    let unresolved = silent_critic::ledger::unresolved_items_in(&std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?)?;
    assert!(
        !unresolved.iter().any(|item| matches!(
            item.residual,
            silent_critic::model::Residual::UncoveredChangedScope { .. }
        )),
        "declared scope should not be reported as uncovered: {unresolved:?}"
    );

    Ok(())
}

/// Decision 4, dedicated: a task that declares no `files` block at all
/// (the stock sentinel fixture's own `T002`, every `mode: single` plan's
/// shape) must not have its every changed path read as uncovered. This is
/// independent of the dual-panel test's fixture, which happens to use the
/// same undeclared-scope `T002` but exists to exercise decision 2, not
/// decision 4 -- this test exists to make decision 4 fail on its own if it
/// regresses, with an agreeing dual panel standing in for "a normal clean
/// judge run" rather than for anything dual-panel-specific.
#[test]
fn a_clean_run_with_an_undeclared_file_list_sets_no_attention_flag() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(&agent_evaluated_plan_body())?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    let response = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"},{"criterion_id":"hidden-0","judgment":"pass","rationale":"looks fine to both judges"}],"disposition":"accept","rationale":"clean agreeing panel"}"#;
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Dual(
            scripted_judge_provider(response),
            scripted_judge_provider(response),
        ),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let raw_response = expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;
    let payload: Value = serde_json::from_str(&raw_response)?;
    assert_eq!(
        Some(false),
        payload
            .get("operator_attention_required")
            .and_then(Value::as_bool),
        "an undeclared file list must not, by itself, require operator attention: {payload}"
    );

    let unresolved = silent_critic::ledger::unresolved_items_in(&std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?)?;
    assert!(
        !unresolved.iter().any(|item| matches!(
            item.residual,
            silent_critic::model::Residual::UncoveredChangedScope { .. }
        )),
        "an undeclared file list must not appear as an uncovered-scope item: {unresolved:?}"
    );

    Ok(())
}

#[test]
fn judge_reports_operator_attention_for_pending_human_judgment_and_uncovered_scope()
-> Result<(), Box<dyn Error>> {
    // Decision 4: an *undeclared* scope (no `files` block at all, the
    // stock fixture's own `T002`) no longer flags every changed path as
    // uncovered -- that was the defect. This test instead declares a
    // scope that does not cover what the harness actually touches
    // (`sensitive.txt`), which is the case that must still flag
    // `UncoveredChangedScope`, together with a `human_judgment` hidden
    // criterion left untouched, so a clean `accept` from the judge over
    // the one visible criterion should still surface both classes of
    // operator attention.
    let plan_body = SENTINEL_PLAN.replacen(
        "    description: A task carrying one hidden criterion for the sentinel test.",
        "    description: A task carrying one hidden criterion for the sentinel test.\n    files:\n      likely_read: []\n      likely_modify:\n        - some-other-file.txt",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );
    let fixture = set_up(&plan_body)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    let response_json = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"}],"disposition":"accept","rationale":"all clear"}"#;
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(response_json)),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let response = expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;
    let payload: Value = serde_json::from_str(&response)?;
    assert_eq!(
        Some("accept"),
        payload.get("disposition").and_then(Value::as_str)
    );
    assert_eq!(
        Some(true),
        payload
            .get("operator_attention_required")
            .and_then(Value::as_bool),
        "a pending human_judgment criterion and uncovered changed scope should still \
         require operator attention even on a clean accept: {payload}"
    );

    let unresolved = silent_critic::ledger::unresolved_items_in(&std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?)?;
    assert!(unresolved.iter().any(|item| matches!(
        item.residual,
        silent_critic::model::Residual::AwaitingHumanJudgment { .. }
    )));
    assert!(unresolved.iter().any(|item| matches!(
        item.residual,
        silent_critic::model::Residual::UncoveredChangedScope { .. }
    )));

    Ok(())
}

/// A recorded verdict on a `human_judgment` hidden criterion clears
/// `operator_attention_required` for that reason: the flag gates on
/// `verdict.is_none()`, exactly as `unresolved_items_in` does, not merely on
/// the criterion's evaluator kind.
#[test]
fn recording_a_human_judgment_verdict_clears_operator_attention() -> Result<(), Box<dyn Error>> {
    let plan_body = SENTINEL_PLAN.replacen(
        "    description: A task carrying one hidden criterion for the sentinel test.",
        "    description: A task carrying one hidden criterion for the sentinel test.\n    files:\n      likely_read: []\n      likely_modify:\n        - sensitive.txt",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );
    let fixture = set_up(&plan_body)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    // Simulate an operator having already recorded the `human_judgment`
    // criterion's verdict (index 0) before this judge run -- the tool
    // surface does not expose this directly yet, but the ledger's public
    // API does, exactly as a future operator-facing tool would call it.
    let ledger = Ledger::new(&fixture.store, &fixture.repo_start, &fixture.plan_id);
    ledger.record_hidden_verdict(
        &TaskId::new("T002"),
        0,
        &Verdict::new(
            CriterionId::new("hidden-0"),
            Judgment::Pass,
            NonEmptyString::new("recorded by a human operator before this judge run")?,
        ),
        &[],
    )?;

    // This run's scripted response judges only the visible criterion; the
    // already-resolved `human_judgment` criterion is left untouched.
    let response_json = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"}],"disposition":"accept","rationale":"all clear"}"#;
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(response_json)),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let response = expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;
    let payload: Value = serde_json::from_str(&response)?;
    assert_eq!(
        Some(false),
        payload
            .get("operator_attention_required")
            .and_then(Value::as_bool),
        "a resolved human_judgment criterion should not force operator attention: {payload}"
    );

    let unresolved = silent_critic::ledger::unresolved_items_in(&std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?)?;
    assert!(
        !unresolved.iter().any(|item| matches!(
            item.residual,
            silent_critic::model::Residual::AwaitingHumanJudgment { .. }
        )),
        "expected no AwaitingHumanJudgment residual once the verdict is recorded: {unresolved:?}"
    );

    Ok(())
}

// -- Planning Document Format v2 -----------------------------------------

/// `plan_status` derives each task's `blocks` from `depends_on` rather than
/// reading the document's authored field, which planner 0.4 never
/// populates for a v2 plan. T001 has no dependents authored (v2 carries no
/// `blocks` field at all) but T002 depends on it, so `plan_status` must
/// still report T001 as blocking T002.
#[test]
fn plan_status_lists_dependents_for_a_v2_plan() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(V2_SENTINEL_PLAN)?;
    let tools = orchestrator(&fixture);

    let text = expect_text(tools.call("plan_status", &json!({})))?;
    let payload: Value = serde_json::from_str(&text)?;
    let tasks = payload
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or("expected a tasks array")?;

    let t001 = tasks
        .iter()
        .find(|task| task.get("id").and_then(Value::as_str) == Some("T001"))
        .ok_or("expected task T001")?;
    let blocks: Vec<&str> = t001
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or("expected a blocks array")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        vec!["T002"],
        blocks,
        "T001 should report T002 as a dependent, derived from depends_on: {payload}"
    );

    let t002 = tasks
        .iter()
        .find(|task| task.get("id").and_then(Value::as_str) == Some("T002"))
        .ok_or("expected task T002")?;
    let t002_blocks = t002
        .get("blocks")
        .and_then(Value::as_array)
        .ok_or("expected a blocks array")?;
    assert!(t002_blocks.is_empty(), "T002 has no dependents: {payload}");

    Ok(())
}

/// On a v2 plan `judge` never runs the uncovered-changed-scope check
/// (`files.likely_modify` does not exist in the format), even when the
/// dispatched worktree has real changed paths that a v1 plan's check would
/// flag. It must instead record an explicit "did not run" note -- visible
/// in the plan's Operator Guidance Log and in the tool response's
/// `scope_check` field -- and that note must never surface as an
/// `UncoveredChangedScope` residual or set `operator_attention_required`.
#[test]
fn judge_reports_scope_check_did_not_run_for_a_v2_plan_with_changed_paths()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up(V2_SENTINEL_PLAN)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    let response_json = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"}],"disposition":"accept","rationale":"all clear"}"#;
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(response_json)),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let response = expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;
    let payload: Value = serde_json::from_str(&response)?;
    assert_eq!(
        Some("not_run"),
        payload.get("scope_check").and_then(Value::as_str),
        "a v2 plan must report the scope check as not run: {payload}"
    );
    assert_eq!(
        Some(false),
        payload
            .get("operator_attention_required")
            .and_then(Value::as_bool),
        "the scope check not running on a v2 plan must not, by itself, require \
         operator attention: {payload}"
    );

    let plan_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?;
    let plan_source = std::fs::read_to_string(&plan_path)?;
    assert!(
        plan_source.contains("NOTE-BEGIN scope_check_not_run"),
        "the plan's Operator Guidance Log should carry an explicit \
         scope-check-not-run note: {plan_source}"
    );

    let unresolved = silent_critic::ledger::unresolved_items_in(&plan_source)?;
    assert!(
        !unresolved.iter().any(|item| matches!(
            item.residual,
            silent_critic::model::Residual::UncoveredChangedScope { .. }
        )),
        "a v2 plan must never produce an UncoveredChangedScope residual: {unresolved:?}"
    );

    Ok(())
}

#[test]
fn judge_records_disagreement_and_undetermined_judgments_with_a_dual_panel()
-> Result<(), Box<dyn Error>> {
    let plan_body = SENTINEL_PLAN.replacen(
        "        evaluator: human_judgment\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        "        evaluator: agent_evaluated\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );
    let fixture = set_up(&plan_body)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    let response_a = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"undetermined","rationale":"cannot tell from the diff alone","evidence_needed":"operator confirmation"},{"criterion_id":"hidden-0","judgment":"undetermined","rationale":"cannot tell if it was weakened","evidence_needed":"a manual review"}],"disposition":"needs_operator","rationale":"first judge is unsure"}"#;
    let response_b = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"undetermined","rationale":"also unsure","evidence_needed":"operator confirmation"},{"criterion_id":"hidden-0","judgment":"fail","rationale":"looks weakened to me"}],"disposition":"accept","rationale":"second judge disagrees"}"#;

    let judge_config = JudgeConfig {
        providers: JudgeProviders::Dual(
            scripted_judge_provider(response_a),
            scripted_judge_provider(response_b),
        ),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let response = expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;
    let payload: Value = serde_json::from_str(&response)?;
    // The first (canonical) provider's disposition and verdicts are what is
    // returned and recorded; the second provider's disagreement is a
    // residual, not a second answer.
    assert_eq!(
        Some("needs_operator"),
        payload.get("disposition").and_then(Value::as_str)
    );
    assert_eq!(
        Some(true),
        payload
            .get("operator_attention_required")
            .and_then(Value::as_bool)
    );
    let judgments = payload
        .get("judgments")
        .and_then(Value::as_array)
        .ok_or("expected a judgments array")?;
    assert_eq!(
        Some("undetermined"),
        judgments
            .first()
            .and_then(|j| j.get("judgment"))
            .and_then(Value::as_str)
    );

    // The disagreement is on `hidden-0`: it never round-trips through
    // `unresolved_items_in` (the guidance log carries no criterion id or
    // rationale for a hidden disagreement, by design), so the record of it
    // lives on the criterion itself as evidence instead.
    let stored = std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?;
    let plan = tftio_planner::parse_markdown(&stored)?;
    let task = plan
        .tasks
        .iter()
        .find(|task| task.id.as_str() == "T002")
        .ok_or("plan has no T002")?;
    let criterion = task
        .hidden_criteria
        .first()
        .ok_or("plan lost hidden_criteria[0]")?;
    let has = |text: &str| {
        criterion
            .evidence
            .iter()
            .any(|record| record.summary.contains(text) && record.provenance == "judge")
    };
    assert!(has("cannot tell if it was weakened") && has("looks weakened to me"));
    // Finding #6: no guidance-log line at all for a hidden disagreement.
    let guidance_log = plan.operator_guidance_log.unwrap_or_default();
    assert!(
        !guidance_log.contains("judge disagreement")
            && !guidance_log.contains("cannot tell if it was weakened")
            && !guidance_log.contains("looks weakened to me")
            && !guidance_log.contains("hidden-0"),
        "{guidance_log:?}"
    );

    Ok(())
}

/// Decision 2: a dual panel that *agrees* on every criterion must still be
/// visible on the stored plan -- before this change, agreement left no
/// trace at all (no `Residual::JudgeDisagreement`, no evidence pair), so
/// `measure` could not tell a single-provider run from a dual-provider run
/// that agreed. This also extends the containment sentinel: the two
/// judge-provenance evidence records land only inside `hidden_criteria`
/// (stripped by projection), never in the `judge` tool's own response.
/// Flip `T002`'s `human_judgment` hidden criterion to `agent_evaluated`
/// (so the judge renders a verdict for it) and give its `automated` sibling
/// a trivially-passing `check`, so a dispatched-and-judged `T002` leaves no
/// unrelated unresolved item to mask whether
/// `operator_attention_required` reflects the panel itself.
fn agent_evaluated_plan_body() -> String {
    let plan_body = SENTINEL_PLAN.replacen(
        "        evaluator: human_judgment\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        "        evaluator: agent_evaluated\n        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );
    let plan_body = plan_body.replacen(
        "        check: SENTINEL-7f3a9c run the sentinel check command.",
        "        check: \"true\"",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace (automated check) was not found"
    );
    plan_body
}

/// Assert that `hidden_criteria[0]`'s evidence carries a judge-provenance
/// record for `provider_id` with judgment `judged`, in the `"judge <id>
/// judged <judgment>"` form [`silent_critic::ledger::judge_provenance_evidence`]
/// writes.
fn assert_has_judge_provenance(
    task: &tftio_planner::model::OperatorTask,
    provider_id: &str,
    judged: &str,
) -> Result<(), Box<dyn Error>> {
    let criterion = task
        .hidden_criteria
        .first()
        .ok_or("plan lost hidden_criteria[0]")?;
    let text = format!("judge {provider_id} judged {judged}");
    assert!(
        criterion
            .evidence
            .iter()
            .any(|record| record.summary.contains(&text) && record.provenance == "judge"),
        "expected {text:?} in hidden_criteria[0]'s evidence: {:?}",
        criterion.evidence
    );
    Ok(())
}

#[test]
fn judge_records_dual_panel_evidence_when_both_providers_agree_and_measure_reports_zero_disagreements()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up(&agent_evaluated_plan_body())?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    // Both providers return the identical verdicts: a clean agreeing dual
    // panel, with the sentinel embedded in the hidden criterion's
    // rationale so a leak into the response would be caught.
    let response = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"},{"criterion_id":"hidden-0","judgment":"pass","rationale":"SENTINEL-7f3a9c both judges agree this is fine"}],"disposition":"accept","rationale":"clean agreeing panel"}"#;

    let judge_config = JudgeConfig {
        providers: JudgeProviders::Dual(
            scripted_judge_provider(response),
            scripted_judge_provider(response),
        ),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    let raw_response = expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;
    assert!(
        !raw_response.contains(SENTINEL),
        "the judge tool's own response must never carry hidden-criterion material: {raw_response}"
    );
    let payload: Value = serde_json::from_str(&raw_response)?;
    assert_eq!(
        Some("accept"),
        payload.get("disposition").and_then(Value::as_str)
    );
    assert_eq!(
        Some(false),
        payload
            .get("operator_attention_required")
            .and_then(Value::as_bool)
    );

    let stored = std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?;
    let plan = tftio_planner::parse_markdown(&stored)?;
    let task = plan
        .tasks
        .iter()
        .find(|task| task.id.as_str() == "T002")
        .ok_or("plan has no T002")?;
    assert_has_judge_provenance(task, "judge-a", "pass")?;
    assert_has_judge_provenance(task, "judge-b", "pass")?;

    // No guidance-log entry either -- agreement is not a residual.
    let guidance_log = plan.operator_guidance_log.unwrap_or_default();
    assert!(
        !guidance_log.contains("judge disagreement"),
        "{guidance_log:?}"
    );

    // The measurement of record: a dual panel that agreed on everything
    // reports zero disagreements, not "no dual panel".
    let measurement = silent_critic::measure::measure(&fixture.plan_id, &stored)?;
    assert_eq!(
        silent_critic::measure::JudgeDisagreementRate::DualPanel {
            judged: 1,
            disagreements: 0,
        },
        measurement.judge_disagreement
    );

    Ok(())
}

#[test]
fn judge_records_a_passing_automated_hidden_check() -> Result<(), Box<dyn Error>> {
    let plan_body = SENTINEL_PLAN.replacen(
        "        check: SENTINEL-7f3a9c run the sentinel check command.",
        "        check: \"true\"",
        1,
    );
    assert_ne!(
        plan_body, SENTINEL_PLAN,
        "fixture text to replace was not found"
    );
    let fixture = set_up(&plan_body)?;

    let dispatching = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        Some(scripted_harness()),
    );
    expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

    let response_json = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"looks fine"}],"disposition":"accept","rationale":"all clear"}"#;
    let judge_config = JudgeConfig {
        providers: JudgeProviders::Single(scripted_judge_provider(response_json)),
        retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
        git: test_git_invocation(),
        check_shell: PathBuf::from("/bin/sh"),
        check_environment: CheckEnvironment::default(),
        check_timeout: CheckTimeout::new(Duration::from_secs(10)),
    };
    let judging = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        None,
    )
    .with_judge(judge_config);

    expect_text(judging.call("judge", &json!({ "task_id": "T002" })))?;

    let stored = std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?;
    let plan = tftio_planner::parse_markdown(&stored)?;
    let task = plan
        .tasks
        .iter()
        .find(|task| task.id.as_str() == "T002")
        .ok_or("plan lost its task")?;
    assert_eq!(
        Some(tftio_planner::model::HiddenVerdictJudgment::Pass),
        task.hidden_criteria.get(1).and_then(|c| c.verdict)
    );

    Ok(())
}

#[test]
fn plan_status_shape_is_identical_with_and_without_hidden_criteria() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let tools = orchestrator(&fixture);

    let text = expect_text(tools.call("plan_status", &json!({})))?;
    let payload: Value = serde_json::from_str(&text)?;
    let tasks = payload
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or("expected a tasks array")?;
    assert_eq!(2, tasks.len());

    let mut key_sets: Vec<Vec<String>> = tasks
        .iter()
        .map(|task| {
            let mut keys: Vec<String> = task
                .as_object()
                .into_iter()
                .flat_map(serde_json::Map::keys)
                .cloned()
                .collect();
            keys.sort();
            keys
        })
        .collect();
    key_sets.dedup();
    assert_eq!(
        1,
        key_sets.len(),
        "every task's plan_status shape must be identical regardless of hidden criteria"
    );

    Ok(())
}

// -- gating -------------------------------------------------------------

#[test]
fn worker_token_is_rejected_on_every_orchestrator_tool() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let plan_id = PlanId::new(fixture.plan_id.clone());
    let mut registry = TokenRegistry::new(plan_id.clone());
    let task = TaskId::new("T002");
    let token = registry.mint(Role::Worker { task: task.clone() }, &plan_id)?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &token,
    );
    assert_eq!(Some(&Role::Worker { task: task.clone() }), worker.caller());

    for name in [
        "plan_status",
        "next_ready",
        "dispatch",
        "judge",
        "record_decision",
        "request_guidance",
    ] {
        let outcome = worker.call(name, &json!({}));
        assert!(
            matches!(outcome, ToolOutcome::Failed(_)),
            "{name} was not rejected"
        );
    }
    assert!(worker.specs().iter().all(|spec| {
        ![
            "plan_status",
            "next_ready",
            "dispatch",
            "judge",
            "record_decision",
            "request_guidance",
        ]
        .contains(&spec.name)
    }));

    Ok(())
}

#[test]
fn a_token_for_another_task_is_rejected_on_worker_tools() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let plan_id = PlanId::new(fixture.plan_id.clone());
    let mut registry = TokenRegistry::new(plan_id.clone());
    let token_for_t002 = registry.mint(
        Role::Worker {
            task: TaskId::new("T002"),
        },
        &plan_id,
    )?;

    // A worker session nominally for T001, presenting T002's token.
    let mismatched = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &TaskId::new("T001"),
        &token_for_t002,
    );
    assert_eq!(None, mismatched.caller());

    for (name, arguments) in [
        ("brief", json!({})),
        ("note", json!({ "text": "hello" })),
        ("submit", json!({ "summary": "done" })),
    ] {
        let outcome = mismatched.call(name, &arguments);
        assert!(matches!(outcome, ToolOutcome::Failed(_)));
    }
    assert!(mismatched.specs().is_empty());

    // An entirely unknown token is rejected the same way.
    let unknown = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &TaskId::new("T002"),
        &Token::new("silent_critic_worker_unknown"),
    );
    assert_eq!(None, unknown.caller());
    assert!(matches!(
        unknown.call("brief", &json!({})),
        ToolOutcome::Failed(_)
    ));

    // An orchestrator-role token presented to `worker()` is rejected too.
    let orchestrator_token = registry.mint(Role::Orchestrator, &plan_id)?;
    let orchestrator_as_worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &TaskId::new("T002"),
        &orchestrator_token,
    );
    assert_eq!(None, orchestrator_as_worker.caller());

    Ok(())
}

#[test]
fn a_token_minted_for_a_different_plan_is_rejected_even_with_a_colliding_task_id()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;

    let plan_a = PlanId::new("plan-a");
    let plan_b = PlanId::new("plan-b");
    let colliding_task = TaskId::new("T001");

    let mut registry_a = TokenRegistry::new(plan_a.clone());
    let token_from_plan_a = registry_a.mint(
        Role::Worker {
            task: colliding_task.clone(),
        },
        &plan_a,
    )?;

    // A second registry, scoped to plan B, minting for the very same task
    // id: two distinct registries genuinely collide on a task id, not just
    // two plan-id strings compared in isolation.
    let mut registry_b = TokenRegistry::new(plan_b.clone());
    let _token_from_plan_b = registry_b.mint(
        Role::Worker {
            task: colliding_task.clone(),
        },
        &plan_b,
    )?;

    // A `Tools` surface constructed for plan B, but authenticated against
    // plan A's registry with plan A's token for the colliding task id, must
    // be rejected — even though the task id matches and the token itself
    // validates fine against `registry_a` in isolation.
    let cross_plan = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        plan_b.as_str().to_owned(),
        &registry_a,
        &colliding_task,
        &token_from_plan_a,
    );
    assert_eq!(None, cross_plan.caller());
    assert!(matches!(
        cross_plan.call("brief", &json!({})),
        ToolOutcome::Failed(_)
    ));

    // Sanity: the same token does authenticate a surface correctly labeled
    // with plan A's own id, so the rejection above is specifically about
    // the plan mismatch, not some other defect.
    let same_plan = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        plan_a.as_str().to_owned(),
        &registry_a,
        &colliding_task,
        &token_from_plan_a,
    );
    assert_eq!(
        Some(&Role::Worker {
            task: colliding_task
        }),
        same_plan.caller()
    );

    Ok(())
}

#[test]
fn orchestrator_tools_reject_unknown_names() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let tools = orchestrator(&fixture);
    assert!(matches!(
        tools.call("brief", &json!({})),
        ToolOutcome::Failed(_)
    ));
    Ok(())
}

// -- brief byte-identity --------------------------------------------

#[test]
fn brief_is_byte_identical_to_planners_worker_projection() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let plan_id = PlanId::new(fixture.plan_id.clone());
    let mut registry = TokenRegistry::new(plan_id.clone());
    let task = TaskId::new("T002");
    let token = registry.mint(Role::Worker { task: task.clone() }, &plan_id)?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &token,
    );

    let actual = expect_text(worker.call("brief", &json!({})))?;
    let expected = tftio_planner::project_worker_markdown(SENTINEL_PLAN)?;
    assert_eq!(expected, actual);
    assert!(!actual.contains(SENTINEL));

    Ok(())
}

// -- worker note/submit ------------------------------------------------

fn worker_tools(
    fixture: &Fixture,
    task_id: &str,
) -> Result<(TokenRegistry, Token, TaskId), Box<dyn Error>> {
    let plan_id = PlanId::new(fixture.plan_id.clone());
    let mut registry = TokenRegistry::new(plan_id.clone());
    let task = TaskId::new(task_id);
    let token = registry.mint(Role::Worker { task: task.clone() }, &plan_id)?;
    Ok((registry, token, task))
}

#[test]
fn note_and_submit_record_worker_narrated_evidence() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (registry, token, task) = worker_tools(&fixture, "T002")?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &token,
    );

    let note_outcome = worker.call("note", &json!({ "text": "started digging" }));
    assert_eq!(ToolOutcome::Text("noted".to_owned()), note_outcome);

    let submit_outcome = worker.call("submit", &json!({ "summary": "all done" }));
    assert_eq!(ToolOutcome::Text("submitted".to_owned()), submit_outcome);

    let state_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?
        .parent()
        .ok_or("plan path has no parent")?
        .join("run")
        .join("T002.toml");
    let state_body = std::fs::read_to_string(state_path)?;
    assert!(state_body.contains("started digging"));
    assert!(state_body.contains("all done"));

    Ok(())
}

#[test]
fn concurrent_notes_across_two_tasks_are_never_lost() -> Result<(), Box<dyn Error>> {
    const NOTES_PER_TASK: usize = 8;

    let fixture = set_up(SENTINEL_PLAN)?;
    let plan_id = PlanId::new(fixture.plan_id.clone());
    let mut registry = TokenRegistry::new(plan_id.clone());
    let task_a = TaskId::new("T001");
    let task_b = TaskId::new("T002");
    let token_a = registry.mint(
        Role::Worker {
            task: task_a.clone(),
        },
        &plan_id,
    )?;
    let token_b = registry.mint(
        Role::Worker {
            task: task_b.clone(),
        },
        &plan_id,
    )?;

    let worker_a = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task_a,
        &token_a,
    );
    let worker_b = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task_b,
        &token_b,
    );

    std::thread::scope(|scope| {
        for i in 0..NOTES_PER_TASK {
            let worker_a = &worker_a;
            scope.spawn(move || {
                let outcome = worker_a.call("note", &json!({ "text": format!("note-a-{i}") }));
                assert_eq!(ToolOutcome::Text("noted".to_owned()), outcome);
            });
            let worker_b = &worker_b;
            scope.spawn(move || {
                let outcome = worker_b.call("note", &json!({ "text": format!("note-b-{i}") }));
                assert_eq!(ToolOutcome::Text("noted".to_owned()), outcome);
            });
        }
    });

    let plan_dir = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?
        .parent()
        .ok_or("plan path has no parent")?
        .to_path_buf();
    let run_dir = plan_dir.join("run");
    let state_a = std::fs::read_to_string(run_dir.join("T001.toml"))?;
    let state_b = std::fs::read_to_string(run_dir.join("T002.toml"))?;
    for i in 0..NOTES_PER_TASK {
        assert!(
            state_a.contains(&format!("note-a-{i}")),
            "missing note-a-{i} in {state_a}"
        );
        assert!(
            state_b.contains(&format!("note-b-{i}")),
            "missing note-b-{i} in {state_b}"
        );
    }

    Ok(())
}

#[test]
fn note_rejects_blank_and_missing_text() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (registry, token, task) = worker_tools(&fixture, "T002")?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &token,
    );

    assert!(matches!(
        worker.call("note", &json!({})),
        ToolOutcome::Failed(_)
    ));
    assert!(matches!(
        worker.call("note", &json!({ "text": "   " })),
        ToolOutcome::Failed(_)
    ));
    Ok(())
}

#[test]
fn note_rejects_a_task_id_that_is_unsafe_as_a_filename() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let plan_id = PlanId::new(fixture.plan_id.clone());
    let mut registry = TokenRegistry::new(plan_id.clone());
    let unsafe_task = TaskId::new("../escape");
    let token = registry.mint(
        Role::Worker {
            task: unsafe_task.clone(),
        },
        &plan_id,
    )?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &unsafe_task,
        &token,
    );

    assert!(matches!(
        worker.call("note", &json!({ "text": "hello" })),
        ToolOutcome::Failed(_)
    ));
    Ok(())
}

#[test]
fn load_run_state_reports_malformed_sidecars() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (registry, token, task) = worker_tools(&fixture, "T002")?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &token,
    );
    let plan_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?;
    let run_dir = plan_path
        .parent()
        .ok_or("plan path has no parent")?
        .join("run");
    std::fs::create_dir_all(&run_dir)?;
    std::fs::write(run_dir.join("T002.toml"), "not = [valid toml")?;

    let outcome = worker.call("note", &json!({ "text": "hello" }));
    assert!(matches!(outcome, ToolOutcome::Failed(_)));
    Ok(())
}

#[test]
fn save_run_state_reports_create_dir_failures() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let (registry, token, task) = worker_tools(&fixture, "T002")?;
    let worker = Tools::worker(
        &fixture.store,
        fixture.repo_start.clone(),
        fixture.plan_id.clone(),
        &registry,
        &task,
        &token,
    );
    let plan_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?;
    let plan_dir = plan_path.parent().ok_or("plan path has no parent")?;
    // A plain file where the `run/` directory needs to go blocks
    // `create_dir_all`.
    std::fs::write(plan_dir.join("run"), "not a directory")?;

    let outcome = worker.call("note", &json!({ "text": "hello" }));
    assert!(matches!(outcome, ToolOutcome::Failed(_)));
    Ok(())
}

// -- record_decision / request_guidance mutate the stored plan ---------

#[test]
fn record_decision_mutates_and_the_result_still_validates() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(VALID_PLAN)?;
    let tools = orchestrator(&fixture);

    let outcome = tools.call(
        "record_decision",
        &json!({ "title": "Use approach X", "body": "Because it is simpler." }),
    );
    assert!(matches!(outcome, ToolOutcome::Text(_)));

    let plan_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?;
    let updated = std::fs::read_to_string(&plan_path)?;
    assert!(updated.contains("Use approach X"));
    assert!(updated.contains("Because it is simpler."));

    // The store always names the stored file `plan.md`; `validate_markdown_path`
    // separately checks the filename shape, which the store's own name never
    // satisfies. Pair the mutated content with a properly-shaped filename
    // (matching the plan's original name before it entered the store) so this
    // checks only what the mutation changed.
    let name = Path::new("2026-09-05-store-plan.md");
    let report = tftio_planner::validate_markdown_path(&updated, name)?;
    assert!(report.is_valid(), "{:?}", report.diagnostics);

    Ok(())
}

#[test]
fn record_decision_rejects_missing_arguments() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(VALID_PLAN)?;
    let tools = orchestrator(&fixture);
    assert!(matches!(
        tools.call("record_decision", &json!({ "title": "Only a title" })),
        ToolOutcome::Failed(_)
    ));
    Ok(())
}

#[test]
fn request_guidance_blocks_a_blockable_task_and_notes_unblockable_ones()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let tools = orchestrator(&fixture);

    // T001 is `done` (not blockable); T002 is `not_started` (also not
    // blockable, since only `ready`/`in_progress` tasks can be
    // blocked) — exercising the "recorded, not blocked" path.
    let outcome = tools.call(
        "request_guidance",
        &json!({
            "question": "Which approach?",
            "affected_tasks": ["T001", "T002", "does-not-exist"],
        }),
    );
    let text = expect_text(outcome)?;
    assert!(text.contains("could not block"));

    let plan_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?;
    let updated = std::fs::read_to_string(&plan_path)?;
    assert!(updated.contains("Which approach?"));
    let name = Path::new("2026-09-05-hidden-sentinel-plan.md");
    let report = tftio_planner::validate_markdown_path(&updated, name)?;
    assert!(report.is_valid(), "{:?}", report.diagnostics);

    Ok(())
}

#[test]
fn request_guidance_blocks_a_ready_task() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let tools = orchestrator(&fixture);

    // First mark T002 as started (in_progress is blockable), via a raw
    // planner mutation so this test does not depend on `dispatch` (still a
    // placeholder in T005).
    let plan_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?;
    let source = std::fs::read_to_string(&plan_path)?;
    let ready_request = tftio_planner::MutationRequest {
        date: "2026-09-05".to_owned(),
        mutation: tftio_planner::Mutation::Task {
            task_id: tftio_planner::model::TaskId::parse("T002")?,
            action: tftio_planner::TaskAction::Ready,
        },
    };
    let prepared = tftio_planner::prepare_markdown_mutation(&source, &ready_request)?;
    tftio_planner::apply_prepared(&plan_path, &prepared)?;
    let source = prepared.replacement().to_owned();
    let start_request = tftio_planner::MutationRequest {
        date: "2026-09-05".to_owned(),
        mutation: tftio_planner::Mutation::Task {
            task_id: tftio_planner::model::TaskId::parse("T002")?,
            action: tftio_planner::TaskAction::Start,
        },
    };
    let prepared = tftio_planner::prepare_markdown_mutation(&source, &start_request)?;
    tftio_planner::apply_prepared(&plan_path, &prepared)?;

    let outcome = tools.call(
        "request_guidance",
        &json!({ "question": "Proceed?", "affected_tasks": ["T002"] }),
    );
    let text = expect_text(outcome)?;
    assert!(text.contains("blocked tasks: T002"));
    assert!(!text.contains(SENTINEL));

    Ok(())
}

#[test]
fn request_guidance_with_no_affected_tasks_only_records_the_question() -> Result<(), Box<dyn Error>>
{
    let fixture = set_up(VALID_PLAN)?;
    let tools = orchestrator(&fixture);
    let outcome = tools.call(
        "request_guidance",
        &json!({ "question": "Any concerns?", "affected_tasks": [] }),
    );
    assert_eq!(
        ToolOutcome::Text("recorded guidance request".to_owned()),
        outcome
    );
    Ok(())
}

#[test]
fn request_guidance_rejects_missing_affected_tasks() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(VALID_PLAN)?;
    let tools = orchestrator(&fixture);
    assert!(matches!(
        tools.call("request_guidance", &json!({ "question": "Any concerns?" })),
        ToolOutcome::Failed(_)
    ));
    Ok(())
}

// -- misc error paths ----------------------------------------------------

#[test]
fn read_plan_source_reports_unreadable_plans() -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = set_up(VALID_PLAN)?;
        let plan_path = fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?;
        let mut permissions = std::fs::metadata(&plan_path)?.permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&plan_path, permissions.clone())?;

        let tools = orchestrator(&fixture);
        let outcome = tools.call("plan_status", &json!({}));

        permissions.set_mode(0o644);
        std::fs::set_permissions(&plan_path, permissions)?;

        assert!(matches!(outcome, ToolOutcome::Failed(_)));
    }
    Ok(())
}

#[test]
fn judge_propagates_a_ledger_write_failure_for_a_visible_verdict() -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        // Flip both hidden criteria to `agent_evaluated` so no automated
        // check ever runs: the very first ledger write `judge` attempts is
        // then the visible verdict's, inside `route_verdict`, isolating
        // that specific call site's failure from the automated-check one.
        let plan_body = SENTINEL_PLAN
            .replacen("evaluator: human_judgment", "evaluator: agent_evaluated", 1)
            .replacen("evaluator: automated", "evaluator: agent_evaluated", 1);
        assert_ne!(
            plan_body, SENTINEL_PLAN,
            "fixture text to replace was not found"
        );
        let fixture = set_up(&plan_body)?;

        let dispatching = Tools::orchestrator(
            &fixture.store,
            fixture.repo_start.clone(),
            fixture.plan_id.clone(),
            Some(scripted_harness()),
        );
        expect_text(dispatching.call("dispatch", &json!({ "task_id": "T002" })))?;

        let response_json = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"fine"},{"criterion_id":"hidden-0","judgment":"pass","rationale":"fine"},{"criterion_id":"hidden-1","judgment":"pass","rationale":"fine"}],"disposition":"accept","rationale":"all clear"}"#;
        let judge_config = JudgeConfig {
            providers: JudgeProviders::Single(scripted_judge_provider(response_json)),
            retry_budget: silent_critic::evaluate::judge::RetryBudget::default(),
            git: test_git_invocation(),
            check_shell: PathBuf::from("/bin/sh"),
            check_environment: CheckEnvironment::default(),
            check_timeout: CheckTimeout::new(Duration::from_secs(10)),
        };

        let plan_dir = fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?
            .parent()
            .ok_or("plan path has no parent")?
            .to_path_buf();
        let mut permissions = std::fs::metadata(&plan_dir)?.permissions();
        let original_mode = permissions.mode();
        permissions.set_mode(0o555);
        std::fs::set_permissions(&plan_dir, permissions.clone())?;

        let judging = Tools::orchestrator(
            &fixture.store,
            fixture.repo_start.clone(),
            fixture.plan_id.clone(),
            None,
        )
        .with_judge(judge_config);
        let outcome = judging.call("judge", &json!({ "task_id": "T002" }));

        permissions.set_mode(original_mode);
        std::fs::set_permissions(&plan_dir, permissions)?;

        assert!(
            matches!(outcome, ToolOutcome::Failed(_)),
            "expected judge to fail when the plan directory cannot accept a lock file"
        );
    }
    Ok(())
}

#[test]
fn plan_path_reports_a_missing_plan() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(VALID_PLAN)?;
    let tools = Tools::orchestrator(
        &fixture.store,
        fixture.repo_start.clone(),
        "does-not-exist".to_owned(),
        None,
    );
    assert!(matches!(
        tools.call("plan_status", &json!({})),
        ToolOutcome::Failed(_)
    ));
    Ok(())
}

// -- direct `Ledger` public-API coverage ---------------------------------
//
// `silent_critic::ledger::Ledger`'s methods are otherwise reached only indirectly
// through `judge()` (via `route_verdict` in `src/tools.rs`) in this file's
// other tests, or directly from `src/ledger.rs`'s own inline `#[cfg(test)]`
// unit tests. Rust compiles the library twice -- once with `--cfg test`
// (linked only into the library's own unit-test binary) and once without
// (linked into every integration test and both binaries) -- so a public
// method called only from one side never accumulates an execution count on
// the other side's separately-compiled copy. This test calls `Ledger`'s
// public API directly, from an integration test (the non-`--cfg test`
// build), so methods `src/ledger.rs`'s own unit tests already cover from
// the other side are covered from this side too.
#[test]
fn ledger_public_api_is_reachable_directly_from_an_integration_test() -> Result<(), Box<dyn Error>>
{
    let fixture = set_up(SENTINEL_PLAN)?;
    let ledger = Ledger::new(&fixture.store, &fixture.repo_start, &fixture.plan_id);

    let task_id = TaskId::new("T001");
    let verdict = Verdict::new(
        CriterionId::new("visible-0"),
        Judgment::Pass,
        NonEmptyString::new("looks fine from an integration test")?,
    );
    ledger.record_visible_verdict(&task_id, &verdict)?;

    let source = std::fs::read_to_string(
        fixture
            .store
            .plan_path(&fixture.repo_start, &fixture.plan_id)?,
    )?;
    assert!(source.contains("looks fine from an integration test"));

    let unresolved = ledger.unresolved_items()?;
    assert!(
        unresolved.iter().any(|item| matches!(
            item.residual,
            silent_critic::model::Residual::AwaitingHumanJudgment { .. }
        )),
        "expected T002's human_judgment hidden criterion to still be awaiting the operator"
    );

    Ok(())
}

#[test]
fn ledger_reclaims_a_stale_lock_left_by_a_crashed_writer() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(SENTINEL_PLAN)?;
    let plan_path = fixture
        .store
        .plan_path(&fixture.repo_start, &fixture.plan_id)?;
    let lock_path = plan_path.with_extension("md.lock");
    std::fs::write(&lock_path, "left behind by a crashed writer")?;
    let stale_time = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
    let lock_file = std::fs::File::open(&lock_path)?;
    lock_file.set_modified(stale_time)?;

    let ledger = Ledger::new(&fixture.store, &fixture.repo_start, &fixture.plan_id);
    let task_id = TaskId::new("T001");
    let verdict = Verdict::new(
        CriterionId::new("visible-0"),
        Judgment::Pass,
        NonEmptyString::new("written after reclaiming a stale lock")?,
    );
    ledger.record_visible_verdict(&task_id, &verdict)?;

    assert!(
        !lock_path.exists(),
        "the lock should be released after the write"
    );
    let source = std::fs::read_to_string(&plan_path)?;
    assert!(source.contains("written after reclaiming a stale lock"));
    Ok(())
}
