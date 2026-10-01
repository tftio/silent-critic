//! The ledger: writes verdicts, evidence, and residuals into the plan
//! document, under a mutation lock.
//!
//! The plan document is the review artifact, so every verdict, evidence
//! record, and residual lands in it through `tftio_planner`'s mutations and
//! its atomic write; this module never writes plan Markdown directly
//! (`REPO_INVARIANTS.md` HO-003). A mutation lock (`<plan>.md.lock`,
//! `create_new`, bounded retries with backoff, stale-lock detection by age)
//! serializes writers so two concurrent verdict writes retry rather than one
//! silently losing to the other's stale-source `Conflict`; inside the lock,
//! a `Conflict` from `tftio_planner::apply_prepared` (byte-compare mismatch
//! against a source that changed since it was read) is itself retried by
//! re-reading and re-preparing, bounded, as a second layer against any
//! writer outside this lock's own convention.
//!
//! # Criterion addressing
//!
//! Neither this crate's [`crate::model::CriterionId`] nor a call into the
//! judge names a stable identifier for a hidden criterion in the plan
//! document — `tftio_planner::model::HiddenCriterion` carries no `id` field,
//! by design (`REPO_INVARIANTS.md` HO-002 keeps the format minimal). This
//! module addresses a criterion by its position, encoded into the id every
//! criterion is judged under: `visible-<n>` for the task's `n`-th
//! `acceptance_checks` entry, `hidden-<n>` for the `n`-th `hidden_criteria`
//! entry. [`criterion_address`] is the single place this scheme is defined;
//! everything else in this crate goes through it rather than re-deriving the
//! convention.
//!
//! # Residual routing
//!
//! [`Ledger::unresolved_items`] reconstructs every unresolved
//! [`Residual`] from the stored plan alone: `AwaitingHumanJudgment` and
//! `UndeterminedJudgment` are read directly off each hidden criterion's
//! structural verdict fields (no separate write needed — the verdict write
//! itself is the record). `UncoveredChangedScope` and `JudgeDisagreement`
//! have no home in the hidden-criteria or completion-evidence fields, so
//! [`Ledger::record_residual`] appends a tagged entry to the plan's Operator
//! Guidance Log (`RESIDUAL-BEGIN ... RESIDUAL-END`, embedded rationale
//! newline-escaped) and [`Ledger::unresolved_items`] parses those tags back
//! out. This is prose within the plan document's own guidance log, not a
//! second store: the plan remains the only durable artifact.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;

use tftio_planner::model::{
    HiddenEvidenceRecord as PlannerEvidenceRecord, HiddenVerdictJudgment as PlannerVerdict,
    TaskId as PlannerTaskId,
};
use tftio_planner::{Mutation, MutationRequest};

use crate::model::{
    ChangedPath, CriterionId, EmptyStringError, Evidence, EvidenceId, EvidenceNeeded,
    EvidenceProvenance, Judgment, NonEmptyString, Residual, TaskId, Verdict,
};
use crate::store::{Store, StoreError};
use uuid::Uuid;

// ---------------------------------------------------------------------
// Criterion addressing
// ---------------------------------------------------------------------

/// Where one criterion lives in the plan document.
///
/// Decoded from the `visible-<n>`/`hidden-<n>` id scheme this module
/// defines (see module docs). Assigned when the judge tool builds its
/// criteria list, consumed here when a verdict is written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CriterionAddress {
    /// The task's `n`-th `acceptance_checks` entry.
    Visible {
        /// The entry's position in `acceptance_checks`.
        index: usize,
    },
    /// The task's `n`-th `hidden_criteria` entry.
    Hidden {
        /// The entry's position in `hidden_criteria`.
        index: usize,
    },
}

/// Build a criterion id for the task's `n`-th visible acceptance check.
#[must_use]
pub fn visible_criterion_id(index: usize) -> CriterionId {
    CriterionId::new(format!("visible-{index}"))
}

/// Build a criterion id for the task's `n`-th hidden criterion.
#[must_use]
pub fn hidden_criterion_id(index: usize) -> CriterionId {
    CriterionId::new(format!("hidden-{index}"))
}

/// Decode a criterion id built by [`visible_criterion_id`] or
/// [`hidden_criterion_id`] back into its address, or `None` if it does not
/// match either shape.
#[must_use]
pub fn criterion_address(id: &CriterionId) -> Option<CriterionAddress> {
    if let Some(rest) = id.as_str().strip_prefix("visible-") {
        return rest
            .parse::<usize>()
            .ok()
            .map(|index| CriterionAddress::Visible { index });
    }
    if let Some(rest) = id.as_str().strip_prefix("hidden-") {
        return rest
            .parse::<usize>()
            .ok()
            .map(|index| CriterionAddress::Hidden { index });
    }
    None
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

/// A failure writing to, or reading residuals from, the ledger.
#[derive(Debug, Error)]
pub enum LedgerError {
    /// The plan store reported a failure resolving the plan.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A filesystem operation failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path the operation was performed on.
        path: PathBuf,
        /// The underlying filesystem error.
        source: io::Error,
    },
    /// The mutation lock could not be acquired within its retry budget.
    #[error("could not acquire the plan mutation lock at {path} within the retry budget")]
    LockTimeout {
        /// The lock file path contention was observed on.
        path: PathBuf,
    },
    /// The stored plan could not be parsed.
    #[error(transparent)]
    Parse(#[from] tftio_planner::ParseError),
    /// A requested mutation could not be prepared.
    #[error(transparent)]
    Mutation(#[from] tftio_planner::MutationError),
    /// A prepared mutation could not be written back to the store, even
    /// after retrying a stale-source conflict.
    #[error(transparent)]
    Write(#[from] tftio_planner::WriteError),
    /// The given task id is not a valid `tftio_planner` task identifier.
    #[error("not a valid task id: {0}")]
    InvalidTaskId(String),
    /// No task with the given identifier exists in the stored plan.
    #[error("no such task: {0}")]
    UnknownTask(String),
    /// A rendered evidence summary was empty (practically unreachable: it
    /// always carries a non-empty literal prefix).
    #[error(transparent)]
    EmptyEvidence(#[from] crate::model::EmptyStringError),
}

fn io_error(path: &Path) -> impl Fn(io::Error) -> LedgerError + '_ {
    move |source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn planner_task_id(task_id: &TaskId) -> Result<PlannerTaskId, LedgerError> {
    PlannerTaskId::parse(task_id.as_str().to_owned())
        .map_err(|_| LedgerError::InvalidTaskId(task_id.as_str().to_owned()))
}

// ---------------------------------------------------------------------
// Mutation lock
// ---------------------------------------------------------------------

/// How many times [`MutationLock::acquire`] retries before giving up.
const LOCK_ACQUIRE_ATTEMPTS: u32 = 100;
/// The delay between lock-acquisition attempts.
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
/// How old an existing lock file must be before it is treated as abandoned
/// (its writer crashed or was killed without cleaning up) and removed.
const STALE_LOCK_THRESHOLD: Duration = Duration::from_secs(30);
/// How many times a write retries after a stale-source `Conflict`, inside
/// the mutation lock, before giving up.
const STALE_SOURCE_RETRIES: u32 = 5;

/// A held lock on one plan's mutation path (`<plan path>.lock`), released
/// (the lock file removed) on drop.
struct MutationLock {
    path: PathBuf,
}

impl MutationLock {
    /// Acquire the lock for `plan_path`, retrying with backoff, and
    /// reclaiming a stale lock left behind by a crashed writer.
    ///
    /// A bare `loop` whose only exits are `return`: every attempt either
    /// returns (success, an I/O error, or the retry budget exhausted) or
    /// `continue`s after reclaiming a stale lock, so there is no
    /// fallthrough state requiring a trailing statement after the loop
    /// (mirrors `evaluate::judge::judge_once`'s identical shape and its
    /// matching comment).
    fn acquire(plan_path: &Path) -> Result<Self, LedgerError> {
        let lock_path = lock_path_for(plan_path);
        let mut attempt = 0_u32;
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(_file) => return Ok(Self { path: lock_path }),
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                    if is_stale(&lock_path) {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    attempt += 1;
                    if attempt >= LOCK_ACQUIRE_ATTEMPTS {
                        return Err(LedgerError::LockTimeout { path: lock_path });
                    }
                    thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(source) => return Err(io_error(&lock_path)(source)),
            }
        }
    }
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// `plan_path` is always `<plan_dir>/plan.md`, so it always has both a file
// name and a parent; the eager fallbacks (`map_or`/`unwrap_or`, not the
// `_else` forms) mean the fallback value is always computed as part of
// evaluating this same line, mirroring `src/tools.rs`'s identical choice for
// the same structurally-unreachable case.
#[allow(clippy::or_fun_call)]
fn lock_path_for(plan_path: &Path) -> PathBuf {
    let mut name = plan_path.file_name().map_or("plan.md".to_owned(), |name| {
        name.to_string_lossy().into_owned()
    });
    name.push_str(".lock");
    plan_path.parent().unwrap_or(Path::new(".")).join(name)
}

/// Whether the lock file at `path` is older than [`STALE_LOCK_THRESHOLD`],
/// treating an unreadable metadata call (the file already gone, a racing
/// remove) the same as "not stale" — a lock that has already vanished needs
/// no reclamation, and the next `create_new` attempt will simply succeed.
fn is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age >= STALE_LOCK_THRESHOLD)
        })
}

// ---------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------

/// The write path into one stored plan's ledger.
pub struct Ledger<'a> {
    store: &'a Store,
    repo_start: &'a Path,
    plan_id: &'a str,
}

impl<'a> Ledger<'a> {
    /// Build a ledger over the plan `plan_id` stores for `repo_start`.
    #[must_use]
    pub const fn new(store: &'a Store, repo_start: &'a Path, plan_id: &'a str) -> Self {
        Self {
            store,
            repo_start,
            plan_id,
        }
    }

    fn plan_path(&self) -> Result<PathBuf, LedgerError> {
        Ok(self.store.plan_path(self.repo_start, self.plan_id)?)
    }

    /// Apply one mutation, built fresh from the current on-disk source on
    /// every attempt, serialized against every other writer through the
    /// mutation lock and retried on a stale-source conflict.
    fn mutate(
        &self,
        build: impl Fn(&str) -> Result<MutationRequest, LedgerError>,
    ) -> Result<(), LedgerError> {
        self.mutate_with_hook(build, |_source, _attempts| Ok(()))
    }

    /// [`Ledger::mutate`], plus a hook run immediately after each attempt's
    /// read and before its mutation is prepared. Production code always
    /// passes a no-op hook (via [`Ledger::mutate`]); tests use it to
    /// deterministically inject a same-directory write from outside this
    /// lock's own convention, at the exact point that produces a real
    /// `tftio_planner::WriteError::Conflict` on this attempt's own
    /// `apply_prepared` call, rather than relying on a timing-dependent
    /// race between threads to exercise the retry.
    fn mutate_with_hook(
        &self,
        build: impl Fn(&str) -> Result<MutationRequest, LedgerError>,
        mut after_read: impl FnMut(&str, u32) -> Result<(), LedgerError>,
    ) -> Result<(), LedgerError> {
        let path = self.plan_path()?;
        let _lock = MutationLock::acquire(&path)?;
        let mut attempts = 0_u32;
        loop {
            let source = std::fs::read_to_string(&path).map_err(io_error(&path))?;
            after_read(&source, attempts)?;
            let request = build(&source)?;
            let prepared = tftio_planner::prepare_markdown_mutation(&source, &request)?;
            match tftio_planner::apply_prepared(&path, &prepared) {
                Ok(()) => return Ok(()),
                Err(tftio_planner::WriteError::Conflict { .. })
                    if attempts < STALE_SOURCE_RETRIES =>
                {
                    attempts += 1;
                }
                Err(other) => return Err(other.into()),
            }
        }
    }

    /// Record a verdict against one of a task's hidden criteria, by its
    /// position in `hidden_criteria`. Never reaches the worker-safe
    /// projection: `hidden_criteria` is stripped wholesale by
    /// `tftio_planner::project_worker_markdown` (`REPO_INVARIANTS.md`
    /// HO-001).
    ///
    /// `evidence` is empty for the common case -- a re-judge that only
    /// updates the verdict and rationale -- and in that case this call
    /// preserves whatever evidence the criterion already carries (fix
    /// round 2, finding #3): it maps to
    /// `tftio_planner::Mutation::RecordHiddenVerdict`'s `evidence: None`,
    /// never to a replacement with an empty list, which would silently
    /// erase judge-provenance evidence (a run-level rationale, or a panel
    /// disagreement) a prior [`Ledger::append_hidden_evidence`] call
    /// already attached to this same criterion -- exactly the bug this
    /// fix round closed at `src/tools.rs`'s `route_verdict`, which now
    /// calls this with an empty slice specifically to get that preserving
    /// behavior. A non-empty `evidence` still replaces the criterion's
    /// evidence list wholesale, as `run_one_automated_check` intends when
    /// it hands this the current run's fresh check result.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] when the plan cannot be resolved, read, or
    /// parsed, the task id is invalid, the mutation lock cannot be
    /// acquired, or the write is rejected (including an out-of-range
    /// index, or an `evidence_needed` inconsistent with the verdict).
    pub fn record_hidden_verdict(
        &self,
        task_id: &TaskId,
        index: usize,
        verdict: &Verdict,
        evidence: &[Evidence],
    ) -> Result<(), LedgerError> {
        let task_id = planner_task_id(task_id)?;
        let (judgment, evidence_needed) = split_judgment(verdict.judgment());
        let rationale = verdict.rationale().to_owned();
        let evidence: Option<Vec<PlannerEvidenceRecord>> = if evidence.is_empty() {
            None
        } else {
            Some(evidence.iter().map(to_planner_evidence).collect())
        };
        self.mutate(move |_source| {
            Ok(MutationRequest {
                date: today_utc(),
                mutation: Mutation::RecordHiddenVerdict {
                    task_id: task_id.clone(),
                    index,
                    verdict: judgment,
                    rationale: rationale.clone(),
                    evidence_needed: evidence_needed.clone(),
                    evidence: evidence.clone(),
                },
            })
        })
    }

    /// Append evidence to one of a task's hidden criteria, without touching
    /// its verdict, rationale, or `evidence_needed`: the home for
    /// judge-provenance evidence discovered after a verdict was already
    /// recorded by [`Ledger::record_hidden_verdict`] -- a run's own
    /// disposition rationale, or a second provider's disagreement -- so
    /// that material never has to travel through a worker-safe or
    /// operator-guidance surface it must not reach
    /// (`REPO_INVARIANTS.md` HO-001). Never reaches the worker-safe
    /// projection, for the same reason [`Ledger::record_hidden_verdict`]
    /// does not.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] under the same conditions as
    /// [`Ledger::record_hidden_verdict`].
    pub fn append_hidden_evidence(
        &self,
        task_id: &TaskId,
        index: usize,
        evidence: &[Evidence],
    ) -> Result<(), LedgerError> {
        let task_id = planner_task_id(task_id)?;
        let evidence: Vec<PlannerEvidenceRecord> =
            evidence.iter().map(to_planner_evidence).collect();
        self.mutate(move |_source| {
            Ok(MutationRequest {
                date: today_utc(),
                mutation: Mutation::AppendHiddenEvidence {
                    task_id: task_id.clone(),
                    index,
                    evidence: evidence.clone(),
                },
            })
        })
    }

    /// Append text to a task's completion evidence, without any lifecycle
    /// transition: the home for visible-criterion verdicts, automated check
    /// results, git facts, and residual summaries (everything that belongs
    /// in the review artifact but is not a hidden-criterion field).
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] under the same conditions as
    /// [`Ledger::record_hidden_verdict`].
    pub fn append_completion_evidence(
        &self,
        task_id: &TaskId,
        text: &str,
    ) -> Result<(), LedgerError> {
        let task_id = planner_task_id(task_id)?;
        let text = text.to_owned();
        self.mutate(move |_source| {
            Ok(MutationRequest {
                date: today_utc(),
                mutation: Mutation::AppendCompletionEvidence {
                    task_id: task_id.clone(),
                    text: text.clone(),
                },
            })
        })
    }

    /// Record a visible-criterion verdict as completion evidence, in a
    /// compact, deterministic form.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] under the same conditions as
    /// [`Ledger::append_completion_evidence`].
    pub fn record_visible_verdict(
        &self,
        task_id: &TaskId,
        verdict: &Verdict,
    ) -> Result<(), LedgerError> {
        self.append_completion_evidence(task_id, &render_visible_verdict(verdict))
    }

    /// Record a residual that has no structural home elsewhere in the plan
    /// (`UncoveredChangedScope`, `JudgeDisagreement`) as a tagged Operator
    /// Guidance Log entry, tagged with `task_id` so [`Ledger::unresolved_items`]
    /// can hand it back to its caller as an [`OperatorItem`] without the
    /// caller having to re-derive which task it belongs to.
    /// `AwaitingHumanJudgment` and `UndeterminedJudgment` need no write here:
    /// they are read directly off the hidden criterion's own verdict fields
    /// by [`Ledger::unresolved_items`], recorded already by
    /// [`Ledger::record_hidden_verdict`].
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] under the same conditions as
    /// [`Ledger::append_completion_evidence`], except this mutation targets
    /// the plan-level Operator Guidance Log rather than one task.
    pub fn record_residual(
        &self,
        task_id: &TaskId,
        residual: &Residual,
    ) -> Result<(), LedgerError> {
        if let Residual::JudgeDisagreement {
            criterion_id,
            first,
            second,
        } = residual
            && let Some(CriterionAddress::Hidden { index }) = criterion_address(criterion_id)
        {
            // A disagreement about a hidden criterion never writes its
            // criterion id or either provider's rationale to the Operator
            // Guidance Log (the projection passes that log through
            // untouched): both verdicts land instead as judge-provenance
            // evidence on the criterion itself, inside `hidden_criteria`,
            // which is stripped wholesale (`REPO_INVARIANTS.md` HO-001).
            //
            // Fix round 2, finding #6: no guidance-log entry is written
            // for this case at all, not even a criterion-free "see the
            // operator ledger" line -- a line written only when the
            // disagreeing criterion happens to be hidden is itself a
            // side-channel: its mere presence in the worker-visible
            // guidance log discloses that a hidden criterion exists,
            // which HO-001 forbids regardless of what the line says. The
            // operator still learns of the disagreement through `measure`
            // and the sealed artifact, which read `hidden_criteria`'s
            // evidence directly.
            let evidence = [
                disagreement_evidence("first", first)?,
                disagreement_evidence("second", second)?,
            ];
            return self.append_hidden_evidence(task_id, index, &evidence);
        }
        let Some(entry) = render_guidance_residual(task_id, residual) else {
            return Ok(());
        };
        self.add_guidance(entry)
    }

    /// Record that the uncovered-changed-scope check did not run for
    /// `task_id`, because its plan is Planning Document Format v2 (which
    /// carries no `files.likely_modify` for any task to check against).
    ///
    /// This is deliberately *not* a [`Residual`]: it is appended to the
    /// Operator Guidance Log under its own `NOTE-BEGIN`/`NOTE-END` tag,
    /// which the private `parse_guidance_residuals` does not recognize, so
    /// it can never be reconstructed as an [`OperatorItem`] by
    /// [`Ledger::unresolved_items`] and can never contribute to
    /// `operator_attention_required`. It is still visible in the plan
    /// document (the Operator Guidance Log is rendered verbatim in the
    /// sealed artifact, `src/render.rs`'s `render_logs`) and is textually
    /// distinct from both silence (a v1 task whose declared scope is fully
    /// covered) and an `UncoveredChangedScope` residual, so a reader can
    /// never mistake "did not run" for "ran and passed".
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] under the same conditions as
    /// [`Ledger::record_residual`].
    pub fn record_scope_check_not_run(&self, task_id: &TaskId) -> Result<(), LedgerError> {
        self.add_guidance(format!(
            "{NOTE_BEGIN}scope_check_not_run\ntask: {task_id}\nreason: plan format v2 declares no files.likely_modify for any task, so the uncovered-changed-scope check does not run\n{NOTE_END}"
        ))
    }

    /// Append one entry to the plan's Operator Guidance Log.
    fn add_guidance(&self, entry: String) -> Result<(), LedgerError> {
        self.mutate(move |_source| {
            Ok(MutationRequest {
                date: today_utc(),
                mutation: Mutation::AddGuidance(entry.clone()),
            })
        })
    }

    /// Every unresolved item the stored plan carries, reconstructed from the
    /// plan document alone: `AwaitingHumanJudgment` and
    /// `UndeterminedJudgment` from each task's hidden-criterion verdict
    /// fields, `UncoveredChangedScope` and `JudgeDisagreement` from tagged
    /// Operator Guidance Log entries (see [`Ledger::record_residual`]). Each
    /// item carries the task id it belongs to, so a caller (T011/T012) never
    /// has to re-derive it.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] when the plan cannot be resolved, read, or
    /// parsed.
    pub fn unresolved_items(&self) -> Result<Vec<OperatorItem>, LedgerError> {
        let path = self.plan_path()?;
        let source = std::fs::read_to_string(&path).map_err(io_error(&path))?;
        Ok(unresolved_items_in(&source)?)
    }

    /// Apply a plan-level mutation (`T011`'s seal transition; potentially
    /// other plan-level callers later) under this ledger's own
    /// mutation-lock and stale-source-retry path, rather than a caller
    /// re-implementing that protocol against `tftio_planner` directly.
    ///
    /// `mutation` is cloned on every retry attempt, mirroring every other
    /// `Ledger` write method's `move` closure over its own pre-built
    /// mutation value.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] under the same conditions as
    /// [`Ledger::record_hidden_verdict`].
    pub(crate) fn apply_plan_mutation(&self, mutation: Mutation) -> Result<(), LedgerError> {
        self.mutate(move |_source| {
            Ok(MutationRequest {
                date: today_utc(),
                mutation: mutation.clone(),
            })
        })
    }
}

/// One unresolved item routed to the operator, together with the task it belongs to.
///
/// [`Residual`] alone (`src/model.rs`) does not always carry a task id
/// (`AwaitingHumanJudgment`/`UndeterminedJudgment` carry only a criterion id,
/// which is only unique within one task); this wrapper is this module's own
/// addition, not a change to [`Residual`] itself, so a consumer of
/// [`Ledger::unresolved_items`] never has to re-derive which task a residual
/// came from by re-walking the plan itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorItem {
    /// The task this item belongs to.
    pub task_id: TaskId,
    /// The unresolved item itself.
    pub residual: Residual,
}

fn split_judgment(judgment: &Judgment) -> (PlannerVerdict, Option<String>) {
    match judgment {
        Judgment::Pass => (PlannerVerdict::Pass, None),
        Judgment::Fail => (PlannerVerdict::Fail, None),
        Judgment::Undetermined { evidence_needed } => (
            PlannerVerdict::Undetermined,
            Some(evidence_needed.as_str().to_owned()),
        ),
    }
}

const fn provenance_name(provenance: EvidenceProvenance) -> &'static str {
    match provenance {
        EvidenceProvenance::ToolAuthored => "tool_authored",
        EvidenceProvenance::Judge => "judge",
        EvidenceProvenance::WorkerNarrated => "worker_narrated",
        EvidenceProvenance::OperatorObserved => "operator_observed",
    }
}

fn to_planner_evidence(evidence: &Evidence) -> PlannerEvidenceRecord {
    PlannerEvidenceRecord {
        summary: evidence.summary().to_owned(),
        provenance: provenance_name(evidence.provenance()).to_owned(),
    }
}

/// Build a judge-provenance [`Evidence`] record naming `label` (`"first"` or
/// `"second"`, the disagreeing panel member) and `verdict`'s own judgment
/// and rationale, for [`Ledger::record_residual`]'s hidden-criterion
/// disagreement branch. The rationale text this carries is exactly the kind
/// of hidden-criterion material `REPO_INVARIANTS.md` HO-001 requires stay
/// out of any worker-facing or operator-guidance surface; an `Evidence`
/// record on the criterion itself is the only place it may live.
/// The prefix every hidden-criterion disagreement evidence record's
/// summary begins with. Shared between `disagreement_evidence` (which
/// writes it) and [`hidden_disagreement_count`] (which reads it back), in
/// this one `const`, so the two can never drift apart.
const HIDDEN_DISAGREEMENT_PREFIX: &str = "judge disagreement (";

/// The exact prefix `disagreement_evidence` writes for the `"first"`
/// disagreeing panel member's own record. [`Ledger::record_residual`]'s
/// hidden-criterion branch calls `disagreement_evidence` exactly twice per
/// disagreement *event* — once labelled `"first"`, once `"second"` — so
/// [`hidden_disagreement_count`] counts *events* by counting only the
/// `"first"`-labelled record of each pair, never the raw record count
/// (which would silently double it).
const HIDDEN_DISAGREEMENT_FIRST_PREFIX: &str = "judge disagreement (first)";

fn disagreement_evidence(label: &str, verdict: &Verdict) -> Result<Evidence, LedgerError> {
    let judgment = judgment_name(verdict.judgment());
    let rationale = verdict.rationale();
    let text =
        format!("{HIDDEN_DISAGREEMENT_PREFIX}{label}): judgment={judgment} rationale={rationale}");
    let summary = NonEmptyString::new(text)?;
    Ok(Evidence::new(
        EvidenceId::new(Uuid::new_v4().simple().to_string()),
        EvidenceProvenance::Judge,
        summary,
    ))
}

/// The number of judge-panel disagreement *events* recorded for one hidden
/// criterion, from `evidence` (that criterion's own evidence list, as
/// parsed straight from the stored plan).
///
/// [`Ledger::record_residual`]'s hidden-criterion branch always writes one
/// disagreement as a pair of judge-provenance records built by
/// `disagreement_evidence` (labelled `"first"` and `"second"`, the two
/// disagreeing panel members) — so this counts only the `"first"`-labelled
/// record of each pair, never both, which would double-count. A criterion
/// re-judged more than once accumulates one such pair per disagreeing run,
/// so this is a true event count, not a presence flag. This is the only
/// place a hidden-criterion disagreement is reconstructable from the
/// stored plan: unlike a visible criterion's, it is never written as a
/// criterion-attributed [`Residual::JudgeDisagreement`] in the Operator
/// Guidance Log (`REPO_INVARIANTS.md` HO-001 — reading this count off the
/// operator plan is fine; the plan's own guidance log, which no worker or
/// orchestrator-scope response passes through, must still never carry it,
/// which is why [`Ledger::record_residual`] never writes it there for a
/// hidden criterion).
#[must_use]
pub fn hidden_disagreement_count(evidence: &[PlannerEvidenceRecord]) -> usize {
    evidence
        .iter()
        .filter(|record| record.summary.starts_with(HIDDEN_DISAGREEMENT_FIRST_PREFIX))
        .count()
}

// ---------------------------------------------------------------------
// Judge-provenance evidence (dual-panel visibility on agreement)
// ---------------------------------------------------------------------

/// The prefix every judge-provenance evidence record's summary begins
/// with, shared between [`judge_provenance_evidence`] (which writes it)
/// and [`judge_provenance_provider_ids`] (which reads it back).
///
/// Deliberately distinct from [`HIDDEN_DISAGREEMENT_PREFIX`] (`"judge
/// disagreement ("`), so the two record kinds never collide even though
/// both start with `"judge "`: [`judge_provenance_provider_ids`] rejects
/// any record whose text after this prefix itself starts with
/// `"disagreement ("`.
const JUDGE_PROVENANCE_PREFIX: &str = "judge ";
const JUDGE_PROVENANCE_DISAGREEMENT_MARKER: &str = "disagreement (";
const JUDGE_PROVENANCE_INFIX: &str = " judged ";

/// Build a judge-provenance [`Evidence`] record naming `provider_id` and `verdict`'s judgment.
///
/// Written for every provider that judges a hidden criterion on every run
/// -- not only the provider(s) that produced the canonical verdict, and
/// not only when two providers disagree.
///
/// This is what makes a dual panel visible on the stored plan even when
/// both providers agree: agreement itself leaves no trace in
/// [`Residual::JudgeDisagreement`] or in the disagreement-evidence pair
/// [`hidden_disagreement_count`] reads, so without this record a dual
/// panel that agreed on every criterion is indistinguishable from a
/// single-provider run. The rationale-free summary text
/// (`"judge <id> judged <judgment>"`) carries no hidden-criterion claim
/// text or rationale -- only the id and judgment -- but it is still
/// `EvidenceProvenance::Judge` evidence living only inside
/// `hidden_criteria` (`REPO_INVARIANTS.md` HO-001): stripped by
/// projection, never reaching a worker-facing or orchestrator-scope
/// surface.
///
/// # Errors
///
/// Returns [`EmptyStringError`] only if `provider_id` is empty (the
/// summary would then be empty too); unreachable in practice since every
/// caller's provider id comes from a non-empty [`crate::provider::ProviderId`].
pub fn judge_provenance_evidence(
    provider_id: &str,
    verdict: &Verdict,
) -> Result<Evidence, EmptyStringError> {
    let judgment = judgment_name(verdict.judgment());
    let text = format!("{JUDGE_PROVENANCE_PREFIX}{provider_id}{JUDGE_PROVENANCE_INFIX}{judgment}");
    let summary = NonEmptyString::new(text)?;
    Ok(Evidence::new(
        EvidenceId::new(Uuid::new_v4().simple().to_string()),
        EvidenceProvenance::Judge,
        summary,
    ))
}

/// The distinct provider ids recorded on one hidden criterion's evidence.
///
/// Reads back what [`judge_provenance_evidence`] wrote, from `evidence`
/// (that criterion's own evidence list, as parsed straight from the stored
/// plan).
///
/// A stored plan written before this change carries no such records at
/// all -- its dual-panel disagreements, if any, are still readable through
/// [`hidden_disagreement_count`], but its *agreements* are not
/// reconstructable, which is exactly the gap this record closes for plans
/// judged after this change.
#[must_use]
pub fn judge_provenance_provider_ids(
    evidence: &[PlannerEvidenceRecord],
) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    for record in evidence {
        let Some(rest) = record.summary.strip_prefix(JUDGE_PROVENANCE_PREFIX) else {
            continue;
        };
        if rest.starts_with(JUDGE_PROVENANCE_DISAGREEMENT_MARKER) {
            continue;
        }
        if let Some((id, _judgment)) = rest.split_once(JUDGE_PROVENANCE_INFIX) {
            ids.insert(id.to_owned());
        }
    }
    ids
}

fn render_visible_verdict(verdict: &Verdict) -> String {
    use std::fmt::Write as _;

    let mut out = format!(
        "visible criterion {} verdict: {}",
        verdict.criterion_id(),
        judgment_name(verdict.judgment())
    );
    let _ = write!(out, "\nrationale: {}", verdict.rationale());
    if let Judgment::Undetermined { evidence_needed } = verdict.judgment() {
        let _ = write!(out, "\nevidence needed: {}", evidence_needed.as_str());
    }
    out
}

/// A `completion_evidence` block began with the `visible criterion <id>
/// verdict: <name>` marker `render_visible_verdict` writes, so it is
/// recognizably meant to be one, but could not be parsed as one.
///
/// Deliberately distinct from "not a verdict block at all" (an unrelated
/// entry, silently skipped): a block that looks like a verdict record but
/// is corrupted must never be read as "no verdict here", since that would
/// let a hand-edited or truncated plan silently manufacture a false
/// "every visible criterion passed" reading.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("malformed visible-criterion verdict block: {block:?}")]
pub struct VisibleVerdictParseError {
    /// The raw, unparsed block text.
    pub block: String,
}

/// Read every visible-criterion verdict `render_visible_verdict` wrote
/// back out of a task's `completion_evidence`, in the order they were
/// appended.
///
/// `completion_evidence` is `tftio_planner`'s own append log: each entry is
/// one `{date} — evidence: {body}` record (`append_task_record` in
/// `tftio_planner::state`), separated by a blank line, with only the first
/// line of `body` carrying that date prefix. A `render_visible_verdict`
/// block is recognized by its own `visible criterion <id> verdict: <name>`
/// marker regardless of what (if anything) precedes it on that first line,
/// so this survives the date prefix without needing to parse it. Every
/// other entry (a judge-run summary, an operator note, a plain completion
/// note such as a fixture's `"Done in a prior run."`) is silently skipped:
/// this function's only job is recovering the visible verdicts, not
/// validating the rest of the log. A block that *does* carry the marker but
/// fails to parse beyond it is never silently skipped: it is a
/// [`VisibleVerdictParseError`], failing the whole call, since a caller
/// (`crate::measure`) that read a corrupted block as "no verdict here"
/// could manufacture a false "every visible criterion passed" reading.
///
/// [`crate::measure`] is this function's only intended caller: visible
/// criteria carry no structural verdict field in the stored plan (unlike a
/// hidden criterion's own `verdict`), so this text is the only place a
/// visible judgment survives. A later append for the same criterion id
/// (a re-judged task) simply appears later in the returned list; a caller
/// that wants "the current verdict" keeps the last one per criterion id.
///
/// # Errors
///
/// Returns [`VisibleVerdictParseError`] for the first block that carries
/// the verdict marker but cannot be fully parsed.
pub fn parse_visible_verdicts_in(
    completion_evidence: &str,
) -> Result<Vec<Verdict>, VisibleVerdictParseError> {
    completion_evidence
        .split("\n\n")
        .filter_map(parse_one_visible_verdict_block)
        .collect()
}

/// `None`: `block` does not carry the verdict marker at all (not a verdict
/// block, silently skipped by the caller). `Some(Err(_))`: `block` carries
/// the marker but fails to parse beyond it (a real error, per this
/// function's own docs). `Some(Ok(_))`: a fully parsed verdict.
fn parse_one_visible_verdict_block(
    block: &str,
) -> Option<Result<Verdict, VisibleVerdictParseError>> {
    const MARKER: &str = "visible criterion ";

    let malformed = || VisibleVerdictParseError {
        block: block.to_owned(),
    };

    let mut lines = block.lines();
    let first = lines.next()?;
    // `split_once` both finds the marker and slices the remainder in one
    // step, unlike a separate `find` + byte-offset `get`: there is no
    // second, separately-fallible step here that could fail only because
    // of an invalid byte boundary `split_once` itself already ruled out by
    // succeeding.
    let (_, rest) = first.split_once(MARKER)?;
    // Recognized as a verdict block from here on: every remaining failure
    // is `Some(Err(_))`, never `None`.
    let Some((criterion_id, judgment_name)) = rest.split_once(" verdict: ") else {
        return Some(Err(malformed()));
    };
    let criterion_id = CriterionId::new(criterion_id.trim());

    let mut rationale = None;
    let mut evidence_needed = None;
    for line in lines {
        if let Some(value) = parse_field(line, "rationale:") {
            rationale = Some(value.to_owned());
        } else if let Some(value) = parse_field(line, "evidence needed:") {
            evidence_needed = Some(value.to_owned());
        }
    }

    let Some(rationale) = rationale.and_then(|value| NonEmptyString::new(value).ok()) else {
        return Some(Err(malformed()));
    };
    let Some(judgment) = parse_judgment_name(judgment_name.trim(), evidence_needed) else {
        return Some(Err(malformed()));
    };
    Some(Ok(Verdict::new(criterion_id, judgment, rationale)))
}

const fn judgment_name(judgment: &Judgment) -> &'static str {
    match judgment {
        Judgment::Pass => "pass",
        Judgment::Fail => "fail",
        Judgment::Undetermined { .. } => "undetermined",
    }
}

// ---------------------------------------------------------------------
// Guidance-log residual tags
// ---------------------------------------------------------------------

const RESIDUAL_BEGIN: &str = "RESIDUAL-BEGIN ";
const RESIDUAL_END: &str = "RESIDUAL-END";

/// Tag for [`Ledger::record_scope_check_not_run`]'s informational note.
/// Deliberately a different tag from [`RESIDUAL_BEGIN`]/[`RESIDUAL_END`]:
/// [`parse_guidance_residuals`] only ever looks for `RESIDUAL_BEGIN`, so a
/// `NOTE_BEGIN` entry is structurally invisible to
/// [`Ledger::unresolved_items`] and can never become an [`OperatorItem`] --
/// which is what keeps it from ever contributing to
/// `operator_attention_required`.
const NOTE_BEGIN: &str = "NOTE-BEGIN ";
const NOTE_END: &str = "NOTE-END";

fn escape_line(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') | None => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Render `residual` (belonging to `task_id`) as a tagged Operator Guidance
/// Log entry, or `None` for a residual class that is instead read
/// structurally (see [`Ledger::record_residual`]'s docs). Every rendered tag
/// carries `task_id` on its own `task:` line, independent of whether the
/// [`Residual`] variant itself has a task-id field, so
/// [`unresolved_items_in`] can hand every item back as an [`OperatorItem`].
fn render_guidance_residual(task_id: &TaskId, residual: &Residual) -> Option<String> {
    match residual {
        Residual::AwaitingHumanJudgment { .. } | Residual::UndeterminedJudgment { .. } => None,
        Residual::UncoveredChangedScope { paths, .. } => {
            let path_list = paths
                .iter()
                .map(|path| escape_line(path.as_str()))
                .collect::<Vec<_>>()
                .join(",");
            Some(format!(
                "{RESIDUAL_BEGIN}uncovered_changed_scope\ntask: {task_id}\npaths: {path_list}\n{RESIDUAL_END}"
            ))
        }
        Residual::JudgeDisagreement {
            criterion_id,
            first,
            second,
        } => {
            let mut body = format!(
                "{RESIDUAL_BEGIN}judge_disagreement\n\
                 task: {task_id}\n\
                 criterion: {criterion_id}\n\
                 first-judgment: {}\n\
                 first-rationale: {}\n",
                judgment_name(first.judgment()),
                escape_line(first.rationale()),
            );
            if let Judgment::Undetermined { evidence_needed } = first.judgment() {
                let _ = writeln!(
                    body,
                    "first-evidence-needed: {}",
                    escape_line(evidence_needed.as_str())
                );
            }
            let _ = writeln!(
                body,
                "second-judgment: {}",
                judgment_name(second.judgment())
            );
            let _ = writeln!(
                body,
                "second-rationale: {}",
                escape_line(second.rationale())
            );
            if let Judgment::Undetermined { evidence_needed } = second.judgment() {
                let _ = writeln!(
                    body,
                    "second-evidence-needed: {}",
                    escape_line(evidence_needed.as_str())
                );
            }
            body.push_str(RESIDUAL_END);
            Some(body)
        }
    }
}

fn parse_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.strip_prefix(key).map(str::trim_start)
}

fn parse_judgment_name(name: &str, evidence_needed: Option<String>) -> Option<Judgment> {
    match name {
        "pass" => Some(Judgment::Pass),
        "fail" => Some(Judgment::Fail),
        "undetermined" => {
            let evidence_needed = EvidenceNeeded::new(evidence_needed.unwrap_or_default()).ok()?;
            Some(Judgment::Undetermined { evidence_needed })
        }
        _ => None,
    }
}

fn parse_guidance_residuals(guidance_log: &str) -> Vec<OperatorItem> {
    let mut items = Vec::new();
    let mut lines = guidance_log.lines();
    while let Some(line) = lines.next() {
        let Some(kind) = line.strip_prefix(RESIDUAL_BEGIN) else {
            continue;
        };
        let kind = kind.trim().to_owned();
        let mut body = Vec::new();
        for body_line in lines.by_ref() {
            if body_line == RESIDUAL_END {
                break;
            }
            body.push(body_line);
        }
        items.extend(parse_one_residual(&kind, &body));
    }
    items
}

fn parse_one_residual(kind: &str, body: &[&str]) -> Option<OperatorItem> {
    let task_id = body.iter().find_map(|line| parse_field(line, "task:"))?;
    let residual = parse_one_residual_body(kind, body)?;
    Some(OperatorItem {
        task_id: TaskId::new(task_id),
        residual,
    })
}

fn parse_one_residual_body(kind: &str, body: &[&str]) -> Option<Residual> {
    match kind {
        "uncovered_changed_scope" => {
            let task_id = body.iter().find_map(|line| parse_field(line, "task:"))?;
            let paths_line = body.iter().find_map(|line| parse_field(line, "paths:"))?;
            let paths = paths_line
                .split(',')
                .filter(|entry| !entry.is_empty())
                .map(|entry| ChangedPath::new(unescape_line(entry)))
                .collect();
            Some(Residual::UncoveredChangedScope {
                task_id: TaskId::new(task_id),
                paths,
            })
        }
        "judge_disagreement" => {
            let criterion_id = body
                .iter()
                .find_map(|line| parse_field(line, "criterion:"))?;
            let first_judgment = body
                .iter()
                .find_map(|line| parse_field(line, "first-judgment:"))?;
            let first_rationale = body
                .iter()
                .find_map(|line| parse_field(line, "first-rationale:"))?;
            let second_judgment = body
                .iter()
                .find_map(|line| parse_field(line, "second-judgment:"))?;
            let second_rationale = body
                .iter()
                .find_map(|line| parse_field(line, "second-rationale:"))?;
            let first_evidence_needed = body
                .iter()
                .find_map(|line| parse_field(line, "first-evidence-needed:"))
                .map(unescape_line);
            let second_evidence_needed = body
                .iter()
                .find_map(|line| parse_field(line, "second-evidence-needed:"))
                .map(unescape_line);
            let first = Verdict::new(
                CriterionId::new(criterion_id),
                parse_judgment_name(first_judgment, first_evidence_needed)?,
                NonEmptyString::new(unescape_line(first_rationale)).ok()?,
            );
            let second = Verdict::new(
                CriterionId::new(criterion_id),
                parse_judgment_name(second_judgment, second_evidence_needed)?,
                NonEmptyString::new(unescape_line(second_rationale)).ok()?,
            );
            Some(Residual::JudgeDisagreement {
                criterion_id: CriterionId::new(criterion_id),
                first: Box::new(first),
                second: Box::new(second),
            })
        }
        _ => None,
    }
}

/// Reconstruct every unresolved item from `plan_source` alone, each carrying
/// the task it belongs to: pure, no filesystem or store access.
///
/// # Errors
///
/// Returns [`tftio_planner::ParseError`] when `plan_source` cannot be
/// parsed.
pub fn unresolved_items_in(
    plan_source: &str,
) -> Result<Vec<OperatorItem>, tftio_planner::ParseError> {
    let plan = tftio_planner::parse_markdown(plan_source)?;
    Ok(unresolved_items_in_plan(&plan))
}

/// [`unresolved_items_in`], given an already-parsed `plan`.
///
/// Pure and parses nothing itself, for a caller (such as `crate::measure`)
/// that already holds a parsed [`tftio_planner::model::OperatorPlan`] and
/// would otherwise have to parse the same source text a second time.
#[must_use]
pub fn unresolved_items_in_plan(plan: &tftio_planner::model::OperatorPlan) -> Vec<OperatorItem> {
    let mut items = Vec::new();
    for task in &plan.tasks {
        let task_id = TaskId::new(task.id.as_str());
        for (index, criterion) in task.hidden_criteria.iter().enumerate() {
            match criterion.verdict {
                None if criterion.evaluator == tftio_planner::model::Evaluator::HumanJudgment => {
                    items.push(OperatorItem {
                        task_id: task_id.clone(),
                        residual: Residual::AwaitingHumanJudgment {
                            criterion_id: hidden_criterion_id(index),
                        },
                    });
                }
                Some(PlannerVerdict::Undetermined) => {
                    if let Some(evidence_needed) = criterion
                        .evidence_needed
                        .clone()
                        .and_then(|value| EvidenceNeeded::new(value).ok())
                    {
                        items.push(OperatorItem {
                            task_id: task_id.clone(),
                            residual: Residual::UndeterminedJudgment {
                                criterion_id: hidden_criterion_id(index),
                                evidence_needed,
                            },
                        });
                    }
                }
                None | Some(PlannerVerdict::Pass | PlannerVerdict::Fail) => {}
            }
        }
    }
    if let Some(guidance_log) = &plan.operator_guidance_log {
        items.extend(parse_guidance_residuals(guidance_log));
    }
    items
}

/// The current UTC date, in `YYYY-MM-DD` form, for stamping plan mutations.
/// Duplicated from `src/tools.rs`'s identical helper rather than shared:
/// both are small, self-contained, and pulling a shared helper into a third
/// module for two call sites would cost more than it saves (see that
/// module's own doc comment for why this crate hand-rolls it rather than
/// adding a dependency).
fn today_utc() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`, duplicated from `src/tools.rs` for
/// the same reason as [`today_utc`].
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

#[cfg(test)]
mod tests {
    use super::{
        CriterionAddress, criterion_address, escape_line, hidden_criterion_id, parse_field,
        unescape_line, visible_criterion_id,
    };
    use crate::model::{
        ChangedPath, CriterionId, Judgment, NonEmptyString, Residual, TaskId, Verdict,
    };
    use crate::store::{Store, StoreRoot};
    use std::error::Error;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

    const SENTINEL_PLAN: &str =
        include_str!("../tests/fixtures/2026-09-05-hidden-sentinel-plan.md");

    fn run_git(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).args(args);
        crate::test_support::harden_git_command(&mut command, dir);
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("git {args:?} failed in {}", dir.display()).into())
        }
    }

    /// Build a bare repository plus a stored plan, mirroring the fixture
    /// pattern in `tests/tools.rs`.
    #[test]
    fn run_git_reports_command_failures() {
        let temp_dir = std::env::temp_dir();
        assert!(run_git(&temp_dir, &["not-a-real-git-subcommand"]).is_err());
    }

    fn stored_plan(
        temp: &TempDir,
        plan_source: &str,
    ) -> Result<(Store, std::path::PathBuf, String), Box<dyn Error>> {
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo)?;
        run_git(&repo, &["init", "--quiet"])?;
        std::fs::write(repo.join("README.md"), "hello\n")?;
        run_git(&repo, &["add", "README.md"])?;
        let commit_args = [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "initial commit",
        ];
        let commit_result = run_git(&repo, &commit_args);
        assert!(
            commit_result.is_ok(),
            "git commit failed: {commit_result:?}"
        );

        let store_root = temp.path().join("store");
        let store = Store::new(StoreRoot::new(store_root));
        let plan_path = temp.path().join("2026-09-05-hidden-sentinel-plan.md");
        std::fs::write(&plan_path, plan_source)?;
        let plan_id = store.add_plan(&plan_path, &repo, "HEAD")?;
        Ok((store, repo, plan_id.as_str().to_owned()))
    }

    fn as_judge_disagreement(residual: &Residual) -> Option<(&Verdict, &Verdict)> {
        match residual {
            Residual::JudgeDisagreement { first, second, .. } => {
                Some((first.as_ref(), second.as_ref()))
            }
            Residual::AwaitingHumanJudgment { .. }
            | Residual::UndeterminedJudgment { .. }
            | Residual::UncoveredChangedScope { .. } => None,
        }
    }

    #[test]
    fn as_judge_disagreement_rejects_other_residual_variants() {
        let other = Residual::AwaitingHumanJudgment {
            criterion_id: CriterionId::new("hidden-0"),
        };
        assert!(as_judge_disagreement(&other).is_none());
    }

    fn sample_verdict(criterion_id: &str, judgment: Judgment) -> Result<Verdict, Box<dyn Error>> {
        Ok(Verdict::new(
            CriterionId::new(criterion_id),
            judgment,
            NonEmptyString::new("because the evidence says so")?,
        ))
    }

    #[test]
    fn criterion_address_round_trips_visible_and_hidden_ids() {
        assert_eq!(
            Some(CriterionAddress::Visible { index: 2 }),
            criterion_address(&visible_criterion_id(2))
        );
        assert_eq!(
            Some(CriterionAddress::Hidden { index: 0 }),
            criterion_address(&hidden_criterion_id(0))
        );
        assert_eq!(None, criterion_address(&CriterionId::new("nonsense")));
        assert_eq!(None, criterion_address(&CriterionId::new("hidden-x")));
    }

    #[test]
    fn escape_and_unescape_round_trip_backslashes_and_newlines() {
        let text = "line one\\nliteral\nline two\\\\end";
        assert_eq!(text, unescape_line(&escape_line(text)));
    }

    #[test]
    fn unescape_line_passes_through_an_unrecognized_escape() {
        // Not produced by `escape_line` itself, but `unescape_line` must
        // still do something sensible with it rather than panic.
        assert_eq!("\\x", unescape_line("\\x"));
    }

    #[test]
    fn provenance_name_covers_every_variant() {
        use crate::model::EvidenceProvenance;

        assert_eq!(
            "tool_authored",
            super::provenance_name(EvidenceProvenance::ToolAuthored)
        );
        assert_eq!("judge", super::provenance_name(EvidenceProvenance::Judge));
        assert_eq!(
            "worker_narrated",
            super::provenance_name(EvidenceProvenance::WorkerNarrated)
        );
        assert_eq!(
            "operator_observed",
            super::provenance_name(EvidenceProvenance::OperatorObserved)
        );
    }

    #[test]
    fn parse_judgment_name_rejects_unknown_names() {
        assert_eq!(None, super::parse_judgment_name("bogus", None));
    }

    #[test]
    fn parse_one_residual_body_rejects_an_unknown_kind() {
        assert_eq!(None, super::parse_one_residual_body("bogus-kind", &[]));
    }

    #[test]
    fn civil_from_days_matches_known_dates_across_both_month_halves() {
        // `days_since_epoch = 0` (1970-01-01) exercises the `month <= 2`
        // half of the civil-calendar conversion (`month_index >= 10`);
        // `2000-03-01` exercises the other half. Both are asserted here
        // (this module's own copy of the conversion, duplicated from
        // `src/tools.rs` for the reason given in `today_utc`'s docs).
        assert_eq!((1970, 1, 1), super::civil_from_days(0));
        assert_eq!((2000, 3, 1), super::civil_from_days(11_017));
    }

    #[test]
    fn record_residual_is_a_no_op_for_structurally_derived_classes() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);
        let before = std::fs::read_to_string(store.plan_path(&repo, &plan_id)?)?;

        let task_id = TaskId::new("T002");
        let awaiting = Residual::AwaitingHumanJudgment {
            criterion_id: CriterionId::new("hidden-0"),
        };
        ledger.record_residual(&task_id, &awaiting)?;
        let evidence_needed = crate::model::EvidenceNeeded::new("something")?;
        let undetermined = Residual::UndeterminedJudgment {
            criterion_id: CriterionId::new("hidden-0"),
            evidence_needed,
        };
        ledger.record_residual(&task_id, &undetermined)?;

        let after = std::fs::read_to_string(store.plan_path(&repo, &plan_id)?)?;
        assert_eq!(
            before, after,
            "these residual classes must not write anything"
        );
        Ok(())
    }

    #[test]
    fn parse_field_strips_the_key_and_leading_whitespace() {
        assert_eq!(Some("value"), parse_field("key:   value", "key:"));
        assert_eq!(None, parse_field("other: value", "key:"));
    }

    #[test]
    fn parse_visible_verdicts_round_trips_render_visible_verdict() -> Result<(), Box<dyn Error>> {
        let pass = sample_verdict("visible-0", Judgment::Pass)?;
        let fail = sample_verdict("visible-1", Judgment::Fail)?;
        let undetermined = Verdict::new(
            CriterionId::new("visible-2"),
            Judgment::Undetermined {
                evidence_needed: crate::model::EvidenceNeeded::new("a rerun")?,
            },
            NonEmptyString::new("could not tell from the transcript")?,
        );

        for verdict in [&pass, &fail, &undetermined] {
            let rendered = super::render_visible_verdict(verdict);
            assert_eq!(
                Ok(vec![verdict.clone()]),
                super::parse_visible_verdicts_in(&rendered),
                "round trip failed for {rendered:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn parse_visible_verdicts_in_reads_real_completion_evidence() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);
        // T001 already carries "Done in a prior run." as completion
        // evidence from the fixture, ahead of the append this test makes:
        // exercising the parser against a real, pre-existing unrelated
        // block plus a real date-prefixed append, not a hand-built string.
        let task_id = TaskId::new("T001");

        let pass = sample_verdict("visible-0", Judgment::Pass)?;
        ledger.record_visible_verdict(&task_id, &pass)?;

        let source = std::fs::read_to_string(store.plan_path(&repo, &plan_id)?)?;
        let plan = tftio_planner::parse_markdown(&source)?;
        let task = plan
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "T001")
            .ok_or("plan lost its task")?;
        let completion_evidence = task.completion_evidence.as_deref().unwrap_or_default();

        // The fixture's own placeholder text precedes this append and must
        // be silently skipped, and the real append is wrapped in
        // tftio_planner's own "{date} — evidence: " prefix.
        assert!(completion_evidence.contains("Done in a prior run."));
        assert!(completion_evidence.contains(" — evidence: visible criterion"));
        assert_eq!(
            Ok(vec![pass]),
            super::parse_visible_verdicts_in(completion_evidence)
        );
        Ok(())
    }

    #[test]
    fn parse_visible_verdicts_in_ignores_unrelated_text() {
        assert_eq!(
            Ok(Vec::<Verdict>::new()),
            super::parse_visible_verdicts_in("Pending.\n\njudge run: disposition=accept")
        );
    }

    #[test]
    fn parse_visible_verdicts_in_errors_on_a_recognizable_but_malformed_block() {
        // Carries the `render_visible_verdict` marker (so it must not be
        // silently skipped as "unrelated text") but never got a rationale
        // line -- corrupted, truncated, or hand-edited.
        let corrupted = "visible criterion visible-0 verdict: pass";
        let result = super::parse_visible_verdicts_in(corrupted);
        assert_eq!(
            Err(super::VisibleVerdictParseError {
                block: corrupted.to_owned()
            }),
            result
        );
    }

    #[test]
    fn parse_visible_verdicts_in_errors_on_a_block_missing_the_verdict_separator() {
        // Carries the marker but never got `" verdict: "` at all.
        let corrupted = "visible criterion visible-0 something-else\nrationale: because";
        let result = super::parse_visible_verdicts_in(corrupted);
        assert_eq!(
            Err(super::VisibleVerdictParseError {
                block: corrupted.to_owned()
            }),
            result
        );
    }

    #[test]
    fn parse_visible_verdicts_in_errors_on_an_unrecognized_judgment_name() {
        let corrupted =
            "visible criterion visible-0 verdict: sideways\nrationale: because it works";
        let result = super::parse_visible_verdicts_in(corrupted);
        assert_eq!(
            Err(super::VisibleVerdictParseError {
                block: corrupted.to_owned()
            }),
            result
        );
    }

    #[test]
    fn mutate_propagates_a_non_conflict_write_failure() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);
        let plan_path = store.plan_path(&repo, &plan_id)?;

        let result = ledger.mutate_with_hook(
            |_source| {
                Ok(tftio_planner::MutationRequest {
                    date: "2026-09-05".to_owned(),
                    mutation: tftio_planner::Mutation::AddDecision("from the ledger".to_owned()),
                })
            },
            |_source, _attempts| {
                // Removing the plan file entirely (rather than changing its
                // content, as `mutate_retries_a_stale_source_conflict`
                // does) makes `apply_prepared`'s own compare-before-write
                // read fail outright: a `WriteError::Io`, not a `Conflict`,
                // so this exercises the non-retried `Err(other)` arm
                // specifically.
                std::fs::remove_file(&plan_path).map_err(super::io_error(&plan_path))?;
                Ok(())
            },
        );

        assert!(result.is_err());
        Ok(())
    }

    /// Apply an `AddDecision` mutation to the plan at `plan_path` directly
    /// through `tftio_planner`, bypassing the ledger's own mutation lock --
    /// standing in for a writer outside its locking convention.
    fn apply_external_decision(plan_path: &Path, decision: &str) -> Result<(), super::LedgerError> {
        let source = std::fs::read_to_string(plan_path).map_err(super::io_error(plan_path))?;
        let request = tftio_planner::MutationRequest {
            date: "2026-09-05".to_owned(),
            mutation: tftio_planner::Mutation::AddDecision(decision.to_owned()),
        };
        let prepared = tftio_planner::prepare_markdown_mutation(&source, &request)?;
        tftio_planner::apply_prepared(plan_path, &prepared)?;
        Ok(())
    }

    #[test]
    fn mutate_retries_a_stale_source_conflict() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);
        let plan_path = store.plan_path(&repo, &plan_id)?;
        let injected = std::sync::atomic::AtomicBool::new(false);

        let build = |_source: &str| {
            Ok(tftio_planner::MutationRequest {
                date: "2026-09-05".to_owned(),
                mutation: tftio_planner::Mutation::AddDecision("from the ledger".to_owned()),
            })
        };
        // On the very first attempt only, apply an unrelated mutation
        // directly through `tftio_planner` -- a writer outside this lock's
        // own convention -- so this attempt's `apply_prepared` call sees a
        // source that changed after it read, a real `WriteError::Conflict`,
        // deterministically rather than by racing a second thread against
        // the lock.
        let after_read = |_source: &str, attempts: u32| {
            if attempts == 0 && !injected.swap(true, std::sync::atomic::Ordering::SeqCst) {
                apply_external_decision(&plan_path, "from outside the lock")?;
            }
            Ok(())
        };
        let mutate_result = ledger.mutate_with_hook(build, after_read);
        assert!(mutate_result.is_ok(), "{mutate_result:?}");

        let final_source = std::fs::read_to_string(&plan_path)?;
        assert!(final_source.contains("from outside the lock"));
        assert!(final_source.contains("from the ledger"));
        assert!(tftio_planner::validate_markdown(&final_source)?.is_valid());
        Ok(())
    }

    #[test]
    fn concurrent_verdict_writes_both_land() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;

        let store_a = std::sync::Arc::new(store);
        let store_b = std::sync::Arc::clone(&store_a);
        let repo_a = repo.clone();
        let repo_b = repo.clone();
        let plan_a = plan_id.clone();
        let plan_b = plan_id.clone();

        let verdict = sample_verdict("hidden-1", Judgment::Pass)?;
        let handle_a = std::thread::spawn(move || {
            let ledger = super::Ledger::new(&store_a, &repo_a, &plan_a);
            ledger.record_hidden_verdict(&TaskId::new("T002"), 1, &verdict, &[])
        });
        let handle_b = std::thread::spawn(move || {
            let ledger = super::Ledger::new(&store_b, &repo_b, &plan_b);
            ledger.append_completion_evidence(&TaskId::new("T002"), "concurrent-write-two")
        });

        handle_a.join().map_err(|_| "writer thread A panicked")??;
        handle_b.join().map_err(|_| "writer thread B panicked")??;

        let raw_store = Store::new(StoreRoot::new(temp.path().join("store")));
        let source = std::fs::read_to_string(raw_store.plan_path(&repo, &plan_id)?)?;
        let plan = tftio_planner::parse_markdown(&source)?;
        let task = plan
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "T002")
            .ok_or("plan lost its task")?;
        assert_eq!(
            Some(tftio_planner::model::HiddenVerdictJudgment::Pass),
            task.hidden_criteria.get(1).and_then(|c| c.verdict)
        );
        assert!(
            task.completion_evidence
                .as_deref()
                .is_some_and(|text| text.contains("concurrent-write-two"))
        );
        assert!(tftio_planner::validate_markdown(&source)?.is_valid());
        Ok(())
    }

    #[test]
    fn unresolved_items_covers_every_residual_class() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);

        // Class 1: an undetermined judgment, with its evidence_needed.
        let undetermined = Verdict::new(
            CriterionId::new("hidden-1"),
            Judgment::Undetermined {
                evidence_needed: crate::model::EvidenceNeeded::new("a rerun of the check")?,
            },
            NonEmptyString::new("the check output was ambiguous")?,
        );
        let task_id = TaskId::new("T002");
        ledger.record_hidden_verdict(&task_id, 1, &undetermined, &[])?;
        // Class 2: hidden-0 (human_judgment) is left with no verdict at all,
        // so it is structurally still awaiting the operator.

        // Class 3: uncovered changed scope.
        let uncovered_scope = Residual::UncoveredChangedScope {
            task_id: task_id.clone(),
            paths: vec![ChangedPath::new("src/unexpected.rs")],
        };
        ledger.record_residual(&task_id, &uncovered_scope)?;

        // Class 4: judge disagreement. Addressed at a *visible* criterion
        // here deliberately: a disagreement about a *hidden* criterion is
        // never reconstructable from the guidance log by design (see
        // `hidden_judge_disagreement_never_reaches_the_guidance_log_or_worker_projection`
        // below) -- this test exercises the class of residual `unresolved_items`
        // can still hand back structurally, which remains true for visible ones.
        let disagreement = Residual::JudgeDisagreement {
            criterion_id: CriterionId::new("visible-0"),
            first: Box::new(sample_verdict("visible-0", Judgment::Pass)?),
            second: Box::new(sample_verdict("visible-0", Judgment::Fail)?),
        };
        ledger.record_residual(&task_id, &disagreement)?;

        let residuals = ledger.unresolved_items()?;
        let has = |predicate: &dyn Fn(&Residual) -> bool| {
            residuals.iter().any(|item| predicate(&item.residual))
        };
        assert!(
            residuals.iter().all(|item| item.task_id == task_id),
            "expected every residual to carry task T002, got {residuals:?}"
        );
        assert!(
            has(&|r| matches!(r, Residual::AwaitingHumanJudgment { .. })),
            "expected an AwaitingHumanJudgment residual, got {residuals:?}"
        );
        assert!(
            has(&|r| matches!(r, Residual::UndeterminedJudgment { .. })),
            "expected an UndeterminedJudgment residual, got {residuals:?}"
        );
        assert!(
            has(&|r| matches!(r, Residual::UncoveredChangedScope { .. })),
            "expected an UncoveredChangedScope residual, got {residuals:?}"
        );
        assert!(
            has(&|r| matches!(r, Residual::JudgeDisagreement { .. })),
            "expected a JudgeDisagreement residual, got {residuals:?}"
        );

        // The plan is still a valid planning document after every write.
        let source = std::fs::read_to_string(store.plan_path(&repo, &plan_id)?)?;
        assert!(tftio_planner::validate_markdown(&source)?.is_valid());
        Ok(())
    }

    #[test]
    fn record_hidden_verdict_never_reaches_the_worker_projection() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);

        let evidence_needed =
            crate::model::EvidenceNeeded::new("SENTINEL-LEDGER-9f1c a manual review")?;
        let rationale =
            NonEmptyString::new("SENTINEL-LEDGER-9f1c inconclusive from the transcript")?;
        let verdict = Verdict::new(
            CriterionId::new("hidden-0"),
            Judgment::Undetermined { evidence_needed },
            rationale,
        );
        ledger.record_hidden_verdict(&TaskId::new("T002"), 0, &verdict, &[])?;

        let source = std::fs::read_to_string(store.plan_path(&repo, &plan_id)?)?;
        let worker_markdown = tftio_planner::project_worker_markdown(&source)?;
        assert!(!worker_markdown.contains("SENTINEL-LEDGER-9f1c"));
        Ok(())
    }

    /// Drives `record_visible_verdict` (and, through it, `render_visible_verdict`
    /// and `planner_task_id`) directly from this crate's own `--cfg test`
    /// build. `tests/tools.rs` already exercises this same path
    /// exhaustively, but only from the crate's *other* compiled copy (the
    /// plain, non-`--cfg test` one every integration test links); see
    /// `crate::test_support`'s module docs for why both copies need a real
    /// execution for `mise run check:coverage` to pass.
    #[test]
    fn record_visible_verdict_executes_in_this_build() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);

        let verdict = Verdict::new(
            CriterionId::new("visible-0"),
            Judgment::Pass,
            NonEmptyString::new("recorded from src/ledger.rs's own unit tests")?,
        );
        ledger.record_visible_verdict(&TaskId::new("T002"), &verdict)?;

        let source = std::fs::read_to_string(store.plan_path(&repo, &plan_id)?)?;
        assert!(source.contains("recorded from src/ledger.rs's own unit tests"));
        Ok(())
    }

    #[test]
    fn judge_disagreement_round_trips_an_undetermined_verdicts_evidence_needed()
    -> Result<(), Box<dyn Error>> {
        // A *visible*-criterion disagreement: still rendered structurally
        // into the tagged Operator Guidance Log entry and reconstructed by
        // `unresolved_items` from it, exactly as before. A *hidden*-criterion
        // disagreement no longer round-trips this way by design; see
        // `hidden_judge_disagreement_never_leaks_and_lands_as_criterion_evidence`
        // below for that behavior.
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);

        let first_verdict = Verdict::new(
            CriterionId::new("visible-0"),
            Judgment::Undetermined {
                evidence_needed: crate::model::EvidenceNeeded::new("a manual review")?,
            },
            NonEmptyString::new("cannot tell from the transcript")?,
        );
        let second_verdict = Verdict::new(
            CriterionId::new("visible-0"),
            Judgment::Undetermined {
                evidence_needed: crate::model::EvidenceNeeded::new("a different rerun")?,
            },
            NonEmptyString::new("also cannot tell")?,
        );
        let task_id = TaskId::new("T002");
        let disagreement = Residual::JudgeDisagreement {
            criterion_id: CriterionId::new("visible-0"),
            first: Box::new(first_verdict.clone()),
            second: Box::new(second_verdict.clone()),
        };
        ledger.record_residual(&task_id, &disagreement)?;

        let residuals = ledger.unresolved_items()?;
        let item = residuals
            .iter()
            .find(|item| matches!(item.residual, Residual::JudgeDisagreement { .. }))
            .ok_or("expected a JudgeDisagreement residual")?;
        assert_eq!(task_id, item.task_id);
        let (first, second) =
            as_judge_disagreement(&item.residual).ok_or("expected a JudgeDisagreement")?;
        assert_eq!(&first_verdict, first);
        assert_eq!(&second_verdict, second);
        Ok(())
    }

    /// A disagreement about a *hidden* criterion never writes the criterion
    /// id or either provider's rationale to the Operator Guidance Log --
    /// only a fixed line naming the task -- and records both verdicts as
    /// judge-provenance evidence on the criterion itself instead, which is
    /// stripped wholesale from the worker projection.
    #[test]
    fn hidden_judge_disagreement_never_leaks_and_lands_as_criterion_evidence()
    -> Result<(), Box<dyn Error>> {
        const SENTINEL_FIRST: &str = "SENTINEL-DISAGREEMENT-FIRST-8f21";
        const SENTINEL_SECOND: &str = "SENTINEL-DISAGREEMENT-SECOND-8f21";

        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);
        let task_id = TaskId::new("T002");

        let first_verdict = Verdict::new(
            CriterionId::new("hidden-1"),
            Judgment::Fail,
            NonEmptyString::new(SENTINEL_FIRST)?,
        );
        let second_verdict = Verdict::new(
            CriterionId::new("hidden-1"),
            Judgment::Pass,
            NonEmptyString::new(SENTINEL_SECOND)?,
        );
        let disagreement = Residual::JudgeDisagreement {
            criterion_id: CriterionId::new("hidden-1"),
            first: Box::new(first_verdict),
            second: Box::new(second_verdict),
        };
        ledger.record_residual(&task_id, &disagreement)?;

        let source = std::fs::read_to_string(store.plan_path(&repo, &plan_id)?)?;

        // The operator plan itself carries both rationales -- as evidence
        // on the hidden criterion, not in the guidance log.
        let plan = tftio_planner::parse_markdown(&source)?;
        let task = plan
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "T002")
            .ok_or("plan has no T002")?;
        let criterion = task
            .hidden_criteria
            .get(1)
            .ok_or("plan lost hidden_criteria[1]")?;
        assert!(
            criterion
                .evidence
                .iter()
                .any(|record| record.summary.contains(SENTINEL_FIRST)
                    && record.provenance == "judge"),
            "expected hidden_criteria[1].evidence to carry the first verdict's rationale, got {:?}",
            criterion.evidence
        );
        assert!(
            criterion
                .evidence
                .iter()
                .any(|record| record.summary.contains(SENTINEL_SECOND)
                    && record.provenance == "judge"),
            "expected hidden_criteria[1].evidence to carry the second verdict's rationale, got {:?}",
            criterion.evidence
        );

        // `hidden_disagreement_count` reads this back as one recorded
        // disagreement, from the plan alone.
        assert_eq!(1, super::hidden_disagreement_count(&criterion.evidence));

        // Fix round 2, finding #6: the guidance log carries nothing at
        // all for a hidden-criterion disagreement, not even a
        // criterion-free line -- that line's mere presence would itself
        // disclose that a hidden criterion exists.
        let guidance_log = plan.operator_guidance_log.unwrap_or_default();
        assert!(
            !guidance_log.contains("judge disagreement"),
            "a hidden-criterion disagreement must write nothing to the \
             guidance log, got: {guidance_log:?}"
        );
        assert!(!guidance_log.contains(SENTINEL_FIRST));
        assert!(!guidance_log.contains(SENTINEL_SECOND));
        assert!(!guidance_log.contains("hidden-1"));

        // Never reaches the worker-safe projection, by either route.
        let worker_markdown = tftio_planner::project_worker_markdown(&source)?;
        assert!(!worker_markdown.contains(SENTINEL_FIRST));
        assert!(!worker_markdown.contains(SENTINEL_SECOND));
        Ok(())
    }

    #[test]
    fn hidden_disagreement_count_is_zero_without_a_matching_summary() {
        assert_eq!(0, super::hidden_disagreement_count(&[]));
        let unrelated = tftio_planner::model::HiddenEvidenceRecord {
            summary: "automated check outcome: Passed".to_owned(),
            provenance: "tool_authored".to_owned(),
        };
        assert_eq!(0, super::hidden_disagreement_count(&[unrelated]));
    }

    #[test]
    fn hidden_disagreement_count_counts_events_not_raw_evidence_records()
    -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);
        let task_id = TaskId::new("T002");

        // Two separate disagreement events on the same hidden criterion
        // (e.g. two separate re-judged runs): four raw evidence records
        // (two "first"/"second" pairs), but two events.
        for _ in 0..2 {
            let disagreement = Residual::JudgeDisagreement {
                criterion_id: CriterionId::new("hidden-1"),
                first: Box::new(sample_verdict("hidden-1", Judgment::Pass)?),
                second: Box::new(sample_verdict("hidden-1", Judgment::Fail)?),
            };
            ledger.record_residual(&task_id, &disagreement)?;
        }

        let source = std::fs::read_to_string(store.plan_path(&repo, &plan_id)?)?;
        let plan = tftio_planner::parse_markdown(&source)?;
        let task = plan
            .tasks
            .iter()
            .find(|task| task.id.as_str() == "T002")
            .ok_or("plan has no T002")?;
        let criterion = task
            .hidden_criteria
            .get(1)
            .ok_or("plan lost hidden_criteria[1]")?;

        assert_eq!(4, criterion.evidence.len(), "expected four raw records");
        assert_eq!(2, super::hidden_disagreement_count(&criterion.evidence));
        Ok(())
    }

    /// Mutate a stored plan through [`Ledger`], then confirm the result still
    /// validates under `tftio_planner::validate_markdown_path`. The validator
    /// checks the filename shape as well as the content, so the mutated plan is
    /// validated as a copy under a compliantly shaped name.
    #[test]
    fn ledger_writes_still_validate_under_planner() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);

        let verdict = Verdict::new(
            CriterionId::new("hidden-1"),
            Judgment::Undetermined {
                evidence_needed: crate::model::EvidenceNeeded::new("a rerun of the check")?,
            },
            NonEmptyString::new("the automated check output was ambiguous")?,
        );
        ledger.record_hidden_verdict(&TaskId::new("T002"), 1, &verdict, &[])?;
        ledger
            .append_completion_evidence(&TaskId::new("T002"), "visible criterion verdict: pass")?;

        let plan_path = store.plan_path(&repo, &plan_id)?;
        let checked_copy = temp.path().join("2026-09-05-hidden-sentinel-plan.md");
        std::fs::copy(&plan_path, &checked_copy)?;
        let source = std::fs::read_to_string(&checked_copy)?;
        assert!(tftio_planner::validate_markdown_path(&source, &checked_copy)?.is_valid());
        Ok(())
    }

    #[test]
    fn mutation_lock_reports_io_failures() {
        let missing_parent = Path::new("/no/such/directory/2026-09-05-plan.md");
        let result = super::MutationLock::acquire(missing_parent);
        assert!(matches!(result, Err(super::LedgerError::Io { .. })));
    }

    #[test]
    fn mutation_lock_reclaims_a_stale_lock() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let plan_path = temp.path().join("2026-09-05-plan.md");
        std::fs::write(&plan_path, "placeholder")?;
        let lock_path = plan_path.with_extension("md.lock");
        std::fs::write(&lock_path, "stale")?;
        // Back-date the lock file well past `STALE_LOCK_THRESHOLD` so
        // `acquire` reclaims it instead of waiting out the retry budget.
        let stale_time = std::time::SystemTime::now() - std::time::Duration::from_hours(1);
        let file = std::fs::File::open(&lock_path)?;
        file.set_modified(stale_time)?;

        let lock = super::MutationLock::acquire(&plan_path)?;
        drop(lock);
        assert!(!lock_path.exists());
        Ok(())
    }

    #[test]
    fn mutation_lock_times_out_against_a_fresh_contending_lock() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let plan_path = temp.path().join("2026-09-05-plan.md");
        std::fs::write(&plan_path, "placeholder")?;
        let lock_path = plan_path.with_extension("md.lock");
        std::fs::write(&lock_path, "fresh")?;

        let result = super::MutationLock::acquire(&plan_path);
        assert!(matches!(
            result,
            Err(super::LedgerError::LockTimeout { .. })
        ));
        Ok(())
    }

    #[test]
    fn ledger_error_display_covers_every_variant() {
        let variants: Vec<super::LedgerError> = vec![
            super::LedgerError::Io {
                path: PathBuf::from("/x"),
                source: std::io::Error::other("boom"),
            },
            super::LedgerError::LockTimeout {
                path: PathBuf::from("/x.lock"),
            },
            super::LedgerError::InvalidTaskId("not a task id".to_owned()),
            super::LedgerError::UnknownTask("T999".to_owned()),
        ];
        for variant in variants {
            assert!(!variant.to_string().is_empty());
        }
    }

    #[test]
    fn record_hidden_verdict_rejects_an_invalid_task_id() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);
        let verdict = sample_verdict("hidden-0", Judgment::Pass)?;
        let result = ledger.record_hidden_verdict(&TaskId::new("not a valid id"), 0, &verdict, &[]);
        assert!(matches!(result, Err(super::LedgerError::InvalidTaskId(_))));
        Ok(())
    }

    #[test]
    fn record_hidden_verdict_rejects_an_out_of_range_index() -> Result<(), Box<dyn Error>> {
        let temp = TempDir::new()?;
        let (store, repo, plan_id) = stored_plan(&temp, SENTINEL_PLAN)?;
        let ledger = super::Ledger::new(&store, &repo, &plan_id);
        let verdict = sample_verdict("hidden-99", Judgment::Pass)?;
        let result = ledger.record_hidden_verdict(&TaskId::new("T002"), 99, &verdict, &[]);
        assert!(result.is_err());
        Ok(())
    }
}
