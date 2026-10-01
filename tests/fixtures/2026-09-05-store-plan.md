---
plan_format_version: 1
plan_id: PLAN-20260905-store-plan
title: Silent Critic store test fixture
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
  summary: Exercise the silent-critic plan store with a valid planning document.
  severity: low
  affected_area: scaffold
  user_impact: A store that cannot round-trip a valid plan cannot supervise anything.
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: ready
---

# ADR: Silent Critic store test fixture

## Problem Statement

The `silent-critic` plan store must accept a valid planning document, bind it to a
repository and base ref, and hand it back unchanged.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

This fixture exists only to exercise `silent_critic::store::Store` from the crate's
own test suite.

## Context

Task T002 implements the out-of-repo plan store and provenance binding.

## Constraints

Use LF line endings.

## Non-Goals

Exercising mutation or projection.

## Decision

Store this fixture through `silent_critic::store::Store::add_plan` in a `silent-critic`
unit test.

## Alternatives Considered

Reimplementing a minimal parser was rejected: `tftio_planner` owns parsing.

## Consequences

`silent-critic` fails its tests if the store cannot round-trip a valid plan.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: Round-trip this fixture through the store
    status: not_started
    depends_on: []
    description: Add this fixture to the store and read it back.
    invariants:
      - The fixture round-trips without diagnostics.
    acceptance_checks:
      - The stored plan's provenance matches the repository it was bound to.
```
<!-- TASK_GRAPH:END -->

# Task Details

## T001 — Round-trip this fixture through the store

Status: `not_started`

Depends on: none

### Description

Add this fixture to the store and read it back.

### Invariants

- The fixture round-trips without diagnostics.

### Acceptance Checks

- The stored plan's provenance matches the repository it was bound to.

### Completion Evidence

Pending.
