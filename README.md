# silent-critic

MCP supervision server that runs planning-doc plans with silent-critic criteria

Silent Critic uses the TOML execution ledger and sealed Markdown artifact as
its canonical acceptance record. These former silent-critic capabilities are
retired: the criterion library, the immutable `.rec` record graph and its codec,
adjudication and the decision log, the evidence store, the `.sc` contract DSL,
the OpenSpec bridge, the sidecar UI, and the SQLite session model.

## Install

```sh
cargo install --locked --git https://github.com/tftio/silent-critic
```

This installs both binaries, `silent-critic` and `silent-critic-mcp`.

## Legacy plan store

silent-critic's predecessor stored plans under `$XDG_DATA_HOME/holdout` (or
`~/.local/share/holdout`); `silent-critic legacy` reads that store without
modifying it, and new state is written only below `$XDG_DATA_HOME/silent-critic`
(or `~/.local/share/silent-critic`):

```sh
silent-critic legacy plan-list --repo /path/to/repository
silent-critic legacy plan-path PLAN-ID --repo /path/to/repository
```

## Getting started

Toolchain, task execution, and hook tools are managed by mise.
The Rust toolchain is declared in `.mise.toml`. `rust-toolchain.toml` is derived
from that declaration by `mise run update` and checked against it by
`mise run check:locks`, so rustup, rust-analyzer, and your IDE resolve the same
pin mise does. Never edit it by hand.
Entering the directory does not prepare, install, or regenerate anything: setup is
explicit, so no lockfile ever changes because someone walked into the repository.

```sh
mise trust --quiet
mise install
mise run setup:idea     # optional; regenerates the gitignored .idea/
```

Tools come from `mise activate <shell>` in an interactive shell, and from mise shims
for non-interactive processes such as editors and coding agents.

## Tasks

```sh
mise run check  # check-only hooks, as CI runs them
mise run lint   # manual autofix hooks
mise run test   # test suite
mise run ci     # full CI gate
```

Dependencies move on one deliberate command, and never on their own:

```sh
mise run update       # mise tools, cargo crates, prek hooks
mise run check:locks  # read-only; fails if a lockfile is stale
```

The generated Rust gate includes formatting, TOML formatting, shell linting,
spelling, clippy, nextest, docs, unused-dependency detection, advisory audit,
license/source policy, package contents, and line coverage (see the RS-007
bypass in `REPO_INVARIANTS.md`).

## `silent-critic-mcp`

`silent-critic-mcp` is the MCP server an agent session (orchestrator or worker)
connects to for one plan. It is a thin `rmcp` stdio adapter
(`src/mcp_stdio.rs`) over the protocol-free tool surface in `src/tools.rs`:
it enumerates `ToolSurface::specs()` as MCP tools and dispatches
`tools/call` through `ToolSurface::call`, and knows nothing else about
supervision. A tool that could not do its job (an unknown task, a missing
argument, an unauthenticated caller) answers with the reason as ordinary
tool-result content flagged `isError` — never a transport-level error — so
the agent can read why and decide whether to retry.

### Environment contract

`silent-critic-mcp` reads its configuration once, at the process edge, from these
environment variables (each also settable as a `--flag`):

| Variable             | Required | Meaning                                                                                                                       |
| -------------------- | -------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `SILENT_CRITIC_STORE_ROOT`  | no       | The plan store's root directory. Defaults to `$XDG_DATA_HOME/silent-critic`, or `~/.local/share/silent-critic` when `XDG_DATA_HOME` is unset (same default `silent-critic`'s own CLI uses). |
| `SILENT_CRITIC_REPO`        | no       | The repository (or worktree) this session supervises. Defaults to the process's current directory.                             |
| `SILENT_CRITIC_PLAN_ID`     | **yes**  | The plan this session serves, as printed by `silent-critic plan add`.                                                                 |
| `SILENT_CRITIC_TOKEN`       | no       | A worker bearer token (`silent_critic_worker_...`, minted by the orchestrating session). Present means this session is scoped to that token's one task; absent means the orchestrator's own scope (no token needed — the session itself is the operator's). |
| `SILENT_CRITIC_TASK_ID`     | no       | The task a worker session is for, written into `mcp.json`'s own `env` block by `dispatch`. Passed straight through as the expected task a presented `SILENT_CRITIC_TOKEN` must resolve to; absent (a hand-built worker session, or `mcp.json` written before this field existed) falls back to deriving the expected task from the token itself. |
| `SILENT_CRITIC_CONFIG`      | no       | Path to a TOML file supplying `[harness]` and/or `[judge]` configuration (see [Harness and judge configuration](#harness-and-judge-configuration)). Absent means `dispatch` and `judge` both answer "not configured". |
| `--git-binary`        | no       | The git binary a configured harness's own worktree creation invokes (`--flag` only, no environment variable; defaults to `git`). Only consulted when `SILENT_CRITIC_CONFIG` supplies `[harness]`. |

Without `SILENT_CRITIC_TOKEN` the session sees the orchestrator tools
(`plan_status`, `next_ready`, `dispatch`, `judge`, `record_decision`,
`request_guidance`). With a valid `SILENT_CRITIC_TOKEN` it sees only the bound
task's worker tools (`brief`, `note`, `submit`). An invalid or unrecognized
token still starts the server — every call then fails readably rather than
the process refusing to start.

### Client configuration: orchestrator scope

For an orchestrating Claude Code session (no token — its own MCP session is
the operator's):

```json
{
  "mcpServers": {
    "silent-critic": {
      "command": "/path/to/silent-critic-mcp",
      "env": {
        "SILENT_CRITIC_STORE_ROOT": "/home/you/.local/share/silent-critic",
        "SILENT_CRITIC_REPO": "/path/to/the/supervised/repo",
        "SILENT_CRITIC_PLAN_ID": "2026-09-05-my-plan"
      }
    }
  }
}
```

### Client configuration: worker scope

For a dispatched worker session, bound to one task by its token:

```json
{
  "mcpServers": {
    "silent-critic": {
      "command": "/path/to/silent-critic-mcp",
      "env": {
        "SILENT_CRITIC_STORE_ROOT": "/home/you/.local/share/silent-critic",
        "SILENT_CRITIC_REPO": "/path/to/the/supervised/repo-or-worktree",
        "SILENT_CRITIC_PLAN_ID": "2026-09-05-my-plan",
        "SILENT_CRITIC_TOKEN": "silent_critic_worker_<...>"
      }
    }
  }
}
```

### Harness and judge configuration

`dispatch` and `judge` are both unreachable — every call answers "not
configured" — until an orchestrator session's `SILENT_CRITIC_CONFIG` (or
`--config`) names a TOML file supplying `[harness]` and/or `[judge]`.
Either table may be absent; a file with neither keeps both tools
unreachable, exactly as if `SILENT_CRITIC_CONFIG` were unset. The file is read
once, at the process edge, into typed configuration (`src/config.rs`) —
`silent_critic::tools` never reads it or the environment itself.

```toml
# silent-critic-config.toml
#
# [harness] is what `dispatch` launches as the worker: here, a single
# `claude -p` session pointed at the rendered brief by path, via
# `{brief}` -- recommended for the Claude CLI (see the `prompt_via` note
# below for why `brief_path` rather than `argument`).
[harness]
program = "claude"
args = ["-p", "{brief}"]
model = "claude-opus-4-6"
model_flag = "--model"
prompt_via = "brief_path"   # the harness reads the brief itself from {brief}
shell = "/bin/sh"
timeout_secs = 1800
silent_critic_mcp_path = "/usr/local/bin/silent-critic-mcp"

[harness.environment]
ANTHROPIC_API_KEY_PATH = "/run/secrets/anthropic-api-key"

# [judge] is what `judge` consults. `providers` takes exactly one or two
# entries: one provider is a single-judge run; two run the same input
# through both and record any disagreement between them. This example
# pairs `claude -p` (prompt on the command line) with `opencode run`
# (prompt on stdin) as a second, independent judge.
[judge]
retry_budget = 2
git_binary = "git"
path = "/usr/bin:/bin"
home = "/home/operator"
check_shell = "/bin/sh"
check_timeout_secs = 60

[judge.check_environment]
CI = "true"

[[judge.providers]]
id = "claude"
program = "claude"
args = ["-p"]
model = "claude-opus-4-6"
model_flag = "--model"
prompt_via = "argument"
timeout_secs = 120

[[judge.providers]]
id = "opencode"
program = "opencode"
args = ["run"]
prompt_via = "stdin"
timeout_secs = 120
```

Field notes:

- `[harness].prompt_via` is `argument` or `brief_path` only — a harness is
  never given the prompt over stdin (`crate::dispatch::PromptVia` has no
  such variant; `judge`'s own providers do, via `[[judge.providers]].prompt_via`,
  which additionally accepts `stdin`). `brief_path` (the example above) is
  recommended for the Claude CLI: it substitutes `{brief}` in `args` with
  the rendered brief's path and lets the harness read it itself, so the
  brief's content never has to survive being an argument at all. `argument`
  appends the brief's full text as the harness's final argument instead —
  after every configured option, including `model_flag`/`model` — preceded
  by a literal `--` so a POSIX-style option parser (the Claude CLI
  included) treats it as positional text rather than as an option: a
  rendered brief always begins with the plan's YAML front matter (`---`),
  which without the `--` separator a CLI would otherwise try to parse as a
  flag and fail before the worker ever starts.
- `[[judge.providers]].id` is a human-readable label for the file itself;
  `judge` always mints its own internal `judge-a`/`judge-b` identifiers
  regardless of what is written here.
- Every `*.environment` table (`[harness.environment]`,
  `[judge.check_environment]`, `[[judge.providers]].environment`) is the
  *entire* environment the corresponding subprocess sees after
  `env_clear()` — never the ambient process environment.
- `[harness]` deliberately carries no git binary/`PATH`/`HOME` fields: a
  configured harness's own worktree-creation git environment comes from
  this process's own CLI flags/environment (`--git-binary`, the inherited
  `PATH`, `HOME`), the same way `silent-critic dispatch --manual` already reads
  it. `[judge]`'s `git_binary`/`path`/`home` are unrelated: they configure
  a separate git invocation `judge` uses to capture facts from the
  dispatched worktree.
