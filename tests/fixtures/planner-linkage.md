---
plan_format_version: 1
plan_id: PLAN-20260906-silent-critic-planner-linkage
title: Silent Critic planner linkage fixture
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
  summary: Prove that silent-critic links against tftio_planner.
  severity: low
  affected_area: scaffold
  user_impact: A broken dependency would silently reimplement planner behavior.
execution:
  requires_operator_approval_before_implementation: true
  requires_plan_updates_during_execution: true
  task_graph_status: ready
---

# ADR: Silent Critic planner linkage fixture

## Problem Statement

`silent-critic` must depend on `tftio_planner` rather than reimplementing any part of its
model, projection, or write path. This fixture proves the dependency links and parses.

## Source Material

### Ticket

No ticket was provided.

### Discussion Summary

This fixture exists only to exercise `tftio_planner::parse_markdown` from `silent-critic`'s
own test suite.

## Context

Task T001 scaffolds the `silent-critic` repository and must prove `tftio_planner` linkage.

## Constraints

Use LF line endings.

## Non-Goals

Exercising mutation or projection.

## Decision

Parse this fixture with `tftio_planner::parse_markdown` in a `silent-critic` integration test.

## Alternatives Considered

Reimplementing a minimal parser was rejected: the dependency exists precisely so no
part of the planner domain is duplicated here.

## Consequences

`silent-critic` fails to build if the dependency is misconfigured, catching drift early.

# Task Graph

<!-- TASK_GRAPH:BEGIN -->
```yaml
tasks:
  - id: T001
    title: Prove planner linkage
    status: not_started
    depends_on: []
    description: Parse this fixture from a silent-critic integration test.
    invariants:
      - The fixture parses without diagnostics.
    acceptance_checks:
      - The typed plan is available to the caller.
```
<!-- TASK_GRAPH:END -->

# Task Details

## T001 — Prove planner linkage

Status: `not_started`

Depends on: none

### Description

Parse this fixture from a silent-critic integration test.

### Invariants

- The fixture parses without diagnostics.

### Acceptance Checks

- The typed plan is available to the caller.

### Completion Evidence

Pending.
