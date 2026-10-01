---
name: silent-critic-session-manifest
description: Read the worker-visible contract manifest for the active session. Use when the worker needs to read the contract manifest of visible criteria for the active session. Do not use when the user is asking about session history, completed sessions, or the global criterion library
---

# silent-critic — session-manifest

Read the worker-visible contract manifest for the active session

## When to use

the worker needs to read the contract manifest of visible criteria for the active session

## When not to use

the user is asking about session history, completed sessions, or the global criterion library

## Commands

- `silent-critic session manifest`

## Flags

- none declared

## Examples

- `silent-critic session manifest`

## Output

output follows the existing CLI contract for session manifest

## Constraints

existing command validation and auth rules still apply
