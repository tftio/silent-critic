# Developing silent-critic

This guide covers building, testing, and the quality gates. Before changing
any Rust, read [`REPO_INVARIANTS.md`](REPO_INVARIANTS.md): it records design
invariants the compiler and lints cannot express, and the active bypasses.

## Prerequisites

- [`mise`](https://mise.jdx.dev/), which declares every tool this repository
  uses in `.mise.toml` and pins them in `mise.lock`: the Rust toolchain,
  `prek`, `cocogitto`, `taplo`, `shellcheck`, `typos`, and the cargo helpers
  (`cargo-nextest`, `cargo-machete`, `cargo-audit`, `cargo-deny`,
  `cargo-llvm-cov`, `cargo-mutants`).
- `rust-toolchain.toml` is derived from the `rust` entry in `.mise.toml` by
  `mise run update`; never edit it by hand.
- Entering the directory installs nothing. Run once per clone:

  ```sh
  mise trust --quiet
  mise install
  mise run hooks:install   # prek pre-commit and commit-msg hooks
  mise run setup:idea      # optional; regenerates the gitignored .idea/
  ```

- `tftio-planner` is a git dependency on
  `https://github.com/tftio/planner` at a release tag, so the first build
  needs network access to GitHub as well as crates.io.

## Tasks

Tasks are defined in `.mise.toml`; `mise tasks` lists them.

```sh
mise run check            # every check-only hook, as CI runs it
mise run ci               # check plus coverage: the full CI gate
mise run test             # cargo nextest
mise run lint             # manual autofix hooks (fmt, taplo, typos)
mise run update           # move every pinned dependency
mise run check:locks      # fail if Cargo.lock or the toolchain pin is stale
mise run exhaustive:mutants   # mutation testing; slow, not part of CI
```

Individual checks are `check:fmt`, `check:toml`, `check:shell`,
`check:spelling`, `check:clippy`, `check:test`, `check:doc`, `check:deps`,
`check:audit`, `check:deny`, `check:package`, and `check:coverage`. `update`
is the only task that rewrites a lockfile.

## Quality gates

`prek.toml` is the source of truth for the hooks.

| Stage | Runs |
|-------|------|
| pre-commit | TOML/YAML/large-file checks and every `check:*` task except coverage |
| commit-msg | `cog verify` (Conventional Commits) |
| manual | the `fix:*` tasks, via `mise run lint` |

Lints live in the `[lints]` table of `Cargo.toml` (clippy `all` and `pedantic`
denied, plus `unwrap`, `expect`, `panic`, indexing, and similar).
`clippy.toml` denies `std::env` outside the process edge (RS-008).
Coverage must stay at or above the floor in `check:coverage`.

`tests/judge_real_providers.rs` is `#[ignore]`d: it calls real model
providers and needs their CLIs installed and authenticated. Run it by hand
with `cargo test --test judge_real_providers -- --ignored --nocapture`.

## Code organization

- `src/bin/silent-critic.rs`: the operator CLI (plan store, manual dispatch,
  seal, measure, read-only legacy store access).
- `src/bin/silent-critic-mcp.rs`: the stdio MCP server; `src/mcp_stdio.rs`
  adapts MCP to the protocol-free tool surface in `src/tools.rs`.
- `src/lib.rs` and its modules hold all behavior: `model`, `store`,
  `provenance`, `ledger`, `dispatch`, `worktree`, `token`, `evaluate`
  (automated checks and the judge), `provider`, `git`, `seal`, `render`,
  `measure`, and `config`.
- Plan parsing, projection, and writes are delegated to `tftio_planner`
  (HO-003).
- Unit tests live in `#[cfg(test)]` modules; integration tests and their
  fixtures are under `tests/`.

## CI and releases

GitHub Actions runs `mise run ci` on every pull request and on pushes to
`main`, through the shared workflow in `tftio/gh-actions`.

Releases use [release-plz](https://release-plz.dev) in git-only mode with
publishing disabled: a push to `main` opens a release pull request that bumps
the version and `CHANGELOG.md`; merging it tags the release and creates a
GitHub release, which triggers the binary artifact build. Versions come from
Conventional Commits; do not edit versions by hand. Nothing is published to
crates.io.
