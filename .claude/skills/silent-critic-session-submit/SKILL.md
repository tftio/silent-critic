---
name: silent-critic-session-submit
description: Submit evidence for one visible criterion. Use when the worker is ready to submit evidence against one of the criteria visible in the active session manifest. Do not use when the criterion is not visible in the current manifest or the worker token is not set
---

# silent-critic — session-submit

Submit evidence for one visible criterion

## When to use

the worker is ready to submit evidence against one of the criteria visible in the active session manifest

## When not to use

the criterion is not visible in the current manifest or the worker token is not set

## Commands

- `silent-critic session submit`

## Flags

- `session submit --criterion`

## Examples

- `silent-critic session submit --criterion <ID>`

## Output

output follows the existing CLI contract for session submit

## Constraints

requires the runtime SILENT_CRITIC_TOKEN worker token and a visible criterion id
