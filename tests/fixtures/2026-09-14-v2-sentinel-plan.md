---
plan_format_version: 2
plan_id: PLAN-20260914-v2-sentinel-plan
title: Silent Critic tool-surface v2 sentinel fixture
status: approved
created_at: 2026-09-14
updated_at: 2026-09-14
owner: test
source:
  type: manual
  url: null
  external_id: null
  imported_at: null
bug:
  summary: Exercise the silent-critic tool surface against a v2 plan with no declared mode or files.
  severity: low
  affected_area: scaffold
  user_impact: A tool surface that mishandles v2 plans cannot supervise them.
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: ready
---

# ADR: Silent Critic tool-surface v2 sentinel fixture

## Problem Statement

The `silent-critic` tool surface must read a Planning Document Format v2 plan: no `mode`,
no authored `blocks`, and no `files` on any task.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

This fixture exists only to exercise `silent_critic::tools` against a v2 document from the
crate's own test suite.

## Context

silent-critic uses planner 0.4.0 or later, which reads v2 documents.

## Constraints

Use LF line endings.

## Non-Goals

Exercising every planner mutation.

## Decision

Carry one task with no hidden criteria (T001, already done) and one task with a single
visible acceptance check (T002, ready to start since T001 is done), and no `files` block
on either task, since v2 plans never carry one.

## Alternatives Considered

Reimplementing hidden-criteria stripping was rejected: `tftio_planner` owns projection.

## Consequences

`silent-critic` reports T002 as downstream of T001 and does not run the
uncovered-changed-scope check against this plan.

## Operator Guidance Log

Append-only. Agents add a dated entry whenever they need operator guidance or operator
guidance changes the plan.

## Decision Log

Append-only. Decisions affecting architecture, behavior, sequencing, or tradeoffs are
recorded here.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: Prepare the ground
    status: done
    owner: null
    depends_on: []
    description: A visible task with no hidden criteria.
    work_items:
      - Prepare the ground.
    invariants:
      - The ground stays prepared.
    acceptance_checks:
      - The ground is prepared.
    completion_evidence: Done in a prior run.
  - id: T002
    title: Do the ordinary thing
    status: not_started
    owner: null
    depends_on:
      - T001
    description: A task carrying one visible acceptance check for the v2 sentinel test.
    work_items:
      - Do the ordinary thing.
    invariants:
      - The ordinary thing remains done.
    acceptance_checks:
      - The ordinary thing is done.
    completion_evidence: null
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

## T002 — Do the ordinary thing

Status: `not_started`

Depends on: T001

### Description

A task carrying one visible acceptance check for the v2 sentinel test.

### Invariants

- The ordinary thing remains done.

### Acceptance Checks

- The ordinary thing is done.

### Completion Evidence

Pending.

# Execution Protocol

Agents executing this plan must:

- Read the full plan and verify a task's dependencies are `done` before starting it.
- Set a task to `in_progress` and update `updated_at` when starting.
- Update the plan when they find it wrong, discover missing context, change the approach,
  need operator guidance, or alter the task graph.
- When operator guidance is needed: add an Operator Guidance Log entry, set affected tasks
  to `blocked`, and not guess when guessing risks rework, data loss, security, or
  product-behavior changes.
- When completing a task: verify invariants, run or describe acceptance checks, record
  `completion_evidence`, set status `done`, update `updated_at`, and update ADR sections
  only when the task changed context, decision, alternatives, or consequences.
- Treat the Operator Guidance Log, Decision Log, and completion evidence as append-only;
  supersede prior entries with a new dated entry rather than deleting them.
