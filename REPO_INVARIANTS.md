# Repository invariants

These invariants are binding for humans and coding agents working in this repository.
Bypasses must be explicit: use `INVARIANT-BYPASS(<ID>): <reason>` in the smallest
possible change and explain why the invariant cannot hold.

## Engineering invariants

| ID | Invariant | Enforcement |
|---|---|---|
| ENG-001 | Keep changes single-purpose and use Conventional Commits for committed work. | Review + `cog verify` |
| ENG-002 | Never commit secrets, credentials, or tokens; inject configuration from the environment rather than hardcoding it. | Review + secret scanning |
| ENG-003 | Every behavior change ships with tests that exercise real systems, assert outcomes rather than implementation, and cover error paths. Do not skip, xfail, or delete tests to make checks pass. | Tests + review |
| ENG-004 | Fail loudly with actionable context at the source; do not add silent `except` blocks or swallowed `Result` values. | Review |
| ENG-005 | Distinguish recoverable domain errors (return typed error values) from invariant violations (assert and crash with diagnostics). | Review |
| ENG-006 | Parse external data into typed containers at the system boundary; validate once on the way in and trust the types inside. | Review |
| ENG-007 | Keep retained state immutable; store snapshots in fields, module- and class-level bindings, closures, and caches, never aliased mutable objects. | Review |
| ENG-008 | Keep business logic in pure, deterministic functions; confine I/O, logging, and state mutation to a thin imperative shell. | Review |
| ENG-009 | Model data so illegal states are unrepresentable; dispatch over closed sum types exhaustively with no silent catch-all. | Review |
| ENG-010 | Wrap third-party dependencies behind domain-specific interfaces rather than threading their APIs through the codebase. | Review |
| ENG-011 | Fit the repository's established conventions; do not rewrite working code solely to change its library, tooling, or style. | Review |
| ENG-012 | Keep formatting, linting, typing, and tests clean before handing off work. | `mise run check` |
| ENG-013 | `.mise.toml` plus `mise.lock` are the repository-owned tool declarations; hooks and CI must invoke `mise run` tasks rather than reimplementing checks. | Review + CI |
| ENG-014 | Canonical formats, one per tier. Human-editable configuration is TOML. Internal serialization of this project's own data structures uses typed s-expressions with repo-owned readers, writers, and a checked-in schema. Machine-to-machine protocol uses CBOR constrained to RFC 8949 §4.2 Core Deterministic Encoding (definite lengths, smallest lossless numbers, bytewise-sorted map keys), with schemas in CDDL. JSON and XML are never canonical: they appear only behind compatibility adapters for external tools and platform standards, including optional `--json` output flags. | Review + language checks below |

## Rust invariants

| ID | Invariant | Enforcement |
|---|---|---|
| RS-001 | Use Rust 2024 with the exact Rust toolchain declared in `.mise.toml`. `rust-toolchain.toml` is derived from that declaration by `mise run update` and verified against it by `mise run check:locks`; never hand-edit it, and never introduce a second hand-maintained pin. | Review + `mise run check:locks` |
| RS-002 | Commit `Cargo.lock` for every Rust repository, including libraries, so local and CI dependency resolution use the same artifact. | Review + CI |
| RS-003 | Deny unsafe code, missing docs, clippy warnings, unwrap/expect/panic/todo/unimplemented/dbg, wildcard imports, enum glob imports, and unchecked indexing. | `mise run check:clippy` |
| RS-004 | Keep command entry points thin; put behavior in the library crate and cover it with tests. | Tests + review |
| RS-005 | Model recoverable failures as typed error enums with `thiserror`; use `anyhow` only at binary or integration boundaries. | Review |
| RS-006 | Validate dependency advisories, licenses, duplicate dependency shape, unused dependencies, documentation, packaging, and spelling before handoff. | `mise run check` |
| RS-007 | Maintain 100% line coverage for generated CI. Lowering coverage requires an explicit invariant bypass. | `mise run check:coverage` |
| RS-008 | Read the process environment once, at the process edge, into a typed configuration value — `clap`'s `env` attribute for a CLI, or a single constructor — and never elsewhere; never mutate it. `clippy.toml` denies `std::env` outright, so that one boundary carries an `#[allow(clippy::disallowed_methods)]` with a reason. | `mise run check:clippy` |
| RS-009 | Refines ENG-014. TOML is the configuration format; internal serialization goes through repo-owned typed s-expression codecs; machine protocol goes through ciborium constrained to RFC 8949 §4.2 Core Deterministic Encoding (definite lengths, smallest lossless numbers, bytewise-sorted map keys), with schemas in CDDL. `serde_json` is banned by `deny.toml`; a compatibility adapter removes the ban with an `INVARIANT-BYPASS(RS-009)` note beside the entry it removes. | `mise run check:deny` |

## CLI invariants

| ID | Invariant | Enforcement |
|---|---|---|
| CLI-001 | Keep command parsing and process I/O in `src/main.rs`; delegate domain behavior into `src/lib.rs`. | Tests + review |
| CLI-002 | Console output is user-facing API; cover non-trivial output behavior with integration tests. | Tests |

## SilentCritic invariants

`silent-critic` declares two binaries — `silent-critic` (operator CLI) and `silent-critic-mcp` (stdio MCP
server) — instead of a single `src/main.rs`, so CLI-001's process-I/O boundary applies
per binary under `src/bin/`, not to one file.

| ID | Invariant | Enforcement |
|---|---|---|
| HO-001 | Hidden criteria — their text, count, identifiers, evaluator specification, or any value from which their existence can be inferred — reach only the judge subprocess and the operator. No orchestrator-scope tool response, worker-facing projection, log line, or error message may carry them or leak their presence. | Review + tests on the judge/orchestrator boundary |
| HO-002 | No agent authors `why_hidden` or `counterfactual`. The plan document produced and validated by `tftio_planner` is the only durable artifact for hidden-criteria material; nothing here mints, caches, or re-derives it. | Review |
| HO-003 | `tftio_planner` owns plan parsing, projection, mutation, and atomic writes. None of that model, projection, or write path is reimplemented in `silent-critic`; every such operation is delegated to `tftio_planner`, proven by a linkage test that calls a real `tftio_planner` function. | `cargo tree` + tests + review |
| HO-004 | The operator plan's concealment from a dispatched worker is by *location*, not by enforcement: `dispatch` (T009) places the worktree and its scratch files under the store root, outside the plan directory, so a worker confined to its own worktree cannot see the plan — but a worker that reads outside its own worktree can still reach the operator plan through the store. Sandboxing that enforces the boundary is out of scope for T009 and deferred. | Review |

Active bypasses: `INVARIANT-BYPASS(RS-009)` in `deny.toml` (T001) permits `serde_json` for the `tftio_planner` dependency's diagnostic interface plus the future MCP protocol adapter and judge provider channel, never the domain model or canonical storage; `INVARIANT-BYPASS(RS-007)` (T010, widened by T011) lowers `check:coverage`'s `--fail-under-lines` from 100 to 99.8 in `.mise.toml` because the pinned cargo-llvm-cov/LLVM report counts lines as missed in never-executed crate instances although every line executes in another instance, confirmed by `--show-missing-lines` and the LCOV export both reporting 0 uncovered lines regardless; the phantom count grows with crate size (5 lines at ~6.5k lines on the morning of 2026-09-06, 9 at ~8.3k that evening), restorable once a toolchain/cargo-llvm-cov update reconciles per-instance line counts.

`INVARIANT-BYPASS(RS-006)` in `.mise.toml` narrows `check:package` to `cargo package --list`: `tftio-planner` is a git dependency with no crates.io release, `cargo package` refuses a dependency without a registry version, and the crate sets `publish = false`, so only the package's contents are checked. Restore the full package build if `tftio-planner` is published to a registry again.
