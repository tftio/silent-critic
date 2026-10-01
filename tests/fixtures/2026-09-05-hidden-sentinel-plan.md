---
plan_format_version: 1
plan_id: PLAN-20260905-hidden-sentinel-plan
title: Silent Critic tool-surface sentinel fixture
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
  summary: Exercise the silent-critic tool surface against a plan with hidden criteria.
  severity: low
  affected_area: scaffold
  user_impact: A tool surface that leaks hidden criteria cannot supervise anything.
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: ready
---

# ADR: Silent Critic tool-surface sentinel fixture

## Problem Statement

The `silent-critic` tool surface must never leak hidden-criterion text, count, or
identifiers through any orchestrator-scope tool response.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

This fixture exists only to exercise `silent_critic::tools` from the crate's own
test suite.

## Context

Task T005 implements the protocol-free tool surface with role gating and
worker projection.

## Constraints

Use LF line endings.

## Non-Goals

Exercising every planner mutation.

## Decision

Carry one task with no hidden criteria (T001, already done) and one task
with two hidden criteria, one per evaluator kind that carries its own
check/ask field (`human_judgment` with `ask`, `automated` with `check`),
each carrying the unique sentinel string in every hidden field it has
(T002, ready to start since T001 is done).

## Alternatives Considered

Reimplementing hidden-criteria stripping was rejected: `tftio_planner` owns
projection.

## Consequences

`silent-critic` fails its tests if any orchestrator-scope tool response contains
the sentinel.

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
    completion_evidence: Done in a prior run.
  - id: T002
    title: Do the sensitive thing
    status: not_started
    depends_on:
      - T001
    description: A task carrying one hidden criterion for the sentinel test.
    invariants:
      - The sensitive thing remains done.
    acceptance_checks:
      - The sensitive thing is done.
    hidden_criteria:
      - claim: SENTINEL-7f3a9c the worker did not weaken any existing check.
        criticality: must
        evaluator: human_judgment
        ask: SENTINEL-7f3a9c did the worker disable, skip, or loosen any check?
        why_hidden: SENTINEL-7f3a9c visible, this becomes permission to argue.
        counterfactual: SENTINEL-7f3a9c visible, the worker would rationalize instead.
      - claim: SENTINEL-7f3a9c the automated check still passes.
        criticality: must
        evaluator: automated
        check: SENTINEL-7f3a9c run the sentinel check command.
        why_hidden: SENTINEL-7f3a9c visible, the worker would special-case this check.
        counterfactual: SENTINEL-7f3a9c visible, the worker would hardcode the expected result.
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

Done in a prior run.

## T002 — Do the sensitive thing

Status: `not_started`

Depends on: T001

### Description

A task carrying one hidden criterion for the sentinel test.

### Invariants

- The sensitive thing remains done.

### Acceptance Checks

- The sensitive thing is done.

### Completion Evidence

Pending.
