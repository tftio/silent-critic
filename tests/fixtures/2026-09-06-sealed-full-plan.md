---
plan_format_version: 1
plan_id: PLAN-20260906-sealed-full-plan
title: Silent Critic seal fixture
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
  summary: Exercise sealing a closed task graph with every residual kind.
  severity: low
  affected_area: scaffold
  user_impact: A seal that hides a residual cannot supervise anything.
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: executing
---

# ADR: Silent Critic seal fixture

## Problem Statement

Sealing a closed task graph must disclose every hidden criterion and
surface every residual before routine passing checks.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

This fixture exists only to exercise `silent_critic::seal` and `silent_critic::render`
from the crate's own test suite.

## Context

Task T011 implements sealing and the disclosed review artifact.

## Constraints

Use LF line endings.

## Non-Goals

Exercising every planner mutation.

## Decision

Carry one closed task graph with a human-judgment criterion still awaiting
the operator, an undetermined hidden criterion, a passing automated hidden
criterion, an abandoned task, and an uncovered-changed-scope plus
judge-disagreement residual recorded in the Operator Guidance Log.

## Alternatives Considered

Driving every residual through `silent_critic::ledger::Ledger` was rejected for
this fixture: authoring the residual shapes directly keeps the fixture
readable and exercises `unresolved_items_in`'s parsing on its own terms.

## Consequences

`silent-critic` fails its tests if a sealed artifact omits a hidden criterion's
verdict or renders a residual after a routine passing check.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: Prepare the ground
    status: done
    depends_on: []
    description: A visible task with no hidden criteria.
    invariants:
      - The ground stays prepared.
    acceptance_checks:
      - The ground is prepared.
    completion_evidence: "2026-09-01 — evidence: visible criterion visible-0 verdict: pass\nrationale: the ground looked fine\n\n2026-09-01 — completed: shipped"
  - id: T002
    title: Do the sensitive thing
    status: done
    depends_on:
      - T001
    description: A task carrying three hidden criteria in three states.
    invariants:
      - The sensitive thing remains done.
    acceptance_checks:
      - The sensitive thing is done.
    hidden_criteria:
      - claim: the worker did not weaken any existing check
        criticality: must
        evaluator: human_judgment
        ask: did the worker disable, skip, or loosen any check?
        why_hidden: visible, this becomes permission to argue.
        counterfactual: visible, the worker would rationalize instead.
      - claim: the automated check still passes
        criticality: must
        evaluator: automated
        check: run the sentinel check command.
        why_hidden: visible, the worker would special-case this check.
        counterfactual: visible, the worker would hardcode the expected result.
        verdict: pass
        rationale: the check ran green.
        evidence:
          - summary: sentinel check exited 0
            provenance: tool_authored
      - claim: the design doc's caveat was actually followed
        criticality: should
        evaluator: agent_evaluated
        ask: did the worker follow the design doc's stated caveat?
        why_hidden: visible, the worker would claim compliance without evidence.
        counterfactual: visible, the worker would assert compliance.
        verdict: undetermined
        rationale: the transcript did not mention the caveat either way.
        evidence_needed: a transcript excerpt naming the caveat.
    completion_evidence: "2026-09-02 — evidence: visible criterion visible-0 verdict: pass\nrationale: also fine\n\n2026-09-02 — completed: done"
  - id: T003
    title: Descoped side quest
    status: abandoned
    depends_on:
      - T001
    description: A task abandoned before completion.
    invariants:
      - Nothing carried over half-finished.
    acceptance_checks:
      - Nothing to check.
    completion_evidence: "2026-09-03 — abandoned: descoped after triage; no longer needed"
  - id: T004
    title: Touches more than it declared
    status: done
    depends_on:
      - T001
    description: A task whose judge run disagreed with itself and whose diff exceeded its declared scope.
    invariants:
      - Declared scope is honored.
    acceptance_checks:
      - The declared scope covers every changed path.
    completion_evidence: "2026-09-04 — evidence: judge run: disposition=accept; visible[0] criterion=\"the declared scope covers every changed path\" judgment=pass rationale=\"looked complete\"\n\n2026-09-04 — completed: shipped anyway"
```
<!-- TASK_GRAPH:END -->

# Task Details

## T001 — Prepare the ground

Status: `done`

Depends on: none

### Description

A visible task with no hidden criteria.

### Invariants

- The ground stays prepared.

### Acceptance Checks

- The ground is prepared.

### Completion Evidence

Shipped.

## T002 — Do the sensitive thing

Status: `done`

Depends on: T001

### Description

A task carrying three hidden criteria in three states.

### Invariants

- The sensitive thing remains done.

### Acceptance Checks

- The sensitive thing is done.

### Completion Evidence

Done.

## T003 — Descoped side quest

Status: `abandoned`

Depends on: T001

### Description

A task abandoned before completion.

### Invariants

- Nothing carried over half-finished.

### Acceptance Checks

- Nothing to check.

### Completion Evidence

Descoped after triage.

## T004 — Touches more than it declared

Status: `done`

Depends on: T001

### Description

A task whose judge run disagreed with itself and whose diff exceeded its declared scope.

### Invariants

- Declared scope is honored.

### Acceptance Checks

- The declared scope covers every changed path.

### Completion Evidence

Shipped anyway.

## Operator Guidance Log

RESIDUAL-BEGIN uncovered_changed_scope
task: T004
paths: src/unexpected.rs,src/other.rs
RESIDUAL-END

RESIDUAL-BEGIN judge_disagreement
task: T004
criterion: hidden-0
first-judgment: pass
first-rationale: looks fine
second-judgment: fail
second-rationale: not fine
RESIDUAL-END

## Decision Log

**Sealing discloses hidden criteria**

Sealing a plan discloses its hidden criteria and their verdicts to keep
post-adjudication transparency a core property of the framework.
