---
name: silent-critic-session-status
description: Inspect the currently active worker session. Use when the user wants to check the state of the active worker session under silent-critic supervision. Do not use when no silent-critic session is active or the user is asking about silent-critic configuration outside a session
---

# silent-critic — session-status

Inspect the currently active worker session

## When to use

the user wants to check the state of the active worker session under silent-critic supervision

## When not to use

no silent-critic session is active or the user is asking about silent-critic configuration outside a session

## Commands

- `silent-critic session status`

## Flags

- none declared

## Examples

- `silent-critic session status`

## Output

output follows the existing CLI contract for session status

## Constraints

existing command validation and auth rules still apply
