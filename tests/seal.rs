//! Integration tests for `silent_critic::seal` and `silent_critic::render`, driven
//! entirely through the crate's public API.

use std::error::Error;
use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;

use silent_critic::ledger::Ledger;
use silent_critic::render::SealDate;
use silent_critic::seal::{self, BurnedThreshold, SealError};
use silent_critic::store::{Store, StoreRoot};

const FULL_PLAN: &str = include_str!("fixtures/2026-09-06-sealed-full-plan.md");
const OPEN_TASK_PLAN: &str = include_str!("fixtures/2026-09-05-hidden-sentinel-plan.md");

/// Reading `PATH` here is test code finding the real `git` binary, not the
/// library reading its own environment.
#[allow(clippy::disallowed_methods)]
fn test_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// Run `git` against `dir` with a hermetic environment: `env_clear()` plus
/// an explicit, minimal environment. `env_remove("GIT_DIR")`/
/// `env_remove("GIT_WORK_TREE")` alone is not enough -- git also exports
/// `GIT_INDEX_FILE` into any process it spawns as a hook (this crate's own
/// pre-commit hook runs `cargo nextest`), and a test spawning `git` for its
/// own temporary repository without clearing the full environment then
/// operates against the outer repository's index/worktree instead of its
/// own, corrupting it.
fn git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_clear()
        .env("PATH", test_path())
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()?;
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

struct Fixture {
    _temp: tempfile::TempDir,
    store: Store,
    repo: std::path::PathBuf,
    plan_id: String,
}

fn set_up(plan_source: &str, file_name: &str) -> Result<Fixture, Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    init_repo(&repo)?;

    let store_root = temp.path().join("store");
    let store = Store::new(StoreRoot::new(store_root));

    let plan_path = temp.path().join(file_name);
    std::fs::write(&plan_path, plan_source)?;
    let plan_id = store.add_plan(&plan_path, &repo, "HEAD")?;

    Ok(Fixture {
        _temp: temp,
        store,
        repo,
        plan_id: plan_id.as_str().to_owned(),
    })
}

fn minimal_plan(plan_id: &str, claim: &str) -> String {
    format!(
        r#"---
plan_format_version: 1
plan_id: {plan_id}
title: Disclosure fixture {plan_id}
status: approved
mode: single
created_at: 2026-09-06
updated_at: 2026-09-06
owner: test
source:
  type: manual
  url: null
  external_id: null
  imported_at: null
bug:
  summary: Exercise disclosure counting.
  severity: low
  affected_area: scaffold
  user_impact: none
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: executing
---

# ADR: Disclosure fixture {plan_id}

## Problem Statement

Problem statement text.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

Discussion summary text.

## Context

Context text.

## Constraints

Constraints text.

## Non-Goals

Non-goals text.

## Decision

Decision text.

## Alternatives Considered

Alternatives text.

## Consequences

Consequences text.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: Do the shared thing
    status: done
    depends_on: []
    description: A task carrying the shared disclosure claim.
    invariants:
      - The shared thing stays done.
    acceptance_checks:
      - The shared thing is done.
    hidden_criteria:
      - claim: {claim}
        criticality: must
        evaluator: automated
        check: run the shared check.
        why_hidden: visible, the worker would special-case it.
        counterfactual: visible, the worker would hardcode the result.
        verdict: pass
        rationale: it passed.
    completion_evidence: "2026-09-06 — completed: done"
```
<!-- TASK_GRAPH:END -->

# Task Details

## T001 — Do the shared thing

Status: `done`

Depends on: none

### Description

A task carrying the shared disclosure claim.

### Invariants

- The shared thing stays done.

### Acceptance Checks

- The shared thing is done.

### Completion Evidence

Done.
"#
    )
}

#[test]
fn full_plan_fixture_validates() -> Result<(), Box<dyn Error>> {
    let report = tftio_planner::validate_markdown(FULL_PLAN)?;
    assert!(
        report.is_valid(),
        "fixture failed validation: {:?}",
        report.diagnostics
    );
    Ok(())
}

#[test]
fn seal_discloses_every_hidden_criterion_with_its_verdict() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(FULL_PLAN, "2026-09-06-sealed-full-plan.md")?;
    let date = SealDate::new("2026-09-06")?;

    let outcome = seal::seal(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
        None,
    )?;

    let artifact = &outcome.artifact;
    assert!(artifact.contains("the worker did not weaken any existing check"));
    assert!(artifact.contains("no verdict recorded"));
    assert!(artifact.contains("the automated check still passes"));
    assert!(artifact.contains("Verdict: pass"));
    assert!(artifact.contains("the design doc's caveat was actually followed"));
    assert!(artifact.contains("Verdict: undetermined"));
    Ok(())
}

#[test]
fn seal_places_unresolved_items_above_passing_checks() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(FULL_PLAN, "2026-09-06-sealed-full-plan.md")?;
    let date = SealDate::new("2026-09-06")?;

    let outcome = seal::seal(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
        None,
    )?;
    let artifact = &outcome.artifact;

    let human_judgment_offset = artifact
        .find("Awaiting human judgment")
        .ok_or("human-judgment residual must render")?;
    let undetermined_offset = artifact
        .find("**Undetermined**")
        .ok_or("undetermined residual must render")?;
    let abandoned_offset = artifact
        .find("**Abandoned**")
        .ok_or("abandoned residual must render")?;
    let uncovered_offset = artifact
        .find("Uncovered changed scope")
        .ok_or("uncovered-changed-scope residual must render")?;
    let disagreement_offset = artifact
        .find("Judge disagreement")
        .ok_or("judge-disagreement residual must render")?;
    let passing_check_offset = artifact
        .find("the ground looked fine")
        .ok_or("routine passing check must render")?;

    for offset in [
        human_judgment_offset,
        undetermined_offset,
        abandoned_offset,
        uncovered_offset,
        disagreement_offset,
    ] {
        assert!(
            offset < passing_check_offset,
            "expected unresolved item at {offset} to render before the passing check at {passing_check_offset}"
        );
    }
    Ok(())
}

#[test]
fn seal_fails_with_open_tasks_named() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(OPEN_TASK_PLAN, "2026-09-05-hidden-sentinel-plan.md")?;
    let date = SealDate::new("2026-09-06")?;

    let result = seal::seal(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
        None,
    );

    match result {
        Err(SealError::OpenTasks(open)) => {
            assert_eq!(vec!["T002".to_owned()], open);
            Ok(())
        }
        other => Err(format!("expected SealError::OpenTasks, got {other:?}").into()),
    }
}

#[test]
fn seal_rejects_an_artifact_inside_the_repository_and_leaves_the_plan_approved()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    init_repo(&repo)?;
    // The store root lives inside the repository's own working tree, so
    // the computed artifact path resolves inside it too -- step (2) of
    // `seal`'s own ordering must reject this before anything is rendered,
    // mutated, or written.
    let store = Store::new(StoreRoot::new(repo.join(".silent-critic-store")));
    let plan_path = repo.join("2026-09-06-sealed-full-plan.md");
    std::fs::write(&plan_path, FULL_PLAN)?;
    let plan_id = store.add_plan(&plan_path, &repo, "HEAD")?;

    let date = SealDate::new("2026-09-06")?;
    let result = seal::seal(
        &store,
        &repo,
        plan_id.as_str(),
        &date,
        BurnedThreshold::default(),
        None,
    );
    assert!(matches!(
        result,
        Err(SealError::ArtifactInsideRepository { .. })
    ));

    // The plan-level transition never ran: still `approved`.
    let plan_file = store.plan_path(&repo, plan_id.as_str())?;
    let source = std::fs::read_to_string(&plan_file)?;
    let plan = tftio_planner::parse_markdown(&source)?;
    assert_eq!(
        tftio_planner::model::PlanStatus::Approved,
        plan.metadata.status
    );

    // No disclosures were recorded either.
    let disclosures_path = store.root_path().join("disclosures.toml");
    assert!(!disclosures_path.exists());
    Ok(())
}

#[test]
fn seal_removes_the_artifact_when_the_plan_mutation_fails() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = set_up(FULL_PLAN, "2026-09-06-sealed-full-plan.md")?;
    let plan_directory = fixture
        .store
        .plan_directory(&fixture.repo, &fixture.plan_id)?;
    // A read-only plan directory blocks `Ledger`'s own mutation-lock file
    // from ever being created, so `apply_plan_mutation` fails after the
    // artifact has already been rendered and written (steps 3-4 succeed,
    // step 5 fails).
    std::fs::set_permissions(&plan_directory, std::fs::Permissions::from_mode(0o500))?;

    let date = SealDate::new("2026-09-06")?;
    let result = seal::seal(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
        None,
    );

    std::fs::set_permissions(&plan_directory, std::fs::Permissions::from_mode(0o700))?;

    assert!(result.is_err());
    let provenance = fixture.store.provenance(&fixture.repo, &fixture.plan_id)?;
    let artifact_path = fixture
        .store
        .root_path()
        .join("runs")
        .join(provenance.repo.as_str())
        .join(&fixture.plan_id)
        .join("sealed.md");
    assert!(
        !artifact_path.exists(),
        "no artifact must remain for a plan that never actually sealed"
    );
    Ok(())
}

#[test]
fn disclosures_only_records_exactly_once_after_a_failed_disclosure_commit()
-> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = set_up(FULL_PLAN, "2026-09-06-sealed-full-plan.md")?;
    let provenance = fixture.store.provenance(&fixture.repo, &fixture.plan_id)?;
    let run_dir = fixture
        .store
        .root_path()
        .join("runs")
        .join(provenance.repo.as_str())
        .join(&fixture.plan_id);
    // Pre-create the artifact's own directory (writable) so the artifact
    // write in step (4) still succeeds once the store root itself is
    // locked down for step (6): each directory's permissions are its own,
    // not inherited from an ancestor's later `chmod`.
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(
        fixture.store.root_path(),
        std::fs::Permissions::from_mode(0o500),
    )?;

    let date = SealDate::new("2026-09-06")?;
    let first = seal::seal(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
        None,
    );

    std::fs::set_permissions(
        fixture.store.root_path(),
        std::fs::Permissions::from_mode(0o700),
    )?;

    assert!(matches!(
        first,
        Err(SealError::DisclosuresNotRecorded { .. })
    ));
    // The plan is nonetheless sealed: the artifact exists and the plan is
    // `implemented`.
    assert!(run_dir.join("sealed.md").exists());
    let plan_file = fixture.store.plan_path(&fixture.repo, &fixture.plan_id)?;
    let sealed_source = std::fs::read_to_string(&plan_file)?;
    let sealed_plan = tftio_planner::parse_markdown(&sealed_source)?;
    assert_eq!(
        tftio_planner::model::PlanStatus::Implemented,
        sealed_plan.metadata.status
    );

    // Disclosures were not recorded by the failed seal.
    let disclosures_path = fixture.store.root_path().join("disclosures.toml");
    assert!(!disclosures_path.exists());

    // `--disclosures-only` records them, exactly once.
    let outcome = seal::seal_disclosures_only(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
    )?;
    assert_eq!(
        seal::DisclosuresOutcome::Recorded { burned: Vec::new() },
        outcome,
        "first disclosure is never burned"
    );

    let disclosures_body = std::fs::read_to_string(&disclosures_path)?;
    let disclosures: toml::Table = toml::from_str(&disclosures_body)?;
    let criteria = disclosures
        .get("criteria")
        .and_then(toml::Value::as_table)
        .ok_or("expected a criteria table")?;
    assert_eq!(
        3,
        criteria.len(),
        "one entry per hidden criterion in the fixture"
    );
    for (_, entry) in criteria {
        let count = entry
            .get("count")
            .and_then(toml::Value::as_integer)
            .ok_or("expected an integer count")?;
        assert_eq!(1, count, "each claim must be disclosed exactly once");
    }

    // Calling `--disclosures-only` again is refused: the recovery path is
    // for a plan whose disclosures were never recorded, not a general
    // "add another disclosure" command; nothing here re-verifies that
    // rejection (covered separately by `seal_disclosures_only_rejects_a_plan_that_is_not_sealed`
    // and the CLI tests), but the count above already proves this run
    // recorded the claims exactly once, not zero or two times.

    Ok(())
}

#[test]
fn seal_disclosures_only_rejects_a_plan_that_is_not_sealed() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(OPEN_TASK_PLAN, "2026-09-05-hidden-sentinel-plan.md")?;
    let date = SealDate::new("2026-09-06")?;

    let result = seal::seal_disclosures_only(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
    );
    assert!(matches!(result, Err(SealError::NotSealed { .. })));
    Ok(())
}

#[test]
fn seal_writes_outside_the_supervised_repository_and_the_plan_directory()
-> Result<(), Box<dyn Error>> {
    let fixture = set_up(FULL_PLAN, "2026-09-06-sealed-full-plan.md")?;
    let date = SealDate::new("2026-09-06")?;

    let outcome = seal::seal(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
        None,
    )?;

    assert!(!outcome.artifact_path.starts_with(&fixture.repo));
    let plan_directory = fixture
        .store
        .plan_directory(&fixture.repo, &fixture.plan_id)?;
    assert!(!outcome.artifact_path.starts_with(&plan_directory));
    assert!(outcome.artifact_path.exists());
    Ok(())
}

#[test]
fn seal_leaves_the_plan_valid_and_implemented() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(FULL_PLAN, "2026-09-06-sealed-full-plan.md")?;
    let date = SealDate::new("2026-09-06")?;

    seal::seal(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
        None,
    )?;

    let plan_path = fixture.store.plan_path(&fixture.repo, &fixture.plan_id)?;
    let source = std::fs::read_to_string(&plan_path)?;
    // `validate_markdown_path`'s filename check is path-shaped
    // (`YYYY-MM-DD-<slug>.md`), independent of where the store happens to
    // keep the plan's bytes on disk (`<plan_dir>/plan.md`); validate
    // against the plan's own original filename shape, not the store's
    // internal one, to test what this assertion is actually about: sealing
    // does not corrupt the document itself.
    let report = tftio_planner::validate_markdown_path(
        &source,
        Path::new("2026-09-06-sealed-full-plan.md"),
    )?;
    assert!(
        report.is_valid(),
        "sealed plan failed validation: {:?}",
        report.diagnostics
    );

    let plan = tftio_planner::parse_markdown(&source)?;
    assert_eq!(
        tftio_planner::model::PlanStatus::Implemented,
        plan.metadata.status
    );
    assert_eq!(
        tftio_planner::model::TaskGraphStatus::Complete,
        plan.metadata.execution.task_graph_status
    );
    Ok(())
}

#[test]
fn rendering_is_deterministic() -> Result<(), Box<dyn Error>> {
    let fixture = set_up(FULL_PLAN, "2026-09-06-sealed-full-plan.md")?;
    let date = SealDate::new("2026-09-06")?;

    let outcome = seal::seal(
        &fixture.store,
        &fixture.repo,
        &fixture.plan_id,
        &date,
        BurnedThreshold::default(),
        None,
    )?;

    let plan_path = fixture.store.plan_path(&fixture.repo, &fixture.plan_id)?;
    let sealed_source = std::fs::read_to_string(&plan_path)?;
    let sealed_plan = tftio_planner::parse_markdown(&sealed_source)?;
    let ledger = Ledger::new(&fixture.store, &fixture.repo, &fixture.plan_id);
    let unresolved = ledger.unresolved_items()?;

    let mut disclosures = std::collections::HashMap::new();
    disclosures.insert(
        silent_critic::render::normalize_claim("the automated check still passes"),
        silent_critic::render::DisclosureSnapshot {
            count: 1,
            burned: false,
        },
    );

    let input = silent_critic::render::RenderInput {
        plan: &sealed_plan,
        unresolved: &unresolved,
        disclosures: &disclosures,
        date: &date,
    };
    let first = silent_critic::render::render_sealed_artifact(&input);
    let second = silent_critic::render::render_sealed_artifact(&input);
    assert_eq!(first, second);
    assert_eq!(!outcome.artifact.is_empty(), !first.is_empty());
    Ok(())
}

#[test]
fn a_criterion_disclosed_past_the_thressilent_critic_is_reported_as_burned()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    init_repo(&repo)?;
    let store = Store::new(StoreRoot::new(temp.path().join("store")));

    let claim = "the shared disclosure claim";
    let mut last_outcome = None;
    for index in 0..3 {
        let plan_id_field = format!("PLAN-20260906-disclosure-{index}");
        let file_name = format!("2026-09-06-disclosure-{index}.md");
        let source = minimal_plan(&plan_id_field, claim);
        let plan_path = temp.path().join(&file_name);
        std::fs::write(&plan_path, &source)?;
        let plan_id = store.add_plan(&plan_path, &repo, "HEAD")?;

        let date = SealDate::new("2026-09-06")?;
        let outcome = seal::seal(
            &store,
            &repo,
            plan_id.as_str(),
            &date,
            BurnedThreshold::new(2),
            None,
        )?;
        last_outcome = Some(outcome);
    }

    let outcome = last_outcome.ok_or("three seals ran")?;
    assert_eq!(1, outcome.burned.len());
    let burned = outcome.burned.first().ok_or("expected one burned entry")?;
    assert_eq!(claim, burned.claim);
    assert_eq!(3, burned.count);
    assert!(outcome.artifact.contains("**Burned**"));
    Ok(())
}

#[test]
fn a_criterion_disclosed_at_or_below_the_thressilent_critic_is_not_burned()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    init_repo(&repo)?;
    let store = Store::new(StoreRoot::new(temp.path().join("store")));

    let claim = "a claim disclosed exactly at the threshold";
    let mut last_outcome = None;
    for index in 0..2 {
        let plan_id_field = format!("PLAN-20260906-under-{index}");
        let file_name = format!("2026-09-06-under-{index}.md");
        let source = minimal_plan(&plan_id_field, claim);
        let plan_path = temp.path().join(&file_name);
        std::fs::write(&plan_path, &source)?;
        let plan_id = store.add_plan(&plan_path, &repo, "HEAD")?;

        let date = SealDate::new("2026-09-06")?;
        let outcome = seal::seal(
            &store,
            &repo,
            plan_id.as_str(),
            &date,
            BurnedThreshold::new(2),
            None,
        )?;
        last_outcome = Some(outcome);
    }

    let outcome = last_outcome.ok_or("two seals ran")?;
    assert!(outcome.burned.is_empty());
    assert!(!outcome.artifact.contains("**Burned**"));
    Ok(())
}

#[test]
fn cli_seal_writes_the_artifact_outside_the_repository() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    init_repo(&repo)?;
    let store_dir = temp.path().join("store");

    let plan_path = repo.join("2026-09-06-sealed-full-plan.md");
    std::fs::write(&plan_path, FULL_PLAN)?;

    let mut add = AssertCommand::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", &store_dir)
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(&repo)
        .output()?;
    assert!(
        add_output.status.success(),
        "plan add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_owned();

    let mut sealed = AssertCommand::cargo_bin("silent-critic")?;
    let sealed_output = sealed
        .env("XDG_DATA_HOME", &store_dir)
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(&repo)
        .output()?;
    assert!(
        sealed_output.status.success(),
        "seal failed: {}",
        String::from_utf8_lossy(&sealed_output.stderr)
    );
    let stdout = String::from_utf8(sealed_output.stdout)?;
    let artifact_line = stdout
        .lines()
        .find(|line| line.starts_with("artifact: "))
        .ok_or("seal must print the artifact path")?;
    let artifact_path = Path::new(artifact_line.trim_start_matches("artifact: "));
    assert!(artifact_path.exists());
    assert!(!artifact_path.starts_with(&repo));
    Ok(())
}

fn cli_add_plan(store_dir: &Path, plan_path: &Path, repo: &Path) -> Result<String, Box<dyn Error>> {
    let mut add = AssertCommand::cargo_bin("silent-critic")?;
    let output = add
        .env("XDG_DATA_HOME", store_dir)
        .arg("plan")
        .arg("add")
        .arg(plan_path)
        .arg("--repo")
        .arg(repo)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "plan add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[test]
fn cli_seal_out_writes_an_additional_copy() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    init_repo(&repo)?;
    let store_dir = temp.path().join("store");

    let plan_path = repo.join("2026-09-06-sealed-full-plan.md");
    std::fs::write(&plan_path, FULL_PLAN)?;
    let plan_id = cli_add_plan(&store_dir, &plan_path, &repo)?;

    let out_path = temp.path().join("copy.md");
    let mut sealed = AssertCommand::cargo_bin("silent-critic")?;
    let output = sealed
        .env("XDG_DATA_HOME", &store_dir)
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(&repo)
        .arg("--out")
        .arg(&out_path)
        .output()?;
    assert!(
        output.status.success(),
        "seal failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("artifact copy: "));
    assert!(out_path.exists());
    let copy = std::fs::read_to_string(&out_path)?;
    assert!(copy.contains("Sealed review"));
    Ok(())
}

#[test]
fn cli_seal_out_inside_the_repository_is_rejected() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    init_repo(&repo)?;
    let store_dir = temp.path().join("store");

    let plan_path = repo.join("2026-09-06-sealed-full-plan.md");
    std::fs::write(&plan_path, FULL_PLAN)?;
    let plan_id = cli_add_plan(&store_dir, &plan_path, &repo)?;

    let out_path = repo.join("sealed-copy.md");
    let mut sealed = AssertCommand::cargo_bin("silent-critic")?;
    let output = sealed
        .env("XDG_DATA_HOME", &store_dir)
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(&repo)
        .arg("--out")
        .arg(&out_path)
        .output()?;
    assert!(!output.status.success());
    assert!(!out_path.exists());
    Ok(())
}

#[test]
fn cli_seal_reports_burned_criteria_on_stderr() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    init_repo(&repo)?;
    let store_dir = temp.path().join("store");
    let claim = "the shared cli disclosure claim";

    let mut last_output = None;
    for index in 0..3 {
        let plan_id_field = format!("PLAN-20260906-cli-disclosure-{index}");
        let file_name = format!("2026-09-06-cli-disclosure-{index}.md");
        let source = minimal_plan(&plan_id_field, claim);
        let plan_path = temp.path().join(&file_name);
        std::fs::write(&plan_path, &source)?;
        let plan_id = cli_add_plan(&store_dir, &plan_path, &repo)?;

        let mut sealed = AssertCommand::cargo_bin("silent-critic")?;
        let output = sealed
            .env("XDG_DATA_HOME", &store_dir)
            .arg("seal")
            .arg(&plan_id)
            .arg("--repo")
            .arg(&repo)
            .arg("--burned-threshold")
            .arg("2")
            .output()?;
        assert!(
            output.status.success(),
            "seal failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        last_output = Some(output);
    }

    let output = last_output.ok_or("three seals ran")?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("burned:"));
    assert!(stderr.contains(claim));
    Ok(())
}
