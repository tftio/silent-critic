---
plan_format_version: 2
plan_id: PLAN-20260923-store-plan-v2
title: Silent Critic v2 store test fixture
status: approved
created_at: 2026-09-23
updated_at: 2026-09-23
owner: test
source:
  type: manual
  url: null
  external_id: null
  imported_at: null
bug:
  summary: Exercise the silent-critic plan store with a valid v2 planning document that declares a project.
  severity: low
  affected_area: scaffold
  user_impact: A store that cannot round-trip a v2 plan's project cannot group measurement by it.
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: ready
project:
  slug: silent-critic
  remote: github.com/tftio/silent-critic
---

# ADR: Silent Critic v2 store test fixture

## Problem Statement

The `silent-critic` plan store must accept a valid v2 planning document that declares
a project, bind it to a repository and base ref, and record the project's slug in the
binding alongside the repository identity and base ref.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

This fixture exists only to exercise `silent_critic::store::Store` and
`silent_critic::measure` from the crate's own test suite (T012).

## Context

T012 records the plan's project in silent-critic's binding and groups measurement by
it.

## Constraints

Use LF line endings.

## Non-Goals

Exercising mutation or projection.

## Decision

Store this fixture through `silent_critic::store::Store::add_plan` in a `silent-critic`
test and assert its binding carries `project.slug` and that `plan list` shows it.

## Alternatives Considered

Reimplementing a minimal parser was rejected: `tftio_planner` owns parsing.

## Consequences

`silent-critic` fails its tests if the store cannot round-trip a v2 plan's project.

## Operator Guidance Log

Append-only. Agents add a dated entry whenever they need operator guidance or
operator guidance changes the plan.

## Decision Log

Append-only. Decisions affecting architecture, behavior, sequencing, or
tradeoffs are recorded here.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: Round-trip this fixture through the store
    status: not_started
    owner: null
    depends_on: []
    description: Add this fixture to the store and read its binding back.
    work_items:
      - Call Store::add_plan with this fixture.
    invariants:
      - The fixture round-trips without diagnostics.
    acceptance_checks:
      - The stored plan's provenance carries project.slug silent-critic.
    completion_evidence: null
```
<!-- TASK_GRAPH:END -->

# Task Details

## T001 — Round-trip this fixture through the store

Status: `not_started`

Depends on: none

### Description

Add this fixture to the store and read its binding back.

### Invariants

- The fixture round-trips without diagnostics.

### Acceptance Checks

- The stored plan's provenance carries project.slug silent-critic.

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
