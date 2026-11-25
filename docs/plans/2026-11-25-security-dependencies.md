---
plan_format_version: 2
plan_id: PLAN-20261125-security-dependencies
title: Update security dependencies
status: implemented
created_at: 2026-11-25
updated_at: 2026-11-25
owner: tftio
agent_generated: true
agent_model: unknown
agent_date: 2026-11-25
source:
  type: manual
  url: null
  external_id: null
  imported_at: null
bug:
  summary: Dependency advisories and missing tool checksums prevent security updates from passing CI.
  severity: high
  affected_area: dependency graph and developer tools
  user_impact: Security updates remain unmerged.
execution:
  requires_operator_approval_before_implementation: false
  requires_plan_updates_during_execution: true
  task_graph_status: complete
---

# ADR: Update security dependencies

## Problem Statement

Three Renovate security PRs stop at a missing Taplo checksum. The base also
fails advisory checks for rmcp, Tokio, chrono and unsafe-libyaml.

## Source Material

### Ticket

GitHub PRs tftio/silent-critic#5, #6 and #7; the operator requested local fixes.

### Discussion Summary

There is no separate ticket discussion. The operator approved upgrades and an
explicit allowance for chrono, rather than an rmcp fork. No push is authorized.

## Context

Verified checksums are prepared in this worktree. Tests and coverage pass, but
advisory checks fail. Planner v0.5.5 replaces chrono and unsafe-libyaml; rmcp
v3.11.0 still needs chrono. Rustix also has an unsoundness warning in cargo audit.

## Constraints

Preserve the original checkout, Rust pin, coverage gate and every advisory
except the approved chrono exception. Use SSH for Git fetches. Limit updates
to affected packages and their required dependencies.

## Non-Goals

No forks, upstream repository edits, blanket tool upgrades, commits or pushes.

## Decision

Raise rmcp and Tokio minimums to patched versions, pin planner v0.5.5 over SSH,
upgrade nextest, and refresh affected locks. Allow only RUSTSEC-2026-0543 in
cargo-deny, documenting the reason and upstream issue. Keep cargo audit's warning
visible. Use targeted Cargo and mise lock refreshes instead of the blanket update
task to avoid unrelated upgrades. Automated checks remain read-only.

## Alternatives Considered

The operator declined an rmcp fork. Allowing all advisories would conceal issues
the upgrades can fix. The blanket update task changes unrelated tools.

## Consequences

Chrono remains an accepted maintenance risk, not a repaired dependency.
Planner's YAML parser update changes diagnostics; compatibility tests must pass.
Linux CI must confirm platform-specific behavior after publication.

## Operator Guidance Log

Append-only.

### 2026-11-25 — operator

The operator approved released upgrades and an explicit chrono allowance rather
than a maintained rmcp fork. Only RUSTSEC-2026-0543 may be allowed.

## Decision Log

Append-only. Use released dependencies and leave unrelated tool pins unchanged.

### 2026-11-25 — compatibility update

rmcp 3.11.0 deprecates the ServerInfo alias. Use its ServerConfig alias in the
stdio adapter; both name InitializeResult. Existing protocol tests and coverage
pass with the new dependencies. No protocol behavior is changed by the rename.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: Update and verify affected dependencies
    status: done
    depends_on: []
    owner: tftio
    description: Upgrade affected dependencies and verify the accepted chrono exception.
    work_items:
      - Update dependency minimums and nextest while retaining verified checksums.
      - Add the chrono allowance and refresh affected locks.
      - Run full CI, inspect advisories and review the diff.
    invariants:
      - Only chrono is allowed and coverage enforcement is unchanged.
      - The original checkout is untouched and nothing is pushed.
    acceptance_checks:
      - mise run ci passes with only the documented chrono warning remaining.
      - mise lock --check and git diff --check pass.
      - unsafe-libyaml is absent from the resolved graph.
    completion_evidence: >
      Cargo.toml and Cargo.lock resolve rmcp 3.11.0, Tokio 1.54.1,
      rustix 1.1.6 and planner 0.5.5; unsafe-libyaml is absent. .mise.toml
      and mise.lock pin nextest 0.9.150 and retain verified tool checksums.
      deny.toml allows only RUSTSEC-2026-0543 with the operator's reason.
      src/mcp_stdio.rs uses the non-deprecated ServerConfig alias.
      mise run ci passes: 475 tests pass, one existing test is skipped,
      and line coverage is 99.81% against the unchanged 99.8% gate.
      cargo audit reports zero vulnerabilities and only the chrono warning.
      mise lock --check and git diff --check pass; the original checkout
      retains its original mise.lock hash. Nothing was committed or pushed.
```
<!-- TASK_GRAPH:END -->

# Task Details

## T001 — Update and verify affected dependencies

Status: `done`. Depends on: none.

Upgrade affected dependencies and nextest, refresh affected locks, and record the
chrono exception. Preserve other advisory and coverage enforcement and the
original checkout. Run `mise run ci`, `mise lock --check`, `git diff --check`,
and graph inspection proving unsafe-libyaml is absent. Completion evidence will
name changed files and verification results.

### Completion Evidence

`Cargo.toml` and `Cargo.lock` upgrade rmcp, Tokio, rustix and planner, removing
unsafe-libyaml. `.mise.toml` and `mise.lock` pin nextest 0.9.150 and retain the
verified tool checksums. `deny.toml` allows only RUSTSEC-2026-0543, with the
operator-approved reason. `src/mcp_stdio.rs` uses the ServerConfig alias.
`mise run ci` passes: 475 tests pass, one existing test is skipped, and line
coverage is 99.81% against the unchanged 99.8% gate. Cargo audit reports zero
vulnerabilities and only chrono's unmaintained warning. Lock validation and
diff whitespace checks pass. The original checkout is unchanged; no commit or
push was made.

# Execution Protocol

Read the full plan and verify dependencies are done before starting. Set the task
in progress and update the date at start. Update the plan for missing context, an
incorrect approach or a changed graph. When guidance is needed, append guidance,
block the task and ask rather than guessing about scope, security or behavior.
On completion, verify invariants and acceptance checks, append evidence naming
changed files, mark the task done and update the date. Amend the ADR when its facts
change. Keep guidance, decisions and completion evidence append-only. Validate
after each transition.
