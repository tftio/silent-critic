//! CLI integration tests for the `silent-critic` operator binary.

use std::error::Error;
use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use silent_critic::ledger::Ledger;
use silent_critic::model::{CriterionId, Judgment, NonEmptyString, TaskId, Verdict};
use silent_critic::store::{Store, StoreRoot};

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
    let status = StdCommand::new("git")
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

fn worktree_add(repo_dir: &Path, worktree_path: &Path, branch: &str) -> Result<(), Box<dyn Error>> {
    let worktree_path = worktree_path
        .to_str()
        .ok_or("worktree path must be valid UTF-8 for this test")?;
    git(
        repo_dir,
        &["worktree", "add", "--quiet", "-b", branch, worktree_path],
    )
}

const VALID_PLAN: &str = include_str!("fixtures/2026-09-05-store-plan.md");
const SENTINEL_PLAN: &str = include_str!("fixtures/2026-09-05-hidden-sentinel-plan.md");
const SEALED_FULL_PLAN: &str = include_str!("fixtures/2026-09-06-sealed-full-plan.md");
const OPEN_TASK_PLAN: &str = include_str!("fixtures/2026-09-05-hidden-sentinel-plan.md");
const V2_PLAN_WITH_PROJECT: &str = include_str!("fixtures/2026-09-23-store-plan-v2.md");

fn cli_add_plan(store_dir: &Path, plan_path: &Path, repo: &Path) -> Result<String, Box<dyn Error>> {
    let mut add = Command::cargo_bin("silent-critic")?;
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
fn legacy_plan_list_reads_holdout_store_without_creating_silent_critic_state()
-> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;

    let data_home = tempfile::tempdir()?;
    let legacy_root = data_home.path().join("holdout");
    let legacy_store = Store::new(StoreRoot::new(legacy_root.clone()));
    let plan_id = legacy_store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;

    let mut command = Command::cargo_bin("silent-critic")?;
    let output = command
        .env("XDG_DATA_HOME", data_home.path())
        .arg("legacy")
        .arg("plan-list")
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(output.status.success(), "{output:?}");
    assert_eq!(format!("{plan_id}\n"), String::from_utf8(output.stdout)?);
    assert!(legacy_root.exists());
    assert!(
        !data_home.path().join("silent-critic").exists(),
        "read-only legacy inspection must not create new-state storage"
    );

    Ok(())
}

#[test]
fn legacy_plan_path_resolves_an_existing_legacy_plan_without_creating_silent_critic_state()
-> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;

    let data_home = tempfile::tempdir()?;
    let legacy_root = data_home.path().join("holdout");
    let legacy_store = Store::new(StoreRoot::new(legacy_root.clone()));
    let plan_id = legacy_store.add_plan(&plan_path, repo_dir.path(), "HEAD")?;
    let expected_path = legacy_store.plan_path(repo_dir.path(), plan_id.as_str())?;

    let mut command = Command::cargo_bin("silent-critic")?;
    let output = command
        .env("XDG_DATA_HOME", data_home.path())
        .arg("legacy")
        .arg("plan-path")
        .arg(plan_id.as_str())
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        format!("{}\n", expected_path.display()),
        String::from_utf8(output.stdout)?
    );
    assert!(legacy_root.exists());
    assert!(
        !data_home.path().join("silent-critic").exists(),
        "read-only legacy inspection must not create new-state storage"
    );

    Ok(())
}

/// `main` resolves each store root lazily, only inside the arm that needs
/// it: `legacy` commands resolve only `default_legacy_store_root` and never
/// touch the plain store root, so with neither `XDG_DATA_HOME` nor `HOME`
/// set, a `legacy` command reports the legacy root's own error message, not
/// the plain store root's.
#[test]
fn legacy_command_reports_when_neither_xdg_data_home_nor_home_is_set() -> Result<(), Box<dyn Error>>
{
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;

    let mut command = Command::cargo_bin("silent-critic")?;
    let output = command
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .arg("legacy")
        .arg("plan-list")
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr
            .contains("cannot determine the legacy holdout store root: set XDG_DATA_HOME or HOME"),
        "{stderr}"
    );

    Ok(())
}

/// The mirror image of the previous test: a non-legacy command with neither
/// `XDG_DATA_HOME` nor `HOME` set reports the plain store root's error
/// message, proving that message is reachable now that it is resolved only
/// inside the arms that need it (previously it fired unconditionally in
/// `main`, before any subcommand ran, which made the legacy message above
/// unreachable).
#[test]
fn plan_command_reports_when_neither_xdg_data_home_nor_home_is_set() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;

    let mut command = Command::cargo_bin("silent-critic")?;
    let output = command
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .arg("plan")
        .arg("list")
        .current_dir(repo_dir.path())
        .output()?;

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("cannot determine a store root: set XDG_DATA_HOME or HOME"),
        "{stderr}"
    );

    Ok(())
}

/// A minimal, valid, single-task, single-hidden-criterion plan carrying
/// `claim`, for the burned-disclosure CLI test: sealing several distinct
/// plans that share one claim, in the same store, is how a claim's
/// disclosure count climbs past a threshold.
fn minimal_plan_with_claim(plan_id: &str, claim: &str) -> String {
    format!(
        r#"---
plan_format_version: 1
plan_id: {plan_id}
title: CLI disclosure fixture {plan_id}
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
  summary: Exercise disclosure counting from the CLI.
  severity: low
  affected_area: scaffold
  user_impact: none
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: executing
---

# ADR: CLI disclosure fixture {plan_id}

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
fn seal_writes_the_default_artifact_path_outside_the_repository() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;

    let plan_path = repo_dir.path().join("2026-09-06-sealed-full-plan.md");
    std::fs::write(&plan_path, SEALED_FULL_PLAN)?;
    let plan_id = cli_add_plan(store_dir.path(), &plan_path, repo_dir.path())?;

    let mut sealed = Command::cargo_bin("silent-critic")?;
    let output = sealed
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(
        output.status.success(),
        "seal failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let artifact_line = stdout
        .lines()
        .find(|line| line.starts_with("artifact: "))
        .ok_or("seal must print the default artifact path")?;
    let artifact_path = Path::new(artifact_line.trim_start_matches("artifact: "));
    assert!(artifact_path.exists());
    assert!(!artifact_path.starts_with(repo_dir.path()));
    assert!(!stdout.contains("artifact copy: "));
    Ok(())
}

#[test]
fn seal_out_writes_an_additional_copy_at_the_given_path() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;
    let out_dir = tempfile::tempdir()?;

    let plan_path = repo_dir.path().join("2026-09-06-sealed-full-plan.md");
    std::fs::write(&plan_path, SEALED_FULL_PLAN)?;
    let plan_id = cli_add_plan(store_dir.path(), &plan_path, repo_dir.path())?;

    let out_path = out_dir.path().join("copy.md");
    let mut sealed = Command::cargo_bin("silent-critic")?;
    let output = sealed
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--out")
        .arg(&out_path)
        .output()?;

    assert!(
        output.status.success(),
        "seal failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(&format!("artifact copy: {}", out_path.display())));
    assert!(out_path.exists());
    let copy = std::fs::read_to_string(&out_path)?;
    assert!(copy.contains("Sealed review"));
    Ok(())
}

#[test]
fn seal_burned_thressilent_critic_reports_a_burned_criterion_on_stderr()
-> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;
    let claim = "the cli-arm shared disclosure claim";

    let mut last_output = None;
    for index in 0..3 {
        let plan_id_field = format!("PLAN-20260906-cli-arm-disclosure-{index}");
        let file_name = format!("2026-09-06-cli-arm-disclosure-{index}.md");
        let source = minimal_plan_with_claim(&plan_id_field, claim);
        let plan_path = repo_dir.path().join(&file_name);
        std::fs::write(&plan_path, &source)?;
        let plan_id = cli_add_plan(store_dir.path(), &plan_path, repo_dir.path())?;

        let mut sealed = Command::cargo_bin("silent-critic")?;
        let output = sealed
            .env("XDG_DATA_HOME", store_dir.path())
            .arg("seal")
            .arg(&plan_id)
            .arg("--repo")
            .arg(repo_dir.path())
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
    assert!(stderr.contains("burned:"), "unexpected stderr: {stderr}");
    assert!(stderr.contains(claim), "unexpected stderr: {stderr}");
    assert!(
        stderr.contains("disclosed 3 times"),
        "unexpected stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn seal_reports_open_tasks_with_a_nonzero_exit_code() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;

    let plan_path = repo_dir.path().join("2026-09-05-hidden-sentinel-plan.md");
    std::fs::write(&plan_path, OPEN_TASK_PLAN)?;
    let plan_id = cli_add_plan(store_dir.path(), &plan_path, repo_dir.path())?;

    let mut sealed = Command::cargo_bin("silent-critic")?;
    let output = sealed
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("open tasks remain") && stderr.contains("T002"),
        "unexpected stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn seal_disclosures_only_reports_already_recorded_after_a_successful_seal()
-> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;

    let plan_path = repo_dir.path().join("2026-09-06-sealed-full-plan.md");
    std::fs::write(&plan_path, SEALED_FULL_PLAN)?;
    let plan_id = cli_add_plan(store_dir.path(), &plan_path, repo_dir.path())?;

    let mut sealed = Command::cargo_bin("silent-critic")?;
    let seal_output = sealed
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    assert!(
        seal_output.status.success(),
        "seal failed: {}",
        String::from_utf8_lossy(&seal_output.stderr)
    );

    // A normal `seal` already recorded disclosures; disclosure recording
    // is idempotent per (criterion, plan id), so `--disclosures-only`
    // afterward reports `AlreadyRecorded` rather than double-counting.
    let mut disclosures_only = Command::cargo_bin("silent-critic")?;
    let output = disclosures_only
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--disclosures-only")
        .output()?;
    assert!(
        output.status.success(),
        "seal --disclosures-only failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains(&format!("disclosures already recorded for {plan_id}")),
        "unexpected stdout: {stdout}"
    );
    Ok(())
}

#[test]
fn seal_disclosures_only_records_after_a_failed_disclosure_commit() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt as _;

    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;

    let plan_path = repo_dir.path().join("2026-09-06-sealed-full-plan.md");
    std::fs::write(&plan_path, SEALED_FULL_PLAN)?;
    let plan_id = cli_add_plan(store_dir.path(), &plan_path, repo_dir.path())?;

    // `silent-critic plan add`/`silent-critic seal` resolve the store root as
    // `$XDG_DATA_HOME/silent-critic` (`default_store_root`), not
    // `$XDG_DATA_HOME` itself.
    let store_root = store_dir.path().join("silent-critic");

    // Locate the store's repo-identity directory (the same ancestor
    // `runs/<repo_identity>/` and `disclosures.toml` both live under), then
    // lock the store root down so the disclosure commit step fails while
    // the artifact write (a different, already-existing directory) still
    // succeeds.
    let store_root_entries: Vec<_> = std::fs::read_dir(&store_root)?
        .filter_map(Result::ok)
        .collect();
    let repo_identity_dir = store_root_entries
        .first()
        .ok_or("expected the store root to contain a repo-identity directory")?
        .path();
    let repo_identity_name = repo_identity_dir
        .file_name()
        .ok_or("expected a directory name")?;
    let run_dir = store_root
        .join("runs")
        .join(repo_identity_name)
        .join(&plan_id);
    std::fs::create_dir_all(&run_dir)?;
    std::fs::set_permissions(&store_root, std::fs::Permissions::from_mode(0o500))?;

    let mut sealed = Command::cargo_bin("silent-critic")?;
    let seal_output = sealed
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    std::fs::set_permissions(&store_root, std::fs::Permissions::from_mode(0o700))?;

    assert!(!seal_output.status.success());
    let seal_stderr = String::from_utf8(seal_output.stderr)?;
    assert!(
        seal_stderr.contains("disclosures were not recorded"),
        "unexpected stderr: {seal_stderr}"
    );

    let mut disclosures_only = Command::cargo_bin("silent-critic")?;
    let output = disclosures_only
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("seal")
        .arg(&plan_id)
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--disclosures-only")
        .output()?;
    assert!(
        output.status.success(),
        "seal --disclosures-only failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.contains(&format!("disclosures recorded for {plan_id}")),
        "unexpected stdout: {stdout}"
    );
    Ok(())
}

#[test]
fn cli_reports_its_version() -> Result<(), Box<dyn Error>> {
    let mut command = Command::cargo_bin("silent-critic")?;

    let output = command.arg("--version").output()?;

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(
        stdout.starts_with("silent-critic "),
        "unexpected version output: {stdout}"
    );

    Ok(())
}

#[test]
fn plan_add_list_and_path_round_trip() -> Result<(), Box<dyn Error>> {
    let repo_a = tempfile::tempdir()?;
    let repo_b = tempfile::tempdir()?;
    init_repo(repo_a.path())?;
    init_repo(repo_b.path())?;

    let plan_path = repo_a.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;

    let store_dir = tempfile::tempdir()?;

    // `plan add` against repo A.
    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_a.path())
        .output()?;
    assert!(
        add_output.status.success(),
        "plan add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();
    assert_eq!("2026-09-05-store-plan", plan_id);

    // `plan list` from inside repo A shows the plan.
    let mut list_a = Command::cargo_bin("silent-critic")?;
    list_a
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_a.path())
        .arg("plan")
        .arg("list")
        .assert()
        .success()
        .stdout(format!("{plan_id}\n"));

    // `plan list` from inside repo B shows nothing: it is a different
    // repository.
    let mut list_b = Command::cargo_bin("silent-critic")?;
    list_b
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_b.path())
        .arg("plan")
        .arg("list")
        .assert()
        .success()
        .stdout("");

    // `plan path` from inside repo A prints the stored plan's absolute path.
    let mut path_cmd = Command::cargo_bin("silent-critic")?;
    let path_output = path_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_a.path())
        .arg("plan")
        .arg("path")
        .arg(&plan_id)
        .output()?;
    assert!(path_output.status.success());
    let printed_path = String::from_utf8(path_output.stdout)?.trim().to_string();
    let printed_path = Path::new(&printed_path);
    assert!(printed_path.is_absolute());
    assert!(printed_path.is_file());
    assert_eq!(VALID_PLAN, std::fs::read_to_string(printed_path)?);

    Ok(())
}

/// T012 acceptance check: `plan add` on a v2 plan that declares a project
/// records the slug in the stored `binding.toml`, and `plan list` shows it.
#[test]
fn plan_add_records_and_plan_list_shows_the_project_slug() -> Result<(), Box<dyn Error>> {
    let repo = tempfile::tempdir()?;
    init_repo(repo.path())?;

    let plan_path = repo.path().join("2026-09-23-store-plan-v2.md");
    std::fs::write(&plan_path, V2_PLAN_WITH_PROJECT)?;

    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo.path())
        .output()?;
    assert!(
        add_output.status.success(),
        "plan add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    // `binding.toml` carries the slug.
    let mut path_cmd = Command::cargo_bin("silent-critic")?;
    let path_output = path_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo.path())
        .arg("plan")
        .arg("path")
        .arg(&plan_id)
        .output()?;
    assert!(path_output.status.success());
    let plan_file_text = String::from_utf8(path_output.stdout)?;
    let plan_file = Path::new(plan_file_text.trim());
    let binding_file = plan_file
        .parent()
        .ok_or("plan file has no parent directory")?
        .join("binding.toml");
    let binding_body = std::fs::read_to_string(binding_file)?;
    assert!(
        binding_body.contains("project = \"silent-critic\""),
        "{binding_body}"
    );

    // `plan list` shows it.
    let mut list = Command::cargo_bin("silent-critic")?;
    list.env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo.path())
        .arg("plan")
        .arg("list")
        .assert()
        .success()
        .stdout(format!("{plan_id} project=silent-critic\n"));

    Ok(())
}

#[test]
fn plan_path_reports_unknown_plans() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;

    let store_dir = tempfile::tempdir()?;

    let mut path_cmd = Command::cargo_bin("silent-critic")?;
    path_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("plan")
        .arg("path")
        .arg("does-not-exist")
        .assert()
        .failure();

    Ok(())
}

#[test]
fn plan_list_falls_back_to_home_when_xdg_data_home_is_unset() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;

    let home_dir = tempfile::tempdir()?;

    let mut list_cmd = Command::cargo_bin("silent-critic")?;
    list_cmd
        .env_remove("XDG_DATA_HOME")
        .env("HOME", home_dir.path())
        .current_dir(repo_dir.path())
        .arg("plan")
        .arg("list")
        .assert()
        .success()
        .stdout("");

    Ok(())
}

#[test]
fn plan_add_rejects_a_document_that_fails_validation() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;

    let plan_path = repo_dir.path().join("2026-09-05-invalid-plan.md");
    std::fs::write(&plan_path, "not a planning document\n")?;

    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    add.env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .assert()
        .failure();

    Ok(())
}

#[test]
fn plan_add_reports_an_unreadable_plan_file() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt as _;

    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;

    let plan_path = repo_dir.path().join("2026-09-05-unreadable-plan.md");
    std::fs::write(&plan_path, "irrelevant\n")?;
    // Deterministically forces the plan-read step's `io::Error` path (as
    // opposed to a document that merely fails validation): unreadable for
    // a non-root user, unlike a missing file, so this exercises the actual
    // `fs::read_to_string` failure branch through the compiled binary.
    std::fs::set_permissions(&plan_path, std::fs::Permissions::from_mode(0o000))?;

    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("I/O error"), "unexpected stderr: {stderr}");

    Ok(())
}

#[test]
fn plan_list_converges_across_linked_worktrees_of_the_same_repository() -> Result<(), Box<dyn Error>>
{
    let main_checkout = tempfile::tempdir()?;
    init_repo(main_checkout.path())?;

    let worktree_parent = tempfile::tempdir()?;
    let linked_worktree = worktree_parent.path().join("linked-worktree");
    worktree_add(main_checkout.path(), &linked_worktree, "linked")?;

    let plan_path = main_checkout.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;

    let store_dir = tempfile::tempdir()?;

    // `plan add` from the main checkout.
    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(main_checkout.path())
        .output()?;
    assert!(
        add_output.status.success(),
        "plan add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    // `plan list` from the *linked worktree* shows the plan added from the
    // main checkout: both resolve to the same repository identity.
    let mut list_cmd = Command::cargo_bin("silent-critic")?;
    list_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(&linked_worktree)
        .arg("plan")
        .arg("list")
        .assert()
        .success()
        .stdout(format!("{plan_id}\n"));

    Ok(())
}

#[test]
fn dispatch_manual_prints_the_expected_paths_and_environment() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;

    let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;

    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    assert!(
        add_output.status.success(),
        "plan add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    let mut dispatch = Command::cargo_bin("silent-critic")?;
    let dispatch_output = dispatch
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("dispatch")
        .arg("--manual")
        .arg(&plan_id)
        .arg("T001")
        .arg("--repo")
        .arg(repo_dir.path())
        .arg("--silent-critic-mcp-path")
        .arg("/usr/local/bin/silent-critic-mcp")
        .output()?;
    assert!(
        dispatch_output.status.success(),
        "dispatch --manual failed: {}",
        String::from_utf8_lossy(&dispatch_output.stderr)
    );
    let stdout = String::from_utf8(dispatch_output.stdout)?;

    assert!(stdout.contains("worktree: "), "{stdout}");
    assert!(stdout.contains("brief: "), "{stdout}");
    assert!(stdout.contains("mcp config: "), "{stdout}");
    // Fix round 1: the manual hatch is operator-facing, so it still prints
    // every `mcp.json`-bound pair (token, plan id, repo, store root, brief)
    // plus where mcp.json itself lives.
    assert!(stdout.contains("export SILENT_CRITIC_TOKEN="), "{stdout}");
    assert!(stdout.contains("export SILENT_CRITIC_PLAN_ID="), "{stdout}");
    assert!(stdout.contains("export SILENT_CRITIC_REPO="), "{stdout}");
    assert!(
        stdout.contains("export SILENT_CRITIC_STORE_ROOT="),
        "{stdout}"
    );
    assert!(stdout.contains("export SILENT_CRITIC_BRIEF="), "{stdout}");
    assert!(
        stdout.contains("export SILENT_CRITIC_MCP_CONFIG="),
        "{stdout}"
    );

    let worktree_line = stdout
        .lines()
        .find(|line| line.starts_with("worktree: "))
        .ok_or("missing worktree line")?;
    let worktree_path = std::path::Path::new(
        worktree_line
            .strip_prefix("worktree: ")
            .ok_or("malformed worktree line")?,
    );
    assert!(worktree_path.is_dir());
    let canonical_worktree = worktree_path.canonicalize()?;
    let canonical_repo = repo_dir.path().canonicalize()?;
    assert!(!canonical_worktree.starts_with(&canonical_repo));
    assert!(!worktree_path.join("2026-09-05-store-plan.md").exists());

    // Fix round 1 (`REPO_INVARIANTS.md` HO-004): no ancestor of the
    // worktree is the plan's own directory.
    let mut path_cmd = Command::cargo_bin("silent-critic")?;
    let plan_path_output = path_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("plan")
        .arg("path")
        .arg(&plan_id)
        .output()?;
    let stored_plan_path = String::from_utf8(plan_path_output.stdout)?
        .trim()
        .to_string();
    let plan_dir = std::path::Path::new(&stored_plan_path)
        .parent()
        .ok_or("stored plan path has no parent")?
        .canonicalize()?;
    assert!(
        canonical_worktree
            .ancestors()
            .all(|ancestor| ancestor != plan_dir),
        "no ancestor of the worktree may be the plan directory"
    );

    Ok(())
}

#[test]
fn dispatch_without_manual_is_rejected() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;
    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    let mut dispatch = Command::cargo_bin("silent-critic")?;
    dispatch
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("dispatch")
        .arg(&plan_id)
        .arg("T001")
        .arg("--repo")
        .arg(repo_dir.path())
        .assert()
        .failure();

    Ok(())
}

#[test]
fn dispatch_manual_reports_an_unknown_plan() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;

    let mut dispatch = Command::cargo_bin("silent-critic")?;
    let output = dispatch
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("dispatch")
        .arg("--manual")
        .arg("does-not-exist")
        .arg("T001")
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("resolving plan path"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

#[test]
fn dispatch_manual_reports_an_unknown_task() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;
    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    // Fix round 1: `--manual` gates on the task actually existing in the
    // parsed plan, not just accepting whatever the operator typed.
    let mut dispatch = Command::cargo_bin("silent-critic")?;
    let output = dispatch
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("dispatch")
        .arg("--manual")
        .arg(&plan_id)
        .arg("does-not-exist")
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("no such task"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

#[test]
fn dispatch_manual_reports_a_malformed_stored_plan() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;
    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    // Corrupt the store's own copy after a valid `plan add`, forcing
    // `dispatch --manual`'s own re-parse (to gate on the task existing) to
    // fail, rather than `plan add`'s original validation.
    let mut path_cmd = Command::cargo_bin("silent-critic")?;
    let stored_path = path_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("plan")
        .arg("path")
        .arg(&plan_id)
        .output()?;
    let stored_path = String::from_utf8(stored_path.stdout)?.trim().to_string();
    std::fs::write(&stored_path, "not a planning document\n")?;

    let mut dispatch = Command::cargo_bin("silent-critic")?;
    let output = dispatch
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("dispatch")
        .arg("--manual")
        .arg(&plan_id)
        .arg("T001")
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("parsing plan"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

#[test]
fn dispatch_manual_reports_an_unreadable_plan() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt as _;

    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;
    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    // Make the store's own copy of the plan unreadable, forcing
    // `dispatch --manual`'s own re-read of the plan (as opposed to `plan
    // add`'s original read) to fail.
    let mut path_cmd = Command::cargo_bin("silent-critic")?;
    let stored_path = path_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("plan")
        .arg("path")
        .arg(&plan_id)
        .output()?;
    let stored_path = String::from_utf8(stored_path.stdout)?.trim().to_string();
    std::fs::set_permissions(&stored_path, std::fs::Permissions::from_mode(0o000))?;

    let mut dispatch = Command::cargo_bin("silent-critic")?;
    let output = dispatch
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("dispatch")
        .arg("--manual")
        .arg(&plan_id)
        .arg("T001")
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("reading plan"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

#[test]
fn dispatch_manual_reports_a_prepare_failure_on_a_repeat_dispatch() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-store-plan.md");
    std::fs::write(&plan_path, VALID_PLAN)?;
    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    let mut first = Command::cargo_bin("silent-critic")?;
    first
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("dispatch")
        .arg("--manual")
        .arg(&plan_id)
        .arg("T001")
        .arg("--repo")
        .arg(repo_dir.path())
        .assert()
        .success();

    let mut second = Command::cargo_bin("silent-critic")?;
    let output = second
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("dispatch")
        .arg("--manual")
        .arg(&plan_id)
        .arg("T001")
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("preparing dispatch"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

/// Record a verdict directly through the library's own [`Ledger`], against
/// the same store the CLI subprocess wrote to.
///
/// `xdg_data_home` is the value passed to the CLI subprocess as
/// `XDG_DATA_HOME`; the CLI's own `default_store_root` appends `silent-critic/`
/// to it (`src/bin/silent-critic.rs`), so this does the same rather than
/// constructing a [`Store`] over `xdg_data_home` directly.
fn record_verdict(
    xdg_data_home: &Path,
    repo_dir: &Path,
    plan_id: &str,
    task_id: &str,
    criterion_id: &str,
    judgment: Judgment,
) -> Result<(), Box<dyn Error>> {
    let store = Store::new(StoreRoot::new(xdg_data_home.join("silent-critic")));
    let ledger = Ledger::new(&store, repo_dir, plan_id);
    let verdict = Verdict::new(
        CriterionId::new(criterion_id),
        judgment,
        NonEmptyString::new("because the evidence says so")?,
    );
    if let Some(index) = criterion_id.strip_prefix("hidden-") {
        let index: usize = index.parse()?;
        ledger.record_hidden_verdict(&TaskId::new(task_id), index, &verdict, &[])?;
    } else {
        ledger.record_visible_verdict(&TaskId::new(task_id), &verdict)?;
    }
    Ok(())
}

#[test]
fn measure_reports_one_run_from_the_store() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let plan_path = repo_dir.path().join("2026-09-05-hidden-sentinel-plan.md");
    std::fs::write(&plan_path, SENTINEL_PLAN)?;
    let store_dir = tempfile::tempdir()?;

    let mut add = Command::cargo_bin("silent-critic")?;
    let add_output = add
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    assert!(add_output.status.success());
    let plan_id = String::from_utf8(add_output.stdout)?.trim().to_string();

    // A hidden-only finding: T002's hidden-1 fails while its only visible
    // judgment (visible-0) passes.
    record_verdict(
        store_dir.path(),
        repo_dir.path(),
        &plan_id,
        "T002",
        "visible-0",
        Judgment::Pass,
    )?;
    record_verdict(
        store_dir.path(),
        repo_dir.path(),
        &plan_id,
        "T002",
        "hidden-1",
        Judgment::Fail,
    )?;

    let mut human = Command::cargo_bin("silent-critic")?;
    let human_output = human
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("measure")
        .arg(&plan_id)
        .output()?;
    assert!(human_output.status.success());
    let human_stdout = String::from_utf8(human_output.stdout)?;
    assert!(
        human_stdout.contains("hidden-only finding (T002)"),
        "unexpected stdout: {human_stdout}"
    );

    let mut json = Command::cargo_bin("silent-critic")?;
    let json_output = json
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("measure")
        .arg(&plan_id)
        .arg("--format")
        .arg("json")
        .output()?;
    assert!(json_output.status.success());
    let stdout = String::from_utf8(json_output.stdout)?;
    assert!(stdout.contains("\"plan_id\""));
    assert!(stdout.contains("\"hidden_only_finding\""));
    assert!(stdout.contains("\"attribution\": \"hidden\""));

    Ok(())
}

#[test]
fn measure_with_no_plan_id_reports_a_cross_run_comparison_from_the_store_alone()
-> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;

    let plan_path_a = repo_dir.path().join("2026-09-05-run-a.md");
    std::fs::write(&plan_path_a, SENTINEL_PLAN)?;
    let mut add_a = Command::cargo_bin("silent-critic")?;
    let add_first_output = add_a
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path_a)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    assert!(add_first_output.status.success());
    let plan_id_a = String::from_utf8(add_first_output.stdout)?
        .trim()
        .to_string();
    record_verdict(
        store_dir.path(),
        repo_dir.path(),
        &plan_id_a,
        "T002",
        "visible-0",
        Judgment::Pass,
    )?;

    let plan_source_b = SENTINEL_PLAN.replace("updated_at: 2026-09-05", "updated_at: 2026-09-06");
    let plan_path_b = repo_dir.path().join("2026-09-06-run-b.md");
    std::fs::write(&plan_path_b, plan_source_b)?;
    let mut add_b = Command::cargo_bin("silent-critic")?;
    let add_second_output = add_b
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path_b)
        .arg("--repo")
        .arg(repo_dir.path())
        .output()?;
    assert!(add_second_output.status.success());
    let plan_id_b = String::from_utf8(add_second_output.stdout)?
        .trim()
        .to_string();
    record_verdict(
        store_dir.path(),
        repo_dir.path(),
        &plan_id_b,
        "T002",
        "visible-0",
        Judgment::Pass,
    )?;
    record_verdict(
        store_dir.path(),
        repo_dir.path(),
        &plan_id_b,
        "T002",
        "hidden-0",
        Judgment::Fail,
    )?;

    let mut compare = Command::cargo_bin("silent-critic")?;
    let compare_output = compare
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("measure")
        .output()?;
    assert!(compare_output.status.success());
    let compare_stdout = String::from_utf8(compare_output.stdout)?;
    assert!(compare_stdout.contains(&plan_id_a), "{compare_stdout}");
    assert!(compare_stdout.contains(&plan_id_b), "{compare_stdout}");
    assert!(
        compare_stdout.contains("runs with a hidden-only finding: 1/2"),
        "{compare_stdout}"
    );

    let mut compare_json = Command::cargo_bin("silent-critic")?;
    let compare_json_output = compare_json
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("measure")
        .arg("--format")
        .arg("json")
        .output()?;
    assert!(compare_json_output.status.success());
    let stdout = String::from_utf8(compare_json_output.stdout)?;
    assert!(stdout.contains("\"hidden_only_run_count\": 1"));
    assert!(stdout.contains("\"deltas\""));

    Ok(())
}

#[test]
fn measure_with_all_spans_every_repository_in_the_store() -> Result<(), Box<dyn Error>> {
    let repo_a = tempfile::tempdir()?;
    let repo_b = tempfile::tempdir()?;
    init_repo(repo_a.path())?;
    init_repo(repo_b.path())?;
    let store_dir = tempfile::tempdir()?;

    let plan_path_a = repo_a.path().join("2026-09-05-repo-a.md");
    std::fs::write(&plan_path_a, SENTINEL_PLAN)?;
    let mut add_a = Command::cargo_bin("silent-critic")?;
    let add_first_output = add_a
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path_a)
        .arg("--repo")
        .arg(repo_a.path())
        .output()?;
    assert!(add_first_output.status.success());
    let plan_id_a = String::from_utf8(add_first_output.stdout)?
        .trim()
        .to_string();
    record_verdict(
        store_dir.path(),
        repo_a.path(),
        &plan_id_a,
        "T002",
        "visible-0",
        Judgment::Pass,
    )?;

    let plan_path_b = repo_b.path().join("2026-09-05-repo-b.md");
    std::fs::write(&plan_path_b, SENTINEL_PLAN)?;
    let mut add_b = Command::cargo_bin("silent-critic")?;
    let add_second_output = add_b
        .env("XDG_DATA_HOME", store_dir.path())
        .arg("plan")
        .arg("add")
        .arg(&plan_path_b)
        .arg("--repo")
        .arg(repo_b.path())
        .output()?;
    assert!(add_second_output.status.success());
    let plan_id_b = String::from_utf8(add_second_output.stdout)?
        .trim()
        .to_string();
    record_verdict(
        store_dir.path(),
        repo_b.path(),
        &plan_id_b,
        "T002",
        "visible-0",
        Judgment::Pass,
    )?;

    // `plan list` from inside repo A alone would show only repo A's plan;
    // `measure --all` must see both, spanning repositories.
    let mut all_cmd = Command::cargo_bin("silent-critic")?;
    let all_output = all_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_a.path())
        .arg("measure")
        .arg("--all")
        .output()?;
    assert!(all_output.status.success());
    let stdout = String::from_utf8(all_output.stdout)?;
    assert!(stdout.contains(&plan_id_a), "{stdout}");
    assert!(stdout.contains(&plan_id_b), "{stdout}");

    Ok(())
}

#[test]
fn measure_reports_an_unknown_plan() -> Result<(), Box<dyn Error>> {
    let repo_dir = tempfile::tempdir()?;
    init_repo(repo_dir.path())?;
    let store_dir = tempfile::tempdir()?;

    let mut measure_cmd = Command::cargo_bin("silent-critic")?;
    let output = measure_cmd
        .env("XDG_DATA_HOME", store_dir.path())
        .current_dir(repo_dir.path())
        .arg("measure")
        .arg("does-not-exist")
        .output()?;

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("measuring plan"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}
