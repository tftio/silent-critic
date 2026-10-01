//! Dispatch: launch a worker into a git worktree with a task-scoped token
//! (T009).
//!
//! Given a ready task, [`dispatch`] creates a git worktree from the plan's
//! recorded base commit, mints a token scoped to exactly that task, renders
//! the plan's worker-safe projection to a scratch path outside the
//! repository, writes an MCP client config a Claude-Code-shaped harness can
//! be pointed at, launches the configured harness as a child process
//! through [`crate::evaluate::automated::run_check`], and records the
//! invocation as tool-authored [`Evidence`]. [`prepare`] does everything
//! except the launch, for the manual escape hatch (`silent-critic dispatch
//! --manual`) documented on `src/bin/silent-critic.rs`.
//!
//! The operator plan itself is read only in memory
//! (`tftio_planner::project_worker_markdown`'s input); nothing here writes
//! it, copies it, or symlinks it into the worktree or the run-scratch
//! directory -- the only files this module writes under either are
//! `brief.md` (the worker-safe projection, already stripped of hidden
//! criteria by `tftio_planner`) and `mcp.json` (client wiring, no plan
//! content). The worktree and run-scratch directory live under the store
//! root (`<store_root>/runs/<repo_identity>/<plan_id>/<task_id>/`), never
//! under the plan's own directory, so no ancestor of either is the plan
//! directory (`REPO_INVARIANTS.md` HO-004): this conceals the operator plan
//! by *location*, not by enforcement -- a worker that reads outside its own
//! worktree can still reach the plan through the store, and closing that
//! gap is a deferred sandboxing track, not this task's job.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use thiserror::Error;
use uuid::Uuid;

use crate::evaluate::automated::{
    self, AutomatedCheckError, CheckEnvironment, CheckResult, CheckSpec, CheckTimeout,
};
use crate::model::{
    EmptyStringError, Evidence, EvidenceId, EvidenceProvenance, NonEmptyString, PlanId, TaskId,
};
use crate::provenance::RepoIdentity;
use crate::store::{Store, StoreError};
use crate::token::{Role, Token, TokenError, TokenRegistry};
use crate::tools::is_safe_filename_component;
use crate::worktree::{self, GitEnv, WorktreeError};

/// The run-state sidecar directory name, matching `src/tools.rs`'s own
/// `RUN_DIR` constant (kept separate rather than shared: the two modules
/// have no other coupling, and both names are fixed strings unlikely to
/// diverge). Only the token registry (`tokens.toml`) still lives here,
/// under the plan directory: the worktree and its scratch files moved out
/// to [`RUNS_DIR`] under the store root (fix round 1).
const RUN_DIR: &str = "run";
const TOKENS_FILE_NAME: &str = "tokens.toml";
/// The directory, directly under the store root, holding every dispatched
/// task's worktree and scratch files: `<store_root>/runs/<repo_identity>/<plan_id>/<task_id>/`.
const RUNS_DIR: &str = "runs";
const WORKTREE_DIR_NAME: &str = "worktree";
const BRIEF_FILE_NAME: &str = "brief.md";
const MCP_CONFIG_FILE_NAME: &str = "mcp.json";

// ---------------------------------------------------------------------
// Harness configuration
// ---------------------------------------------------------------------

/// Configuration `dispatch` needs regardless of whether it launches a
/// harness itself: the git environment for worktree creation, and where the
/// `silent-critic-mcp` binary lives so `mcp.json` can name it.
///
/// Read once at the process edge (RS-008) and passed through -- this crate
/// never reads `std::env` itself.
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// The explicit git environment used to create and remove worktrees.
    pub git: GitEnv,
    /// The path to the `silent-critic-mcp` binary, written into `mcp.json` as the
    /// server command a harness's MCP client should run.
    pub silent_critic_mcp_path: PathBuf,
}

/// How the harness expects to receive the worker's prompt.
///
/// No `Stdin` variant: [`crate::evaluate::automated::run_check`] always
/// spawns its child with a null stdin and this crate does not modify
/// `src/evaluate/`, so a stdin-delivered prompt is not representable here
/// rather than typed as a variant `dispatch` would have to reject at
/// runtime (`REPO_INVARIANTS.md` ENG-009). A harness that needs one is a
/// later change — `judge`'s `CommandProvider` (T008) already supports
/// stdin if that path is ever needed for dispatch too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptVia {
    /// Appended to the harness's argument list, as the brief's full text.
    Argument,
    /// Not passed as prompt content at all: the harness is expected to
    /// read the brief itself from the path substituted for `{brief}` in
    /// its arguments.
    BriefPath,
}

/// The full specification of a harness to launch for a dispatched worker.
#[derive(Debug, Clone)]
pub struct HarnessSpec {
    /// Configuration shared with the no-launch (`prepare`) path.
    pub config: DispatchConfig,
    /// The harness executable to run.
    pub program: PathBuf,
    /// Arguments to pass, in order. May reference `{brief}`, `{worktree}`,
    /// or `{mcp}` as literal placeholders; `dispatch` substitutes each with
    /// the resolved brief path, worktree path, or MCP config path before
    /// launching.
    pub args: Vec<String>,
    /// The model identifier to request, if the harness takes one.
    pub model: Option<String>,
    /// The flag name used to pass `model` (e.g. `--model`), if any. Only
    /// consulted when `model` is also `Some`.
    pub model_flag: Option<String>,
    /// How the harness expects the worker's prompt delivered.
    pub prompt_via: PromptVia,
    /// Explicit environment pairs to add to the harness's process
    /// environment, on top of the `SILENT_CRITIC_*` pairs `dispatch` always
    /// supplies ([`Prepared::harness_env`]).
    pub environment: Vec<(String, String)>,
    /// The shell binary `run_check` invokes the rendered command through.
    pub shell: PathBuf,
    /// The bounded timeout for the harness invocation.
    pub timeout: CheckTimeout,
}

// ---------------------------------------------------------------------
// Prepared dispatch (worktree + token + projection + mcp.json, no launch)
// ---------------------------------------------------------------------

/// Everything [`prepare`] produces.
///
/// The paths a caller (the full [`dispatch`] launch, or the manual escape
/// hatch) needs, plus the minted token and the two environments a worker
/// session needs: [`Prepared::harness_env`] for the harness process itself,
/// and [`Prepared::mcp_env`] for the `silent-critic-mcp` child its MCP client
/// spawns (fix round 1: the token, store root, and plan id must never
/// reach the harness's own environment, only `mcp.json`).
#[derive(Debug, Clone)]
pub struct Prepared {
    /// The worktree's path, outside the supervised repository and outside
    /// the plan directory.
    pub worktree_path: PathBuf,
    /// The rendered worker projection's path, outside every worktree and
    /// outside the plan directory.
    pub brief_path: PathBuf,
    /// The written MCP client config's path.
    pub mcp_path: PathBuf,
    /// The token minted for this task. Never logged or printed except by
    /// the manual escape hatch, which is operator-facing output by design.
    pub token: Token,
    /// The environment pairs the harness process itself receives:
    /// `SILENT_CRITIC_BRIEF`, `SILENT_CRITIC_REPO`, `SILENT_CRITIC_MCP_CONFIG`. Carries no
    /// secret and no store-internal path.
    pub harness_env: Vec<(String, String)>,
    /// The environment pairs written into `mcp.json`'s `env` block for the
    /// `silent-critic-mcp` child a harness's MCP client spawns: `SILENT_CRITIC_TOKEN`,
    /// `SILENT_CRITIC_PLAN_ID`, `SILENT_CRITIC_REPO`, `SILENT_CRITIC_STORE_ROOT`,
    /// `SILENT_CRITIC_BRIEF`, `SILENT_CRITIC_TASK_ID`. Never sent to the harness
    /// process itself.
    pub mcp_env: Vec<(String, String)>,
}

/// The branch name a dispatched task's worktree is created on.
fn branch_name(plan_id: &str, task_id: &TaskId) -> String {
    format!("silent-critic/{plan_id}/{}", task_id.as_str())
}

/// The directory holding one dispatched task's worktree and scratch files,
/// under the store root rather than the plan directory (fix round 1;
/// `REPO_INVARIANTS.md` HO-004).
///
/// `pub(crate)` (fix round 2, finding #1): `judge` (`src/tools.rs`)
/// recomputes a task's own worktree path from this function rather than
/// reading a plan-level binding, so two dispatched tasks never collide on
/// which worktree gets judged.
pub(crate) fn run_dir(
    store_root: &Path,
    repo: &RepoIdentity,
    plan_id: &str,
    task_id: &TaskId,
) -> PathBuf {
    store_root
        .join(RUNS_DIR)
        .join(repo.as_str())
        .join(plan_id)
        .join(task_id.as_str())
}

/// Reject a plan or task identifier that is not safe to use as a path
/// component, before it is ever joined onto a filesystem path (fix round 1:
/// `prepare`/`cleanup` take `plan_id`/`task_id` from callers that include
/// unvalidated CLI input, via the manual escape hatch).
///
/// # Errors
///
/// Returns [`DispatchError::UnsafeIdentifier`] when `value` is empty, is
/// `.` or `..`, or contains a path separator.
fn validate_identifier(value: &str) -> Result<(), DispatchError> {
    if is_safe_filename_component(value) {
        Ok(())
    } else {
        Err(DispatchError::UnsafeIdentifier(value.to_owned()))
    }
}

/// Create the worktree, mint the task's token, render the worker
/// projection, and write `mcp.json` -- everything `dispatch` does short of
/// launching a harness.
///
/// `plan_id` is the store's own plan identifier (the same string
/// [`crate::tools::Tools`] is constructed with), not necessarily the
/// document's own front-matter `plan_id`. `plan_source` is the plan's
/// already-read Markdown text (the operator plan, read once by the
/// caller); this function never re-reads it from disk.
///
/// If any step after the worktree is created fails, `prepare` makes a
/// best-effort attempt to remove the worktree and its branch before
/// returning the original error, so a failed `prepare` never leaves a
/// dispatched-looking worktree behind (fix round 1). The rollback itself is
/// best-effort: its own failure is swallowed rather than shadowing the real
/// error the caller needs to see.
///
/// # Errors
///
/// Returns [`DispatchError::UnsafeIdentifier`] when `plan_id` or `task_id`
/// is not safe to use as a path component; otherwise returns
/// [`DispatchError`] when the plan's directory or provenance cannot be
/// resolved, the worktree cannot be created (including a repeat call for a
/// task already dispatched: the branch and path already exist, which is a
/// readable git failure rather than silent corruption), the projection
/// cannot be rendered, the token registry cannot be loaded, minted into, or
/// saved, or any resulting file cannot be written.
pub fn prepare(
    store: &Store,
    repo_start: &Path,
    plan_id: &str,
    task_id: &TaskId,
    plan_source: &str,
    config: &DispatchConfig,
) -> Result<Prepared, DispatchError> {
    validate_identifier(plan_id)?;
    validate_identifier(task_id.as_str())?;

    let plan_dir = store.plan_directory(repo_start, plan_id)?;
    let provenance = store.provenance(repo_start, plan_id)?;
    let run_dir = run_dir(store.root_path(), &provenance.repo, plan_id, task_id);
    let worktree_path = run_dir.join(WORKTREE_DIR_NAME);
    let branch = branch_name(plan_id, task_id);

    worktree::create(
        &config.git,
        repo_start,
        &worktree_path,
        &branch,
        &provenance.base_ref.commit,
    )?;

    let outcome = (|| -> Result<Prepared, DispatchError> {
        let brief_text = tftio_planner::project_worker_markdown(plan_source)?;
        let brief_path = run_dir.join(BRIEF_FILE_NAME);
        fs::write(&brief_path, &brief_text).map_err(io_error(&brief_path))?;

        let tokens_path = plan_dir.join(RUN_DIR).join(TOKENS_FILE_NAME);
        let plan_scope = PlanId::new(plan_id.to_owned());
        let mut registry = load_or_create_registry(&tokens_path, plan_scope.clone())?;
        let worker_role = Role::Worker {
            task: task_id.clone(),
        };
        let token = registry.mint(worker_role, &plan_scope)?;
        registry.save(&tokens_path)?;

        let mcp_path = run_dir.join(MCP_CONFIG_FILE_NAME);
        let mcp_env = vec![
            ("SILENT_CRITIC_TOKEN".to_owned(), token.as_str().to_owned()),
            ("SILENT_CRITIC_PLAN_ID".to_owned(), plan_id.to_owned()),
            (
                "SILENT_CRITIC_REPO".to_owned(),
                worktree_path.to_string_lossy().into_owned(),
            ),
            (
                "SILENT_CRITIC_STORE_ROOT".to_owned(),
                store.root_path().to_string_lossy().into_owned(),
            ),
            (
                "SILENT_CRITIC_BRIEF".to_owned(),
                brief_path.to_string_lossy().into_owned(),
            ),
            // Fix round 2, finding #8: dispatch already knows which task
            // this session is for; writing it here lets `silent-critic-mcp`
            // pass it as `Tools::worker`'s caller-declared `expected_task`
            // directly, rather than deriving it *from* the presented
            // token -- which made that second factor of `Tools::worker`'s
            // check constrain nothing, since the "expected" value and the
            // "actual" value were the same token's own resolution.
            (
                "SILENT_CRITIC_TASK_ID".to_owned(),
                task_id.as_str().to_owned(),
            ),
        ];
        write_mcp_config(&mcp_path, &config.silent_critic_mcp_path, &mcp_env)?;

        let harness_env = vec![
            (
                "SILENT_CRITIC_BRIEF".to_owned(),
                brief_path.to_string_lossy().into_owned(),
            ),
            (
                "SILENT_CRITIC_REPO".to_owned(),
                worktree_path.to_string_lossy().into_owned(),
            ),
            (
                "SILENT_CRITIC_MCP_CONFIG".to_owned(),
                mcp_path.to_string_lossy().into_owned(),
            ),
        ];

        Ok(Prepared {
            worktree_path: worktree_path.clone(),
            brief_path,
            mcp_path,
            token,
            harness_env,
            mcp_env,
        })
    })();

    if outcome.is_err() {
        let _ = worktree::remove(&config.git, repo_start, &worktree_path, &branch);
    }

    outcome
}

/// Remove a task's worktree and branch.
///
/// Never touches the plan itself or its `run/` sidecars (notes,
/// submissions, the token registry, the rendered brief): those are
/// harmless once the worktree is gone, and the operator plan was never
/// there to begin with. Used by tests and the manual escape hatch to avoid
/// leaking worktrees, and internally by [`prepare`] to roll back a partial
/// failure.
///
/// # Errors
///
/// Returns [`DispatchError::UnsafeIdentifier`] when `plan_id` or `task_id`
/// is not safe to use as a path component; otherwise returns
/// [`DispatchError`] when the plan's provenance cannot be resolved or the
/// worktree or branch cannot be removed.
pub fn cleanup(
    store: &Store,
    repo_start: &Path,
    plan_id: &str,
    task_id: &TaskId,
    config: &DispatchConfig,
) -> Result<(), DispatchError> {
    validate_identifier(plan_id)?;
    validate_identifier(task_id.as_str())?;

    let provenance = store.provenance(repo_start, plan_id)?;
    let run_dir = run_dir(store.root_path(), &provenance.repo, plan_id, task_id);
    let worktree_path = run_dir.join(WORKTREE_DIR_NAME);
    let branch = branch_name(plan_id, task_id);
    worktree::remove(&config.git, repo_start, &worktree_path, &branch)?;
    Ok(())
}

// ---------------------------------------------------------------------
// Full dispatch (prepare + launch)
// ---------------------------------------------------------------------

/// The recorded outcome of a dispatch.
///
/// The worktree a worker ran in, the full check result of the harness
/// invocation (with any environment value whose name ends in `_TOKEN`
/// redacted -- see `redact_secrets`), and the tool-authored evidence
/// summarizing it (harness, model, arguments, exit status, wall time --
/// never the token, never the brief content).
#[derive(Debug, Clone)]
pub struct DispatchReport {
    /// The worktree the harness ran in.
    pub worktree_path: PathBuf,
    /// The full result of running the harness through `run_check`, with
    /// its environment's secret-shaped values redacted.
    pub result: CheckResult,
    /// Tool-authored evidence summarizing the invocation.
    pub evidence: Evidence,
}

/// Prepare a task's worktree, token, projection, and MCP config, then
/// launch `harness` against it and return once it exits.
///
/// # Errors
///
/// Returns everything [`prepare`] can return, plus [`DispatchError::Io`]
/// when the rendered brief cannot be re-read for [`PromptVia::Argument`],
/// or [`DispatchError::Check`] when the harness process cannot be spawned
/// or waited on.
pub fn dispatch(
    store: &Store,
    repo_start: &Path,
    plan_id: &str,
    task_id: &TaskId,
    plan_source: &str,
    harness: &HarnessSpec,
) -> Result<DispatchReport, DispatchError> {
    let prepared = prepare(
        store,
        repo_start,
        plan_id,
        task_id,
        plan_source,
        &harness.config,
    )?;

    let mut args = substitute_placeholders(&harness.args, &prepared);
    if let (Some(flag), Some(model)) = (&harness.model_flag, &harness.model) {
        args.push(flag.clone());
        args.push(model.clone());
    }
    if harness.prompt_via == PromptVia::Argument {
        let brief_text =
            fs::read_to_string(&prepared.brief_path).map_err(io_error(&prepared.brief_path))?;
        // The rendered brief begins with the plan's YAML front matter
        // (`---`), which a CLI-style argument parser (the Claude CLI
        // included) would otherwise read as an option rather than
        // positional text. A literal `--` after every configured option --
        // including the model flag just above -- forces every remaining
        // argument, including the brief, to be treated as positional.
        args.push("--".to_owned());
        args.push(brief_text);
    }

    let mut environment = CheckEnvironment::new();
    for (name, value) in &harness.environment {
        environment = environment.with(name.clone(), value.clone());
    }
    for (name, value) in &prepared.harness_env {
        environment = environment.with(name.clone(), value.clone());
    }

    let spec = CheckSpec {
        check: render_shell_command(&harness.program, &args),
        shell: harness.shell.clone(),
        worktree: prepared.worktree_path.clone(),
        working_dir: None,
        environment,
        timeout: harness.timeout,
    };

    let result = automated::run_check(&spec)?;
    let evidence = build_evidence(task_id, harness, &args, &result, &prepared.token)?;
    let result = CheckResult {
        environment: redact_secrets(&result.environment),
        ..result
    };

    Ok(DispatchReport {
        worktree_path: prepared.worktree_path,
        result,
        evidence,
    })
}

fn substitute_placeholders(args: &[String], prepared: &Prepared) -> Vec<String> {
    args.iter()
        .map(|arg| {
            arg.replace("{brief}", &prepared.brief_path.to_string_lossy())
                .replace("{worktree}", &prepared.worktree_path.to_string_lossy())
                .replace("{mcp}", &prepared.mcp_path.to_string_lossy())
        })
        .collect()
}

/// Replace the value of any environment pair whose name ends in `_TOKEN`
/// with `<redacted>`, keeping the name so a stored [`CheckResult`] still
/// shows which variables were set. Applied to every [`CheckResult`] before
/// it is stored or rendered anywhere durable (fix round 1, the critical
/// finding): [`Prepared::harness_env`] no longer carries `SILENT_CRITIC_TOKEN` at
/// all, but a caller-supplied [`HarnessSpec::environment`] pair could still
/// end in `_TOKEN`, and this closes that gap unconditionally rather than
/// trusting every call site that persists a `CheckResult` to remember.
fn redact_secrets(environment: &CheckEnvironment) -> CheckEnvironment {
    let mut redacted = CheckEnvironment::new();
    for (name, value) in environment.pairs() {
        let value = if name.ends_with("_TOKEN") {
            "<redacted>".to_owned()
        } else {
            value.clone()
        };
        redacted = redacted.with(name.to_owned(), value);
    }
    redacted
}

/// Build the tool-authored evidence for one dispatch invocation. Arguments
/// are redacted defensively (the token is never a placeholder substitution
/// target, so it should not appear in `args` at all, but this closes the
/// gap if it ever does); the environment itself is never rendered into this
/// summary at all (see `redact_secrets` for the separate, stored
/// [`CheckResult`]).
fn build_evidence(
    task_id: &TaskId,
    harness: &HarnessSpec,
    args: &[String],
    result: &CheckResult,
    token: &Token,
) -> Result<Evidence, DispatchError> {
    let redacted_args: Vec<String> = args.iter().map(|arg| redact_token(arg, token)).collect();
    let model_display = harness.model.as_deref().unwrap_or("none");
    let summary = format!(
        "dispatch {task}: harness={harness} model={model} arguments={args:?} exit={outcome:?} wall_time={wall:?}",
        task = task_id.as_str(),
        harness = harness.program.display(),
        model = model_display,
        args = redacted_args,
        outcome = result.outcome,
        wall = result.wall_time,
    );
    Ok(Evidence::new(
        EvidenceId::new(Uuid::new_v4().simple().to_string()),
        EvidenceProvenance::ToolAuthored,
        NonEmptyString::new(summary)?,
    ))
}

fn redact_token(value: &str, token: &Token) -> String {
    if value.contains(token.as_str()) {
        value.replace(token.as_str(), "<redacted>")
    } else {
        value.to_owned()
    }
}

fn load_or_create_registry(path: &Path, plan_id: PlanId) -> Result<TokenRegistry, DispatchError> {
    match TokenRegistry::load(path) {
        Ok(registry) => Ok(registry),
        Err(TokenError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(TokenRegistry::new(plan_id))
        }
        Err(other) => Err(other.into()),
    }
}

fn write_mcp_config(
    path: &Path,
    silent_critic_mcp_path: &Path,
    env: &[(String, String)],
) -> Result<(), DispatchError> {
    let mut env_map = serde_json::Map::new();
    for (name, value) in env {
        env_map.insert(name.clone(), serde_json::Value::String(value.clone()));
    }
    // INVARIANT-BYPASS(RS-009): `mcp.json` is the compatibility adapter for
    // an external tool's own config format (a Claude-Code-shaped MCP
    // client), covered by the same bypass `deny.toml` already documents for
    // the future MCP protocol adapter -- never the domain model or
    // canonical storage, which stay on TOML/CBOR.
    let document = json!({
        "mcpServers": {
            "silent-critic": {
                "command": silent_critic_mcp_path.to_string_lossy(),
                "env": serde_json::Value::Object(env_map),
            }
        }
    });
    let body =
        serde_json::to_string_pretty(&document).map_err(DispatchError::SerializeMcpConfig)?;
    // `mcp.json` carries a bearer token in the clear (fix round 1), so it
    // is written through the shared owner-only-permissions helper (fix
    // round 2, finding #7 moved this out of a private copy here and into
    // `crate::secret_file`, now also used by `TokenRegistry::save`).
    crate::secret_file::write_secret_file(path, body.as_bytes()).map_err(io_error(path))
}

/// Build a `map_err` closure that wraps a filesystem `io::Error` with
/// `path`, mirroring `src/store.rs`'s own `io_error`: every I/O call site
/// in this module shares the exact same compiled closure code, so whichever
/// one's error actually fires at test time covers it for all of them.
fn io_error(path: &Path) -> impl Fn(std::io::Error) -> DispatchError + '_ {
    move |source| DispatchError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Quote `value` for safe inclusion in a POSIX shell command line: wrap in
/// single quotes, escaping any embedded single quote as `'\''`. Used to
/// build [`CheckSpec::check`]'s single command string from a harness's
/// separately-typed program and arguments, since `run_check` runs `<shell>
/// -c <check>` rather than taking an argv list directly.
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%,+".contains(c))
    {
        return value.to_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn render_shell_command(program: &Path, args: &[String]) -> String {
    let mut parts = vec![shell_quote(&program.to_string_lossy())];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

/// Failures preparing or launching a dispatch.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// The worktree could not be created or removed.
    #[error(transparent)]
    Worktree(#[from] WorktreeError),
    /// The plan store reported a failure resolving the plan, its
    /// provenance, or recording its worktree.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The token registry could not be loaded, minted into, or saved.
    #[error(transparent)]
    Token(#[from] TokenError),
    /// The plan could not be projected to the worker-safe view.
    #[error(transparent)]
    Projection(#[from] tftio_planner::ProjectionError),
    /// A filesystem operation failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The harness process could not be spawned or waited on.
    #[error(transparent)]
    Check(#[from] AutomatedCheckError),
    /// The MCP client config could not be serialized.
    #[error("serializing mcp.json: {0}")]
    SerializeMcpConfig(serde_json::Error),
    /// The dispatch invocation summary failed the model's non-empty-text
    /// rule (unreachable in practice: the summary is always built from a
    /// non-empty `format!`, but the constructor is still fallible).
    #[error(transparent)]
    Evidence(#[from] EmptyStringError),
    /// A plan or task identifier is not safe to use as a path component
    /// (empty, `.`/`..`, or containing a path separator).
    #[error("identifier {0:?} is not safe to use as a path component")]
    UnsafeIdentifier(String),
}

#[cfg(test)]
mod tests {
    use super::{redact_secrets, redact_token, render_shell_command, shell_quote};
    use crate::evaluate::automated::CheckEnvironment;
    use crate::token::Token;
    use std::path::Path;

    #[test]
    fn shell_quote_leaves_plain_tokens_alone() {
        assert_eq!("plain-value.txt", shell_quote("plain-value.txt"));
    }

    #[test]
    fn shell_quote_escapes_embedded_quotes_and_spaces() {
        assert_eq!("'it'\\''s a test'", shell_quote("it's a test"));
    }

    #[test]
    fn shell_quote_handles_empty_strings() {
        assert_eq!("''", shell_quote(""));
    }

    #[test]
    fn render_shell_command_quotes_program_and_args() {
        let rendered = render_shell_command(
            Path::new("/bin/my harness"),
            &["arg one".to_owned(), "plain".to_owned()],
        );
        assert_eq!("'/bin/my harness' 'arg one' plain", rendered);
    }

    #[test]
    fn redact_token_replaces_only_when_present() {
        let token = Token::new("silent_critic_worker_abc");
        assert_eq!(
            "prefix <redacted> suffix",
            redact_token("prefix silent_critic_worker_abc suffix", &token)
        );
        assert_eq!("no token here", redact_token("no token here", &token));
    }

    #[test]
    fn redact_secrets_masks_only_token_shaped_names() {
        let environment = CheckEnvironment::new()
            .with("SILENT_CRITIC_TOKEN", "silent_critic_worker_abc")
            .with("SILENT_CRITIC_REPO", "/some/worktree")
            .with("MY_CUSTOM_TOKEN", "shh");

        let redacted = redact_secrets(&environment);

        assert_eq!(
            Some(&"<redacted>".to_owned()),
            redacted
                .pairs()
                .iter()
                .find(|(name, _)| name == "SILENT_CRITIC_TOKEN")
                .map(|(_, value)| value)
        );
        assert_eq!(
            Some(&"<redacted>".to_owned()),
            redacted
                .pairs()
                .iter()
                .find(|(name, _)| name == "MY_CUSTOM_TOKEN")
                .map(|(_, value)| value)
        );
        assert_eq!(
            Some(&"/some/worktree".to_owned()),
            redacted
                .pairs()
                .iter()
                .find(|(name, _)| name == "SILENT_CRITIC_REPO")
                .map(|(_, value)| value)
        );
    }
}
