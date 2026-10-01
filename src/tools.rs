//! The agent-facing tool surface, independent of the protocol carrying it.
//!
//! This is the containment boundary. Two caller scopes share one type,
//! [`Tools`], each constructed already authenticated: [`Tools::orchestrator`]
//! needs no token (the MCP session itself is the operator's), and
//! [`Tools::worker`] validates a presented [`crate::token::Token`] against a
//! [`crate::token::TokenRegistry`] and the task the caller claims to be
//! acting as, before any tool call is possible. A token that does not
//! validate, or that validates for a different task than the caller claims,
//! produces a `Tools` value whose every call returns [`ToolOutcome::Failed`]
//! with the same generic message a mismatched task would get — never a
//! constructor error, and never a message that reveals which case applied.
//!
//! **Nothing here knows about MCP.** A tool is a name, a JSON schema, and a
//! function from arguments to text or failure; the wire protocol is a future
//! adapter over [`ToolSurface`], replaceable without touching a handler
//! (`REPO_INVARIANTS.md` ENG-010). That is also what makes the containment
//! boundary testable without speaking a wire protocol: every test here calls
//! [`ToolSurface::call`] directly.
//!
//! The orchestrator is on the worker side of the containment boundary — it
//! authors worker briefs, so anything it can see eventually reaches a worker
//! by paraphrase if not verbatim. `plan_status` and `next_ready` are
//! therefore built from `tftio_planner`'s worker-safe projection
//! (`tftio_planner::WorkerPlan`, via its `From<&OperatorPlan>` conversion),
//! which has no field a hidden criterion could occupy, rather than from the
//! operator plan `tftio_planner::parse_markdown` first produces in memory.
//! `record_decision` and `request_guidance` never echo plan content back.
//! `judge` (T010) is the one orchestrator-scope tool that reasons over hidden
//! criteria internally (via [`crate::evaluate::judge`] and the ledger,
//! [`crate::ledger`]) but returns to its caller only the run-level
//! disposition, visible-criterion judgments, and an operator-attention
//! boolean -- never hidden-criterion text, ids, rationale, or counts
//! (`REPO_INVARIANTS.md` HO-001). `brief` returns
//! `tftio_planner::project_worker_markdown`'s output verbatim: a second
//! implementation of hidden-criterion stripping is a second place for it to
//! be wrong (`REPO_INVARIANTS.md` HO-002/HO-003).
//!
//! Run state — worker notes, submissions, and (once real) dispatch/judge
//! bookkeeping — is a disposable per-run sidecar, not the operator-owned
//! `binding.toml` [`crate::store`] writes: one file per task,
//! `<plan_dir>/run/<task_id>.toml`, alongside `<plan_dir>/run/tokens.toml`
//! ([`crate::token::TokenRegistry`]). One file per task, rather than one
//! shared file for the whole plan, is deliberate: two tasks' worker sessions
//! run as separate processes and must never contend for the same file, and
//! a shared file's read-modify-write would silently drop one task's
//! evidence under concurrent access. Within one task, `note`/`submit` also
//! serialize against an in-process lock keyed by the sidecar path, and every
//! write goes through a temp-file-then-rename in the same directory
//! (mirroring `tftio_planner::write::apply_prepared`'s pattern), so a torn
//! write can never leave a task's sidecar partially written. A future
//! cross-task read (nothing needs one yet) is a directory listing of
//! `run/*.toml`, not a second index to keep in sync. T010 moves whatever of
//! this belongs in the durable review artifact into the plan through
//! `tftio_planner`; nothing here is meant to survive that.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use tftio_planner::model::{
    Evaluator as PlannerEvaluator, FormatVersion, TaskId as PlannerTaskId, TaskStatus,
};
use tftio_planner::{Mutation, MutationRequest, TaskAction};

use crate::dispatch::{self, DispatchError, HarnessSpec};
use crate::evaluate::automated::{
    CheckEnvironment, CheckOutcome, CheckResult, CheckSpec, CheckTimeout, run_check,
};
use crate::evaluate::judge::{
    self, DirectoryRawResponseSink, JudgeCriterion, JudgeInput, JudgeOutcome, RetryBudget,
};
use crate::git::{self, GitInvocation};
use crate::ledger::{
    CriterionAddress, Ledger, criterion_address, hidden_criterion_id, visible_criterion_id,
};
use crate::model::{
    BaseRef, Disposition, EvaluatorKind, Evidence, EvidenceId, EvidenceProvenance, Judgment,
    NonEmptyString, Residual, TaskId, Verdict,
};
use crate::provider::{CommandProvider, CommandProviderSpec};
use crate::store::{Store, StoreError};
use crate::token::{Role, Token, TokenRegistry};

/// One tool the surface offers.
pub struct ToolSpec {
    /// The name an agent calls it by.
    pub name: &'static str,
    /// What it does, as the agent reads it.
    pub description: &'static str,
    /// The JSON Schema its arguments must satisfy.
    pub schema: Value,
}

/// What a tool call produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutcome {
    /// The tool succeeded, with this as its output.
    Text(String),
    /// The tool failed, with this as the reason. Distinct from a transport
    /// error: the call was well-formed and reached the handler, which is a
    /// fact the caller needs in order to decide whether to retry.
    Failed(String),
}

/// A set of tools a caller can call.
///
/// The seam this crate copies from `kb` (`REPO_INVARIANTS.md` ENG-010). A
/// protocol adapter enumerates [`ToolSurface::specs`] and dispatches through
/// [`ToolSurface::call`], and knows nothing else about `silent-critic`.
pub trait ToolSurface {
    /// Every tool this caller may call, with its schema.
    fn specs(&self) -> Vec<ToolSpec>;
    /// Invoke `name` with `arguments`.
    ///
    /// Never fails as a `Result`: a tool that could not do its job, or a
    /// caller not authorized to call it, returns [`ToolOutcome::Failed`],
    /// because an agent needs the reason as content it can read rather than
    /// as a transport-level error it cannot.
    fn call(&self, name: &str, arguments: &Value) -> ToolOutcome;
    /// The server instructions this caller's scope should see (fix round
    /// 2, finding #4).
    ///
    /// Scope-dependent, not one fixed string every session was told
    /// before this fix: a worker session's text says only that the server
    /// carries the task brief and accepts notes/submissions, never that
    /// the plan carries hidden acceptance criteria at all
    /// (`REPO_INVARIANTS.md` HO-001 -- even the *existence* of hidden
    /// criteria must reach only the judge subprocess and the operator, and
    /// telling every worker session that the plan has some is exactly
    /// such a leak). An orchestrator session's text may mention
    /// supervision generally, but not hidden criteria either -- there is
    /// no operator-only transport this instructions string travels over.
    fn instructions(&self) -> &'static str;
}

/// The name for the `run/` sidecar directory under a plan's store directory.
const RUN_DIR: &str = "run";
/// The file extension for one task's run-state sidecar (`run/<task_id>.toml`).
const STATE_FILE_EXTENSION: &str = "toml";
/// The subdirectory (under a task's `run/` sidecar) `judge` persists raw
/// judge responses into, through `DirectoryRawResponseSink`.
const JUDGE_RAW_DIR: &str = "judge-raw";

/// One or two judge providers to run a task's agent-evaluated criteria through.
///
/// `Dual` runs both over the same input and records their disagreement
/// (see `evaluate::judge::judge_twice`).
pub enum JudgeProviders {
    /// A single judge provider.
    Single(CommandProviderSpec),
    /// Two judge providers, run over the same input.
    Dual(CommandProviderSpec, CommandProviderSpec),
}

/// Configuration the `judge` tool needs to run: the provider(s) to consult,
/// the retry budget for a malformed response, and the git environment used
/// to capture facts from the task's dispatched worktree.
///
/// Read once at the process edge and passed through (RS-008) -- this crate
/// never reads `std::env` itself.
pub struct JudgeConfig {
    /// The provider(s) `judge` sends the prompt to.
    pub providers: JudgeProviders,
    /// How many repair retries a malformed judge response gets.
    pub retry_budget: RetryBudget,
    /// The explicit git environment used to capture facts from the
    /// dispatched worktree.
    pub git: GitInvocation,
    /// The shell binary used to run a hidden criterion's automated check.
    pub check_shell: PathBuf,
    /// The explicit environment automated checks run under.
    pub check_environment: CheckEnvironment,
    /// The bounded timeout for one automated check.
    pub check_timeout: CheckTimeout,
}

/// The tool surface over one stored plan, for one already-authenticated
/// caller.
///
/// Construct with [`Tools::orchestrator`] or [`Tools::worker`]; there is no
/// other way to obtain one, so every `Tools` value in a running program has
/// already had its caller resolved (see the module docs on how a mismatched
/// or invalid worker token still produces a value rather than an error).
pub struct Tools<'store> {
    store: &'store Store,
    repo_start: PathBuf,
    plan_id: String,
    caller: Option<Role>,
    /// The harness `dispatch` launches, if one is configured (T009). A
    /// worker's tool surface never needs this: only the orchestrator scope
    /// calls `dispatch`.
    harness: Option<HarnessSpec>,
    /// The judge configuration `judge` uses, if one is configured (T010). A
    /// worker's tool surface never needs this: only the orchestrator scope
    /// calls `judge`.
    judge: Option<JudgeConfig>,
}

impl<'store> Tools<'store> {
    /// The orchestrator's own tool surface: no token, because the MCP
    /// session itself is the operator's.
    ///
    /// `harness` is the configuration `dispatch` launches a worker through
    /// (T009); `None` means `dispatch` fails readably rather than doing
    /// nothing silently. Read once at the process edge and passed through
    /// (RS-008) -- this crate never reads `std::env` itself.
    #[must_use]
    pub const fn orchestrator(
        store: &'store Store,
        repo_start: PathBuf,
        plan_id: String,
        harness: Option<HarnessSpec>,
    ) -> Self {
        Self {
            store,
            repo_start,
            plan_id,
            caller: Some(Role::Orchestrator),
            harness,
            judge: None,
        }
    }

    /// Attach a [`JudgeConfig`] to this (orchestrator-scope) tool surface, so
    /// `judge` can run. Builder-style, to avoid changing
    /// [`Tools::orchestrator`]'s signature for every existing caller that
    /// has no need of `judge` yet.
    #[must_use]
    pub fn with_judge(mut self, judge: JudgeConfig) -> Self {
        self.judge = Some(judge);
        self
    }

    /// A worker's tool surface, authenticated against `registry`.
    ///
    /// `expected_task` is the task this worker session is nominally for
    /// (known independently of the token, e.g. from how the session was
    /// dispatched); `token` is what the caller presented. All of the
    /// following must agree for `token` to authenticate: `registry` must be
    /// scoped to this surface's own `plan_id` (a token minted in a registry
    /// for a different plan is rejected outright, even if its task id
    /// happens to collide with a task id in this plan); `token` must
    /// validate to a [`Role::Worker`]; and its bound task must equal
    /// `expected_task`. Any failure of these produces the same
    /// unauthenticated `Tools` value, whose every call fails identically
    /// (see module docs).
    #[must_use]
    pub fn worker(
        store: &'store Store,
        repo_start: PathBuf,
        plan_id: String,
        registry: &TokenRegistry,
        expected_task: &TaskId,
        token: &Token,
    ) -> Self {
        let caller = if registry.plan_id().as_str() == plan_id {
            match registry.validate(token) {
                Some(Role::Worker { task }) if task == expected_task => {
                    Some(Role::Worker { task: task.clone() })
                }
                _ => None,
            }
        } else {
            None
        };
        Self {
            store,
            repo_start,
            plan_id,
            caller,
            harness: None,
            judge: None,
        }
    }

    /// This surface's authenticated caller, or `None` when authentication
    /// failed (every call then returns [`ToolOutcome::Failed`]).
    #[must_use]
    pub const fn caller(&self) -> Option<&Role> {
        self.caller.as_ref()
    }

    fn plan_path(&self) -> Result<PathBuf, ToolError> {
        Ok(self.store.plan_path(&self.repo_start, &self.plan_id)?)
    }

    fn read_plan_source(&self) -> Result<(PathBuf, String), ToolError> {
        let path = self.plan_path()?;
        let source = std::fs::read_to_string(&path).map_err(io_error(&path))?;
        Ok((path, source))
    }

    /// The sidecar path for one task's run state: `<plan_dir>/run/<task_id>.toml`.
    fn task_run_state_path(&self, task: &TaskId) -> Result<PathBuf, ToolError> {
        if !is_safe_filename_component(task.as_str()) {
            return Err(ToolError::UnsafeTaskId(task.as_str().to_owned()));
        }
        let plan_path = self.plan_path()?;
        // `plan_path` always has a parent (it is at minimum a filename under
        // the store's plan directory), so this fallback is never actually
        // reached; it is eager (`Option::unwrap_or`, a `core`-only call with
        // no project-local branch of its own to cover) rather than a closure
        // passed to `unwrap_or_else`, mirroring `provenance.rs`'s identical
        // choice for the same reason.
        #[allow(clippy::or_fun_call)]
        let plan_dir = plan_path.parent().unwrap_or(Path::new("."));
        Ok(plan_dir
            .join(RUN_DIR)
            .join(task.as_str())
            .with_extension(STATE_FILE_EXTENSION))
    }

    /// Load, mutate, and atomically save one task's run state, serialized
    /// against every other call (in this process) for the same sidecar
    /// path, so concurrent `note`/`submit` calls for the same task can never
    /// interleave a read and a write and drop one caller's evidence.
    /// Not generic over its `mutate` callback's type: every call site's
    /// closure is boxed into the same trait object, so this compiles to one
    /// concrete function rather than one monomorphized copy per call site
    /// (and per binary/test crate that links this module) -- the same class
    /// of coverage-tool false "0% covered" report that motivated making
    /// `Tools::orchestrator`/`Tools::worker`'s `plan_id` a concrete `String`
    /// instead of `impl Into<String>`.
    fn with_task_run_state(
        &self,
        task: &TaskId,
        mutate: Box<dyn FnOnce(&mut TaskRunState) + '_>,
    ) -> Result<(), ToolError> {
        let path = self.task_run_state_path(task)?;
        let lock = run_state_lock(&path);
        let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut state = load_task_run_state(&path)?;
        mutate(&mut state);
        save_task_run_state_atomic(&path, &state)?;
        Ok(())
    }

    // -- orchestrator-scope tools --------------------------------------

    fn plan_status(&self) -> Result<String, ToolError> {
        let (_, source) = self.read_plan_source()?;
        let plan = worker_plan(&source)?;
        let dependents = plan.derived_blocks();
        let tasks: Vec<Value> = plan
            .tasks
            .iter()
            .map(|task| task_view(task, &dependents))
            .collect();
        Ok(json!({ "tasks": tasks }).to_string())
    }

    fn next_ready(&self) -> Result<String, ToolError> {
        let (_, source) = self.read_plan_source()?;
        let plan = worker_plan(&source)?;
        let dependents = plan.derived_blocks();
        let done = done_task_ids(&plan);
        let ready: Vec<Value> = plan
            .tasks
            .iter()
            .filter(|task| is_ready(task, &done))
            .map(|task| task_view(task, &dependents))
            .collect();
        Ok(json!({ "tasks": ready }).to_string())
    }

    fn dispatch(&self, arguments: &Value) -> Result<String, ToolError> {
        let task_id = required_str(arguments, "task_id")?;
        let (_, source) = self.read_plan_source()?;
        let plan = worker_plan(&source)?;
        let done = done_task_ids(&plan);
        let task = plan
            .tasks
            .iter()
            .find(|task| task.id.as_str() == task_id)
            .ok_or_else(|| ToolError::UnknownTask(task_id.to_owned()))?;
        if !is_ready(task, &done) {
            return Err(ToolError::NotReady(task_id.to_owned()));
        }
        let harness = self
            .harness
            .as_ref()
            .ok_or(ToolError::HarnessNotConfigured)?;
        let task = TaskId::new(task_id);
        let report = dispatch::dispatch(
            self.store,
            &self.repo_start,
            &self.plan_id,
            &task,
            &source,
            harness,
        )?;
        let summary = format!(
            "dispatched {task}: worktree={worktree} outcome={outcome:?} wall_time={wall:?}",
            task = task_id,
            worktree = report.worktree_path.display(),
            outcome = report.result.outcome,
            wall = report.result.wall_time,
        );
        self.with_task_run_state(&task, Box::new(move |state| record_dispatch(state, report)))?;
        Ok(summary)
    }

    fn judge(&self, arguments: &Value) -> Result<String, ToolError> {
        let task_id_str = required_str(arguments, "task_id")?;
        let judge_config = self.judge.as_ref().ok_or(ToolError::JudgeNotConfigured)?;
        let (_, source) = self.read_plan_source()?;
        let operator_plan = tftio_planner::parse_markdown(&source)?;
        // A plain loop (not `.find(|task| ...)`) so this predicate is not
        // its own separately-tracked closure: small closures like this one
        // are inlined away entirely, so a coverage tool's function-level
        // metric can report "0 executions" for the closure symbol itself
        // even though the line inside it always runs -- the same class of
        // false report `with_task_run_state`'s boxed-closure change above
        // resolves for a generic function.
        let mut found_task = None;
        for candidate in &operator_plan.tasks {
            if candidate.id.as_str() == task_id_str {
                found_task = Some(candidate);
                break;
            }
        }
        let Some(task) = found_task else {
            return Err(ToolError::UnknownTask(task_id_str.to_owned()));
        };
        let task_id = TaskId::new(task_id_str);

        let provenance = self.store.provenance(&self.repo_start, &self.plan_id)?;
        // The task's own worktree, recomputed deterministically rather than
        // read from a single plan-level binding field: with two tasks
        // dispatched, a plan-level path would name whichever dispatched
        // last, and `judge` would file this task's verdicts under the
        // wrong worktree (fix round 2, finding #1).
        let worktree = dispatch::run_dir(
            self.store.root_path(),
            &provenance.repo,
            &self.plan_id,
            &task_id,
        )
        .join("worktree");
        if !worktree.is_dir() {
            return Err(ToolError::NotDispatched(task_id_str.to_owned()));
        }

        let base_ref = BaseRef::new(provenance.base_ref.commit);
        let facts = git::capture(&worktree, &base_ref, &judge_config.git)?;

        // The uncovered-changed-scope check reads `files.likely_modify`,
        // which planner only ever populates from a v1 (`mode: single` or
        // `mode: multi`) document; a v2 document's `files` is always
        // `None` for every task, structurally indistinguishable from a v1
        // task that simply declared no scope. Running the check unchanged
        // against a v2 plan would therefore silently find nothing for
        // every task -- the same value a v1 task reports once its
        // declared scope is fully covered. So v1 behavior is unchanged
        // below; on a v2 plan the check does not run
        // at all, and `record_scope_check_not_run` (below) records that
        // explicitly -- as an informational note, never as a `Residual` --
        // so "did not run" can never be confused with "ran and passed"
        // (silence) or with an `UncoveredChangedScope` residual that would
        // raise operator attention.
        let scope_check_ran = matches!(
            operator_plan.metadata.format_version,
            FormatVersion::V1 { .. }
        );
        let uncovered_scope = if scope_check_ran {
            // Deliberately not `.map_or_else(Vec::new, |files| ...)`: that
            // reintroduces a small closure whose own "function" entry a
            // coverage tool can report as 0% executed even though the line
            // inside it always runs (see the comment above on `found_task`).
            // `None` (not an empty `Vec`) when the task declares no `files`
            // block at all -- decision 4: "no declared scope" and "declared an
            // empty scope" are different claims, and only the latter should
            // make every changed path uncovered.
            let likely_modify: Option<&[String]> = task
                .files
                .as_ref()
                .map(|files| files.likely_modify.as_slice());
            git::uncovered_changed_scope(task_id.clone(), &facts, likely_modify)
        } else {
            None
        };

        let ledger = Ledger::new(self.store, &self.repo_start, &self.plan_id);
        let checks = run_automated_hidden_checks(&task_id, task, &worktree, judge_config, &ledger)?;
        let criteria = judge_criteria_for_task(task);
        let input = JudgeInput::new(&task_id, &criteria, &facts, &checks);

        let run_dir_root = judge_run_dir_root(&worktree);
        let (disposition, rationale, verdicts, disagreements, outcomes) =
            run_judge_panel(&input, judge_config, &run_dir_root)?;

        let mut visible_judgments = Vec::new();
        let mut attention_required = !matches!(disposition, Disposition::Accept);
        let mut judged_hidden_indices = Vec::new();
        for verdict in &verdicts {
            let hidden_index = route_verdict(
                &ledger,
                task,
                &task_id,
                verdict,
                &mut attention_required,
                &mut visible_judgments,
            )?;
            if let Some(index) = hidden_index {
                judged_hidden_indices.push(index);
            }
        }

        // Decision 2: record judge-provenance evidence for every provider
        // that judged a hidden criterion this run, agreement included, so
        // `measure` can tell a dual panel ran even when it never
        // disagreed. Written after every verdict is routed, so it only
        // ever adds evidence to a hidden criterion whose verdict fields
        // are already recorded.
        record_judge_provenance(&ledger, &task_id, &outcomes)?;

        // The run-level rationale (the judge's own reasoning across every
        // criterion, hidden ones included) never reaches `completion_evidence`
        // (`REPO_INVARIANTS.md` HO-001): it lands as judge-provenance
        // evidence on whichever hidden criteria this run actually judged,
        // or on the task's first hidden criterion if none were (still the
        // only home this material may have, so it stays inside
        // `hidden_criteria` rather than going unrecorded).
        let rationale_recorded = record_run_rationale_as_hidden_evidence(
            &ledger,
            &task_id,
            task,
            &rationale,
            &judged_hidden_indices,
        );
        rationale_recorded?;

        if let Some(residual) = &uncovered_scope {
            ledger.record_residual(&task_id, residual)?;
            attention_required = true;
        } else if !scope_check_ran {
            ledger.record_scope_check_not_run(&task_id)?;
        }
        if !disagreements.is_empty() {
            for disagreement in &disagreements {
                ledger.record_residual(&task_id, disagreement)?;
            }
            attention_required = true;
        }

        // Gate on `verdict.is_none()` exactly as `unresolved_items_in` does,
        // by deriving the flag from `Ledger::unresolved_items` itself (read
        // after every write above), so the two can never diverge: a task
        // with a `human_judgment` hidden criterion needs operator attention
        // only while that criterion has never been judged, not merely
        // because it exists.
        if ledger
            .unresolved_items()?
            .iter()
            .any(|item| item.task_id == task_id)
        {
            attention_required = true;
        }

        summarize_judge_run(&ledger, &task_id, disposition, &visible_judgments)?;

        Ok(json!({
            "disposition": disposition_str(disposition),
            "judgments": visible_judgments,
            "operator_attention_required": attention_required,
            // Distinguishes "the scope check ran" from "it did not run" (a
            // v2 plan) at the call site too, not only in the plan's
            // Operator Guidance Log -- never "passed"/"failed", which
            // would misstate a check that did not execute at all.
            "scope_check": if scope_check_ran { "ran" } else { "not_run" },
        })
        .to_string())
    }

    fn record_decision(&self, arguments: &Value) -> Result<String, ToolError> {
        let title = required_str(arguments, "title")?;
        let body = required_str(arguments, "body")?;
        let entry = format!("**{title}**\n\n{body}");
        let (path, source) = self.read_plan_source()?;
        let request = MutationRequest {
            date: today_utc(),
            mutation: Mutation::AddDecision(entry),
        };
        let prepared = tftio_planner::prepare_markdown_mutation(&source, &request)?;
        tftio_planner::apply_prepared(&path, &prepared)?;
        Ok(format!("recorded decision: {title}"))
    }

    fn request_guidance(&self, arguments: &Value) -> Result<String, ToolError> {
        let question = required_str(arguments, "question")?;
        let affected = required_str_array(arguments, "affected_tasks")?;
        let entry = if affected.is_empty() {
            question.to_owned()
        } else {
            format!("{question}\n\nAffected tasks: {}", affected.join(", "))
        };
        let (path, source) = self.read_plan_source()?;
        let request = MutationRequest {
            date: today_utc(),
            mutation: Mutation::AddGuidance(entry),
        };
        let prepared = tftio_planner::prepare_markdown_mutation(&source, &request)?;
        tftio_planner::apply_prepared(&path, &prepared)?;

        let mut current_source = prepared.replacement().to_owned();
        let mut blocked = Vec::new();
        let mut not_blocked = Vec::new();
        for task_id in &affected {
            let reason = format!("blocked pending operator guidance: {question}");
            let block_prepared = PlannerTaskId::parse(task_id.clone())
                .ok()
                .and_then(|task_id| {
                    let block_request = MutationRequest {
                        date: today_utc(),
                        mutation: Mutation::Task {
                            task_id,
                            action: TaskAction::Block(reason),
                        },
                    };
                    tftio_planner::prepare_markdown_mutation(&current_source, &block_request).ok()
                });
            match block_prepared {
                Some(prepared) => {
                    tftio_planner::apply_prepared(&path, &prepared)?;
                    prepared.replacement().clone_into(&mut current_source);
                    blocked.push(task_id.clone());
                }
                None => not_blocked.push(task_id.clone()),
            }
        }

        let mut summary = "recorded guidance request".to_owned();
        if !blocked.is_empty() {
            let _ = write!(summary, "; blocked tasks: {}", blocked.join(", "));
        }
        if !not_blocked.is_empty() {
            let _ = write!(
                summary,
                "; could not block (not in a blockable state): {}",
                not_blocked.join(", ")
            );
        }
        Ok(summary)
    }

    // -- worker-scope tools ----------------------------------------------

    fn brief(&self) -> Result<String, ToolError> {
        let (_, source) = self.read_plan_source()?;
        Ok(tftio_planner::project_worker_markdown(&source)?)
    }

    fn note(&self, task: &TaskId, arguments: &Value) -> Result<String, ToolError> {
        let text = required_str(arguments, "text")?;
        let evidence = worker_evidence(text)?;
        self.with_task_run_state(task, Box::new(move |state| state.notes.push(evidence)))?;
        Ok("noted".to_owned())
    }

    fn submit(&self, task: &TaskId, arguments: &Value) -> Result<String, ToolError> {
        let summary = required_str(arguments, "summary")?;
        let evidence = worker_evidence(summary)?;
        let mutate = Box::new(move |state: &mut TaskRunState| state.submission = Some(evidence));
        self.with_task_run_state(task, mutate)?;
        Ok("submitted".to_owned())
    }

    fn call_orchestrator(&self, name: &str, arguments: &Value) -> Result<String, ToolError> {
        match name {
            "plan_status" => self.plan_status(),
            "next_ready" => self.next_ready(),
            "dispatch" => self.dispatch(arguments),
            "judge" => self.judge(arguments),
            "record_decision" => self.record_decision(arguments),
            "request_guidance" => self.request_guidance(arguments),
            other => Err(ToolError::NoSuchTool(other.to_owned())),
        }
    }

    fn call_worker(
        &self,
        task: &TaskId,
        name: &str,
        arguments: &Value,
    ) -> Result<String, ToolError> {
        match name {
            "brief" => self.brief(),
            "note" => self.note(task, arguments),
            "submit" => self.submit(task, arguments),
            other => Err(ToolError::NoSuchTool(other.to_owned())),
        }
    }
}

/// Server instructions for an authenticated orchestrator session. Mentions
/// supervision in general terms, never hidden criteria (fix round 2,
/// finding #4).
const ORCHESTRATOR_INSTRUCTIONS: &str = "A supervision server for a planning-doc plan: plan \
     management, dispatching tasks into worker sessions, and judging their results.";

/// Server instructions for an authenticated worker session, or an
/// unauthenticated one (a worker token that failed to resolve, whose
/// specs/calls are already empty/unauthorized): says only that the server
/// carries this session's task brief and accepts notes/submissions.
/// Deliberately silent on supervision, judging, or hidden criteria (fix
/// round 2, finding #4).
const WORKER_INSTRUCTIONS: &str = "A server carrying this task's brief; use its tools to record notes and submit completion \
     evidence.";

impl ToolSurface for Tools<'_> {
    fn specs(&self) -> Vec<ToolSpec> {
        match &self.caller {
            Some(Role::Orchestrator) => orchestrator_specs(),
            Some(Role::Worker { .. }) => worker_specs(),
            None => Vec::new(),
        }
    }

    fn instructions(&self) -> &'static str {
        match &self.caller {
            Some(Role::Orchestrator) => ORCHESTRATOR_INSTRUCTIONS,
            Some(Role::Worker { .. }) | None => WORKER_INSTRUCTIONS,
        }
    }

    fn call(&self, name: &str, arguments: &Value) -> ToolOutcome {
        let outcome = match &self.caller {
            Some(Role::Orchestrator) => self.call_orchestrator(name, arguments),
            Some(Role::Worker { task }) => {
                let task = task.clone();
                self.call_worker(&task, name, arguments)
            }
            None => Err(ToolError::Unauthorized),
        };
        match outcome {
            Ok(text) => ToolOutcome::Text(text),
            Err(err) => ToolOutcome::Failed(err.to_string()),
        }
    }
}

/// Build the full judge-facing criteria list for `task`: its visible
/// `acceptance_checks` (treated as `agent_evaluated`/visible, because
/// `tftio_planner::model::OperatorTask` carries no evaluator field for
/// them) followed by its `hidden_criteria`, in order,
/// addressed through [`visible_criterion_id`]/[`hidden_criterion_id`] so a
/// verdict can be routed back to its source on the way out.
/// Route one judge-rendered `verdict` to its criterion, write it through the
/// ledger, and update `attention_required`/`visible_judgments` in place.
///
/// `criterion_address` returning `None` (an id this module itself never
/// builds) is defended against, not reachable through any verdict this
/// module's own `judge_criteria_for_task` -> `judge_once`/`judge_twice` ->
/// here pipeline can produce; exercised directly by
/// `route_verdict_ignores_an_unaddressable_criterion_id` below rather than
/// by contriving a malformed judge response that would already be rejected
/// by `parse_and_validate` (src/evaluate/judge.rs) before reaching here.
/// Returns the hidden criterion's index when `verdict` addressed one, so a
/// caller can attach judge-run-level evidence to it afterward without
/// re-deriving which criteria this run actually judged (`None` for a
/// visible or unaddressable criterion id).
fn route_verdict(
    ledger: &Ledger<'_>,
    task: &tftio_planner::model::OperatorTask,
    task_id: &TaskId,
    verdict: &Verdict,
    attention_required: &mut bool,
    visible_judgments: &mut Vec<Value>,
) -> Result<Option<usize>, ToolError> {
    let Some(address) = criterion_address(verdict.criterion_id()) else {
        return Ok(None);
    };
    let undetermined = matches!(verdict.judgment(), Judgment::Undetermined { .. });
    match address {
        CriterionAddress::Visible { index } => {
            ledger.record_visible_verdict(task_id, verdict)?;
            *attention_required = *attention_required || undetermined;
            let text = task
                .acceptance_checks
                .get(index)
                .cloned()
                .unwrap_or_default();
            visible_judgments.push(json!({
                "criterion": text,
                "judgment": judgment_str(verdict.judgment()),
                "rationale": verdict.rationale(),
            }));
            Ok(None)
        }
        CriterionAddress::Hidden { index } => {
            // An empty evidence slice, deliberately (fix round 2, finding
            // #3): this call updates only the verdict and rationale, and
            // `Ledger::record_hidden_verdict` treats an empty slice as
            // "preserve whatever evidence this criterion already
            // carries" rather than replacing it with nothing -- a
            // re-judge must not erase a prior run's judge-provenance
            // evidence (a run-level rationale, or a panel disagreement)
            // that `record_run_rationale_as_hidden_evidence` or
            // `Ledger::record_residual` already attached to this same
            // criterion.
            ledger.record_hidden_verdict(task_id, index, verdict, &[])?;
            *attention_required = *attention_required || undetermined;
            Ok(Some(index))
        }
    }
}

fn judge_criteria_for_task(task: &tftio_planner::model::OperatorTask) -> Vec<JudgeCriterion> {
    let mut criteria = Vec::new();
    for (index, check) in task.acceptance_checks.iter().enumerate() {
        if let Ok(claim) = NonEmptyString::new(check.clone()) {
            let ask = NonEmptyString::new(check.clone()).ok();
            criteria.push(JudgeCriterion::new(
                visible_criterion_id(index),
                claim,
                EvaluatorKind::AgentEvaluated,
                ask,
                None,
            ));
        }
    }
    for (index, hidden) in task.hidden_criteria.iter().enumerate() {
        if let Ok(claim) = NonEmptyString::new(hidden.claim.clone()) {
            criteria.push(JudgeCriterion::new(
                hidden_criterion_id(index),
                claim,
                map_evaluator(hidden.evaluator),
                hidden
                    .ask
                    .clone()
                    .and_then(|value| NonEmptyString::new(value).ok()),
                hidden
                    .check
                    .clone()
                    .and_then(|value| NonEmptyString::new(value).ok()),
            ));
        }
    }
    criteria
}

const fn map_evaluator(evaluator: PlannerEvaluator) -> EvaluatorKind {
    match evaluator {
        PlannerEvaluator::Automated => EvaluatorKind::Automated,
        PlannerEvaluator::AgentEvaluated => EvaluatorKind::AgentEvaluated,
        PlannerEvaluator::HumanJudgment => EvaluatorKind::HumanJudgment,
    }
}

/// Run every `automated`-evaluator hidden criterion's check in the
/// dispatched worktree, recording each as tool-authored evidence and its
/// pass/fail as the criterion's verdict through the ledger immediately
/// (automated criteria never go through the judge's own reasoning, so their
/// verdict is settled here, not by `judge_once`/`judge_twice`). Never
/// touches `completion_evidence`: that is worker-visible, and a hidden
/// criterion's outcome recorded there would leak its presence
/// (`REPO_INVARIANTS.md` HO-001).
fn run_automated_hidden_checks(
    task_id: &TaskId,
    task: &tftio_planner::model::OperatorTask,
    worktree: &Path,
    judge_config: &JudgeConfig,
    ledger: &Ledger<'_>,
) -> Result<Vec<CheckResult>, ToolError> {
    let mut results = Vec::new();
    for (index, hidden) in task.hidden_criteria.iter().enumerate() {
        if hidden.evaluator != PlannerEvaluator::Automated {
            continue;
        }
        let outcome = run_one_automated_check(
            task_id,
            index,
            hidden.check.clone(),
            worktree,
            judge_config,
            ledger,
        )?;
        if let Some(result) = outcome {
            results.push(result);
        }
    }
    Ok(results)
}

/// Run one `automated`-evaluator hidden criterion's check (the `index`-th in
/// its task's `hidden_criteria`) and settle its verdict through the ledger.
/// `check_text` is `None` only for a criterion `tftio_planner::validate_markdown`
/// (enforced on the way into the store, `Store::add_plan`) would have
/// rejected -- defended here, not reachable through `Store::add_plan`'s own
/// validated plans, and exercised directly by
/// `run_one_automated_check_is_a_no_op_without_a_check` below rather than by
/// contriving an invalid stored plan.
fn run_one_automated_check(
    task_id: &TaskId,
    index: usize,
    check_text: Option<String>,
    worktree: &Path,
    judge_config: &JudgeConfig,
    ledger: &Ledger<'_>,
) -> Result<Option<CheckResult>, ToolError> {
    let Some(check_text) = check_text else {
        return Ok(None);
    };
    let spec = CheckSpec {
        check: check_text,
        shell: judge_config.check_shell.clone(),
        worktree: worktree.to_path_buf(),
        working_dir: None,
        environment: judge_config.check_environment.clone(),
        timeout: judge_config.check_timeout,
    };
    let result = run_check(&spec)?;
    let judgment = if matches!(result.outcome, CheckOutcome::Passed) {
        Judgment::Pass
    } else {
        Judgment::Fail
    };
    let rationale = NonEmptyString::new(format!("automated check outcome: {:?}", result.outcome))?;
    let evidence_id = EvidenceId::new(Uuid::new_v4().simple().to_string());
    let criterion_id = hidden_criterion_id(index);
    let evidence = result.to_evidence(evidence_id, &criterion_id, task_id)?;
    let verdict = Verdict::new(criterion_id, judgment, rationale);
    let evidence_slice = std::slice::from_ref(&evidence);
    ledger.record_hidden_verdict(task_id, index, &verdict, evidence_slice)?;
    Ok(Some(result))
}

/// The canonical (first) outcome's disposition, rationale, and verdicts,
/// any judge-disagreement residuals from a second provider, and every
/// provider's own [`JudgeOutcome`] (one entry for a single-provider run,
/// two for a dual-provider run) -- the latter is what lets a caller record
/// judge-provenance evidence per provider per judged hidden criterion,
/// agreement included, so a dual panel is visible on the stored plan even
/// when both providers agree (decision 2). [`run_judge_panel`]'s own return
/// type, factored out so its signature stays legible
/// (`clippy::type_complexity`).
type JudgePanelResult = (
    Disposition,
    NonEmptyString,
    Vec<Verdict>,
    Vec<Residual>,
    Vec<JudgeOutcome>,
);

/// Run the configured judge provider(s) over `input`, persisting raw
/// responses under `run_dir_root/judge-raw/` (one subdirectory per provider,
/// so a dual-provider run's same-numbered attempts never collide). See
/// [`JudgePanelResult`] for what this returns.
fn run_judge_panel(
    input: &JudgeInput<'_>,
    judge_config: &JudgeConfig,
    run_dir_root: &Path,
) -> Result<JudgePanelResult, ToolError> {
    match &judge_config.providers {
        JudgeProviders::Single(spec) => {
            let provider =
                CommandProvider::new(crate::provider::ProviderId::new("judge-a"), spec.clone());
            let sink = judge_raw_sink(run_dir_root, "a")?;
            let outcome = judge::judge_once(input, &provider, &sink, judge_config.retry_budget)?;
            Ok((
                outcome.disposition,
                outcome.rationale.clone(),
                outcome.verdicts.clone(),
                Vec::new(),
                vec![outcome],
            ))
        }
        JudgeProviders::Dual(spec_a, spec_b) => {
            let provider_a =
                CommandProvider::new(crate::provider::ProviderId::new("judge-a"), spec_a.clone());
            let provider_b =
                CommandProvider::new(crate::provider::ProviderId::new("judge-b"), spec_b.clone());
            let sink_a = judge_raw_sink(run_dir_root, "a")?;
            let sink_b = judge_raw_sink(run_dir_root, "b")?;
            let panel = judge::judge_twice(
                input,
                &provider_a,
                &sink_a,
                &provider_b,
                &sink_b,
                judge_config.retry_budget,
            )?;
            Ok((
                panel.first.disposition,
                panel.first.rationale.clone(),
                panel.first.verdicts.clone(),
                panel.disagreements,
                vec![panel.first, panel.second],
            ))
        }
    }
}

/// Record judge-provenance evidence for every provider that judged a
/// hidden criterion in this run, agreement or disagreement alike (decision
/// 2). Never touches a verdict, rationale, or `evidence_needed` --
/// [`Ledger::append_hidden_evidence`] only adds to the criterion's evidence
/// list -- so this never disturbs the verdict [`route_verdict`] already
/// recorded from the canonical outcome.
fn record_judge_provenance(
    ledger: &Ledger<'_>,
    task_id: &TaskId,
    outcomes: &[JudgeOutcome],
) -> Result<(), ToolError> {
    for outcome in outcomes {
        for verdict in &outcome.verdicts {
            if let Some(CriterionAddress::Hidden { index }) =
                criterion_address(verdict.criterion_id())
            {
                let evidence = crate::ledger::judge_provenance_evidence(
                    outcome.provider_id.as_str(),
                    verdict,
                )?;
                ledger.append_hidden_evidence(task_id, index, std::slice::from_ref(&evidence))?;
            }
        }
    }
    Ok(())
}

/// Append a compact, deterministic summary of a completed judge run to the
/// task's completion evidence.
/// Append a compact, deterministic, criteria-free summary of a completed
/// judge run to the task's completion evidence: the disposition, plus each
/// visible-criterion judgment (`visible_judgments`, as already built by
/// [`route_verdict`]'s `Visible` branch). Never includes the run-level
/// rationale or anything about a hidden criterion -- `completion_evidence`
/// is worker-visible, and either would leak a hidden criterion's existence
/// or content (`REPO_INVARIANTS.md` HO-001); the run-level rationale's home
/// is [`record_run_rationale_as_hidden_evidence`] instead.
fn summarize_judge_run(
    ledger: &Ledger<'_>,
    task_id: &TaskId,
    disposition: Disposition,
    visible_judgments: &[Value],
) -> Result<(), ToolError> {
    use std::fmt::Write as _;

    let mut text = format!("judge run: disposition={}", disposition_str(disposition));
    for (index, judgment) in visible_judgments.iter().enumerate() {
        let criterion = judgment
            .get("criterion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let verdict = judgment
            .get("judgment")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let judgment_rationale = judgment
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let _ = write!(
            text,
            "; visible[{index}] criterion={criterion:?} judgment={verdict} rationale={judgment_rationale:?}"
        );
    }
    ledger.append_completion_evidence(task_id, &text)?;
    Ok(())
}

/// Attach the judge run's own disposition rationale as judge-provenance
/// evidence on the hidden criteria it actually judged this run
/// (`judged_hidden_indices`), or on the task's first hidden criterion if it
/// judged none -- the only home this material may have, so it stays inside
/// `hidden_criteria` (`REPO_INVARIANTS.md` HO-001) rather than reaching
/// `completion_evidence` or the Operator Guidance Log. A task with no
/// hidden criteria at all has no home for it and none is recorded, which is
/// safe: nothing hidden-criteria-shaped exists to leak in that case.
fn record_run_rationale_as_hidden_evidence(
    ledger: &Ledger<'_>,
    task_id: &TaskId,
    task: &tftio_planner::model::OperatorTask,
    rationale: &NonEmptyString,
    judged_hidden_indices: &[usize],
) -> Result<(), ToolError> {
    let evidence = Evidence::new(
        EvidenceId::new(Uuid::new_v4().simple().to_string()),
        EvidenceProvenance::Judge,
        rationale.clone(),
    );
    if judged_hidden_indices.is_empty() {
        if task.hidden_criteria.is_empty() {
            return Ok(());
        }
        ledger.append_hidden_evidence(task_id, 0, &[evidence])?;
        return Ok(());
    }
    for &index in judged_hidden_indices {
        ledger.append_hidden_evidence(task_id, index, std::slice::from_ref(&evidence))?;
    }
    Ok(())
}

/// The directory raw judge responses are persisted under: the dispatched
/// worktree's own parent directory, or the worktree path itself in the
/// (practically unreachable -- a real dispatched worktree is always nested
/// under the store root) case that it has no parent, defended against here
/// and exercised directly by `judge_run_dir_root_falls_back_without_a_parent`
/// below rather than by contriving a dispatch that produces such a path.
#[allow(clippy::option_if_let_else)]
fn judge_run_dir_root(worktree: &Path) -> PathBuf {
    match worktree.parent() {
        Some(parent) => parent.to_path_buf(),
        None => worktree.to_path_buf(),
    }
}

fn judge_raw_sink(run_dir_root: &Path, label: &str) -> Result<DirectoryRawResponseSink, ToolError> {
    let dir = run_dir_root.join(JUDGE_RAW_DIR).join(label);
    std::fs::create_dir_all(&dir).map_err(io_error(&dir))?;
    Ok(DirectoryRawResponseSink::new(dir))
}

const fn judgment_str(judgment: &Judgment) -> &'static str {
    match judgment {
        Judgment::Pass => "pass",
        Judgment::Fail => "fail",
        Judgment::Undetermined { .. } => "undetermined",
    }
}

const fn disposition_str(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Accept => "accept",
        Disposition::Reject => "reject",
        Disposition::NeedsOperator => "needs_operator",
    }
}

fn worker_evidence(text: &str) -> Result<Evidence, ToolError> {
    Ok(Evidence::new(
        EvidenceId::new(Uuid::new_v4().simple().to_string()),
        EvidenceProvenance::WorkerNarrated,
        NonEmptyString::new(text)?,
    ))
}

/// Record a completed `dispatch` invocation's evidence and full check
/// result into `state`, alongside each other (T009).
fn record_dispatch(state: &mut TaskRunState, report: dispatch::DispatchReport) {
    state.dispatch = Some(report.evidence);
    state.dispatch_result = Some(report.result);
}

/// One task's disposable run-state: notes and a submission, both
/// worker-narrated evidence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TaskRunState {
    #[serde(default)]
    notes: Vec<Evidence>,
    #[serde(default)]
    submission: Option<Evidence>,
    /// Tool-authored evidence recording the most recent `dispatch`
    /// invocation for this task (T009): harness, model, arguments, exit
    /// status, wall time. `None` until the task has been dispatched.
    #[serde(default)]
    dispatch: Option<Evidence>,
    /// The full check result of the most recent `dispatch` invocation,
    /// alongside `dispatch`'s summarized `Evidence`: outcome, captured
    /// stdout/stderr, and wall time, verbatim.
    #[serde(default)]
    dispatch_result: Option<CheckResult>,
}

/// Whether `component` is safe to use as a single filesystem path
/// component (a task id, here): non-empty, contains no path separator, and
/// is not `.` or `..`. Task ids reach [`Tools::task_run_state_path`] from a
/// token's own binding or an orchestrator-supplied string; this is a
/// defensive floor against a task id ever being used to escape the `run/`
/// directory, not a validation of the planning-document task id shape
/// (`tftio_planner` owns that).
pub(crate) fn is_safe_filename_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.contains('/')
        && !component.contains('\\')
}

/// Per-path locks serializing this process's `note`/`submit` calls for the
/// same task's run-state sidecar. Keyed by the resolved file path (not the
/// task id alone) so two different plans' same-named task never share a
/// lock, and vice versa.
static RUN_STATE_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn run_state_lock(path: &Path) -> Arc<Mutex<()>> {
    let table = RUN_STATE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut table = table.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        table
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

fn load_task_run_state(path: &Path) -> Result<TaskRunState, ToolError> {
    match std::fs::read_to_string(path) {
        Ok(body) => toml::from_str(&body).map_err(|source| ToolError::RunStateParse {
            path: path.to_path_buf(),
            source,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(TaskRunState::default()),
        Err(source) => Err(ToolError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Write `state` to `path` by writing a temporary file in the same
/// directory and renaming it into place, so a task's sidecar is never
/// observed half-written and two tasks (different `path`s) never contend
/// for the same file.
fn save_task_run_state_atomic(path: &Path, state: &TaskRunState) -> Result<(), ToolError> {
    // `path` is always `task_run_state_path`'s own output, which always has
    // a parent; eager fallback for the same reason as that function's own
    // (see its comment).
    #[allow(clippy::or_fun_call)]
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io_error(parent))?;
    // `TaskRunState`'s only content is `Evidence`, built entirely from plain
    // UTF-8 strings and closed enums of them (see `TokenRegistry::save`'s
    // matching comment for why TOML serialization of such a value has no
    // fallible case this module expects to reach).
    let body = toml::to_string_pretty(state).unwrap_or_default();
    let temp_path = parent.join(format!(
        ".{}.silent-critic-{}.tmp",
        path.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("state.toml"),
        Uuid::new_v4().simple()
    ));
    std::fs::write(&temp_path, body).map_err(io_error(&temp_path))?;
    std::fs::rename(&temp_path, path).map_err(io_error(path))
}

/// Parse the stored operator plan and convert it to `tftio_planner`'s
/// worker-safe projection, which structurally has no field a hidden
/// criterion could occupy.
fn worker_plan(source: &str) -> Result<tftio_planner::WorkerPlan, ToolError> {
    let operator_plan = tftio_planner::parse_markdown(source)?;
    Ok(tftio_planner::WorkerPlan::from(&operator_plan))
}

fn done_task_ids(plan: &tftio_planner::WorkerPlan) -> BTreeSet<&str> {
    plan.tasks
        .iter()
        .filter(|task| matches!(task.status, TaskStatus::Done))
        .map(|task| task.id.as_str())
        .collect()
}

fn is_ready(task: &tftio_planner::model::WorkerTask, done: &BTreeSet<&str>) -> bool {
    matches!(task.status, TaskStatus::NotStarted | TaskStatus::Ready)
        && task
            .depends_on
            .iter()
            .all(|dependency| done.contains(dependency.as_str()))
}

/// Render one worker-safe task, with `blocks` reported from `dependents`
/// (the plan's [`tftio_planner::WorkerPlan::derived_blocks`] map: each
/// task's downstream set, computed as the exact inverse of `depends_on`)
/// rather than from `task.blocks`. `task.blocks` carries only what a v1
/// document happened to author and is always empty for a v2 document
/// (planner 0.4 never populates it from `depends_on`); deriving dependents
/// uniformly means `plan_status`/`next_ready` report the same thing for
/// both format versions instead of quietly going empty on v2.
fn task_view(
    task: &tftio_planner::model::WorkerTask,
    dependents: &BTreeMap<PlannerTaskId, Vec<PlannerTaskId>>,
) -> Value {
    let blocks: &[PlannerTaskId] = dependents
        .get(&task.id)
        .map_or(&[], |downstream| downstream.as_slice());
    json!({
        "id": task.id.as_str(),
        "title": task.title,
        "status": task_status_str(task.status),
        "depends_on": task.depends_on.iter().map(PlannerTaskId::as_str).collect::<Vec<_>>(),
        "blocks": blocks.iter().map(PlannerTaskId::as_str).collect::<Vec<_>>(),
    })
}

const fn task_status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::NotStarted => "not_started",
        TaskStatus::Ready => "ready",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        TaskStatus::Abandoned => "abandoned",
    }
}

/// The current UTC date, in `YYYY-MM-DD` form, for stamping plan mutations.
///
/// A hand-rolled civil-calendar conversion rather than a new dependency: the
/// brief adds only `uuid` beyond what this crate already depends on, and
/// this crate's pure domain core (`src/model.rs`) already bans clock reads
/// outright — this lives in the tool surface's imperative shell instead,
/// alongside its other I/O.
fn today_utc() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a
/// proleptic-Gregorian `(year, month, day)`, kept entirely in `i64`
/// arithmetic (`div_euclid`/`rem_euclid`) so it needs no signed/unsigned
/// casts.
const fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

fn required_str<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::MissingArgument(name.to_owned()))
}

fn required_str_array(arguments: &Value, name: &str) -> Result<Vec<String>, ToolError> {
    arguments
        .get(name)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .ok_or_else(|| ToolError::MissingArgument(name.to_owned()))
}

fn io_error(path: &Path) -> impl Fn(std::io::Error) -> ToolError + '_ {
    move |source| ToolError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// A tool handler's internal failure, always rendered through its `Display`
/// (never `Debug`) into a [`ToolOutcome::Failed`] message — `Display` is
/// what every variant below keeps free of hidden-criterion content, per the
/// module docs.
#[derive(Debug, Error)]
enum ToolError {
    /// No tool with this name exists for the caller's scope.
    #[error("no such tool: {0}")]
    NoSuchTool(String),
    /// A required argument was missing, not a string, or blank.
    #[error("missing or invalid argument: {0}")]
    MissingArgument(String),
    /// Authentication failed: an invalid token, a token for a different
    /// role, or a token for a different task than the caller claims.
    /// Deliberately uninformative: it must not reveal which of those
    /// applied, nor whether the claimed task even exists.
    #[error("token rejected")]
    Unauthorized,
    /// The plan store reported a failure resolving or reading the plan.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A filesystem operation on the run-state sidecar failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The run-state sidecar could not be parsed.
    #[error("run state {path} is malformed: {source}")]
    RunStateParse {
        /// The sidecar path that failed to parse.
        path: PathBuf,
        /// The underlying TOML deserialization error.
        source: toml::de::Error,
    },
    /// The stored plan could not be parsed.
    #[error(transparent)]
    Parse(#[from] tftio_planner::ParseError),
    /// The stored plan could not be projected to the worker view.
    #[error(transparent)]
    Projection(#[from] tftio_planner::ProjectionError),
    /// A requested mutation could not be prepared.
    #[error(transparent)]
    Mutation(#[from] tftio_planner::MutationError),
    /// A prepared mutation could not be written back to the store.
    #[error(transparent)]
    Write(#[from] tftio_planner::WriteError),
    /// No task with the given identifier exists in the stored plan.
    #[error("no such task: {0}")]
    UnknownTask(String),
    /// A task id could not be used as a run-state sidecar filename.
    #[error("task id {0:?} cannot be used as a run-state filename")]
    UnsafeTaskId(String),
    /// The given task is not ready to dispatch.
    #[error("task {0} is not ready: its dependencies are not all done")]
    NotReady(String),
    /// Worker-narrated text failed the model's non-empty-text rule.
    #[error(transparent)]
    EmptyEvidence(#[from] crate::model::EmptyStringError),
    /// `dispatch` was called with no harness configured for this `Tools`.
    #[error("no harness is configured for this server: dispatch cannot launch a worker")]
    HarnessNotConfigured,
    /// A dispatch invocation (worktree creation, token minting, projection
    /// rendering, or the harness launch itself) failed.
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
    /// `judge` was called with no [`JudgeConfig`] configured for this
    /// `Tools`.
    #[error("no judge is configured for this server: judge cannot render a verdict")]
    JudgeNotConfigured,
    /// `judge` was called for a task with no recorded worktree: it has not
    /// been dispatched.
    #[error("task {0} has not been dispatched: judge has no worktree to inspect")]
    NotDispatched(String),
    /// Capturing git facts from the dispatched worktree failed.
    #[error(transparent)]
    Git(#[from] git::GitError),
    /// Running a hidden criterion's automated check failed to even start.
    #[error(transparent)]
    Automated(#[from] crate::evaluate::automated::AutomatedCheckError),
    /// Running the judge to completion failed.
    #[error(transparent)]
    Judge(#[from] judge::JudgeError),
    /// A ledger write or read failed.
    #[error(transparent)]
    Ledger(#[from] crate::ledger::LedgerError),
}

fn orchestrator_specs() -> Vec<ToolSpec> {
    let no_args = json!({ "type": "object", "properties": {}, "required": [] });
    let task_id_arg = json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "The task's identifier, as it appears in the task graph.",
            },
        },
        "required": ["task_id"],
    });
    vec![
        ToolSpec {
            name: "plan_status",
            description: "Every task in the plan's task graph: id, title, status, \
                dependencies, and what it blocks. Built from the worker-safe \
                projection, so it never contains hidden acceptance criteria.",
            schema: no_args.clone(),
        },
        ToolSpec {
            name: "next_ready",
            description: "Tasks whose dependencies are all done and which are ready \
                to start.",
            schema: no_args,
        },
        ToolSpec {
            name: "dispatch",
            description: "Dispatch a ready task to a worker: create its worktree, \
                mint a task-scoped token, render its worker projection, and launch \
                the configured harness, returning a summary once it exits. Fails \
                readably if no harness is configured, the task does not exist, or \
                is not ready.",
            schema: task_id_arg.clone(),
        },
        ToolSpec {
            name: "judge",
            description: "Render a verdict for a task's submitted work: captures git \
                facts and runs automated checks in the dispatched worktree, sends the \
                agent-evaluated criteria to the configured judge provider(s), writes \
                every verdict and residual into the plan, and returns the run-level \
                disposition, the visible-criterion judgments, and whether operator \
                attention is required. Never returns hidden-criterion content. Fails \
                readably if no judge is configured, the task does not exist, or the \
                task has not been dispatched.",
            schema: task_id_arg,
        },
        ToolSpec {
            name: "record_decision",
            description: "Append an entry to the plan's Decision Log.",
            schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "A short decision title." },
                    "body": { "type": "string", "description": "The decision's full text." },
                },
                "required": ["title", "body"],
            }),
        },
        ToolSpec {
            name: "request_guidance",
            description: "Append an entry to the plan's Operator Guidance Log, and \
                block the named tasks pending an answer where their current state \
                allows blocking (a task not yet started or in progress cannot be \
                blocked; when that happens the response says so instead).",
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question needing an operator answer.",
                    },
                    "affected_tasks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Task ids to block pending the answer, if any.",
                    },
                },
                "required": ["question", "affected_tasks"],
            }),
        },
    ]
}

fn worker_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "brief",
            description: "The plan's worker-safe projection, verbatim: every visible \
                task, with no hidden acceptance criteria.",
            schema: json!({ "type": "object", "properties": {}, "required": [] }),
        },
        ToolSpec {
            name: "note",
            description: "Record a worker-narrated observation about the bound task.",
            schema: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "What to note." },
                },
                "required": ["text"],
            }),
        },
        ToolSpec {
            name: "submit",
            description: "Record the bound task as submitted, with a worker-narrated \
                summary of the work.",
            schema: json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "A summary of the completed work.",
                    },
                },
                "required": ["summary"],
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{HarnessSpec, JudgeConfig, JudgeProviders, ToolError, ToolSurface, Tools};
    use crate::evaluate::automated::{CheckEnvironment, CheckTimeout};
    use crate::evaluate::judge::RetryBudget;
    use crate::git::GitInvocation;
    use crate::model::{CriterionId, NonEmptyString, PlanId, TaskId, Verdict};
    use crate::provider::{CommandEnvironment, CommandProviderSpec, PromptVia};
    use crate::store::{Store, StoreRoot};
    use crate::token::{Role, TokenRegistry};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;

    // The acceptance-test suite (sentinel leak, gating, brief byte-identity,
    // note/submit, record_decision/request_guidance) lives in
    // `tests/tools.rs`, not here: an integration test compiles against the
    // plain (non-`--cfg test`) library artifact, the same one a consuming
    // binary would use, so the containment boundary is proven on the
    // artifact that matters rather than only on this crate's own unit-test
    // build. What remains here tests private helpers `tests/tools.rs`
    // cannot reach at all -- plus, below,
    // `every_orchestrator_and_worker_tool_executes_once_in_this_build`,
    // which exists purely to give *this* crate's own `--cfg test` compiled
    // copy of every public method here a real execution too (see
    // `crate::test_support`'s module docs for why that copy otherwise stays
    // at zero even though `tests/tools.rs` exhaustively covers the same
    // code in the crate's other compiled copy).

    #[test]
    fn every_orchestrator_and_worker_tool_executes_once_in_this_build()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = crate::test_support::set_up(crate::test_support::SENTINEL_PLAN)?;

        let orchestrator = Tools::orchestrator(
            &fixture.store,
            fixture.repo_start.clone(),
            fixture.plan_id.clone(),
            None,
        );
        let _ = orchestrator.caller();
        let _ = orchestrator.specs();
        let _ = orchestrator.call("plan_status", &json!({}));
        let _ = orchestrator.call("next_ready", &json!({}));
        // No harness/judge configured: both fail readably rather than
        // doing nothing, which is itself the behavior under test here.
        let _ = orchestrator.call("dispatch", &json!({ "task_id": "T002" }));
        let _ = orchestrator.call("judge", &json!({ "task_id": "T002" }));
        let _ = orchestrator.call(
            "record_decision",
            &json!({ "title": "a decision", "body": "recorded from the lib test build" }),
        );
        let _ = orchestrator.call(
            "request_guidance",
            &json!({ "question": "a question", "affected_tasks": [] }),
        );
        let _ = orchestrator.call("no_such_tool", &json!({}));

        let task = TaskId::new("T002");
        let plan_scope = PlanId::new(fixture.plan_id.clone());
        let mut registry = TokenRegistry::new(plan_scope.clone());
        let token = registry.mint(Role::Worker { task: task.clone() }, &plan_scope)?;
        let worker = Tools::worker(
            &fixture.store,
            fixture.repo_start.clone(),
            fixture.plan_id.clone(),
            &registry,
            &task,
            &token,
        );
        let _ = worker.caller();
        let _ = worker.specs();
        let _ = worker.call("brief", &json!({}));
        let _ = worker.call("note", &json!({ "text": "a note from the lib test build" }));
        let _ = worker.call(
            "submit",
            &json!({ "summary": "submitted from the lib test build" }),
        );

        Ok(())
    }

    /// Exercises the dual-provider `judge` path -- including
    /// [`record_judge_provenance`] -- from this crate's own `--cfg test`
    /// compiled copy, not only from `tests/tools.rs`'s separately compiled
    /// integration-test copy: without this, `record_judge_provenance`'s
    /// loop body never runs in *this* copy, and shows as an uncovered line
    /// in the coverage gate even though `tests/tools.rs`'s
    /// `judge_records_dual_panel_evidence_when_both_providers_agree_and_measure_reports_zero_disagreements`
    /// already exercises the exact same code in the other copy (see
    /// `crate::test_support`'s module docs for why the two copies are
    /// counted separately).
    #[test]
    fn dual_provider_judge_records_provenance_in_this_build()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = crate::test_support::set_up(crate::test_support::SENTINEL_PLAN)?;

        let harness = HarnessSpec {
            config: crate::dispatch::DispatchConfig {
                git: crate::worktree::GitEnv::new(
                    "git".to_owned(),
                    test_path(),
                    std::env::temp_dir().to_string_lossy().into_owned(),
                ),
                silent_critic_mcp_path: PathBuf::from("silent-critic-mcp"),
            },
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".to_owned(),
                "echo did-the-sensitive-thing > sensitive.txt".to_owned(),
            ],
            model: None,
            model_flag: None,
            prompt_via: crate::dispatch::PromptVia::BriefPath,
            environment: Vec::new(),
            shell: PathBuf::from("/bin/sh"),
            timeout: CheckTimeout::new(Duration::from_secs(10)),
        };
        let dispatching = Tools::orchestrator(
            &fixture.store,
            fixture.repo_start.clone(),
            fixture.plan_id.clone(),
            Some(harness),
        );
        let _ = dispatching.call("dispatch", &json!({ "task_id": "T002" }));

        let response = r#"{"judgments":[{"criterion_id":"visible-0","judgment":"pass","rationale":"fine"},{"criterion_id":"hidden-0","judgment":"pass","rationale":"fine"}],"disposition":"accept","rationale":"clean"}"#;
        let provider_spec = || CommandProviderSpec {
            program: "/bin/sh".into(),
            // Drain stdin before answering, so the prompt write never races the
            // child's exit.
            args: vec![
                "-c".to_owned(),
                format!("cat >/dev/null; printf '%s' '{response}'"),
            ],
            model_flag: None,
            model: None,
            prompt_via: PromptVia::Stdin,
            environment: CommandEnvironment::default(),
            timeout: Duration::from_secs(10),
        };
        let judge_config = JudgeConfig {
            providers: JudgeProviders::Dual(provider_spec(), provider_spec()),
            retry_budget: RetryBudget::default(),
            git: test_git_invocation(),
            check_shell: PathBuf::from("/bin/sh"),
            check_environment: CheckEnvironment::default(),
            check_timeout: CheckTimeout::new(Duration::from_secs(10)),
        };
        let judging = Tools::orchestrator(
            &fixture.store,
            fixture.repo_start.clone(),
            fixture.plan_id.clone(),
            None,
        )
        .with_judge(judge_config);
        let _ = judging.call("judge", &json!({ "task_id": "T002" }));

        Ok(())
    }

    fn test_path() -> String {
        #[allow(
            clippy::disallowed_methods,
            reason = "test harness stands in for the caller-side process edge; the library under test never reads PATH itself"
        )]
        std::env::var("PATH").unwrap_or_default()
    }

    fn test_git_invocation() -> GitInvocation {
        GitInvocation::new(
            "git".to_owned(),
            test_path(),
            std::env::temp_dir().to_string_lossy().into_owned(),
        )
    }

    fn dummy_operator_task()
    -> Result<tftio_planner::model::OperatorTask, Box<dyn std::error::Error>> {
        Ok(tftio_planner::model::OperatorTask {
            id: tftio_planner::model::TaskId::parse("T001".to_owned())?,
            title: String::new(),
            status: tftio_planner::model::TaskStatus::NotStarted,
            owner: None,
            depends_on: Vec::new(),
            blocks: Vec::new(),
            description: String::new(),
            work_items: Vec::new(),
            invariants: Vec::new(),
            acceptance_checks: Vec::new(),
            hidden_criteria: Vec::new(),
            files: None,
            completion_evidence: None,
        })
    }

    fn dummy_ledger_parts() -> (Store, PathBuf, String) {
        (
            Store::new(StoreRoot::new(PathBuf::from("/nonexistent-store"))),
            PathBuf::from("/nonexistent-repo"),
            "PLAN-1".to_owned(),
        )
    }

    fn dummy_judge_config() -> JudgeConfig {
        JudgeConfig {
            providers: JudgeProviders::Single(CommandProviderSpec {
                program: "/bin/false".into(),
                args: Vec::new(),
                model_flag: None,
                model: None,
                prompt_via: PromptVia::Stdin,
                environment: CommandEnvironment::default(),
                timeout: Duration::from_secs(1),
            }),
            retry_budget: RetryBudget::default(),
            git: GitInvocation::new(String::new(), String::new(), String::new()),
            check_shell: PathBuf::from("/bin/sh"),
            check_environment: CheckEnvironment::default(),
            check_timeout: CheckTimeout::new(Duration::from_secs(1)),
        }
    }

    #[test]
    fn route_verdict_ignores_an_unaddressable_criterion_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let (store, repo_start, plan_id) = dummy_ledger_parts();
        let ledger = super::Ledger::new(&store, &repo_start, &plan_id);
        let task = dummy_operator_task()?;
        let task_id = TaskId::new("T001");
        let verdict = Verdict::new(
            CriterionId::new("not-an-address"),
            crate::model::Judgment::Pass,
            NonEmptyString::new("irrelevant")?,
        );
        let mut attention_required = false;
        let mut visible_judgments = Vec::new();

        let result = super::route_verdict(
            &ledger,
            &task,
            &task_id,
            &verdict,
            &mut attention_required,
            &mut visible_judgments,
        );

        assert!(result.is_ok());
        assert!(!attention_required);
        assert!(visible_judgments.is_empty());
        Ok(())
    }

    #[test]
    fn run_one_automated_check_is_a_no_op_without_a_check() {
        let (store, repo_start, plan_id) = dummy_ledger_parts();
        let ledger = super::Ledger::new(&store, &repo_start, &plan_id);
        let judge_config = dummy_judge_config();
        let task_id = TaskId::new("T001");

        let outcome = super::run_one_automated_check(
            &task_id,
            0,
            None,
            std::path::Path::new("/nonexistent-worktree"),
            &judge_config,
            &ledger,
        );

        assert!(matches!(outcome, Ok(None)));
    }

    #[test]
    fn judge_run_dir_root_falls_back_without_a_parent() {
        // The root path has no parent -- the practically-unreachable case
        // this function defends against.
        assert_eq!(
            std::path::PathBuf::from("/"),
            super::judge_run_dir_root(std::path::Path::new("/"))
        );
        // An ordinary nested path resolves to its parent, as every real
        // dispatched worktree does.
        assert_eq!(
            std::path::PathBuf::from("/worktree"),
            super::judge_run_dir_root(std::path::Path::new("/worktree/child"))
        );
    }

    #[test]
    fn judge_criteria_for_task_skips_blank_claims() -> Result<(), Box<dyn std::error::Error>> {
        // Both branches are defended against a document that reached this
        // code with a blank claim/check text -- `tftio_planner::validate_markdown`
        // (enforced on the way into the store) requires both non-empty, so
        // neither branch is reachable through a stored, validated plan;
        // exercised directly here rather than by contriving an invalid one.
        let mut task = dummy_operator_task()?;
        task.acceptance_checks.push("   ".to_owned());
        task.hidden_criteria
            .push(tftio_planner::model::HiddenCriterion {
                claim: "   ".to_owned(),
                criticality: tftio_planner::model::Criticality::Must,
                evaluator: tftio_planner::model::Evaluator::HumanJudgment,
                check: None,
                ask: Some("a question".to_owned()),
                why_hidden: "because".to_owned(),
                counterfactual: "gaming".to_owned(),
                verdict: None,
                rationale: None,
                evidence_needed: None,
                evidence: Vec::new(),
            });

        let criteria = super::judge_criteria_for_task(&task);

        assert!(criteria.is_empty());
        Ok(())
    }

    #[test]
    fn route_verdict_propagates_a_ledger_write_failure() -> Result<(), Box<dyn std::error::Error>> {
        // `dummy_ledger_parts` points at a repository/store that does not
        // exist, so any ledger write this reaches fails readably; this
        // exercises `route_verdict`'s own error-propagation path with a
        // real (if deliberately broken) `Ledger`, not a contrived mock.
        let (store, repo_start, plan_id) = dummy_ledger_parts();
        let ledger = super::Ledger::new(&store, &repo_start, &plan_id);
        let task = dummy_operator_task()?;
        let task_id = TaskId::new("T001");
        let verdict = Verdict::new(
            super::hidden_criterion_id(0),
            crate::model::Judgment::Pass,
            NonEmptyString::new("irrelevant")?,
        );
        let mut attention_required = false;
        let mut visible_judgments = Vec::new();

        let result = super::route_verdict(
            &ledger,
            &task,
            &task_id,
            &verdict,
            &mut attention_required,
            &mut visible_judgments,
        );

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn run_one_automated_check_propagates_a_ledger_write_failure() {
        let (store, repo_start, plan_id) = dummy_ledger_parts();
        let ledger = super::Ledger::new(&store, &repo_start, &plan_id);
        let judge_config = dummy_judge_config();
        let task_id = TaskId::new("T001");

        // A real, successful check ("true" always exits 0) run in a real
        // directory, so the failure this test proves comes from the
        // broken `Ledger` at the end, not from `run_check` itself.
        let result = super::run_one_automated_check(
            &task_id,
            0,
            Some("true".to_owned()),
            std::path::Path::new("/tmp"),
            &judge_config,
            &ledger,
        );

        assert!(result.is_err());
    }

    #[test]
    fn run_automated_hidden_checks_propagates_a_ledger_write_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let (store, repo_start, plan_id) = dummy_ledger_parts();
        let ledger = super::Ledger::new(&store, &repo_start, &plan_id);
        let judge_config = dummy_judge_config();
        let task_id = TaskId::new("T001");
        let mut task = dummy_operator_task()?;
        task.hidden_criteria
            .push(tftio_planner::model::HiddenCriterion {
                claim: "a claim".to_owned(),
                criticality: tftio_planner::model::Criticality::Must,
                evaluator: tftio_planner::model::Evaluator::Automated,
                check: Some("true".to_owned()),
                ask: None,
                why_hidden: "because".to_owned(),
                counterfactual: "gaming".to_owned(),
                verdict: None,
                rationale: None,
                evidence_needed: None,
                evidence: Vec::new(),
            });

        let result = super::run_automated_hidden_checks(
            &task_id,
            &task,
            std::path::Path::new("/tmp"),
            &judge_config,
            &ledger,
        );

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn summarize_judge_run_propagates_a_ledger_write_failure() {
        let (store, repo_start, plan_id) = dummy_ledger_parts();
        let ledger = super::Ledger::new(&store, &repo_start, &plan_id);
        let task_id = TaskId::new("T001");
        let visible_judgments = vec![json!({
            "criterion": "a visible criterion",
            "judgment": "pass",
            "rationale": "fine",
        })];

        let result = super::summarize_judge_run(
            &ledger,
            &task_id,
            crate::model::Disposition::Accept,
            &visible_judgments,
        );

        assert!(result.is_err());
    }

    #[test]
    fn record_run_rationale_as_hidden_evidence_is_a_no_op_without_hidden_criteria()
    -> Result<(), Box<dyn std::error::Error>> {
        let (store, repo_start, plan_id) = dummy_ledger_parts();
        let ledger = super::Ledger::new(&store, &repo_start, &plan_id);
        let task_id = TaskId::new("T001");
        let task = dummy_operator_task()?;
        let rationale = NonEmptyString::new("nothing hidden to attach this to")?;

        // A broken `Ledger` (see `dummy_ledger_parts`) never gets a write
        // attempt: a task with no hidden criteria has no home for this
        // evidence, so this returns `Ok(())` without touching the ledger.
        let result = super::record_run_rationale_as_hidden_evidence(
            &ledger,
            &task_id,
            &task,
            &rationale,
            &[],
        );

        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn record_run_rationale_as_hidden_evidence_propagates_a_ledger_write_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let (store, repo_start, plan_id) = dummy_ledger_parts();
        let ledger = super::Ledger::new(&store, &repo_start, &plan_id);
        let task_id = TaskId::new("T001");
        let mut task = dummy_operator_task()?;
        task.hidden_criteria
            .push(tftio_planner::model::HiddenCriterion {
                claim: "a claim".to_owned(),
                criticality: tftio_planner::model::Criticality::Must,
                evaluator: tftio_planner::model::Evaluator::AgentEvaluated,
                check: None,
                ask: Some("a question".to_owned()),
                why_hidden: "because".to_owned(),
                counterfactual: "gaming".to_owned(),
                verdict: None,
                rationale: None,
                evidence_needed: None,
                evidence: Vec::new(),
            });
        let rationale = NonEmptyString::new("across every criterion this run considered")?;

        // No hidden criteria were judged this run, so this falls back to
        // the task's first hidden criterion -- and the broken `Ledger`
        // fails that write readably.
        let result = super::record_run_rationale_as_hidden_evidence(
            &ledger,
            &task_id,
            &task,
            &rationale,
            &[],
        );

        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn today_utc_has_the_expected_shape() -> Result<(), Box<dyn std::error::Error>> {
        let today = super::today_utc();
        let mut parts = today.split('-');
        let year = parts.next().ok_or("missing year")?;
        let month = parts.next().ok_or("missing month")?;
        let day = parts.next().ok_or("missing day")?;
        assert!(parts.next().is_none());
        assert_eq!(4, year.len());
        assert_eq!(2, month.len());
        assert_eq!(2, day.len());
        assert!(year.bytes().all(|byte| byte.is_ascii_digit()));
        assert!(month.bytes().all(|byte| byte.is_ascii_digit()));
        assert!(day.bytes().all(|byte| byte.is_ascii_digit()));
        Ok(())
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!((1970, 1, 1), super::civil_from_days(0));
        assert_eq!((2000, 1, 1), super::civil_from_days(10_957));
        assert_eq!((2000, 3, 1), super::civil_from_days(11_017));
        assert_eq!((1969, 12, 31), super::civil_from_days(-1));
    }

    #[test]
    fn task_status_str_covers_every_variant() {
        use tftio_planner::model::TaskStatus;

        assert_eq!(
            "not_started",
            super::task_status_str(TaskStatus::NotStarted)
        );
        assert_eq!("ready", super::task_status_str(TaskStatus::Ready));
        assert_eq!(
            "in_progress",
            super::task_status_str(TaskStatus::InProgress)
        );
        assert_eq!("blocked", super::task_status_str(TaskStatus::Blocked));
        assert_eq!("done", super::task_status_str(TaskStatus::Done));
        assert_eq!("abandoned", super::task_status_str(TaskStatus::Abandoned));
    }

    #[test]
    fn worker_evidence_rejects_blank_text() {
        assert!(matches!(
            super::worker_evidence("   "),
            Err(ToolError::EmptyEvidence(_))
        ));
    }

    #[test]
    fn judgment_str_covers_every_variant() -> Result<(), Box<dyn std::error::Error>> {
        use crate::model::{EvidenceNeeded, Judgment};

        assert_eq!("pass", super::judgment_str(&Judgment::Pass));
        assert_eq!("fail", super::judgment_str(&Judgment::Fail));
        assert_eq!(
            "undetermined",
            super::judgment_str(&Judgment::Undetermined {
                evidence_needed: EvidenceNeeded::new("more evidence")?,
            })
        );
        Ok(())
    }

    #[test]
    fn disposition_str_covers_every_variant() {
        use crate::model::Disposition;

        assert_eq!("accept", super::disposition_str(Disposition::Accept));
        assert_eq!("reject", super::disposition_str(Disposition::Reject));
        assert_eq!(
            "needs_operator",
            super::disposition_str(Disposition::NeedsOperator)
        );
    }

    #[test]
    fn map_evaluator_covers_every_variant() {
        use crate::model::EvaluatorKind;
        use tftio_planner::model::Evaluator as PlannerEvaluator;

        assert_eq!(
            EvaluatorKind::Automated,
            super::map_evaluator(PlannerEvaluator::Automated)
        );
        assert_eq!(
            EvaluatorKind::AgentEvaluated,
            super::map_evaluator(PlannerEvaluator::AgentEvaluated)
        );
        assert_eq!(
            EvaluatorKind::HumanJudgment,
            super::map_evaluator(PlannerEvaluator::HumanJudgment)
        );
    }
}
